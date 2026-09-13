use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::error::DownloadError;

pub(crate) const DEFAULT_HTTP_IDLE_POOL_MAX_PER_HOST: usize = 4;
pub(crate) const DEFAULT_HTTP_IDLE_POOL_TIMEOUT: Duration = Duration::from_secs(30);
pub(crate) const DEFAULT_REQUEST_BATCH_SIZE: u64 = 4 * 1024 * 1024;
pub(crate) const DEFAULT_DYNAMIC_MIN_SPLIT_SIZE: u64 = 1024 * 1024;
pub(crate) const DEFAULT_DYNAMIC_MAX_REQUEST_SIZE: u64 = 64 * 1024 * 1024;

/// Shared scalar validation for language bindings and the Rust specification.
pub fn require_nonzero(field: &str, value: u128) -> Result<(), DownloadError> {
    if value == 0 {
        return Err(DownloadError::InvalidConfig(format!(
            "{field} must be >= 1"
        )));
    }
    Ok(())
}

/// Log verbosity level for download tasks.
///
/// The default is `Off`, which means no log events are emitted by the library.
/// When set to a non-`Off` value, log events up to and including that level
/// will be produced via the `tracing` crate. A subscriber must be installed
/// by the application (or the Python binding) to actually see the output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub enum LogLevel {
    /// No logging.
    #[default]
    Off = 0,
    /// Errors only.
    Error = 1,
    /// Errors and warnings.
    Warn = 2,
    /// Errors, warnings, and informational messages.
    Info = 3,
    /// All of the above plus debug details.
    Debug = 4,
    /// Most verbose level including trace-level details.
    Trace = 5,
}

impl std::fmt::Display for LogLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            LogLevel::Off => "off",
            LogLevel::Error => "error",
            LogLevel::Warn => "warn",
            LogLevel::Info => "info",
            LogLevel::Debug => "debug",
            LogLevel::Trace => "trace",
        };
        f.write_str(s)
    }
}

impl std::str::FromStr for LogLevel {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "off" => Ok(LogLevel::Off),
            "error" => Ok(LogLevel::Error),
            "warn" | "warning" => Ok(LogLevel::Warn),
            "info" => Ok(LogLevel::Info),
            "debug" => Ok(LogLevel::Debug),
            "trace" => Ok(LogLevel::Trace),
            _ => Err(format!(
                "invalid log level: '{s}'; expected one of: off, error, warn, info, debug, trace"
            )),
        }
    }
}

impl LogLevel {
    /// Check whether a `tracing::Level` should be emitted given this log level.
    pub(crate) fn enabled(self, level: tracing::Level) -> bool {
        match level {
            tracing::Level::ERROR => self >= LogLevel::Error,
            tracing::Level::WARN => self >= LogLevel::Warn,
            tracing::Level::INFO => self >= LogLevel::Info,
            tracing::Level::DEBUG => self >= LogLevel::Debug,
            tracing::Level::TRACE => self >= LogLevel::Trace,
        }
    }

    /// Convert to the corresponding `tracing::level_filters::LevelFilter`.
    pub fn to_tracing_level_filter(self) -> tracing::level_filters::LevelFilter {
        match self {
            LogLevel::Off => tracing::level_filters::LevelFilter::OFF,
            LogLevel::Error => tracing::level_filters::LevelFilter::ERROR,
            LogLevel::Warn => tracing::level_filters::LevelFilter::WARN,
            LogLevel::Info => tracing::level_filters::LevelFilter::INFO,
            LogLevel::Debug => tracing::level_filters::LevelFilter::DEBUG,
            LogLevel::Trace => tracing::level_filters::LevelFilter::TRACE,
        }
    }
}

/// File allocation strategy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FileAllocation {
    /// No pre-allocation; file grows as data is written.
    None,
    /// Pre-allocate the full file size by writing zeros before downloading.
    #[default]
    Prealloc,
}

/// Checksum algorithm for post-download verification.
#[derive(Debug, Clone)]
pub enum Checksum {
    /// SHA-256 digest (hex-encoded).
    Sha256(String),
    /// SHA-1 digest (hex-encoded).
    Sha1(String),
    /// MD5 digest (hex-encoded).
    Md5(String),
    /// SHA-512 digest (hex-encoded).
    Sha512(String),
}

/// Automatic recovery policy for known-size, multi-connection Range downloads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SlowTransferMode {
    /// Preserve ordinary timeout/retry behavior without performance recovery.
    Disabled,
    /// Recover persistently slow ranges and keep idle workers available.
    #[default]
    Adaptive,
    /// Also race eligible tail ranges using a bounded temporary-file challenger.
    AdaptiveWithHedging,
}

/// Strategy used to turn available piece ranges into ordinary HTTP requests.
///
/// `Fixed` preserves the `request_batch_size` behaviour. `Dynamic` plans
/// requests from the currently available ranges and request slots; its
/// `request_batch_size` value is ignored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum RangeSchedulingMode {
    /// Preserve the legacy fixed request batch limit.
    Fixed,
    /// Dynamically split the largest available contiguous ranges.
    #[default]
    Dynamic,
}

impl std::fmt::Display for RangeSchedulingMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Fixed => "fixed",
            Self::Dynamic => "dynamic",
        })
    }
}

impl std::str::FromStr for RangeSchedulingMode {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.to_ascii_lowercase().as_str() {
            "fixed" => Ok(Self::Fixed),
            "dynamic" => Ok(Self::Dynamic),
            _ => Err(format!(
                "invalid range scheduling mode: '{value}'; expected one of: fixed, dynamic"
            )),
        }
    }
}

pub(crate) fn effective_dynamic_min_split_size(piece_size: u64, configured: u64) -> u64 {
    let piece_size = piece_size.max(1);
    configured
        .max(piece_size)
        .div_ceil(piece_size)
        .saturating_mul(piece_size)
        .max(piece_size)
}

pub(crate) fn effective_dynamic_max_request_size(piece_size: u64, configured: u64) -> u64 {
    configured.max(piece_size.max(1))
}

/// Optional task overrides; absence inherits the downloader configuration.
#[derive(Debug, Clone, Default)]
pub(crate) struct NetworkOverrides {
    pub all_proxy: Option<String>,
    pub http_proxy: Option<String>,
    pub https_proxy: Option<String>,
    pub ca_info: Option<PathBuf>,
    pub ca_path: Option<PathBuf>,
    pub client_cert: Option<PathBuf>,
    pub client_key: Option<PathBuf>,
    pub connect_timeout: Option<Duration>,
    pub idle_pool: Option<(usize, Duration)>,
}

#[derive(Debug, Clone)]
pub(crate) struct RetryConfig {
    /// Maximum additional retries per request/transfer scope (0 = no retries).
    pub max_retries: u32,
    /// Base delay for exponential backoff between retries.
    pub retry_base_delay: Duration,
    /// Maximum delay cap for exponential backoff.
    pub retry_max_delay: Duration,
    /// Optional total elapsed retry budget across retries for one request/transfer scope.
    pub max_retry_elapsed: Option<Duration>,
}

