//! Anti-polling / rate limiting for MCP tool calls.
//!
//! Detects when agents repeatedly call the same operation in rapid succession
//! (e.g., polling task status every second) and warns or blocks.

use std::collections::HashMap;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

/// Configuration for rate limiting behavior
#[derive(Debug, Clone)]
pub struct RateLimitConfig {
    /// Minimum time to wait after task spawn before first status check
    pub initial_wait: Duration,
    /// Minimum time between subsequent status checks
    pub poll_cooldown: Duration,
    /// Minimum time between calls before hard blocking
    pub block_cooldown: Duration,
    /// Maximum warnings before auto-blocking
    pub max_warnings: u32,
    /// How long to remember call history
    pub history_ttl: Duration,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            initial_wait: Duration::from_secs(5),      // Wait 5s after spawn
            poll_cooldown: Duration::from_secs(10),    // 10s between polls
            block_cooldown: Duration::from_secs(3),    // Block if <3s between polls
            max_warnings: 1,                            // Block after 1 warning
            history_ttl: Duration::from_secs(300),     // 5 minutes
        }
    }
}

#[derive(Debug, Clone)]
struct CallRecord {
    /// When the task was first registered (spawned)
    created_at: Instant,
    /// When the last poll happened
    last_poll: Option<Instant>,
    /// Number of poll attempts
    poll_count: u32,
    /// Number of warnings issued
    warning_count: u32,
    /// Last offset used for output pagination (None = status check, Some = output with offset)
    last_output_offset: Option<usize>,
    /// Total bytes that have been returned to the agent (for continuation)
    bytes_returned: usize,
    /// Current penalty multiplier (doubles each violation, starts at 1.0)
    penalty_multiplier: f64,
    /// Expected duration in ms (from command history, 0 = unknown)
    expected_duration_ms: u64,
}

/// Result of checking rate limits
#[derive(Debug, Clone)]
pub enum RateLimitResult {
    /// Call is allowed to proceed
    Allow,
    /// Call is allowed but agent should be warned (with penalty delay)
    Warn {
        message: String,
        seconds_since_last: f64,
        call_count: u32,
        /// Artificial delay to apply before responding (exponential backoff)
        penalty_delay_secs: u64,
    },
    /// Call is blocked due to excessive polling (with penalty delay)
    Block {
        message: String,
        retry_after_ms: u64,
        /// Artificial delay to apply before responding
        penalty_delay_secs: u64,
    },
}

/// Information extracted from a tool call for rate limiting
#[derive(Debug, Clone)]
pub struct CallInfo {
    pub key: String,
    pub is_poll: bool,
    /// For output calls, the offset being requested (for pagination detection)
    pub output_offset: Option<usize>,
}

pub struct RateLimiter {
    config: RateLimitConfig,
    /// Map of call_key -> CallRecord
    history: Mutex<HashMap<String, CallRecord>>,
}

impl Default for RateLimiter {
    fn default() -> Self {
        Self::new(RateLimitConfig::default())
    }
}

impl RateLimiter {
    pub fn new(config: RateLimitConfig) -> Self {
        Self {
            config,
            history: Mutex::new(HashMap::new()),
        }
    }

    /// Generate a cache key for a task based on task_id
    pub fn make_task_key(task_id: &str) -> String {
        format!("task:{}", task_id)
    }

    /// Generate call info for a tool call based on tool name and relevant parameters
    pub fn make_call_info(tool: &str, params: &serde_json::Value) -> CallInfo {
        match tool {
            "task" => {
                let action = params.get("action").and_then(|a| a.as_str()).unwrap_or("");
                let task_id = params.get("task_id").and_then(|t| t.as_str()).unwrap_or("");
                match action {
                    "status" => CallInfo {
                        key: Self::make_task_key(task_id),
                        is_poll: true,
                        output_offset: None,
                    },
                    "output" => {
                        let offset = params.get("offset").and_then(|o| o.as_u64()).map(|o| o as usize);
                        CallInfo {
                            key: Self::make_task_key(task_id),
                            is_poll: true,
                            output_offset: offset,
                        }
                    }
                    _ => CallInfo { key: String::new(), is_poll: false, output_offset: None },
                }
            }
            _ => CallInfo { key: String::new(), is_poll: false, output_offset: None },
        }
    }

