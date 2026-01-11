use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::sync::Arc;
use tokio::sync::Mutex;

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

pub struct McpServer {
    task_manager: TaskManager,
    session_manager: SessionManager,
    output_cache: OutputCache,
    client_capabilities: Arc<Mutex<ClientCapabilities>>,
    elicitation_manager: Arc<ElicitationManager>,
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
        }
    }

    pub async fn run(&self) -> Result<()> {
        let stdin = std::io::stdin();
        let mut stdout = std::io::stdout();

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

            // Don't send responses for notifications (they return null)
            if response.is_null() {
                continue;
            }

            let response_str = serde_json::to_string(&response)?;
            writeln!(stdout, "{}", response_str)?;
            stdout.flush()?;
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
                    "description": "Manage background tasks: status, output (paginated), kill, or list all.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "action": { "type": "string", "enum": ["status", "output", "kill", "list"] },
                            "task_id": { "type": "string", "description": "Required for status/output/kill" },
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

            return self.ok(&json!({ "task_id": task_id }));
        }

        let format = self.parse_format(args.get("format"));
        let max_output = args
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
        let parsed = crate::parser::parse_output(&stdout_text, &stderr_text, output.status.code());

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
                self.ok(&json!({
                    "st": task.status,
                    "exit": task.exit_code,
                    "dur": task.duration_ms,
                    "pid": task.pid,
                    "done": output_complete
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
                let processed = crate::output::process_output(&data, format, Some(limit));
                self.ok(&json!({
                    "output": processed.as_string_lossy(),
                    "has_more": has_more,
                    "total_bytes": processed.total_bytes,
                    "truncated": processed.truncated
                }))
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
                let parsed = crate::parser::parse_output(&stdout_text, &stderr_text, result.exit_code);
                
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
        let mut stdout = std::io::stdout();
        writeln!(stdout, "{}", request_str)?;
        stdout.flush()?;

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

    fn ok(&self, data: &Value) -> Result<Value> {
        Ok(json!({"content": [{"type": "text", "text": serde_json::to_string(data)?}]}))
    }
}
