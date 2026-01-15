use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::collections::HashMap;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::cache::OutputCache;
use crate::elicitation::{ClientCapabilities, ElicitAction, ElicitationManager, RequestSchema, schemas};
use crate::memory_client::MemoryClient;
use crate::output::{OutputFormat, DEFAULT_MAX_BYTES};
use crate::path_utils::normalize_path;
use crate::file_ops::{try_intercept, InterceptResult};
use crate::process::{
    cleanup_all_processes, register_pid, spawn_isolated, unregister_pid, SpawnConfig,
};
use crate::session::SessionManager;
use crate::shell::{
    detect_available_package_managers, detect_available_shells, detect_default_shell, ensure_shell,
    get_install_instructions, Shell,
};
use crate::rate_limit::{RateLimitConfig, RateLimitResult, RateLimiter};
use crate::task::{OutputStream, TaskManager};

pub struct McpServer {
    task_manager: TaskManager,
    session_manager: SessionManager,
    output_cache: OutputCache,
    client_capabilities: Arc<Mutex<ClientCapabilities>>,
    elicitation_manager: Arc<ElicitationManager>,
    rate_limiter: RateLimiter,
    memory_client: Arc<MemoryClient>,
}

impl Default for McpServer {
    fn default() -> Self {
        Self::new()
    }
}

impl McpServer {
    pub fn new() -> Self {
        Self {
            task_manager: TaskManager::new(),
            session_manager: SessionManager::new(),
            output_cache: OutputCache::default(),
            client_capabilities: Arc::new(Mutex::new(ClientCapabilities::default())),
            elicitation_manager: Arc::new(ElicitationManager::new()),
            rate_limiter: RateLimiter::new(RateLimitConfig::default()),
            memory_client: Arc::new(MemoryClient::new()),
        }
    }

    pub async fn run(&self) -> Result<()> {
        let stdin = tokio::io::stdin();
        let mut reader = BufReader::new(stdin);
        let mut stdout = tokio::io::stdout();

        let mut line = String::new();
        loop {
            match reader.read_line(&mut line).await {
                Ok(0) => break, // EOF
                Ok(_) => {}
                Err(e) => {
                    eprintln!("maxi-terminal stdin error: {}", e);
                    break;
                }
            }

            if line.trim().is_empty() {
                line.clear();
                continue;
            }

            let msg: Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(e) => {
                    let err = json!({
                        "jsonrpc": "2.0",
                        "error": { "code": -32700, "message": format!("Parse error: {}", e) },
                        "id": null
                    });
                    if let Ok(err_str) = serde_json::to_string(&err) {
                        let _ = stdout.write_all(err_str.as_bytes()).await;
                        let _ = stdout.write_all(b"\n").await;
                        let _ = stdout.flush().await;
                    }
                    line.clear();
                    continue;
                }
            };
            
            // Check if this is a response to an elicitation request we sent
            if let Some((id, result)) = ElicitationManager::is_elicitation_response(&msg) {
                self.elicitation_manager.handle_response(id, result).await;
                line.clear();
                continue;
            }

            // Otherwise, handle as a normal request
            let response = self.handle_request(msg).await;

            // Don't send responses for notifications (they return null)
            if response.is_null() {
                line.clear();
                continue;
            }

            if let Ok(response_str) = serde_json::to_string(&response) {
                if stdout.write_all(response_str.as_bytes()).await.is_err()
                    || stdout.write_all(b"\n").await.is_err()
                    || stdout.flush().await.is_err()
                {
                    eprintln!("maxi-terminal stdout error, client disconnected");
                    break;
                }
            }
            line.clear();
        }

