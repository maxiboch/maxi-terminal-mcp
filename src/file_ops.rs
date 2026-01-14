//! File operation interception - uses efficient output format to save tokens.

use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// Result of attempting to intercept a command
pub enum InterceptResult {
    /// Command was intercepted and executed
    Handled(Value),
    /// Command should be executed normally by the shell
    PassThrough,
}

/// Try to intercept a file operation command
pub async fn try_intercept(command: &str, cwd: Option<&Path>) -> InterceptResult {
    let parts: Vec<&str> = command.split_whitespace().collect();
    if parts.is_empty() {
        return InterceptResult::PassThrough;
    }

    match parts[0] {
        "cat" => handle_cat(&parts[1..], cwd).await,
        "head" => handle_head(&parts[1..], cwd).await,
        "tail" => handle_tail(&parts[1..], cwd).await,
        _ => InterceptResult::PassThrough,
    }
}

async fn handle_cat(args: &[&str], cwd: Option<&Path>) -> InterceptResult {
    let files: Vec<&str> = args.iter().filter(|a| !a.starts_with('-')).copied().collect();
    if files.is_empty() {
        return InterceptResult::PassThrough;
    }

    let mut results: Vec<Value> = Vec::new();
    for file in files {
        let path = resolve_path(file, cwd);
        match tokio::fs::read_to_string(&path).await {
            Ok(content) => results.push(json!({"f": path.display().to_string(), "c": content})),
            Err(e) => results.push(json!({"f": path.display().to_string(), "e": e.to_string()})),
        }
    }

    InterceptResult::Handled(json!({"via": "direct", "r": results}))
}

async fn handle_head(args: &[&str], cwd: Option<&Path>) -> InterceptResult {
    let (lines, files) = parse_line_args(args, 10);
    if files.is_empty() {
        return InterceptResult::PassThrough;
    }

    let mut results: Vec<Value> = Vec::new();
    for file in files {
        let path = resolve_path(file, cwd);
        match tokio::fs::read_to_string(&path).await {
            Ok(content) => {
                let head: String = content.lines().take(lines).collect::<Vec<_>>().join("\n");
                results.push(json!({"f": path.display().to_string(), "c": head, "n": lines}));
            }
            Err(e) => results.push(json!({"f": path.display().to_string(), "e": e.to_string()})),
        }
    }

    InterceptResult::Handled(json!({"via": "direct", "r": results}))
}

async fn handle_tail(args: &[&str], cwd: Option<&Path>) -> InterceptResult {
    let (lines, files) = parse_line_args(args, 10);
    if files.is_empty() {
        return InterceptResult::PassThrough;
    }

    let mut results: Vec<Value> = Vec::new();
    for file in files {
        let path = resolve_path(file, cwd);
        match tokio::fs::read_to_string(&path).await {
            Ok(content) => {
                let all: Vec<&str> = content.lines().collect();
                let start = all.len().saturating_sub(lines);
                let tail: String = all[start..].join("\n");
                results.push(json!({"f": path.display().to_string(), "c": tail, "n": lines}));
            }
            Err(e) => results.push(json!({"f": path.display().to_string(), "e": e.to_string()})),
        }
    }

    InterceptResult::Handled(json!({"via": "direct", "r": results}))
}

fn parse_line_args<'a>(args: &[&'a str], default: usize) -> (usize, Vec<&'a str>) {
    let mut lines = default;
    let mut files = Vec::new();
    let mut i = 0;
    while i < args.len() {
        if args[i] == "-n" && i + 1 < args.len() {
            lines = args[i + 1].parse().unwrap_or(default);
            i += 2;
        } else if args[i].starts_with("-n") {
            lines = args[i][2..].parse().unwrap_or(default);
            i += 1;
        } else if !args[i].starts_with('-') {
            files.push(args[i]);
            i += 1;
        } else {
            i += 1;
        }
    }
    (lines, files)
}

fn resolve_path(path: &str, cwd: Option<&Path>) -> PathBuf {
    let p = Path::new(path);
    if p.is_absolute() {
        p.to_path_buf()
    } else if let Some(base) = cwd {
        base.join(p)
    } else {
        std::env::current_dir().unwrap_or_default().join(p)
    }
}
