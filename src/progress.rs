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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_progress_snapshot_default() {
        let snap = ProgressSnapshot::default();
        assert_eq!(snap.total_size, None);
        assert_eq!(snap.downloaded, 0);
        assert_eq!(snap.state, DownloadState::Pending);
        assert_eq!(snap.speed_bytes_per_sec, 0.0);
        assert!(snap.start_time.is_none());
    }

    #[test]
    fn test_download_state_eq() {
        assert_eq!(DownloadState::Pending, DownloadState::Pending);
        assert_ne!(DownloadState::Pending, DownloadState::Downloading);
        assert_ne!(DownloadState::Completed, DownloadState::Failed);
        assert_ne!(DownloadState::Cancelled, DownloadState::Paused);
    }

    #[test]
    fn test_progress_snapshot_clone() {
        let snap = ProgressSnapshot {
            total_size: Some(1000),
            downloaded: 500,
            state: DownloadState::Downloading,
            speed_bytes_per_sec: 100.0,
            start_time: Some(Instant::now()),
        };
        let cloned = snap.clone();
        assert_eq!(cloned.total_size, Some(1000));
        assert_eq!(cloned.downloaded, 500);
        assert_eq!(cloned.state, DownloadState::Downloading);
    }
}
