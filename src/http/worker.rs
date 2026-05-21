use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use hyper::header::LOCATION;
use hyper::{HeaderMap, StatusCode};
use tokio::sync::Mutex;
use url::Url;

use crate::config::DownloadSpec;
use crate::error::DownloadError;
use crate::http::request;
use crate::http::response::ResponseMeta;
use crate::http::HttpResponse;
use crate::network::BytehaulClient;

/// HTTP worker responsible for sending requests and validating responses.
#[derive(Clone)]
pub(crate) struct HttpWorker {
    client: BytehaulClient,
    url: String,
    headers: HashMap<String, String>,
    timeout: Duration,
    final_url: Arc<Mutex<Option<String>>>,
}

impl HttpWorker {
    pub fn new(client: BytehaulClient, spec: &DownloadSpec) -> Self {
        Self {
            client,
            url: spec.url.clone(),
            headers: spec.headers.clone(),
            timeout: spec.read_timeout,
            final_url: Arc::new(Mutex::new(None)),
        }
    }

    pub async fn final_url(&self) -> Result<String, DownloadError> {
        if let Some(url) = self.cached_final_url().await {
            return Ok(url);
        }

        let resolved = self.resolve_redirects(&self.url).await?;
        self.set_final_url(resolved.clone()).await;
        Ok(resolved)
    }

    /// Send an initial GET request (no Range) and validate the response.
    pub async fn send_get(&self) -> Result<(HttpResponse, ResponseMeta), DownloadError> {
        let start_url = self
            .cached_final_url()
            .await
            .unwrap_or_else(|| self.url.clone());
        let can_refresh = start_url != self.url;
        match self.send_get_following_redirects(&start_url).await {
            Err(error) if can_refresh && is_stale_redirect_target_error(&error) => {
                self.clear_final_url().await;
                self.send_get_following_redirects(&self.url).await
            }
            result => result,
        }
    }

    /// Send a Range GET request and validate the 206 response.
    pub async fn send_range(
        &self,
        start: u64,
        end: u64,
    ) -> Result<(HttpResponse, ResponseMeta), DownloadError> {
        let start_url = self
            .cached_final_url()
            .await
            .unwrap_or_else(|| self.url.clone());
        let can_refresh = start_url != self.url;
        match self
            .send_range_following_redirects(&start_url, start, end)
            .await
        {
            Err(error) if can_refresh && is_stale_redirect_target_error(&error) => {
                self.clear_final_url().await;
                self.send_range_following_redirects(&self.url, start, end)
                    .await
            }
            result => result,
        }
    }

    async fn cached_final_url(&self) -> Option<String> {
        self.final_url.lock().await.clone()
    }

    async fn set_final_url(&self, url: String) {
        *self.final_url.lock().await = Some(url);
    }

    async fn clear_final_url(&self) {
        *self.final_url.lock().await = None;
    }

    async fn send_get_following_redirects(
        &self,
        start_url: &str,
    ) -> Result<(HttpResponse, ResponseMeta), DownloadError> {
        let mut current = parse_download_url(start_url)?;

        for _ in 0..10 {
            tracing::debug!(url = %current, "sending GET request");
            let req = request::build_get_request(&current, &self.headers);
            let response = self.client.request_with_timeout(req, self.timeout).await?;
            let status = response.status();
            tracing::debug!(status = status.as_u16(), "GET response received");

            if status.is_redirection() {
                current = redirect_location(&current, response.headers(), status)?;
                continue;
            }

            self.set_final_url(current.clone()).await;
            if !status.is_success() {
                return Err(make_http_error(response.headers(), status.as_u16()));
            }

            let meta = ResponseMeta::from_parts(status, response.headers(), None);
            return Ok((response, meta));
        }

        too_many_redirects_error()
    }

    async fn send_range_following_redirects(
        &self,
        start_url: &str,
        start: u64,
        end: u64,
    ) -> Result<(HttpResponse, ResponseMeta), DownloadError> {
        let mut current = parse_download_url(start_url)?;

        for _ in 0..10 {
            let req = request::build_range_request(&current, &self.headers, start, end);
            let response = self.client.request_with_timeout(req, self.timeout).await?;

            let status = response.status();
            tracing::debug!(status = status.as_u16(), start = start, end = end, url = %current, "Range response received");

            if status.is_redirection() {
                current = redirect_location(&current, response.headers(), status)?;
                continue;
            }

            self.set_final_url(current.clone()).await;
            if status.as_u16() == 200 {
                let meta = ResponseMeta::from_parts(status, response.headers(), None);
                return Ok((response, meta));
            }
            if status.as_u16() != 206 {
                return Err(make_http_error(response.headers(), status.as_u16()));
            }

            let meta = ResponseMeta::from_parts(status, response.headers(), None);
            return Ok((response, meta));
        }

        too_many_redirects_error()
    }

    async fn resolve_redirects(&self, url: &str) -> Result<String, DownloadError> {
        let mut current = parse_download_url(url)?;

        for _ in 0..10 {
            let req = request::build_get_request(&current, &self.headers);
            let response = self.client.request_with_timeout(req, self.timeout).await?;
            let status = response.status();
            if !status.is_redirection() {
                return Ok(current);
            }

            current = redirect_location(&current, response.headers(), status)?;
        }

        too_many_redirects_error()
    }
}

