use thiserror::Error;

/// Errors that can occur during a download.
#[derive(Debug, Error)]
pub enum DownloadError {
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("download cancelled")]
    Cancelled,

    #[error("download paused")]
    Paused,

    #[error("HTTP {status}: {message}")]
    HttpStatus { status: u16, message: String },

    #[error("internal channel closed unexpectedly")]
    ChannelClosed,

    #[error("invalid download config: {0}")]
    InvalidConfig(String),

    #[error("resume state mismatch: {0}")]
    ResumeMismatch(String),

    #[error("control file corrupted: {0}")]
    ControlFileCorrupted(String),

    #[error("retry budget exhausted after {elapsed:?} (limit {limit:?})")]
    RetryBudgetExceeded {
        elapsed: std::time::Duration,
        limit: std::time::Duration,
    },

    #[error("task failure: {0}")]
    TaskFailed(String),

    #[error("internal error: {0}")]
    Internal(String),

    #[error("checksum mismatch: expected {expected}, got {actual}")]
    ChecksumMismatch { expected: String, actual: String },
}

impl DownloadError {
    /// Returns `true` if this error is transient and the request should be retried.
    ///
    /// Retryable conditions include timeouts, connection resets, and
    /// server errors (429, 500, 502, 503, 504).
    pub fn is_retryable(&self) -> bool {
        match self {
            DownloadError::Http(e) => {
                e.is_timeout() || e.is_connect() || e.is_request() || e.is_body()
            }
            DownloadError::HttpStatus { status, .. } => {
                matches!(status, 429 | 500 | 502 | 503 | 504)
            }
            DownloadError::Io(e) => {
                matches!(
                    e.kind(),
                    std::io::ErrorKind::ConnectionReset
                        | std::io::ErrorKind::ConnectionAborted
                        | std::io::ErrorKind::TimedOut
                        | std::io::ErrorKind::UnexpectedEof
                )
            }
            _ => false,
        }
    }

    /// Extract a Retry-After hint (in seconds) from a 429/503 response, if available.
    pub fn retry_after_secs(&self) -> Option<u64> {
        if let DownloadError::HttpStatus { status, message } = self {
            if *status == 429 || *status == 503 {
                // The message may contain a Retry-After value set during response parsing
                message
                    .strip_prefix("retry-after:")
                    .and_then(|s| s.trim().parse().ok())
            } else {
                None
            }
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_retryable_http_status_codes() {
        let retryable = [429, 500, 502, 503, 504];
        for status in retryable {
            let e = DownloadError::HttpStatus {
                status,
                message: "test".into(),
            };
            assert!(e.is_retryable(), "status {status} should be retryable");
        }

        let not_retryable = [400, 401, 403, 404, 405];
        for status in not_retryable {
            let e = DownloadError::HttpStatus {
                status,
                message: "test".into(),
            };
            assert!(!e.is_retryable(), "status {status} should not be retryable");
        }
    }

    #[test]
    fn test_is_retryable_io_errors() {
        let retryable_kinds = [
            std::io::ErrorKind::ConnectionReset,
            std::io::ErrorKind::ConnectionAborted,
            std::io::ErrorKind::TimedOut,
            std::io::ErrorKind::UnexpectedEof,
        ];
        for kind in retryable_kinds {
            let e = DownloadError::Io(std::io::Error::new(kind, "test"));
            assert!(e.is_retryable(), "{kind:?} should be retryable");
        }

        let not_retryable =
            DownloadError::Io(std::io::Error::new(std::io::ErrorKind::NotFound, "test"));
        assert!(!not_retryable.is_retryable());
    }

    #[test]
    fn test_is_retryable_other_variants() {
        assert!(!DownloadError::Cancelled.is_retryable());
        assert!(!DownloadError::Paused.is_retryable());
        assert!(!DownloadError::ChannelClosed.is_retryable());
        assert!(!DownloadError::InvalidConfig("test".into()).is_retryable());
        assert!(!DownloadError::ResumeMismatch("test".into()).is_retryable());
        assert!(!DownloadError::ControlFileCorrupted("test".into()).is_retryable());
        assert!(!DownloadError::RetryBudgetExceeded {
            elapsed: std::time::Duration::from_secs(2),
            limit: std::time::Duration::from_secs(1),
        }
        .is_retryable());
        assert!(!DownloadError::TaskFailed("test".into()).is_retryable());
        assert!(!DownloadError::Internal("test".into()).is_retryable());
        assert!(!DownloadError::ChecksumMismatch {
            expected: "a".into(),
            actual: "b".into(),
        }
        .is_retryable());
    }

    #[test]
    fn test_retry_after_secs_with_hint() {
        let e = DownloadError::HttpStatus {
            status: 429,
            message: "retry-after:30".into(),
        };
        assert_eq!(e.retry_after_secs(), Some(30));

        let e = DownloadError::HttpStatus {
            status: 503,
            message: "retry-after:5".into(),
        };
        assert_eq!(e.retry_after_secs(), Some(5));
    }

    #[test]
    fn test_retry_after_secs_without_hint() {
        let e = DownloadError::HttpStatus {
            status: 429,
            message: "Too Many Requests".into(),
        };
        assert_eq!(e.retry_after_secs(), None);

        let e = DownloadError::HttpStatus {
            status: 500,
            message: "retry-after:10".into(),
        };
        assert_eq!(e.retry_after_secs(), None);
    }

    #[test]
    fn test_retry_after_secs_non_http_status() {
        assert_eq!(DownloadError::Cancelled.retry_after_secs(), None);
        assert_eq!(DownloadError::Paused.retry_after_secs(), None);
        assert_eq!(DownloadError::ChannelClosed.retry_after_secs(), None);
    }

    #[test]
    fn test_error_display() {
        let e = DownloadError::Cancelled;
        assert_eq!(format!("{e}"), "download cancelled");

        let e = DownloadError::Paused;
        assert_eq!(format!("{e}"), "download paused");

        let e = DownloadError::ChecksumMismatch {
            expected: "aaa".into(),
            actual: "bbb".into(),
        };
        assert!(format!("{e}").contains("aaa"));
        assert!(format!("{e}").contains("bbb"));

        let e = DownloadError::InvalidConfig("max_connections must be >= 1".into());
        assert!(format!("{e}").contains("invalid download config"));

        let e = DownloadError::HttpStatus {
            status: 404,
            message: "Not Found".into(),
        };
        assert!(format!("{e}").contains("404"));
    }
}
