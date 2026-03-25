use std::time::{Duration, Instant};

use bytehaul::{Checksum, DownloadSpec, Downloader, FileAllocation};
use sha2::{Digest, Sha256};
use warp::Filter;

#[tokio::test]
async fn test_rate_limiting() {
    // 10 KB of data
    let data = vec![0xABu8; 10 * 1024];
    let expected = data.clone();

    let route = warp::path("file").map(move || {
        warp::http::Response::builder()
            .header("content-length", data.len().to_string())
            .body(data.clone())
            .unwrap()
    });
    let (addr, server) = warp::serve(route).bind_ephemeral(([127, 0, 0, 1], 0));
    tokio::spawn(server);

    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("rate_limited.bin");

    // Limit to ~5 KB/s — should take at least 1.5s for 10 KB
    let spec = DownloadSpec::new(format!("http://{addr}/file"))
        .output_path(out.clone())
        .max_connections(1)
        .file_allocation(FileAllocation::None)
        .resume(false)
        .max_download_speed(5 * 1024);

    let dl = Downloader::builder().build().unwrap();
    let start = Instant::now();
    let handle = dl.download(spec);
    handle.wait().await.unwrap();
    let elapsed = start.elapsed();

    // With 10KB at 5KB/s, should take ~2 seconds. Allow at least 1s.
    assert!(
        elapsed >= Duration::from_secs(1),
        "rate limiting should slow the download, but it finished in {elapsed:?}"
    );

    let content = std::fs::read(&out).unwrap();
    assert_eq!(content, expected);
}

#[tokio::test]
async fn test_no_rate_limit_is_fast() {
    let data = vec![0xCDu8; 100 * 1024];
    let expected = data.clone();

    let route = warp::path("file").map(move || warp::http::Response::new(data.clone()));
    let (addr, server) = warp::serve(route).bind_ephemeral(([127, 0, 0, 1], 0));
    tokio::spawn(server);

    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("no_limit.bin");

    let spec = DownloadSpec::new(format!("http://{addr}/file"))
        .output_path(out.clone())
        .max_connections(1)
        .file_allocation(FileAllocation::None)
        .resume(false)
        .max_download_speed(0);

    let dl = Downloader::builder().build().unwrap();
    let start = Instant::now();
    let handle = dl.download(spec);
    handle.wait().await.unwrap();
    let elapsed = start.elapsed();

    assert!(
        elapsed < Duration::from_secs(5),
        "unlimited download should be fast, but took {elapsed:?}"
    );

    let content = std::fs::read(&out).unwrap();
    assert_eq!(content, expected);
}

#[tokio::test]
async fn test_checksum_sha256_pass() {
    let body = b"Hello, checksum world!";
    let hash = Sha256::digest(body);
    let hex = format!("{hash:x}");

    let data = body.to_vec();
    let route = warp::path("file").map(move || warp::http::Response::new(data.clone()));
    let (addr, server) = warp::serve(route).bind_ephemeral(([127, 0, 0, 1], 0));
    tokio::spawn(server);

    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("checksum_ok.bin");

    let spec = DownloadSpec::new(format!("http://{addr}/file"))
        .output_path(out.clone())
        .max_connections(1)
        .file_allocation(FileAllocation::None)
        .resume(false)
        .checksum(Checksum::Sha256(hex));

    let dl = Downloader::builder().build().unwrap();
    let handle = dl.download(spec);
    handle.wait().await.unwrap();

    let content = std::fs::read(&out).unwrap();
    assert_eq!(content, body.as_slice());
}

#[tokio::test]
async fn test_checksum_sha256_fail() {
    let body = b"Hello, checksum world!";
    let data = body.to_vec();

    let route = warp::path("file").map(move || warp::http::Response::new(data.clone()));
    let (addr, server) = warp::serve(route).bind_ephemeral(([127, 0, 0, 1], 0));
    tokio::spawn(server);

    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("checksum_bad.bin");

    let spec = DownloadSpec::new(format!("http://{addr}/file"))
        .output_path(out.clone())
        .max_connections(1)
        .file_allocation(FileAllocation::None)
        .resume(false)
        .checksum(Checksum::Sha256(
            "0000000000000000000000000000000000000000000000000000000000000000".to_string(),
        ));

    let dl = Downloader::builder().build().unwrap();
    let handle = dl.download(spec);
    let result = handle.wait().await;
    assert!(result.is_err(), "should fail with checksum mismatch");
    let err_str = result.unwrap_err().to_string();
    assert!(
        err_str.contains("checksum mismatch"),
        "error should mention checksum mismatch, got: {err_str}"
    );
}

#[tokio::test]
async fn test_checksum_error_is_public() {
    let err = bytehaul::DownloadError::ChecksumMismatch {
        expected: "abc".into(),
        actual: "def".into(),
    };
    assert!(err.to_string().contains("abc"));
    assert!(err.to_string().contains("def"));
    assert!(!err.is_retryable());
}
