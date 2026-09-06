use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::thread;

use bytehaul::{DownloadSpec, Downloader, FileAllocation};
use warp::Filter;

/// Server that fails the first N requests with 503, then serves normally.
fn flaky_server(
    path_segment: &'static str,
    data: Vec<u8>,
    fail_count: u32,
) -> (std::net::SocketAddr, impl std::future::Future<Output = ()>) {
    let data = Arc::new(data);
    let counter = Arc::new(AtomicU32::new(0));

    let d = data.clone();
    let c = counter.clone();

    let route = warp::path(path_segment)
        .and(warp::header::optional::<String>("range"))
        .map(move |range_header: Option<String>| {
            let data = d.clone();
            let count = c.fetch_add(1, Ordering::SeqCst);
            let total = data.len();

            if count < fail_count {
                return warp::http::Response::builder()
                    .status(503)
                    .header("retry-after", "0")
                    .body(Vec::new())
                    .unwrap();
            }

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
                        .header("etag", "\"flaky\"")
                        .body(Vec::from(slice))
                        .unwrap()
                }
                None => warp::http::Response::builder()
                    .status(200)
                    .header("content-length", total.to_string())
                    .header("accept-ranges", "bytes")
                    .header("etag", "\"flaky\"")
                    .body(data.to_vec())
                    .unwrap(),
            }
        });

    warp::serve(route).bind_ephemeral(([127, 0, 0, 1], 0))
}

/// The first full response is truncated after a prefix. The next request must
/// resume at that persisted prefix and receive a strict 206 response.
fn truncated_body_then_range_server(
    path_segment: &'static str,
    data: Vec<u8>,
) -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let handle = thread::spawn(move || {
        for request_no in 0..2 {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            let mut buffer = [0u8; 1024];
            loop {
                let read = stream.read(&mut buffer).unwrap();
                assert!(read > 0, "client closed before sending request");
                request.extend_from_slice(&buffer[..read]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            let request = String::from_utf8_lossy(&request);
            let total = data.len();
            if request_no == 0 {
                assert!(!request.to_ascii_lowercase().contains("range:"));
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {total}\r\nETag: \"retry-body\"\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n"
                )
                .unwrap();
                stream.write_all(&data[..4]).unwrap();
            } else {
                let range_line = request
                    .lines()
                    .find(|line| line.to_ascii_lowercase().starts_with("range:"))
                    .expect("retry request must carry Range");
                let range = range_line.split_once(':').unwrap().1.trim();
                let range = range.trim_start_matches("bytes=");
                let (start, end) = range.split_once('-').expect("valid Range header");
                let start: usize = start.parse().unwrap();
                let end: usize = end.parse().unwrap();
                assert_eq!(start, 4, "retry must resume at persisted prefix");
                assert_eq!(end, total - 1);
                let body = &data[start..=end];
                write!(
                    stream,
                    "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {start}-{end}/{total}\r\nETag: \"retry-body\"\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .unwrap();
                stream.write_all(body).unwrap();
            }
            stream.shutdown(Shutdown::Both).unwrap();
        }
    });
    (format!("http://{address}/{path_segment}"), handle)
}

/// The first chunked response ends before its terminating chunk. With no
/// trustworthy total size, the client must restart with a plain full GET.
fn truncated_chunked_then_full_server(
    path_segment: &'static str,
    data: Vec<u8>,
) -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let handle = thread::spawn(move || {
        for request_no in 0..2 {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            let mut buffer = [0u8; 1024];
            loop {
                let read = stream.read(&mut buffer).unwrap();
                assert!(read > 0, "client closed before sending request");
                request.extend_from_slice(&buffer[..read]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            let request = String::from_utf8_lossy(&request);
            assert!(!request.to_ascii_lowercase().contains("range:"));
            if request_no == 0 {
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n4\r\n{}\r\n",
                    String::from_utf8_lossy(&data[..4])
                )
                .unwrap();
            } else {
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:x}\r\n{}\r\n0\r\n\r\n",
                    data.len(),
                    String::from_utf8_lossy(&data)
                )
                .unwrap();
            }
            stream.shutdown(Shutdown::Both).unwrap();
        }
    });
    (format!("http://{address}/{path_segment}"), handle)
}

