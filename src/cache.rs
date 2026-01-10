use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::RwLock;

/// Cached output from a foreground command
#[derive(Debug, Clone)]
pub struct CachedOutput {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub exit_code: Option<i32>,
    pub created_at: Instant,
}

impl CachedOutput {
    pub fn new(stdout: Vec<u8>, stderr: Vec<u8>, exit_code: Option<i32>) -> Self {
        Self {
            stdout,
            stderr,
            exit_code,
            created_at: Instant::now(),
        }
    }

    pub fn get_output(&self, stream: OutputStream) -> &[u8] {
        match stream {
            OutputStream::Stdout => &self.stdout,
            OutputStream::Stderr => &self.stderr,
        }
    }

    pub fn get_combined(&self) -> Vec<u8> {
        let mut combined = self.stdout.clone();
        combined.extend(&self.stderr);
        combined
    }
}

#[derive(Debug, Clone, Copy)]
pub enum OutputStream {
    Stdout,
    Stderr,
}

/// Cache for recent foreground command outputs
/// Allows agents to retrieve raw output after receiving structured results
pub struct OutputCache {
    entries: Arc<RwLock<HashMap<String, CachedOutput>>>,
    max_entries: usize,
    max_age_secs: u64,
}

impl Default for OutputCache {
    fn default() -> Self {
        Self::new(20, 300) // 20 entries, 5 minutes max age
    }
}

impl OutputCache {
    pub fn new(max_entries: usize, max_age_secs: u64) -> Self {
        Self {
            entries: Arc::new(RwLock::new(HashMap::new())),
            max_entries,
            max_age_secs,
        }
    }

    /// Store output and return the output_id
    pub async fn store(&self, stdout: Vec<u8>, stderr: Vec<u8>, exit_code: Option<i32>) -> String {
        let output_id = format!("out_{}", uuid::Uuid::new_v4().to_string().split('-').next().unwrap_or("0"));
        let cached = CachedOutput::new(stdout, stderr, exit_code);

        let mut entries = self.entries.write().await;

        // Cleanup old entries if at capacity
        if entries.len() >= self.max_entries {
            self.cleanup_oldest(&mut entries);
        }

        entries.insert(output_id.clone(), cached);
        output_id
    }

    /// Get cached output by ID
    pub async fn get(&self, output_id: &str) -> Option<CachedOutput> {
        let entries = self.entries.read().await;
        entries.get(output_id).cloned()
    }

    /// Get a slice of the cached output (for pagination)
    pub async fn get_slice(
        &self,
        output_id: &str,
        stream: Option<OutputStream>,
        offset: usize,
        limit: usize,
    ) -> Option<(Vec<u8>, bool)> {
        let entries = self.entries.read().await;
        let cached = entries.get(output_id)?;

        let data = match stream {
            Some(s) => cached.get_output(s).to_vec(),
            None => cached.get_combined(),
        };

        if offset >= data.len() {
            return Some((Vec::new(), false));
        }

        let end = (offset + limit).min(data.len());
        let slice = data[offset..end].to_vec();
        let has_more = end < data.len();

        Some((slice, has_more))
    }

    /// Check if an output_id exists
    pub async fn exists(&self, output_id: &str) -> bool {
        let entries = self.entries.read().await;
        entries.contains_key(output_id)
    }

    /// Cleanup expired entries
    pub async fn cleanup_expired(&self) {
        let now = Instant::now();
        let mut entries = self.entries.write().await;
        entries.retain(|_, cached| {
            now.duration_since(cached.created_at).as_secs() < self.max_age_secs
        });
    }

    fn cleanup_oldest(&self, entries: &mut HashMap<String, CachedOutput>) {
        // Find and remove the oldest entry
        if let Some(oldest_key) = entries
            .iter()
            .min_by_key(|(_, v)| v.created_at)
            .map(|(k, _)| k.clone())
        {
            entries.remove(&oldest_key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_store_and_retrieve() {
        let cache = OutputCache::new(10, 60);
        let output_id = cache.store(b"hello".to_vec(), b"error".to_vec(), Some(0)).await;

        assert!(output_id.starts_with("out_"));

        let cached = cache.get(&output_id).await.expect("should exist");
        assert_eq!(cached.stdout, b"hello");
        assert_eq!(cached.stderr, b"error");
        assert_eq!(cached.exit_code, Some(0));
    }

    #[tokio::test]
    async fn test_get_slice() {
        let cache = OutputCache::new(10, 60);
        let output_id = cache.store(b"hello world".to_vec(), Vec::new(), Some(0)).await;

        let (slice, has_more) = cache.get_slice(&output_id, Some(OutputStream::Stdout), 0, 5).await.unwrap();
        assert_eq!(slice, b"hello");
        assert!(has_more);

        let (slice, has_more) = cache.get_slice(&output_id, Some(OutputStream::Stdout), 6, 100).await.unwrap();
        assert_eq!(slice, b"world");
        assert!(!has_more);
    }

    #[tokio::test]
    async fn test_max_entries() {
        let cache = OutputCache::new(2, 60);

        let _id1 = cache.store(b"one".to_vec(), Vec::new(), Some(0)).await;
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        let id2 = cache.store(b"two".to_vec(), Vec::new(), Some(0)).await;
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        let id3 = cache.store(b"three".to_vec(), Vec::new(), Some(0)).await;

        // First entry should be evicted
        let entries = cache.entries.read().await;
        assert_eq!(entries.len(), 2);
        assert!(entries.contains_key(&id2));
        assert!(entries.contains_key(&id3));
    }
}