#[derive(Debug, Clone)]
pub(crate) struct SchedulingConfig {
    pub piece_size: u64,
    pub request_batch_size: u64,
    pub range_scheduling_mode: RangeSchedulingMode,
    pub dynamic_min_split_size: u64,
    pub dynamic_max_request_size: u64,
    pub min_split_size: u64,
    pub min_segment_size: u64,
}

#[derive(Debug, Clone)]
pub(crate) struct RecoveryConfig {
    pub slow_transfer_mode: SlowTransferMode,
    pub low_speed_limit: Option<u64>,
    pub low_speed_duration: Duration,
    pub slow_start_grace: Duration,
    pub slow_sample_window: Duration,
}

#[derive(Debug, Clone)]
pub(crate) struct StorageConfig {
    pub memory_budget: usize,
    pub file_allocation: FileAllocation,
    pub channel_buffer: usize,
    pub resume: bool,
    /// Optional checksum for post-download verification.
    pub checksum: Option<Checksum>,
    /// Interval for periodic control-file saves (default 5 s).
    pub control_save_interval: Duration,
    /// Persist a durable autosave every N autosave ticks with unsaved progress.
    pub autosave_sync_every: u32,
}

/// Specification for a download task.
#[derive(Debug, Clone)]
pub struct DownloadSpec {
    pub(crate) storage: StorageConfig,
    pub(crate) recovery: RecoveryConfig,
    pub(crate) scheduling: SchedulingConfig,
    pub(crate) retry: RetryConfig,
    pub(crate) url: String,
    pub(crate) output_path: Option<PathBuf>,
    pub(crate) output_dir: Option<PathBuf>,
    pub(crate) headers: HashMap<String, String>,
    pub(crate) max_connections: u32,
    pub(crate) network_overrides: NetworkOverrides,
    pub(crate) read_timeout: Duration,
    pub(crate) request_headers_timeout: Option<Duration>,
    /// Maximum download speed in bytes/sec. 0 = unlimited.
    pub(crate) max_download_speed: u64,
}

impl DownloadSpec {
    pub(crate) fn resolve_network_config(
        &self,
        base_config: &crate::network::ClientNetworkConfig,
    ) -> crate::network::ClientNetworkConfig {
        let mut requested = base_config.clone();

        if self.has_connect_timeout_override() {
            requested.connect_timeout = self.get_connect_timeout();
        }

        if self.has_pool_override() {
            requested.pool_max_idle_per_host = self.get_pool_max_idle_per_host();
            if requested.pool_max_idle_per_host > 0 {
                requested.pool_idle_timeout = self.get_pool_idle_timeout();
            }
        }

        if self.has_proxy_override() {
            requested.all_proxy = None;
            requested.http_proxy = None;
            requested.https_proxy = None;

            if let Some(proxy) = self.get_all_proxy() {
                requested.all_proxy = Some(proxy.to_owned());
            }
            if let Some(proxy) = self.get_http_proxy() {
                requested.http_proxy = Some(proxy.to_owned());
            }
            if let Some(proxy) = self.get_https_proxy() {
                requested.https_proxy = Some(proxy.to_owned());
            }
        }

        if let Some(path) = self.get_ca_info() {
            requested.ca_info = Some(path.to_owned());
        }
        if let Some(path) = self.get_ca_path() {
            requested.ca_path = Some(path.to_owned());
        }
        if let Some(path) = self.get_client_cert() {
            requested.client_cert = Some(path.to_owned());
        }
        if let Some(path) = self.get_client_key() {
            requested.client_key = Some(path.to_owned());
        }

        requested
    }

