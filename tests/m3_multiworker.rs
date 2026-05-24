use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use bytehaul::{DownloadSpec, DownloadState, Downloader, FileAllocation, LogLevel};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use warp::Filter;

#[derive(Debug, Clone, PartialEq, Eq)]
struct RequestEvent {
    connection_id: usize,
    range_header: Option<String>,
}

#[derive(Debug, Default)]
struct RequestLog {
    next_connection_id: AtomicUsize,
    events: Mutex<Vec<RequestEvent>>,
}

impl RequestLog {
    fn allocate_connection_id(&self) -> usize {
        self.next_connection_id.fetch_add(1, Ordering::Relaxed)
    }

    fn record(&self, connection_id: usize, range_header: Option<String>) {
        self.events.lock().unwrap().push(RequestEvent {
            connection_id,
            range_header,
        });
    }

    fn snapshot(&self) -> Vec<RequestEvent> {
        self.events.lock().unwrap().clone()
    }
}

#[derive(Debug)]
struct ParsedRequest {
    path: String,
    range_header: Option<String>,
}

fn spawn_connection_counting_range_server(
    path_segment: &'static str,
    data: Vec<u8>,
) -> (
    std::net::SocketAddr,
    Arc<RequestLog>,
    oneshot::Sender<()>,
    tokio::task::JoinHandle<()>,
) {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let addr = listener.local_addr().unwrap();
    let listener = TcpListener::from_std(listener).unwrap();
    let data = Arc::new(data);
    let request_log = Arc::new(RequestLog::default());
    let (shutdown_tx, mut shutdown_rx) = oneshot::channel();

    let server_log = request_log.clone();
    let server = tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut shutdown_rx => break,
                accepted = listener.accept() => {
                    let Ok((socket, _peer_addr)) = accepted else {
                        break;
                    };
                    let data = data.clone();
                    let request_log = server_log.clone();
                    let connection_id = request_log.allocate_connection_id();
                    tokio::spawn(async move {
                        let _ = handle_counting_connection(
                            socket,
                            connection_id,
                            path_segment,
                            data,
                            request_log,
                        )
                        .await;
                    });
                }
            }
        }
    });

    (addr, request_log, shutdown_tx, server)
}

async fn handle_counting_connection(
    socket: tokio::net::TcpStream,
    connection_id: usize,
    path_segment: &'static str,
    data: Arc<Vec<u8>>,
    request_log: Arc<RequestLog>,
) -> std::io::Result<()> {
    let (read_half, mut write_half) = socket.into_split();
    let mut reader = BufReader::new(read_half);

    loop {
        let Some(request) = read_http_request(&mut reader).await? else {
            return Ok(());
        };

        request_log.record(connection_id, request.range_header.clone());

        let response = build_counting_response(&request, path_segment, &data);
        write_half.write_all(&response).await?;
        write_half.flush().await?;
    }
}

async fn read_http_request<R>(reader: &mut R) -> std::io::Result<Option<ParsedRequest>>
where
    R: AsyncBufRead + Unpin,
{
    let mut request_line = String::new();
    loop {
        request_line.clear();
        let read = reader.read_line(&mut request_line).await?;
        if read == 0 {
            return Ok(None);
        }
        if request_line != "\r\n" {
            break;
        }
    }

    let mut parts = request_line.split_whitespace();
    let _method = parts.next();
    let path = parts.next().unwrap_or_default().to_string();
    let mut range_header = None;

    loop {
        let mut header_line = String::new();
        let read = reader.read_line(&mut header_line).await?;
        if read == 0 {
            return Ok(None);
        }
        let trimmed = header_line.trim_end();
        if trimmed.is_empty() {
            break;
        }
        if let Some((name, value)) = trimmed.split_once(':') {
            if name.eq_ignore_ascii_case("range") {
                range_header = Some(value.trim().to_string());
            }
        }
    }

    Ok(Some(ParsedRequest { path, range_header }))
}

