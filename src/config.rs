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
        }
    }
}
