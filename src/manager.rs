use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use tokio::sync::{watch, Semaphore};
use tokio::task::JoinHandle;

use crate::config::{DownloadSpec, LogLevel};
use crate::error::DownloadError;
use crate::logging::next_download_id;
use crate::network::ClientNetworkConfig;
use crate::progress::{DownloadState, ProgressSnapshot};
use crate::session;

/// Top-level downloader that manages shared resources (e.g. HTTP client).
pub struct Downloader {
    client_cache: Arc<Mutex<HashMap<ClientNetworkConfig, reqwest::Client>>>,
    client_config: ClientNetworkConfig,
    log_level: LogLevel,
    concurrency_limit: Option<Arc<Semaphore>>,
}

/// Builder for [`Downloader`].
pub struct DownloaderBuilder {
    client_config: ClientNetworkConfig,
    log_level: LogLevel,
    max_concurrent_downloads: Option<usize>,
}

impl DownloaderBuilder {
    /// Set the default TCP connect timeout for the HTTP client (default: 30 s).
    pub fn connect_timeout(mut self, timeout: Duration) -> Self {
        self.client_config.connect_timeout = timeout;
        self
    }

    /// Set an HTTP/HTTPS/SOCKS proxy for all requests.
    pub fn all_proxy(mut self, proxy: impl Into<String>) -> Self {
        self.client_config.all_proxy = Some(proxy.into());
        self
    }

    /// Set a proxy used only for plain HTTP requests.
    pub fn http_proxy(mut self, proxy: impl Into<String>) -> Self {
        self.client_config.http_proxy = Some(proxy.into());
        self
    }

    /// Set a proxy used only for HTTPS requests.
    pub fn https_proxy(mut self, proxy: impl Into<String>) -> Self {
        self.client_config.https_proxy = Some(proxy.into());
        self
    }

    /// Add a custom DNS server address.
    pub fn dns_server(mut self, server: std::net::SocketAddr) -> Self {
        self.client_config.dns_servers.push(server);
        self
    }

    /// Replace the DNS server list with the given addresses.
    pub fn dns_servers<I>(mut self, servers: I) -> Self
    where
        I: IntoIterator<Item = std::net::SocketAddr>,
    {
        self.client_config.dns_servers = servers.into_iter().collect();
        self
    }

    /// Add a DNS-over-HTTPS (DoH) server URL.
    pub fn doh_server(mut self, server: impl Into<String>) -> Self {
        self.client_config.doh_servers.push(server.into());
        self
    }

    /// Replace the DoH server list with the given URLs.
    pub fn doh_servers<I, S>(mut self, servers: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.client_config.doh_servers = servers.into_iter().map(Into::into).collect();
        self
    }

    /// Enable or disable IPv6 support (default: `true`).
    pub fn enable_ipv6(mut self, enabled: bool) -> Self {
        self.client_config.enable_ipv6 = enabled;
        self
    }

    /// Use the platform-native TLS stack instead of the default rustls backend.
    ///
    /// When `true`, bytehaul will use the OS-native TLS implementation:
    /// OpenSSL on Linux, SChannel on Windows, or Secure Transport on macOS.
    /// When `false` (the default), the pure-Rust rustls library is used.
    ///
    /// Switching to native TLS may help diagnose CDN compatibility issues
    /// caused by differences in TLS behaviour between rustls and OpenSSL.
    pub fn use_native_tls(mut self, enabled: bool) -> Self {
        self.client_config.use_native_tls = enabled;
        self
    }

    /// Set the log verbosity level for download tasks (default: [`LogLevel::Off`]).
    pub fn log_level(mut self, level: LogLevel) -> Self {
        self.log_level = level;
        self
    }

    /// Limit the number of downloads that can run concurrently.
    ///
    /// Additional downloads will wait for a semaphore permit.
    pub fn max_concurrent_downloads(mut self, limit: usize) -> Self {
        self.max_concurrent_downloads = Some(limit);
        self
    }

