//! Response body owned by the libcurl transport (P1/P2 of the migration plan).
//!
//! Sessions read response bytes through this type only, so libcurl stays behind
//! one internal seam instead of leaking driver handles into the download logic.
//!
//! The transport-body contract is:
//!
//! - chunks are delivered in order and `Ok(None)` is a clean EOF;
//! - a transport failure after the headers keeps its original cause instead of
//!   degrading into a short, apparently successful body;
//! - already-queued valid bytes are delivered before the terminal error;
//! - every read is bounded by the caller's `read_timeout`;
//! - dropping the body cancels the underlying transfer.

use std::time::Duration;

use bytes::Bytes;

use crate::error::DownloadError;

/// Streaming response body returned by the libcurl transport.
pub(crate) enum HttpBody {
    #[cfg(feature = "curl-backend")]
    Curl(crate::network::curl::driver::BodyStream),
}

impl HttpBody {
    /// Await the next data chunk, skipping non-data frames such as trailers.
    ///
    /// The wait is what the pipeline harness reports as body time: it is the
    /// time the transport took to deliver bytes, without the time the caller
    /// spent writing the previous chunk. Collection is off by default, so this
    /// costs one relaxed branch per read.
    pub(crate) async fn next_chunk(
        &mut self,
        read_timeout: Duration,
    ) -> Result<Option<Bytes>, DownloadError> {
        let started = crate::bench_stats::phase_start();
        let result = match self {
            #[cfg(feature = "curl-backend")]
            Self::Curl(body) => body.next_chunk(read_timeout).await,
        };
        crate::bench_stats::record_phase(started, crate::bench_stats::record_body_read);
        result
    }
}

impl std::fmt::Debug for HttpBody {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            #[cfg(feature = "curl-backend")]
            Self::Curl(_) => formatter.write_str("HttpBody::Curl"),
        }
    }
}

#[cfg(test)]
impl HttpBody {
    /// Drain the body for tests that only assert on the received payload.
    pub(crate) async fn collect_to_bytes(self) -> Result<Bytes, DownloadError> {
        let mut collected = Vec::new();
        let mut body = self;
        while let Some(chunk) = body.next_chunk(Duration::from_secs(5)).await? {
            collected.extend_from_slice(&chunk);
        }
        Ok(Bytes::from(collected))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::TransportErrorKind;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    /// Serve one raw scripted response to one client.
    async fn scripted_body(response: &'static [u8], delay: Duration) -> HttpBody {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            read_request(&mut stream).await;
            stream.write_all(response).await.unwrap();
            if delay > Duration::ZERO {
                tokio::time::sleep(delay).await;
            }
            stream.shutdown().await.unwrap();
        });

        let client = crate::network::ClientNetworkConfig::default()
            .build_client()
            .unwrap();
        let request = http::Request::builder()
            .method("GET")
            .uri(format!("http://{address}/body"))
            .body(crate::http::HttpRequestBody::new())
            .unwrap();
        let response = client.request(request).await.unwrap();
        drop(server);
        response.into_body()
    }

    async fn read_request(stream: &mut TcpStream) {
        let mut request = Vec::new();
        let mut buffer = [0u8; 1024];
        loop {
            let read = stream.read(&mut buffer).await.unwrap();
            assert!(read > 0, "client closed before sending a request");
            request.extend_from_slice(&buffer[..read]);
            if request.windows(4).any(|window| window == b"\r\n\r\n") {
                return;
            }
        }
    }

    #[tokio::test]
    async fn test_chunked_trailers_are_skipped_and_eof_is_clean() {
        let mut body = scripted_body(
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n\
              3\r\nabc\r\n0\r\nX-Checksum: 1\r\n\r\n",
            Duration::ZERO,
        )
        .await;

        let first = body.next_chunk(Duration::from_secs(1)).await.unwrap();
        assert_eq!(first.as_deref(), Some(&b"abc"[..]));
        // The trailer frame must not surface as data or as an error.
        assert!(body
            .next_chunk(Duration::from_secs(1))
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn test_stalled_body_reports_a_timeout_kind() {
        let mut body = scripted_body(
            b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\n",
            Duration::from_millis(500),
        )
        .await;

        let error = body
            .next_chunk(Duration::from_millis(50))
            .await
            .unwrap_err();
        match error {
            DownloadError::Transport(transport) => {
                assert_eq!(transport.kind(), TransportErrorKind::Timeout);
            }
            other => panic!("expected a timeout transport error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_truncated_body_keeps_its_cause_after_valid_bytes() {
        let mut body = scripted_body(
            b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhe",
            Duration::ZERO,
        )
        .await;

        let first = body.next_chunk(Duration::from_secs(1)).await.unwrap();
        assert_eq!(first.as_deref(), Some(&b"he"[..]));

        let error = body.next_chunk(Duration::from_secs(1)).await.unwrap_err();
        match error {
            DownloadError::Transport(transport) => {
                assert_eq!(transport.kind(), TransportErrorKind::Body);
                assert!(DownloadError::Transport(transport).is_retryable());
            }
            other => panic!("expected a body transport error, got {other:?}"),
        }
    }
}
