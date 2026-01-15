use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Instant;

use tokio::sync::RwLock;

// Short session IDs (s1, s2, s3...)
static SESSION_COUNTER: AtomicU32 = AtomicU32::new(1);
fn next_session_id() -> String {
    format!("s{}", SESSION_COUNTER.fetch_add(1, Ordering::Relaxed))
}
use tokio::time::{timeout, Duration};

use crate::output::{process_output, OutputBuffer, OutputFormat};
use crate::process::{register_pid, spawn_isolated, unregister_pid, SpawnConfig};
use crate::shell::Shell;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: String,
    pub shell: Shell,
    pub cwd: PathBuf,
    pub env: HashMap<String, String>,
    pub created_at_ms: u64,
}

struct SessionInner {
    info: SessionInfo,
}

pub struct SessionManager {
    sessions: Arc<RwLock<HashMap<String, SessionInner>>>,
}

impl Default for SessionManager {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandResult {
    #[serde(rename = "exit")]
    pub exit_code: Option<i32>,
    #[serde(rename = "out")]
    pub stdout: OutputBuffer,
    #[serde(rename = "err")]
    pub stderr: OutputBuffer,
    #[serde(rename = "dur")]
    pub duration_ms: u64,
}

impl SessionManager {
    pub fn new() -> Self {
        Self {
            sessions: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub async fn create_session(
        &self,
        shell: Shell,
        cwd: Option<PathBuf>,
        env: Option<HashMap<String, String>>,
    ) -> Result<String> {
        let _ = which::which(shell.executable_name())
            .map_err(|_| anyhow!("Shell {} not found", shell.executable_name()))?;

        let working_dir =
            cwd.unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")));
        let env_vars = env.unwrap_or_default();

        let session_id = next_session_id();

        let info = SessionInfo {
            id: session_id.clone(),
            shell,
            cwd: working_dir.clone(),
            env: env_vars.clone(),
            created_at_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64,
        };

        let inner = SessionInner { info };

        let mut sessions = self.sessions.write().await;
        sessions.insert(session_id.clone(), inner);

        Ok(session_id)
    }

    pub async fn run_in_session(
        &self,
        session_id: &str,
        command: &str,
        timeout_ms: Option<u64>,
        format: OutputFormat,
        max_output: Option<usize>,
    ) -> Result<CommandResult> {
        let (shell, cwd, env) = {
            let sessions = self.sessions.read().await;
            let session = sessions
                .get(session_id)
                .ok_or_else(|| anyhow!("Session not found"))?;
            (
                session.info.shell,
                session.info.cwd.clone(),
                session.info.env.clone(),
            )
        };

        // Check if this is a simple "cd" command - handle it without spawning a shell
        let trimmed = command.trim();
        if let Some(new_cwd) = Self::parse_cd_command(trimmed, &cwd) {
            if new_cwd.exists() && new_cwd.is_dir() {
                self.update_session_cwd(session_id, new_cwd).await?;
                return Ok(CommandResult {
                    exit_code: Some(0),
                    stdout: OutputBuffer::new(Vec::new(), 0, false),
                    stderr: OutputBuffer::new(Vec::new(), 0, false),
                    duration_ms: 0,
                });
            } else {
                return Ok(CommandResult {
                    exit_code: Some(1),
                    stdout: OutputBuffer::new(Vec::new(), 0, false),
                    stderr: process_output(
                        format!("cd: no such directory: {}", trimmed).as_bytes(),
                        format,
                        max_output,
                    ),
                    duration_ms: 0,
                });
            }
        }

        let shell_path = which::which(shell.executable_name())?;
        let args = shell.command_args(command);

        let config = SpawnConfig {
            program: shell_path,
            args,
            cwd: Some(cwd.clone()),
            env: Some(env),
            new_process_group: true,
        };

        let started = Instant::now();
        let child = spawn_isolated(&config)?;
        let pid = child.id();

        if let Some(p) = pid {
            register_pid(p).await;
        }

        let wait_future = child.wait_with_output();

        let result = if let Some(ms) = timeout_ms {
            match timeout(Duration::from_millis(ms), wait_future).await {
                Ok(r) => r?,
                Err(_) => {
                    if let Some(p) = pid {
                        let _ = crate::process::kill_process_tree(p, true).await;
                    }
                    return Err(anyhow!("Command timed out after {}ms", ms));
                }
            }
        } else {
            wait_future.await?
        };

        if let Some(p) = pid {
            unregister_pid(p).await;
        }

        let duration_ms = started.elapsed().as_millis() as u64;

        let stdout = process_output(&result.stdout, format, max_output);
        let stderr = process_output(&result.stderr, format, max_output);

        // Try to extract cwd changes from compound commands containing cd
        if result.status.success() {
            if let Some(new_cwd) = Self::extract_cd_from_compound(command, &cwd) {
                if new_cwd.exists() && new_cwd.is_dir() {
                    let _ = self.update_session_cwd(session_id, new_cwd).await;
                }
            }
        }

        Ok(CommandResult {
            exit_code: result.status.code(),
            stdout,
            stderr,
            duration_ms,
        })
    }

    /// Parse a simple "cd <path>" command and return the new directory
    fn parse_cd_command(command: &str, current_cwd: &PathBuf) -> Option<PathBuf> {
        let parts: Vec<&str> = command.split_whitespace().collect();
        if parts.len() > 2 || parts.is_empty() {
            return None; // Not a simple cd command
        }
        if parts[0] != "cd" {
            return None;
        }
        
        let target = parts.get(1).copied().unwrap_or("~");
        let new_dir = if target == "~" {
            std::env::var("USERPROFILE")
                .or_else(|_| std::env::var("HOME"))
                .map(PathBuf::from)
                .unwrap_or_else(|_| PathBuf::from("/"))
        } else if target == "-" {
            return None; // Can't handle "cd -" without history
        } else {
            let path = PathBuf::from(target);
            if path.is_absolute() {
                path
            } else {
                current_cwd.join(path)
            }
        };
        
        // Canonicalize to resolve .. and . components
        new_dir.canonicalize().ok()
    }

    /// Extract cd target from compound commands like "cd /foo && pwd"
    fn extract_cd_from_compound(command: &str, current_cwd: &PathBuf) -> Option<PathBuf> {
        // Look for "cd <path>" at the start of compound commands
        let command = command.trim();
        
        // Handle "cd /path && ..." or "cd /path; ..."
        for sep in ["&&", ";", "||"] {
            if let Some(first_cmd) = command.split(sep).next() {
                let first_cmd = first_cmd.trim();
                if first_cmd.starts_with("cd ") {
                    return Self::parse_cd_command(first_cmd, current_cwd);
                }
            }
        }
        
        None
    }

    pub async fn get_session(&self, session_id: &str) -> Option<SessionInfo> {
        let sessions = self.sessions.read().await;
        sessions.get(session_id).map(|s| s.info.clone())
    }

    pub async fn destroy_session(&self, session_id: &str) -> bool {
        let mut sessions = self.sessions.write().await;
        sessions.remove(session_id).is_some()
    }

    pub async fn list_sessions(&self) -> Vec<SessionInfo> {
        let sessions = self.sessions.read().await;
        sessions.values().map(|s| s.info.clone()).collect()
    }

    pub async fn update_session_cwd(&self, session_id: &str, new_cwd: PathBuf) -> Result<()> {
        let mut sessions = self.sessions.write().await;
        let session = sessions
            .get_mut(session_id)
            .ok_or_else(|| anyhow!("Session not found"))?;
        session.info.cwd = new_cwd;
        Ok(())
    }

    pub async fn update_session_env(
        &self,
        session_id: &str,
        key: String,
        value: String,
    ) -> Result<()> {
        let mut sessions = self.sessions.write().await;
        let session = sessions
            .get_mut(session_id)
            .ok_or_else(|| anyhow!("Session not found"))?;
        session.info.env.insert(key, value);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shell::detect_default_shell;

    #[tokio::test]
    async fn test_create_and_run_session() {
        let manager = SessionManager::new();
        let (shell, _) = detect_default_shell().expect("Need shell");

        let session_id = manager
            .create_session(shell, None, None)
            .await
            .expect("Should create session");

        let result = manager
            .run_in_session(
                &session_id,
                "echo hello",
                Some(5000),
                OutputFormat::Compact,
                None,
            )
            .await
            .expect("Should run command");

        assert_eq!(result.exit_code, Some(0));
        assert!(result.stdout.as_string_lossy().contains("hello"));
    }

    #[tokio::test]
    async fn test_session_preserves_env() {
        let manager = SessionManager::new();
        let (shell, _) = detect_default_shell().expect("Need shell");

        let session_id = manager
            .create_session(shell, None, None)
            .await
            .expect("Should create session");

        manager
            .update_session_env(&session_id, "TEST_VAR".into(), "test_value".into())
            .await
            .expect("Should update env");

        let cmd = if cfg!(windows) {
            "echo %TEST_VAR%"
        } else {
            "echo $TEST_VAR"
        };
        let result = manager
            .run_in_session(&session_id, cmd, Some(5000), OutputFormat::Compact, None)
            .await
            .expect("Should run command");

        assert!(result.stdout.as_string_lossy().contains("test_value"));
    }

    #[tokio::test]
    async fn test_destroy_session() {
        let manager = SessionManager::new();
        let (shell, _) = detect_default_shell().expect("Need shell");

        let session_id = manager
            .create_session(shell, None, None)
            .await
            .expect("Should create");

        assert!(manager.get_session(&session_id).await.is_some());
        assert!(manager.destroy_session(&session_id).await);
        assert!(manager.get_session(&session_id).await.is_none());
    }

    #[tokio::test]
    async fn test_list_sessions() {
        let manager = SessionManager::new();
        let (shell, _) = detect_default_shell().expect("Need shell");

        let _ = manager.create_session(shell, None, None).await;
        let _ = manager.create_session(shell, None, None).await;

        let sessions = manager.list_sessions().await;
        assert_eq!(sessions.len(), 2);
    }
}