    /// Build the [`Downloader`] instance.
    ///
    /// Returns an error if the HTTP client cannot be constructed
    /// (e.g. an invalid proxy URL).
    pub fn build(self) -> Result<Downloader, DownloadError> {
        let log_level = self.log_level;
        let client = self.client_config.build_client()?;
        let client_cache = Arc::new(Mutex::new(HashMap::from([(
            self.client_config.clone(),
            client,
        )])));
        log_debug!(
            log_level,
            log_level = %log_level,
            connect_timeout_ms = self.client_config.connect_timeout.as_millis() as u64,
            has_proxy = self.client_config.all_proxy.is_some()
                || self.client_config.http_proxy.is_some()
                || self.client_config.https_proxy.is_some(),
            custom_dns_count = self.client_config.dns_servers.len(),
            custom_doh_count = self.client_config.doh_servers.len(),
            ipv6 = self.client_config.enable_ipv6,
            "downloader built"
        );
        Ok(Downloader {
            client_cache,
            client_config: self.client_config,
            log_level,
            concurrency_limit: self
                .max_concurrent_downloads
                .map(|n| Arc::new(Semaphore::new(n))),
        })
    }
}

impl Downloader {
    /// Create a new [`DownloaderBuilder`] with default settings.
    pub fn builder() -> DownloaderBuilder {
        DownloaderBuilder {
            client_config: ClientNetworkConfig::default(),
            log_level: LogLevel::default(),
            max_concurrent_downloads: None,
        }
    }

