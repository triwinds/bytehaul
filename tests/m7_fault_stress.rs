use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use bytehaul::{DownloadSpec, Downloader, FileAllocation};
use warp::Filter;

// ──────────────────────────────────────────────────────────────
//  Fault injection: random disconnect mid-stream
// ──────────────────────────────────────────────────────────────

/// Server that drops the connection after sending `cutoff` bytes.
fn cutoff_server(data: Vec<u8>, cutoff: usize) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
    let route = warp::path("file").map(move || {
        let partial = data[..cutoff].to_vec();
        warp::http::Response::builder()
            .header("content-length", data.len().to_string())
            .body(partial)
            .unwrap()
    });
    let (addr, server) = warp::serve(route).bind_ephemeral(([127, 0, 0, 1], 0));
    let handle = tokio::spawn(server);
    (addr, handle)
}

#[tokio::test]
async fn test_early_eof_single_connection() {
    // Server claims 100KB but only sends 10KB — should fail
    let data = vec![0xABu8; 100_000];
    let (addr, handle) = cutoff_server(data, 10_000);

    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("eof.bin");

    let mut spec = DownloadSpec::new(format!("http://{addr}/file"), &out);
    spec.max_connections = 1;
    spec.file_allocation = FileAllocation::None;
    spec.resume = false;
    spec.read_timeout = Duration::from_secs(2);
    spec.max_retries = 0;

    let dl = Downloader::builder().build().unwrap();
    let h = dl.download(spec);
    let result = h.wait().await;
    // Should fail with some IO/HTTP error (body incomplete)
    assert!(result.is_err(), "early EOF should cause an error");

    handle.abort();
}

// ──────────────────────────────────────────────────────────────
//  Fault injection: 503 then recovery (transient fault)
// ──────────────────────────────────────────────────────────────

fn flaky_then_ok_server(
    data: Vec<u8>,
    fail_count: usize,
) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
    let counter = Arc::new(AtomicUsize::new(0));
    let route = warp::path("file").map(move || {
        let n = counter.fetch_add(1, Ordering::SeqCst);
        if n < fail_count {
            warp::http::Response::builder()
                .status(503)
                .header("retry-after", "0")
                .body(Vec::new())
                .unwrap()
        } else {
            warp::http::Response::builder()
                .header("content-length", data.len().to_string())
                .body(data.clone())
                .unwrap()
        }
    });
    let (addr, server) = warp::serve(route).bind_ephemeral(([127, 0, 0, 1], 0));
    let handle = tokio::spawn(server);
    (addr, handle)
}

#[tokio::test]
async fn test_transient_503_recovery() {
    let data: Vec<u8> = (0..50_000u32).map(|i| (i % 251) as u8).collect();
    let expected = data.clone();
    let (addr, handle) = flaky_then_ok_server(data, 2); // fail first 2 times

    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("recovered.bin");

    let mut spec = DownloadSpec::new(format!("http://{addr}/file"), &out);
    spec.max_connections = 1;
    spec.file_allocation = FileAllocation::None;
    spec.resume = false;
    spec.max_retries = 5;
    spec.retry_base_delay = Duration::from_millis(50);
    spec.retry_max_delay = Duration::from_millis(200);

    let dl = Downloader::builder().build().unwrap();
    let h = dl.download(spec);
    h.wait().await.unwrap();

    let content = std::fs::read(&out).unwrap();
    assert_eq!(content, expected);

    handle.abort();
}

// ──────────────────────────────────────────────────────────────
//  Fault injection: permanent failure (all 404)
// ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_permanent_failure_404() {
    let route = warp::path("file").map(|| {
        warp::http::Response::builder()
            .status(404)
            .body(Vec::<u8>::new())
            .unwrap()
    });
    let (addr, server) = warp::serve(route).bind_ephemeral(([127, 0, 0, 1], 0));
    tokio::spawn(server);

    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("notfound.bin");

    let mut spec = DownloadSpec::new(format!("http://{addr}/file"), &out);
    spec.max_connections = 1;
    spec.file_allocation = FileAllocation::None;
    spec.resume = false;

    let dl = Downloader::builder().build().unwrap();
    let h = dl.download(spec);
    let result = h.wait().await;
    assert!(result.is_err());
}

// ──────────────────────────────────────────────────────────────
//  Fault injection: corrupted control file
// ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_corrupted_control_file_recovery() {
    let data: Vec<u8> = (0..80_000u32).map(|i| (i % 199) as u8).collect();
    let expected = data.clone();

    let route = warp::path("file").map(move || warp::http::Response::new(data.clone()));
    let (addr, server) = warp::serve(route).bind_ephemeral(([127, 0, 0, 1], 0));
    tokio::spawn(server);

    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("corrupted_ctrl.bin");
    let ctrl_path = {
        let mut p = out.as_os_str().to_os_string();
        p.push(".bytehaul");
        std::path::PathBuf::from(p)
    };

    // Write garbage as control file
    std::fs::write(&ctrl_path, b"this is not a valid control file").unwrap();

    let mut spec = DownloadSpec::new(format!("http://{addr}/file"), &out);
    spec.max_connections = 1;
    spec.file_allocation = FileAllocation::None;
    spec.resume = true; // will try to load corrupted control file

    let dl = Downloader::builder().build().unwrap();
    let h = dl.download(spec);
    // Should recover by discarding corrupted control file and downloading fresh
    h.wait().await.unwrap();

    let content = std::fs::read(&out).unwrap();
    assert_eq!(content, expected);

    // Control file should be cleaned up after successful download
    assert!(!ctrl_path.exists());
}