    /// Register that a task was spawned - call this when background task is created
    /// `expected_duration_ms` should come from command history (0 = unknown, use defaults)
    pub async fn register_task(&self, task_id: &str, expected_duration_ms: u64) {
        let key = Self::make_task_key(task_id);
        let mut history = self.history.lock().await;
        history.insert(key, CallRecord {
            created_at: Instant::now(),
            last_poll: None,
            poll_count: 0,
            warning_count: 0,
            last_output_offset: None,
            bytes_returned: 0,
            penalty_multiplier: 1.0,
            expected_duration_ms,
        });
    }

    /// Record that bytes were returned to the agent for a task
    /// Returns the recommended next offset for continuation
    pub async fn record_bytes_returned(&self, task_id: &str, offset: usize, bytes_read: usize) -> usize {
        let key = Self::make_task_key(task_id);
        let mut history = self.history.lock().await;
        
        if let Some(record) = history.get_mut(&key) {
            let new_end = offset + bytes_read;
            if new_end > record.bytes_returned {
                record.bytes_returned = new_end;
            }
            record.bytes_returned
        } else {
            offset + bytes_read
        }
    }

    /// Get the last offset that was returned to the agent (for status response continuation hints)
    pub async fn get_bytes_returned(&self, task_id: &str) -> usize {
        let key = Self::make_task_key(task_id);
        let history = self.history.lock().await;
        history.get(&key).map(|r| r.bytes_returned).unwrap_or(0)
    }

