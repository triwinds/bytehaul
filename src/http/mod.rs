mod body;
pub(crate) mod request;
pub(crate) mod response;
pub(crate) mod worker;

use std::time::Duration;

use bytes::Bytes;

use crate::error::DownloadError;

pub(crate) use body::HttpBody;

/// Request body of every request bytehaul sends: GET/HEAD carry no payload.
///
/// Owning the type keeps the request side independent of libcurl handles.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HttpRequestBody;

impl HttpRequestBody {
    pub(crate) fn new() -> Self {
        Self
    }
}

/// Neutral response shared by the worker and the sessions: `http` types plus
/// the backend-owned streaming body.
pub(crate) type HttpResponse = http::Response<HttpBody>;

/// Direct multi-IP routing identity pinned by the transport. Absent for
/// proxies and unpinned requests; never inferred from a server header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PinnedOriginIp(pub std::net::IpAddr);

/// Request extension: how many body bytes the transport may buffer for this
/// transfer before it has to stop reading from the network.
///
/// The session derives it from its own `MemoryBudget` (see
/// `session::flow::MemoryBudget::transport_body_budget`), which is what keeps
/// the bytes queued inside the transport inside the memory the session promised
/// to bound. The libcurl transport uses the hint to bound its callback queue.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct BodyBudget(pub(crate) usize);

/// Request identifier copied into transport diagnostics so a completed
/// libcurl transfer can be joined back to the worker's Range request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RequestTraceId(pub(crate) u64);

/// Largest byte budget a transport may buffer for one transfer.
///
/// libcurl delivers at most 16 KiB per write callback, so this always leaves
/// room for a complete callback: a paused transfer can still make progress once
/// the consumer drains its queue.
pub(crate) const MAX_BODY_BUDGET_BYTES: usize = 256 * 1024;

/// Smallest byte budget a transport accepts for one transfer.
///
/// A session budget can be far smaller than one libcurl callback; the transport
/// still keeps a few callbacks of headroom so a tiny budget does not turn every
/// chunk into a pause/unpause round trip. The queue stays bounded either way,
/// and the write callback accepts a whole chunk whenever the queue is empty, so
/// progress never depends on this floor.
pub(crate) const MIN_BODY_BUDGET_BYTES: usize = 64 * 1024;

impl BodyBudget {
    /// Per-transfer transport budget for a session with `memory_budget` bytes
    /// and at most `transfers` bodies in flight at the same time.
    pub(crate) fn for_session(memory_budget: usize, transfers: usize) -> Self {
        Self((memory_budget / transfers.max(1)).clamp(MIN_BODY_BUDGET_BYTES, MAX_BODY_BUDGET_BYTES))
    }
}

/// Read the next data chunk of a response body, bounded by the session's own
/// wait between chunks. `Ok(None)` is a clean EOF; a failure keeps its cause.
pub(crate) async fn next_data_chunk(
    body: &mut HttpBody,
    read_timeout: Duration,
) -> Result<Option<Bytes>, DownloadError> {
    body.next_chunk(read_timeout).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::TransportErrorKind;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn test_body_read_errors_are_retryable() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buffer = [0u8; 1024];

            loop {
                let read = stream.read(&mut buffer).await.unwrap();
                assert!(read > 0, "client closed before sending a request");
                request.extend_from_slice(&buffer[..read]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }

            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhe")
                .await
                .unwrap();
            stream.shutdown().await.unwrap();
        });

        let client = crate::network::ClientNetworkConfig::default()
            .build_client()
            .unwrap();
        let request = http::Request::builder()
            .method("GET")
            .uri(format!("http://{address}/body"))
            .body(HttpRequestBody::new())
            .unwrap();
        let mut response = client.request(request).await.unwrap();

        let chunk = next_data_chunk(response.body_mut(), Duration::from_secs(1))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&chunk[..], b"he");

        let error = next_data_chunk(response.body_mut(), Duration::from_secs(1))
            .await
            .unwrap_err();
        match error {
            DownloadError::Transport(transport) => {
                assert_eq!(transport.kind(), TransportErrorKind::Body);
                assert!(DownloadError::Transport(transport).is_retryable());
            }
            other => panic!("expected a transport error, got {other:?}"),
        }

        server.await.unwrap();
    }
}
