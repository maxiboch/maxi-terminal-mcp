//! File operation interception - uses efficient output format to save tokens.
//!
//! Intercepts file operations including:
//! - Simple commands: `cat file`, `head -n 20 file`
//! - Piped commands: `cat file | grep pattern`, `head file | tail -5`
//! - Sed substitutions: `sed 's/old/new/g' file`

use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// Result of attempting to intercept a command
pub enum InterceptResult {
    /// Command was intercepted and executed
    Handled(Value),
    /// Command should be executed normally by the shell
    PassThrough,
}

/// Try to intercept a file operation command (including pipes)
pub async fn try_intercept(command: &str, cwd: Option<&Path>) -> InterceptResult {
    // Check for pipes - handle pipeline
    if command.contains('|') {
        return handle_pipeline(command, cwd).await;
    }
    
    // Check for subcommands - pass through for now (complex)
    if command.contains("$(") || command.contains('`') {
        return InterceptResult::PassThrough;
    }
    
    let parts: Vec<&str> = command.split_whitespace().collect();
    if parts.is_empty() {
        return InterceptResult::PassThrough;
    }

    match parts[0] {
        "cat" => handle_cat(&parts[1..], cwd).await,
        "head" => handle_head(&parts[1..], cwd).await,
        "tail" => handle_tail(&parts[1..], cwd).await,
        "sed" => handle_sed(&parts[1..], cwd).await,
        _ => InterceptResult::PassThrough,
    }
}

/// Handle piped commands like `cat file | grep pattern | head -5`
async fn handle_pipeline(command: &str, cwd: Option<&Path>) -> InterceptResult {
    let stages: Vec<&str> = command.split('|').map(|s| s.trim()).collect();
    if stages.is_empty() {
        return InterceptResult::PassThrough;
    }
    
    // First stage must produce content (cat, head, tail on a file)
    let first_parts: Vec<&str> = stages[0].split_whitespace().collect();
    if first_parts.is_empty() {
        return InterceptResult::PassThrough;
    }
    
    // Get initial content from first command
    let mut content = match first_parts[0] {
        "cat" | "head" | "tail" => {
            match get_file_content(first_parts[0], &first_parts[1..], cwd).await {
                Some(c) => c,
                None => return InterceptResult::PassThrough,
            }
        }
        _ => return InterceptResult::PassThrough,
    };
    
    // Apply each subsequent stage as a filter
    for stage in &stages[1..] {
        let parts: Vec<&str> = stage.split_whitespace().collect();
        if parts.is_empty() {
            continue;
        }
        
        content = match parts[0] {
            "grep" => apply_grep(&content, &parts[1..]),
            "head" => apply_head(&content, &parts[1..]),
            "tail" => apply_tail(&content, &parts[1..]),
            "sed" => apply_sed(&content, &parts[1..]),
            "wc" => apply_wc(&content, &parts[1..]),
            "sort" => apply_sort(&content, &parts[1..]),
            "uniq" => apply_uniq(&content, &parts[1..]),
            _ => return InterceptResult::PassThrough, // Unknown filter, pass through
        };
    }
    
        // Build suggestion for next time
    let suggestion = build_pipeline_suggestion(&stages);

    InterceptResult::Handled(json!({
        "exit": 0,
        "dur": 0,
        "oid": null,
        "out": content,
        "data": null,
        "note": suggestion
    }))
}

/// Get content from cat/head/tail command
async fn get_file_content(cmd: &str, args: &[&str], cwd: Option<&Path>) -> Option<String> {
    let (lines, files) = parse_line_args(args, if cmd == "head" || cmd == "tail" { 10 } else { usize::MAX });
    
    if files.is_empty() {
        return None;
    }
    
    let mut result = String::new();
    for file in files {
        let path = resolve_path(file, cwd);
        if let Ok(content) = tokio::fs::read_to_string(&path).await {
            let processed = match cmd {
                "head" => content.lines().take(lines).collect::<Vec<_>>().join("\n"),
                "tail" => {
                    let all: Vec<&str> = content.lines().collect();
                    let start = all.len().saturating_sub(lines);
                    all[start..].join("\n")
                }
                _ => content,
            };
            if !result.is_empty() {
                result.push('\n');
            }
            result.push_str(&processed);
        }
    }
    
    if result.is_empty() { None } else { Some(result) }
}

