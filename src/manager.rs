use std::time::Duration;

use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::config::DownloadSpec;
use crate::error::DownloadError;
use crate::progress::ProgressSnapshot;
use crate::session;

/// Top-level downloader that manages shared resources (e.g. HTTP client).
pub struct Downloader {
    client: reqwest::Client,
}

/// Builder for [`Downloader`].
pub struct DownloaderBuilder {
    connect_timeout: Duration,
}

impl DownloaderBuilder {
    pub fn connect_timeout(mut self, timeout: Duration) -> Self {
        self.connect_timeout = timeout;
        self
    }

    pub fn build(self) -> Result<Downloader, DownloadError> {
        let client = reqwest::Client::builder()
            .connect_timeout(self.connect_timeout)
            .build()?;
        Ok(Downloader { client })
    }
}

impl Downloader {
    pub fn builder() -> DownloaderBuilder {
        DownloaderBuilder {
            connect_timeout: Duration::from_secs(30),
        }
    }

    /// Start a download and return a handle for monitoring / cancellation.
    pub fn download(&self, spec: DownloadSpec) -> DownloadHandle {
        let (progress_tx, progress_rx) = watch::channel(ProgressSnapshot::default());
        let (cancel_tx, cancel_rx) = watch::channel(false);
        let client = self.client.clone();

        let task = tokio::spawn(async move {
            session::run_download(client, spec, progress_tx, cancel_rx).await
        });

        DownloadHandle {
            progress_rx,
            cancel_tx,
            task,
        }
    }
}

/// Handle to a running download task.
pub struct DownloadHandle {
    progress_rx: watch::Receiver<ProgressSnapshot>,
    cancel_tx: watch::Sender<bool>,
    task: JoinHandle<Result<(), DownloadError>>,
}

impl DownloadHandle {
    /// Get a snapshot of the current download progress.
    pub fn progress(&self) -> ProgressSnapshot {
        self.progress_rx.borrow().clone()
    }

    /// Get a clone of the progress watch receiver for async monitoring.
    pub fn subscribe_progress(&self) -> watch::Receiver<ProgressSnapshot> {
        self.progress_rx.clone()
    }

    /// Request cancellation of the download.
    pub fn cancel(&self) {
        let _ = self.cancel_tx.send(true);
    }

    /// Wait for the download to finish and return the result.
    pub async fn wait(self) -> Result<(), DownloadError> {
        match self.task.await {
            Ok(result) => result,
            Err(e) => Err(DownloadError::Other(format!("task panicked: {e}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_downloader_builder_default() {
        let downloader = Downloader::builder().build().unwrap();
        // Should construct without errors
        drop(downloader);
    }

    #[test]
    fn test_downloader_builder_custom_timeout() {
        let downloader = Downloader::builder()
            .connect_timeout(Duration::from_secs(10))
            .build()
            .unwrap();
        drop(downloader);
    }

    #[tokio::test]
    async fn test_download_handle_progress_default() {
        let downloader = Downloader::builder().build().unwrap();
        let spec = crate::config::DownloadSpec::new(
            "http://127.0.0.1:1/nonexistent",
            std::env::temp_dir().join("bytehaul_test_never_created"),
        );
        let handle = downloader.download(spec);

        // Initial progress should be pending
        let progress = handle.progress();
        assert_eq!(progress.state, crate::progress::DownloadState::Pending);

        // Test subscribe_progress
        let _rx = handle.subscribe_progress();

        handle.cancel();
        // Wait should return an error (cancelled or connection refused)
        let result = handle.wait().await;
        assert!(result.is_err());
    }
}
