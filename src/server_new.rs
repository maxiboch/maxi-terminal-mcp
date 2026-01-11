use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot, Mutex};

use crate::cache::OutputCache;
use crate::elicitation::{ClientCapabilities, ElicitAction, ElicitationManager, RequestSchema, schemas};
use crate::output::{OutputFormat, DEFAULT_MAX_BYTES};
use crate::path_utils::normalize_path;
use crate::process::{
    cleanup_all_processes, register_pid, spawn_isolated, unregister_pid, SpawnConfig,
};
use crate::session::SessionManager;
use crate::shell::{
    detect_available_package_managers, detect_available_shells, detect_default_shell, ensure_shell,
    get_install_instructions, Shell,
};
use crate::task::{OutputStream, TaskManager};

/// Channel for sending outgoing messages (elicitation requests)
type OutgoingTx = mpsc::UnboundedSender<Value>;

pub struct McpServer {
    task_manager: TaskManager,
    session_manager: SessionManager,
    output_cache: OutputCache,
    client_capabilities: Arc<Mutex<ClientCapabilities>>,
    elicitation_manager: Arc<ElicitationManager>,
    outgoing_tx: Arc<Mutex<Option<OutgoingTx>>>,
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
            outgoing_tx: Arc::new(Mutex::new(None)),
        }
    }

    pub async fn run(&self) -> Result<()> {
        let stdin = std::io::stdin();
        let mut stdout = std::io::stdout();

        // Create channel for outgoing messages
        let (tx, mut rx) = mpsc::unbounded_channel::<Value>();
        *self.outgoing_tx.lock().await = Some(tx);

        // Spawn a task to handle outgoing messages
        let stdout_handle = tokio::task::spawn_blocking({
            let mut stdout = std::io::stdout();
            move || {
                while let Some(msg) = rx.blocking_recv() {
                    if let Ok(response_str) = serde_json::to_string(&msg) {
                        let _ = writeln!(stdout, "{}", response_str);
                        let _ = stdout.flush();
                    }
                }
            }
        });

        for line in stdin.lock().lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }

            let msg: Value = serde_json::from_str(&line)?;
            
            // Check if this is a response to an elicitation request we sent
            if let Some((id, result)) = ElicitationManager::is_elicitation_response(&msg) {
                self.elicitation_manager.handle_response(id, result).await;
                continue;
            }

            // Otherwise, handle as a normal request
            let response = self.handle_request(msg).await;

            let response_str = serde_json::to_string(&response)?;
            writeln!(stdout, "{}", response_str)?;
            stdout.flush()?;
        }

        // Clean up
        *self.outgoing_tx.lock().await = None;
        drop(stdout_handle);

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
        let has_elicitation = caps.elicitation;
        *self.client_capabilities.lock().await = caps;

        Ok(json!({
            "protocolVersion": "2024-11-05",
            "capabilities": { 
                "tools": {},
                // Advertise that we can use elicitation if client supports it
                "elicitation": if has_elicitation { json!({}) } else { json!(null) }
            },
            "serverInfo": { "name": "maxi-terminal-mcp", "version": env!("CARGO_PKG_VERSION") }
        }))
    }

    async fn handle_tools_list(&self) -> Result<Value> {
        let has_elicitation = self.client_capabilities.lock().await.elicitation;
        
        Ok(json!({
            "tools": [
                {
                    "name": "run",
                    "description": "Execute shell command. Use background:true for long-running commands (returns task_id). Returns 'oid' (output ID) in response for all runs, which can be used with 'task' action:output to page through full output if truncated. POWERSHELL TIP: Avoid $_ pipeline variable (may get stripped). Use direct property access: (Get-Process).WorkingSet64 instead of Get-Process | ForEach-Object { $_.WorkingSet64 }",
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
                    "description": "Manage background tasks or retrieve output from any run. Works with BOTH 'task_id' (from background:true) and 'oid' (from any run). The 'output' action supports pagination via 'offset' and 'limit' params, and returns 'has_more: true' if more content is available.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "action": { "type": "string", "enum": ["status", "output", "kill", "list"] },
                            "task_id": { "type": "string", "description": "Task ID or output_id from run command" },
                            "stream": { "type": "string", "enum": ["stdout", "stderr", "both"], "default": "both" },
                            "offset": { "type": "integer", "default": 0 },
                            "limit": { "type": "integer", "default": 50000 },
                            "format": { "type": "string", "enum": ["raw", "compact", "summary"], "default": "compact" },
                            "force": { "type": "boolean", "default": false, "description": "For kill: use SIGKILL" }
                        },
                        "required": ["action"]
                    }
                },
                {
                    "name": "session",
                    "description": "Persistent shell sessions: create, run command, destroy, or list. Sessions preserve working directory and environment variables across multiple commands.",
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
                            "Uses MCP elicitation (client supported)."
                        } else {
                            "Requires TTY - will fail in non-interactive environments."
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

        match name {
            "run" => self.tool_run(&args).await,
            "task" => self.tool_task(&args).await,
            "session" => self.tool_session(&args).await,
            "shell" => self.tool_shell(&args).await,
            "interact" => self.tool_interact(&args).await,
            _ => Err(anyhow!("Unknown tool: {}", name)),
        }
    }
