use std::sync::Arc;

use bytehaul::{DownloadSpec, DownloadState, Downloader, FileAllocation, LogLevel};
use warp::Filter;

#[tokio::test]
async fn test_basic_download() {
    let content: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
    let expected = content.clone();

    let route = warp::path("testfile").map(move || warp::http::Response::new(content.clone()));
    let (addr, server) = warp::serve(route).bind_ephemeral(([127, 0, 0, 1], 0));
    tokio::spawn(server);

    let dir = tempfile::tempdir().unwrap();
    let output_path = dir.path().join("downloaded.bin");

    let downloader = Downloader::builder().build().unwrap();
    let spec = DownloadSpec::new(format!("http://{addr}/testfile"))
        .output_path(output_path.clone())
        .file_allocation(FileAllocation::None);

    let handle = downloader.download(spec);
    handle.wait().await.unwrap();

    let downloaded = std::fs::read(&output_path).unwrap();
    assert_eq!(downloaded, expected);
}

#[tokio::test]
async fn test_download_with_prealloc() {
    let content: Vec<u8> = vec![0x42; 50_000];
    let expected = content.clone();

    let route = warp::path("preallocfile").map(move || warp::http::Response::new(content.clone()));
    let (addr, server) = warp::serve(route).bind_ephemeral(([127, 0, 0, 1], 0));
    tokio::spawn(server);

    let dir = tempfile::tempdir().unwrap();
    let output_path = dir.path().join("prealloc.bin");

    let downloader = Downloader::builder().build().unwrap();
    let spec = DownloadSpec::new(format!("http://{addr}/preallocfile"))
        .output_path(output_path.clone())
        .file_allocation(FileAllocation::Prealloc);

    let handle = downloader.download(spec);
    handle.wait().await.unwrap();

    let downloaded = std::fs::read(&output_path).unwrap();
    assert_eq!(downloaded, expected);
}

#[tokio::test]
async fn test_download_progress_reports() {
    let content: Vec<u8> = vec![0xAB; 80_000];
    let expected_len = content.len() as u64;

    let route = warp::path("progressfile").map(move || warp::http::Response::new(content.clone()));
    let (addr, server) = warp::serve(route).bind_ephemeral(([127, 0, 0, 1], 0));
    tokio::spawn(server);

    let dir = tempfile::tempdir().unwrap();
    let output_path = dir.path().join("progress.bin");

    let downloader = Downloader::builder().build().unwrap();
    let spec = DownloadSpec::new(format!("http://{addr}/progressfile"))
        .output_path(output_path.clone())
        .file_allocation(FileAllocation::None);

    let handle = downloader.download(spec);
    let mut rx = handle.subscribe_progress();

    // Wait for completion
    handle.wait().await.unwrap();

    // After completion the progress receiver should show Completed
    let snap = rx.borrow_and_update().clone();
    assert_eq!(snap.state, DownloadState::Completed);
    assert_eq!(snap.downloaded, expected_len);
    assert_eq!(snap.total_size, Some(expected_len));
    assert_eq!(snap.eta_secs, Some(0.0));
}

#[tokio::test]
async fn test_single_connection_eta_reports() {
    let route = warp::path("etafile").map(|| {
        let stream = futures::stream::unfold(0u32, |count| async move {
            if count >= 30 {
                return None;
            }
            tokio::time::sleep(std::time::Duration::from_millis(80)).await;
            let chunk = vec![0xEEu8; 8_192];
            Some((Ok::<_, std::convert::Infallible>(chunk), count + 1))
        });
        let body = warp::hyper::Body::wrap_stream(stream);
        warp::http::Response::builder()
            .header("content-length", (30 * 8_192).to_string())
            .body(body)
            .unwrap()
    });
    let (addr, server) = warp::serve(route).bind_ephemeral(([127, 0, 0, 1], 0));
    tokio::spawn(server);

    let dir = tempfile::tempdir().unwrap();
    let output_path = dir.path().join("eta.bin");

    let downloader = Downloader::builder().build().unwrap();
    let spec = DownloadSpec::new(format!("http://{addr}/etafile"))
        .output_path(output_path.clone())
        .file_allocation(FileAllocation::None);

    let handle = downloader.download(spec);
    let mut rx = handle.subscribe_progress();
    let mut saw_eta = false;
    let mut saw_speed = false;

    for _ in 0..40 {
        tokio::time::sleep(std::time::Duration::from_millis(120)).await;
        let snap = rx.borrow_and_update().clone();
        if matches!(snap.state, DownloadState::Downloading) && snap.eta_secs.is_some() {
            saw_eta = true;
            saw_speed = snap.speed_bytes_per_sec > 0.0;
            break;
        }
    }

    handle.wait().await.unwrap();
    let final_snap = rx.borrow_and_update().clone();
    assert!(saw_eta, "eta should become available during download");
    assert!(saw_speed, "speed should be driven by the same recent samples as eta");
    assert_eq!(final_snap.eta_secs, Some(0.0));
}