/// The first response is truncated, then a retry either ignores Range or
/// changes validators. The client must reset before the final full GET.
fn truncated_then_restart_server(
    path_segment: &'static str,
    data: Vec<u8>,
    range_returns_206_with_changed_etag: bool,
) -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let handle = thread::spawn(move || {
        let request_count = if range_returns_206_with_changed_etag {
            3
        } else {
            2
        };
        for request_no in 0..request_count {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            let mut buffer = [0u8; 1024];
            loop {
                let read = stream.read(&mut buffer).unwrap();
                assert!(read > 0, "client closed before sending request");
                request.extend_from_slice(&buffer[..read]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            let request = String::from_utf8_lossy(&request);
            let total = data.len();
            match request_no {
                0 => {
                    assert!(!request.to_ascii_lowercase().contains("range:"));
                    write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Length: {total}\r\nETag: \"old\"\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n"
                    )
                    .unwrap();
                    stream.write_all(&data[..4]).unwrap();
                }
                1 if range_returns_206_with_changed_etag => {
                    let range_line = request
                        .lines()
                        .find(|line| line.to_ascii_lowercase().starts_with("range:"))
                        .expect("retry request must carry Range");
                    assert_eq!(range_line.split_once(':').unwrap().1.trim(), "bytes=4-9");
                    let body = &data[4..];
                    write!(
                        stream,
                        "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes 4-9/{total}\r\nETag: \"new\"\r\nConnection: close\r\n\r\n",
                        body.len()
                    )
                    .unwrap();
                    stream.write_all(body).unwrap();
                }
                1 => {
                    let range_line = request
                        .lines()
                        .find(|line| line.to_ascii_lowercase().starts_with("range:"))
                        .expect("retry request must carry Range");
                    assert_eq!(range_line.split_once(':').unwrap().1.trim(), "bytes=4-9");
                    write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Length: {total}\r\nETag: \"old\"\r\nConnection: close\r\n\r\n"
                    )
                    .unwrap();
                    stream.write_all(&data).unwrap();
                }
                2 => {
                    assert!(!request.to_ascii_lowercase().contains("range:"));
                    write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Length: {total}\r\nETag: \"{}\"\r\nConnection: close\r\n\r\n",
                        if range_returns_206_with_changed_etag { "new" } else { "old" }
                    )
                    .unwrap();
                    stream.write_all(&data).unwrap();
                }
                _ => unreachable!(),
            }
            stream.shutdown(Shutdown::Both).unwrap();
        }
    });
    (format!("http://{address}/{path_segment}"), handle)
}

/// Server that rate-limits only the initial Range probe but would allow a plain GET.
fn range_probe_retryable_error_server(
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

            if range_header.is_some() {
                return warp::http::Response::builder()
                    .status(503)
                    .header("retry-after", "0")
                    .body(Vec::new())
                    .unwrap();
            }

            warp::http::Response::builder()
                .status(200)
                .header("content-length", total.to_string())
                .body(data.to_vec())
                .unwrap()
        });

    warp::serve(route).bind_ephemeral(([127, 0, 0, 1], 0))
}

/// Server that always returns 403 (non-retryable).
fn forbidden_server(
    path_segment: &'static str,
) -> (std::net::SocketAddr, impl std::future::Future<Output = ()>) {
    let route = warp::path(path_segment).map(|| {
        warp::http::Response::builder()
            .status(403)
            .body(Vec::<u8>::new())
            .unwrap()
    });

    warp::serve(route).bind_ephemeral(([127, 0, 0, 1], 0))
}

