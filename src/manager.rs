use std::time::Duration;

use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::config::{DownloadSpec, LogLevel};
use crate::error::DownloadError;
use crate::logging::next_download_id;
use crate::network::ClientNetworkConfig;
use crate::progress::ProgressSnapshot;
use crate::session;

/// Top-level downloader that manages shared resources (e.g. HTTP client).
pub struct Downloader {
    client: reqwest::Client,
    client_config: ClientNetworkConfig,
    log_level: LogLevel,
}

/// Builder for [`Downloader`].
pub struct DownloaderBuilder {
    client_config: ClientNetworkConfig,
    log_level: LogLevel,
}

impl DownloaderBuilder {
    pub fn connect_timeout(mut self, timeout: Duration) -> Self {
        self.client_config.connect_timeout = timeout;
        self
    }

    pub fn all_proxy(mut self, proxy: impl Into<String>) -> Self {
        self.client_config.all_proxy = Some(proxy.into());
        self
    }

    pub fn http_proxy(mut self, proxy: impl Into<String>) -> Self {
        self.client_config.http_proxy = Some(proxy.into());
        self
    }

    pub fn https_proxy(mut self, proxy: impl Into<String>) -> Self {
        self.client_config.https_proxy = Some(proxy.into());
        self
    }

    pub fn dns_server(mut self, server: std::net::SocketAddr) -> Self {
        self.client_config.dns_servers.push(server);
        self
    }

    pub fn dns_servers<I>(mut self, servers: I) -> Self
    where
        I: IntoIterator<Item = std::net::SocketAddr>,
    {
        self.client_config.dns_servers = servers.into_iter().collect();
        self
    }

    pub fn enable_ipv6(mut self, enabled: bool) -> Self {
        self.client_config.enable_ipv6 = enabled;
        self
    }

    pub fn log_level(mut self, level: LogLevel) -> Self {
        self.log_level = level;
        self
    }

    pub fn build(self) -> Result<Downloader, DownloadError> {
        let log_level = self.log_level;
        let client = self.client_config.build_client()?;
        log_debug!(
            log_level,
            log_level = %log_level,
            connect_timeout_ms = self.client_config.connect_timeout.as_millis() as u64,
            has_proxy = self.client_config.all_proxy.is_some()
                || self.client_config.http_proxy.is_some()
                || self.client_config.https_proxy.is_some(),
            custom_dns_count = self.client_config.dns_servers.len(),
            ipv6 = self.client_config.enable_ipv6,
            "downloader built"
        );
        Ok(Downloader {
            client,
            client_config: self.client_config,
            log_level,
        })
    }
}

impl Downloader {
    pub fn builder() -> DownloaderBuilder {
        DownloaderBuilder {
            client_config: ClientNetworkConfig::default(),
            log_level: LogLevel::default(),
        }
    }

    /// Start a download and return a handle for monitoring / cancellation.
    pub fn download(&self, spec: DownloadSpec) -> DownloadHandle {
        let (progress_tx, progress_rx) = watch::channel(ProgressSnapshot::default());
        let (cancel_tx, cancel_rx) = watch::channel(session::StopSignal::Running);
        let shared_client = self.client.clone();
        let client_config = self.client_config.clone();
        let log_level = self.log_level;
        let download_id = next_download_id();
        let output = spec
            .output_path
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "<auto>".to_string());

        log_info!(
            log_level,
            download_id,
            url = %spec.url,
            output = %output,
            max_connections = spec.max_connections,
            resume = spec.resume,
            "download task created"
        );

        let task = tokio::spawn(async move {
            let client = if spec.connect_timeout == client_config.connect_timeout {
                shared_client
            } else {
                client_config
                    .with_connect_timeout(spec.connect_timeout)
                    .build_client()?
            };
            session::run_download(client, spec, log_level, download_id, progress_tx, cancel_rx)
                .await
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
    cancel_tx: watch::Sender<session::StopSignal>,
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
        let _ = self.cancel_tx.send(session::StopSignal::Cancel);
    }

    /// Request the download to pause and persist resume state.
    pub fn pause(&self) {
        let _ = self.cancel_tx.send(session::StopSignal::Pause);
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
    fn test_downloader_builder_with_log_level() {
        let downloader = Downloader::builder()
            .log_level(crate::config::LogLevel::Debug)
            .build()
            .unwrap();
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

    #[test]
    fn test_downloader_builder_proxy_and_dns_options() {
        let downloader = Downloader::builder()
            .all_proxy("http://127.0.0.1:7890")
            .dns_server(std::net::SocketAddr::from(([1, 1, 1, 1], 53)))
            .enable_ipv6(false)
            .build()
            .unwrap();
        drop(downloader);
    }

    #[tokio::test]
    async fn test_download_handle_progress_default() {
        let downloader = Downloader::builder().build().unwrap();
        let spec = crate::config::DownloadSpec::new("http://127.0.0.1:1/nonexistent")
            .output_path(std::env::temp_dir().join("bytehaul_test_never_created"));
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

    #[tokio::test]
    async fn test_download_with_logging_enabled() {
        let downloader = Downloader::builder()
            .log_level(crate::config::LogLevel::Debug)
            .build()
            .unwrap();
        let spec = crate::config::DownloadSpec::new("http://127.0.0.1:1/nonexistent")
            .output_path(std::env::temp_dir().join("bytehaul_test_log_enabled"));
        let handle = downloader.download(spec);
        handle.cancel();
        let result = handle.wait().await;
        assert!(result.is_err());
    }

    #[test]
    fn test_downloader_builder_sets_scheme_specific_proxies_and_dns_servers() {
        let servers = vec![
            std::net::SocketAddr::from(([1, 1, 1, 1], 53)),
            std::net::SocketAddr::from(([8, 8, 8, 8], 53)),
        ];

        let builder = Downloader::builder()
            .http_proxy("http://127.0.0.1:8080")
            .https_proxy("http://127.0.0.1:8443")
            .dns_servers(servers.clone());

        assert_eq!(
            builder.client_config.http_proxy.as_deref(),
            Some("http://127.0.0.1:8080")
        );
        assert_eq!(
            builder.client_config.https_proxy.as_deref(),
            Some("http://127.0.0.1:8443")
        );
        assert_eq!(builder.client_config.dns_servers, servers);
    }

    #[tokio::test]
    async fn test_download_rebuilds_client_for_spec_timeout_override() {
        let downloader = Downloader::builder().build().unwrap();
        let mut spec = crate::config::DownloadSpec::new("http://127.0.0.1:1/nonexistent")
            .output_path(std::env::temp_dir().join("bytehaul_test_timeout_override"));
        spec.connect_timeout = Duration::from_secs(1);

        let handle = downloader.download(spec);
        let result = handle.wait().await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_download_handle_wait_maps_panics() {
        let (progress_tx, progress_rx) = watch::channel(ProgressSnapshot::default());
        let (cancel_tx, _) = watch::channel(session::StopSignal::Running);
        drop(progress_tx);

        let handle = DownloadHandle {
            progress_rx,
            cancel_tx,
            task: tokio::spawn(async {
                panic!("boom");
                #[allow(unreachable_code)]
                Ok(())
            }),
        };

        let err = handle.wait().await.unwrap_err().to_string();
        assert!(err.contains("task panicked"));
    }
}