    /// Check if a poll call should be allowed, warned, or blocked
    /// 
    /// `call_info` contains the key and optional output offset for pagination detection
    pub async fn check(&self, call_info: &CallInfo) -> RateLimitResult {
        if call_info.key.is_empty() {
            return RateLimitResult::Allow;
        }

        let now = Instant::now();
        let mut history = self.history.lock().await;

        // Clean up old entries
        history.retain(|_, record| now.duration_since(record.created_at) < self.config.history_ttl);

        // If no record exists, this task wasn't registered (maybe spawned before rate limiting)
        // Create a record now but be lenient (use default 10s expected duration)
        let record = history.entry(call_info.key.clone()).or_insert(CallRecord {
            created_at: now,
            last_poll: None,
            poll_count: 0,
            warning_count: 0,
            last_output_offset: None,
            bytes_returned: 0,
            penalty_multiplier: 1.0,
            expected_duration_ms: 10_000, // Default 10s if unknown
        });

        // Calculate proportional thresholds based on expected duration
        // initial_wait = 25% of expected duration (min 2s, max 30s)
        // poll_cooldown = 50% of expected duration (min 5s, max 60s)
        // block_cooldown = 10% of expected duration (min 1s, max 10s)
        let expected_secs = (record.expected_duration_ms as f64 / 1000.0).max(5.0);
        let initial_wait = Duration::from_secs_f64((expected_secs * 0.25).clamp(2.0, 30.0));
        let poll_cooldown = Duration::from_secs_f64((expected_secs * 0.50).clamp(5.0, 60.0));
        let block_cooldown = Duration::from_secs_f64((expected_secs * 0.10).clamp(1.0, 10.0));

        // Check if this is pagination (output call with increasing offset)
        let is_pagination = if let Some(current_offset) = call_info.output_offset {
            if let Some(last_offset) = record.last_output_offset {
                // Pagination: offset is greater than last time (continuing to read)
                current_offset > last_offset
            } else {
                // First output call with offset - could be pagination start
                current_offset > 0
            }
        } else {
            false
        };

        // Update last offset if this is an output call
        if call_info.output_offset.is_some() {
            record.last_output_offset = call_info.output_offset;
        }

        // Allow pagination without rate limiting - agent is correctly reading output in chunks
        if is_pagination {
            record.last_poll = Some(now);
            return RateLimitResult::Allow;
        }

        record.poll_count += 1;

        // Check time since task was created (for first poll)
        let since_created = now.duration_since(record.created_at);
        
        // Check time since last poll (for subsequent polls)
        let since_last_poll = record.last_poll.map(|t| now.duration_since(t));
        
        // Update last poll time
        record.last_poll = Some(now);

        // First poll - check if too soon after task creation (proportional to expected duration)
        if record.poll_count == 1 {
            if since_created < initial_wait {
                record.warning_count += 1;
                // Penalty = 25% of expected duration * multiplier (min 2s, max 2min)
                let base_penalty = (expected_secs * 0.25).clamp(2.0, 120.0);
                record.penalty_multiplier *= 2.0;
                let penalty = (base_penalty * record.penalty_multiplier).min(120.0) as u64;
                return RateLimitResult::Warn {
                    message: format!(
                        "Polling too soon ({:.1}s elapsed, expected runtime ~{:.0}s, wait {:.0}s). \
                        PENALTY: {}s delay (doubles each time). Use `task` action `wait` instead.",
                        since_created.as_secs_f64(),
                        expected_secs,
                        initial_wait.as_secs_f64(),
                        penalty
                    ),
                    seconds_since_last: since_created.as_secs_f64(),
                    call_count: record.poll_count,
                    penalty_delay_secs: penalty,
                };
            }
            return RateLimitResult::Allow;
        }

        // Subsequent polls - check interval (proportional to expected duration)
        let elapsed = since_last_poll.unwrap_or(Duration::ZERO);

        // Sufficient time passed - allow and reset penalty
        if elapsed >= poll_cooldown {
            record.penalty_multiplier = 1.0;
            record.warning_count = 0;
            return RateLimitResult::Allow;
        }

        // Too fast - check severity
        if elapsed < block_cooldown || record.warning_count >= self.config.max_warnings {
            // Penalty proportional to expected duration * multiplier
            let base_penalty = (expected_secs * 0.5).clamp(5.0, 120.0);
            record.penalty_multiplier *= 2.0;
            let penalty = (base_penalty * record.penalty_multiplier).min(120.0) as u64;
            let retry_after = (poll_cooldown.as_millis() as u64).max(penalty * 1000);
            return RateLimitResult::Block {
                message: format!(
                    "POLLING ABUSE: {} polls, {:.1}s interval (expected ~{:.0}s runtime, min {:.0}s between checks). \
                    PENALTY: {}s delay. Use `task` action `wait` instead.",
                    record.poll_count,
                    elapsed.as_secs_f64(),
                    expected_secs,
                    poll_cooldown.as_secs_f64(),
                    penalty
                ),
                retry_after_ms: retry_after,
                penalty_delay_secs: penalty,
            };
        }

        // In warning zone - apply proportional penalty
        record.warning_count += 1;
        let base_penalty = (expected_secs * 0.25).clamp(2.0, 60.0);
        record.penalty_multiplier *= 2.0;
        let penalty = (base_penalty * record.penalty_multiplier).min(120.0) as u64;
        RateLimitResult::Warn {
            message: format!(
                "Polling too fast (poll #{}, {:.1}s since last, expected ~{:.0}s runtime). \
                PENALTY: {}s delay (doubles each time). Use `task` action `wait` instead.",
                record.poll_count,
                elapsed.as_secs_f64(),
                expected_secs,
                penalty
            ),
            seconds_since_last: elapsed.as_secs_f64(),
            call_count: record.poll_count,
            penalty_delay_secs: penalty,
        }
    }

