use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::error::DownloadError;

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

/// Specification for a download task.
#[derive(Debug, Clone)]
pub struct DownloadSpec {
    pub(crate) url: String,
    pub(crate) output_path: Option<PathBuf>,
    pub(crate) output_dir: Option<PathBuf>,
    pub(crate) headers: HashMap<String, String>,
    pub(crate) max_connections: u32,
    pub(crate) connect_timeout: Duration,
    pub(crate) read_timeout: Duration,
    pub(crate) memory_budget: usize,
    pub(crate) file_allocation: FileAllocation,
    pub(crate) channel_buffer: usize,
    pub(crate) resume: bool,
    pub(crate) piece_size: u64,
    pub(crate) min_split_size: u64,
    /// Maximum retry attempts per segment (0 = no retries).
    pub(crate) max_retries: u32,
    /// Base delay for exponential backoff between retries.
    pub(crate) retry_base_delay: Duration,
    /// Maximum delay cap for exponential backoff.
    pub(crate) retry_max_delay: Duration,
    /// Optional total elapsed retry budget across retries for a single request.
    pub(crate) max_retry_elapsed: Option<Duration>,
    /// Maximum download speed in bytes/sec. 0 = unlimited.
    pub(crate) max_download_speed: u64,
    /// Optional checksum for post-download verification.
    pub(crate) checksum: Option<Checksum>,
    /// Interval for periodic control-file saves (default 5 s).
    pub(crate) control_save_interval: Duration,
}

