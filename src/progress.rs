use std::time::Instant;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DownloadState {
    Pending,
    Downloading,
    Completed,
    Failed,
    Cancelled,
    Paused,
}

#[derive(Debug, Clone)]
pub struct ProgressSnapshot {
    pub total_size: Option<u64>,
    pub downloaded: u64,
    pub state: DownloadState,
    pub speed_bytes_per_sec: f64,
    pub start_time: Option<Instant>,
}

impl Default for ProgressSnapshot {
    fn default() -> Self {
        Self {
            total_size: None,
            downloaded: 0,
            state: DownloadState::Pending,
            speed_bytes_per_sec: 0.0,
            start_time: None,
        }
    }
}