fn build_counting_response(request: &ParsedRequest, path_segment: &str, data: &[u8]) -> Vec<u8> {
    let expected_path = format!("/{path_segment}");
    if request.path != expected_path {
        return concat!(
            "HTTP/1.1 404 Not Found\r\n",
            "Content-Length: 0\r\n",
            "Connection: keep-alive\r\n\r\n",
        )
        .as_bytes()
        .to_vec();
    }

    match request.range_header.as_deref() {
        Some(range_header) => match parse_range_header(range_header, data.len()) {
            Some((start, end)) => {
                let body = &data[start..=end];
                let mut response = format!(
                    concat!(
                        "HTTP/1.1 206 Partial Content\r\n",
                        "Content-Length: {}\r\n",
                        "Content-Range: bytes {}-{}/{}\r\n",
                        "Accept-Ranges: bytes\r\n",
                        "ETag: \"conn-count\"\r\n",
                        "Last-Modified: Sat, 01 Jan 2026 00:00:00 GMT\r\n",
                        "Connection: keep-alive\r\n\r\n",
                    ),
                    body.len(),
                    start,
                    end,
                    data.len(),
                )
                .into_bytes();
                response.extend_from_slice(body);
                response
            }
            None => format!(
                concat!(
                    "HTTP/1.1 416 Range Not Satisfiable\r\n",
                    "Content-Length: 0\r\n",
                    "Content-Range: bytes */{}\r\n",
                    "Connection: keep-alive\r\n\r\n",
                ),
                data.len(),
            )
            .into_bytes(),
        },
        None => concat!(
            "HTTP/1.1 200 OK\r\n",
            "Content-Length: 0\r\n",
            "Accept-Ranges: bytes\r\n",
            "ETag: \"conn-count\"\r\n",
            "Last-Modified: Sat, 01 Jan 2026 00:00:00 GMT\r\n",
            "Connection: keep-alive\r\n\r\n",
        )
        .as_bytes()
        .to_vec(),
    }
}

fn parse_range_header(range_header: &str, total_len: usize) -> Option<(usize, usize)> {
    let range = range_header.strip_prefix("bytes=")?;
    let (start, end) = range.split_once('-')?;
    let start = start.parse::<usize>().ok()?;
    let end = if end.is_empty() {
        total_len.checked_sub(1)?
    } else {
        end.parse::<usize>().ok()?.min(total_len.checked_sub(1)?)
    };
    if start > end || end >= total_len {
        return None;
    }
    Some((start, end))
}

/// Test server that supports Range requests for a known file.
fn range_file_server(
    path_segment: &'static str,
    data: Vec<u8>,
) -> (std::net::SocketAddr, impl std::future::Future<Output = ()>) {
    let data = Arc::new(data);
    let d = data.clone();

    let route = warp::path(path_segment)
        .and(warp::header::optional::<String>("range"))
        .map(move |range_header: Option<String>| {
            let data = d.clone();
            let total = data.len();

            match range_header {
                Some(range) => {
                    let range = range.trim_start_matches("bytes=");
                    let parts: Vec<&str> = range.split('-').collect();
                    let start: u64 = parts[0].parse().unwrap_or(0);
                    let end: u64 = if parts.len() > 1 && !parts[1].is_empty() {
                        parts[1]
                            .parse::<u64>()
                            .unwrap_or(total as u64 - 1)
                            .min(total as u64 - 1)
                    } else {
                        total as u64 - 1
                    };
                    let slice = &data[start as usize..=end as usize];
                    warp::http::Response::builder()
                        .status(206)
                        .header("content-length", slice.len().to_string())
                        .header(
                            "content-range",
                            format!("bytes {}-{}/{}", start, end, total),
                        )
                        .header("accept-ranges", "bytes")
                        .header("etag", "\"multitest\"")
                        .header("last-modified", "Sat, 01 Jan 2026 00:00:00 GMT")
                        .body(Vec::from(slice))
                        .unwrap()
                }
                None => warp::http::Response::builder()
                    .status(200)
                    .header("content-length", total.to_string())
                    .header("accept-ranges", "bytes")
                    .header("etag", "\"multitest\"")
                    .header("last-modified", "Sat, 01 Jan 2026 00:00:00 GMT")
                    .body(data.to_vec())
                    .unwrap(),
            }
        });

    warp::serve(route).bind_ephemeral(([127, 0, 0, 1], 0))
}

