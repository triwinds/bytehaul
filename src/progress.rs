use std::time::{Duration, Instant};

use tokio::sync::watch;

use crate::error::DownloadError;

pub(crate) const PROGRESS_REPORT_INTERVAL: Duration = Duration::from_millis(200);
pub(crate) const PROGRESS_REPORT_BYTES: u64 = 256 * 1024;

/// Current state of a download task.
///
/// Exactly one of the four end states is published per task, after the task has
/// finished its writer finalization, required cleanup and any configured
/// verification. The published end state always corresponds to the value
/// `wait()` returns: `Completed` for success, `Cancelled` / `Paused` for a stop
/// request, and `Failed` otherwise.
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
    /// UI-oriented downloaded byte count.
    ///
    /// In multi-worker mode this tracks bytes currently received from the network,
    /// so retries may temporarily move it backward. It is not the durable resume
    /// byte count stored in the control file.
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

/// The one public terminal state implied by a finished task's result.
///
/// A stop request keeps its own outcome (`Cancelled` / `Paused`); everything
/// else that did not succeed is a failure, including a rejected configuration,
/// a client that could not be built and a verification mismatch.
pub(crate) fn terminal_state_for(result: &Result<(), DownloadError>) -> DownloadState {
    match result {
        Ok(()) => DownloadState::Completed,
        Err(DownloadError::Cancelled) => DownloadState::Cancelled,
        Err(DownloadError::Paused) => DownloadState::Paused,
        Err(_) => DownloadState::Failed,
    }
}

/// Publish the terminal state of one download task.
///
/// Only the task-level exit calls this, and only after writer finalization,
/// required cleanup and any configured verification succeeded, so `Completed`
/// can neither precede a failure nor be published twice. The last reported
/// byte counts are preserved: this step decides the state, not the progress.
pub(crate) fn publish_terminal_state(
    progress_tx: &watch::Sender<ProgressSnapshot>,
    result: &Result<(), DownloadError>,
) {
    let state = terminal_state_for(result);
    progress_tx.send_modify(|progress| {
        progress.state = state;
        progress.eta_secs = match state {
            DownloadState::Completed => Some(0.0),
            _ => None,
        };
    });
}

/// A progress sample produced by a transfer loop.
///
/// It carries byte counts, speed and ETA only: the download state is owned by
/// the task-level exit, so a transfer loop cannot report a state transition that
/// the lifecycle has not decided yet.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ProgressUpdate {
    pub downloaded: u64,
    pub speed_bytes_per_sec: f64,
    pub eta_secs: Option<f64>,
}

