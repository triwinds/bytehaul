use std::collections::HashMap;

use hyper::{Request, Version};

use crate::http::HttpRequestBody;

/// Build an HTTP GET request with custom headers.
pub(crate) fn build_get_request(
    url: &str,
    headers: &HashMap<String, String>,
)-> Request<HttpRequestBody> {
    let mut req = Request::builder()
        .method("GET")
        .uri(url)
        .version(Version::HTTP_11);
    for (k, v) in headers {
        req = req.header(k.as_str(), v.as_str());
    }
    req.body(HttpRequestBody::new()).expect("get request")
}

/// Build an HTTP GET request with a Range header.
pub(crate) fn build_range_request(
    url: &str,
    headers: &HashMap<String, String>,
    start: u64,
    end: u64,
) -> Request<HttpRequestBody> {
    let mut req = Request::builder()
        .method("GET")
        .uri(url)
        .version(Version::HTTP_11)
        .header("Range", format!("bytes={start}-{end}"))
        .header("Accept-Encoding", "identity");
    for (k, v) in headers {
        req = req.header(k.as_str(), v.as_str());
    }
    req.body(HttpRequestBody::new()).expect("range request")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_range_request_forces_identity_encoding() {
        let req = build_range_request(
            "https://example.com/file.bin",
            &HashMap::new(),
            0,
            99,
        );

        assert_eq!(req.headers().get("range").unwrap(), "bytes=0-99");
        assert_eq!(req.headers().get("accept-encoding").unwrap(), "identity");
        assert_eq!(req.version(), Version::HTTP_11);
    }

    #[test]
    fn test_build_get_request_with_custom_headers() {
        let mut headers = HashMap::new();
        headers.insert("X-Custom".to_string(), "value123".to_string());
        headers.insert("Authorization".to_string(), "Bearer token".to_string());

        let req = build_get_request("https://example.com/file.bin", &headers);

        assert_eq!(req.headers().get("x-custom").unwrap(), "value123");
        assert_eq!(req.headers().get("authorization").unwrap(), "Bearer token");
        assert_eq!(req.version(), Version::HTTP_11);
    }

    #[test]
    fn test_build_range_request_with_custom_headers() {
        let mut headers = HashMap::new();
        headers.insert("X-Trace".to_string(), "abc".to_string());

        let req = build_range_request("https://example.com/file.bin", &headers, 10, 19);

        assert_eq!(req.headers().get("range").unwrap(), "bytes=10-19");
        assert_eq!(req.headers().get("x-trace").unwrap(), "abc");
    }
}