#[derive(Clone, Copy)]
enum MalformedRangeMode {
    ShortBody,
    LongBody,
}

fn malformed_range_server(
    path_segment: &'static str,
    data: Vec<u8>,
    mode: MalformedRangeMode,
) -> (std::net::SocketAddr, impl std::future::Future<Output = ()>) {
    let data = Arc::new(data);
    let first_range = Arc::new(AtomicUsize::new(0));
    let d = data.clone();
    let f = first_range.clone();

    let route = warp::path(path_segment)
        .and(warp::header::optional::<String>("range"))
        .map(move |range_header: Option<String>| {
            let data = d.clone();
            let total = data.len();

            match range_header {
                Some(range) => {
                    let range = range.trim_start_matches("bytes=");
                    let parts: Vec<&str> = range.split('-').collect();
                    let start: u64 = parts[0].parse().unwrap_or(0);
                    let end: u64 = if parts.len() > 1 && !parts[1].is_empty() {
                        parts[1]
                            .parse::<u64>()
                            .unwrap_or(total as u64 - 1)
                            .min(total as u64 - 1)
                    } else {
                        total as u64 - 1
                    };
                    let slice = &data[start as usize..=end as usize];
                    let should_mangle = f.fetch_add(1, Ordering::SeqCst) == 0;
                    let body = if should_mangle {
                        match mode {
                            MalformedRangeMode::ShortBody => slice[..slice.len() - 1].to_vec(),
                            MalformedRangeMode::LongBody => {
                                let mut body = slice.to_vec();
                                body.push(0xFF);
                                body
                            }
                        }
                    } else {
                        slice.to_vec()
                    };

                    warp::http::Response::builder()
                        .status(206)
                        .header("content-length", body.len().to_string())
                        .header(
                            "content-range",
                            format!("bytes {}-{}/{}", start, end, total),
                        )
                        .header("accept-ranges", "bytes")
                        .body(body)
                        .unwrap()
                }
                None => warp::http::Response::builder()
                    .status(200)
                    .header("content-length", total.to_string())
                    .header("accept-ranges", "bytes")
                    .body(data.to_vec())
                    .unwrap(),
            }
        });

    warp::serve(route).bind_ephemeral(([127, 0, 0, 1], 0))
}

fn truncated_once_range_server(
    path_segment: &'static str,
    data: Vec<u8>,
    truncate_start: usize,
) -> (std::net::SocketAddr, impl std::future::Future<Output = ()>) {
    let data = Arc::new(data);
    let truncated = Arc::new(AtomicUsize::new(0));
    let d = data.clone();
    let t = truncated.clone();

    let route = warp::path(path_segment)
        .and(warp::header::optional::<String>("range"))
        .map(move |range_header: Option<String>| {
            let data = d.clone();
            let total = data.len();

            match range_header {
                Some(range) => {
                    let range = range.trim_start_matches("bytes=");
                    let parts: Vec<&str> = range.split('-').collect();
                    let start: u64 = parts[0].parse().unwrap_or(0);
                    let end: u64 = if parts.len() > 1 && !parts[1].is_empty() {
                        parts[1]
                            .parse::<u64>()
                            .unwrap_or(total as u64 - 1)
                            .min(total as u64 - 1)
                    } else {
                        total as u64 - 1
                    };
                    let slice = &data[start as usize..=end as usize];
                    let should_truncate =
                        start as usize == truncate_start && t.fetch_add(1, Ordering::SeqCst) == 0;
                    let body = if should_truncate {
                        slice[..slice.len() / 2].to_vec()
                    } else {
                        slice.to_vec()
                    };

                    warp::http::Response::builder()
                        .status(206)
                        .header("content-length", body.len().to_string())
                        .header(
                            "content-range",
                            format!("bytes {}-{}/{}", start, end, total),
                        )
                        .header("accept-ranges", "bytes")
                        .body(body)
                        .unwrap()
                }
                None => warp::http::Response::builder()
                    .status(200)
                    .header("content-length", total.to_string())
                    .header("accept-ranges", "bytes")
                    .body(data.to_vec())
                    .unwrap(),
            }
        });

    warp::serve(route).bind_ephemeral(([127, 0, 0, 1], 0))
}

