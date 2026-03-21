use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

/// File allocation strategy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileAllocation {
    /// No pre-allocation; file grows as data is written.
    None,
    /// Pre-allocate the full file size by writing zeros before downloading.
    Prealloc,
}

impl Default for FileAllocation {
    fn default() -> Self {
        FileAllocation::Prealloc
    }
}

/// Checksum algorithm for post-download verification.
#[derive(Debug, Clone)]
pub enum Checksum {
    /// SHA-256 digest (hex-encoded, lowercase).
    Sha256(String),
}

/// Specification for a download task.
#[derive(Debug, Clone)]
pub struct DownloadSpec {
    pub url: String,
    pub output_path: PathBuf,
    pub headers: HashMap<String, String>,
    pub max_connections: u32,
    pub connect_timeout: Duration,
    pub read_timeout: Duration,
    pub memory_budget: usize,
    pub file_allocation: FileAllocation,
    pub channel_buffer: usize,
    pub resume: bool,
    pub piece_size: u64,
    pub min_split_size: u64,
    /// Maximum retry attempts per segment (0 = no retries).
    pub max_retries: u32,
    /// Base delay for exponential backoff between retries.
    pub retry_base_delay: Duration,
    /// Maximum delay cap for exponential backoff.
    pub retry_max_delay: Duration,
    /// Maximum download speed in bytes/sec. 0 = unlimited.
    pub max_download_speed: u64,
    /// Optional checksum for post-download verification.
    pub checksum: Option<Checksum>,
}

impl DownloadSpec {
    pub fn new(url: impl Into<String>, output_path: impl Into<PathBuf>) -> Self {
        Self {
            url: url.into(),
            output_path: output_path.into(),
            headers: HashMap::new(),
            max_connections: 4,
            connect_timeout: Duration::from_secs(30),
            read_timeout: Duration::from_secs(60),
            memory_budget: 64 * 1024 * 1024, // 64 MiB
            file_allocation: FileAllocation::default(),
            channel_buffer: 64,
            resume: true,
            piece_size: 1024 * 1024,           // 1 MiB
            min_split_size: 10 * 1024 * 1024,  // 10 MiB
            max_retries: 5,
            retry_base_delay: Duration::from_secs(1),
            retry_max_delay: Duration::from_secs(30),
            max_download_speed: 0,
            checksum: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_download_spec_defaults() {
        let spec = DownloadSpec::new("https://example.com/file", "/tmp/file");
        assert_eq!(spec.url, "https://example.com/file");
        assert_eq!(spec.output_path, PathBuf::from("/tmp/file"));
        assert_eq!(spec.max_connections, 4);
        assert_eq!(spec.connect_timeout, Duration::from_secs(30));
        assert_eq!(spec.read_timeout, Duration::from_secs(60));
        assert_eq!(spec.memory_budget, 64 * 1024 * 1024);
        assert_eq!(spec.file_allocation, FileAllocation::Prealloc);
        assert_eq!(spec.channel_buffer, 64);
        assert!(spec.resume);
        assert_eq!(spec.piece_size, 1024 * 1024);
        assert_eq!(spec.min_split_size, 10 * 1024 * 1024);
        assert_eq!(spec.max_retries, 5);
        assert_eq!(spec.max_download_speed, 0);
        assert!(spec.checksum.is_none());
        assert!(spec.headers.is_empty());
    }

    #[test]
    fn test_file_allocation_default() {
        assert_eq!(FileAllocation::default(), FileAllocation::Prealloc);
    }

    #[test]
    fn test_checksum_debug() {
        let c = Checksum::Sha256("abc123".into());
        let debug = format!("{c:?}");
        assert!(debug.contains("abc123"));
    }
}
