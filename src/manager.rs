use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use tokio::sync::{watch, Semaphore};
use tokio::task::JoinHandle;

use crate::config::{DownloadSpec, LogLevel};
use crate::error::DownloadError;
use crate::logging::next_download_id;
use crate::network::{BytehaulClient, ClientNetworkConfig};
use crate::progress::{publish_terminal_state, DownloadState, ProgressSnapshot};
use crate::session;

/// Top-level downloader that manages shared resources (e.g. HTTP client).
pub struct Downloader {
    client_cache: Arc<Mutex<ClientCache>>,
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
    /// Set the default TCP connect timeout for HTTP clients (default: 30 s).
    ///
    /// Individual downloads can override this via [`DownloadSpec::connect_timeout`].
    pub fn connect_timeout(mut self, timeout: Duration) -> Self {
        self.client_config.connect_timeout = timeout;
        self
    }

    /// Configure the HTTP idle pool for clients built by this downloader.
    pub fn http_idle_pool(mut self, max_idle_per_host: usize, idle_timeout: Duration) -> Self {
        self.client_config.pool_max_idle_per_host = max_idle_per_host;
        self.client_config.pool_idle_timeout = idle_timeout;
        self
    }

    /// Set the default proxy for all requests built by this downloader.
    ///
    /// Individual downloads can override this via [`DownloadSpec::all_proxy`].
    pub fn all_proxy(mut self, proxy: impl Into<String>) -> Self {
        self.client_config.all_proxy = Some(proxy.into());
        self
    }

    /// Set the default proxy used only for plain HTTP requests.
    ///
    /// Individual downloads can override this via [`DownloadSpec::http_proxy`].
    pub fn http_proxy(mut self, proxy: impl Into<String>) -> Self {
        self.client_config.http_proxy = Some(proxy.into());
        self
    }

    /// Set the default proxy used only for HTTPS requests.
    ///
    /// Individual downloads can override this via [`DownloadSpec::https_proxy`].
    pub fn https_proxy(mut self, proxy: impl Into<String>) -> Self {
        self.client_config.https_proxy = Some(proxy.into());
        self
    }

    /// Add a PEM trust bundle for the default libcurl client.
    pub fn ca_info(mut self, path: impl Into<PathBuf>) -> Self {
        self.client_config.ca_info = Some(path.into());
        self
    }