/// Test server that does NOT support Range (always returns 200 with full body).
fn no_range_server(
    path_segment: &'static str,
    data: Vec<u8>,
) -> (std::net::SocketAddr, impl std::future::Future<Output = ()>) {
    let data = Arc::new(data);
    let d = data.clone();

    let route = warp::path(path_segment).map(move || {
        let data = d.clone();
        warp::http::Response::builder()
            .status(200)
            .header("content-length", data.len().to_string())
            .body(data.to_vec())
            .unwrap()
    });

    warp::serve(route).bind_ephemeral(([127, 0, 0, 1], 0))
}

#[tokio::test]
async fn test_multi_worker_download() {
    // 20 MiB file with 1 MiB pieces → should trigger multi-worker (> 10 MiB min_split_size)
    let size = 20 * 1024 * 1024;
    let content: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
    let expected = content.clone();

    let (addr, server) = range_file_server("bigfile", content);
    tokio::spawn(server);

    let dir = tempfile::tempdir().unwrap();
    let output_path = dir.path().join("bigfile.bin");

    let downloader = Downloader::builder().build().unwrap();
    let spec = DownloadSpec::new(format!("http://{addr}/bigfile"))
        .output_path(output_path.clone())
        .file_allocation(FileAllocation::Prealloc)
        .max_connections(4)
        .piece_size(1024 * 1024)
        .min_split_size(10 * 1024 * 1024);

    let handle = downloader.download(spec);
    handle.wait().await.unwrap();

    let downloaded = std::fs::read(&output_path).unwrap();
    assert_eq!(downloaded.len(), expected.len());
    assert_eq!(downloaded, expected);

    // Control file should be cleaned up
    let ctrl_path = output_path.with_file_name("bigfile.bin.bytehaul");
    assert!(!ctrl_path.exists());
}

#[tokio::test]
async fn test_multi_worker_range_requests_use_distinct_connections() {
    let piece_size = 64 * 1024usize;
    let piece_count = 8usize;
    let size = piece_size * piece_count;
    let content: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();

    let (addr, request_log, shutdown_tx, server) =
        spawn_connection_counting_range_server("conncount", content.clone());

    let dir = tempfile::tempdir().unwrap();
    let output_path = dir.path().join("conncount.bin");

    let downloader = Downloader::builder().build().unwrap();
    let spec = DownloadSpec::new(format!("http://{addr}/conncount"))
        .output_path(output_path.clone())
        .file_allocation(FileAllocation::None)
        .max_connections(4)
        .piece_size(piece_size as u64)
        .min_split_size(1);

    let handle = downloader.download(spec);
    handle.wait().await.unwrap();

    let _ = shutdown_tx.send(());
    server.await.unwrap();

    let downloaded = std::fs::read(&output_path).unwrap();
    assert_eq!(downloaded, content);

    let events = request_log.snapshot();
    let range_events: Vec<_> = events
        .iter()
        .filter(|event| event.range_header.is_some())
        .cloned()
        .collect();
    let unique_range_connections: HashSet<_> = range_events
        .iter()
        .map(|event| event.connection_id)
        .collect();

    assert_eq!(range_events.len(), piece_count);
    assert_eq!(
        unique_range_connections.len(),
        range_events.len(),
        "range requests reused a TCP connection: {:?}",
        events
    );
}