#[tokio::test]
async fn test_fresh_probe_503_does_not_fallback_to_plain_get() {
    let content: Vec<u8> = (0..50_000u32).map(|i| (i % 199) as u8).collect();
    let (addr, server) = range_probe_retryable_error_server("probe503", content);
    tokio::spawn(server);

    let dir = tempfile::tempdir().unwrap();
    let output_path = dir.path().join("probe503.bin");

    let downloader = Downloader::builder().build().unwrap();
    let spec = DownloadSpec::new(format!("http://{addr}/probe503"))
        .output_path(output_path)
        .file_allocation(FileAllocation::None)
        .max_connections(4)
        .max_retries(0);

    let err = downloader.download(spec).wait().await.unwrap_err();

    match err {
        bytehaul::DownloadError::HttpStatus { status, message } => {
            assert_eq!(status, 503);
            assert_eq!(message, "retry-after:0");
        }
        other => panic!("expected HttpStatus 503, got: {other:?}"),
    }
}

#[tokio::test]
async fn test_retry_on_503_multi_worker() {
    // Server fails the first 3 requests with 503, then succeeds.
    // With max_retries=5, the download should eventually succeed.
    let size = 15 * 1024 * 1024;
    let content: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
    let expected = content.clone();

    let (addr, server) = flaky_server("retry503", content, 3);
    tokio::spawn(server);

    let dir = tempfile::tempdir().unwrap();
    let output_path = dir.path().join("retry503.bin");

    let downloader = Downloader::builder().build().unwrap();
    let spec = DownloadSpec::new(format!("http://{addr}/retry503"))
        .output_path(output_path.clone())
        .file_allocation(FileAllocation::None)
        .max_connections(4)
        .piece_size(1024 * 1024)
        .min_split_size(10 * 1024 * 1024)
        .retry_policy(
            5,
            std::time::Duration::from_millis(10),
            std::time::Duration::from_millis(100),
        );

    let handle = downloader.download(spec);
    handle.wait().await.unwrap();

    let downloaded = std::fs::read(&output_path).unwrap();
    assert_eq!(downloaded.len(), expected.len());
    assert_eq!(downloaded, expected);
}

#[tokio::test]
async fn test_retry_on_503_single_connection() {
    // Small file (single-connection path) with initial failures
    let content: Vec<u8> = (0..50_000u32).map(|i| (i % 199) as u8).collect();
    let expected = content.clone();

    // Fail the first 2 requests (probe + fallback GET), then succeed
    let (addr, server) = flaky_server("retrysmall", content, 2);
    tokio::spawn(server);

    let dir = tempfile::tempdir().unwrap();
    let output_path = dir.path().join("retrysmall.bin");

    let downloader = Downloader::builder().build().unwrap();
    let spec = DownloadSpec::new(format!("http://{addr}/retrysmall"))
        .output_path(output_path.clone())
        .file_allocation(FileAllocation::None)
        .max_connections(1)
        .retry_policy(
            5,
            std::time::Duration::from_millis(10),
            std::time::Duration::from_millis(100),
        );

    let handle = downloader.download(spec);
    handle.wait().await.unwrap();

    let downloaded = std::fs::read(&output_path).unwrap();
    assert_eq!(downloaded, expected);
}

#[tokio::test]
async fn test_retry_on_truncated_body_single_connection_resumes_range() {
    let content = b"0123456789".to_vec();
    let (url, server) = truncated_body_then_range_server("retry-body", content.clone());

    let dir = tempfile::tempdir().unwrap();
    let output_path = dir.path().join("retry-body.bin");

    let downloader = Downloader::builder().build().unwrap();
    let spec = DownloadSpec::new(url)
        .output_path(output_path.clone())
        .file_allocation(FileAllocation::None)
        .max_connections(1)
        .retry_policy(
            2,
            std::time::Duration::from_millis(1),
            std::time::Duration::from_millis(5),
        );

    downloader.download(spec).wait().await.unwrap();
    assert_eq!(std::fs::read(output_path).unwrap(), content);
    server.join().unwrap();
}