    /// Use a directory of hashed CA certificates for the default libcurl client.
    pub fn ca_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.client_config.ca_path = Some(path.into());
        self
    }

    /// Configure the client certificate for mutual TLS.
    pub fn client_cert(mut self, path: impl Into<PathBuf>) -> Self {
        self.client_config.client_cert = Some(path.into());
        self
    }

    /// Configure the private key for mutual TLS.
    pub fn client_key(mut self, path: impl Into<PathBuf>) -> Self {
        self.client_config.client_key = Some(path.into());
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
    /// (e.g. an invalid proxy URL), or if a configured concurrency limit is
    /// zero, which could never run a download (B4).
    pub fn build(self) -> Result<Downloader, DownloadError> {
        let log_level = self.log_level;
        if self.max_concurrent_downloads == Some(0) {
            return Err(DownloadError::InvalidConfig(
                "max_concurrent_downloads must be >= 1".into(),
            ));
        }
        let client = self.client_config.build_client()?;
        let client_cache = Arc::new(Mutex::new(ClientCache::default()));
        client_cache.lock().entries.push_back((
            ClientKey::new(self.client_config.clone()),
            Arc::new(Mutex::new(Some(client))),
        ));
        log_debug!(
            log_level,
            log_level = %log_level,
            connect_timeout_ms = self.client_config.connect_timeout.as_millis() as u64,
            pool_max_idle_per_host = self.client_config.pool_max_idle_per_host,
            pool_idle_timeout_ms = self.client_config.pool_idle_timeout.as_millis() as u64,
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
        let client_cache = self.client_cache.clone();
        let client_config = self.client_config.clone();
        let concurrency_limit = self.concurrency_limit.clone();
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
            resume = spec.storage.resume,
            "download task created"
        );

        let task = tokio::spawn(async move {
            let mut cancel_rx = cancel_rx;
            // One exit owns the whole task: configuration validation, waiting
            // for a concurrency permit, client construction, the transfer, the
            // writer's finalization and the configured verification. The result
            // returned here is the single source of the public terminal state,
            // and no sub-step publishes one of its own (B1, B2).
            let result = async {
                spec.validate().inspect_err(|error| {
                    log_error!(
                        log_level,
                        download_id,
                        url = %spec.url,
                        error = %error,
                        "download task rejected due to invalid configuration"
                    );
                })?;

                // Held for the lifetime of this task; a download that is still
                // queued must observe stop requests instead of waiting for
                // another download to release its permit (B3).
                let _permit = match &concurrency_limit {
                    Some(semaphore) => {
                        let queued_at = crate::bench_stats::phase_start();
                        let permit = acquire_download_permit(semaphore, &mut cancel_rx).await?;
                        crate::bench_stats::record_phase(
                            queued_at,
                            crate::bench_stats::record_queue_wait,
                        );
                        Some(permit)
                    }
                    None => None,
                };

                let requested_config = requested_client_config_for_spec(&client_config, &spec);
                let spec = spec.connect_timeout(requested_config.connect_timeout);
                let client = cached_client_for_config(&client_cache, requested_config)?;
                session::run_download(
                    client,
                    spec,
                    log_level,
                    download_id,
                    &progress_tx,
                    cancel_rx,
                )
                .await
            }
            .await;

            publish_terminal_state(&progress_tx, &result);
            result
        });

        DownloadHandle {
            progress_rx,
            cancel_tx,
            task,
        }
    }
}

/// Acquire a concurrency permit while continuing to observe stop requests.
///
/// The stop check comes first in a biased `select!`, so a request that was
/// already issued when this download was queued wins over an available permit.
/// Dropping the progress handle does not close the stop channel's sender
/// meaningfully here: `wait_for_stop` stays pending, and the download proceeds
/// exactly as it does today.
async fn acquire_download_permit<'a>(
    semaphore: &'a Semaphore,
    cancel_rx: &mut watch::Receiver<session::StopSignal>,
) -> Result<tokio::sync::SemaphorePermit<'a>, DownloadError> {
    tokio::select! {
        biased;
        error = session::wait_for_stop(cancel_rx) => Err(error),
        permit = semaphore.acquire() => permit.map_err(|_| {
            DownloadError::Internal("concurrency semaphore closed".into())
        }),
    }
}

fn requested_client_config_for_spec(
    base_config: &ClientNetworkConfig,
    spec: &DownloadSpec,
) -> ClientNetworkConfig {
    spec.resolve_network_config(base_config)
}

// Bounds retained references, including the default client, not active downloads.
const CLIENT_CACHE_CAPACITY: usize = 16;

#[derive(Clone, PartialEq, Eq)]
struct ClientKey(ClientNetworkConfig);
impl ClientKey {
    fn new(mut config: ClientNetworkConfig) -> Self {
        // Connection deadlines belong to easy handles, not shared pools.
        config.connect_timeout = Duration::from_secs(30);
        Self(config)
    }
}

type ClientEntry = Arc<Mutex<Option<BytehaulClient>>>;

#[derive(Default)]
struct ClientCache {
    // Least recently used first. A small fixed capacity keeps lookup bounded.
    entries: VecDeque<(ClientKey, ClientEntry)>,
}
impl ClientCache {
    fn len(&self) -> usize {
        self.entries.len()
    }
}

