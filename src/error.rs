use thiserror::Error;

#[derive(Debug, Error)]
pub enum DownloadError {
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("download cancelled")]
    Cancelled,

    #[error("HTTP {status}: {message}")]
    HttpStatus { status: u16, message: String },

    #[error("internal channel closed unexpectedly")]
    ChannelClosed,

    #[error("resume metadata mismatch: {0}")]
    ResumeMismatch(String),

    #[error("control file corrupted: {0}")]
    ControlFileCorrupted(String),

    #[error("{0}")]
    Other(String),
}

impl DownloadError {
    pub fn is_retryable(&self) -> bool {
        match self {
            DownloadError::Http(e) => {
                e.is_timeout() || e.is_connect() || e.is_request()
                    || e.is_body()
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
                message.strip_prefix("retry-after:").and_then(|s| s.trim().parse().ok())
            } else {
                None
            }
        } else {
            None
        }
    }
}