impl DownloadSpec {
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            output_path: None,
            output_dir: None,
            headers: HashMap::new(),
            max_connections: 4,
            connect_timeout: Duration::from_secs(30),
            read_timeout: Duration::from_secs(60),
            memory_budget: 64 * 1024 * 1024, // 64 MiB
            file_allocation: FileAllocation::default(),
            channel_buffer: 64,
            resume: true,
            piece_size: 1024 * 1024,          // 1 MiB
            min_split_size: 10 * 1024 * 1024, // 10 MiB
            max_retries: 5,
            retry_base_delay: Duration::from_secs(1),
            retry_max_delay: Duration::from_secs(30),
            max_retry_elapsed: None,
            max_download_speed: 0,
            checksum: None,
            control_save_interval: Duration::from_secs(5),
        }
    }

    pub fn get_url(&self) -> &str {
        &self.url
    }

    pub fn get_output_path(&self) -> Option<&Path> {
        self.output_path.as_deref()
    }

    pub fn get_output_dir(&self) -> Option<&Path> {
        self.output_dir.as_deref()
    }

    pub fn get_headers(&self) -> &HashMap<String, String> {
        &self.headers
    }

    pub fn get_max_connections(&self) -> u32 {
        self.max_connections
    }

    pub fn get_connect_timeout(&self) -> Duration {
        self.connect_timeout
    }

    pub fn get_read_timeout(&self) -> Duration {
        self.read_timeout
    }

    pub fn get_memory_budget(&self) -> usize {
        self.memory_budget
    }

    pub fn get_file_allocation(&self) -> FileAllocation {
        self.file_allocation
    }

    pub fn get_channel_buffer(&self) -> usize {
        self.channel_buffer
    }

    pub fn get_resume(&self) -> bool {
        self.resume
    }

    pub fn get_piece_size(&self) -> u64 {
        self.piece_size
    }

    pub fn get_min_split_size(&self) -> u64 {
        self.min_split_size
    }

    pub fn get_max_retries(&self) -> u32 {
        self.max_retries
    }

    pub fn get_retry_base_delay(&self) -> Duration {
        self.retry_base_delay
    }

    pub fn get_retry_max_delay(&self) -> Duration {
        self.retry_max_delay
    }

    pub fn get_max_retry_elapsed(&self) -> Option<Duration> {
        self.max_retry_elapsed
    }

    pub fn get_max_download_speed(&self) -> u64 {
        self.max_download_speed
    }

    pub fn get_checksum(&self) -> Option<&Checksum> {
        self.checksum.as_ref()
    }

    pub fn get_control_save_interval(&self) -> Duration {
        self.control_save_interval
    }

    pub fn output_path(mut self, output_path: impl Into<PathBuf>) -> Self {
        self.output_path = Some(output_path.into());
        self
    }

    pub fn output_dir(mut self, output_dir: impl Into<PathBuf>) -> Self {
        self.output_dir = Some(output_dir.into());
        self
    }

    pub fn headers(mut self, headers: HashMap<String, String>) -> Self {
        self.headers = headers;
        self
    }

    pub fn max_connections(mut self, max_connections: u32) -> Self {
        self.max_connections = max_connections;
        self
    }

    pub fn connect_timeout(mut self, connect_timeout: Duration) -> Self {
        self.connect_timeout = connect_timeout;
        self
    }

    pub fn read_timeout(mut self, read_timeout: Duration) -> Self {
        self.read_timeout = read_timeout;
        self
    }

    pub fn memory_budget(mut self, memory_budget: usize) -> Self {
        self.memory_budget = memory_budget;
        self
    }

    pub fn file_allocation(mut self, file_allocation: FileAllocation) -> Self {
        self.file_allocation = file_allocation;
        self
    }

    pub fn channel_buffer(mut self, channel_buffer: usize) -> Self {
        self.channel_buffer = channel_buffer;
        self
    }

    pub fn resume(mut self, resume: bool) -> Self {
        self.resume = resume;
        self
    }

    pub fn piece_size(mut self, piece_size: u64) -> Self {
        self.piece_size = piece_size;
        self
    }

    pub fn min_split_size(mut self, min_split_size: u64) -> Self {
        self.min_split_size = min_split_size;
        self
    }

    pub fn max_retries(mut self, max_retries: u32) -> Self {
        self.max_retries = max_retries;
        self
    }

    pub fn retry_base_delay(mut self, retry_base_delay: Duration) -> Self {
        self.retry_base_delay = retry_base_delay;
        self
    }

    pub fn retry_max_delay(mut self, retry_max_delay: Duration) -> Self {
        self.retry_max_delay = retry_max_delay;
        self
    }

    pub fn max_retry_elapsed(mut self, max_retry_elapsed: Duration) -> Self {
        self.max_retry_elapsed = Some(max_retry_elapsed);
        self
    }

    pub fn retry_policy(
        mut self,
        max_retries: u32,
        retry_base_delay: Duration,
        retry_max_delay: Duration,
    ) -> Self {
        self.max_retries = max_retries;
        self.retry_base_delay = retry_base_delay;
        self.retry_max_delay = retry_max_delay;
        self
    }

    pub fn max_download_speed(mut self, max_download_speed: u64) -> Self {
        self.max_download_speed = max_download_speed;
        self
    }

    pub fn checksum(mut self, checksum: Checksum) -> Self {
        self.checksum = Some(checksum);
        self
    }

    pub fn control_save_interval(mut self, interval: Duration) -> Self {
        self.control_save_interval = interval;
        self
    }

    pub fn validate(&self) -> Result<(), DownloadError> {
        if self.url.trim().is_empty() {
            return Err(DownloadError::InvalidConfig("url cannot be empty".into()));
        }
        if self.max_connections == 0 {
            return Err(DownloadError::InvalidConfig(
                "max_connections must be >= 1".into(),
            ));
        }
        if self.memory_budget == 0 {
            return Err(DownloadError::InvalidConfig(
                "memory_budget must be >= 1".into(),
            ));
        }
        if self.channel_buffer == 0 {
            return Err(DownloadError::InvalidConfig(
                "channel_buffer must be >= 1".into(),
            ));
        }
        if self.piece_size == 0 {
            return Err(DownloadError::InvalidConfig("piece_size must be >= 1".into()));
        }
        if self.min_split_size == 0 {
            return Err(DownloadError::InvalidConfig(
                "min_split_size must be >= 1".into(),
            ));
        }
        if self.retry_base_delay > self.retry_max_delay {
            return Err(DownloadError::InvalidConfig(
                "retry_base_delay cannot exceed retry_max_delay".into(),
            ));
        }
        if let Some(ref checksum) = self.checksum {
            let value = match checksum {
                Checksum::Sha256(v) | Checksum::Sha1(v) | Checksum::Md5(v) | Checksum::Sha512(v) => v,
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
    use super::*;

    #[test]
    fn test_download_spec_defaults() {
        let spec = DownloadSpec::new("https://example.com/file");
        assert_eq!(spec.url, "https://example.com/file");
        assert_eq!(spec.output_path, None);
        assert_eq!(spec.output_dir, None);
        assert_eq!(spec.max_connections, 4);
        assert_eq!(spec.connect_timeout, Duration::from_secs(30));
        assert_eq!(spec.read_timeout, Duration::from_secs(60));
        assert_eq!(spec.memory_budget, 64 * 1024 * 1024);
        assert_eq!(spec.file_allocation, FileAllocation::Prealloc);
        assert_eq!(spec.channel_buffer, 64);
        assert!(spec.resume);
        assert_eq!(spec.piece_size, 1024 * 1024);
        assert_eq!(spec.min_split_size, 10 * 1024 * 1024);
        assert_eq!(spec.max_retries, 5);
        assert_eq!(spec.max_retry_elapsed, None);
        assert_eq!(spec.max_download_speed, 0);
        assert!(spec.checksum.is_none());
        assert!(spec.headers.is_empty());
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
            .read_timeout(Duration::from_secs(20))
            .memory_budget(1024)
            .file_allocation(FileAllocation::None)
            .channel_buffer(8)
            .resume(false)
            .piece_size(2048)
            .min_split_size(4096)
            .retry_policy(7, Duration::from_millis(10), Duration::from_millis(50))
            .max_retry_elapsed(Duration::from_secs(3))
            .max_download_speed(12345)
            .checksum(Checksum::Sha256("abc123".into()));

        assert_eq!(spec.headers, headers);
        assert_eq!(spec.max_connections, 8);
        assert_eq!(spec.connect_timeout, Duration::from_secs(10));
        assert_eq!(spec.read_timeout, Duration::from_secs(20));
        assert_eq!(spec.memory_budget, 1024);
        assert_eq!(spec.file_allocation, FileAllocation::None);
        assert_eq!(spec.channel_buffer, 8);
        assert!(!spec.resume);
        assert_eq!(spec.piece_size, 2048);
        assert_eq!(spec.min_split_size, 4096);
        assert_eq!(spec.max_retries, 7);
        assert_eq!(spec.retry_base_delay, Duration::from_millis(10));
        assert_eq!(spec.retry_max_delay, Duration::from_millis(50));
        assert_eq!(spec.max_retry_elapsed, Some(Duration::from_secs(3)));
        assert_eq!(spec.max_download_speed, 12345);
        assert!(matches!(spec.checksum, Some(Checksum::Sha256(ref value)) if value == "abc123"));
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
}
