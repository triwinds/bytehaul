use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

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
    let mut spec = DownloadSpec::new(format!("http://{addr}/retry503"), &output_path);
    spec.file_allocation = FileAllocation::None;
    spec.max_connections = 4;
    spec.piece_size = 1024 * 1024;
    spec.min_split_size = 10 * 1024 * 1024;
    spec.max_retries = 5;
    spec.retry_base_delay = std::time::Duration::from_millis(10);
    spec.retry_max_delay = std::time::Duration::from_millis(100);

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
    let mut spec = DownloadSpec::new(format!("http://{addr}/retrysmall"), &output_path);
    spec.file_allocation = FileAllocation::None;
    spec.max_connections = 1;
    spec.max_retries = 5;
    spec.retry_base_delay = std::time::Duration::from_millis(10);
    spec.retry_max_delay = std::time::Duration::from_millis(100);

    let handle = downloader.download(spec);
    handle.wait().await.unwrap();

    let downloaded = std::fs::read(&output_path).unwrap();
    assert_eq!(downloaded, expected);
}

#[tokio::test]
async fn test_no_retry_on_403() {
    // 403 is non-retryable; download should fail immediately.
    let (addr, server) = forbidden_server("forbidden");
    tokio::spawn(server);

    let dir = tempfile::tempdir().unwrap();
    let output_path = dir.path().join("forbidden.bin");

    let downloader = Downloader::builder().build().unwrap();
    let mut spec = DownloadSpec::new(format!("http://{addr}/forbidden"), &output_path);
    spec.file_allocation = FileAllocation::None;
    spec.max_retries = 5;

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
    let mut spec = DownloadSpec::new(format!("http://{addr}/alwaysfail"), &output_path);
    spec.file_allocation = FileAllocation::None;
    spec.max_connections = 1;
    spec.max_retries = 2;
    spec.retry_base_delay = std::time::Duration::from_millis(10);
    spec.retry_max_delay = std::time::Duration::from_millis(50);

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
    let mut spec = DownloadSpec::new(format!("http://{addr}/rate_limit"), &output_path);
    spec.file_allocation = FileAllocation::None;
    spec.max_connections = 4;
    spec.piece_size = 1024 * 1024;
    spec.min_split_size = 10 * 1024 * 1024;
    spec.max_retries = 5;
    spec.retry_base_delay = std::time::Duration::from_millis(10);
    spec.retry_max_delay = std::time::Duration::from_millis(100);

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
    let mut spec = DownloadSpec::new(format!("http://{addr}/gzip_enc"), &output_path);
    spec.file_allocation = FileAllocation::None;
    spec.max_connections = 4;
    spec.piece_size = 1024 * 1024;
    spec.min_split_size = 10 * 1024 * 1024;

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
    let mut spec = DownloadSpec::new(format!("http://{addr}/segretry"), &output_path);
    spec.file_allocation = FileAllocation::None;
    spec.max_connections = 4;
    spec.piece_size = 1024 * 1024;
    spec.min_split_size = 1024 * 1024;
    spec.max_retries = 8;
    spec.retry_base_delay = std::time::Duration::from_millis(10);
    spec.retry_max_delay = std::time::Duration::from_millis(100);

    let handle = downloader.download(spec);
    handle.wait().await.unwrap();

    let downloaded = std::fs::read(&output_path).unwrap();
    assert_eq!(downloaded.len(), expected.len());
    assert_eq!(downloaded, expected);
}
