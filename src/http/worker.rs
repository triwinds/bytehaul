use std::collections::HashMap;
use std::time::Duration;

use hyper::header::LOCATION;
use hyper::{HeaderMap, StatusCode};
use tokio::sync::OnceCell;
use url::Url;

use crate::config::DownloadSpec;
use crate::error::DownloadError;
use crate::http::request;
use crate::http::response::ResponseMeta;
use crate::http::HttpResponse;
use crate::network::BytehaulClient;

/// HTTP worker responsible for sending requests and validating responses.
pub(crate) struct HttpWorker {
    client: BytehaulClient,
    url: String,
    headers: HashMap<String, String>,
    timeout: Duration,
    final_url: OnceCell<String>,
}

impl HttpWorker {
    pub fn new(client: BytehaulClient, spec: &DownloadSpec) -> Self {
        Self {
            client,
            url: spec.url.clone(),
            headers: spec.headers.clone(),
            timeout: spec.read_timeout,
            final_url: OnceCell::new(),
        }
    }

    pub async fn final_url(&self) -> Result<String, DownloadError> {
        self.final_url
            .get_or_try_init(|| async { self.resolve_redirects(&self.url).await })
            .await
            .cloned()
    }

    /// Send an initial GET request (no Range) and validate the response.
    pub async fn send_get(&self) -> Result<(HttpResponse, ResponseMeta), DownloadError> {
        let final_url = self.final_url().await?;
        tracing::debug!(url = %final_url, "sending GET request");
        let req = request::build_get_request(&final_url, &self.headers);
        let response = self.client.request_with_timeout(req, self.timeout).await?;

        let status = response.status();
        tracing::debug!(status = status.as_u16(), "GET response received");
        if !status.is_success() {
            return Err(make_http_error(response.headers(), status.as_u16()));
        }

        let meta = ResponseMeta::from_parts(status, response.headers(), None);
        Ok((response, meta))
    }

    /// Send a Range GET request and validate the 206 response.
    pub async fn send_range(
        &self,
        start: u64,
        end: u64,
    ) -> Result<(HttpResponse, ResponseMeta), DownloadError> {
        let final_url = self.final_url().await?;
        let req = request::build_range_request(&final_url, &self.headers, start, end);
        let response = self.client.request_with_timeout(req, self.timeout).await?;

        let status = response.status();
        tracing::debug!(status = status.as_u16(), start = start, end = end, "Range response received");
        if status.as_u16() == 200 {
            let meta = ResponseMeta::from_parts(status, response.headers(), None);
            return Ok((response, meta));
        }
        if status.as_u16() != 206 {
            return Err(make_http_error(response.headers(), status.as_u16()));
        }

        let meta = ResponseMeta::from_parts(status, response.headers(), None);
        Ok((response, meta))
    }

    async fn resolve_redirects(&self, url: &str) -> Result<String, DownloadError> {
        let mut current = validate_redirect_target(Url::parse(url).map_err(|error| {
            DownloadError::InvalidConfig(format!("invalid download URL '{url}': {error}"))
        })?)?
        .to_string();

        for _ in 0..10 {
            let req = request::build_get_request(&current, &self.headers);
            let response = self.client.request_with_timeout(req, self.timeout).await?;
            let status = response.status();
            if !status.is_redirection() {
                return Ok(current);
            }

            let location = response
                .headers()
                .get(LOCATION)
                .and_then(|value| value.to_str().ok())
                .ok_or_else(|| DownloadError::HttpStatus {
                    status: status.as_u16(),
                    message: "redirect response missing Location header".into(),
                })?;
            current = resolve_redirect_target(&current, location)?;
        }

        Err(DownloadError::HttpStatus {
            status: StatusCode::LOOP_DETECTED.as_u16(),
            message: "too many redirects".into(),
        })
    }
}

fn validate_redirect_target(url: Url) -> Result<Url, DownloadError> {
    if matches!(url.scheme(), "http" | "https") {
        Ok(url)
    } else {
        Err(DownloadError::InvalidConfig(format!(
            "redirect target '{url}' must use http or https"
        )))
    }
}

fn resolve_redirect_target(current_url: &str, location: &str) -> Result<String, DownloadError> {
    let base = Url::parse(current_url).map_err(|error| {
        DownloadError::InvalidConfig(format!("invalid redirect base URL '{current_url}': {error}"))
    })?;
    let target = match Url::parse(location) {
        Ok(url) => url,
        Err(url::ParseError::RelativeUrlWithoutBase) => base.join(location).map_err(|error| {
            DownloadError::InvalidConfig(format!(
                "invalid redirect location '{location}' from '{current_url}': {error}"
            ))
        })?,
        Err(error) => {
            return Err(DownloadError::InvalidConfig(format!(
                "invalid redirect location '{location}': {error}"
            )))
        }
    };
    Ok(validate_redirect_target(target)?.to_string())
}

/// Build an appropriate HttpStatus error, embedding Retry-After hint for 429/503.
fn make_http_error(headers: &HeaderMap, status: u16) -> DownloadError {
    let retry_after = headers
        .get("retry-after")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok());

    let message = match retry_after {
        Some(seconds) => format!("retry-after:{seconds}"),
        None => StatusCode::from_u16(status)
            .ok()
            .and_then(|code| code.canonical_reason().map(str::to_string))
            .unwrap_or_else(|| "unknown".to_string()),
    };

    DownloadError::HttpStatus { status, message }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use http_body_util::Empty;
    use hyper::Response;
    use warp::Filter;

    fn worker_for(url: String) -> HttpWorker {
        let mut spec = DownloadSpec::new(url).output_path("unused.bin");
        spec.read_timeout = Duration::from_secs(5);
        let client = crate::network::ClientNetworkConfig::default()
            .build_client()
            .unwrap();
        HttpWorker::new(client, &spec)
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
    async fn send_get_caches_final_url_after_redirect() {
        let redirect = warp::path("redirect").map(|| {
            warp::redirect::temporary(warp::http::Uri::from_static("/target"))
        });
        let target = warp::path("target").map(|| {
            warp::http::Response::builder()
                .status(200)
                .header("content-length", "4")
                .body("done")
                .unwrap()
        });
        let routes = redirect.or(target);
        let (addr, server) = warp::serve(routes).bind_ephemeral(([127, 0, 0, 1], 0));
        tokio::spawn(server);

        let worker = worker_for(format!("http://{addr}/redirect"));
        worker.send_get().await.unwrap();

        assert_eq!(worker.final_url().await.unwrap(), format!("http://{addr}/target"));
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
        let response = Response::builder()
            .status(404)
            .body(Empty::<Bytes>::new())
            .unwrap();
        let (parts, _) = response.into_parts();

        let err = make_http_error(&parts.headers, 404);
        match err {
            DownloadError::HttpStatus { status, message } => {
                assert_eq!(status, 404);
                assert_eq!(message, "Not Found");
            }
            other => panic!("expected HttpStatus, got {other:?}"),
        }
    }

    #[test]
    fn resolve_redirect_target_rejects_non_http_scheme() {
        let err = resolve_redirect_target("https://example.com/file", "file:///tmp/secret")
            .unwrap_err()
            .to_string();
        assert!(err.contains("must use http or https"));
    }
}
