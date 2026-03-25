use bytehaul::{DownloadSpec, Downloader, FileAllocation};
use warp::Filter;

#[tokio::test]
async fn test_empty_file_download() {
    let route = warp::path("empty").map(|| {
        warp::http::Response::builder()
            .header("content-length", "0")
            .body(Vec::<u8>::new())
            .unwrap()
    });
    let (addr, server) = warp::serve(route).bind_ephemeral(([127, 0, 0, 1], 0));
    tokio::spawn(server);

    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("empty.bin");

    let spec = DownloadSpec::new(format!("http://{addr}/empty"))
        .output_path(out.clone())
        .file_allocation(FileAllocation::None)
        .resume(false)
        .max_connections(1);

    let dl = Downloader::builder().build().unwrap();
    let handle = dl.download(spec);
    handle.wait().await.unwrap();

    let content = std::fs::read(&out).unwrap();
    assert!(content.is_empty(), "empty file should have 0 bytes");

    // No control file should exist for a 0-byte download
    let ctrl = out.with_extension("bin.bytehaul");
    assert!(!ctrl.exists(), "control file should not exist for empty download");
}

#[tokio::test]
async fn test_cancel_and_pause_race() {
    // Start a slow server that drips data slowly
    let route = warp::path("slow").map(|| {
        let body = vec![0xAAu8; 1024 * 1024]; // 1 MB
        warp::http::Response::builder()
            .header("content-length", body.len().to_string())
            .body(body)
            .unwrap()
    });
    let (addr, server) = warp::serve(route).bind_ephemeral(([127, 0, 0, 1], 0));
    tokio::spawn(server);

    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("race.bin");

    let spec = DownloadSpec::new(format!("http://{addr}/slow"))
        .output_path(out.clone())
        .file_allocation(FileAllocation::None)
        .resume(false)
        .max_connections(1)
        .max_download_speed(1024); // Slow it down so we can race

    let dl = Downloader::builder().build().unwrap();
    let handle = dl.download(spec);

    // Fire both cancel and pause nearly simultaneously
    // The last write wins in a watch channel
    handle.pause();
    handle.cancel();

    let result = handle.wait().await;
    assert!(result.is_err(), "download should fail after cancel+pause race");
    // Either Cancelled or Paused is acceptable — no panic or hang is the key assertion
    let err = result.unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("cancelled") || msg.contains("paused"),
        "expected cancel or pause error, got: {msg}"
    );
}

#[tokio::test]
async fn test_corrupted_control_file_recovery() {
    let data = vec![0xBBu8; 4096];
    let expected = data.clone();

    let route = warp::path("recover").map(move || {
        warp::http::Response::builder()
            .header("content-length", data.len().to_string())
            .header("accept-ranges", "bytes")
            .body(data.clone())
            .unwrap()
    });
    let (addr, server) = warp::serve(route).bind_ephemeral(([127, 0, 0, 1], 0));
    tokio::spawn(server);

    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("recover.bin");
    let ctrl = out.with_extension("bin.bytehaul");

    // Write garbage as the control file (simulating corruption)
    std::fs::write(&ctrl, b"this is not a valid control file format").unwrap();

    let spec = DownloadSpec::new(format!("http://{addr}/recover"))
        .output_path(out.clone())
        .file_allocation(FileAllocation::None)
        .resume(true)
        .max_connections(1);

    let dl = Downloader::builder().build().unwrap();
    let handle = dl.download(spec);
    let result = handle.wait().await;

    // The download should either succeed by re-downloading from scratch,
    // or fail with a clear "corrupted control file" error — not panic.
    match result {
        Ok(()) => {
            let content = std::fs::read(&out).unwrap();
            assert_eq!(content, expected);
        }
        Err(err) => {
            let msg = err.to_string();
            assert!(
                msg.contains("control") || msg.contains("corrupt") || msg.contains("resume"),
                "unexpected error: {msg}"
            );
        }
    }
}

#[tokio::test]
async fn test_dns_failure_propagates() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("dns_fail.bin");

    let spec = DownloadSpec::new("http://this.domain.definitely.does.not.exist.invalid/file")
        .output_path(out.clone())
        .file_allocation(FileAllocation::None)
        .resume(false)
        .max_connections(1);

    let dl = Downloader::builder().build().unwrap();
    let handle = dl.download(spec);
    let result = handle.wait().await;

    assert!(result.is_err(), "DNS failure should result in error");
    // The error should propagate, not panic
}