        Ok(())
    }

    pub async fn shutdown(&self) {
        self.task_manager.kill_all_running().await;
        cleanup_all_processes().await;
    }

    async fn handle_request(&self, request: Value) -> Value {
        let method = request.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let id = request.get("id").cloned();
        let params = request.get("params").cloned().unwrap_or(json!({}));

        // Notifications don't get responses
        if method.starts_with("notifications/") {
            return Value::Null;
        }

        let result = match method {
            "initialize" => self.handle_initialize(&params).await,
            "tools/list" => self.handle_tools_list().await,
            "tools/call" => self.handle_tools_call(&params).await,
            _ => Err(anyhow!("Unknown method: {}", method)),
        };

        match result {
            Ok(value) => json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": value
            }),
            Err(e) => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -32000, "message": e.to_string() }
            }),
        }
    }

    async fn handle_initialize(&self, params: &Value) -> Result<Value> {
        // Parse and store client capabilities
        let caps = ClientCapabilities::from_initialize_params(params);
        *self.client_capabilities.lock().await = caps;

        Ok(json!({
            "protocolVersion": "2024-11-05",
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "maxi-terminal-mcp", "version": env!("CARGO_PKG_VERSION") }
        }))
    }

    async fn handle_tools_list(&self) -> Result<Value> {
        let has_elicitation = self.client_capabilities.lock().await.elicitation;
        
        Ok(json!({
            "tools": [
                {
                    "name": "run",
                    "description": "Execute command. Use background:true for long-running commands (returns task_id).",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "command": { "type": "string" },
                            "background": { "type": "boolean", "default": false },
                            "shell": { "type": "string", "enum": ["bash", "zsh", "fish", "powershell_core", "powershell", "cmd", "sh", "nushell", "wsl"] },
                            "cwd": { "type": "string" },
                            "env": { "type": "object" },
                            "timeout_ms": { "type": "integer" },
                            "format": { "type": "string", "enum": ["raw", "compact", "summary"], "default": "compact" },
                            "max_output": { "type": "integer" }
                        },
                        "required": ["command"]
                    }
                },
                {
                    "name": "task",
                    "description": "Manage background tasks: status, output (paginated), kill, list, or wait. USE `wait` TO BLOCK UNTIL DONE - do NOT poll status/output repeatedly (rate-limited with exponential penalty delays).",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "action": { "type": "string", "enum": ["status", "output", "kill", "list", "wait"], "description": "Use `wait` to block until task completes (recommended). Polling status/output incurs penalty delays." },
                            "task_id": { "type": "string", "description": "Required for status/output/kill/wait" },
                            "stream": { "type": "string", "enum": ["stdout", "stderr", "both"], "default": "both" },
                            "offset": { "type": "integer", "default": 0 },
                            "limit": { "type": "integer", "default": 50000 },
                            "format": { "type": "string", "enum": ["raw", "compact", "summary"], "default": "compact" },
                            "force": { "type": "boolean", "default": false, "description": "For kill: use SIGKILL" },
                            "timeout_ms": { "type": "integer", "default": 300000, "description": "For wait: max time to wait (default 5min)" }
                        },
                        "required": ["action"]
                    }
                },
                {
                    "name": "session",
                    "description": "Persistent shell sessions (preserve env/cwd): create, run command, destroy, or list.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "action": { "type": "string", "enum": ["create", "run", "destroy", "list"] },
                            "session_id": { "type": "string", "description": "Required for run/destroy" },
                            "command": { "type": "string", "description": "Required for run" },
                            "shell": { "type": "string", "enum": ["bash", "zsh", "fish", "powershell_core", "powershell", "cmd", "sh", "nushell", "wsl"] },
                            "cwd": { "type": "string" },
                            "env": { "type": "object" },
                            "timeout_ms": { "type": "integer" },
                            "format": { "type": "string", "enum": ["raw", "compact", "summary"], "default": "compact" },
                            "max_output": { "type": "integer" }
                        },
                        "required": ["action"]
                    }
                },
                {
                    "name": "shell",
                    "description": "Shell management: detect available shells/package managers, or install a shell.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "action": { "type": "string", "enum": ["detect", "install"] },
                            "shell": { "type": "string", "enum": ["bash", "zsh", "fish", "powershell_core", "nushell"], "description": "Required for install" },
                            "dry_run": { "type": "boolean", "default": false }
                        },
                        "required": ["action"]
                    }
                },
                {
                    "name": "interact",
                    "description": format!(
                        "Interactive prompts: input, confirm, select, multiselect. {}",
                        if has_elicitation {
                            "Uses MCP elicitation (structured prompts via client UI)."
                        } else {
                            "Requires TTY terminal - returns error with structured data in non-interactive environments."
                        }
                    ),
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "kind": { "type": "string", "enum": ["input", "confirm", "select", "multiselect"] },
                            "message": { "type": "string" },
                            "options": { "type": "array", "items": { "type": "string" }, "description": "For select/multiselect" },
                            "default": { "type": ["string", "boolean"], "description": "Default value" }
                        },
                        "required": ["kind", "message"]
                    }
                }
            ]
        }))
    }

    async fn handle_tools_call(&self, params: &Value) -> Result<Value> {
        let name = params
            .get("name")
            .and_then(|n| n.as_str())
            .ok_or_else(|| anyhow!("Missing tool name"))?;

        let args = params.get("arguments").cloned().unwrap_or(json!({}));

        // Check rate limits for polling operations - STRICT enforcement with penalties
        // Exception: status calls always succeed (they return timing info agent needs)
        let call_info = RateLimiter::make_call_info(name, &args);
        let is_status_call = name == "task" && 
            args.get("action").and_then(|a| a.as_str()) == Some("status");
        
        if call_info.is_poll && !is_status_call {
            match self.rate_limiter.check(&call_info).await {
                RateLimitResult::Allow => {}
                RateLimitResult::Warn { message, seconds_since_last, penalty_delay_secs, .. } => {
                    // Actually sleep the penalty - makes polling costly
                    if penalty_delay_secs > 0 {
                        eprintln!("[rate-limit] Applying {}s penalty delay", penalty_delay_secs);
                        tokio::time::sleep(tokio::time::Duration::from_secs(penalty_delay_secs)).await;
                    }
                    return Err(anyhow!(
                        "POLLING TOO FAST ({:.1}s since last): {} \
                        This response was delayed {}s. Penalty doubles each violation (max 120s).",
                        seconds_since_last,
                        message,
                        penalty_delay_secs
                    ));
                }
                RateLimitResult::Block { message, penalty_delay_secs, .. } => {
                    // Actually sleep the penalty
                    if penalty_delay_secs > 0 {
                        eprintln!("[rate-limit] Applying {}s penalty delay", penalty_delay_secs);
                        tokio::time::sleep(tokio::time::Duration::from_secs(penalty_delay_secs)).await;
                    }
                    return Err(anyhow!(
                        "POLLING BLOCKED: {} \
                        This response was delayed {}s. Penalty doubles each violation (max 120s).",
                        message,
                        penalty_delay_secs
                    ));
                }
            }
        }

        match name {
            "run" => self.tool_run(&args).await,
            "task" => self.tool_task(&args).await,
            "session" => self.tool_session(&args).await,
            "shell" => self.tool_shell(&args).await,
            "interact" => self.tool_interact(&args).await,
            _ => Err(anyhow!("Unknown tool: {}", name)),
        }
    }

    async fn tool_run(&self, args: &Value) -> Result<Value> {
        let command = args
            .get("command")
            .and_then(|c| c.as_str())
            .ok_or_else(|| anyhow!("Missing command"))?;

        // Try to intercept file operations and delegate to maxi-ide
        let cwd_for_intercept = args.get("cwd").and_then(|c| c.as_str()).map(std::path::Path::new);
        if let InterceptResult::Handled(result) = try_intercept(command, cwd_for_intercept).await {
            return Ok(result);
        }

        let background = args
            .get("background")
            .and_then(|b| b.as_bool())
            .unwrap_or(false);
        let shell = self.resolve_shell(args.get("shell"))?;
        let shell_path = which::which(shell.executable_name())?;
        let shell_args = shell.command_args(command);

        let cwd = args
            .get("cwd")
            .and_then(|c| c.as_str())
            .map(|p| normalize_path(p));
        let env: Option<HashMap<String, String>> = args
            .get("env")
            .and_then(|e| serde_json::from_value(e.clone()).ok());
        let timeout_ms = args.get("timeout_ms").and_then(|t| t.as_u64()).or(Some(30000));

        if background {
            let task_id = self
                .task_manager
                .spawn_task(
                    command.to_string(),
                    shell_path,
                    shell_args,
                    timeout_ms,
                    cwd,
                    env,
                )
                .await?;

                        // Get expected duration from command history for proportional rate limiting
            let expected_ms = self.memory_client.get_cmd_estimate(command).await
                .map(|e| e.estimate_ms)
                .unwrap_or(10_000); // Default 10s if unknown

            // Register task for rate limiting with expected duration
            self.rate_limiter.register_task(&task_id, expected_ms).await;

            let expected_secs = expected_ms / 1000;
            return self.ok(&json!({
                "task_id": task_id,
                "expected_secs": expected_secs,
                "hint": format!("Use `task` with action `wait` to block until done (~{}s). Polling incurs penalty delays proportional to runtime.", expected_secs)
            }));
        }

        let _format = self.parse_format(args.get("format"));
        let _max_output = args
            .get("max_output")
            .and_then(|m| m.as_u64())
            .map(|m| m as usize);

        let config = SpawnConfig {
            program: shell_path,
            args: shell_args,
            cwd,
            env,
            new_process_group: true,
        };

        let started = std::time::Instant::now();
        let child = spawn_isolated(&config)?;
        let pid = child.id();

        if let Some(p) = pid {
            register_pid(p).await;
        }

        let wait_future = child.wait_with_output();

        let output = if let Some(ms) = timeout_ms {
            match tokio::time::timeout(tokio::time::Duration::from_millis(ms), wait_future).await {
                Ok(r) => r?,
                Err(_) => {
                    if let Some(p) = pid {
                        let _ = crate::process::kill_process_tree(p, true).await;
                    }
                    return Err(anyhow!("Timeout after {}ms", ms));
                }
            }
        } else {
            wait_future.await?
        };

        if let Some(p) = pid {
            unregister_pid(p).await;
        }

        let duration_ms = started.elapsed().as_millis() as u64;

        // Record to maxi-memory for duration tracking
        self.record_task_completion(command, duration_ms);

        let output_id = self
            .output_cache
            .store(
                output.stdout.clone(),
                output.stderr.clone(),
                output.status.code(),
            )
            .await;

        let stdout_text = String::from_utf8_lossy(&output.stdout);
        let stderr_text = String::from_utf8_lossy(&output.stderr);
        let mut parsed = crate::parser::parse_output(&stdout_text, &stderr_text, output.status.code());

        // Inject actual output_id into truncation message
        if parsed.summary.contains("[Output truncated. Use task output with oid") {
            parsed.summary = parsed.summary.replace(
                "with oid for",
                &format!("with oid=\"{}\" for", output_id)
            );
        }

        // Response is ALWAYS structured - raw output available via output_id pagination
        // This keeps responses small and predictable
        self.ok(&json!({
            "exit": output.status.code(),
            "dur": duration_ms,
            "oid": output_id,
            "out": parsed.summary,
            "data": parsed.data
        }))
    }

    async fn tool_task(&self, args: &Value) -> Result<Value> {
        let action = args
            .get("action")
            .and_then(|a| a.as_str())
            .ok_or_else(|| anyhow!("Missing action"))?;

        match action {
            "list" => {
                let tasks = self.task_manager.list_tasks().await;
                let summaries: Vec<_> = tasks
                    .iter()
                    .map(|t| json!({"id": t.id, "cmd": t.command, "st": t.status, "exit": t.exit_code, "dur": t.duration_ms, "pid": t.pid}))
                    .collect();
                self.ok(&json!(summaries))
            }
                        "status" => {
                let task_id = args
                    .get("task_id")
                    .and_then(|t| t.as_str())
                    .ok_or_else(|| anyhow!("Missing task_id"))?;
                let task = self
                    .task_manager
                    .get_task(task_id)
                    .await
                    .ok_or_else(|| anyhow!("Task not found"))?;
                let output_complete = self
                    .task_manager
                    .is_output_complete(task_id)
                    .await
                    .unwrap_or(true);

                // Get timing info (always available, even when rate limited)
                let timing = self.rate_limiter.get_task_timing(task_id).await;
                let bytes_read = self.rate_limiter.get_bytes_returned(task_id).await;

                self.ok(&json!({
                    "st": task.status,
                    "exit": task.exit_code,
                    "dur": task.duration_ms,
                    "pid": task.pid,
                    "done": output_complete,
                    "next_offset": bytes_read,
                    // Timing info - always included so agent knows how long to wait
                    "expected_ms": timing.expected_ms,
                    "elapsed_ms": timing.elapsed_ms,
                    "remaining_ms": timing.remaining_ms,
                    "wait_ms": timing.recommended_wait_ms
                }))
            }
            "output" => {
                let task_id = args
                    .get("task_id")
                    .and_then(|t| t.as_str())
                    .ok_or_else(|| anyhow!("Missing task_id"))?;
                let offset = args.get("offset").and_then(|o| o.as_u64()).unwrap_or(0) as usize;
                let limit = args
                    .get("limit")
                    .and_then(|l| l.as_u64())
                    .unwrap_or(DEFAULT_MAX_BYTES as u64) as usize;
                let format = self.parse_format(args.get("format"));

                if task_id.starts_with("out_") {
                    let cache_stream = match args.get("stream").and_then(|s| s.as_str()) {
                        Some("stdout") => Some(crate::cache::OutputStream::Stdout),
                        Some("stderr") => Some(crate::cache::OutputStream::Stderr),
                        _ => None,
                    };

                    let (data, has_more) = self
                        .output_cache
                        .get_slice(task_id, cache_stream, offset, limit)
                        .await
                        .ok_or_else(|| anyhow!("Output not found or expired"))?;

                    let processed = crate::output::process_output(&data, format, Some(limit));
                    return self.ok(&json!({
                        "output": processed.as_string_lossy(),
                        "has_more": has_more,
                        "total_bytes": processed.total_bytes,
                        "truncated": processed.truncated
                    }));
                }

                let stream = match args.get("stream").and_then(|s| s.as_str()) {
                    Some("stdout") => OutputStream::Stdout,
                    Some("stderr") => OutputStream::Stderr,
                    _ => OutputStream::Both,
                };

                let (data, has_more) = self
                    .task_manager
                    .get_output(task_id, stream, offset, limit)
                    .await
                    .ok_or_else(|| anyhow!("Task not found"))?;

                // Record bytes returned for continuation tracking
                let bytes_read = data.len();
                let next_offset = self.rate_limiter.record_bytes_returned(task_id, offset, bytes_read).await;

                // Check if task is done to guide polling behavior
                let task_done = self
                    .task_manager
                    .is_output_complete(task_id)
                    .await
                    .unwrap_or(true);

                let processed = crate::output::process_output(&data, format, Some(limit));

                // Build response with guidance on whether to poll again
                let mut response = json!({
                    "output": processed.as_string_lossy(),
                    "has_more": has_more,
                    "next_offset": next_offset
                });

                // Only include metadata if relevant
                if processed.truncated {
                    response["truncated"] = json!(true);
                }

                // Critical: tell agent whether task is done and if they should wait
                response["done"] = json!(task_done);
                if !has_more && !task_done {
                    // Get smart estimate based on command history
                    let hint = if let Some(task) = self.task_manager.get_task(task_id).await {
                        if let Some(estimate) = self.memory_client.get_cmd_estimate(&task.command).await {
                            estimate.as_hint()
                        } else {
                            "STOP. Run `mcp__maxi-terminal__run` with command: `sleep 10` before checking again.".to_string()
                        }
                    } else {
                        "STOP. Run `mcp__maxi-terminal__run` with command: `sleep 10` before checking again.".to_string()
                    };
                    response["hint"] = json!(hint);
                }

                self.ok(&response)
            }
            "kill" => {
                let task_id = args
                    .get("task_id")
                    .and_then(|t| t.as_str())
                    .ok_or_else(|| anyhow!("Missing task_id"))?;
                let force = args.get("force").and_then(|f| f.as_bool()).unwrap_or(false);
                let killed = self.task_manager.kill_task(task_id, force).await?;
                self.ok(&json!({"killed": killed}))
            }
            "wait" => {
                // Block until task completes - no polling needed
                let task_id = args
                    .get("task_id")
                    .and_then(|t| t.as_str())
                    .ok_or_else(|| anyhow!("Missing task_id"))?;
                let timeout_ms = args.get("timeout_ms").and_then(|t| t.as_u64()).unwrap_or(300_000); // 5min default
                let format = self.parse_format(args.get("format"));

                let start = std::time::Instant::now();
                let timeout = tokio::time::Duration::from_millis(timeout_ms);

                // Poll internally until done (not exposed to agent)
                loop {
                    if start.elapsed() > timeout {
                        return Err(anyhow!("Task wait timed out after {}ms", timeout_ms));
                    }

                    let done = self.task_manager.is_output_complete(task_id).await.unwrap_or(true);
                    if done {
                        break;
                    }

                    // Internal poll - 500ms interval (not rate-limited since it's internal)
                    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
                }

                // Task is done - return full output
                let task = self.task_manager.get_task(task_id).await
                    .ok_or_else(|| anyhow!("Task not found"))?;
                let (data, _) = self.task_manager.get_output(task_id, OutputStream::Both, 0, usize::MAX).await
                    .ok_or_else(|| anyhow!("Task output not found"))?;

                let processed = crate::output::process_output(&data, format, None);

                self.ok(&json!({
                    "status": task.status,
                    "exit_code": task.exit_code,
                    "duration_ms": task.duration_ms,
                    "output": processed.as_string_lossy()
                }))
            }
            _ => Err(anyhow!("Unknown action: {}", action)),
        }
    }

    async fn tool_session(&self, args: &Value) -> Result<Value> {
        let action = args
            .get("action")
            .and_then(|a| a.as_str())
            .ok_or_else(|| anyhow!("Missing action"))?;

        match action {
            "create" => {
                let shell = self.resolve_shell(args.get("shell"))?;
                let cwd = args
                    .get("cwd")
                    .and_then(|c| c.as_str())
                    .map(|p| normalize_path(p));
                let env: Option<HashMap<String, String>> = args
                    .get("env")
                    .and_then(|e| serde_json::from_value(e.clone()).ok());
                let session_id = self.session_manager.create_session(shell, cwd, env).await?;
                self.ok(&json!({"session_id": session_id}))
            }
            "run" => {
                let session_id = args
                    .get("session_id")
                    .and_then(|s| s.as_str())
                    .ok_or_else(|| anyhow!("Missing session_id"))?;
                let command = args
                    .get("command")
                    .and_then(|c| c.as_str())
                    .ok_or_else(|| anyhow!("Missing command"))?;
                let timeout_ms = args.get("timeout_ms").and_then(|t| t.as_u64());
                let format = self.parse_format(args.get("format"));
                let max_output = args
                    .get("max_output")
                    .and_then(|m| m.as_u64())
                    .map(|m| m as usize);

                let result = self
                    .session_manager
                    .run_in_session(session_id, command, timeout_ms, format, max_output)
                    .await?;
                
                // Store in cache for pagination
                let oid = self.output_cache.store(
                    result.stdout.data.clone(),
                    result.stderr.data.clone(),
                    result.exit_code,
                ).await;
                
                let stdout_text = String::from_utf8_lossy(&result.stdout.data);
                let stderr_text = String::from_utf8_lossy(&result.stderr.data);
                let mut parsed = crate::parser::parse_output(&stdout_text, &stderr_text, result.exit_code);

                // Inject actual output_id into truncation message
                if parsed.summary.contains("[Output truncated. Use task output with oid") {
                    parsed.summary = parsed.summary.replace(
                        "with oid for",
                        &format!("with oid=\"{}\" for", oid)
                    );
                }
                
                self.ok(&json!({
                    "exit": result.exit_code,
                    "dur": result.duration_ms,
                    "oid": oid,
                    "out": parsed.summary,
                    "data": parsed.data
                }))
            }
            "destroy" => {
                let session_id = args
                    .get("session_id")
                    .and_then(|s| s.as_str())
                    .ok_or_else(|| anyhow!("Missing session_id"))?;
                let destroyed = self.session_manager.destroy_session(session_id).await;
                self.ok(&json!({"destroyed": destroyed}))
            }
            "list" => {
                let sessions = self.session_manager.list_sessions().await;
                self.ok(&json!(sessions))
            }
            _ => Err(anyhow!("Unknown action: {}", action)),
        }
    }

    async fn tool_shell(&self, args: &Value) -> Result<Value> {
        let action = args
            .get("action")
            .and_then(|a| a.as_str())
            .ok_or_else(|| anyhow!("Missing action"))?;

        match action {
            "detect" => {
                let available = detect_available_shells();
                let default = detect_default_shell();
                let pms = detect_available_package_managers();

                self.ok(&json!({
                    "default": default.map(|(s, p)| json!({"shell": s, "path": p.to_string_lossy()})),
                    "available": available.iter().map(|(s, p)| json!({"shell": s, "path": p.to_string_lossy()})).collect::<Vec<_>>(),
                    "package_managers": pms.iter().map(|(pm, p)| json!({"name": pm, "path": p.to_string_lossy()})).collect::<Vec<_>>()
                }))
            }
            "install" => {
                let shell_str = args
                    .get("shell")
                    .and_then(|s| s.as_str())
                    .ok_or_else(|| anyhow!("Missing shell"))?;
                let shell: Shell = serde_json::from_value(json!(shell_str))?;
                let dry_run = args
                    .get("dry_run")
                    .and_then(|d| d.as_bool())
                    .unwrap_or(false);

                if dry_run {
                    let instructions = get_install_instructions(shell);
                    let pms = detect_available_package_managers();
                    let commands: Vec<_> = pms
                        .iter()
                        .filter_map(|(pm, _)| {
                            instructions
                                .get(pm)
                                .map(|cmd| json!({"pm": pm, "command": cmd}))
                        })
                        .collect();
                    return self.ok(&json!({"dry_run": true, "commands": commands}));
                }

                let path = ensure_shell(shell).await?;
                self.ok(&json!({"installed": true, "path": path.to_string_lossy()}))
            }
            _ => Err(anyhow!("Unknown action: {}", action)),
        }
    }

    fn resolve_shell(&self, shell_arg: Option<&Value>) -> Result<Shell> {
        if let Some(s) = shell_arg.and_then(|s| s.as_str()) {
            return Shell::from_name(s)
                .ok_or_else(|| anyhow!("Unknown shell: {}. Options: bash, zsh, fish, pwsh, powershell, cmd, sh, nu, wsl", s));
        }
        detect_default_shell()
            .map(|(s, _)| s)
            .ok_or_else(|| anyhow!("No shell available"))
    }

    async fn tool_interact(&self, args: &Value) -> Result<Value> {
        let has_elicitation = self.client_capabilities.lock().await.elicitation;
        
        let kind = args
            .get("kind")
            .and_then(|k| k.as_str())
            .ok_or_else(|| anyhow!("Missing kind"))?;

        let message = args
            .get("message")
            .and_then(|m| m.as_str())
            .ok_or_else(|| anyhow!("Missing message"))?;

        // If client supports elicitation, use it
        if has_elicitation {
            return self.interact_via_elicitation(kind, message, args).await;
        }

        // Try TTY-based interaction
        if atty::is(atty::Stream::Stdin) {
            let result = tokio::task::spawn_blocking({
                let args = args.clone();
                move || crate::interact::handle_interact(&args)
            })
            .await??;
            return self.ok(&result);
        }

        // No TTY and no elicitation - return structured error with context
        // This allows the LLM to potentially use AskUserQuestion or similar
        let schema = match kind {
            "input" => {
                let default = args.get("default").and_then(|d| d.as_str());
                json!({
                    "type": "input",
                    "message": message,
                    "default": default,
                    "field_name": "response"
                })
            }
            "confirm" => {
                let default = args.get("default").and_then(|d| d.as_bool()).unwrap_or(false);
                json!({
                    "type": "confirm", 
                    "message": message,
                    "default": default,
                    "field_name": "confirmed"
                })
            }
            "select" => {
                let options: Vec<String> = args
                    .get("options")
                    .and_then(|o| o.as_array())
                    .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                    .unwrap_or_default();
                json!({
                    "type": "select",
                    "message": message,
                    "options": options,
                    "field_name": "selection"
                })
            }
            "multiselect" => {
                let options: Vec<String> = args
                    .get("options")
                    .and_then(|o| o.as_array())
                    .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                    .unwrap_or_default();
                json!({
                    "type": "multiselect",
                    "message": message,
                    "options": options,
                    "field_name": "selections"
                })
            }
            _ => json!({"type": kind, "message": message})
        };

        // Return error with structured data that an LLM could use
        Err(anyhow!(
            "Interactive prompt not available (no TTY, client doesn't support elicitation). \
            Prompt details: {}",
            serde_json::to_string(&schema)?
        ))
    }

    async fn interact_via_elicitation(&self, kind: &str, message: &str, args: &Value) -> Result<Value> {
        // Build schema based on interaction kind
        let schema = match kind {
            "input" => {
                let default = args.get("default").and_then(|d| d.as_str());
                let mut s = schemas::text_input("response", message);
                if let Some(d) = default {
                    s = RequestSchema::new()
                        .with_string_default("response", Some(message), d)
                        .required_fields(vec!["response"]);
                }
                s
            }
            "confirm" => schemas::confirm("confirmed", message),
            "select" => {
                let options: Vec<String> = args
                    .get("options")
                    .and_then(|o| o.as_array())
                    .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                    .unwrap_or_default();
                schemas::select("selection", message, options)
            }
            "multiselect" => {
                let options: Vec<String> = args
                    .get("options")
                    .and_then(|o| o.as_array())
                    .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                    .unwrap_or_default();
                // MCP elicitation doesn't support true multiselect, use hint
                schemas::multiselect_hint("selections", message, options)
            }
            _ => return Err(anyhow!("Unknown interaction kind: {}", kind)),
        };

        // Create and send elicitation request
        let (id, request) = self.elicitation_manager.create_request(message, schema);
        let rx = self.elicitation_manager.register_pending(id).await;

        // Send request to client via stdout
        let request_str = serde_json::to_string(&request)?;
        let mut stdout = tokio::io::stdout();
        stdout.write_all(request_str.as_bytes()).await?;
        stdout.write_all(b"\n").await?;
        stdout.flush().await?;

        // Wait for response (with timeout)
        let result = tokio::time::timeout(
            tokio::time::Duration::from_secs(300), // 5 minute timeout for user input
            rx
        )
        .await
        .map_err(|_| anyhow!("Elicitation timed out waiting for user response"))?
        .map_err(|_| anyhow!("Elicitation channel closed"))?;

        // Convert result based on action
        match result.into_action() {
            ElicitAction::Accept(content) => {
                // Return the appropriate field based on kind
                let value = match kind {
                    "input" => content.get("response").cloned().unwrap_or(json!(null)),
                    "confirm" => content.get("confirmed").cloned().unwrap_or(json!(false)),
                    "select" => content.get("selection").cloned().unwrap_or(json!(null)),
                    "multiselect" => {
                        // Parse comma-separated response back to array
                        let raw = content.get("selections")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        let items: Vec<&str> = raw.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()).collect();
                        json!(items)
                    }
                    _ => json!(content)
                };
                self.ok(&value)
            }
            ElicitAction::Decline => {
                Err(anyhow!("User declined the prompt"))
            }
            ElicitAction::Cancel => {
                Err(anyhow!("User cancelled the prompt"))
            }
        }
    }

    fn parse_format(&self, format_arg: Option<&Value>) -> OutputFormat {
        match format_arg.and_then(|f| f.as_str()) {
            Some("raw") => OutputFormat::Raw,
            Some("summary") => OutputFormat::Summary,
            _ => OutputFormat::Compact,
        }
    }

    /// Record command completion to maxi-memory for duration tracking.
    /// Fire-and-forget: spawns a background task so it doesn't block response.
    fn record_task_completion(&self, command: &str, duration_ms: u64) {
        let client = Arc::clone(&self.memory_client);
        let cmd = command.to_string();
        tokio::spawn(async move {
            client.record_cmd_duration(&cmd, duration_ms).await;
        });
    }

    fn ok(&self, data: &Value) -> Result<Value> {
        Ok(json!({"content": [{"type": "text", "text": serde_json::to_string(data)?}]}))
    }
}