/// Apply grep filter to content
fn apply_grep(content: &str, args: &[&str]) -> String {
    let mut invert = false;
    let mut ignore_case = false;
    let mut pattern = None;
    
    for arg in args {
        if *arg == "-v" {
            invert = true;
        } else if *arg == "-i" {
            ignore_case = true;
        } else if !arg.starts_with('-') && pattern.is_none() {
            pattern = Some(*arg);
        }
    }
    
    let pattern = match pattern {
        Some(p) => p,
        None => return content.to_string(),
    };
    
    content.lines()
        .filter(|line| {
            let matches = if ignore_case {
                line.to_lowercase().contains(&pattern.to_lowercase())
            } else {
                line.contains(pattern)
            };
            if invert { !matches } else { matches }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Apply head filter to content
fn apply_head(content: &str, args: &[&str]) -> String {
    let (lines, _) = parse_line_args(args, 10);
    content.lines().take(lines).collect::<Vec<_>>().join("\n")
}

/// Apply tail filter to content
fn apply_tail(content: &str, args: &[&str]) -> String {
    let (lines, _) = parse_line_args(args, 10);
    let all: Vec<&str> = content.lines().collect();
    let start = all.len().saturating_sub(lines);
    all[start..].join("\n")
}

/// Apply sed substitution to content
fn apply_sed(content: &str, args: &[&str]) -> String {
    for arg in args {
        let arg = arg.trim_matches('\'').trim_matches('"');
        if arg.starts_with("s/") {
            if let Some((old, new, global)) = parse_sed_pattern(arg) {
                return if global {
                    content.replace(&old, &new)
                } else {
                    content.replacen(&old, &new, 1)
                };
            }
        }
    }
    content.to_string()
}

/// Apply wc (word count) to content
fn apply_wc(content: &str, args: &[&str]) -> String {
    let lines = content.lines().count();
    let words = content.split_whitespace().count();
    let chars = content.len();
    
    if args.contains(&"-l") {
        lines.to_string()
    } else if args.contains(&"-w") {
        words.to_string()
    } else if args.contains(&"-c") {
        chars.to_string()
    } else {
        format!("{} {} {}", lines, words, chars)
    }
}

/// Apply sort to content
fn apply_sort(content: &str, args: &[&str]) -> String {
    let mut lines: Vec<&str> = content.lines().collect();
    
    if args.contains(&"-r") {
        lines.sort();
        lines.reverse();
    } else if args.contains(&"-n") {
        lines.sort_by(|a, b| {
            let a_num: i64 = a.trim().parse().unwrap_or(0);
            let b_num: i64 = b.trim().parse().unwrap_or(0);
            a_num.cmp(&b_num)
        });
    } else {
        lines.sort();
    }
    
    lines.join("\n")
}

/// Apply uniq to content
fn apply_uniq(content: &str, _args: &[&str]) -> String {
    let mut result = Vec::new();
    let mut prev: Option<&str> = None;
    
    for line in content.lines() {
        if prev != Some(line) {
            result.push(line);
            prev = Some(line);
        }
    }
    
    result.join("\n")
}

/// Build a suggestion for what tool to use instead of the pipeline
fn build_pipeline_suggestion(stages: &[&str]) -> String {
    let first = stages[0].split_whitespace().next().unwrap_or("");
    let has_grep = stages.iter().any(|s| s.trim().starts_with("grep"));
    let has_sed = stages.iter().any(|s| s.trim().starts_with("sed"));
    let has_head = stages.iter().any(|s| s.trim().starts_with("head"));
    let has_tail = stages.iter().any(|s| s.trim().starts_with("tail"));
    
    if has_grep {
        return "NEXT TIME: Use mcp__maxi-ide__search instead of cat|grep. Example: search(query=\"pattern\", paths=[\"file.txt\"])".to_string();
    }
    if has_sed {
        return "NEXT TIME: Use mcp__maxi-ide__edit_file for substitutions. Example: edit_file(path, old_text=\"old\", new_text=\"new\")".to_string();
    }
    if has_head || has_tail {
        return format!("NEXT TIME: Use mcp__maxi-ide__read_file with start_line/end_line. Example: read_file(path, start_line=1, end_line=20)");
    }
    
    match first {
        "cat" => "NEXT TIME: Use mcp__maxi-ide__read_file instead of cat".to_string(),
        _ => "NEXT TIME: Use mcp__maxi-ide__* tools for file operations".to_string(),
    }
}

/// Parse sed s/old/new/flags pattern
fn parse_sed_pattern(pattern: &str) -> Option<(String, String, bool)> {
    if !pattern.starts_with("s/") {
        return None;
    }
    
    let rest = &pattern[2..];
    let parts: Vec<&str> = rest.splitn(3, '/').collect();
    
    if parts.len() >= 2 {
        let old = parts[0].to_string();
        let new = parts[1].to_string();
        let global = parts.get(2).map(|f| f.contains('g')).unwrap_or(false);
        Some((old, new, global))
    } else {
        None
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

    let out = serde_json::to_string(&results).unwrap_or_default();
    InterceptResult::Handled(json!({
        "exit": 0,
        "dur": 0,
        "oid": null,
        "out": out,
        "data": results,
        "note": "NEXT TIME: Use mcp__maxi-ide__read_file instead of cat"
    }))
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

    let out = serde_json::to_string(&results).unwrap_or_default();
    InterceptResult::Handled(json!({
        "exit": 0,
        "dur": 0,
        "oid": null,
        "out": out,
        "data": results,
        "note": "NEXT TIME: Use mcp__maxi-ide__read_file(path, end_line=N) instead of head"
    }))
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

    let out = serde_json::to_string(&results).unwrap_or_default();
    InterceptResult::Handled(json!({
        "exit": 0,
        "dur": 0,
        "oid": null,
        "out": out,
        "data": results,
        "note": "NEXT TIME: Use mcp__maxi-ide__read_file with start_line from end instead of tail"
    }))
}

async fn handle_sed(args: &[&str], cwd: Option<&Path>) -> InterceptResult {
    // Parse sed args: sed 's/old/new/g' file or sed -i 's/old/new/g' file
    let mut in_place = false;
    let mut pattern = None;
    let mut file = None;
    
    for arg in args {
        if *arg == "-i" {
            in_place = true;
        } else if arg.starts_with("s/") || arg.starts_with("'s/") || arg.starts_with("\"s/") {
            pattern = Some(arg.trim_matches('\'').trim_matches('"'));
        } else if !arg.starts_with('-') && pattern.is_some() && file.is_none() {
            file = Some(*arg);
        }
    }
    
    let (pattern, file) = match (pattern, file) {
        (Some(p), Some(f)) => (p, f),
        _ => return InterceptResult::PassThrough,
    };
    
    let (old, new, global) = match parse_sed_pattern(pattern) {
        Some(t) => t,
        None => return InterceptResult::PassThrough,
    };
    
    let path = resolve_path(file, cwd);
    
    match tokio::fs::read_to_string(&path).await {
        Ok(content) => {
            let modified = if global {
                content.replace(&old, &new)
            } else {
                content.replacen(&old, &new, 1)
            };
            
                        if in_place {
                if let Err(e) = tokio::fs::write(&path, &modified).await {
                    return InterceptResult::Handled(json!({
                        "exit": 1,
                        "dur": 0,
                        "oid": null,
                        "out": e.to_string(),
                        "data": null,
                        "note": "NEXT TIME: Use mcp__maxi-ide__edit_file(path, old_text, new_text) instead of sed -i"
                    }));
                }
            }

            let out = if in_place { "File modified".to_string() } else { modified };
            InterceptResult::Handled(json!({
                "exit": 0,
                "dur": 0,
                "oid": null,
                "out": out,
                "data": {"f": path.display().to_string()},
                "note": "NEXT TIME: Use mcp__maxi-ide__edit_file(path, old_text, new_text) instead of sed"
            }))
        }
        Err(e) => InterceptResult::Handled(json!({
            "exit": 1,
            "dur": 0,
            "oid": null,
            "out": e.to_string(),
            "data": null,
            "note": "NEXT TIME: Use mcp__maxi-ide__edit_file instead of sed"
        })),
    }
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
