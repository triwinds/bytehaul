use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

/// Log verbosity level for download tasks.
///
/// The default is `Off`, which means no log events are emitted by the library.
/// When set to a non-`Off` value, log events up to and including that level
/// will be produced via the `tracing` crate. A subscriber must be installed
/// by the application (or the Python binding) to actually see the output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LogLevel {
    /// No logging.
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

impl Default for LogLevel {
    fn default() -> Self {
        LogLevel::Off
    }
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileAllocation {
    /// No pre-allocation; file grows as data is written.
    None,
    /// Pre-allocate the full file size by writing zeros before downloading.
    Prealloc,
}

impl Default for FileAllocation {
    fn default() -> Self {
        FileAllocation::Prealloc
    }
}

/// Checksum algorithm for post-download verification.
#[derive(Debug, Clone)]
pub enum Checksum {
    /// SHA-256 digest (hex-encoded, lowercase).
    Sha256(String),
}

/// Specification for a download task.
#[derive(Debug, Clone)]
pub struct DownloadSpec {
    pub url: String,
    pub output_path: PathBuf,
    pub headers: HashMap<String, String>,
    pub max_connections: u32,
    pub connect_timeout: Duration,
    pub read_timeout: Duration,
    pub memory_budget: usize,
    pub file_allocation: FileAllocation,
    pub channel_buffer: usize,
    pub resume: bool,
    pub piece_size: u64,
    pub min_split_size: u64,
    /// Maximum retry attempts per segment (0 = no retries).
    pub max_retries: u32,
    /// Base delay for exponential backoff between retries.
    pub retry_base_delay: Duration,
    /// Maximum delay cap for exponential backoff.
    pub retry_max_delay: Duration,
    /// Maximum download speed in bytes/sec. 0 = unlimited.
    pub max_download_speed: u64,
    /// Optional checksum for post-download verification.
    pub checksum: Option<Checksum>,
}

impl DownloadSpec {
    pub fn new(url: impl Into<String>, output_path: impl Into<PathBuf>) -> Self {
        Self {
            url: url.into(),
            output_path: output_path.into(),
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
            max_download_speed: 0,
            checksum: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_download_spec_defaults() {
        let spec = DownloadSpec::new("https://example.com/file", "/tmp/file");
        assert_eq!(spec.url, "https://example.com/file");
        assert_eq!(spec.output_path, PathBuf::from("/tmp/file"));
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
        assert_eq!(spec.max_download_speed, 0);
        assert!(spec.checksum.is_none());
        assert!(spec.headers.is_empty());
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