// ──────────────────────────────────────────────────────────────
//  Stress: large file multi-connection
// ──────────────────────────────────────────────────────────────

fn range_server(data: Vec<u8>) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
    let total = data.len();
    let data = Arc::new(data);
    let route = warp::path("file")
        .and(warp::header::optional::<String>("range"))
        .map(move |range: Option<String>| {
            if let Some(range_str) = range {
                // Parse "bytes=start-end"
                let range_str = range_str.trim_start_matches("bytes=");
                let parts: Vec<&str> = range_str.split('-').collect();
                let start: u64 = parts[0].parse().unwrap_or(0);
                let end: u64 = parts
                    .get(1)
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(total as u64 - 1)
                    .min(total as u64 - 1);
                let slice = &data[start as usize..=end as usize];
                warp::http::Response::builder()
                    .status(206)
                    .header(
                        "content-range",
                        format!("bytes {start}-{end}/{total}"),
                    )
                    .header("content-length", slice.len().to_string())
                    .header("etag", "\"stress-test\"")
                    .body(slice.to_vec())
                    .unwrap()
            } else {
                warp::http::Response::builder()
                    .header("content-length", total.to_string())
                    .header("etag", "\"stress-test\"")
                    .body((*data).clone())
                    .unwrap()
            }
        });
    let (addr, server) = warp::serve(route).bind_ephemeral(([127, 0, 0, 1], 0));
    let handle = tokio::spawn(server);
    (addr, handle)
}

#[tokio::test]
async fn test_stress_large_file_multi_worker() {
    // 2 MB file with 4 workers
    let data: Vec<u8> = (0..2_000_000u32).map(|i| (i % 251) as u8).collect();
    let expected = data.clone();
    let (addr, handle) = range_server(data);

    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("stress_large.bin");

    let mut spec = DownloadSpec::new(format!("http://{addr}/file"), &out);
    spec.max_connections = 4;
    spec.file_allocation = FileAllocation::Prealloc;
    spec.resume = false;
    spec.piece_size = 256 * 1024; // 256 KB pieces
    spec.min_split_size = 100_000;

    let dl = Downloader::builder().build().unwrap();
    let h = dl.download(spec);
    h.wait().await.unwrap();

    let content = std::fs::read(&out).unwrap();
    assert_eq!(content.len(), expected.len());
    assert_eq!(content, expected);

    handle.abort();
}

#[tokio::test]
async fn test_stress_many_small_pieces() {
    // 500 KB file with small pieces (16 KB each ≈ 32 pieces, 4 workers)
    let data: Vec<u8> = (0..500_000u32).map(|i| (i % 199) as u8).collect();
    let expected = data.clone();
    let (addr, handle) = range_server(data);

    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("stress_small_pieces.bin");

    let mut spec = DownloadSpec::new(format!("http://{addr}/file"), &out);
    spec.max_connections = 4;
    spec.file_allocation = FileAllocation::Prealloc;
    spec.resume = false;
    spec.piece_size = 16 * 1024; // 16 KB pieces
    spec.min_split_size = 10_000;

    let dl = Downloader::builder().build().unwrap();
    let h = dl.download(spec);
    h.wait().await.unwrap();

    let content = std::fs::read(&out).unwrap();
    assert_eq!(content.len(), expected.len());
    assert_eq!(content, expected);

    handle.abort();
}

// ──────────────────────────────────────────────────────────────
//  Stress: concurrent downloads
// ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_stress_concurrent_downloads() {
    let data: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
    let expected = data.clone();

    let route = warp::path("file").map(move || warp::http::Response::new(data.clone()));
    let (addr, server) = warp::serve(route).bind_ephemeral(([127, 0, 0, 1], 0));
    tokio::spawn(server);

    let dir = tempfile::tempdir().unwrap();
    let dl = Downloader::builder().build().unwrap();

    let mut handles = Vec::new();
    for i in 0..5 {
        let out = dir.path().join(format!("concurrent_{i}.bin"));
        let mut spec = DownloadSpec::new(format!("http://{addr}/file"), &out);
        spec.max_connections = 1;
        spec.file_allocation = FileAllocation::None;
        spec.resume = false;
        handles.push(dl.download(spec));
    }

    for h in handles {
        h.wait().await.unwrap();
    }

    for i in 0..5 {
        let out = dir.path().join(format!("concurrent_{i}.bin"));
        let content = std::fs::read(&out).unwrap();
        assert_eq!(content, expected);
    }
}

// ──────────────────────────────────────────────────────────────
//  Fault: cancel during download
// ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_cancel_mid_download_progress() {
    // Large slow data — cancel quickly
    let data = vec![0xFFu8; 1_000_000];

    let route = warp::path("file").map(move || {
        warp::http::Response::builder()
            .header("content-length", data.len().to_string())
            .body(data.clone())
            .unwrap()
    });
    let (addr, server) = warp::serve(route).bind_ephemeral(([127, 0, 0, 1], 0));
    tokio::spawn(server);

    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("cancel_mid.bin");

    let mut spec = DownloadSpec::new(format!("http://{addr}/file"), &out);
    spec.max_connections = 1;
    spec.file_allocation = FileAllocation::None;
    spec.resume = false;
    spec.max_download_speed = 1024; // slow download so we can cancel mid-stream

    let dl = Downloader::builder().build().unwrap();
    let h = dl.download(spec);

    // Wait a bit then cancel
    tokio::time::sleep(Duration::from_millis(200)).await;
    h.cancel();

    let result = h.wait().await;
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(
        err.to_string().contains("cancelled"),
        "expected cancelled error, got: {err}"
    );
}
