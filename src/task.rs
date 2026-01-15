use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Instant;
use tokio::sync::RwLock;
use tokio::time::{timeout, Duration};

use crate::process::{
    graceful_kill, kill_process_tree, register_pid, spawn_isolated, unregister_pid, SpawnConfig,
    StreamingOutput,
};

// 90s EXTREME candy + fruit task names
static TASK_COUNTER: AtomicU32 = AtomicU32::new(0);

const EXTREME: &[&str] = &[
    "atomic", "mega", "ultra", "turbo", "hyper", "super", "extreme", "radical",
    "sonic", "cosmic", "nuclear", "nitro", "power", "blast", "surge", "fury",
];

const CANDY: &[&str] = &[
    "sour", "fizz", "pop", "bang", "zap", "burst", "shock", "punch",
    "rush", "boom", "zing", "warp", "blitz", "crush", "slam", "rip",
];

const FRUITS: &[&str] = &[
    "mango", "peach", "berry", "melon", "grape", "lemon", "apple", "cherry",
    "lime", "kiwi", "guava", "papaya", "orange", "punch", "blast", "razz",
];

fn next_task_id() -> String {
    let n = TASK_COUNTER.fetch_add(1, Ordering::Relaxed);
    let extreme = EXTREME[n as usize % EXTREME.len()];
    let candy = CANDY[(n as usize / EXTREME.len()) % CANDY.len()];
    let fruit = FRUITS[(n as usize / (EXTREME.len() * CANDY.len())) % FRUITS.len()];
    format!("{}_{}_{}", extreme, candy, fruit)
}

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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskInfo {
    pub id: String,
    pub command: String,
    pub status: TaskStatus,
    #[serde(rename = "exit")]
    pub exit_code: Option<i32>,
    pub pid: Option<u32>,
    #[serde(skip)]
    pub started_at: Option<Instant>,
    #[serde(skip)]
    pub completed_at: Option<Instant>,
    #[serde(rename = "dur")]
    pub duration_ms: Option<u64>,
}

