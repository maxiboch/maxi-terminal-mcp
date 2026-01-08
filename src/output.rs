use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputFormat {
    Raw,
    #[default]
    Compact,
    Summary,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputBuffer {
    pub data: Vec<u8>,
    pub total_bytes: usize,
    pub truncated: bool,
}

impl OutputBuffer {
    pub fn new(data: Vec<u8>, total_bytes: usize, truncated: bool) -> Self {
        Self {
            data,
            total_bytes,
            truncated,
        }
    }

    pub fn as_string_lossy(&self) -> String {
        String::from_utf8_lossy(&self.data).to_string()
    }
}

pub fn strip_ansi(input: &[u8]) -> Vec<u8> {
    strip_ansi_escapes::strip(input)
}

pub fn compress_whitespace(input: &str) -> String {
    let mut result = String::with_capacity(input.len());
    let mut blank_count = 0;

    for line in input.lines() {
        if line.trim().is_empty() {
            blank_count += 1;
            if blank_count <= 1 {
                result.push('\n');
            }
        } else {
            blank_count = 0;
            result.push_str(line);
            result.push('\n');
        }
    }

    if result.ends_with('\n') && !input.ends_with('\n') {
        result.pop();
    }

    result
}

pub fn dedup_repeated_lines(input: &str, threshold: usize) -> String {
    if threshold < 2 {
        return input.to_string();
    }

    let lines: Vec<&str> = input.lines().collect();
    if lines.is_empty() {
        return input.to_string();
    }

    let mut result = String::with_capacity(input.len());
    let mut i = 0;

    while i < lines.len() {
        let current = lines[i];
        let mut count = 1;

        while i + count < lines.len() && lines[i + count] == current {
            count += 1;
        }

        if count >= threshold {
            result.push_str(current);
            result.push('\n');
            result.push_str(&format!("[above line repeated {} times]\n", count));
            i += count;
        } else {
            for _ in 0..count {
                result.push_str(current);
                result.push('\n');
            }
            i += count;
        }
    }

    if result.ends_with('\n') && !input.ends_with('\n') {
        result.pop();
    }

    result
}

pub fn smart_truncate(
    input: &str,
    max_lines: usize,
    head_lines: usize,
    tail_lines: usize,
) -> (String, bool) {
    let lines: Vec<&str> = input.lines().collect();
    let total = lines.len();

    if total <= max_lines {
        return (input.to_string(), false);
    }

    let keep_head = head_lines.min(total);
    let keep_tail = tail_lines.min(total.saturating_sub(keep_head));
    let truncated_count = total.saturating_sub(keep_head + keep_tail);

    if truncated_count == 0 {
        return (input.to_string(), false);
    }

    let mut result = String::with_capacity(input.len() / 2);

    for line in lines.iter().take(keep_head) {
        result.push_str(line);
        result.push('\n');
    }

    result.push_str(&format!(
        "\n[... {} lines truncated ...]\n\n",
        truncated_count
    ));

    for line in lines.iter().skip(total - keep_tail) {
        result.push_str(line);
        result.push('\n');
    }

    if result.ends_with('\n') && !input.ends_with('\n') {
        result.pop();
    }

    (result, true)
}

pub const DEFAULT_MAX_BYTES: usize = 50_000;
pub const DEFAULT_HEAD_LINES: usize = 100;
pub const DEFAULT_TAIL_LINES: usize = 50;
pub const DEFAULT_MAX_LINES: usize = 200;
pub const DEFAULT_DEDUP_THRESHOLD: usize = 5;

pub fn process_output(raw: &[u8], format: OutputFormat, max_bytes: Option<usize>) -> OutputBuffer {
    let max = max_bytes.unwrap_or(DEFAULT_MAX_BYTES);
    let total_bytes = raw.len();

    match format {
        OutputFormat::Raw => {
            if raw.len() <= max {
                OutputBuffer::new(raw.to_vec(), total_bytes, false)
            } else {
                let text = String::from_utf8_lossy(raw);
                let (truncated_text, was_truncated) = smart_truncate(
                    &text,
                    DEFAULT_MAX_LINES * 2,
                    DEFAULT_HEAD_LINES,
                    DEFAULT_TAIL_LINES,
                );
                OutputBuffer::new(truncated_text.into_bytes(), total_bytes, was_truncated)
            }
        }
        OutputFormat::Compact => {
            let stripped = strip_ansi(raw);
            let text = String::from_utf8_lossy(&stripped);
            let compressed = compress_whitespace(&text);
            let deduped = dedup_repeated_lines(&compressed, DEFAULT_DEDUP_THRESHOLD);
            let (final_text, was_truncated) = smart_truncate(
                &deduped,
                DEFAULT_MAX_LINES,
                DEFAULT_HEAD_LINES,
                DEFAULT_TAIL_LINES,
            );

            let mut data = final_text.into_bytes();
            let truncated = was_truncated || data.len() > max;

            if data.len() > max {
                data.truncate(max);
            }

            OutputBuffer::new(data, total_bytes, truncated)
        }
        OutputFormat::Summary => {
            let stripped = strip_ansi(raw);
            let text = String::from_utf8_lossy(&stripped);
            let lines: Vec<&str> = text.lines().collect();
            let line_count = lines.len();

            let summary_head = 10;
            let summary_tail = 5;

            let mut summary = String::new();
            summary.push_str(&format!("[{} bytes, {} lines]\n", total_bytes, line_count));

            if line_count <= summary_head + summary_tail {
                for line in &lines {
                    summary.push_str(line);
                    summary.push('\n');
                }
            } else {
                summary.push_str("First lines:\n");
                for line in lines.iter().take(summary_head) {
                    summary.push_str(line);
                    summary.push('\n');
                }
                summary.push_str(&format!(
                    "\n[... {} lines omitted ...]\n\n",
                    line_count - summary_head - summary_tail
                ));
                summary.push_str("Last lines:\n");
                for line in lines.iter().skip(line_count - summary_tail) {
                    summary.push_str(line);
                    summary.push('\n');
                }
            }

            OutputBuffer::new(
                summary.into_bytes(),
                total_bytes,
                line_count > summary_head + summary_tail,
            )
        }
    }
}

pub fn get_output_slice(data: &[u8], offset: usize, limit: usize) -> (Vec<u8>, bool) {
    if offset >= data.len() {
        return (Vec::new(), false);
    }

    let end = (offset + limit).min(data.len());
    let slice = data[offset..end].to_vec();
    let has_more = end < data.len();

    (slice, has_more)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_strip_ansi() {
        let input = b"\x1b[31mred\x1b[0m normal";
        let output = strip_ansi(input);
        assert_eq!(String::from_utf8_lossy(&output), "red normal");
    }

    #[test]
    fn test_compress_whitespace() {
        let input = "line1\n\n\n\nline2\n\nline3";
        let output = compress_whitespace(input);
        assert_eq!(output, "line1\n\nline2\n\nline3");
    }

    #[test]
    fn test_dedup_repeated_lines() {
        let input = "a\nb\nb\nb\nb\nb\nc";
        let output = dedup_repeated_lines(input, 3);
        assert!(output.contains("[above line repeated 5 times]"));
        assert!(output.contains("a\n"));
        assert!(output.contains("c"));
    }

    #[test]
    fn test_smart_truncate() {
        let lines: Vec<String> = (0..100).map(|i| format!("line {}", i)).collect();
        let input = lines.join("\n");

        let (output, truncated) = smart_truncate(&input, 50, 20, 10);
        assert!(truncated);
        assert!(output.contains("line 0"));
        assert!(output.contains("line 19"));
        assert!(output.contains("truncated"));
        assert!(output.contains("line 99"));
    }

    #[test]
    fn test_process_output_compact() {
        let input = b"\x1b[32mtest\x1b[0m\n\n\n\nmore";
        let buffer = process_output(input, OutputFormat::Compact, None);
        let text = buffer.as_string_lossy();
        assert!(!text.contains("\x1b["));
        assert!(!text.contains("\n\n\n"));
    }

    #[test]
    fn test_get_output_slice() {
        let data = b"hello world";
        let (slice, has_more) = get_output_slice(data, 0, 5);
        assert_eq!(slice, b"hello");
        assert!(has_more);

        let (slice, has_more) = get_output_slice(data, 6, 100);
        assert_eq!(slice, b"world");
        assert!(!has_more);
    }
}