fn cached_client_for_config(
    client_cache: &Arc<Mutex<ClientCache>>,
    requested_config: ClientNetworkConfig,
) -> Result<BytehaulClient, DownloadError> {
    let key = ClientKey::new(requested_config);
    let (entry, evicted) = {
        let mut cache = client_cache.lock();
        if let Some(index) = cache
            .entries
            .iter()
            .position(|(existing, _)| existing == &key)
        {
            let existing = cache.entries.remove(index).expect("located cache entry");
            let entry = existing.1.clone();
            cache.entries.push_back(existing);
            (entry, None)
        } else {
            let entry = Arc::new(Mutex::new(None));
            let evicted = if cache.len() == CLIENT_CACHE_CAPACITY {
                cache.entries.pop_front()
            } else {
                None
            };
            cache.entries.push_back((key.clone(), entry.clone()));
            (entry, evicted)
        }
    };
    // Driver shutdown and construction never run under the global cache lock.
    drop(evicted);
    let mut client = entry.lock();
    if client.is_none() {
        *client = Some(key.0.build_client()?);
    }
    Ok(client.as_ref().expect("initialized client").clone())
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

    /// The client the downloader's default network configuration resolves to.
    ///
    /// Used by the local pipeline harness to read the driver counters of the
    /// connection the default download path actually uses.
    pub(crate) fn bench_default_client(&self) -> Result<BytehaulClient, DownloadError> {
        cached_client_for_config(&self.client_cache, self.client_config.clone())
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
                let terminal = !matches!(
                    snap.state,
                    DownloadState::Pending | DownloadState::Downloading
                );
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
    use warp::Filter;

    fn spawn_forbidden_server() -> String {
        let route = warp::any().map(|| {
            warp::http::Response::builder()
                .status(403)
                .body("Forbidden")
                .unwrap()
        });
        let (addr, server) = warp::serve(route).bind_ephemeral(([127, 0, 0, 1], 0));
        tokio::spawn(server);
        format!("http://{addr}")
    }

    async fn assert_forbidden(handle: DownloadHandle) {
        let error = tokio::time::timeout(Duration::from_secs(5), handle.wait())
            .await
            .expect("configuration test must not wait for network retry backoff")
            .unwrap_err();
        assert!(
            matches!(error, DownloadError::HttpStatus { status: 403, .. }),
            "expected HTTP 403 from the local fixture, got {error:?}"
        );
    }

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
    fn test_downloader_builder_http_idle_pool() {
        let downloader = Downloader::builder()
            .http_idle_pool(2, Duration::from_secs(11))
            .build()
            .unwrap();
        assert_eq!(downloader.client_config.pool_max_idle_per_host, 2);
        assert_eq!(
            downloader.client_config.pool_idle_timeout,
            Duration::from_secs(11)
        );
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
        assert!(
            matches!(err, crate::error::DownloadError::InvalidConfig(message) if message.contains("max_connections"))
        );
    }

    #[test]
    fn test_downloader_builder_max_concurrent_downloads() {
        let d = Downloader::builder()
            .max_concurrent_downloads(3)
            .build()
            .unwrap();
        let sem = d
            .concurrency_limit
            .as_ref()
            .expect("semaphore should exist");
        assert_eq!(sem.available_permits(), 3);
    }

    #[test]
    fn test_downloader_builder_no_concurrency_limit_by_default() {
        let d = Downloader::builder().build().unwrap();
        assert!(d.concurrency_limit.is_none());
    }

    #[test]
    fn test_downloader_builder_rejects_zero_concurrency_limit() {
        // B4: a limit of zero could never run a download, so building the
        // downloader must fail instead of queueing every task forever.
        let error = match Downloader::builder().max_concurrent_downloads(0).build() {
            Ok(_) => panic!("a zero concurrency limit must be rejected"),
            Err(error) => error,
        };
        assert!(
            matches!(error, DownloadError::InvalidConfig(ref message) if message.contains("max_concurrent_downloads")),
            "got {error:?}"
        );
    }

    #[tokio::test]
    async fn test_invalid_config_fails_without_touching_the_network_or_disk() {
        // B2: a rejected configuration must end as Failed, not stay Pending.
        let downloader = Downloader::builder().build().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let output_path = dir.path().join("invalid.bin");
        let spec = DownloadSpec::new("")
            .output_path(output_path.clone())
            .resume(true);

        let handle = downloader.download(spec);
        let progress = handle.subscribe_progress();
        let error = handle.wait().await.unwrap_err();

        assert!(
            matches!(error, DownloadError::InvalidConfig(_)),
            "got {error:?}"
        );
        let snapshot = progress.borrow().clone();
        assert_eq!(snapshot.state, DownloadState::Failed);
        assert_eq!(snapshot.downloaded, 0);
        assert!(!output_path.exists(), "no output for a rejected config");
        assert!(
            !dir.path().join("invalid.bin.bytehaul").exists(),
            "a task that never started must not create a checkpoint"
        );
    }

    #[tokio::test]
    async fn test_queued_download_stops_without_waiting_for_a_permit() {
        // B3: a download waiting for a concurrency permit must still observe
        // stop requests. The only permit is held by this test, which is exactly
        // what a running download would do, and it is never released.
        let downloader = Downloader::builder()
            .max_concurrent_downloads(1)
            .build()
            .unwrap();
        let semaphore = downloader.concurrency_limit.clone().unwrap();
        let _held = semaphore.clone().acquire_owned().await.unwrap();

        let dir = tempfile::tempdir().unwrap();
        let queued_spec = |name: &str| {
            DownloadSpec::new("http://127.0.0.1:1/nonexistent")
                .output_path(dir.path().join(name))
                .resume(true)
        };

        let cancelled = downloader.download(queued_spec("queued-cancel.bin"));
        let paused = downloader.download(queued_spec("queued-pause.bin"));
        let cancelled_progress = cancelled.subscribe_progress();
        let paused_progress = paused.subscribe_progress();

        // Neither task may have started while the permit is held.
        assert_eq!(cancelled.progress().state, DownloadState::Pending);
        assert_eq!(paused.progress().state, DownloadState::Pending);

        cancelled.cancel();
        paused.pause();

        let cancel_error = tokio::time::timeout(Duration::from_secs(5), cancelled.wait())
            .await
            .expect("a queued download must not wait for another download to release a permit")
            .unwrap_err();
        let pause_error = tokio::time::timeout(Duration::from_secs(5), paused.wait())
            .await
            .expect("a queued pause must end the task directly")
            .unwrap_err();

        assert!(matches!(cancel_error, DownloadError::Cancelled));
        assert!(matches!(pause_error, DownloadError::Paused));
        assert_eq!(cancelled_progress.borrow().state, DownloadState::Cancelled);
        assert_eq!(paused_progress.borrow().state, DownloadState::Paused);

        for name in ["queued-cancel.bin", "queued-pause.bin"] {
            assert!(
                !dir.path().join(name).exists(),
                "a stopped queued task must not create its output file"
            );
            assert!(
                !dir.path().join(format!("{name}.bytehaul")).exists(),
                "a stopped queued task must not create a checkpoint"
            );
        }
    }

    #[tokio::test]
    async fn test_dropping_the_handle_neither_cancels_nor_wedges_a_queued_download() {
        // The unified stop-wait stays pending once every signal sender is gone,
        // so dropping a handle keeps the existing semantics: the download is not
        // cancelled, and a queued download still receives its permit.
        let route = warp::any().map(|| warp::http::Response::new(b"payload".to_vec()));
        let (addr, server) = warp::serve(route).bind_ephemeral(([127, 0, 0, 1], 0));
        tokio::spawn(server);

        let downloader = Downloader::builder()
            .max_concurrent_downloads(1)
            .build()
            .unwrap();
        let semaphore = downloader.concurrency_limit.clone().unwrap();
        let held = semaphore.clone().acquire_owned().await.unwrap();

        let dir = tempfile::tempdir().unwrap();
        let output_path = dir.path().join("after-handle-drop.bin");
        let spec = DownloadSpec::new(format!("http://{addr}/file"))
            .output_path(output_path.clone())
            .resume(false);
        let handle = downloader.download(spec);
        let progress = handle.subscribe_progress();
        drop(handle);
        drop(held);

        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            if progress.borrow().state == DownloadState::Completed {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "a queued download must proceed after its handle is dropped"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(std::fs::read(&output_path).unwrap(), b"payload");
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
    async fn test_download_reuses_client_for_spec_timeout_override() {
        let server = spawn_forbidden_server();
        let dir = tempfile::tempdir().unwrap();
        let downloader = Downloader::builder().build().unwrap();
        let spec = crate::config::DownloadSpec::new(format!("{server}/timeout"))
            .output_path(dir.path().join("timeout-override.bin"))
            .connect_timeout(Duration::from_secs(1));

        assert_eq!(downloader.client_cache.lock().len(), 1);
        assert_forbidden(downloader.download(spec)).await;
        assert_eq!(downloader.client_cache.lock().len(), 1);
    }

    #[tokio::test]
    async fn test_download_reuses_cached_timeout_override_client() {
        let server = spawn_forbidden_server();
        let dir = tempfile::tempdir().unwrap();
        let downloader = Downloader::builder().build().unwrap();
        let spec = crate::config::DownloadSpec::new(format!("{server}/timeout-reuse"))
            .output_path(dir.path().join("timeout-reuse.bin"))
            .connect_timeout(Duration::from_secs(1));

        assert_eq!(downloader.client_cache.lock().len(), 1);
        assert_forbidden(downloader.download(spec.clone())).await;
        assert_eq!(downloader.client_cache.lock().len(), 1);

        assert_forbidden(downloader.download(spec)).await;
        assert_eq!(downloader.client_cache.lock().len(), 1);
    }

    #[tokio::test]
    async fn test_download_uses_builder_timeout_when_spec_has_no_override() {
        let server = spawn_forbidden_server();
        let dir = tempfile::tempdir().unwrap();
        let downloader = Downloader::builder()
            .connect_timeout(Duration::from_secs(3))
            .build()
            .unwrap();
        let spec = crate::config::DownloadSpec::new(format!("{server}/builder-timeout"))
            .output_path(dir.path().join("builder-timeout.bin"));

        assert_eq!(downloader.client_cache.lock().len(), 1);
        assert_forbidden(downloader.download(spec)).await;
        assert_eq!(downloader.client_cache.lock().len(), 1);
    }

    #[tokio::test]
    async fn test_download_rebuilds_client_for_spec_proxy_override() {
        let proxy = spawn_forbidden_server();
        let dir = tempfile::tempdir().unwrap();
        let downloader = Downloader::builder().build().unwrap();
        let spec = crate::config::DownloadSpec::new("http://proxy-target.invalid/nonexistent")
            .output_path(dir.path().join("proxy-override.bin"))
            .all_proxy(proxy);

        assert_eq!(downloader.client_cache.lock().len(), 1);
        assert_forbidden(downloader.download(spec)).await;
        assert_eq!(downloader.client_cache.lock().len(), 2);
    }

    #[tokio::test]
    async fn test_download_rebuilds_client_for_spec_idle_pool_override() {
        let server = spawn_forbidden_server();
        let dir = tempfile::tempdir().unwrap();
        let downloader = Downloader::builder().build().unwrap();
        let spec = crate::config::DownloadSpec::new(format!("{server}/idle-pool"))
            .output_path(dir.path().join("idle-pool.bin"))
            .http_idle_pool(2, Duration::from_secs(5));

        assert_eq!(downloader.client_cache.lock().len(), 1);
        assert_forbidden(downloader.download(spec)).await;
        assert_eq!(downloader.client_cache.lock().len(), 2);
    }

    #[tokio::test]
    async fn test_download_proxy_override_reuses_cached_client() {
        let proxy = spawn_forbidden_server();
        let dir = tempfile::tempdir().unwrap();
        let downloader = Downloader::builder().build().unwrap();
        let spec = crate::config::DownloadSpec::new("http://proxy-target.invalid/nonexistent")
            .output_path(dir.path().join("proxy-reuse.bin"))
            .all_proxy(proxy);

        assert_eq!(downloader.client_cache.lock().len(), 1);
        assert_forbidden(downloader.download(spec.clone())).await;
        assert_eq!(downloader.client_cache.lock().len(), 2);

        assert_forbidden(downloader.download(spec)).await;
        assert_eq!(downloader.client_cache.lock().len(), 2);
    }

    #[test]
    fn test_download_proxy_override_replaces_builder_proxy_defaults() {
        let downloader = Downloader::builder()
            .http_proxy("http://127.0.0.1:8080")
            .https_proxy("http://127.0.0.1:8443")
            .build()
            .unwrap();
        let spec = crate::config::DownloadSpec::new("http://127.0.0.1:1/nonexistent")
            .all_proxy("http://127.0.0.1:7890");

        let requested = requested_client_config_for_spec(&downloader.client_config, &spec);
        assert_eq!(
            requested.all_proxy.as_deref(),
            Some("http://127.0.0.1:7890")
        );
        assert!(requested.http_proxy.is_none());
        assert!(requested.https_proxy.is_none());
    }

    #[test]
    fn test_download_proxy_override_sets_scheme_specific_proxies() {
        let downloader = Downloader::builder()
            .all_proxy("http://127.0.0.1:9000")
            .build()
            .unwrap();
        let spec = crate::config::DownloadSpec::new("http://127.0.0.1:1/nonexistent")
            .http_proxy("http://127.0.0.1:8080")
            .https_proxy("http://127.0.0.1:8443");

        let requested = requested_client_config_for_spec(&downloader.client_config, &spec);
        assert!(requested.all_proxy.is_none());
        assert_eq!(
            requested.http_proxy.as_deref(),
            Some("http://127.0.0.1:8080")
        );
        assert_eq!(
            requested.https_proxy.as_deref(),
            Some("http://127.0.0.1:8443")
        );
    }

    #[test]
    fn test_download_pool_override_replaces_builder_pool_defaults() {
        let downloader = Downloader::builder()
            .http_idle_pool(3, Duration::from_secs(20))
            .build()
            .unwrap();
        let spec = crate::config::DownloadSpec::new("http://127.0.0.1:1/nonexistent")
            .disable_http_idle_pool();

        let requested = requested_client_config_for_spec(&downloader.client_config, &spec);
        assert_eq!(requested.pool_max_idle_per_host, 0);
        assert_eq!(requested.pool_idle_timeout, Duration::from_secs(20));
    }

    #[test]
    fn environment_proxy_is_resolved_on_client_creation_not_cache_hits() {
        const CHILD: &str = "BYTEHAUL_PROXY_CACHE_TEST_CHILD";
        if std::env::var_os(CHILD).is_some() {
            let downloader = Downloader::builder().build().unwrap();
            let first = downloader.bench_default_client().unwrap();
            // This test runs alone in a subprocess; no parallel test sees this.
            std::env::set_var("ALL_PROXY", "socks5://127.0.0.1:9");
            let reused = cached_client_for_config(
                &downloader.client_cache,
                ClientNetworkConfig {
                    connect_timeout: Duration::from_secs(1),
                    ..Default::default()
                },
            )
            .unwrap();
            assert!(same_transport(&first, &reused));
            let miss = cached_client_for_config(
                &downloader.client_cache,
                ClientNetworkConfig {
                    pool_max_idle_per_host: 19,
                    ..Default::default()
                },
            );
            assert!(matches!(miss, Err(DownloadError::InvalidConfig(_))));
            return;
        }
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command.args([
            "--exact",
            "manager::tests::environment_proxy_is_resolved_on_client_creation_not_cache_hits",
        ]);
        command.env(CHILD, "1");
        for key in [
            "ALL_PROXY",
            "all_proxy",
            "HTTP_PROXY",
            "http_proxy",
            "HTTPS_PROXY",
            "https_proxy",
        ] {
            command.env_remove(key);
        }
        assert!(command.status().unwrap().success());
    }

    #[tokio::test]
    async fn evicted_client_and_its_response_can_finish() {
        use crate::http::next_data_chunk;
        let server = spawn_forbidden_server();
        let downloader = Downloader::builder().build().unwrap();
        let client = downloader.bench_default_client().unwrap();
        let response = client
            .request(
                http::Request::get(server)
                    .body(crate::http::HttpRequestBody::new())
                    .unwrap(),
            )
            .await
            .unwrap();
        for idle in 100..100 + CLIENT_CACHE_CAPACITY {
            cached_client_for_config(
                &downloader.client_cache,
                ClientNetworkConfig {
                    pool_max_idle_per_host: idle,
                    ..Default::default()
                },
            )
            .unwrap();
        }
        assert!(!same_transport(
            &client,
            &downloader.bench_default_client().unwrap()
        ));
        drop(client);
        let mut body = response.into_body();
        while next_data_chunk(&mut body, Duration::from_secs(2))
            .await
            .unwrap()
            .is_some()
        {}
    }

    fn same_transport(left: &BytehaulClient, right: &BytehaulClient) -> bool {
        match (left, right) {
            (BytehaulClient::Curl(left), BytehaulClient::Curl(right)) => Arc::ptr_eq(left, right),
        }
    }

    #[test]
    fn cache_is_lru_bounded_and_eviction_does_not_drop_active_clients() {
        let downloader = Downloader::builder().build().unwrap();
        let default = downloader.bench_default_client().unwrap();
        let config = |idle| ClientNetworkConfig {
            pool_max_idle_per_host: idle,
            ..Default::default()
        };
        let first = cached_client_for_config(&downloader.client_cache, config(100)).unwrap();
        for idle in 101..100 + CLIENT_CACHE_CAPACITY {
            cached_client_for_config(&downloader.client_cache, config(idle)).unwrap();
        }
        assert_eq!(
            downloader.bench_cached_client_count(),
            CLIENT_CACHE_CAPACITY
        );
        let reused = cached_client_for_config(&downloader.client_cache, config(100)).unwrap();
        assert!(same_transport(&first, &reused));
        // Refreshing 100 evicts 101 on the next miss, rather than 100.
        cached_client_for_config(&downloader.client_cache, config(200)).unwrap();
        assert!(same_transport(
            &first,
            &cached_client_for_config(&downloader.client_cache, config(100)).unwrap()
        ));
        assert!(!same_transport(
            &default,
            &downloader.bench_default_client().unwrap()
        ));
        // The evicted default remains usable while this caller holds it.
        assert!(default.driver_stats().is_some());
    }

    #[test]
    fn concurrent_cache_misses_share_one_transport_and_timeouts_do_not_split_it() {
        let downloader = Downloader::builder().build().unwrap();
        let gate = Arc::new(std::sync::Barrier::new(8));
        let clients = std::thread::scope(|scope| {
            let tasks: Vec<_> = (0..8)
                .map(|index| {
                    let cache = downloader.client_cache.clone();
                    let gate = gate.clone();
                    scope.spawn(move || {
                        gate.wait();
                        cached_client_for_config(
                            &cache,
                            ClientNetworkConfig {
                                connect_timeout: Duration::from_secs(index + 1),
                                pool_max_idle_per_host: 17,
                                ..Default::default()
                            },
                        )
                        .unwrap()
                    })
                })
                .collect();
            tasks
                .into_iter()
                .map(|task| task.join().unwrap())
                .collect::<Vec<_>>()
        });
        assert_eq!(downloader.bench_cached_client_count(), 2);
        assert!(clients
            .iter()
            .all(|client| same_transport(client, &clients[0])));
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
        assert_eq!(downloader.bench_cached_client_count(), 1);

        downloader
            .bench_cached_client_lookup(Duration::from_secs(10))
            .unwrap();
        assert_eq!(downloader.bench_cached_client_count(), 1);
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
        assert!(
            matches!(err, crate::error::DownloadError::Internal(message) if message.contains("concurrency semaphore closed"))
        );
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