#[tokio::test]
async fn test_retry_unknown_length_single_connection_restarts_from_zero() {
    let content = b"0123456789".to_vec();
    let (url, server) = truncated_chunked_then_full_server("retry-chunked", content.clone());
    let dir = tempfile::tempdir().unwrap();
    let output_path = dir.path().join("retry-chunked.bin");

    let downloader = Downloader::builder().build().unwrap();
    let spec = DownloadSpec::new(url)
        .output_path(output_path.clone())
        .file_allocation(FileAllocation::None)
        .max_connections(1)
        .retry_policy(
            2,
            std::time::Duration::from_millis(1),
            std::time::Duration::from_millis(5),
        );

    downloader.download(spec).wait().await.unwrap();
    assert_eq!(std::fs::read(&output_path).unwrap(), content);
    let control_path = std::path::PathBuf::from(format!("{}.bytehaul", output_path.display()));
    assert!(!control_path.exists());
    server.join().unwrap();
}

#[tokio::test]
async fn test_retry_range_ignored_resets_before_full_get() {
    let content = b"0123456789".to_vec();
    let (url, server) =
        truncated_then_restart_server("retry-range-ignored", content.clone(), false);
    let dir = tempfile::tempdir().unwrap();
    let output_path = dir.path().join("retry-range-ignored.bin");

    let downloader = Downloader::builder().build().unwrap();
    let spec = DownloadSpec::new(url)
        .output_path(output_path.clone())
        .file_allocation(FileAllocation::None)
        .max_connections(1)
        .retry_policy(
            3,
            std::time::Duration::from_millis(1),
            std::time::Duration::from_millis(5),
        );

    downloader.download(spec).wait().await.unwrap();
    assert_eq!(std::fs::read(output_path).unwrap(), content);
    server.join().unwrap();
}

#[tokio::test]
async fn test_retry_metadata_change_resets_before_full_get() {
    let content = b"0123456789".to_vec();
    let (url, server) = truncated_then_restart_server("retry-metadata", content.clone(), true);
    let dir = tempfile::tempdir().unwrap();
    let output_path = dir.path().join("retry-metadata.bin");

    let downloader = Downloader::builder().build().unwrap();
    let spec = DownloadSpec::new(url)
        .output_path(output_path.clone())
        .file_allocation(FileAllocation::None)
        .max_connections(1)
        .retry_policy(
            3,
            std::time::Duration::from_millis(1),
            std::time::Duration::from_millis(5),
        );

    downloader.download(spec).wait().await.unwrap();
    assert_eq!(std::fs::read(output_path).unwrap(), content);
    server.join().unwrap();
}

#[tokio::test]
async fn test_no_retry_on_403() {
    // 403 is non-retryable; download should fail immediately.
    let (addr, server) = forbidden_server("forbidden");
    tokio::spawn(server);

    let dir = tempfile::tempdir().unwrap();
    let output_path = dir.path().join("forbidden.bin");

    let downloader = Downloader::builder().build().unwrap();
    let spec = DownloadSpec::new(format!("http://{addr}/forbidden"))
        .output_path(output_path.clone())
        .file_allocation(FileAllocation::None)
        .max_retries(5);

    let handle = downloader.download(spec);
    let err = handle.wait().await.unwrap_err();

    // Should be an HTTP 403 error
    match err {
        bytehaul::DownloadError::HttpStatus { status, .. } => {
            assert_eq!(status, 403);
        }
        other => panic!("expected HttpStatus 403, got: {other:?}"),
    }
}

