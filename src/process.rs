use anyhow::Result;
use std::collections::HashSet;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};
use tokio::sync::{broadcast, RwLock};

static PROCESS_REGISTRY: std::sync::OnceLock<Arc<RwLock<HashSet<u32>>>> =
    std::sync::OnceLock::new();

fn get_registry() -> &'static Arc<RwLock<HashSet<u32>>> {
    PROCESS_REGISTRY.get_or_init(|| Arc::new(RwLock::new(HashSet::new())))
}

pub async fn register_pid(pid: u32) {
    let registry = get_registry();
    let mut pids = registry.write().await;
    pids.insert(pid);
}

pub async fn unregister_pid(pid: u32) {
    let registry = get_registry();
    let mut pids = registry.write().await;
    pids.remove(&pid);
}

pub async fn get_all_pids() -> Vec<u32> {
    let registry = get_registry();
    let pids = registry.read().await;
    pids.iter().copied().collect()
}

#[derive(Debug, Clone)]
pub struct SpawnConfig {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub env: Option<std::collections::HashMap<String, String>>,
    pub new_process_group: bool,
}

pub fn spawn_isolated(config: &SpawnConfig) -> Result<Child> {
    let mut cmd = Command::new(&config.program);
    cmd.args(&config.args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    if let Some(dir) = &config.cwd {
        cmd.current_dir(dir);
    }

    if let Some(env_vars) = &config.env {
        cmd.envs(env_vars);
    }

    #[cfg(unix)]
    if config.new_process_group {
        #[allow(unused_imports)]
        use std::os::unix::process::CommandExt;
        unsafe {
            cmd.pre_exec(|| {
                // setsid() creates new session + process group for isolation
                libc::setsid();
                Ok(())
            });
        }
    }

    #[cfg(windows)]
    if config.new_process_group {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x00000200;
        cmd.creation_flags(CREATE_NEW_PROCESS_GROUP);
    }

    let child = cmd.spawn()?;
    Ok(child)
}

pub async fn kill_process_tree(pid: u32, force: bool) -> Result<()> {
    #[cfg(unix)]
    {
        let signal = if force { libc::SIGKILL } else { libc::SIGTERM };
        unsafe {
            // Kill process group (negative PID) to get all children
            let pgid = libc::getpgid(pid as i32);
            if pgid > 0 {
                libc::kill(-pgid, signal);
            }
            libc::kill(pid as i32, signal);
        }
    }

    #[cfg(windows)]
    {
        use std::process::Command as StdCommand;
        let _ = StdCommand::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .output();
    }

    unregister_pid(pid).await;
    Ok(())
}

pub async fn graceful_kill(pid: u32, timeout_ms: u64) -> Result<()> {
    kill_process_tree(pid, false).await?;

    let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_millis(timeout_ms);

    while tokio::time::Instant::now() < deadline {
        if !is_process_running(pid) {
            return Ok(());
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
    }

    kill_process_tree(pid, true).await
}

pub fn is_process_running(pid: u32) -> bool {
    #[cfg(unix)]
    {
        unsafe { libc::kill(pid as i32, 0) == 0 }
    }

    #[cfg(windows)]
    {
        use std::process::Command as StdCommand;
        StdCommand::new("tasklist")
            .args(["/FI", &format!("PID eq {}", pid), "/NH"])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).contains(&pid.to_string()))
            .unwrap_or(false)
    }
}

pub async fn cleanup_all_processes() {
    let pids = get_all_pids().await;
    for pid in pids {
        let _ = graceful_kill(pid, 1000).await;
    }
}

#[derive(Debug)]
pub struct StreamingOutput {
    buffer: Arc<RwLock<Vec<u8>>>,
    complete: Arc<RwLock<bool>>,
    notify: broadcast::Sender<()>,
}

impl StreamingOutput {
    pub fn new() -> (Self, broadcast::Receiver<()>) {
        let (tx, rx) = broadcast::channel(16);
        let output = Self {
            buffer: Arc::new(RwLock::new(Vec::new())),
            complete: Arc::new(RwLock::new(false)),
            notify: tx,
        };
        (output, rx)
    }

    pub fn stream_from<R: AsyncReadExt + Unpin + Send + 'static>(
        &self,
        mut reader: R,
    ) -> tokio::task::JoinHandle<()> {
        let buffer = Arc::clone(&self.buffer);
        let complete = Arc::clone(&self.complete);
        let notify = self.notify.clone();

        tokio::spawn(async move {
            let mut chunk = [0u8; 8192];
            loop {
                match reader.read(&mut chunk).await {
                    Ok(0) => break,
                    Ok(n) => {
                        let mut buf = buffer.write().await;
                        buf.extend_from_slice(&chunk[..n]);
                        drop(buf);
                        let _ = notify.send(());
                    }
                    Err(_) => break,
                }
            }
            *complete.write().await = true;
            let _ = notify.send(());
        })
    }

    pub async fn get_buffer(&self) -> Vec<u8> {
        self.buffer.read().await.clone()
    }

    pub async fn get_slice(&self, offset: usize, limit: usize) -> (Vec<u8>, bool) {
        let buf = self.buffer.read().await;
        let end = (offset + limit).min(buf.len());
        let data = buf.get(offset..end).unwrap_or(&[]).to_vec();
        let has_more = end < buf.len();
        (data, has_more)
    }

    pub async fn is_complete(&self) -> bool {
        *self.complete.read().await
    }

    pub async fn total_bytes(&self) -> usize {
        self.buffer.read().await.len()
    }

    pub async fn wait_for_completion(&self, timeout_ms: Option<u64>) -> bool {
        let wait_fut = async {
            let mut rx = self.notify.subscribe();
            while !*self.complete.read().await {
                let _ = rx.recv().await;
            }
        };

        if let Some(ms) = timeout_ms {
            tokio::time::timeout(tokio::time::Duration::from_millis(ms), wait_fut)
                .await
                .is_ok()
        } else {
            wait_fut.await;
            true
        }
    }
}

impl Default for StreamingOutput {
    fn default() -> Self {
        Self::new().0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_spawn_isolated() {
        let config = SpawnConfig {
            program: PathBuf::from(if cfg!(windows) { "cmd" } else { "sh" }),
            args: if cfg!(windows) {
                vec!["/C".into(), "echo hello".into()]
            } else {
                vec!["-c".into(), "echo hello".into()]
            },
            cwd: None,
            env: None,
            new_process_group: true,
        };

        let child = spawn_isolated(&config).expect("Should spawn");
        let output = child.wait_with_output().await.expect("Should complete");
        assert!(output.status.success());
        assert!(String::from_utf8_lossy(&output.stdout).contains("hello"));
    }

    #[tokio::test]
    async fn test_streaming_output() {
        let (output, _rx) = StreamingOutput::new();

        {
            let mut buf = output.buffer.write().await;
            buf.extend_from_slice(b"hello world");
        }

        let (slice, _) = output.get_slice(0, 5).await;
        assert_eq!(&slice, b"hello");

        let (slice, _) = output.get_slice(6, 5).await;
        assert_eq!(&slice, b"world");
    }

    #[tokio::test]
    async fn test_pid_registry() {
        register_pid(12345).await;
        let pids = get_all_pids().await;
        assert!(pids.contains(&12345));

        unregister_pid(12345).await;
        let pids = get_all_pids().await;
        assert!(!pids.contains(&12345));
    }
}
