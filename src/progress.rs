use std::time::Instant;

/// Current state of a download task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DownloadState {
    /// Waiting to start (initial state).
    Pending,
    /// Actively transferring data.
    Downloading,
    /// Download finished successfully.
    Completed,
    /// Download failed with an error.
    Failed,
    /// Download was cancelled by the user.
    Cancelled,
    /// Download was paused and resume state saved.
    Paused,
}

/// A point-in-time snapshot of download progress.
///
/// Obtained via [`DownloadHandle::progress()`](crate::DownloadHandle::progress),
/// the watch channel, or the progress callback.
#[derive(Debug, Clone)]
pub struct ProgressSnapshot {
    /// Total file size in bytes, if known from the server response.
    pub total_size: Option<u64>,
    /// Number of bytes downloaded so far.
    pub downloaded: u64,
    /// Current download state.
    pub state: DownloadState,
    /// Smoothed download speed in bytes per second.
    pub speed_bytes_per_sec: f64,
    /// Estimated time remaining in seconds, if calculable.
    pub eta_secs: Option<f64>,
    /// Timestamp when the download started transferring data.
    pub start_time: Option<Instant>,
}

impl Default for ProgressSnapshot {
    fn default() -> Self {
        Self {
            total_size: None,
            downloaded: 0,
            state: DownloadState::Pending,
            speed_bytes_per_sec: 0.0,
            eta_secs: None,
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
        assert_eq!(snap.eta_secs, None);
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
            eta_secs: Some(5.0),
            start_time: Some(Instant::now()),
        };
        let cloned = snap.clone();
        assert_eq!(cloned.total_size, Some(1000));
        assert_eq!(cloned.downloaded, 500);
        assert_eq!(cloned.state, DownloadState::Downloading);
        assert_eq!(cloned.eta_secs, Some(5.0));
    }
}