#[tokio::test]
async fn test_multi_worker_dynamic_split_issues_subranges_with_single_piece() {
    let size = 256 * 1024usize;
    let content: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();

    let (addr, request_log, shutdown_tx, server) =
        spawn_connection_counting_range_server("dynamic-split", content.clone());

    let dir = tempfile::tempdir().unwrap();
    let output_path = dir.path().join("dynamic-split.bin");

    let downloader = Downloader::builder().build().unwrap();
    let spec = DownloadSpec::new(format!("http://{addr}/dynamic-split"))
        .output_path(output_path.clone())
        .file_allocation(FileAllocation::None)
        .max_connections(4)
        .piece_size(size as u64)
        .min_split_size(1)
        .min_segment_size((size / 4) as u64);

    let handle = downloader.download(spec);
    handle.wait().await.unwrap();

    let _ = shutdown_tx.send(());
    server.await.unwrap();

    let downloaded = std::fs::read(&output_path).unwrap();
    assert_eq!(downloaded, content);

    let expected_ranges: HashSet<_> = [
        format!("bytes=0-{}", size / 4 - 1),
        format!("bytes={}-{}", size / 4, size / 2 - 1),
        format!("bytes={}-{}", size / 2, size * 3 / 4 - 1),
        format!("bytes={}-{}", size * 3 / 4, size - 1),
    ]
    .into_iter()
    .collect();

    let observed_ranges: HashSet<_> = request_log
        .snapshot()
        .into_iter()
        .filter_map(|event| event.range_header)
        .collect();

    assert!(
        expected_ranges.is_subset(&observed_ranges),
        "dynamic split did not issue all expected subranges: {:?}",
        observed_ranges
    );
}

#[tokio::test]
async fn test_multi_worker_progress() {
    let size = 15 * 1024 * 1024;
    let content: Vec<u8> = (0..size).map(|i| (i % 199) as u8).collect();
    let expected_len = content.len() as u64;

    let (addr, server) = range_file_server("progressmulti", content);
    tokio::spawn(server);

    let dir = tempfile::tempdir().unwrap();
    let output_path = dir.path().join("progressmulti.bin");

    let downloader = Downloader::builder().build().unwrap();
    let spec = DownloadSpec::new(format!("http://{addr}/progressmulti"))
        .output_path(output_path.clone())
        .file_allocation(FileAllocation::None)
        .max_connections(4)
        .piece_size(1024 * 1024)
        .min_split_size(10 * 1024 * 1024);

    let handle = downloader.download(spec);
    let mut rx = handle.subscribe_progress();

    handle.wait().await.unwrap();

    let snap = rx.borrow_and_update().clone();
    assert_eq!(snap.state, DownloadState::Completed);
    assert_eq!(snap.downloaded, expected_len);
    assert_eq!(snap.total_size, Some(expected_len));
    assert_eq!(snap.eta_secs, Some(0.0));
}

#[tokio::test]
async fn test_multi_worker_rejects_short_initial_range_body() {
    let size = 12 * 1024 * 1024;
    let content: Vec<u8> = (0..size).map(|i| (i % 199) as u8).collect();
    let (addr, server) =
        malformed_range_server("short-initial", content, MalformedRangeMode::ShortBody);
    tokio::spawn(server);

    let dir = tempfile::tempdir().unwrap();
    let output_path = dir.path().join("short-initial.bin");

    let downloader = Downloader::builder().build().unwrap();
    let spec = DownloadSpec::new(format!("http://{addr}/short-initial"))
        .output_path(output_path)
        .file_allocation(FileAllocation::None)
        .max_connections(4)
        .piece_size(1024 * 1024)
        .min_split_size(1)
        .max_retries(0);

    let handle = downloader.download(spec);
    assert!(handle.wait().await.is_err());
}

#[tokio::test]
async fn test_multi_worker_rejects_long_initial_range_body() {
    let size = 12 * 1024 * 1024;
    let content: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
    let (addr, server) =
        malformed_range_server("long-initial", content, MalformedRangeMode::LongBody);
    tokio::spawn(server);

    let dir = tempfile::tempdir().unwrap();
    let output_path = dir.path().join("long-initial.bin");

    let downloader = Downloader::builder().build().unwrap();
    let spec = DownloadSpec::new(format!("http://{addr}/long-initial"))
        .output_path(output_path)
        .file_allocation(FileAllocation::None)
        .max_connections(4)
        .piece_size(1024 * 1024)
        .min_split_size(1)
        .max_retries(0);

    let handle = downloader.download(spec);
    assert!(handle.wait().await.is_err());
}