    /// Start a download and return a handle for monitoring / cancellation.
    pub fn download(&self, spec: DownloadSpec) -> DownloadHandle {
        let (progress_tx, progress_rx) = watch::channel(ProgressSnapshot::default());
        let (cancel_tx, cancel_rx) = watch::channel(session::StopSignal::Running);
        let log_level = self.log_level;
        let download_id = next_download_id();

        if let Err(error) = spec.validate() {
            log_error!(
                log_level,
                download_id,
                url = %spec.url,
                error = %error,
                "download task rejected due to invalid configuration"
            );
            let task = tokio::spawn(async move { Err(error) });
            return DownloadHandle {
                progress_rx,
                cancel_tx,
                task,
            };
        }

        let client_cache = self.client_cache.clone();
        let client_config = self.client_config.clone();
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

        let concurrency_limit = self.concurrency_limit.clone();
        let task = tokio::spawn(async move {
            // Acquire a concurrency permit if a limit is configured.
            // The permit is held for the lifetime of this download task.
            let _permit = match &concurrency_limit {
                Some(sem) => Some(sem.acquire().await.map_err(|_| {
                    DownloadError::Internal("concurrency semaphore closed".into())
                })?),
                None => None,
            };
            let requested_config = if spec.connect_timeout == client_config.connect_timeout {
                client_config.clone()
            } else {
                client_config.with_connect_timeout(spec.connect_timeout)
            };
            let client = cached_client_for_config(&client_cache, requested_config)?;
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

fn cached_client_for_config(
    client_cache: &Arc<Mutex<HashMap<ClientNetworkConfig, reqwest::Client>>>,
    requested_config: ClientNetworkConfig,
) -> Result<reqwest::Client, DownloadError> {
    if let Some(client) = client_cache.lock().get(&requested_config).cloned() {
        return Ok(client);
    }

    let client = requested_config.build_client()?;
    let mut cache = client_cache.lock();
    Ok(cache
        .entry(requested_config)
        .or_insert_with(|| client.clone())
        .clone())
}

impl Downloader {
    pub(crate) fn bench_cached_client_lookup(
        &self,
        connect_timeout: Duration,
    ) -> Result<(), DownloadError> {
        let requested_config = if connect_timeout == self.client_config.connect_timeout {
            self.client_config.clone()
        } else {
            self.client_config.with_connect_timeout(connect_timeout)
        };
        cached_client_for_config(&self.client_cache, requested_config).map(|_| ())
    }

    pub(crate) fn bench_cached_client_count(&self) -> usize {
        self.client_cache.lock().len()
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

    /// Register a progress callback that is invoked whenever the progress snapshot changes.
    ///
    /// The callback runs on a spawned tokio task and receives each new [`ProgressSnapshot`].
    /// It continues until the download finishes (state becomes terminal) or the handle is dropped.
    pub fn on_progress<F>(&self, callback: F)
    where
        F: Fn(ProgressSnapshot) + Send + 'static,
    {
        let mut rx = self.progress_rx.clone();
        tokio::spawn(async move {
            while rx.changed().await.is_ok() {
                let snap = rx.borrow().clone();
                let terminal = !matches!(snap.state, DownloadState::Pending | DownloadState::Downloading);
                callback(snap);
                if terminal {
                    break;
                }
            }
        });
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
            Err(e) => Err(DownloadError::TaskFailed(format!("task panicked: {e}"))),
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
            .doh_server("https://127.0.0.1/dns-query")
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

    #[tokio::test]
    async fn test_download_rejects_invalid_spec_before_network_work() {
        let downloader = Downloader::builder().build().unwrap();
        let spec = crate::config::DownloadSpec::new("http://127.0.0.1:1/nonexistent")
            .output_path(std::env::temp_dir().join("bytehaul_test_invalid_spec"))
            .max_connections(0);

        let handle = downloader.download(spec);
        let err = handle.wait().await.unwrap_err();
        assert!(matches!(err, crate::error::DownloadError::InvalidConfig(message) if message.contains("max_connections")));
    }

    #[test]
    fn test_downloader_builder_max_concurrent_downloads() {
        let d = Downloader::builder()
            .max_concurrent_downloads(3)
            .build()
            .unwrap();
        let sem = d.concurrency_limit.as_ref().expect("semaphore should exist");
        assert_eq!(sem.available_permits(), 3);
    }

    #[test]
    fn test_downloader_builder_no_concurrency_limit_by_default() {
        let d = Downloader::builder().build().unwrap();
        assert!(d.concurrency_limit.is_none());
    }

    #[test]
    fn test_downloader_builder_sets_scheme_specific_proxies_and_dns_servers() {
        let servers = vec![
            std::net::SocketAddr::from(([1, 1, 1, 1], 53)),
            std::net::SocketAddr::from(([8, 8, 8, 8], 53)),
        ];
        let doh_servers = vec![
            "https://127.0.0.1/dns-query".to_string(),
            "https://localhost/custom-dns".to_string(),
        ];

        let builder = Downloader::builder()
            .http_proxy("http://127.0.0.1:8080")
            .https_proxy("http://127.0.0.1:8443")
            .dns_servers(servers.clone())
            .doh_servers(doh_servers.clone());

        assert_eq!(
            builder.client_config.http_proxy.as_deref(),
            Some("http://127.0.0.1:8080")
        );
        assert_eq!(
            builder.client_config.https_proxy.as_deref(),
            Some("http://127.0.0.1:8443")
        );
        assert_eq!(builder.client_config.dns_servers, servers);
        assert_eq!(builder.client_config.doh_servers, doh_servers);
    }

    #[tokio::test]
    async fn test_download_rebuilds_client_for_spec_timeout_override() {
        let downloader = Downloader::builder().build().unwrap();
        let mut spec = crate::config::DownloadSpec::new("http://127.0.0.1:1/nonexistent")
            .output_path(std::env::temp_dir().join("bytehaul_test_timeout_override"));
        spec.connect_timeout = Duration::from_secs(1);

        assert_eq!(downloader.client_cache.lock().len(), 1);
        let handle = downloader.download(spec);
        let result = handle.wait().await;
        assert!(result.is_err());
        assert_eq!(downloader.client_cache.lock().len(), 2);
    }

    #[tokio::test]
    async fn test_download_reuses_cached_timeout_override_client() {
        let downloader = Downloader::builder().build().unwrap();
        let mut spec = crate::config::DownloadSpec::new("http://127.0.0.1:1/nonexistent")
            .output_path(std::env::temp_dir().join("bytehaul_test_timeout_override_reuse"));
        spec.connect_timeout = Duration::from_secs(1);

        let _ = downloader.download(spec.clone()).wait().await;
        assert_eq!(downloader.client_cache.lock().len(), 2);

        let _ = downloader.download(spec).wait().await;
        assert_eq!(downloader.client_cache.lock().len(), 2);
    }

    #[test]
    fn test_cached_client_lookup_reuses_existing_entry() {
        let downloader = Downloader::builder().build().unwrap();

        downloader
            .bench_cached_client_lookup(Duration::from_secs(30))
            .unwrap();
        assert_eq!(downloader.bench_cached_client_count(), 1);

        downloader
            .bench_cached_client_lookup(Duration::from_secs(10))
            .unwrap();
        assert_eq!(downloader.bench_cached_client_count(), 2);

        downloader
            .bench_cached_client_lookup(Duration::from_secs(10))
            .unwrap();
        assert_eq!(downloader.bench_cached_client_count(), 2);
    }

    #[tokio::test]
    async fn test_download_reports_closed_concurrency_semaphore() {
        let downloader = Downloader::builder()
            .max_concurrent_downloads(1)
            .build()
            .unwrap();
        downloader
            .concurrency_limit
            .as_ref()
            .expect("semaphore should exist")
            .close();

        let spec = crate::config::DownloadSpec::new("http://127.0.0.1:1/nonexistent")
            .output_path(std::env::temp_dir().join("bytehaul_test_closed_semaphore"));

        let err = downloader.download(spec).wait().await.unwrap_err();
        assert!(matches!(err, crate::error::DownloadError::Internal(message) if message.contains("concurrency semaphore closed")));
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

    #[tokio::test]
    async fn test_on_progress_receives_updates() {
        let (progress_tx, progress_rx) = watch::channel(ProgressSnapshot::default());
        let (cancel_tx, _) = watch::channel(session::StopSignal::Running);

        let task = tokio::spawn(async { Ok(()) });
        let handle = DownloadHandle {
            progress_rx,
            cancel_tx,
            task,
        };

        let received = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let received_clone = received.clone();
        handle.on_progress(move |_snap| {
            received_clone.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        });

        // Send a progress update
        let snap = ProgressSnapshot {
            state: crate::progress::DownloadState::Downloading,
            downloaded: 100,
            ..Default::default()
        };
        progress_tx.send(snap).unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Send a terminal update
        let snap = ProgressSnapshot {
            state: crate::progress::DownloadState::Completed,
            ..Default::default()
        };
        progress_tx.send(snap).unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        assert!(received.load(std::sync::atomic::Ordering::Relaxed) >= 2);
    }

    #[tokio::test]
    async fn test_on_progress_stops_for_all_terminal_states() {
        use std::sync::atomic::{AtomicU32, Ordering};

        let terminal_states = [
            crate::progress::DownloadState::Completed,
            crate::progress::DownloadState::Failed,
            crate::progress::DownloadState::Cancelled,
            crate::progress::DownloadState::Paused,
        ];

        for state in terminal_states {
            let (progress_tx, progress_rx) = watch::channel(ProgressSnapshot::default());
            let (cancel_tx, _) = watch::channel(session::StopSignal::Running);
            let received = Arc::new(AtomicU32::new(0));
            let received_clone = received.clone();

            let handle = DownloadHandle {
                progress_rx,
                cancel_tx,
                task: tokio::spawn(async { Ok(()) }),
            };

            handle.on_progress(move |_snap| {
                received_clone.fetch_add(1, Ordering::Relaxed);
            });

            progress_tx
                .send(ProgressSnapshot {
                    state,
                    ..Default::default()
                })
                .unwrap();
            tokio::time::sleep(Duration::from_millis(50)).await;

            progress_tx
                .send(ProgressSnapshot {
                    state: crate::progress::DownloadState::Downloading,
                    downloaded: 1,
                    ..Default::default()
                })
                .unwrap();
            tokio::time::sleep(Duration::from_millis(50)).await;

            assert_eq!(received.load(Ordering::Relaxed), 1);
        }
    }

    #[tokio::test]
    async fn test_on_progress_emits_terminal_states_deterministically() {
        use tokio::sync::mpsc;

        let terminal_states = [
            crate::progress::DownloadState::Completed,
            crate::progress::DownloadState::Failed,
            crate::progress::DownloadState::Cancelled,
            crate::progress::DownloadState::Paused,
        ];

        for state in terminal_states {
            let (progress_tx, progress_rx) = watch::channel(ProgressSnapshot::default());
            let (cancel_tx, _) = watch::channel(session::StopSignal::Running);
            let (event_tx, mut event_rx) = mpsc::unbounded_channel();

            let handle = DownloadHandle {
                progress_rx,
                cancel_tx,
                task: tokio::spawn(async { Ok(()) }),
            };

            handle.on_progress(move |snap| {
                let _ = event_tx.send(snap.state);
            });

            progress_tx
                .send(ProgressSnapshot {
                    state,
                    ..Default::default()
                })
                .unwrap();

            let observed = tokio::time::timeout(Duration::from_secs(1), event_rx.recv())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(observed, state);
        }
    }
}
