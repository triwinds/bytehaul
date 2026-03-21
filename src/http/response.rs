/// Metadata extracted from an HTTP response.
#[derive(Debug, Clone)]
pub(crate) struct ResponseMeta {
    pub content_length: Option<u64>,
    pub accept_ranges: bool,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub content_encoding: Option<String>,
}

impl ResponseMeta {
    pub fn from_response(response: &reqwest::Response) -> Self {
        let headers = response.headers();

        let accept_ranges = headers
            .get("accept-ranges")
            .and_then(|v| v.to_str().ok())
            .map(|v| v.eq_ignore_ascii_case("bytes"))
            .unwrap_or(false);

        let etag = headers
            .get("etag")
            .and_then(|v| v.to_str().ok())
            .map(String::from);

        let last_modified = headers
            .get("last-modified")
            .and_then(|v| v.to_str().ok())
            .map(String::from);

        let content_encoding = headers
            .get("content-encoding")
            .and_then(|v| v.to_str().ok())
            .map(String::from);

        Self {
            content_length: response.content_length(),
            accept_ranges,
            etag,
            last_modified,
            content_encoding,
        }
    }
}
