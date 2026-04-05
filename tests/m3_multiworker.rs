use std::sync::Arc;

use bytehaul::{DownloadSpec, DownloadState, Downloader, FileAllocation, LogLevel};
use warp::Filter;

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
            let chunks: Vec<Result<Vec<u8>, std::convert::Infallible>> =
                slice.chunks(32 * 1024).map(|chunk| Ok(chunk.to_vec())).collect();
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
    assert!(saw_eta, "eta should become available during multi-worker download");
    assert!(saw_speed, "speed should be driven by the same recent samples as eta");
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
