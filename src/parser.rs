use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// Detected output type based on content patterns
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputType {
    /// Test runner output (jest, pytest, cargo test, etc.)
    TestResults,
    /// Build/compile output with errors/warnings
    BuildOutput,
    /// Git status output
    GitStatus,
    /// Generic error output
    Error,
    /// Unstructured/unknown output
    Plain,
}

/// Structured representation of parsed output
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParsedOutput {
    pub output_type: OutputType,
    pub summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl ParsedOutput {
    pub fn plain(text: &str) -> Self {
        let summary = if text.len() > 200 {
            format!("{}...", &text[..200])
        } else {
            text.to_string()
        };
        Self {
            output_type: OutputType::Plain,
            summary,
            data: None,
        }
    }
}

/// Parse raw output into structured form based on content patterns
pub fn parse_output(stdout: &str, stderr: &str, exit_code: Option<i32>) -> ParsedOutput {
    // Check for test results first (highest value)
    if let Some(parsed) = try_parse_test_results(stdout, stderr, exit_code) {
        return parsed;
    }

    // Check for build output
    if let Some(parsed) = try_parse_build_output(stdout, stderr, exit_code) {
        return parsed;
    }

    // Check for git status
    if let Some(parsed) = try_parse_git_status(stdout) {
        return parsed;
    }

    // Check for error patterns
    if let Some(parsed) = try_parse_error(stdout, stderr, exit_code) {
        return parsed;
    }

    // Fall back to plain
    let combined = if stderr.is_empty() {
        stdout.to_string()
    } else {
        format!("{}\n{}", stdout, stderr)
    };
    ParsedOutput::plain(&combined)
}

fn try_parse_test_results(stdout: &str, stderr: &str, exit_code: Option<i32>) -> Option<ParsedOutput> {
    let combined = format!("{}\n{}", stdout, stderr);
    let lower = combined.to_lowercase();

    // Detect test runner patterns
    let is_test_output = lower.contains("test result:")  // cargo test
        || lower.contains("tests passed")
        || lower.contains("tests failed")
        || lower.contains("passed, ") && lower.contains("failed")
        || (lower.contains("passing") && lower.contains("failing"))  // mocha/jest
        || lower.contains("pytest")
        || lower.contains(" passed") && lower.contains(" failed")
        || lower.contains("test suite")
        || lower.contains("tests:")  // jest summary
        || (lower.contains("ok") && lower.contains("passed") && lower.contains("filtered")); // cargo

    if !is_test_output {
        return None;
    }

    // Extract test counts
    let mut passed = 0u32;
    let mut failed = 0u32;
    let mut skipped = 0u32;

    // cargo test: "test result: ok. 5 passed; 0 failed; 0 ignored"
    if let Some(caps) = regex_lite::Regex::new(r"(\d+) passed").ok()?.captures(&lower) {
        passed = caps.get(1)?.as_str().parse().ok()?;
    }
    if let Some(caps) = regex_lite::Regex::new(r"(\d+) failed").ok()?.captures(&lower) {
        failed = caps.get(1)?.as_str().parse().ok()?;
    }
    if let Some(caps) = regex_lite::Regex::new(r"(\d+) (?:ignored|skipped|pending)").ok()?.captures(&lower) {
        skipped = caps.get(1)?.as_str().parse().ok()?;
    }

    let total = passed + failed + skipped;
    let success = exit_code == Some(0) && failed == 0;

    let summary = if total > 0 {
        format!(
            "{}: {} passed, {} failed{}",
            if success { "PASS" } else { "FAIL" },
            passed,
            failed,
            if skipped > 0 { format!(", {} skipped", skipped) } else { String::new() }
        )
    } else {
        format!("{}", if success { "Tests passed" } else { "Tests failed" })
    };

    Some(ParsedOutput {
        output_type: OutputType::TestResults,
        summary,
        data: Some(json!({
            "passed": passed,
            "failed": failed,
            "skipped": skipped,
            "total": total,
            "success": success
        })),
    })
}

fn try_parse_build_output(stdout: &str, stderr: &str, exit_code: Option<i32>) -> Option<ParsedOutput> {
    let combined = format!("{}\n{}", stdout, stderr);
    let lower = combined.to_lowercase();

    // Detect build patterns
    let is_build = lower.contains("compiling")
        || lower.contains("building")
        || lower.contains("compiled")
        || lower.contains("bundled")
        || lower.contains("webpack")
        || lower.contains("vite")
        || lower.contains("esbuild")
        || lower.contains("tsc")
        || (lower.contains("error") && lower.contains("warning"))
        || lower.contains("finished `");

    if !is_build {
        return None;
    }

    // Count errors and warnings
    let error_re = regex_lite::Regex::new(r"(?i)(\d+)\s*errors?").ok()?;
    let warning_re = regex_lite::Regex::new(r"(?i)(\d+)\s*warnings?").ok()?;

    let errors: u32 = error_re
        .captures(&combined)
        .and_then(|c| c.get(1)?.as_str().parse().ok())
        .unwrap_or(0);

    let warnings: u32 = warning_re
        .captures(&combined)
        .and_then(|c| c.get(1)?.as_str().parse().ok())
        .unwrap_or(0);

    // Also count inline error/warning markers
    let inline_errors = combined.matches(" error").count()
        + combined.matches(" error:").count()
        + combined.matches("ERROR").count();
    let inline_warnings = combined.matches(" warning").count()
        + combined.matches(" warning:").count()
        + combined.matches("WARN").count();

    let total_errors = errors.max(inline_errors as u32);
    let total_warnings = warnings.max(inline_warnings as u32);

    let success = exit_code == Some(0);
    let summary = format!(
        "{}: {} errors, {} warnings",
        if success { "BUILD OK" } else { "BUILD FAILED" },
        total_errors,
        total_warnings
    );

    Some(ParsedOutput {
        output_type: OutputType::BuildOutput,
        summary,
        data: Some(json!({
            "success": success,
            "errors": total_errors,
            "warnings": total_warnings
        })),
    })
}

fn try_parse_git_status(stdout: &str) -> Option<ParsedOutput> {
    // Git status patterns
    if !stdout.contains("On branch") && !stdout.contains("HEAD detached") {
        return None;
    }

    let lines: Vec<&str> = stdout.lines().collect();

    // Extract branch
    let branch = lines
        .iter()
        .find(|l| l.starts_with("On branch"))
        .map(|l| l.trim_start_matches("On branch ").trim())
        .unwrap_or("unknown");

    // Count changes
    let staged = lines.iter().filter(|l| l.starts_with("M  ") || l.starts_with("A  ") || l.starts_with("D  ")).count();
    let modified = lines.iter().filter(|l| l.starts_with(" M ") || l.contains("modified:")).count();
    let untracked = lines.iter().filter(|l| l.starts_with("??") || l.contains("Untracked files")).count();

    let clean = stdout.contains("nothing to commit") || stdout.contains("working tree clean");

    let summary = if clean {
        format!("branch: {}, clean", branch)
    } else {
        format!(
            "branch: {}, {} staged, {} modified, {} untracked",
            branch, staged, modified, untracked
        )
    };

    Some(ParsedOutput {
        output_type: OutputType::GitStatus,
        summary,
        data: Some(json!({
            "branch": branch,
            "clean": clean,
            "staged": staged,
            "modified": modified,
            "untracked": untracked
        })),
    })
}

fn try_parse_error(stdout: &str, stderr: &str, exit_code: Option<i32>) -> Option<ParsedOutput> {
    // Only treat as error if non-zero exit and has error-like content
    if exit_code == Some(0) {
        return None;
    }

    let text = if !stderr.is_empty() { stderr } else { stdout };
    let lower = text.to_lowercase();

    // Look for common error patterns
    let has_error = lower.contains("error:")
        || lower.contains("exception")
        || lower.contains("failed")
        || lower.contains("fatal:")
        || lower.contains("panic")
        || lower.contains("traceback");

    if !has_error {
        return None;
    }

    // Extract first error line
    let first_error = text
        .lines()
        .find(|l| {
            let ll = l.to_lowercase();
            ll.contains("error") || ll.contains("exception") || ll.contains("fatal")
        })
        .unwrap_or(text.lines().next().unwrap_or("Unknown error"));

    let summary = if first_error.len() > 100 {
        format!("{}...", &first_error[..100])
    } else {
        first_error.to_string()
    };

    Some(ParsedOutput {
        output_type: OutputType::Error,
        summary,
        data: Some(json!({
            "exit_code": exit_code
        })),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_cargo_test() {
        let stdout = r#"
   Compiling myproject v0.1.0
    Finished test [unoptimized + debuginfo] target(s) in 0.50s
     Running unittests src/lib.rs

running 5 tests
test tests::test_one ... ok
test tests::test_two ... ok
test tests::test_three ... FAILED
test tests::test_four ... ok
test tests::test_five ... ok

test result: FAILED. 4 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out
"#;
        let parsed = parse_output(stdout, "", Some(1));
        assert_eq!(parsed.output_type, OutputType::TestResults);
        assert!(parsed.summary.contains("FAIL"));
        assert!(parsed.summary.contains("4 passed"));
        assert!(parsed.summary.contains("1 failed"));
    }

    #[test]
    fn test_parse_build_success() {
        let stdout = r#"
   Compiling serde v1.0.0
   Compiling myproject v0.1.0
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 2.30s
"#;
        let parsed = parse_output(stdout, "warning: unused variable `x`\n  --> src/main.rs:10:5", Some(0));
        assert_eq!(parsed.output_type, OutputType::BuildOutput);
        assert!(parsed.summary.contains("BUILD OK"));
    }

    #[test]
    fn test_parse_git_status_clean() {
        let stdout = r#"On branch main
Your branch is up to date with 'origin/main'.

nothing to commit, working tree clean
"#;
        let parsed = parse_output(stdout, "", Some(0));
        assert_eq!(parsed.output_type, OutputType::GitStatus);
        assert!(parsed.summary.contains("main"));
        assert!(parsed.summary.contains("clean"));
    }

    #[test]
    fn test_parse_error() {
        let stderr = "error: could not find `main` function";
        let parsed = parse_output("", stderr, Some(1));
        assert_eq!(parsed.output_type, OutputType::Error);
        assert!(parsed.summary.contains("error"));
    }

    #[test]
    fn test_parse_plain() {
        let stdout = "Hello, world!";
        let parsed = parse_output(stdout, "", Some(0));
        assert_eq!(parsed.output_type, OutputType::Plain);
    }
}
