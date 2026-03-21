use std::collections::HashMap;
use std::time::Duration;

/// Build an HTTP GET request with custom headers and timeout.
pub(crate) fn build_get_request(
    client: &reqwest::Client,
    url: &str,
    headers: &HashMap<String, String>,
    timeout: Duration,
) -> reqwest::RequestBuilder {
    let mut req = client.get(url);
    for (k, v) in headers {
        req = req.header(k.as_str(), v.as_str());
    }
    req.timeout(timeout)
}

/// Build an HTTP GET request with a Range header.
pub(crate) fn build_range_request(
    client: &reqwest::Client,
    url: &str,
    headers: &HashMap<String, String>,
    timeout: Duration,
    start: u64,
    end: u64,
) -> reqwest::RequestBuilder {
    let mut req = build_get_request(client, url, headers, timeout);
    req = req.header("Range", format!("bytes={start}-{end}"));
    req = req.header("Accept-Encoding", "identity");
    req
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_range_request_forces_identity_encoding() {
        let client = reqwest::Client::new();
        let req = build_range_request(
            &client,
            "https://example.com/file.bin",
            &HashMap::new(),
            Duration::from_secs(5),
            0,
            99,
        )
        .build()
        .unwrap();

        assert_eq!(req.headers().get("range").unwrap(), "bytes=0-99");
        assert_eq!(req.headers().get("accept-encoding").unwrap(), "identity");
    }

    #[test]
    fn test_build_get_request_with_custom_headers() {
        let client = reqwest::Client::new();
        let mut headers = HashMap::new();
        headers.insert("X-Custom".to_string(), "value123".to_string());
        headers.insert("Authorization".to_string(), "Bearer token".to_string());

        let req = build_get_request(
            &client,
            "https://example.com/file.bin",
            &headers,
            Duration::from_secs(10),
        )
        .build()
        .unwrap();

        assert_eq!(req.headers().get("x-custom").unwrap(), "value123");
        assert_eq!(req.headers().get("authorization").unwrap(), "Bearer token");
    }
}
