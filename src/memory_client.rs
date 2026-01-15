//! Client for calling maxi-memory MCP server for command duration tracking.
//!
//! Uses a persistent subprocess connection to avoid spawn overhead.

use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;

static REQUEST_ID: AtomicU64 = AtomicU64::new(1);

/// Lazily-initialized connection to maxi-memory
pub struct MemoryClient {
    child: Mutex<Option<Child>>,
}

impl Default for MemoryClient {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryClient {
    pub fn new() -> Self {
        Self {
            child: Mutex::new(None),
        }
    }

    /// Ensure maxi-memory process is running
    async fn ensure_connected(&self) -> Result<()> {
        let mut guard = self.child.lock().await;
        
        // Check if existing process is still alive
        if let Some(ref mut child) = *guard {
            match child.try_wait() {
                Ok(None) => return Ok(()), // Still running
                Ok(Some(_)) | Err(_) => {
                    *guard = None; // Process died, will respawn
                }
            }
        }

        // Spawn new maxi-memory process
        let child = Command::new("maxi-memory")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn();

        match child {
            Ok(mut c) => {
                // Send initialize request (required for MCP)
                if let (Some(stdin), Some(stdout)) = (c.stdin.as_mut(), c.stdout.as_mut()) {
                    let init_req = json!({
                        "jsonrpc": "2.0",
                        "id": REQUEST_ID.fetch_add(1, Ordering::Relaxed),
                        "method": "initialize",
                        "params": {
                            "protocolVersion": "2024-11-05",
                            "capabilities": {},
                            "clientInfo": { "name": "maxi-terminal", "version": "0.1.0" }
                        }
                    });
                    
                    let msg = serde_json::to_string(&init_req)? + "\n";
                    stdin.write_all(msg.as_bytes()).await?;
                    
                    // Read init response
                    let mut reader = BufReader::new(stdout);
                    let mut line = String::new();
                    reader.read_line(&mut line).await?;
                    
                    // Send initialized notification
                    let notif = json!({
                        "jsonrpc": "2.0",
                        "method": "notifications/initialized"
                    });
                    let msg = serde_json::to_string(&notif)? + "\n";
                    if let Some(stdin) = c.stdin.as_mut() {
                        stdin.write_all(msg.as_bytes()).await?;
                    }
                }
                
                *guard = Some(c);
                Ok(())
            }
            Err(e) => {
                // maxi-memory not installed or not in PATH - silently fail
                tracing::debug!("maxi-memory not available: {}", e);
                Err(anyhow!("maxi-memory not available"))
            }
        }
    }

    /// Call a tool on maxi-memory
    async fn call_tool(&self, tool_name: &str, arguments: Value) -> Result<Value> {
        self.ensure_connected().await?;
        
        let mut guard = self.child.lock().await;
        let child = guard.as_mut().ok_or_else(|| anyhow!("No connection"))?;
        
        let stdin = child.stdin.as_mut().ok_or_else(|| anyhow!("No stdin"))?;
        let stdout = child.stdout.as_mut().ok_or_else(|| anyhow!("No stdout"))?;
        
        let request = json!({
            "jsonrpc": "2.0",
            "id": REQUEST_ID.fetch_add(1, Ordering::Relaxed),
            "method": "tools/call",
            "params": {
                "name": tool_name,
                "arguments": arguments
            }
        });
        
        let msg = serde_json::to_string(&request)? + "\n";
        stdin.write_all(msg.as_bytes()).await?;
        
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        reader.read_line(&mut line).await?;
        
        let response: Value = serde_json::from_str(&line)?;
        
        if let Some(error) = response.get("error") {
            return Err(anyhow!("MCP error: {}", error));
        }
        
        Ok(response.get("result").cloned().unwrap_or(Value::Null))
    }

    /// Record a command's execution duration (fire-and-forget)
    pub async fn record_cmd_duration(&self, command: &str, duration_ms: u64) {
        let result = self.call_tool("cmd_record", json!({
            "command": command,
            "duration_ms": duration_ms
        })).await;
        
        if let Err(e) = result {
            tracing::debug!("Failed to record cmd duration: {}", e);
        }
    }

    /// Get estimated wait time for a command
    pub async fn get_cmd_estimate(&self, command: &str) -> Option<CmdEstimate> {
        let result = self.call_tool("cmd_estimate", json!({
            "command": command
        })).await;
        
        match result {
            Ok(value) => {
                // Parse the nested content structure
                let content = value.get("content")
                    .and_then(|c| c.as_array())
                    .and_then(|arr| arr.first())
                    .and_then(|item| item.get("text"))
                    .and_then(|t| t.as_str())
                    .and_then(|s| serde_json::from_str::<Value>(s).ok())?;
                
                Some(CmdEstimate {
                    estimate_ms: content.get("estimate_ms").and_then(|v| v.as_u64()).unwrap_or(5000),
                    hint: content.get("hint").and_then(|v| v.as_str()).unwrap_or("~5s").to_string(),
                    samples: content.get("samples").and_then(|v| v.as_u64()).unwrap_or(0) as usize,
                })
            }
            Err(e) => {
                tracing::debug!("Failed to get cmd estimate: {}", e);
                None
            }
        }
    }
}

/// Command duration estimate
#[derive(Debug, Clone)]
pub struct CmdEstimate {
    pub estimate_ms: u64,
    pub hint: String,
    pub samples: usize,
}

impl CmdEstimate {
    /// Format as a hint string for the agent
    pub fn as_hint(&self) -> String {
        if self.samples > 0 {
            format!("STOP POLLING. Task running. Sleep for {} then check once. Do NOT poll in a loop.", self.hint)
        } else {
            "STOP POLLING. Task running. Sleep for ~10s then check once. Do NOT poll repeatedly.".to_string()
        }
    }
}
