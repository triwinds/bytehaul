/// Represents a download source URL with associated metadata.
#[derive(Debug, Clone)]
pub(crate) struct HttpSource {
    pub url: String,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub supports_range: bool,
    pub content_length: Option<u64>,
}

impl HttpSource {
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            etag: None,
            last_modified: None,
            supports_range: false,
            content_length: None,
        }
    }
}