    /// Reset rate limit state for a specific key (e.g., when task completes)
    pub async fn reset(&self, call_key: &str) {
        let mut history = self.history.lock().await;
        history.remove(call_key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn status_call(task_id: &str) -> CallInfo {
        CallInfo {
            key: RateLimiter::make_task_key(task_id),
            is_poll: true,
            output_offset: None,
        }
    }

    fn output_call(task_id: &str, offset: Option<usize>) -> CallInfo {
        CallInfo {
            key: RateLimiter::make_task_key(task_id),
            is_poll: true,
            output_offset: offset,
        }
    }

    fn empty_call() -> CallInfo {
        CallInfo {
            key: String::new(),
            is_poll: false,
            output_offset: None,
        }
    }

    #[test]
    fn test_make_call_info() {
        // Task status should generate a key
        let info = RateLimiter::make_call_info("task", &json!({"action": "status", "task_id": "abc123"}));
        assert_eq!(info.key, "task:abc123");
        assert!(info.is_poll);
        assert!(info.output_offset.is_none());

        // Task output should include offset
        let info = RateLimiter::make_call_info("task", &json!({"action": "output", "task_id": "abc123", "offset": 1000}));
        assert_eq!(info.key, "task:abc123");
        assert!(info.is_poll);
        assert_eq!(info.output_offset, Some(1000));

        // Task list should not generate a key (no rate limiting)
        let info = RateLimiter::make_call_info("task", &json!({"action": "list"}));
        assert!(info.key.is_empty());
        assert!(!info.is_poll);

        // Other tools should not generate keys
        let info = RateLimiter::make_call_info("run", &json!({"command": "echo hello"}));
        assert!(info.key.is_empty());
    }

    #[tokio::test]
    async fn test_rate_limit_allows_first_call_after_wait() {
        let config = RateLimitConfig {
            initial_wait: Duration::from_millis(10), // Very short for testing
            poll_cooldown: Duration::from_secs(2),
            block_cooldown: Duration::from_millis(100),
            max_warnings: 3,
            history_ttl: Duration::from_secs(60),
        };
        let limiter = RateLimiter::new(config);

        // Register task with SHORT expected duration (100ms) so proportional thresholds are small
        // initial_wait = 25% of 100ms = 25ms (clamped to min 2s normally, but we test the logic)
        limiter.register_task("test123", 100).await;

        // Wait longer than expected duration to ensure first poll is allowed
        tokio::time::sleep(Duration::from_millis(2100)).await;

        // First call after wait - should be allowed
        let result = limiter.check(&status_call("test123")).await;
        assert!(matches!(result, RateLimitResult::Allow));
    }

    #[tokio::test]
    async fn test_rate_limit_warns_on_immediate_poll() {
        let config = RateLimitConfig {
            initial_wait: Duration::from_secs(10), // Long wait (config is ignored, proportional used)
            poll_cooldown: Duration::from_secs(2),
            block_cooldown: Duration::from_millis(100),
            max_warnings: 3,
            history_ttl: Duration::from_secs(60),
        };
        let limiter = RateLimiter::new(config);

        // Register task with long expected duration and immediately poll - should warn
        limiter.register_task("test123", 60_000).await; // 60s expected
        let result = limiter.check(&status_call("test123")).await;
        // Should warn because we polled immediately (proportional initial_wait = 15s)
        assert!(matches!(result, RateLimitResult::Warn { .. }));
    }

    #[tokio::test]
    async fn test_rate_limit_allows_pagination() {
        let config = RateLimitConfig {
            initial_wait: Duration::from_millis(1),
            poll_cooldown: Duration::from_secs(10),
            block_cooldown: Duration::from_millis(100),
            max_warnings: 2,
            history_ttl: Duration::from_secs(60),
        };
        let limiter = RateLimiter::new(config);

        // Register task with short expected duration
        limiter.register_task("test123", 100).await;
        // Wait for initial period (2s min clamp)
        tokio::time::sleep(Duration::from_millis(2100)).await;

        // First output call at offset 0
        let result = limiter.check(&output_call("test123", Some(0))).await;
        assert!(matches!(result, RateLimitResult::Allow));

        // Immediate pagination to offset 1000 - should be allowed (pagination bypasses rate limit)
        let result = limiter.check(&output_call("test123", Some(1000))).await;
        assert!(matches!(result, RateLimitResult::Allow));

        // More pagination to offset 2000 - still allowed
        let result = limiter.check(&output_call("test123", Some(2000))).await;
        assert!(matches!(result, RateLimitResult::Allow));

    }

    #[tokio::test]
    async fn test_rate_limit_blocks_after_warnings() {
        let config = RateLimitConfig {
            initial_wait: Duration::from_millis(1),
            poll_cooldown: Duration::from_secs(10),
            block_cooldown: Duration::from_millis(50),
            max_warnings: 2,
            history_ttl: Duration::from_secs(60),
        };
        let limiter = RateLimiter::new(config);

        limiter.register_task("test123", 10_000).await;
        tokio::time::sleep(Duration::from_millis(5)).await;

        // First call - allowed
        limiter.check(&status_call("test123")).await;
        // Second - warn 1
        limiter.check(&status_call("test123")).await;
        // Third - warn 2
        limiter.check(&status_call("test123")).await;
        // Fourth - should block
        let result = limiter.check(&status_call("test123")).await;
        assert!(matches!(result, RateLimitResult::Block { .. }));
    }

    #[tokio::test]
    async fn test_empty_key_always_allowed() {
        let limiter = RateLimiter::default();
        for _ in 0..10 {
            let result = limiter.check(&empty_call()).await;
            assert!(matches!(result, RateLimitResult::Allow));
        }
    }
}
