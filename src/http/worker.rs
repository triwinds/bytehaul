use crate::config::DownloadSpec;
use crate::error::DownloadError;
use crate::http::request;
use crate::http::response::ResponseMeta;

/// HTTP worker responsible for sending requests and validating responses.
pub(crate) struct HttpWorker {
    client: reqwest::Client,
    url: String,
    headers: std::collections::HashMap<String, String>,
    timeout: std::time::Duration,
}

impl HttpWorker {
    pub fn new(client: reqwest::Client, spec: &DownloadSpec) -> Self {
        Self {
            client,
            url: spec.url.clone(),
            headers: spec.headers.clone(),
            timeout: spec.read_timeout,
        }
    }

    /// Send an initial GET request (no Range) and validate the response.
    pub async fn send_get(&self) -> Result<(reqwest::Response, ResponseMeta), DownloadError> {
        tracing::debug!(url = %self.url, "sending GET request");
        let req = request::build_get_request(&self.client, &self.url, &self.headers, self.timeout);
        let response = req.send().await?;

        let status = response.status();
        tracing::debug!(status = status.as_u16(), "GET response received");
        if !status.is_success() {
            return Err(make_http_error(&response, status.as_u16()));
        }

        let meta = ResponseMeta::from_response(&response);
        Ok((response, meta))
    }

    /// Send a Range GET request and validate the 206 response.
    pub async fn send_range(
        &self,
        start: u64,
        end: u64,
    ) -> Result<(reqwest::Response, ResponseMeta), DownloadError> {
        let req = request::build_range_request(
            &self.client,
            &self.url,
            &self.headers,
            self.timeout,
            start,
            end,
        );
        let response = req.send().await?;

        let status = response.status();
        tracing::debug!(status = status.as_u16(), start = start, end = end, "Range response received");
        if status.as_u16() == 200 {
            // Server ignored Range, returned full content
            let meta = ResponseMeta::from_response(&response);
            return Ok((response, meta));
        }
        if status.as_u16() != 206 {
            return Err(make_http_error(&response, status.as_u16()));
        }

        let meta = ResponseMeta::from_response(&response);
        Ok((response, meta))
    }
}

/// Build an appropriate HttpStatus error, embedding Retry-After hint for 429/503.
fn make_http_error(response: &reqwest::Response, status: u16) -> DownloadError {
    let retry_after = response
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok());

    let message = match retry_after {
        Some(secs) => format!("retry-after:{secs}"),
        None => response
            .status()
            .canonical_reason()
            .unwrap_or("unknown")
            .to_string(),
    };

    DownloadError::HttpStatus { status, message }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DownloadSpec;
    use std::time::Duration;
    use warp::Filter;

    fn worker_for(url: String) -> HttpWorker {
        let mut spec = DownloadSpec::new(url).output_path("unused.bin");
        spec.read_timeout = Duration::from_secs(5);
        HttpWorker::new(reqwest::Client::new(), &spec)
    }

    #[tokio::test]
    async fn send_get_returns_response_metadata() {
        let route = warp::path("file").map(|| {
            warp::http::Response::builder()
                .status(200)
                .header("content-length", "4")
                .header("accept-ranges", "bytes")
                .header("etag", "\"etag-1\"")
                .header("last-modified", "Thu, 01 Jan 2026 00:00:00 GMT")
                .body("test")
                .unwrap()
        });
        let (addr, server) = warp::serve(route).bind_ephemeral(([127, 0, 0, 1], 0));
        tokio::spawn(server);

        let worker = worker_for(format!("http://{addr}/file"));
        let (_, meta) = worker.send_get().await.unwrap();

        assert_eq!(meta.content_length, Some(4));
        assert!(meta.accept_ranges);
        assert_eq!(meta.etag.as_deref(), Some("\"etag-1\""));
        assert_eq!(
            meta.last_modified.as_deref(),
            Some("Thu, 01 Jan 2026 00:00:00 GMT")
        );
    }

    #[tokio::test]
    async fn send_range_returns_partial_content_metadata() {
        let route = warp::path("range").map(|| {
            warp::http::Response::builder()
                .status(206)
                .header("content-length", "3")
                .header("content-range", "bytes 2-4/10")
                .header("accept-ranges", "bytes")
                .body("cde")
                .unwrap()
        });
        let (addr, server) = warp::serve(route).bind_ephemeral(([127, 0, 0, 1], 0));
        tokio::spawn(server);

        let worker = worker_for(format!("http://{addr}/range"));
        let (_, meta) = worker.send_range(2, 4).await.unwrap();

        assert_eq!(meta.content_length, Some(3));
        assert_eq!(meta.content_range_start, Some(2));
        assert_eq!(meta.content_range_end, Some(4));
        assert_eq!(meta.content_range_total, Some(10));
    }

    #[tokio::test]
    async fn send_range_accepts_full_content_when_server_ignores_range() {
        let route = warp::path("range-fallback").map(|| {
            warp::http::Response::builder()
                .status(200)
                .header("content-length", "10")
                .body("0123456789")
                .unwrap()
        });
        let (addr, server) = warp::serve(route).bind_ephemeral(([127, 0, 0, 1], 0));
        tokio::spawn(server);

        let worker = worker_for(format!("http://{addr}/range-fallback"));
        let (_, meta) = worker.send_range(2, 4).await.unwrap();

        assert_eq!(meta.content_length, Some(10));
        assert_eq!(meta.content_range_start, None);
        assert_eq!(meta.content_range_end, None);
        assert_eq!(meta.content_range_total, None);
    }

    #[tokio::test]
    async fn send_get_includes_retry_after_in_http_status_error() {
        let route = warp::path("busy").map(|| {
            warp::http::Response::builder()
                .status(503)
                .header("retry-after", "7")
                .body(Vec::<u8>::new())
                .unwrap()
        });
        let (addr, server) = warp::serve(route).bind_ephemeral(([127, 0, 0, 1], 0));
        tokio::spawn(server);

        let worker = worker_for(format!("http://{addr}/busy"));
        let err = worker.send_get().await.unwrap_err();

        match err {
            DownloadError::HttpStatus { status, message } => {
                assert_eq!(status, 503);
                assert_eq!(message, "retry-after:7");
            }
            other => panic!("expected HttpStatus, got {other:?}"),
        }
    }

    #[test]
    fn make_http_error_falls_back_to_canonical_reason() {
        let response = reqwest::Response::from(
            http::Response::builder()
                .status(404)
                .body(Vec::<u8>::new())
                .unwrap(),
        );

        let err = make_http_error(&response, 404);
        match err {
            DownloadError::HttpStatus { status, message } => {
                assert_eq!(status, 404);
                assert_eq!(message, "Not Found");
            }
            other => panic!("expected HttpStatus, got {other:?}"),
        }
    }
}