#[tokio::test]
async fn test_multi_worker_retries_truncated_segment_without_overcounting_progress() {
    let piece_size = 1024 * 1024usize;
    let size = 12 * piece_size;
    let content: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
    let expected = content.clone();
    let (addr, server) = truncated_once_range_server("retry-truncated", content, piece_size);
    tokio::spawn(server);

    let dir = tempfile::tempdir().unwrap();
    let output_path = dir.path().join("retry-truncated.bin");

    let downloader = Downloader::builder().build().unwrap();
    let spec = DownloadSpec::new(format!("http://{addr}/retry-truncated"))
        .output_path(output_path.clone())
        .file_allocation(FileAllocation::None)
        .max_connections(2)
        .piece_size(piece_size as u64)
        .min_split_size(1)
        .retry_policy(
            2,
            std::time::Duration::from_millis(1),
            std::time::Duration::from_millis(1),
        );

    let handle = downloader.download(spec);
    let rx = handle.subscribe_progress();
    handle.wait().await.unwrap();
    let snap = rx.borrow().clone();
    assert_eq!(snap.state, DownloadState::Completed);
    assert_eq!(snap.downloaded, size as u64);

    let downloaded = std::fs::read(&output_path).unwrap();
    assert_eq!(downloaded, expected);
}

#[tokio::test]
async fn test_multi_worker_eta_reports() {
    use futures::StreamExt;

    let size = 15 * 1024 * 1024;
    let content: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
    let data = Arc::new(content);

    let d = data.clone();
    let route = warp::path("eta-multi")
        .and(warp::header::optional::<String>("range"))
        .map(move |range_header: Option<String>| {
            let data = d.clone();
            let total = data.len();

            let (start, end) = match range_header {
                Some(range) => {
                    let range = range.trim_start_matches("bytes=");
                    let parts: Vec<&str> = range.split('-').collect();
                    let s: u64 = parts[0].parse().unwrap_or(0);
                    let e: u64 = if parts.len() > 1 && !parts[1].is_empty() {
                        parts[1]
                            .parse::<u64>()
                            .unwrap_or(total as u64 - 1)
                            .min(total as u64 - 1)
                    } else {
                        total as u64 - 1
                    };
                    (s, e)
                }
                None => (0, total as u64 - 1),
            };

            let slice = data[start as usize..=end as usize].to_vec();
            let chunks: Vec<Result<Vec<u8>, std::convert::Infallible>> = slice
                .chunks(32 * 1024)
                .map(|chunk| Ok(chunk.to_vec()))
                .collect();
            let stream = futures::stream::iter(chunks).then(
                |chunk: Result<Vec<u8>, std::convert::Infallible>| async move {
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    chunk
                },
            );
            let body = warp::hyper::Body::wrap_stream(stream);

            let is_range = start > 0 || end < total as u64 - 1;
            let status = if is_range { 206 } else { 200 };
            let mut builder = warp::http::Response::builder()
                .status(status)
                .header("content-length", (end - start + 1).to_string())
                .header("accept-ranges", "bytes")
                .header("etag", "\"eta-multi\"")
                .header("last-modified", "Sat, 01 Jan 2026 00:00:00 GMT");
            if is_range {
                builder = builder.header(
                    "content-range",
                    format!("bytes {}-{}/{}", start, end, total),
                );
            }
            builder.body(body).unwrap()
        });

    let (addr, server) = warp::serve(route).bind_ephemeral(([127, 0, 0, 1], 0));
    tokio::spawn(server);

    let dir = tempfile::tempdir().unwrap();
    let output_path = dir.path().join("eta-multi.bin");

    let downloader = Downloader::builder().build().unwrap();
    let spec = DownloadSpec::new(format!("http://{addr}/eta-multi"))
        .output_path(output_path.clone())
        .file_allocation(FileAllocation::None)
        .max_connections(4)
        .piece_size(1024 * 1024)
        .min_split_size(10 * 1024 * 1024);

    let handle = downloader.download(spec);
    let mut rx = handle.subscribe_progress();
    let mut saw_eta = false;
    let mut saw_speed = false;

    for _ in 0..30 {
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        let snap = rx.borrow_and_update().clone();
        if matches!(snap.state, DownloadState::Downloading) && snap.eta_secs.is_some() {
            saw_eta = true;
            saw_speed = snap.speed_bytes_per_sec > 0.0;
            break;
        }
    }

    handle.wait().await.unwrap();
    let final_snap = rx.borrow_and_update().clone();
    assert!(
        saw_eta,
        "eta should become available during multi-worker download"
    );
    assert!(
        saw_speed,
        "speed should be driven by the same recent samples as eta"
    );
    assert_eq!(final_snap.eta_secs, Some(0.0));
}