#[tokio::test]
async fn test_exhausted_retries_fails() {
    // Server always fails with 503. With max_retries=2, should fail after retries exhausted.
    let content: Vec<u8> = vec![0u8; 1000];

    // fail_count very high, so it always fails
    let (addr, server) = flaky_server("alwaysfail", content, 1000);
    tokio::spawn(server);

    let dir = tempfile::tempdir().unwrap();
    let output_path = dir.path().join("alwaysfail.bin");

    let downloader = Downloader::builder().build().unwrap();
    let spec = DownloadSpec::new(format!("http://{addr}/alwaysfail"))
        .output_path(output_path.clone())
        .file_allocation(FileAllocation::None)
        .max_connections(1)
        .retry_policy(
            2,
            std::time::Duration::from_millis(10),
            std::time::Duration::from_millis(50),
        );

    let handle = downloader.download(spec);
    let result = handle.wait().await;
    assert!(result.is_err());
}

#[tokio::test]
async fn test_error_is_retryable() {
    // Unit test for is_retryable logic
    let e = bytehaul::DownloadError::HttpStatus {
        status: 503,
        message: "Service Unavailable".into(),
    };
    assert!(e.is_retryable());

    let e = bytehaul::DownloadError::HttpStatus {
        status: 429,
        message: "retry-after:5".into(),
    };
    assert!(e.is_retryable());

    let e = bytehaul::DownloadError::HttpStatus {
        status: 403,
        message: "Forbidden".into(),
    };
    assert!(!e.is_retryable());

    let e = bytehaul::DownloadError::Cancelled;
    assert!(!e.is_retryable());
}

/// Server that responds to multi-worker range requests with 429 + retry-after,
/// then succeeds after a few attempts.
fn rate_limit_server(
    path_segment: &'static str,
    data: Vec<u8>,
    fail_count: u32,
) -> (std::net::SocketAddr, impl std::future::Future<Output = ()>) {
    let data = Arc::new(data);
    let counter = Arc::new(AtomicU32::new(0));

    let d = data.clone();
    let c = counter.clone();

    let route = warp::path(path_segment)
        .and(warp::header::optional::<String>("range"))
        .map(move |range_header: Option<String>| {
            let data = d.clone();
            let count = c.fetch_add(1, Ordering::SeqCst);
            let total = data.len();

            if count < fail_count {
                return warp::http::Response::builder()
                    .status(429)
                    .header("retry-after", "1")
                    .body(Vec::new())
                    .unwrap();
            }

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
                        .body(Vec::from(slice))
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

/// Server that returns range responses with gzip content-encoding (disallowed).
fn gzip_encoding_server(
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
                        .header("content-encoding", "gzip")
                        .body(Vec::from(slice))
                        .unwrap()
                }
                None => warp::http::Response::builder()
                    .status(200)
                    .header("content-length", total.to_string())
                    .body(data.to_vec())
                    .unwrap(),
            }
        });

    warp::serve(route).bind_ephemeral(([127, 0, 0, 1], 0))
}

#[tokio::test]
async fn test_retry_429_with_retry_after_multi_worker() {
    let size = 15 * 1024 * 1024;
    let content: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
    let expected = content.clone();

    let (addr, server) = rate_limit_server("rate_limit", content, 3);
    tokio::spawn(server);

    let dir = tempfile::tempdir().unwrap();
    let output_path = dir.path().join("rate_limit.bin");

    let downloader = Downloader::builder().build().unwrap();
    let spec = DownloadSpec::new(format!("http://{addr}/rate_limit"))
        .output_path(output_path.clone())
        .file_allocation(FileAllocation::None)
        .max_connections(4)
        .piece_size(1024 * 1024)
        .min_split_size(10 * 1024 * 1024)
        .retry_policy(
            5,
            std::time::Duration::from_millis(10),
            std::time::Duration::from_millis(100),
        );

    let handle = downloader.download(spec);
    handle.wait().await.unwrap();

    let downloaded = std::fs::read(&output_path).unwrap();
    assert_eq!(downloaded.len(), expected.len());
    assert_eq!(downloaded, expected);
}

