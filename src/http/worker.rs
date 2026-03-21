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
        let req = request::build_get_request(&self.client, &self.url, &self.headers, self.timeout);
        let response = req.send().await?;

        let status = response.status();
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
        let req =
            request::build_range_request(&self.client, &self.url, &self.headers, self.timeout, start, end);
        let response = req.send().await?;

        let status = response.status();
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
