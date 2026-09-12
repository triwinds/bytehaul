use bytehaul::{DownloadError, DownloadSpec, Downloader, FileAllocation};
use std::{sync::Arc, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::TcpListener,
    sync::mpsc,
    task::{JoinHandle, JoinSet},
};

// Header stalls are event-controlled: they never release naturally. Deadlines
// only bound deadlocks; no test asserts a benchmark speedup.
enum Reply {
    Hold,
    Bytes(String),
    SlowBody,
    SlowHeaders,
}
struct Server {
    url: String,
    requests: mpsc::UnboundedReceiver<String>,
    task: JoinHandle<()>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
async fn server(reply: impl Fn(usize, &str) -> Reply + Send + Sync + 'static) -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/file", listener.local_addr().unwrap());
    let (tx, requests) = mpsc::unbounded_channel();
    let reply = Arc::new(reply);
    let task = tokio::spawn(async move {
        let mut clients = JoinSet::new();
        let mut index = 0;
        loop {
            tokio::select! {
                result = listener.accept() => {
                    let (socket, _) = result.unwrap();
                    let tx = tx.clone();
                    let reply = reply.clone();
                    let id = index;
                    index += 1;
                    clients.spawn(async move {
                        let mut socket = BufReader::new(socket);
                        let mut request = String::new();
                        loop {
                            let mut line = String::new();
                            if socket.read_line(&mut line).await.unwrap() == 0 { return; }
                            request.push_str(&line);
                            if line == "\r\n" { break; }
                        }
                        tx.send(request.clone()).unwrap();
                        match reply(id, &request) {
                            Reply::Hold => std::future::pending::<()>().await,
                            Reply::Bytes(bytes) => { let _ = socket.get_mut().write_all(bytes.as_bytes()).await; }
                            Reply::SlowHeaders => {
                                tokio::time::sleep(Duration::from_millis(150)).await;
                                let _ = socket.get_mut().write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\ndone").await;
                            }
                            Reply::SlowBody => {
                                socket.get_mut().write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\n").await.unwrap();
                                tokio::time::sleep(Duration::from_millis(150)).await;
                                let _ = socket.get_mut().write_all(b"done").await;
                            }
                        }
                    });
                }
                _ = clients.join_next(), if !clients.is_empty() => {}
            }
        }
    });
    Server {
        url,
        requests,
        task,
    }
}
fn spec(server: &Server, path: &std::path::Path) -> DownloadSpec {
    DownloadSpec::new(&server.url)
        .output_path(path)
        .resume(false)
        .file_allocation(FileAllocation::None)
        .max_connections(1)
        .max_retries(0)
        .piece_size(4)
        .min_split_size(1)
        .read_timeout(Duration::from_secs(5))
        .request_headers_timeout(Duration::from_millis(50))
}
fn assert_timeout(error: DownloadError) {
    match error {
        DownloadError::Transport(error) => assert_eq!(
            error.to_string(),
            "timeout transport error: request timed out"
        ),
        other => panic!("expected transport timeout, got {other:?}"),
    }
}
async fn wait(handle: bytehaul::DownloadHandle) -> Result<(), DownloadError> {
    tokio::time::timeout(Duration::from_secs(5), handle.wait())
        .await
        .expect("download deadlocked")
}

#[tokio::test]
async fn fresh_probe_and_plain_get_have_header_deadlines() {
    for connections in [1, 4] {
        let mut server = server(|_, _| Reply::Hold).await;
        let temp = tempfile::tempdir().unwrap();
        let handle = Downloader::builder()
            .build()
            .unwrap()
            .download(spec(&server, &temp.path().join("out")).max_connections(connections));
        assert_timeout(wait(handle).await.unwrap_err());
        let request = server.requests.recv().await.unwrap();
        assert_eq!(
            request.to_ascii_lowercase().contains("range: bytes=0-3"),
            connections > 1
        );
        if connections > 1 {
            // Existing transport-failure probe fallback gets its own request deadline.
            let fallback = server.requests.recv().await.unwrap().to_ascii_lowercase();
            assert!(!fallback.contains("range:"));
        }
        assert!(server.requests.try_recv().is_err());
    }
}

#[tokio::test]
async fn subsequent_range_headers_have_separate_deadline() {
    let mut server = server(|index, _| if index == 0 {
        Reply::Bytes("HTTP/1.1 206 Partial Content\r\nContent-Length: 4\r\nContent-Range: bytes 0-3/16\r\nETag: \"v1\"\r\nConnection: close\r\n\r\nabcd".into())
    } else { Reply::Hold }).await;
    let temp = tempfile::tempdir().unwrap();
    let handle = Downloader::builder()
        .build()
        .unwrap()
        .download(spec(&server, &temp.path().join("out")).max_connections(2));
    assert_timeout(wait(handle).await.unwrap_err());
    assert!(server
        .requests
        .recv()
        .await
        .unwrap()
        .to_ascii_lowercase()
        .contains("range: bytes=0-3"));
    assert!(server
        .requests
        .recv()
        .await
        .unwrap()
        .to_ascii_lowercase()
        .contains("range:"));
}

#[tokio::test]
async fn redirect_hop_and_get_fallback_have_header_deadlines() {
    for redirect in [false, true] {
        let mut server = server(move |index, _| if index == 0 {
            Reply::Bytes(if redirect {
                "HTTP/1.1 302 Found\r\nLocation: /target\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into()
            } else {
                "HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into()
            })
        } else { Reply::Hold }).await;
        let temp = tempfile::tempdir().unwrap();
        let handle = Downloader::builder()
            .build()
            .unwrap()
            .download(spec(&server, &temp.path().join("out")).max_connections(4));
        assert_timeout(wait(handle).await.unwrap_err());
        server.requests.recv().await.unwrap();
        let second = server.requests.recv().await.unwrap();
        if redirect {
            assert!(second.starts_with("GET /target "));
        } else {
            assert!(!second.to_ascii_lowercase().contains("range:"));
        }
    }
}

#[tokio::test]
async fn headers_deadline_does_not_limit_body_and_default_keeps_read_timeout() {
    let server = server(|_, _| Reply::SlowBody).await;
    let temp = tempfile::tempdir().unwrap();
    let output = temp.path().join("out");
    wait(
        Downloader::builder()
            .build()
            .unwrap()
            .download(spec(&server, &output)),
    )
    .await
    .unwrap();
    assert_eq!(tokio::fs::read(&output).await.unwrap(), b"done");
    assert_eq!(
        DownloadSpec::new(&server.url).get_request_headers_timeout(),
        None
    );
}

#[tokio::test]
async fn cancellation_interrupts_header_wait() {
    let mut server = server(|_, _| Reply::Hold).await;
    let temp = tempfile::tempdir().unwrap();
    let handle = Downloader::builder().build().unwrap().download(
        spec(&server, &temp.path().join("out")).request_headers_timeout(Duration::from_secs(60)),
    );
    tokio::time::timeout(Duration::from_secs(5), server.requests.recv())
        .await
        .unwrap()
        .unwrap();
    handle.cancel();
    assert!(matches!(wait(handle).await, Err(DownloadError::Cancelled)));
}

#[tokio::test]
async fn retry_after_is_not_bypassed_by_header_deadline() {
    for status in [429, 503] {
        let mut server = server(move |_, _| Reply::Bytes(format!("HTTP/1.1 {status} Busy\r\nRetry-After: 60\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"))).await;
        let temp = tempfile::tempdir().unwrap();
        let handle = Downloader::builder().build().unwrap().download(
            spec(&server, &temp.path().join("out"))
                .max_connections(4)
                .max_retries(2)
                .max_retry_elapsed(Duration::from_millis(100)),
        );
        assert!(matches!(
            wait(handle).await,
            Err(DownloadError::RetryBudgetExceeded { .. })
        ));
        assert!(server
            .requests
            .recv()
            .await
            .unwrap()
            .to_ascii_lowercase()
            .contains("range:"));
        assert!(server.requests.try_recv().is_err());
    }
}

#[tokio::test]
async fn omitted_header_timeout_uses_existing_read_timeout() {
    let server = server(|_, _| Reply::SlowHeaders).await;
    let temp = tempfile::tempdir().unwrap();
    let output = temp.path().join("out");
    let config = DownloadSpec::new(&server.url)
        .output_path(&output)
        .resume(false)
        .max_connections(1)
        .max_retries(0)
        .read_timeout(Duration::from_secs(2));
    wait(
        Downloader::builder()
            .build()
            .unwrap()
            .download(config.clone()),
    )
    .await
    .unwrap();
    assert_eq!(tokio::fs::read(&output).await.unwrap(), b"done");
    assert_timeout(
        wait(
            Downloader::builder()
                .build()
                .unwrap()
                .download(config.request_headers_timeout(Duration::from_millis(50))),
        )
        .await
        .unwrap_err(),
    );
}

#[tokio::test]
async fn explicit_headers_deadline_can_exceed_body_read_timeout() {
    let server = server(|_, _| Reply::SlowHeaders).await;
    let temp = tempfile::tempdir().unwrap();
    let output = temp.path().join("out");
    let config = spec(&server, &output)
        .read_timeout(Duration::from_millis(50))
        .request_headers_timeout(Duration::from_secs(2));
    wait(Downloader::builder().build().unwrap().download(config))
        .await
        .unwrap();
    assert_eq!(tokio::fs::read(&output).await.unwrap(), b"done");
}

#[tokio::test]
async fn resumed_download_probe_uses_header_deadline() {
    let mut server = server(|index, _| if index == 0 {
        // Flush a partial single-stream body, then leave a resumable checkpoint.
        Reply::Bytes("HTTP/1.1 200 OK\r\nContent-Length: 16\r\nETag: \"v1\"\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\nabcd".into())
    } else { Reply::Hold }).await;
    let temp = tempfile::tempdir().unwrap();
    let config = spec(&server, &temp.path().join("out")).resume(true);
    let first = wait(
        Downloader::builder()
            .build()
            .unwrap()
            .download(config.clone()),
    )
    .await;
    assert!(matches!(first, Err(DownloadError::Transport(_))));
    server.requests.recv().await.unwrap();
    assert_timeout(
        wait(Downloader::builder().build().unwrap().download(config))
            .await
            .unwrap_err(),
    );
    let resumed = server.requests.recv().await.unwrap().to_ascii_lowercase();
    assert!(resumed.contains("range: bytes=4-15"), "{resumed}");
}

#[tokio::test]
async fn header_deadline_includes_tls_handshake() {
    // TCP accepts, but the peer never completes TLS. No certificate validation
    // override is needed: the deadline must expire before a certificate arrives.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (accepted, arrived) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (_socket, _) = listener.accept().await.unwrap();
        accepted.send(()).unwrap();
        std::future::pending::<()>().await;
    });
    let temp = tempfile::tempdir().unwrap();
    let config = DownloadSpec::new(format!("https://{address}/file"))
        .output_path(temp.path().join("out"))
        .max_connections(1)
        .max_retries(0)
        .resume(false)
        .connect_timeout(Duration::from_secs(30))
        .request_headers_timeout(Duration::from_millis(100));
    let result = wait(Downloader::builder().build().unwrap().download(config)).await;
    server.abort();
    arrived.await.unwrap();
    assert_timeout(result.unwrap_err());
}