    /// Create a new download specification for the given URL.
    ///
    /// All other fields are populated with sensible defaults:
    /// 4 connections, 1 MiB pieces, dynamic range scheduling with a 1 MiB
    /// minimum and 64 MiB maximum request, a 4-connection per-host idle pool,
    /// 64 MiB memory budget, resume enabled, etc. The 4 MiB request batch is
    /// retained for the explicit fixed compatibility mode.
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            storage: StorageConfig {
                memory_budget: 64 * 1024 * 1024, // 64 MiB
                file_allocation: FileAllocation::default(),
                channel_buffer: 64,
                resume: true,
                checksum: None,
                control_save_interval: Duration::from_secs(5),
                autosave_sync_every: 2,
            },
            recovery: RecoveryConfig {
                slow_transfer_mode: SlowTransferMode::default(),
                low_speed_limit: None,
                low_speed_duration: Duration::from_secs(15),
                slow_start_grace: Duration::from_secs(5),
                slow_sample_window: Duration::from_secs(5),
            },
            scheduling: SchedulingConfig {
                piece_size: 1024 * 1024, // 1 MiB
                request_batch_size: DEFAULT_REQUEST_BATCH_SIZE,
                range_scheduling_mode: RangeSchedulingMode::default(),
                dynamic_min_split_size: DEFAULT_DYNAMIC_MIN_SPLIT_SIZE,
                dynamic_max_request_size: DEFAULT_DYNAMIC_MAX_REQUEST_SIZE,
                min_split_size: 10 * 1024 * 1024, // 10 MiB
                min_segment_size: 256 * 1024,     // 256 KiB
            },
            retry: RetryConfig {
                max_retries: 5,
                retry_base_delay: Duration::from_secs(1),
                retry_max_delay: Duration::from_secs(30),
                max_retry_elapsed: None,
            },
            url: url.into(),
            output_path: None,
            output_dir: None,
            headers: HashMap::new(),
            max_connections: 4,
            network_overrides: NetworkOverrides::default(),
            read_timeout: Duration::from_secs(60),
            request_headers_timeout: None,
            max_download_speed: 0,
        }
    }

    /// Returns the download URL.
    pub fn get_url(&self) -> &str {
        &self.url
    }

    /// Returns the explicit output file path, if set.
    pub fn get_output_path(&self) -> Option<&Path> {
        self.output_path.as_deref()
    }

    /// Returns the output directory (filename will be derived from the URL or
    /// `Content-Disposition` header).
    pub fn get_output_dir(&self) -> Option<&Path> {
        self.output_dir.as_deref()
    }

    /// Returns the custom HTTP headers that will be sent with every request.
    pub fn get_headers(&self) -> &HashMap<String, String> {
        &self.headers
    }

    /// Returns the maximum number of parallel connections.
    pub fn get_max_connections(&self) -> u32 {
        self.max_connections
    }

    /// Returns the TCP connect timeout.
    pub fn get_connect_timeout(&self) -> Duration {
        self.network_overrides
            .connect_timeout
            .unwrap_or(Duration::from_secs(30))
    }

    /// Returns the proxy applied to all HTTP/HTTPS requests, if set.
    pub fn get_all_proxy(&self) -> Option<&str> {
        self.network_overrides.all_proxy.as_deref()
    }

    /// Returns the maximum idle HTTP connections kept per host.
    pub fn get_pool_max_idle_per_host(&self) -> usize {
        self.network_overrides
            .idle_pool
            .map_or(DEFAULT_HTTP_IDLE_POOL_MAX_PER_HOST, |pool| pool.0)
    }

    /// Returns how long idle pooled HTTP connections are retained.
    pub fn get_pool_idle_timeout(&self) -> Duration {
        self.network_overrides
            .idle_pool
            .map_or(DEFAULT_HTTP_IDLE_POOL_TIMEOUT, |pool| pool.1)
    }

    /// Returns the proxy applied only to plain HTTP requests, if set.
    pub fn get_http_proxy(&self) -> Option<&str> {
        self.network_overrides.http_proxy.as_deref()
    }

    /// Returns the proxy applied only to HTTPS requests, if set.
    pub fn get_https_proxy(&self) -> Option<&str> {
        self.network_overrides.https_proxy.as_deref()
    }

    /// Returns the additional PEM trust bundle for the libcurl backend.
    pub fn get_ca_info(&self) -> Option<&Path> {
        self.network_overrides.ca_info.as_deref()
    }

    /// Returns the CA certificate directory for the libcurl backend.
    pub fn get_ca_path(&self) -> Option<&Path> {
        self.network_overrides.ca_path.as_deref()
    }

    /// Returns the client certificate used for mutual TLS, if configured.
    pub fn get_client_cert(&self) -> Option<&Path> {
        self.network_overrides.client_cert.as_deref()
    }

    /// Returns the client private key used for mutual TLS, if configured.
    pub fn get_client_key(&self) -> Option<&Path> {
        self.network_overrides.client_key.as_deref()
    }

    /// Returns the per-request read timeout.
    pub fn get_read_timeout(&self) -> Duration {
        self.read_timeout
    }

    /// Optional deadline for each request through receipt of response headers.
    /// Includes connection establishment; each redirected hop gets its own deadline.
    /// `None` preserves the header deadline inherited from `read_timeout`.
    pub fn get_request_headers_timeout(&self) -> Option<Duration> {
        self.request_headers_timeout
    }

    /// Set the per-request response-headers deadline, including connection setup.
    /// Each redirect hop and retry starts a fresh deadline. This does not change
    /// body-read timeouts or retry/Retry-After budgets. The connector timeout may
    /// expire earlier. Must be positive and representable as a monotonic-clock deadline.
    pub fn request_headers_timeout(mut self, timeout: Duration) -> Self {
        self.request_headers_timeout = Some(timeout);
        self
    }

    pub(crate) fn has_connect_timeout_override(&self) -> bool {
        self.network_overrides.connect_timeout.is_some()
    }

    pub(crate) fn has_proxy_override(&self) -> bool {
        self.network_overrides.all_proxy.is_some()
            || self.network_overrides.http_proxy.is_some()
            || self.network_overrides.https_proxy.is_some()
    }

    pub(crate) fn has_pool_override(&self) -> bool {
        self.network_overrides.idle_pool.is_some()
    }

    /// Returns the memory budget (in bytes) for the write-back cache.
    pub fn get_memory_budget(&self) -> usize {
        self.storage.memory_budget
    }

    /// Returns the file allocation strategy.
    pub fn get_file_allocation(&self) -> FileAllocation {
        self.storage.file_allocation
    }

    /// Returns the internal channel buffer size.
    pub fn get_channel_buffer(&self) -> usize {
        self.storage.channel_buffer
    }

    /// Returns whether resume is enabled.
    pub fn get_resume(&self) -> bool {
        self.storage.resume
    }

    /// Returns the piece size in bytes used for multi-connection splitting.
    pub fn get_piece_size(&self) -> u64 {
        self.scheduling.piece_size
    }

    /// Returns the target byte limit for contiguous multi-piece HTTP requests.
    /// The default is 4 MiB; zero disables batching.
    pub fn get_request_batch_size(&self) -> u64 {
        self.scheduling.request_batch_size
    }

    /// Configure contiguous multi-piece HTTP requests, bounded by this byte limit
    /// and an internal lease-count limit. Piece/checkpoint granularity is unchanged.
    /// A value smaller than a piece does not split that piece; zero disables batching.
    /// Applies only to known-size multi-connection Range downloads.
    pub fn request_batch_size(mut self, bytes: u64) -> Self {
        self.scheduling.request_batch_size = bytes;
        self
    }

    /// Returns the request range scheduling strategy.
    pub fn get_range_scheduling_mode(&self) -> RangeSchedulingMode {
        self.scheduling.range_scheduling_mode
    }

    /// Select fixed or dynamic request range scheduling (default: dynamic).
    pub fn range_scheduling_mode(mut self, mode: RangeSchedulingMode) -> Self {
        self.scheduling.range_scheduling_mode = mode;
        self
    }

    /// Returns the configured minimum dynamic split length in bytes.
    /// The scheduler rounds it up to a piece boundary before splitting.
    pub fn get_dynamic_min_split_size(&self) -> u64 {
        self.scheduling.dynamic_min_split_size
    }

    /// Returns the piece-aligned minimum used by the dynamic scheduler.
    pub fn get_effective_dynamic_min_split_size(&self) -> u64 {
        effective_dynamic_min_split_size(
            self.scheduling.piece_size,
            self.scheduling.dynamic_min_split_size,
        )
    }

    /// Set the minimum length of both sides of a dynamic split.
    pub fn dynamic_min_split_size(mut self, bytes: u64) -> Self {
        self.scheduling.dynamic_min_split_size = bytes;
        self
    }

    /// Returns the maximum dynamic HTTP request length in bytes.
    pub fn get_dynamic_max_request_size(&self) -> u64 {
        self.scheduling.dynamic_max_request_size
    }

    /// Returns the effective dynamic request cap, including the one-piece floor.
    pub fn get_effective_dynamic_max_request_size(&self) -> u64 {
        effective_dynamic_max_request_size(
            self.scheduling.piece_size,
            self.scheduling.dynamic_max_request_size,
        )
    }

    /// Set the maximum dynamic HTTP request length in bytes.
    /// A value below one piece still permits one complete piece.
    pub fn dynamic_max_request_size(mut self, bytes: u64) -> Self {
        self.scheduling.dynamic_max_request_size = bytes;
        self
    }

    /// Returns the minimum file size required before the download is split
    /// across multiple connections.
    pub fn get_min_split_size(&self) -> u64 {
        self.scheduling.min_split_size
    }

    /// Returns the minimum sub-segment size used by dynamic multi-worker splitting.
    pub fn get_min_segment_size(&self) -> u64 {
        self.scheduling.min_segment_size
    }

    /// Returns the maximum number of additional retries per request/transfer scope.
    pub fn get_max_retries(&self) -> u32 {
        self.retry.max_retries
    }

    /// Returns the base delay for exponential backoff between retries.
    pub fn get_retry_base_delay(&self) -> Duration {
        self.retry.retry_base_delay
    }

    /// Returns the maximum delay cap for exponential backoff.
    pub fn get_retry_max_delay(&self) -> Duration {
        self.retry.retry_max_delay
    }

    /// Returns the optional total elapsed retry budget.
    pub fn get_max_retry_elapsed(&self) -> Option<Duration> {
        self.retry.max_retry_elapsed
    }

    /// Returns the maximum download speed in bytes/sec (0 = unlimited).
    pub fn get_max_download_speed(&self) -> u64 {
        self.max_download_speed
    }

    /// Returns the checksum used for post-download verification, if set.
    pub fn get_checksum(&self) -> Option<&Checksum> {
        self.storage.checksum.as_ref()
    }

    /// Returns the interval for periodic control-file saves.
    pub fn get_control_save_interval(&self) -> Duration {
        self.storage.control_save_interval
    }

    /// Returns how many autosave ticks are coalesced into one durable save.
    pub fn get_autosave_sync_every(&self) -> u32 {
        self.storage.autosave_sync_every
    }

    /// Set the explicit output file path.
    pub fn output_path(mut self, output_path: impl Into<PathBuf>) -> Self {
        self.output_path = Some(output_path.into());
        self
    }

    /// Set the output directory. The filename will be derived automatically
    /// from the URL or `Content-Disposition` response header.
    pub fn output_dir(mut self, output_dir: impl Into<PathBuf>) -> Self {
        self.output_dir = Some(output_dir.into());
        self
    }

    /// Set custom HTTP headers to include in every request.
    pub fn headers(mut self, headers: HashMap<String, String>) -> Self {
        self.headers = headers;
        self
    }

    /// Set the maximum number of parallel connections (default: 4).
    pub fn max_connections(mut self, max_connections: u32) -> Self {
        self.max_connections = max_connections;
        self
    }

    /// Set the TCP connect timeout (default: 30 s).
    pub fn connect_timeout(mut self, connect_timeout: Duration) -> Self {
        self.network_overrides.connect_timeout = Some(connect_timeout);
        self
    }

    /// Configure the HTTP idle pool for this download only.
    ///
    /// `max_idle_per_host` is the number of connections the client may keep
    /// cached per origin. The libcurl backend maps it to the connection cache
    /// size, so a download that opens more concurrent connections to one origin
    /// than this value closes them as they go idle: keep it at or above
    /// `max_connections` to let concurrent transfers reuse their connections.
    pub fn http_idle_pool(mut self, max_idle_per_host: usize, idle_timeout: Duration) -> Self {
        self.network_overrides.idle_pool = Some((max_idle_per_host, idle_timeout));
        self
    }

    /// Disable HTTP idle connection reuse for this download even if the downloader enables it.
    pub fn disable_http_idle_pool(mut self) -> Self {
        self.network_overrides.idle_pool = Some((0, self.get_pool_idle_timeout()));
        self
    }

    /// Set a proxy applied to all HTTP/HTTPS requests for this download only.
    pub fn all_proxy(mut self, proxy: impl Into<String>) -> Self {
        self.network_overrides.all_proxy = Some(proxy.into());
        self
    }

    /// Set a proxy applied only to plain HTTP requests for this download.
    pub fn http_proxy(mut self, proxy: impl Into<String>) -> Self {
        self.network_overrides.http_proxy = Some(proxy.into());
        self
    }

    /// Set a proxy applied only to HTTPS requests for this download.
    pub fn https_proxy(mut self, proxy: impl Into<String>) -> Self {
        self.network_overrides.https_proxy = Some(proxy.into());
        self
    }

    /// Add a PEM trust bundle for libcurl without disabling certificate or
    /// hostname verification.
    pub fn ca_info(mut self, path: impl Into<PathBuf>) -> Self {
        self.network_overrides.ca_info = Some(path.into());
        self
    }

    /// Use a directory of hashed CA certificates for libcurl.
    pub fn ca_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.network_overrides.ca_path = Some(path.into());
        self
    }

    /// Configure the client certificate used for mutual TLS.
    pub fn client_cert(mut self, path: impl Into<PathBuf>) -> Self {
        self.network_overrides.client_cert = Some(path.into());
        self
    }

    /// Configure the private key used for mutual TLS.
    pub fn client_key(mut self, path: impl Into<PathBuf>) -> Self {
        self.network_overrides.client_key = Some(path.into());
        self
    }

    /// Set the per-request read timeout (default: 60 s).
    pub fn read_timeout(mut self, read_timeout: Duration) -> Self {
        self.read_timeout = read_timeout;
        self
    }

    /// Automatic performance recovery mode (default: adaptive).
    /// Small trailing ranges with idle capacity and healthy reference speeds
    /// can use a shorter observation period; `Disabled` suppresses both paths.
    pub fn slow_transfer_mode(mut self, value: SlowTransferMode) -> Self {
        self.recovery.slow_transfer_mode = value;
        self
    }

    /// Returns the configured slow transfer mode.
    pub fn get_slow_transfer_mode(&self) -> SlowTransferMode {
        self.recovery.slow_transfer_mode
    }

    /// Optional absolute minimum reading speed in bytes/second; must be positive.
    pub fn low_speed_limit(mut self, value: u64) -> Self {
        self.recovery.low_speed_limit = Some(value);
        self
    }

    /// Returns the configured low speed limit.
    pub fn get_low_speed_limit(&self) -> Option<u64> {
        self.recovery.low_speed_limit
    }

    /// Ordinary continuous low-speed duration (default: 15 seconds).
    /// Eligible small trailing ranges use at most 2 seconds, with a healthy baseline.
    pub fn low_speed_duration(mut self, value: Duration) -> Self {
        self.recovery.low_speed_duration = value;
        self
    }

    /// Returns the configured low speed duration.
    pub fn get_low_speed_duration(&self) -> Duration {
        self.recovery.low_speed_duration
    }

    /// Initial reading grace period (default: 5 seconds).
    /// Eligible small trailing ranges use at most 1 second.
    pub fn slow_start_grace(mut self, value: Duration) -> Self {
        self.recovery.slow_start_grace = value;
        self
    }

    /// Returns the configured slow start grace.
    pub fn get_slow_start_grace(&self) -> Duration {
        self.recovery.slow_start_grace
    }

    /// Effective network reading sample window (default: 5 seconds).
    /// Eligible small trailing ranges also use a separate window of at most 1 second.
    pub fn slow_sample_window(mut self, value: Duration) -> Self {
        self.recovery.slow_sample_window = value;
        self
    }

    /// Returns the configured slow sample window.
    pub fn get_slow_sample_window(&self) -> Duration {
        self.recovery.slow_sample_window
    }

    /// Set the memory budget in bytes for the write-back cache (default: 64 MiB).
    pub fn memory_budget(mut self, memory_budget: usize) -> Self {
        self.storage.memory_budget = memory_budget;
        self
    }

    /// Set the file allocation strategy (default: [`FileAllocation::Prealloc`]).
    pub fn file_allocation(mut self, file_allocation: FileAllocation) -> Self {
        self.storage.file_allocation = file_allocation;
        self
    }

    /// Set the internal channel buffer size (default: 64).
    pub fn channel_buffer(mut self, channel_buffer: usize) -> Self {
        self.storage.channel_buffer = channel_buffer;
        self
    }

    /// Enable or disable resume support (default: `true`).
    pub fn resume(mut self, resume: bool) -> Self {
        self.storage.resume = resume;
        self
    }

    /// Set the piece size in bytes for multi-connection splitting (default: 1 MiB).
    pub fn piece_size(mut self, piece_size: u64) -> Self {
        self.scheduling.piece_size = piece_size;
        self
    }

    /// Set the minimum file size before splitting across connections (default: 10 MiB).
    pub fn min_split_size(mut self, min_split_size: u64) -> Self {
        self.scheduling.min_split_size = min_split_size;
        self
    }

    /// Set the minimum sub-segment size for dynamic multi-worker splitting.
    pub fn min_segment_size(mut self, min_segment_size: u64) -> Self {
        self.scheduling.min_segment_size = min_segment_size;
        self
    }

    /// Set the maximum additional retries per request/transfer scope (default: 5).
    pub fn max_retries(mut self, max_retries: u32) -> Self {
        self.retry.max_retries = max_retries;
        self
    }

    /// Set the base delay for exponential backoff (default: 1 s).
    pub fn retry_base_delay(mut self, retry_base_delay: Duration) -> Self {
        self.retry.retry_base_delay = retry_base_delay;
        self
    }

    /// Set the maximum delay cap for exponential backoff (default: 30 s).
    pub fn retry_max_delay(mut self, retry_max_delay: Duration) -> Self {
        self.retry.retry_max_delay = retry_max_delay;
        self
    }

    /// Set the total elapsed retry budget for a single request.
    pub fn max_retry_elapsed(mut self, max_retry_elapsed: Duration) -> Self {
        self.retry.max_retry_elapsed = Some(max_retry_elapsed);
        self
    }

    /// Configure the full retry policy in one call.
    pub fn retry_policy(
        mut self,
        max_retries: u32,
        retry_base_delay: Duration,
        retry_max_delay: Duration,
    ) -> Self {
        self.retry.max_retries = max_retries;
        self.retry.retry_base_delay = retry_base_delay;
        self.retry.retry_max_delay = retry_max_delay;
        self
    }

    /// Set the maximum download speed in bytes/sec (default: 0 = unlimited).
    pub fn max_download_speed(mut self, max_download_speed: u64) -> Self {
        self.max_download_speed = max_download_speed;
        self
    }

    /// Set the checksum for post-download integrity verification.
    pub fn checksum(mut self, checksum: Checksum) -> Self {
        self.storage.checksum = Some(checksum);
        self
    }

    /// Set the interval for periodic control-file saves (default: 5 s).
    pub fn control_save_interval(mut self, interval: Duration) -> Self {
        self.storage.control_save_interval = interval;
        self
    }

    /// Save a durable autosave every N autosave ticks that have unsaved progress.
    pub fn autosave_sync_every(mut self, autosave_sync_every: u32) -> Self {
        self.storage.autosave_sync_every = autosave_sync_every;
        self
    }

    /// Validate the configuration and return an error if any value is out of range.
    pub fn validate(&self) -> Result<(), DownloadError> {
        if let Some(limit) = self.recovery.low_speed_limit {
            require_nonzero("low_speed_limit", limit.into())?;
        }
        for (name, value) in [
            ("low_speed_duration", self.recovery.low_speed_duration),
            ("slow_start_grace", self.recovery.slow_start_grace),
            ("slow_sample_window", self.recovery.slow_sample_window),
        ] {
            if value.is_zero() || value > Duration::from_secs(86400) {
                return Err(DownloadError::InvalidConfig(format!(
                    "{name} must be > 0 and <= 86400 seconds"
                )));
            }
        }
        if self.request_headers_timeout.is_some_and(|timeout| {
            timeout.is_zero() || std::time::Instant::now().checked_add(timeout).is_none()
        }) {
            return Err(DownloadError::InvalidConfig(
                "request_headers_timeout must be positive and representable as a deadline".into(),
            ));
        }
        if self.url.trim().is_empty() {
            return Err(DownloadError::InvalidConfig("url cannot be empty".into()));
        }
        for (label, value) in [
            ("all_proxy", self.network_overrides.all_proxy.as_deref()),
            ("http_proxy", self.network_overrides.http_proxy.as_deref()),
            ("https_proxy", self.network_overrides.https_proxy.as_deref()),
        ] {
            if let Some(value) = value {
                if value.trim().is_empty() {
                    return Err(DownloadError::InvalidConfig(format!(
                        "{label} cannot be empty"
                    )));
                }
            }
        }
        require_nonzero("max_connections", self.max_connections as u128)?;
        require_nonzero("memory_budget", self.storage.memory_budget as u128)?;
        require_nonzero("channel_buffer", self.storage.channel_buffer as u128)?;
        require_nonzero("piece_size", self.scheduling.piece_size as u128)?;
        require_nonzero("min_split_size", self.scheduling.min_split_size as u128)?;
        require_nonzero("min_segment_size", self.scheduling.min_segment_size as u128)?;
        require_nonzero(
            "dynamic_min_split_size",
            self.scheduling.dynamic_min_split_size as u128,
        )?;
        require_nonzero(
            "dynamic_max_request_size",
            self.scheduling.dynamic_max_request_size as u128,
        )?;
        require_nonzero(
            "autosave_sync_every",
            self.storage.autosave_sync_every as u128,
        )?;
        if self.retry.retry_base_delay > self.retry.retry_max_delay {
            return Err(DownloadError::InvalidConfig(
                "retry_base_delay cannot exceed retry_max_delay".into(),
            ));
        }
        if let Some(ref checksum) = self.storage.checksum {
            let value = match checksum {
                Checksum::Sha256(v)
                | Checksum::Sha1(v)
                | Checksum::Md5(v)
                | Checksum::Sha512(v) => v,
            };
            if value.trim().is_empty() {
                return Err(DownloadError::InvalidConfig(
                    "checksum value cannot be empty".into(),
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn network_overrides_preserve_explicit_defaults_and_pool_setter_order() {
        let base = crate::network::ClientNetworkConfig {
            connect_timeout: Duration::from_secs(7),
            pool_idle_timeout: Duration::from_secs(9),
            ..Default::default()
        };
        let spec = DownloadSpec::new("https://example.com");
        assert_eq!(
            spec.resolve_network_config(&base).connect_timeout,
            Duration::from_secs(7)
        );
        let explicit = spec.clone().connect_timeout(Duration::from_secs(30));
        assert_eq!(
            explicit.resolve_network_config(&base).connect_timeout,
            Duration::from_secs(30)
        );
        let disabled = spec
            .clone()
            .http_idle_pool(2, Duration::from_secs(3))
            .disable_http_idle_pool();
        assert_eq!(disabled.get_pool_idle_timeout(), Duration::from_secs(3));
        let resolved = disabled.resolve_network_config(&base);
        assert_eq!(resolved.pool_max_idle_per_host, 0);
        assert_eq!(resolved.pool_idle_timeout, Duration::from_secs(9));
        let enabled = disabled.http_idle_pool(5, Duration::from_secs(11));
        assert_eq!(
            enabled.resolve_network_config(&base).pool_idle_timeout,
            Duration::from_secs(11)
        );
    }

    #[test]
    fn request_headers_timeout_defaults_and_validation() {
        let spec = super::DownloadSpec::new("https://example.com/file");
        assert_eq!(spec.get_request_headers_timeout(), None);
        let spec = spec.request_headers_timeout(std::time::Duration::from_secs(2));
        assert_eq!(
            spec.get_request_headers_timeout(),
            Some(std::time::Duration::from_secs(2))
        );
        assert!(spec.validate().is_ok());
        assert!(spec
            .clone()
            .request_headers_timeout(std::time::Duration::from_secs(86401))
            .validate()
            .is_ok());
        for timeout in [std::time::Duration::ZERO, std::time::Duration::MAX] {
            assert!(matches!(
                spec.clone().request_headers_timeout(timeout).validate(),
                Err(super::DownloadError::InvalidConfig(_))
            ));
        }
    }

    use super::*;

    #[test]
    fn test_download_spec_defaults() {
        let spec = DownloadSpec::new("https://example.com/file");
        assert_eq!(spec.get_request_batch_size(), DEFAULT_REQUEST_BATCH_SIZE);
        assert_eq!(
            spec.get_range_scheduling_mode(),
            RangeSchedulingMode::Dynamic
        );
        assert_eq!(
            spec.get_dynamic_min_split_size(),
            DEFAULT_DYNAMIC_MIN_SPLIT_SIZE
        );
        assert_eq!(
            spec.get_dynamic_max_request_size(),
            DEFAULT_DYNAMIC_MAX_REQUEST_SIZE
        );
        assert_eq!(
            spec.clone()
                .request_batch_size(4 * 1024 * 1024)
                .get_request_batch_size(),
            4 * 1024 * 1024
        );
        assert_eq!(spec.url, "https://example.com/file");
        assert_eq!(spec.output_path, None);
        assert_eq!(spec.output_dir, None);
        assert_eq!(spec.max_connections, 4);
        assert_eq!(spec.get_connect_timeout(), Duration::from_secs(30));
        assert!(!spec.has_connect_timeout_override());
        assert_eq!(
            spec.get_pool_max_idle_per_host(),
            DEFAULT_HTTP_IDLE_POOL_MAX_PER_HOST
        );
        assert_eq!(spec.get_pool_idle_timeout(), DEFAULT_HTTP_IDLE_POOL_TIMEOUT);
        assert!(!spec.has_pool_override());
        assert_eq!(spec.network_overrides.all_proxy, None);
        assert_eq!(spec.network_overrides.http_proxy, None);
        assert_eq!(spec.network_overrides.https_proxy, None);
        assert_eq!(spec.network_overrides.ca_info, None);
        assert_eq!(spec.network_overrides.ca_path, None);
        assert_eq!(spec.network_overrides.client_cert, None);
        assert_eq!(spec.network_overrides.client_key, None);
        assert_eq!(spec.read_timeout, Duration::from_secs(60));
        assert_eq!(spec.storage.memory_budget, 64 * 1024 * 1024);
        assert_eq!(spec.storage.file_allocation, FileAllocation::Prealloc);
        assert_eq!(spec.storage.channel_buffer, 64);
        assert!(spec.storage.resume);
        assert_eq!(spec.scheduling.piece_size, 1024 * 1024);
        assert_eq!(spec.scheduling.min_split_size, 10 * 1024 * 1024);
        assert_eq!(spec.scheduling.min_segment_size, 256 * 1024);
        assert_eq!(spec.retry.max_retries, 5);
        assert_eq!(spec.retry.max_retry_elapsed, None);
        assert_eq!(spec.max_download_speed, 0);
        assert!(spec.storage.checksum.is_none());
        assert!(spec.headers.is_empty());
        assert_eq!(spec.storage.autosave_sync_every, 2);
    }

    #[test]
    fn test_download_spec_output_builders() {
        let spec = DownloadSpec::new("https://example.com/file")
            .output_dir("/tmp")
            .output_path("nested/file.bin");
        assert_eq!(spec.output_dir, Some(PathBuf::from("/tmp")));
        assert_eq!(spec.output_path, Some(PathBuf::from("nested/file.bin")));
    }

    #[test]
    fn test_download_spec_configuration_builders() {
        let mut headers = HashMap::new();
        headers.insert("Authorization".into(), "Bearer token".into());

        let spec = DownloadSpec::new("https://example.com/file")
            .headers(headers.clone())
            .max_connections(8)
            .connect_timeout(Duration::from_secs(10))
            .http_idle_pool(3, Duration::from_secs(15))
            .all_proxy("http://127.0.0.1:8080")
            .http_proxy("http://127.0.0.1:8081")
            .https_proxy("http://127.0.0.1:8443")
            .ca_info("/tmp/ca.pem")
            .ca_path("/tmp/certs")
            .client_cert("/tmp/client.pem")
            .client_key("/tmp/client.key")
            .read_timeout(Duration::from_secs(20))
            .memory_budget(1024)
            .file_allocation(FileAllocation::None)
            .channel_buffer(8)
            .resume(false)
            .piece_size(2048)
            .range_scheduling_mode(RangeSchedulingMode::Dynamic)
            .dynamic_min_split_size(4096)
            .dynamic_max_request_size(8192)
            .min_split_size(4096)
            .min_segment_size(1024)
            .retry_policy(7, Duration::from_millis(10), Duration::from_millis(50))
            .max_retry_elapsed(Duration::from_secs(3))
            .max_download_speed(12345)
            .checksum(Checksum::Sha256("abc123".into()))
            .autosave_sync_every(4);

        assert_eq!(spec.headers, headers);
        assert_eq!(spec.max_connections, 8);
        assert_eq!(spec.get_connect_timeout(), Duration::from_secs(10));
        assert!(spec.has_connect_timeout_override());
        assert_eq!(spec.get_pool_max_idle_per_host(), 3);
        assert_eq!(spec.get_pool_idle_timeout(), Duration::from_secs(15));
        assert!(spec.has_pool_override());
        assert_eq!(
            spec.network_overrides.all_proxy.as_deref(),
            Some("http://127.0.0.1:8080")
        );
        assert_eq!(
            spec.network_overrides.http_proxy.as_deref(),
            Some("http://127.0.0.1:8081")
        );
        assert_eq!(
            spec.network_overrides.https_proxy.as_deref(),
            Some("http://127.0.0.1:8443")
        );
        assert_eq!(spec.get_ca_info(), Some(Path::new("/tmp/ca.pem")));
        assert_eq!(spec.get_ca_path(), Some(Path::new("/tmp/certs")));
        assert_eq!(spec.get_client_cert(), Some(Path::new("/tmp/client.pem")));
        assert_eq!(spec.get_client_key(), Some(Path::new("/tmp/client.key")));
        assert_eq!(spec.read_timeout, Duration::from_secs(20));
        assert_eq!(spec.storage.memory_budget, 1024);
        assert_eq!(spec.storage.file_allocation, FileAllocation::None);
        assert_eq!(spec.storage.channel_buffer, 8);
        assert!(!spec.storage.resume);
        assert_eq!(spec.scheduling.piece_size, 2048);
        assert_eq!(
            spec.scheduling.range_scheduling_mode,
            RangeSchedulingMode::Dynamic
        );
        assert_eq!(spec.scheduling.dynamic_min_split_size, 4096);
        assert_eq!(spec.scheduling.dynamic_max_request_size, 8192);
        assert_eq!(spec.scheduling.min_split_size, 4096);
        assert_eq!(spec.scheduling.min_segment_size, 1024);
        assert_eq!(spec.retry.max_retries, 7);
        assert_eq!(spec.retry.retry_base_delay, Duration::from_millis(10));
        assert_eq!(spec.retry.retry_max_delay, Duration::from_millis(50));
        assert_eq!(spec.retry.max_retry_elapsed, Some(Duration::from_secs(3)));
        assert_eq!(spec.max_download_speed, 12345);
        assert!(
            matches!(spec.storage.checksum, Some(Checksum::Sha256(ref value)) if value == "abc123")
        );
        assert_eq!(spec.storage.autosave_sync_every, 4);
    }

    #[test]
    fn test_download_spec_validate_rejects_invalid_values() {
        let err = DownloadSpec::new("https://example.com/file")
            .max_connections(0)
            .validate()
            .unwrap_err();
        assert!(err.to_string().contains("max_connections"));

        let err = DownloadSpec::new("https://example.com/file")
            .retry_policy(1, Duration::from_secs(5), Duration::from_secs(1))
            .validate()
            .unwrap_err();
        assert!(err.to_string().contains("retry_base_delay"));

        let err = DownloadSpec::new("https://example.com/file")
            .checksum(Checksum::Sha256("   ".into()))
            .validate()
            .unwrap_err();
        assert!(err.to_string().contains("checksum"));

        let err = DownloadSpec::new("https://example.com/file")
            .all_proxy("   ")
            .validate()
            .unwrap_err();
        assert!(err.to_string().contains("all_proxy"));

        let err = DownloadSpec::new("https://example.com/file")
            .autosave_sync_every(0)
            .validate()
            .unwrap_err();
        assert!(err.to_string().contains("autosave_sync_every"));

        let err = DownloadSpec::new("https://example.com/file")
            .min_segment_size(0)
            .validate()
            .unwrap_err();
        assert!(err.to_string().contains("min_segment_size"));
    }

    #[test]
    fn test_download_spec_validate_accepts_defaults() {
        DownloadSpec::new("https://example.com/file")
            .validate()
            .unwrap();
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

    #[test]
    fn test_log_level_default() {
        assert_eq!(LogLevel::default(), LogLevel::Off);
    }

    #[test]
    fn test_log_level_ordering() {
        assert!(LogLevel::Off < LogLevel::Error);
        assert!(LogLevel::Error < LogLevel::Warn);
        assert!(LogLevel::Warn < LogLevel::Info);
        assert!(LogLevel::Info < LogLevel::Debug);
        assert!(LogLevel::Debug < LogLevel::Trace);
    }

    #[test]
    fn test_log_level_display() {
        assert_eq!(LogLevel::Off.to_string(), "off");
        assert_eq!(LogLevel::Error.to_string(), "error");
        assert_eq!(LogLevel::Warn.to_string(), "warn");
        assert_eq!(LogLevel::Info.to_string(), "info");
        assert_eq!(LogLevel::Debug.to_string(), "debug");
        assert_eq!(LogLevel::Trace.to_string(), "trace");
    }

    #[test]
    fn test_log_level_from_str() {
        assert_eq!("off".parse::<LogLevel>().unwrap(), LogLevel::Off);
        assert_eq!("error".parse::<LogLevel>().unwrap(), LogLevel::Error);
        assert_eq!("warn".parse::<LogLevel>().unwrap(), LogLevel::Warn);
        assert_eq!("warning".parse::<LogLevel>().unwrap(), LogLevel::Warn);
        assert_eq!("info".parse::<LogLevel>().unwrap(), LogLevel::Info);
        assert_eq!("debug".parse::<LogLevel>().unwrap(), LogLevel::Debug);
        assert_eq!("trace".parse::<LogLevel>().unwrap(), LogLevel::Trace);
        assert_eq!("INFO".parse::<LogLevel>().unwrap(), LogLevel::Info);
        assert_eq!("Debug".parse::<LogLevel>().unwrap(), LogLevel::Debug);
        assert!("invalid".parse::<LogLevel>().is_err());
    }

    #[test]
    fn test_log_level_enabled() {
        let level = LogLevel::Info;
        assert!(level.enabled(tracing::Level::ERROR));
        assert!(level.enabled(tracing::Level::WARN));
        assert!(level.enabled(tracing::Level::INFO));
        assert!(!level.enabled(tracing::Level::DEBUG));
        assert!(!level.enabled(tracing::Level::TRACE));

        assert!(!LogLevel::Off.enabled(tracing::Level::ERROR));
        assert!(LogLevel::Trace.enabled(tracing::Level::TRACE));
    }

    #[test]
    fn test_log_level_tracing_filter() {
        assert_eq!(
            LogLevel::Off.to_tracing_level_filter(),
            tracing::level_filters::LevelFilter::OFF
        );
        assert_eq!(
            LogLevel::Error.to_tracing_level_filter(),
            tracing::level_filters::LevelFilter::ERROR
        );
        assert_eq!(
            LogLevel::Warn.to_tracing_level_filter(),
            tracing::level_filters::LevelFilter::WARN
        );
        assert_eq!(
            LogLevel::Info.to_tracing_level_filter(),
            tracing::level_filters::LevelFilter::INFO
        );
        assert_eq!(
            LogLevel::Debug.to_tracing_level_filter(),
            tracing::level_filters::LevelFilter::DEBUG
        );
        assert_eq!(
            LogLevel::Trace.to_tracing_level_filter(),
            tracing::level_filters::LevelFilter::TRACE
        );
    }

    #[test]
    fn test_download_spec_getter_methods() {
        let mut headers = HashMap::new();
        headers.insert("X-Custom".into(), "value".into());

        let spec = DownloadSpec::new("https://example.com/file")
            .output_path("/tmp/out.bin")
            .output_dir("/tmp")
            .headers(headers)
            .max_connections(8)
            .connect_timeout(Duration::from_secs(10))
            .http_idle_pool(4, Duration::from_secs(9))
            .all_proxy("http://127.0.0.1:8080")
            .http_proxy("http://127.0.0.1:8081")
            .https_proxy("http://127.0.0.1:8443")
            .read_timeout(Duration::from_secs(20))
            .memory_budget(2048)
            .file_allocation(FileAllocation::None)
            .channel_buffer(16)
            .resume(false)
            .piece_size(4096)
            .min_split_size(8192)
            .min_segment_size(2048)
            .max_retries(3)
            .retry_base_delay(Duration::from_millis(100))
            .retry_max_delay(Duration::from_secs(5))
            .max_retry_elapsed(Duration::from_secs(60))
            .max_download_speed(999)
            .checksum(Checksum::Sha256("abc".into()))
            .control_save_interval(Duration::from_secs(10))
            .autosave_sync_every(6);

        assert_eq!(spec.get_url(), "https://example.com/file");
        assert_eq!(spec.get_output_path().unwrap(), Path::new("/tmp/out.bin"));
        assert_eq!(spec.get_output_dir().unwrap(), Path::new("/tmp"));
        assert_eq!(spec.get_headers().len(), 1);
        assert_eq!(spec.get_max_connections(), 8);
        assert_eq!(spec.get_connect_timeout(), Duration::from_secs(10));
        assert_eq!(spec.get_pool_max_idle_per_host(), 4);
        assert_eq!(spec.get_pool_idle_timeout(), Duration::from_secs(9));
        assert_eq!(spec.get_all_proxy(), Some("http://127.0.0.1:8080"));
        assert_eq!(spec.get_http_proxy(), Some("http://127.0.0.1:8081"));
        assert_eq!(spec.get_https_proxy(), Some("http://127.0.0.1:8443"));
        assert_eq!(spec.get_read_timeout(), Duration::from_secs(20));
        assert_eq!(spec.get_memory_budget(), 2048);
        assert_eq!(spec.get_file_allocation(), FileAllocation::None);
        assert_eq!(spec.get_channel_buffer(), 16);
        assert!(!spec.get_resume());
        assert_eq!(spec.get_piece_size(), 4096);
        assert_eq!(spec.get_min_split_size(), 8192);
        assert_eq!(spec.get_min_segment_size(), 2048);
        assert_eq!(spec.get_max_retries(), 3);
        assert_eq!(spec.get_retry_base_delay(), Duration::from_millis(100));
        assert_eq!(spec.get_retry_max_delay(), Duration::from_secs(5));
        assert_eq!(spec.get_max_retry_elapsed(), Some(Duration::from_secs(60)));
        assert_eq!(spec.get_max_download_speed(), 999);
        assert!(spec.get_checksum().is_some());
        assert_eq!(spec.get_control_save_interval(), Duration::from_secs(10));
        assert_eq!(spec.get_autosave_sync_every(), 6);
    }

    #[test]
    fn test_download_spec_getter_defaults_none() {
        let spec = DownloadSpec::new("https://example.com");
        assert!(spec.get_output_path().is_none());
        assert!(spec.get_output_dir().is_none());
        assert!(spec.get_all_proxy().is_none());
        assert!(spec.get_http_proxy().is_none());
        assert!(spec.get_https_proxy().is_none());
        assert!(spec.get_max_retry_elapsed().is_none());
        assert!(spec.get_checksum().is_none());
    }

    #[test]
    fn test_validate_empty_url() {
        let err = DownloadSpec::new("   ").validate().unwrap_err();
        assert!(err.to_string().contains("url"));
    }

    #[test]
    fn test_validate_zero_memory_budget() {
        let err = DownloadSpec::new("https://x.com")
            .memory_budget(0)
            .validate()
            .unwrap_err();
        assert!(err.to_string().contains("memory_budget"));
    }

    #[test]
    fn test_validate_zero_channel_buffer() {
        let err = DownloadSpec::new("https://x.com")
            .channel_buffer(0)
            .validate()
            .unwrap_err();
        assert!(err.to_string().contains("channel_buffer"));
    }

    #[test]
    fn test_validate_zero_piece_size() {
        let err = DownloadSpec::new("https://x.com")
            .piece_size(0)
            .validate()
            .unwrap_err();
        assert!(err.to_string().contains("piece_size"));
    }

    #[test]
    fn test_validate_zero_min_split_size() {
        let err = DownloadSpec::new("https://x.com")
            .min_split_size(0)
            .validate()
            .unwrap_err();
        assert!(err.to_string().contains("min_split_size"));
    }

    #[test]
    fn test_validate_zero_min_segment_size() {
        let err = DownloadSpec::new("https://x.com")
            .min_segment_size(0)
            .validate()
            .unwrap_err();
        assert!(err.to_string().contains("min_segment_size"));
    }

    #[test]
    fn test_validate_zero_dynamic_range_limits() {
        let err = DownloadSpec::new("https://x.com")
            .dynamic_min_split_size(0)
            .validate()
            .unwrap_err();
        assert!(err.to_string().contains("dynamic_min_split_size"));

        let err = DownloadSpec::new("https://x.com")
            .dynamic_max_request_size(0)
            .validate()
            .unwrap_err();
        assert!(err.to_string().contains("dynamic_max_request_size"));
    }

    #[test]
    fn test_range_scheduling_mode_display_and_parse() {
        assert_eq!(RangeSchedulingMode::Fixed.to_string(), "fixed");
        assert_eq!(RangeSchedulingMode::Dynamic.to_string(), "dynamic");
        assert_eq!(
            "FIXED".parse::<RangeSchedulingMode>().unwrap(),
            RangeSchedulingMode::Fixed
        );
        assert_eq!(
            "dynamic".parse::<RangeSchedulingMode>().unwrap(),
            RangeSchedulingMode::Dynamic
        );
        assert!("automatic".parse::<RangeSchedulingMode>().is_err());
    }

    #[test]
    fn test_effective_dynamic_limits_follow_piece_boundaries() {
        let spec = DownloadSpec::new("https://example.com/file")
            .piece_size(2_048)
            .dynamic_min_split_size(4_097)
            .dynamic_max_request_size(1_024);
        assert_eq!(spec.get_effective_dynamic_min_split_size(), 6_144);
        assert_eq!(spec.get_effective_dynamic_max_request_size(), 2_048);
    }

    #[test]
    fn test_checksum_variants_debug() {
        let sha1 = Checksum::Sha1("aaa".into());
        let md5 = Checksum::Md5("bbb".into());
        let sha512 = Checksum::Sha512("ccc".into());
        assert!(format!("{sha1:?}").contains("Sha1"));
        assert!(format!("{md5:?}").contains("Md5"));
        assert!(format!("{sha512:?}").contains("Sha512"));
    }
}
