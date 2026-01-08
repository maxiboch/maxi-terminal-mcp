use anyhow::{anyhow, Result};

use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::PathBuf;

use crate::output::{OutputFormat, DEFAULT_MAX_BYTES};
use crate::session::SessionManager;
use crate::shell::{
    detect_available_package_managers, detect_available_shells, detect_default_shell, ensure_shell,
    get_install_instructions, Shell,
};
use crate::task::{OutputStream, TaskManager};

pub struct McpServer {
    task_manager: TaskManager,
    session_manager: SessionManager,
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

            let request: Value = serde_json::from_str(&line)?;
            let response = self.handle_request(request).await;

            let response_str = serde_json::to_string(&response)?;
            writeln!(stdout, "{}", response_str)?;
            stdout.flush()?;
        }

        Ok(())
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
                "error": {
                    "code": -32000,
                    "message": e.to_string()
                }
            }),
        }
    }

    async fn handle_initialize(&self, _params: &Value) -> Result<Value> {
        Ok(json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {
                "tools": {}
            },
            "serverInfo": {
                "name": "maxi-terminal-mcp",
                "version": env!("CARGO_PKG_VERSION")
            }
        }))
    }

    async fn handle_tools_list(&self) -> Result<Value> {
        Ok(json!({
            "tools": [
                {
                    "name": "run",
                    "description": "Execute command (stateless). Returns output with token-efficient processing.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "command": { "type": "string", "description": "Command to execute" },
                            "shell": { "type": "string", "enum": ["bash", "zsh", "fish", "powershell_core", "powershell", "cmd", "sh", "nushell"], "description": "Shell to use (auto-detected if omitted)" },
                            "cwd": { "type": "string", "description": "Working directory" },
                            "env": { "type": "object", "description": "Environment variables" },
                            "timeout_ms": { "type": "integer", "description": "Timeout in milliseconds" },
                            "format": { "type": "string", "enum": ["raw", "compact", "summary"], "default": "compact" },
                            "max_output": { "type": "integer", "description": "Max output bytes (default 50KB)" }
                        },
                        "required": ["command"]
                    }
                },
                {
                    "name": "run_background",
                    "description": "Start long-running command in background. Returns task_id for monitoring.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "command": { "type": "string" },
                            "shell": { "type": "string", "enum": ["bash", "zsh", "fish", "powershell_core", "powershell", "cmd", "sh", "nushell"] },
                            "cwd": { "type": "string" },
                            "env": { "type": "object" },
                            "timeout_ms": { "type": "integer" }
                        },
                        "required": ["command"]
                    }
                },
                {
                    "name": "status",
                    "description": "Check background task status",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "task_id": { "type": "string" }
                        },
                        "required": ["task_id"]
                    }
                },
                {
                    "name": "output",
                    "description": "Get output from background task with pagination",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "task_id": { "type": "string" },
                            "stream": { "type": "string", "enum": ["stdout", "stderr", "both"], "default": "both" },
                            "offset": { "type": "integer", "default": 0 },
                            "limit": { "type": "integer", "default": 50000 },
                            "format": { "type": "string", "enum": ["raw", "compact", "summary"], "default": "compact" }
                        },
                        "required": ["task_id"]
                    }
                },
                {
                    "name": "kill",
                    "description": "Terminate a background task",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "task_id": { "type": "string" },
                            "force": { "type": "boolean", "default": false, "description": "Use SIGKILL instead of SIGTERM" }
                        },
                        "required": ["task_id"]
                    }
                },
                {
                    "name": "session_create",
                    "description": "Create persistent shell session (preserves env/cwd between commands)",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "shell": { "type": "string", "enum": ["bash", "zsh", "fish", "powershell_core", "powershell", "cmd", "sh", "nushell"] },
                            "cwd": { "type": "string" },
                            "env": { "type": "object" }
                        }
                    }
                },
                {
                    "name": "session_run",
                    "description": "Run command in persistent session",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "session_id": { "type": "string" },
                            "command": { "type": "string" },
                            "timeout_ms": { "type": "integer" },
                            "format": { "type": "string", "enum": ["raw", "compact", "summary"], "default": "compact" },
                            "max_output": { "type": "integer" }
                        },
                        "required": ["session_id", "command"]
                    }
                },
                {
                    "name": "session_destroy",
                    "description": "Destroy a persistent session",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "session_id": { "type": "string" }
                        },
                        "required": ["session_id"]
                    }
                },
                {
                    "name": "session_list",
                    "description": "List all active sessions",
                    "inputSchema": { "type": "object", "properties": {} }
                },
                {
                    "name": "detect_shell",
                    "description": "Detect available shells on the system",
                    "inputSchema": { "type": "object", "properties": {} }
                },
                {
                    "name": "install_shell",
                    "description": "Install a shell using available package manager",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "shell": { "type": "string", "enum": ["bash", "zsh", "fish", "powershell_core", "nushell"] },
                            "dry_run": { "type": "boolean", "default": false, "description": "Only show install command, don't execute" }
                        },
                        "required": ["shell"]
                    }
                },
                {
                    "name": "tasks_list",
                    "description": "List all background tasks",
                    "inputSchema": { "type": "object", "properties": {} }
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
            "run_background" => self.tool_run_background(&args).await,
            "status" => self.tool_status(&args).await,
            "output" => self.tool_output(&args).await,
            "kill" => self.tool_kill(&args).await,
            "session_create" => self.tool_session_create(&args).await,
            "session_run" => self.tool_session_run(&args).await,
            "session_destroy" => self.tool_session_destroy(&args).await,
            "session_list" => self.tool_session_list(&args).await,
            "detect_shell" => self.tool_detect_shell(&args).await,
            "install_shell" => self.tool_install_shell(&args).await,
            "tasks_list" => self.tool_tasks_list(&args).await,
            _ => Err(anyhow!("Unknown tool: {}", name)),
        }
    }

    async fn tool_run(&self, args: &Value) -> Result<Value> {
        let command = args
            .get("command")
            .and_then(|c| c.as_str())
            .ok_or_else(|| anyhow!("Missing command"))?;

        let shell = self.resolve_shell(args.get("shell"))?;
        let shell_path = which::which(shell.executable_name())?;
        let shell_args = shell.command_args(command);

        let cwd = args.get("cwd").and_then(|c| c.as_str()).map(PathBuf::from);
        let env: Option<HashMap<String, String>> = args
            .get("env")
            .and_then(|e| serde_json::from_value(e.clone()).ok());
        let timeout_ms = args.get("timeout_ms").and_then(|t| t.as_u64());
        let format = self.parse_format(args.get("format"));
        let max_output = args
            .get("max_output")
            .and_then(|m| m.as_u64())
            .map(|m| m as usize);

        let mut cmd = tokio::process::Command::new(&shell_path);
        cmd.args(&shell_args)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .stdin(std::process::Stdio::null());

        if let Some(dir) = cwd {
            cmd.current_dir(dir);
        }
        if let Some(vars) = env {
            cmd.envs(vars);
        }

        let started = std::time::Instant::now();

        let output = if let Some(ms) = timeout_ms {
            match tokio::time::timeout(tokio::time::Duration::from_millis(ms), cmd.output()).await {
                Ok(r) => r?,
                Err(_) => return Err(anyhow!("Command timed out after {}ms", ms)),
            }
        } else {
            cmd.output().await?
        };

        let duration_ms = started.elapsed().as_millis() as u64;

        let stdout = crate::output::process_output(&output.stdout, format, max_output);
        let stderr = crate::output::process_output(&output.stderr, format, max_output);

        Ok(json!({
            "content": [{
                "type": "text",
                "text": serde_json::to_string(&json!({
                    "exit_code": output.status.code(),
                    "stdout": stdout.as_string_lossy(),
                    "stderr": stderr.as_string_lossy(),
                    "duration_ms": duration_ms,
                    "truncated": stdout.truncated || stderr.truncated
                }))?
            }]
        }))
    }

    async fn tool_run_background(&self, args: &Value) -> Result<Value> {
        let command = args
            .get("command")
            .and_then(|c| c.as_str())
            .ok_or_else(|| anyhow!("Missing command"))?;

        let shell = self.resolve_shell(args.get("shell"))?;
        let shell_path = which::which(shell.executable_name())?;
        let shell_args = shell.command_args(command);

        let cwd = args.get("cwd").and_then(|c| c.as_str()).map(PathBuf::from);
        let env: Option<HashMap<String, String>> = args
            .get("env")
            .and_then(|e| serde_json::from_value(e.clone()).ok());
        let timeout_ms = args.get("timeout_ms").and_then(|t| t.as_u64());

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

        Ok(json!({
            "content": [{
                "type": "text",
                "text": serde_json::to_string(&json!({ "task_id": task_id }))?
            }]
        }))
    }

    async fn tool_status(&self, args: &Value) -> Result<Value> {
        let task_id = args
            .get("task_id")
            .and_then(|t| t.as_str())
            .ok_or_else(|| anyhow!("Missing task_id"))?;

        let task = self
            .task_manager
            .get_task(task_id)
            .await
            .ok_or_else(|| anyhow!("Task not found"))?;

        Ok(json!({
            "content": [{
                "type": "text",
                "text": serde_json::to_string(&json!({
                    "status": task.status,
                    "exit_code": task.exit_code,
                    "duration_ms": task.duration_ms
                }))?
            }]
        }))
    }

    async fn tool_output(&self, args: &Value) -> Result<Value> {
        let task_id = args
            .get("task_id")
            .and_then(|t| t.as_str())
            .ok_or_else(|| anyhow!("Missing task_id"))?;

        let stream = match args.get("stream").and_then(|s| s.as_str()) {
            Some("stdout") => OutputStream::Stdout,
            Some("stderr") => OutputStream::Stderr,
            _ => OutputStream::Both,
        };

        let offset = args.get("offset").and_then(|o| o.as_u64()).unwrap_or(0) as usize;
        let limit = args
            .get("limit")
            .and_then(|l| l.as_u64())
            .unwrap_or(DEFAULT_MAX_BYTES as u64) as usize;
        let format = self.parse_format(args.get("format"));

        let (data, has_more) = self
            .task_manager
            .get_output(task_id, stream, offset, limit)
            .await
            .ok_or_else(|| anyhow!("Task not found"))?;

        let processed = crate::output::process_output(&data, format, Some(limit));

        Ok(json!({
            "content": [{
                "type": "text",
                "text": serde_json::to_string(&json!({
                    "output": processed.as_string_lossy(),
                    "has_more": has_more,
                    "total_bytes": processed.total_bytes,
                    "truncated": processed.truncated
                }))?
            }]
        }))
    }

    async fn tool_kill(&self, args: &Value) -> Result<Value> {
        let task_id = args
            .get("task_id")
            .and_then(|t| t.as_str())
            .ok_or_else(|| anyhow!("Missing task_id"))?;

        let force = args.get("force").and_then(|f| f.as_bool()).unwrap_or(false);
        let killed = self.task_manager.kill_task(task_id, force).await?;

        Ok(json!({
            "content": [{
                "type": "text",
                "text": serde_json::to_string(&json!({ "killed": killed }))?
            }]
        }))
    }

    async fn tool_session_create(&self, args: &Value) -> Result<Value> {
        let shell = self.resolve_shell(args.get("shell"))?;
        let cwd = args.get("cwd").and_then(|c| c.as_str()).map(PathBuf::from);
        let env: Option<HashMap<String, String>> = args
            .get("env")
            .and_then(|e| serde_json::from_value(e.clone()).ok());

        let session_id = self.session_manager.create_session(shell, cwd, env).await?;

        Ok(json!({
            "content": [{
                "type": "text",
                "text": serde_json::to_string(&json!({ "session_id": session_id }))?
            }]
        }))
    }

    async fn tool_session_run(&self, args: &Value) -> Result<Value> {
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

        Ok(json!({
            "content": [{
                "type": "text",
                "text": serde_json::to_string(&json!({
                    "exit_code": result.exit_code,
                    "stdout": result.stdout.as_string_lossy(),
                    "stderr": result.stderr.as_string_lossy(),
                    "duration_ms": result.duration_ms,
                    "truncated": result.stdout.truncated || result.stderr.truncated
                }))?
            }]
        }))
    }

    async fn tool_session_destroy(&self, args: &Value) -> Result<Value> {
        let session_id = args
            .get("session_id")
            .and_then(|s| s.as_str())
            .ok_or_else(|| anyhow!("Missing session_id"))?;

        let destroyed = self.session_manager.destroy_session(session_id).await;

        Ok(json!({
            "content": [{
                "type": "text",
                "text": serde_json::to_string(&json!({ "destroyed": destroyed }))?
            }]
        }))
    }

    async fn tool_session_list(&self, _args: &Value) -> Result<Value> {
        let sessions = self.session_manager.list_sessions().await;

        Ok(json!({
            "content": [{
                "type": "text",
                "text": serde_json::to_string(&sessions)?
            }]
        }))
    }

    async fn tool_detect_shell(&self, _args: &Value) -> Result<Value> {
        let available = detect_available_shells();
        let default = detect_default_shell();
        let package_managers = detect_available_package_managers();

        Ok(json!({
            "content": [{
                "type": "text",
                "text": serde_json::to_string(&json!({
                    "default_shell": default.map(|(s, p)| json!({
                        "shell": s,
                        "path": p.to_string_lossy()
                    })),
                    "available_shells": available.iter().map(|(s, p)| json!({
                        "shell": s,
                        "path": p.to_string_lossy()
                    })).collect::<Vec<_>>(),
                    "package_managers": package_managers.iter().map(|(pm, p)| json!({
                        "name": pm,
                        "path": p.to_string_lossy()
                    })).collect::<Vec<_>>()
                }))?
            }]
        }))
    }

    async fn tool_install_shell(&self, args: &Value) -> Result<Value> {
        let shell_str = args
            .get("shell")
            .and_then(|s| s.as_str())
            .ok_or_else(|| anyhow!("Missing shell"))?;

        let shell: Shell = serde_json::from_value(json!(shell_str))?;
        let dry_run = args
            .get("dry_run")
            .and_then(|d| d.as_bool())
            .unwrap_or(false);

        let instructions = get_install_instructions(shell);
        let available_pms = detect_available_package_managers();

        if dry_run {
            let commands: Vec<_> = available_pms
                .iter()
                .filter_map(|(pm, _)| {
                    instructions
                        .get(pm)
                        .map(|cmd| json!({ "pm": pm, "command": cmd }))
                })
                .collect();

            return Ok(json!({
                "content": [{
                    "type": "text",
                    "text": serde_json::to_string(&json!({
                        "dry_run": true,
                        "install_commands": commands
                    }))?
                }]
            }));
        }

        let path = ensure_shell(shell).await?;

        Ok(json!({
            "content": [{
                "type": "text",
                "text": serde_json::to_string(&json!({
                    "installed": true,
                    "path": path.to_string_lossy()
                }))?
            }]
        }))
    }

    async fn tool_tasks_list(&self, _args: &Value) -> Result<Value> {
        let tasks = self.task_manager.list_tasks().await;

        let task_summaries: Vec<_> = tasks
            .iter()
            .map(|t| {
                json!({
                    "id": t.id,
                    "command": t.command,
                    "status": t.status,
                    "exit_code": t.exit_code,
                    "duration_ms": t.duration_ms
                })
            })
            .collect();

        Ok(json!({
            "content": [{
                "type": "text",
                "text": serde_json::to_string(&task_summaries)?
            }]
        }))
    }

    fn resolve_shell(&self, shell_arg: Option<&Value>) -> Result<Shell> {
        if let Some(s) = shell_arg.and_then(|s| s.as_str()) {
            let shell: Shell = serde_json::from_value(json!(s))?;
            return Ok(shell);
        }

        detect_default_shell()
            .map(|(s, _)| s)
            .ok_or_else(|| anyhow!("No shell available"))
    }

    fn parse_format(&self, format_arg: Option<&Value>) -> OutputFormat {
        match format_arg.and_then(|f| f.as_str()) {
            Some("raw") => OutputFormat::Raw,
            Some("summary") => OutputFormat::Summary,
            _ => OutputFormat::Compact,
        }
    }
}