#[tokio::test]
async fn test_gzip_content_encoding_fallback() {
    // Server returns 206 with gzip content-encoding on range requests.
    // The downloader should reject this and fallback to single-connection GET.
    let content: Vec<u8> = (0..50_000u32).map(|i| (i % 199) as u8).collect();
    let expected = content.clone();

    let (addr, server) = gzip_encoding_server("gzip_enc", content);
    tokio::spawn(server);

    let dir = tempfile::tempdir().unwrap();
    let output_path = dir.path().join("gzip_enc.bin");

    let downloader = Downloader::builder().build().unwrap();
    let spec = DownloadSpec::new(format!("http://{addr}/gzip_enc"))
        .output_path(output_path.clone())
        .file_allocation(FileAllocation::None)
        .max_connections(4)
        .piece_size(1024 * 1024)
        .min_split_size(10 * 1024 * 1024);

    let handle = downloader.download(spec);
    handle.wait().await.unwrap();

    let downloaded = std::fs::read(&output_path).unwrap();
    assert_eq!(downloaded.len(), expected.len());
    assert_eq!(downloaded, expected);
}

/// Server where the probe (request #0) always succeeds with 206,
/// but subsequent worker segment requests fail with 503 for the first N attempts.
fn probe_ok_workers_flaky_server(
    path_segment: &'static str,
    data: Vec<u8>,
    worker_fail_count: u32,
) -> (std::net::SocketAddr, impl std::future::Future<Output = ()>) {
    let data = Arc::new(data);
    let req_counter = Arc::new(AtomicU32::new(0));
    let worker_fail_counter = Arc::new(AtomicU32::new(0));

    let d = data.clone();
    let rc = req_counter.clone();
    let wc = worker_fail_counter.clone();

    let route = warp::path(path_segment)
        .and(warp::header::optional::<String>("range"))
        .map(move |range_header: Option<String>| {
            let data = d.clone();
            let req_num = rc.fetch_add(1, Ordering::SeqCst);
            let total = data.len();

            match range_header {
                Some(range) => {
                    // After the first request (probe), fail some worker requests with 503
                    if req_num > 0 {
                        let w = wc.fetch_add(1, Ordering::SeqCst);
                        if w < worker_fail_count {
                            return warp::http::Response::builder()
                                .status(503)
                                .header("retry-after", "0")
                                .body(Vec::new())
                                .unwrap();
                        }
                    }

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
                        .header("etag", "\"probeok\"")
                        .body(Vec::from(slice))
                        .unwrap()
                }
                None => warp::http::Response::builder()
                    .status(200)
                    .header("content-length", total.to_string())
                    .header("accept-ranges", "bytes")
                    .header("etag", "\"probeok\"")
                    .body(data.to_vec())
                    .unwrap(),
            }
        });

    warp::serve(route).bind_ephemeral(([127, 0, 0, 1], 0))
}

#[tokio::test]
async fn test_multi_worker_segment_retry_on_503() {
    // Probe succeeds (206), but subsequent worker segment requests fail 503 a few times.
    // This exercises the download_segment → check_segment_status → worker_loop retry path.
    let size = 15 * 1024 * 1024;
    let content: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
    let expected = content.clone();

    let (addr, server) = probe_ok_workers_flaky_server("segretry", content, 4);
    tokio::spawn(server);

    let dir = tempfile::tempdir().unwrap();
    let output_path = dir.path().join("segretry.bin");

    let downloader = Downloader::builder().build().unwrap();
    let spec = DownloadSpec::new(format!("http://{addr}/segretry"))
        .output_path(output_path.clone())
        .file_allocation(FileAllocation::None)
        .max_connections(4)
        .piece_size(1024 * 1024)
        .min_split_size(1024 * 1024)
        .retry_policy(
            8,
            std::time::Duration::from_millis(10),
            std::time::Duration::from_millis(100),
        );

    let handle = downloader.download(spec);
    handle.wait().await.unwrap();

    let downloaded = std::fs::read(&output_path).unwrap();
    assert_eq!(downloaded.len(), expected.len());
    assert_eq!(downloaded, expected);
}
