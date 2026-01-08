use std::path::PathBuf;

#[cfg(windows)]
pub fn normalize_path(path: &str) -> PathBuf {
    let expanded = expand_tilde(path);
    PathBuf::from(expanded.replace('/', "\\"))
}

#[cfg(not(windows))]
pub fn normalize_path(path: &str) -> PathBuf {
    let expanded = expand_tilde(path);
    PathBuf::from(expanded.replace('\\', "/"))
}

pub fn expand_tilde(path: &str) -> String {
    if path.starts_with("~/") || path == "~" {
        if let Some(home) = home_dir() {
            return format!("{}{}", home.display(), &path[1..]);
        }
    }
    path.to_string()
}

fn home_dir() -> Option<PathBuf> {
    #[cfg(unix)]
    {
        std::env::var("HOME").ok().map(PathBuf::from)
    }
    #[cfg(windows)]
    {
        std::env::var("USERPROFILE")
            .or_else(|_| std::env::var("HOME"))
            .ok()
            .map(PathBuf::from)
    }
}

pub fn ensure_absolute(path: &str, base: Option<&std::path::Path>) -> PathBuf {
    let normalized = normalize_path(path);
    if normalized.is_absolute() {
        normalized
    } else if let Some(b) = base {
        b.join(&normalized)
    } else {
        std::env::current_dir()
            .unwrap_or_default()
            .join(&normalized)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_expand_tilde() {
        let expanded = expand_tilde("~/test");
        assert!(!expanded.starts_with('~'));
    }

    #[test]
    fn test_normalize_separators() {
        let path = normalize_path("a/b\\c");
        let s = path.to_string_lossy();
        #[cfg(windows)]
        assert!(!s.contains('/'));
        #[cfg(not(windows))]
        assert!(!s.contains('\\'));
    }
}
