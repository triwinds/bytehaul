/// Metadata extracted from an HTTP response.
#[derive(Debug, Clone)]
pub(crate) struct ResponseMeta {
    pub content_length: Option<u64>,
    pub content_range_start: Option<u64>,
    pub content_range_end: Option<u64>,
    /// Total file size parsed from `Content-Range` header (206 responses).
    pub content_range_total: Option<u64>,
    #[allow(dead_code)]
    pub accept_ranges: bool,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub content_disposition: Option<String>,
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

        let content_disposition = headers
            .get("content-disposition")
            .and_then(|v| v.to_str().ok())
            .map(String::from);

        let content_encoding = headers
            .get("content-encoding")
            .and_then(|v| v.to_str().ok())
            .map(String::from);

        let (content_range_start, content_range_end, content_range_total) = headers
            .get("content-range")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| {
                let value = s.trim();
                let rest = value.strip_prefix("bytes ")?;
                let (range_part, total_part) = rest.split_once('/')?;
                let (start, end) = range_part.split_once('-')?;
                let start = start.trim().parse::<u64>().ok()?;
                let end = end.trim().parse::<u64>().ok()?;
                if total_part == "*" {
                    return Some((Some(start), Some(end), None));
                }
                Some((
                    Some(start),
                    Some(end),
                    total_part.trim().parse::<u64>().ok(),
                ))
            })
            .unwrap_or((None, None, None));

        Self {
            content_length: response.content_length(),
            content_range_start,
            content_range_end,
            content_range_total,
            accept_ranges,
            etag,
            last_modified,
            content_disposition,
            content_encoding,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_content_range_metadata() {
        let response = reqwest::Response::from(
            http::Response::builder()
                .status(206)
                .header("content-range", "bytes 100-199/1000")
                .header("content-length", "100")
                .header("content-encoding", "identity")
                .body("test")
                .unwrap(),
        );

        let meta = ResponseMeta::from_response(&response);
        assert_eq!(meta.content_range_start, Some(100));
        assert_eq!(meta.content_range_end, Some(199));
        assert_eq!(meta.content_range_total, Some(1000));
        assert_eq!(meta.content_encoding.as_deref(), Some("identity"));
        assert_eq!(meta.content_disposition, None);
    }

    #[test]
    fn test_missing_content_range_metadata() {
        let response = reqwest::Response::from(
            http::Response::builder()
                .status(200)
                .header("content-length", "4")
                .body("test")
                .unwrap(),
        );

        let meta = ResponseMeta::from_response(&response);
        assert_eq!(meta.content_range_start, None);
        assert_eq!(meta.content_range_end, None);
        assert_eq!(meta.content_range_total, None);
    }

    #[test]
    fn test_accept_ranges_bytes() {
        let response = reqwest::Response::from(
            http::Response::builder()
                .status(200)
                .header("accept-ranges", "bytes")
                .body("test")
                .unwrap(),
        );
        let meta = ResponseMeta::from_response(&response);
        assert!(meta.accept_ranges);
    }

    #[test]
    fn test_accept_ranges_none() {
        let response = reqwest::Response::from(
            http::Response::builder()
                .status(200)
                .header("accept-ranges", "none")
                .body("test")
                .unwrap(),
        );
        let meta = ResponseMeta::from_response(&response);
        assert!(!meta.accept_ranges);
    }

    #[test]
    fn test_etag_and_last_modified() {
        let response = reqwest::Response::from(
            http::Response::builder()
                .status(200)
                .header("etag", "\"abc123\"")
                .header("last-modified", "Thu, 01 Jan 2026 00:00:00 GMT")
                .body("test")
                .unwrap(),
        );
        let meta = ResponseMeta::from_response(&response);
        assert_eq!(meta.etag.as_deref(), Some("\"abc123\""));
        assert_eq!(
            meta.last_modified.as_deref(),
            Some("Thu, 01 Jan 2026 00:00:00 GMT")
        );
    }

    #[test]
    fn test_content_range_unknown_total() {
        let response = reqwest::Response::from(
            http::Response::builder()
                .status(206)
                .header("content-range", "bytes 0-99/*")
                .body("test")
                .unwrap(),
        );
        let meta = ResponseMeta::from_response(&response);
        assert_eq!(meta.content_range_start, Some(0));
        assert_eq!(meta.content_range_end, Some(99));
        assert_eq!(meta.content_range_total, None);
    }

    #[test]
    fn test_content_disposition() {
        let response = reqwest::Response::from(
            http::Response::builder()
                .status(200)
                .header("content-disposition", "attachment; filename=test.bin")
                .body("test")
                .unwrap(),
        );
        let meta = ResponseMeta::from_response(&response);
        assert_eq!(
            meta.content_disposition.as_deref(),
            Some("attachment; filename=test.bin")
        );
    }
}
