pub(crate) mod request;
pub(crate) mod response;
pub(crate) mod worker;

use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Empty};
use hyper::body::Incoming;
use hyper::Response;

use crate::error::{DownloadError, TransportError};

pub(crate) type HttpRequestBody = Empty<Bytes>;
pub(crate) type HttpResponse = Response<Incoming>;

pub(crate) async fn next_data_chunk(
    body: &mut Incoming,
    read_timeout: Duration,
) -> Result<Option<Bytes>, DownloadError> {
    loop {
        let frame = tokio::time::timeout(read_timeout, body.frame())
            .await
            .map_err(|_| DownloadError::timeout("response body timed out"))?;

        match frame {
            Some(Ok(frame)) => match frame.into_data() {
                Ok(data) => return Ok(Some(data)),
                Err(_) => continue,
            },
            Some(Err(error)) => return Err(TransportError::body(error).into()),
            None => return Ok(None),
        }
    }
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
        let request = hyper::Request::builder()
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