#[tokio::test]
async fn test_fallback_to_single_connection_no_range() {
    // Server doesn't support Range → should fall back to single connection
    let content: Vec<u8> = (0..50_000u32).map(|i| (i % 251) as u8).collect();
    let expected = content.clone();

    let (addr, server) = no_range_server("norange", content);
    tokio::spawn(server);

    let dir = tempfile::tempdir().unwrap();
    let output_path = dir.path().join("norange.bin");

    let downloader = Downloader::builder().build().unwrap();
    let spec = DownloadSpec::new(format!("http://{addr}/norange"))
        .output_path(output_path.clone())
        .file_allocation(FileAllocation::None)
        .max_connections(4);

    let handle = downloader.download(spec);
    handle.wait().await.unwrap();

    let downloaded = std::fs::read(&output_path).unwrap();
    assert_eq!(downloaded, expected);
}

#[tokio::test]
async fn test_small_file_uses_single_connection() {
    // File is smaller than min_split_size → single connection even with Range support
    let content: Vec<u8> = (0..500_000u32).map(|i| (i % 251) as u8).collect();
    let expected = content.clone();

    let (addr, server) = range_file_server("smallfile", content);
    tokio::spawn(server);

    let dir = tempfile::tempdir().unwrap();
    let output_path = dir.path().join("smallfile.bin");

    let downloader = Downloader::builder().build().unwrap();
    let spec = DownloadSpec::new(format!("http://{addr}/smallfile"))
        .output_path(output_path.clone())
        .file_allocation(FileAllocation::None)
        .max_connections(4)
        .min_split_size(10 * 1024 * 1024);

    let handle = downloader.download(spec);
    handle.wait().await.unwrap();

    let downloaded = std::fs::read(&output_path).unwrap();
    assert_eq!(downloaded, expected);
}

#[tokio::test]
async fn test_small_file_below_min_split_size_refetches_full_body() {
    // File stays below min_split_size, but the initial probe only covers the first piece.
    // Fresh fallback must refetch a full GET instead of reusing the partial 206 body.
    let content: Vec<u8> = (0..1_500_000u32).map(|i| (i % 251) as u8).collect();
    let expected = content.clone();

    let (addr, server) = range_file_server("small-partial-probe", content);
    tokio::spawn(server);

    let dir = tempfile::tempdir().unwrap();
    let output_path = dir.path().join("small-partial-probe.bin");

    let downloader = Downloader::builder().build().unwrap();
    let spec = DownloadSpec::new(format!("http://{addr}/small-partial-probe"))
        .output_path(output_path.clone())
        .file_allocation(FileAllocation::None)
        .max_connections(4)
        .piece_size(256 * 1024)
        .min_split_size(10 * 1024 * 1024);

    let handle = downloader.download(spec);
    handle.wait().await.unwrap();

    let downloaded = std::fs::read(&output_path).unwrap();
    assert_eq!(downloaded, expected);
}

