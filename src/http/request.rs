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
    req
}