impl ProgressUpdate {
    pub(crate) fn new(downloaded: u64, speed_bytes_per_sec: f64, eta_secs: Option<f64>) -> Self {
        Self {
            downloaded,
            speed_bytes_per_sec,
            eta_secs,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ProgressReporter {
    min_interval: Duration,
    min_bytes: u64,
    last_reported_downloaded: u64,
    last_reported_at: Instant,
}

impl ProgressReporter {
    pub(crate) fn new(
        initial_downloaded: u64,
        min_interval: Duration,
        min_bytes: u64,
        started_at: Instant,
    ) -> Self {
        Self {
            min_interval,
            min_bytes,
            last_reported_downloaded: initial_downloaded,
            last_reported_at: started_at,
        }
    }

    pub(crate) fn report_if_due(
        &mut self,
        progress_tx: &watch::Sender<ProgressSnapshot>,
        update: ProgressUpdate,
        now: Instant,
    ) -> bool {
        if !self.should_report(update.downloaded, now) {
            return false;
        }

        self.force_report(progress_tx, update, now);
        true
    }

    pub(crate) fn force_report(
        &mut self,
        progress_tx: &watch::Sender<ProgressSnapshot>,
        update: ProgressUpdate,
        now: Instant,
    ) {
        progress_tx.send_modify(|snapshot| {
            snapshot.downloaded = update.downloaded;
            snapshot.speed_bytes_per_sec = update.speed_bytes_per_sec;
            snapshot.eta_secs = update.eta_secs;
        });
        self.last_reported_downloaded = update.downloaded;
        self.last_reported_at = now;
    }

    fn should_report(&self, downloaded: u64, now: Instant) -> bool {
        downloaded.saturating_sub(self.last_reported_downloaded) >= self.min_bytes
            || now.duration_since(self.last_reported_at) >= self.min_interval
    }
}

#[doc(hidden)]
pub fn bench_progress_reporting(chunk_count: usize, chunk_size: usize, spacing_ms: u64) -> usize {
    let (progress_tx, _progress_rx) = watch::channel(ProgressSnapshot::default());
    let start_time = Instant::now();
    let mut reporter = ProgressReporter::new(
        0,
        PROGRESS_REPORT_INTERVAL,
        PROGRESS_REPORT_BYTES,
        start_time,
    );
    let mut downloaded = 0u64;
    let mut reports = 0usize;

    for index in 0..chunk_count {
        downloaded += chunk_size as u64;
        let now = start_time + Duration::from_millis((index as u64 + 1) * spacing_ms);
        if reporter.report_if_due(
            &progress_tx,
            ProgressUpdate::new(downloaded, chunk_size as f64, None),
            now,
        ) {
            reports += 1;
        }
    }

    reports
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

    #[test]
    fn test_progress_reporter_waits_for_byte_threshold() {
        let (progress_tx, progress_rx) = watch::channel(ProgressSnapshot::default());
        let start_time = Instant::now();
        let mut reporter = ProgressReporter::new(0, Duration::from_secs(5), 100, start_time);

        assert!(!reporter.report_if_due(
            &progress_tx,
            ProgressUpdate::new(99, 1.0, None),
            start_time + Duration::from_millis(50),
        ));
        assert_eq!(progress_rx.borrow().downloaded, 0);

        assert!(reporter.report_if_due(
            &progress_tx,
            ProgressUpdate::new(100, 1.0, None),
            start_time + Duration::from_millis(60),
        ));
        assert_eq!(progress_rx.borrow().downloaded, 100);
    }

    #[test]
    fn test_progress_reporter_waits_for_time_threshold() {
        let (progress_tx, progress_rx) = watch::channel(ProgressSnapshot::default());
        let start_time = Instant::now();
        let mut reporter = ProgressReporter::new(0, Duration::from_millis(100), 1024, start_time);

        assert!(!reporter.report_if_due(
            &progress_tx,
            ProgressUpdate::new(10, 1.0, Some(5.0)),
            start_time + Duration::from_millis(50),
        ));
        assert!(reporter.report_if_due(
            &progress_tx,
            ProgressUpdate::new(20, 2.0, Some(4.0)),
            start_time + Duration::from_millis(100),
        ));

        let snapshot = progress_rx.borrow().clone();
        assert_eq!(snapshot.downloaded, 20);
        assert_eq!(snapshot.speed_bytes_per_sec, 2.0);
        assert_eq!(snapshot.eta_secs, Some(4.0));
    }

    #[test]
    fn test_progress_reporter_force_report_applies_the_sample_without_a_state() {
        let (progress_tx, progress_rx) = watch::channel(ProgressSnapshot::default());
        let start_time = Instant::now();
        let mut reporter = ProgressReporter::new(0, Duration::from_secs(1), 1024, start_time);

        reporter.force_report(
            &progress_tx,
            ProgressUpdate::new(256, 12.5, Some(0.0)),
            start_time + Duration::from_millis(10),
        );

        let snapshot = progress_rx.borrow().clone();
        assert_eq!(snapshot.downloaded, 256);
        assert_eq!(snapshot.speed_bytes_per_sec, 12.5);
        assert_eq!(snapshot.eta_secs, Some(0.0));
        assert_eq!(
            snapshot.state,
            DownloadState::Pending,
            "a transfer loop reports bytes, not the download state"
        );
    }

    #[test]
    fn test_bench_progress_reporting_triggers_on_time_threshold() {
        let reports = bench_progress_reporting(10, 32 * 1024, 50);

        assert_eq!(reports, 2);
    }

    #[test]
    fn test_bench_progress_reporting_triggers_on_byte_threshold() {
        let reports = bench_progress_reporting(4, 300 * 1024, 10);

        assert_eq!(reports, 4);
    }

    #[test]
    fn test_terminal_state_follows_the_task_result() {
        assert_eq!(
            terminal_state_for(&Ok(())),
            DownloadState::Completed,
            "a successful task is Completed"
        );
        assert_eq!(
            terminal_state_for(&Err(DownloadError::Cancelled)),
            DownloadState::Cancelled
        );
        assert_eq!(
            terminal_state_for(&Err(DownloadError::Paused)),
            DownloadState::Paused
        );
        for failure in [
            DownloadError::InvalidConfig("url cannot be empty".into()),
            DownloadError::ChecksumMismatch {
                expected: "a".into(),
                actual: "b".into(),
            },
            DownloadError::Internal("writer failed".into()),
        ] {
            assert_eq!(
                terminal_state_for(&Err(failure)),
                DownloadState::Failed,
                "an unsuccessful task that was not stopped is Failed"
            );
        }
    }

    #[test]
    fn test_publish_terminal_state_keeps_byte_counts_and_sets_eta() {
        let (progress_tx, progress_rx) = watch::channel(ProgressSnapshot {
            total_size: Some(2048),
            downloaded: 2048,
            state: DownloadState::Downloading,
            speed_bytes_per_sec: 512.0,
            eta_secs: Some(0.0),
            start_time: Some(Instant::now()),
        });

        publish_terminal_state(
            &progress_tx,
            &Err(DownloadError::ChecksumMismatch {
                expected: "expected".into(),
                actual: "actual".into(),
            }),
        );

        let snapshot = progress_rx.borrow().clone();
        assert_eq!(snapshot.state, DownloadState::Failed);
        assert_eq!(snapshot.downloaded, 2048);
        assert_eq!(snapshot.total_size, Some(2048));
        assert_eq!(snapshot.eta_secs, None);

        publish_terminal_state(&progress_tx, &Ok(()));
        let snapshot = progress_rx.borrow().clone();
        assert_eq!(snapshot.state, DownloadState::Completed);
        assert_eq!(snapshot.downloaded, 2048);
        assert_eq!(snapshot.eta_secs, Some(0.0));
    }
}