fn parse_download_url(url: &str) -> Result<String, DownloadError> {
    validate_redirect_target(Url::parse(url).map_err(|error| {
        DownloadError::InvalidConfig(format!("invalid download URL '{url}': {error}"))
    })?)
    .map(|url| url.to_string())
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
        DownloadError::InvalidConfig(format!(
            "invalid redirect base URL '{current_url}': {error}"
        ))
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

fn redirect_location(
    current_url: &str,
    headers: &HeaderMap,
    status: StatusCode,
) -> Result<String, DownloadError> {
    let location = headers
        .get(LOCATION)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| DownloadError::HttpStatus {
            status: status.as_u16(),
            message: "redirect response missing Location header".into(),
        })?;
    resolve_redirect_target(current_url, location)
}

fn too_many_redirects_error<T>() -> Result<T, DownloadError> {
    Err(DownloadError::HttpStatus {
        status: StatusCode::LOOP_DETECTED.as_u16(),
        message: "too many redirects".into(),
    })
}

fn is_stale_redirect_target_error(error: &DownloadError) -> bool {
    matches!(
        error,
        DownloadError::HttpStatus {
            status: 403 | 404,
            ..
        }
    )
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
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
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
        let redirect = warp::path("redirect")
            .map(|| warp::redirect::temporary(warp::http::Uri::from_static("/target")));
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

        assert_eq!(
            worker.final_url().await.unwrap(),
            format!("http://{addr}/target")
        );
    }

    #[tokio::test]
    async fn send_range_refreshes_stale_redirect_target_after_404() {
        let active_asset = Arc::new(AtomicUsize::new(1));
        let active_for_redirect = active_asset.clone();
        let active_for_asset = active_asset.clone();

        let release = warp::path("release").map(move || {
            let active = active_for_redirect.load(Ordering::SeqCst);
            let location = format!("/asset/{active}");
            warp::http::Response::builder()
                .status(302)
                .header("location", location)
                .body(Vec::<u8>::new())
                .unwrap()
        });
        let asset = warp::path!("asset" / usize)
            .and(warp::header::optional::<String>("range"))
            .map(move |asset_id: usize, _range: Option<String>| {
                if asset_id != active_for_asset.load(Ordering::SeqCst) {
                    return warp::http::Response::builder()
                        .status(404)
                        .body(Vec::<u8>::new())
                        .unwrap();
                }

                active_for_asset.store(asset_id + 1, Ordering::SeqCst);
                warp::http::Response::builder()
                    .status(206)
                    .header("content-length", "3")
                    .header("content-range", "bytes 0-2/3")
                    .header("accept-ranges", "bytes")
                    .body(b"abc".to_vec())
                    .unwrap()
            });
        let routes = release.or(asset);
        let (addr, server) = warp::serve(routes).bind_ephemeral(([127, 0, 0, 1], 0));
        tokio::spawn(server);

        let worker = worker_for(format!("http://{addr}/release"));
        let (_, meta) = worker.send_range(0, 2).await.unwrap();
        assert_eq!(meta.content_range_total, Some(3));

        let (_, meta) = worker.send_range(0, 2).await.unwrap();
        assert_eq!(meta.content_range_total, Some(3));
        assert_eq!(
            worker.final_url().await.unwrap(),
            format!("http://{addr}/asset/2")
        );
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

    #[tokio::test]
    async fn send_get_fails_with_too_many_redirects() {
        let route = warp::path("loop-redirect").map(|| {
            warp::http::Response::builder()
                .status(302)
                .header("location", "/loop-redirect")
                .body(Vec::<u8>::new())
                .unwrap()
        });
        let (addr, server) = warp::serve(route).bind_ephemeral(([127, 0, 0, 1], 0));
        tokio::spawn(server);

        let worker = worker_for(format!("http://{addr}/loop-redirect"));
        let err = worker.send_get().await.unwrap_err();
        match err {
            DownloadError::HttpStatus {
                status,
                ref message,
            } => {
                assert_eq!(status, StatusCode::LOOP_DETECTED.as_u16());
                assert!(message.contains("too many redirects"), "msg: {message}");
            }
            other => panic!("expected HttpStatus LOOP_DETECTED, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn send_get_fails_when_redirect_missing_location_header() {
        let route = warp::path("no-location").map(|| {
            warp::http::Response::builder()
                .status(301)
                .body(Vec::<u8>::new())
                .unwrap()
        });
        let (addr, server) = warp::serve(route).bind_ephemeral(([127, 0, 0, 1], 0));
        tokio::spawn(server);

        let worker = worker_for(format!("http://{addr}/no-location"));
        let err = worker.send_get().await.unwrap_err();
        match err {
            DownloadError::HttpStatus {
                status,
                ref message,
            } => {
                assert_eq!(status, 301);
                assert!(
                    message.contains("missing Location header"),
                    "msg: {message}"
                );
            }
            other => panic!("expected HttpStatus 301 missing Location, got {other:?}"),
        }
    }

    #[test]
    fn resolve_redirect_target_rejects_unparseable_location() {
        let err = resolve_redirect_target("https://example.com/", "https://[::1")
            .unwrap_err()
            .to_string();
        assert!(err.contains("invalid redirect location"), "got: {err}");
    }

    #[test]
    fn resolve_redirect_target_rejects_invalid_base_url() {
        let err = resolve_redirect_target("not a url", "/target")
            .unwrap_err()
            .to_string();
        assert!(err.contains("invalid redirect base URL"), "got: {err}");
    }

    #[test]
    fn resolve_redirect_target_preserves_literal_relative_path() {
        let resolved = resolve_redirect_target("https://example.com/file", "%zz").unwrap();
        assert_eq!(resolved, "https://example.com/%zz");
    }
}
