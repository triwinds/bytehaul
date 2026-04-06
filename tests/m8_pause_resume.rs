use std::sync::Arc;
use std::time::Duration;

use bytehaul::{DownloadError, DownloadHandle, DownloadSpec, DownloadState, Downloader, FileAllocation};
use futures::StreamExt;
use warp::Filter;

fn slow_single_server(
    path_segment: &'static str,
    data: Vec<u8>,
) -> (std::net::SocketAddr, impl std::future::Future<Output = ()>) {
    let data = Arc::new(data);
    let route = warp::path(path_segment)
        .and(warp::header::optional::<String>("range"))
        .map(move |range_header: Option<String>| {
            let data = data.clone();
            let total = data.len();
            let start = range_header
                .as_deref()
                .and_then(|value| value.trim_start_matches("bytes=").split('-').next())
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or(0);
            let remaining = data[start as usize..].to_vec();
            let chunks: Vec<Result<Vec<u8>, std::convert::Infallible>> = remaining
                .chunks(5_000)
                .map(|chunk| Ok(chunk.to_vec()))
                .collect();
            let stream = futures::stream::iter(chunks).then(
                |chunk: Result<Vec<u8>, std::convert::Infallible>| async move {
                    tokio::time::sleep(Duration::from_millis(30)).await;
                    chunk
                },
            );
            let body = warp::hyper::Body::wrap_stream(stream);
            let status = if start > 0 { 206 } else { 200 };
            let mut builder = warp::http::Response::builder()
                .status(status)
                .header("content-length", (total as u64 - start).to_string())
                .header("accept-ranges", "bytes")
                .header("etag", "\"pause-single\"")
                .header("last-modified", "Sat, 01 Jan 2026 00:00:00 GMT");
            if start > 0 {
                builder = builder.header(
                    "content-range",
                    format!("bytes {}-{}/{}", start, total - 1, total),
                );
            }
            builder.body(body).unwrap()
        });

    warp::serve(route).bind_ephemeral(([127, 0, 0, 1], 0))
}

fn slow_multi_server(
    path_segment: &'static str,
    data: Vec<u8>,
) -> (std::net::SocketAddr, impl std::future::Future<Output = ()>) {
    let data = Arc::new(data);
    let route = warp::path(path_segment)
        .and(warp::header::optional::<String>("range"))
        .map(move |range_header: Option<String>| {
            let data = data.clone();
            let total = data.len();
            let (start, end) = match range_header {
                Some(range) => {
                    let range = range.trim_start_matches("bytes=");
                    let parts: Vec<&str> = range.split('-').collect();
                    let start = parts[0].parse::<u64>().unwrap_or(0);
                    let end = if parts.len() > 1 && !parts[1].is_empty() {
                        parts[1]
                            .parse::<u64>()
                            .unwrap_or(total as u64 - 1)
                            .min(total as u64 - 1)
                    } else {
                        total as u64 - 1
                    };
                    (start, end)
                }
                None => (0, total as u64 - 1),
            };

            let slice = data[start as usize..=end as usize].to_vec();
            let chunks: Vec<Result<Vec<u8>, std::convert::Infallible>> =
                slice.chunks(32 * 1024).map(|chunk| Ok(chunk.to_vec())).collect();
            let stream = futures::stream::iter(chunks).then(
                |chunk: Result<Vec<u8>, std::convert::Infallible>| async move {
                    tokio::time::sleep(Duration::from_millis(5)).await;
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
                .header("etag", "\"pause-multi\"")
                .header("last-modified", "Sat, 01 Jan 2026 00:00:00 GMT");
            if is_range {
                builder = builder.header(
                    "content-range",
                    format!("bytes {}-{}/{}", start, end, total),
                );
            }
            builder.body(body).unwrap()
        });

    warp::serve(route).bind_ephemeral(([127, 0, 0, 1], 0))
}

async fn wait_for_state(handle: &DownloadHandle, expected: DownloadState) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let state = handle.progress().state;
        if state == expected {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "expected progress state {:?}, got {:?}",
            expected,
            state
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[tokio::test]
async fn test_pause_resume_single_connection() {
    let content: Vec<u8> = (0..200_000u32).map(|index| (index % 251) as u8).collect();
    let expected = content.clone();

    let (addr, server) = slow_single_server("pause-single", content);
    tokio::spawn(server);

    let dir = tempfile::tempdir().unwrap();
    let output_path = dir.path().join("pause-single.bin");
    let ctrl_path = output_path.with_file_name("pause-single.bin.bytehaul");

    let downloader = Downloader::builder().build().unwrap();
    let spec = DownloadSpec::new(format!("http://{addr}/pause-single"))
        .output_path(output_path.clone())
        .file_allocation(FileAllocation::None);

    let handle = downloader.download(spec.clone());
    let mut rx = handle.subscribe_progress();
    tokio::time::sleep(Duration::from_millis(250)).await;
    handle.pause();
    wait_for_state(&handle, DownloadState::Paused).await;

    assert!(matches!(handle.wait().await, Err(DownloadError::Paused)));
    assert_eq!(rx.borrow_and_update().state, DownloadState::Paused);
    assert!(ctrl_path.exists(), "control file should exist after pause");

    let partial_size = std::fs::metadata(&output_path)
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    assert!(partial_size > 0, "pause should preserve partial progress");

    let resumed = downloader.download(spec);
    resumed.wait().await.unwrap();

    let downloaded = std::fs::read(&output_path).unwrap();
    assert_eq!(downloaded, expected);
    assert!(!ctrl_path.exists(), "control file should be deleted on success");
}

#[tokio::test]
async fn test_pause_resume_multi_connection() {
    let size = 15 * 1024 * 1024;
    let content: Vec<u8> = (0..size).map(|index| (index % 251) as u8).collect();
    let expected = content.clone();

    let (addr, server) = slow_multi_server("pause-multi", content);
    tokio::spawn(server);

    let dir = tempfile::tempdir().unwrap();
    let output_path = dir.path().join("pause-multi.bin");
    let ctrl_path = output_path.with_file_name("pause-multi.bin.bytehaul");

    let downloader = Downloader::builder().build().unwrap();
    let spec = DownloadSpec::new(format!("http://{addr}/pause-multi"))
        .output_path(output_path.clone())
        .file_allocation(FileAllocation::Prealloc)
        .max_connections(4)
        .piece_size(1024 * 1024)
        .min_split_size(10 * 1024 * 1024);

    let handle = downloader.download(spec.clone());
    tokio::time::sleep(Duration::from_millis(500)).await;
    handle.pause();
    wait_for_state(&handle, DownloadState::Paused).await;

    assert!(matches!(handle.wait().await, Err(DownloadError::Paused)));
    assert!(ctrl_path.exists(), "control file should exist after pause");

    let resumed = downloader.download(spec);
    resumed.wait().await.unwrap();

    let downloaded = std::fs::read(&output_path).unwrap();
    assert_eq!(downloaded, expected);
    assert!(!ctrl_path.exists(), "control file should be deleted on success");
}