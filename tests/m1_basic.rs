use bytehaul::{DownloadSpec, DownloadState, Downloader, FileAllocation};
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
    let mut spec = DownloadSpec::new(format!("http://{addr}/testfile"), &output_path);
    spec.file_allocation = FileAllocation::None;

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
    let mut spec = DownloadSpec::new(format!("http://{addr}/preallocfile"), &output_path);
    spec.file_allocation = FileAllocation::Prealloc;

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
    let mut spec = DownloadSpec::new(format!("http://{addr}/progressfile"), &output_path);
    spec.file_allocation = FileAllocation::None;

    let handle = downloader.download(spec);
    let mut rx = handle.subscribe_progress();

    // Wait for completion
    handle.wait().await.unwrap();

    // After completion the progress receiver should show Completed
    let snap = rx.borrow_and_update().clone();
    assert_eq!(snap.state, DownloadState::Completed);
    assert_eq!(snap.downloaded, expected_len);
    assert_eq!(snap.total_size, Some(expected_len));
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
    let mut spec = DownloadSpec::new(format!("http://{addr}/slowfile"), &output_path);
    spec.file_allocation = FileAllocation::None;

    let handle = downloader.download(spec);

    // Give it a moment to start, then cancel
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    handle.cancel();

    let result = handle.wait().await;
    assert!(result.is_err());
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
    let mut spec = DownloadSpec::new(format!("http://{addr}/missing"), &output_path);
    spec.file_allocation = FileAllocation::None;

    let handle = downloader.download(spec);
    let result = handle.wait().await;
    assert!(result.is_err());
}