impl TaskInfo {
    fn new(id: String, command: String) -> Self {
        Self {
            id,
            command,
            status: TaskStatus::Pending,
            exit_code: None,
            pid: None,
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
    stdout: Arc<StreamingOutput>,
    stderr: Arc<StreamingOutput>,
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
        let task_id = next_task_id();
        let mut info = TaskInfo::new(task_id.clone(), command.clone());

        let config = SpawnConfig {
            program: shell_path,
            args: shell_args,
            cwd,
            env,
            new_process_group: true,
        };

        let mut child = spawn_isolated(&config)?;
        let pid = child.id();

        if let Some(p) = pid {
            register_pid(p).await;
            info.pid = Some(p);
        }

        info.status = TaskStatus::Running;
        info.started_at = Some(Instant::now());

        let (stdout_stream, _) = StreamingOutput::new();
        let (stderr_stream, _) = StreamingOutput::new();
        let stdout_stream = Arc::new(stdout_stream);
        let stderr_stream = Arc::new(stderr_stream);

        if let Some(stdout) = child.stdout.take() {
            stdout_stream.stream_from(stdout);
        }
        if let Some(stderr) = child.stderr.take() {
            stderr_stream.stream_from(stderr);
        }

        let managed = ManagedTask {
            info,
            stdout: Arc::clone(&stdout_stream),
            stderr: Arc::clone(&stderr_stream),
        };

        {
            let mut tasks = self.tasks.write().await;
            tasks.insert(task_id.clone(), managed);
        }

        let tasks = Arc::clone(&self.tasks);
        let id = task_id.clone();
        let timeout_duration = timeout_ms.map(Duration::from_millis);
        let stdout_for_wait = Arc::clone(&stdout_stream);
        let stderr_for_wait = Arc::clone(&stderr_stream);

        tokio::spawn(async move {
            Self::run_task(
                tasks,
                id,
                child,
                pid,
                timeout_duration,
                stdout_for_wait,
                stderr_for_wait,
            )
            .await;
        });

        Ok(task_id)
    }

    async fn run_task(
        tasks: Arc<RwLock<HashMap<String, ManagedTask>>>,
        task_id: String,
        mut child: tokio::process::Child,
        pid: Option<u32>,
        timeout_duration: Option<Duration>,
        stdout_stream: Arc<StreamingOutput>,
        stderr_stream: Arc<StreamingOutput>,
    ) {
        let wait_future = async {
            let status = child.wait().await;
            stdout_stream.wait_for_completion(Some(5000)).await;
            stderr_stream.wait_for_completion(Some(5000)).await;
            status
        };

        let result = if let Some(duration) = timeout_duration {
            match timeout(duration, wait_future).await {
                Ok(r) => Some(r),
                Err(_) => {
                    if let Some(p) = pid {
                        let _ = kill_process_tree(p, true).await;
                    }
                    None
                }
            }
        } else {
            Some(wait_future.await)
        };

        if let Some(p) = pid {
            unregister_pid(p).await;
        }

        let mut tasks_guard = tasks.write().await;
        if let Some(task) = tasks_guard.get_mut(&task_id) {
            task.info.completed_at = Some(Instant::now());
            task.info.compute_duration();

            match result {
                Some(Ok(exit_status)) => {
                    task.info.exit_code = exit_status.code();
                    task.info.status = if exit_status.success() {
                        TaskStatus::Completed
                    } else {
                        TaskStatus::Failed
                    };
                }
                Some(Err(_)) => {
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
        // Clone Arc refs before releasing lock to avoid deadlock
        let (stdout_stream, stderr_stream) = {
            let tasks = self.tasks.read().await;
            let task = tasks.get(task_id)?;
            (Arc::clone(&task.stdout), Arc::clone(&task.stderr))
        };

        match stream {
            OutputStream::Stdout => Some(stdout_stream.get_slice(offset, limit).await),
            OutputStream::Stderr => Some(stderr_stream.get_slice(offset, limit).await),
            OutputStream::Both => {
                let stdout = stdout_stream.get_buffer().await;
                let stderr = stderr_stream.get_buffer().await;
                let mut combined = stdout;
                combined.extend(stderr);
                Some(crate::output::get_output_slice(&combined, offset, limit))
            }
        }
    }

    pub async fn is_output_complete(&self, task_id: &str) -> Option<bool> {
        // Clone Arc refs before releasing lock to avoid deadlock
        let (stdout_stream, stderr_stream) = {
            let tasks = self.tasks.read().await;
            let task = tasks.get(task_id)?;
            (Arc::clone(&task.stdout), Arc::clone(&task.stderr))
        };
        let stdout_done = stdout_stream.is_complete().await;
        let stderr_done = stderr_stream.is_complete().await;
        Some(stdout_done && stderr_done)
    }

    pub async fn kill_task(&self, task_id: &str, force: bool) -> Result<bool> {
        let tasks = self.tasks.read().await;
        let task = tasks
            .get(task_id)
            .ok_or_else(|| anyhow!("Task not found"))?;

        if task.info.status != TaskStatus::Running {
            return Ok(false);
        }

        let pid = task.info.pid.ok_or_else(|| anyhow!("No PID for task"))?;
        drop(tasks);

        if force {
            kill_process_tree(pid, true).await?;
        } else {
            graceful_kill(pid, 3000).await?;
        }

        let mut tasks = self.tasks.write().await;
        if let Some(task) = tasks.get_mut(task_id) {
            task.info.status = TaskStatus::Killed;
            task.info.completed_at = Some(Instant::now());
            task.info.compute_duration();
        }

        Ok(true)
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

    pub async fn kill_all_running(&self) {
        let tasks = self.tasks.read().await;
        let running_pids: Vec<u32> = tasks
            .values()
            .filter(|t| t.info.status == TaskStatus::Running)
            .filter_map(|t| t.info.pid)
            .collect();
        drop(tasks);

        for pid in running_pids {
            let _ = graceful_kill(pid, 2000).await;
        }
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
        assert!(task.pid.is_some());

        let (output, _) = manager
            .get_output(&task_id, OutputStream::Stdout, 0, 1000)
            .await
            .expect("Should have output");
        assert!(String::from_utf8_lossy(&output).contains("hello"));
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