#[tokio::test]
async fn test_single_connection_progress_callback_is_throttled_and_terminal() {
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Mutex;

    let route = warp::path("throttled-progress").map(|| {
        let stream = futures::stream::unfold(0u32, |count| async move {
            if count >= 60 {
                return None;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            let chunk = vec![0xAAu8; 8_192];
            Some((Ok::<_, std::convert::Infallible>(chunk), count + 1))
        });
        let body = warp::hyper::Body::wrap_stream(stream);
        warp::http::Response::builder()
            .header("content-length", (60 * 8_192).to_string())
            .body(body)
            .unwrap()
    });
    let (addr, server) = warp::serve(route).bind_ephemeral(([127, 0, 0, 1], 0));
    tokio::spawn(server);

    let dir = tempfile::tempdir().unwrap();
    let output_path = dir.path().join("throttled-progress.bin");

    let downloader = Downloader::builder().build().unwrap();
    let spec = DownloadSpec::new(format!("http://{addr}/throttled-progress"))
        .output_path(output_path)
        .file_allocation(FileAllocation::None);

    let handle = downloader.download(spec);
    let callback_count = Arc::new(AtomicU32::new(0));
    let final_state = Arc::new(Mutex::new(None));
    let callback_count_clone = callback_count.clone();
    let final_state_clone = final_state.clone();
    handle.on_progress(move |snap| {
        callback_count_clone.fetch_add(1, Ordering::Relaxed);
        *final_state_clone.lock().unwrap() = Some(snap.state);
    });

    handle.wait().await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let delivered = callback_count.load(Ordering::Relaxed);
    assert!(delivered < 30, "expected throttled callback delivery, got {delivered}");
    assert_eq!(*final_state.lock().unwrap(), Some(DownloadState::Completed));
}

#[tokio::test]
async fn test_download_cancel() {
    // Serve a slow stream via chunked responses so cancel has time to fire.
    let route = warp::path("slowfile").map(|| {
        let stream = futures::stream::unfold(0u32, |count| async move {
            if count >= 200 {
                return None;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            let chunk = vec![0xCDu8; 10_000];
            Some((Ok::<_, std::convert::Infallible>(chunk), count + 1))
        });
        let body = warp::hyper::Body::wrap_stream(stream);
        warp::http::Response::builder()
            .header("content-length", (200 * 10_000).to_string())
            .body(body)
            .unwrap()
    });
    let (addr, server) = warp::serve(route).bind_ephemeral(([127, 0, 0, 1], 0));
    tokio::spawn(server);

    let dir = tempfile::tempdir().unwrap();
    let output_path = dir.path().join("cancelled.bin");

    let downloader = Downloader::builder().build().unwrap();
    let spec = DownloadSpec::new(format!("http://{addr}/slowfile"))
        .output_path(output_path.clone())
        .file_allocation(FileAllocation::None);

    let handle = downloader.download(spec);
    let mut rx = handle.subscribe_progress();

    // Give it a moment to start, then cancel
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    handle.cancel();

    let result = handle.wait().await;
    assert!(result.is_err());
    assert_eq!(rx.borrow_and_update().state, DownloadState::Cancelled);
}

#[tokio::test]
async fn test_download_404() {
    let route = warp::path("missing").map(|| {
        warp::http::Response::builder()
            .status(404)
            .body(Vec::<u8>::new())
            .unwrap()
    });
    let (addr, server) = warp::serve(route).bind_ephemeral(([127, 0, 0, 1], 0));
    tokio::spawn(server);

    let dir = tempfile::tempdir().unwrap();
    let output_path = dir.path().join("missing.bin");

    let downloader = Downloader::builder().build().unwrap();
    let spec = DownloadSpec::new(format!("http://{addr}/missing"))
        .output_path(output_path.clone())
        .file_allocation(FileAllocation::None);

    let handle = downloader.download(spec);
    let mut rx = handle.subscribe_progress();
    let result = handle.wait().await;
    assert!(result.is_err());
    assert_eq!(rx.borrow_and_update().state, DownloadState::Failed);
}

#[tokio::test]
async fn test_basic_download_with_logging() {
    let content: Vec<u8> = (0..10_000u32).map(|i| (i % 251) as u8).collect();
    let expected = content.clone();

    let route = warp::path("logtest").map(move || warp::http::Response::new(content.clone()));
    let (addr, server) = warp::serve(route).bind_ephemeral(([127, 0, 0, 1], 0));
    tokio::spawn(server);

    let dir = tempfile::tempdir().unwrap();
    let output_path = dir.path().join("log_download.bin");

    let downloader = Downloader::builder()
        .log_level(LogLevel::Debug)
        .build()
        .unwrap();
    let spec = DownloadSpec::new(format!("http://{addr}/logtest"))
        .output_path(output_path.clone())
        .file_allocation(FileAllocation::None);

    let handle = downloader.download(spec);
    handle.wait().await.unwrap();

    let downloaded = std::fs::read(&output_path).unwrap();
    assert_eq!(downloaded, expected);
}
