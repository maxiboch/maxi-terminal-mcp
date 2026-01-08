use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;
use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};
use tokio::sync::RwLock;
use tokio::time::{timeout, Duration};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Pending,
    Running,
    Completed,
    Failed,
    Timeout,
    Killed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputStream {
    Stdout,
    Stderr,
    Both,
}

#[derive(Debug)]
struct TaskInner {
    child: Option<Child>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskInfo {
    pub id: String,
    pub command: String,
    pub status: TaskStatus,
    pub exit_code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    #[serde(skip)]
    pub started_at: Option<Instant>,
    #[serde(skip)]
    pub completed_at: Option<Instant>,
    pub duration_ms: Option<u64>,
}

impl TaskInfo {
    fn new(id: String, command: String) -> Self {
        Self {
            id,
            command,
            status: TaskStatus::Pending,
            exit_code: None,
            stdout: Vec::new(),
            stderr: Vec::new(),
            started_at: None,
            completed_at: None,
            duration_ms: None,
        }
    }

    fn compute_duration(&mut self) {
        if let (Some(started), Some(completed)) = (self.started_at, self.completed_at) {
            self.duration_ms = Some(completed.duration_since(started).as_millis() as u64);
        }
    }
}

struct ManagedTask {
    info: TaskInfo,
    inner: Option<TaskInner>,
}

pub struct TaskManager {
    tasks: Arc<RwLock<HashMap<String, ManagedTask>>>,
}

impl Default for TaskManager {
    fn default() -> Self {
        Self::new()
    }
}

impl TaskManager {
    pub fn new() -> Self {
        Self {
            tasks: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub async fn spawn_task(
        &self,
        command: String,
        shell_path: PathBuf,
        shell_args: Vec<String>,
        timeout_ms: Option<u64>,
        cwd: Option<PathBuf>,
        env: Option<HashMap<String, String>>,
    ) -> Result<String> {
        let task_id = uuid::Uuid::new_v4().to_string();
        let mut info = TaskInfo::new(task_id.clone(), command.clone());

        let mut cmd = Command::new(&shell_path);
        cmd.args(&shell_args)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .stdin(std::process::Stdio::null());

        if let Some(dir) = cwd {
            cmd.current_dir(dir);
        }

        if let Some(env_vars) = env {
            cmd.envs(env_vars);
        }

        let child = cmd.spawn()?;
        info.status = TaskStatus::Running;
        info.started_at = Some(Instant::now());

        let managed = ManagedTask {
            info,
            inner: Some(TaskInner { child: Some(child) }),
        };

        {
            let mut tasks = self.tasks.write().await;
            tasks.insert(task_id.clone(), managed);
        }

        let tasks = Arc::clone(&self.tasks);
        let id = task_id.clone();
        let timeout_duration = timeout_ms.map(Duration::from_millis);

        tokio::spawn(async move {
            Self::run_task(tasks, id, timeout_duration).await;
        });

        Ok(task_id)
    }

    async fn run_task(
        tasks: Arc<RwLock<HashMap<String, ManagedTask>>>,
        task_id: String,
        timeout_duration: Option<Duration>,
    ) {
        let child = {
            let mut tasks_guard = tasks.write().await;
            if let Some(task) = tasks_guard.get_mut(&task_id) {
                task.inner.as_mut().and_then(|i| i.child.take())
            } else {
                return;
            }
        };

        let Some(mut child) = child else { return };

        let mut stdout_handle = child.stdout.take();
        let mut stderr_handle = child.stderr.take();

        let wait_future = async {
            let mut stdout_data = Vec::new();
            let mut stderr_data = Vec::new();

            if let Some(ref mut stdout) = stdout_handle {
                let _ = stdout.read_to_end(&mut stdout_data).await;
            }
            if let Some(ref mut stderr) = stderr_handle {
                let _ = stderr.read_to_end(&mut stderr_data).await;
            }

            let status = child.wait().await;
            (status, stdout_data, stderr_data)
        };

        let result = if let Some(duration) = timeout_duration {
            match timeout(duration, wait_future).await {
                Ok(r) => Some(r),
                Err(_) => {
                    let _ = child.kill().await;
                    None
                }
            }
        } else {
            Some(wait_future.await)
        };

        let mut tasks_guard = tasks.write().await;
        if let Some(task) = tasks_guard.get_mut(&task_id) {
            task.info.completed_at = Some(Instant::now());
            task.info.compute_duration();

            match result {
                Some((Ok(exit_status), stdout, stderr)) => {
                    task.info.stdout = stdout;
                    task.info.stderr = stderr;
                    task.info.exit_code = exit_status.code();
                    task.info.status = if exit_status.success() {
                        TaskStatus::Completed
                    } else {
                        TaskStatus::Failed
                    };
                }
                Some((Err(_), stdout, stderr)) => {
                    task.info.stdout = stdout;
                    task.info.stderr = stderr;
                    task.info.status = TaskStatus::Failed;
                }
                None => {
                    task.info.status = TaskStatus::Timeout;
                }
            }
        }
    }

    pub async fn get_status(&self, task_id: &str) -> Option<TaskStatus> {
        let tasks = self.tasks.read().await;
        tasks.get(task_id).map(|t| t.info.status)
    }

    pub async fn get_task(&self, task_id: &str) -> Option<TaskInfo> {
        let tasks = self.tasks.read().await;
        tasks.get(task_id).map(|t| t.info.clone())
    }

    pub async fn get_output(
        &self,
        task_id: &str,
        stream: OutputStream,
        offset: usize,
        limit: usize,
    ) -> Option<(Vec<u8>, bool)> {
        let tasks = self.tasks.read().await;
        let task = tasks.get(task_id)?;

        let data = match stream {
            OutputStream::Stdout => &task.info.stdout,
            OutputStream::Stderr => &task.info.stderr,
            OutputStream::Both => {
                let mut combined = task.info.stdout.clone();
                combined.extend(&task.info.stderr);
                return Some(crate::output::get_output_slice(&combined, offset, limit));
            }
        };

        Some(crate::output::get_output_slice(data, offset, limit))
    }

    pub async fn kill_task(&self, task_id: &str, force: bool) -> Result<bool> {
        let mut tasks = self.tasks.write().await;
        let task = tasks
            .get_mut(task_id)
            .ok_or_else(|| anyhow!("Task not found"))?;

        if task.info.status != TaskStatus::Running {
            return Ok(false);
        }

        if let Some(inner) = &mut task.inner {
            if let Some(child) = &mut inner.child {
                if force {
                    child.kill().await?;
                } else {
                    #[cfg(unix)]
                    {
                        if let Some(pid) = child.id() {
                            unsafe {
                                libc::kill(pid as i32, libc::SIGTERM);
                            }
                        }
                    }
                    #[cfg(not(unix))]
                    {
                        child.kill().await?;
                    }
                }
                task.info.status = TaskStatus::Killed;
                task.info.completed_at = Some(Instant::now());
                task.info.compute_duration();
                return Ok(true);
            }
        }

        Ok(false)
    }

    pub async fn cleanup_completed(&self, max_age_secs: u64) {
        let now = Instant::now();
        let mut tasks = self.tasks.write().await;

        tasks.retain(|_, task| {
            if task.info.status == TaskStatus::Running || task.info.status == TaskStatus::Pending {
                return true;
            }
            if let Some(completed) = task.info.completed_at {
                now.duration_since(completed).as_secs() < max_age_secs
            } else {
                true
            }
        });
    }

    pub async fn list_tasks(&self) -> Vec<TaskInfo> {
        let tasks = self.tasks.read().await;
        tasks.values().map(|t| t.info.clone()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shell::detect_default_shell;

    #[tokio::test]
    async fn test_spawn_and_complete_task() {
        let manager = TaskManager::new();
        let (shell, path) = detect_default_shell().expect("Need a shell");

        let args = shell.command_args("echo hello");
        let task_id = manager
            .spawn_task("echo hello".into(), path, args, Some(5000), None, None)
            .await
            .expect("Should spawn");

        tokio::time::sleep(Duration::from_millis(500)).await;

        let status = manager.get_status(&task_id).await;
        assert!(matches!(status, Some(TaskStatus::Completed)));

        let task = manager.get_task(&task_id).await.expect("Should have task");
        assert_eq!(task.exit_code, Some(0));
        assert!(String::from_utf8_lossy(&task.stdout).contains("hello"));
    }

    #[tokio::test]
    async fn test_task_timeout() {
        let manager = TaskManager::new();
        let (shell, path) = detect_default_shell().expect("Need a shell");

        let cmd = if cfg!(windows) {
            "ping -n 10 127.0.0.1"
        } else {
            "sleep 10"
        };
        let args = shell.command_args(cmd);

        let task_id = manager
            .spawn_task(cmd.into(), path, args, Some(100), None, None)
            .await
            .expect("Should spawn");

        tokio::time::sleep(Duration::from_millis(500)).await;

        let status = manager.get_status(&task_id).await;
        assert!(matches!(status, Some(TaskStatus::Timeout)));
    }

    #[tokio::test]
    async fn test_list_tasks() {
        let manager = TaskManager::new();
        let (shell, path) = detect_default_shell().expect("Need a shell");

        let args = shell.command_args("echo test");
        let _ = manager
            .spawn_task("echo test".into(), path, args, Some(5000), None, None)
            .await;

        tokio::time::sleep(Duration::from_millis(100)).await;

        let tasks = manager.list_tasks().await;
        assert!(!tasks.is_empty());
    }
}