#[tokio::test]
async fn test_multi_worker_resume_after_cancel() {
    use futures::StreamExt;

    // Slow streaming server for cancel testing
    let size = 15 * 1024 * 1024;
    let content: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
    let expected = content.clone();
    let data = Arc::new(content);

    let d = data.clone();
    let route = warp::path("slowmulti")
        .and(warp::header::optional::<String>("range"))
        .map(move |range_header: Option<String>| {
            let data = d.clone();
            let total = data.len();

            let (start, end) = match range_header {
                Some(range) => {
                    let range = range.trim_start_matches("bytes=");
                    let parts: Vec<&str> = range.split('-').collect();
                    let s: u64 = parts[0].parse().unwrap_or(0);
                    let e: u64 = if parts.len() > 1 && !parts[1].is_empty() {
                        parts[1]
                            .parse::<u64>()
                            .unwrap_or(total as u64 - 1)
                            .min(total as u64 - 1)
                    } else {
                        total as u64 - 1
                    };
                    (s, e)
                }
                None => (0, total as u64 - 1),
            };

            let slice = data[start as usize..=end as usize].to_vec();
            let chunk_size = 32 * 1024; // 32 KB chunks
            let chunks: Vec<Result<Vec<u8>, std::convert::Infallible>> =
                slice.chunks(chunk_size).map(|c| Ok(c.to_vec())).collect();
            let stream = futures::stream::iter(chunks).then(
                |chunk: Result<Vec<u8>, std::convert::Infallible>| async move {
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                    chunk
                },
            );
            let body = warp::hyper::Body::wrap_stream(stream);

            let is_range = start > 0 || end < total as u64 - 1;
            let status = if is_range { 206 } else { 200 };
            let mut builder = warp::http::Response::builder()
                .status(status)
                .header("content-length", (end - start + 1).to_string())
                .header("accept-ranges", "bytes")
                .header("etag", "\"slowmultitest\"")
                .header("last-modified", "Sat, 01 Jan 2026 00:00:00 GMT");
            if is_range {
                builder = builder.header(
                    "content-range",
                    format!("bytes {}-{}/{}", start, end, total),
                );
            }
            builder.body(body).unwrap()
        });

    let (addr, server) = warp::serve(route).bind_ephemeral(([127, 0, 0, 1], 0));
    tokio::spawn(server);

    let dir = tempfile::tempdir().unwrap();
    let output_path = dir.path().join("slowmulti.bin");
    let ctrl_path = output_path.with_file_name("slowmulti.bin.bytehaul");

    let downloader = Downloader::builder().build().unwrap();

    // First download: cancel after some data arrives
    let spec = DownloadSpec::new(format!("http://{addr}/slowmulti"))
        .output_path(output_path.clone())
        .file_allocation(FileAllocation::Prealloc)
        .max_connections(4)
        .piece_size(1024 * 1024)
        .min_split_size(10 * 1024 * 1024);

    let handle = downloader.download(spec.clone());
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    handle.cancel();
    let _ = handle.wait().await;

    // Control file should exist
    assert!(ctrl_path.exists(), "control file should exist after cancel");

    // Second download: should resume and complete
    let handle2 = downloader.download(spec);
    handle2.wait().await.unwrap();

    let downloaded = std::fs::read(&output_path).unwrap();
    assert_eq!(downloaded.len(), expected.len());
    assert_eq!(downloaded, expected);

    assert!(
        !ctrl_path.exists(),
        "control file should be deleted on success"
    );
}

#[tokio::test]
async fn test_multi_connection_with_logging() {
    let content: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
    let expected = content.clone();

    let (addr, server) = range_file_server("logmulti", content);
    tokio::spawn(server);

    let dir = tempfile::tempdir().unwrap();
    let output_path = dir.path().join("log_multi.bin");

    let downloader = Downloader::builder()
        .log_level(LogLevel::Debug)
        .build()
        .unwrap();
    let spec = DownloadSpec::new(format!("http://{addr}/logmulti"))
        .output_path(output_path.clone())
        .max_connections(4)
        .piece_size(50_000)
        .min_split_size(1)
        .file_allocation(FileAllocation::None);

    let handle = downloader.download(spec);
    handle.wait().await.unwrap();

    let downloaded = std::fs::read(&output_path).unwrap();
    assert_eq!(downloaded.len(), expected.len());
    assert_eq!(downloaded, expected);
}
