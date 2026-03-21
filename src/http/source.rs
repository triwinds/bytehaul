use std::time::Instant;

/// Represents a download source URL with associated metadata and failure tracking.
#[derive(Debug, Clone)]
pub(crate) struct HttpSource {
    /// Original URL provided by the user.
    pub original_url: String,
    /// Effective URL (after redirects). Used for actual requests.
    pub effective_url: String,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub supports_range: bool,
    pub content_length: Option<u64>,
    /// Number of consecutive failures on this source.
    pub fail_count: u32,
    /// When the last failure occurred (for backoff timing).
    pub last_fail_time: Option<Instant>,
}

impl HttpSource {
    pub fn new(url: impl Into<String>) -> Self {
        let url = url.into();
        Self {
            original_url: url.clone(),
            effective_url: url,
            etag: None,
            last_modified: None,
            supports_range: false,
            content_length: None,
            fail_count: 0,
            last_fail_time: None,
        }
    }

    /// Record a failure on this source.
    pub fn record_failure(&mut self) {
        self.fail_count += 1;
        self.last_fail_time = Some(Instant::now());
    }

    /// Reset failure tracking (e.g. after a successful request).
    pub fn reset_failures(&mut self) {
        self.fail_count = 0;
        self.last_fail_time = None;
    }

    /// Update the effective URL after a redirect.
    pub fn update_effective_url(&mut self, url: String) {
        self.effective_url = url;
    }
}
