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
            DownloadError::Http(e) => e.is_timeout() || e.is_connect(),
            DownloadError::HttpStatus { status, .. } => {
                matches!(status, 429 | 500 | 502 | 503 | 504)
            }
            _ => false,
        }
    }
}
