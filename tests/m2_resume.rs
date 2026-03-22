use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use bytehaul::{DownloadSpec, Downloader, FileAllocation};
use futures::StreamExt;
use warp::Filter;

/// Helper: serve a file with proper Content-Length and support for Range requests.
fn range_server(
    path_segment: &'static str,
    data: Vec<u8>,
) -> (std::net::SocketAddr, impl std::future::Future<Output = ()>) {
    let data = Arc::new(data);
    let d = data.clone();

    let route = warp::path(path_segment)
        .and(warp::header::optional::<String>("range"))
        .map(move |range_header: Option<String>| {
            let data = d.clone();
            match range_header {
                Some(range) => {
                    // Parse "bytes=START-END"
                    let range = range.trim_start_matches("bytes=");
                    let parts: Vec<&str> = range.split('-').collect();
                    let start: u64 = parts[0].parse().unwrap_or(0);
                    let end: u64 = if parts.len() > 1 && !parts[1].is_empty() {
                        parts[1].parse().unwrap_or(data.len() as u64 - 1)
                    } else {
                        data.len() as u64 - 1
                    };
                    let slice = &data[start as usize..=end as usize];
                    warp::http::Response::builder()
                        .status(206)
                        .header("content-length", slice.len().to_string())
                        .header(
                            "content-range",
                            format!("bytes {}-{}/{}", start, end, data.len()),
                        )
                        .header("accept-ranges", "bytes")
                        .header("etag", "\"test-etag-123\"")
                        .header("last-modified", "Sat, 01 Jan 2026 00:00:00 GMT")
                        .body(Vec::from(slice))
                        .unwrap()
                }
                None => warp::http::Response::builder()
                    .status(200)
                    .header("content-length", data.len().to_string())
                    .header("accept-ranges", "bytes")
                    .header("etag", "\"test-etag-123\"")
                    .header("last-modified", "Sat, 01 Jan 2026 00:00:00 GMT")
                    .body(data.to_vec())
                    .unwrap(),
            }
        });

    warp::serve(route).bind_ephemeral(([127, 0, 0, 1], 0))
}

/// Helper: serve a slow stream that can be interrupted.
fn slow_range_server(
    path_segment: &'static str,
    data: Vec<u8>,
    served_bytes: Arc<AtomicU64>,
) -> (std::net::SocketAddr, impl std::future::Future<Output = ()>) {
    let data = Arc::new(data);
    let d = data.clone();

    let route = warp::path(path_segment)
        .and(warp::header::optional::<String>("range"))
        .map(move |range_header: Option<String>| {
            let data = d.clone();
            let served = served_bytes.clone();

            let start: u64 = range_header
                .as_deref()
                .and_then(|r| r.trim_start_matches("bytes=").split('-').next())
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);

            let total = data.len();
            let remaining = &data[start as usize..];
            let chunk_size = 5_000usize;
            let chunks: Vec<Result<Vec<u8>, std::convert::Infallible>> = remaining
                .chunks(chunk_size)
                .map(|c| Ok(c.to_vec()))
                .collect();
            let served2 = served.clone();
            let stream = futures::stream::iter(chunks).then(
                move |chunk: Result<Vec<u8>, std::convert::Infallible>| {
                    let served = served2.clone();
                    async move {
                        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
                        let Ok(ref c) = chunk;
                        served.fetch_add(c.len() as u64, Ordering::Relaxed);
                        chunk
                    }
                },
            );
            let body = warp::hyper::Body::wrap_stream(stream);

            let status = if start > 0 { 206 } else { 200 };
            let mut builder = warp::http::Response::builder()
                .status(status)
                .header("content-length", (total as u64 - start).to_string())
                .header("accept-ranges", "bytes")
                .header("etag", "\"slow-etag\"")
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

#[tokio::test]
async fn test_resume_after_cancel() {
    let content: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
    let expected = content.clone();
    let served = Arc::new(AtomicU64::new(0));

    let (addr, server) = slow_range_server("resumefile", content, served.clone());
    tokio::spawn(server);

    let dir = tempfile::tempdir().unwrap();
    let output_path = dir.path().join("resumed.bin");

    let downloader = Downloader::builder().build().unwrap();

    // First download: let it run a bit, then cancel
    let mut spec = DownloadSpec::new(format!("http://{addr}/resumefile"), &output_path);
    spec.file_allocation = FileAllocation::None;
    let handle = downloader.download(spec.clone());

    // Wait for some data to arrive, then cancel
    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    handle.cancel();
    let _ = handle.wait().await;

    let partial_size = std::fs::metadata(&output_path)
        .map(|m| m.len())
        .unwrap_or(0);
    assert!(
        partial_size > 0,
        "should have written some data before cancel"
    );

    // Control file should exist
    let ctrl_path = output_path.with_file_name("resumed.bin.bytehaul");
    assert!(ctrl_path.exists(), "control file should exist after cancel");

    // Second download: should resume from where we left off
    served.store(0, Ordering::Relaxed);
    let handle2 = downloader.download(spec);
    handle2.wait().await.unwrap();

    let downloaded = std::fs::read(&output_path).unwrap();
    assert_eq!(downloaded.len(), expected.len());
    assert_eq!(downloaded, expected);

    // Control file should be cleaned up after success
    assert!(
        !ctrl_path.exists(),
        "control file should be deleted on success"
    );
}

#[tokio::test]
async fn test_resume_etag_mismatch_restarts() {
    // First server with etag "A"
    let content_v1: Vec<u8> = vec![0xAA; 50_000];
    let data_v1 = Arc::new(content_v1);
    let d1 = data_v1.clone();

    let route1 = warp::path("etagfile").map(move || {
        warp::http::Response::builder()
            .header("content-length", d1.len().to_string())
            .header("etag", "\"version-A\"")
            .body(d1.to_vec())
            .unwrap()
    });
    let (addr, server1) = warp::serve(route1).bind_ephemeral(([127, 0, 0, 1], 0));
    tokio::spawn(server1);

    let dir = tempfile::tempdir().unwrap();
    let output_path = dir.path().join("etag.bin");
    let ctrl_path = output_path.with_file_name("etag.bin.bytehaul");

    // Manually create a control file that claims partial download with a different etag
    let fake_ctrl = bytehaul_test_support::make_control_snapshot(
        &format!("http://{addr}/etagfile"),
        50_000,
        25_000,
        Some("\"version-OLD\""), // mismatched etag
        None,
    );
    // Write partial data
    std::fs::write(&output_path, vec![0xBB; 25_000]).unwrap();
    fake_ctrl.save(&ctrl_path).await;

    // Download should detect mismatch, discard old state, and start fresh
    let downloader = Downloader::builder().build().unwrap();
    let mut spec = DownloadSpec::new(format!("http://{addr}/etagfile"), &output_path);
    spec.file_allocation = FileAllocation::None;

    let handle = downloader.download(spec);
    handle.wait().await.unwrap();

    let downloaded = std::fs::read(&output_path).unwrap();
    assert_eq!(downloaded, data_v1.as_ref().as_slice());
}

#[tokio::test]
async fn test_no_resume_when_disabled() {
    let content: Vec<u8> = vec![0x55; 30_000];
    let expected = content.clone();

    let (addr, server) = range_server("noresume", content);
    tokio::spawn(server);

    let dir = tempfile::tempdir().unwrap();
    let output_path = dir.path().join("noresume.bin");

    let downloader = Downloader::builder().build().unwrap();
    let mut spec = DownloadSpec::new(format!("http://{addr}/noresume"), &output_path);
    spec.file_allocation = FileAllocation::None;
    spec.resume = false;

    let handle = downloader.download(spec);
    handle.wait().await.unwrap();

    let downloaded = std::fs::read(&output_path).unwrap();
    assert_eq!(downloaded, expected);

    // No control file should exist
    let ctrl_path = output_path.with_file_name("noresume.bin.bytehaul");
    assert!(!ctrl_path.exists());
}

/// Helper module to access internal types for testing.
mod bytehaul_test_support {
    use std::path::Path;

    /// Minimal control snapshot for test setup.
    pub struct FakeControlSnapshot {
        url: String,
        total_size: u64,
        downloaded_bytes: u64,
        etag: Option<String>,
        last_modified: Option<String>,
    }

    impl FakeControlSnapshot {
        pub async fn save(&self, path: &Path) {
            use std::io::Write;

            let payload = ControlPayload {
                url: self.url.clone(),
                total_size: self.total_size,
                downloaded_bytes: self.downloaded_bytes,
                etag: self.etag.clone(),
                last_modified: self.last_modified.clone(),
                piece_size: self.total_size,
            };
            let encoded = bincode::serialize(&payload).unwrap();
            let checksum = crc32fast::hash(&encoded);

            let mut file = std::fs::File::create(path).unwrap();
            file.write_all(&0x4259_4845u32.to_le_bytes()).unwrap(); // magic
            file.write_all(&1u32.to_le_bytes()).unwrap(); // version
            file.write_all(&(encoded.len() as u32).to_le_bytes())
                .unwrap();
            file.write_all(&checksum.to_le_bytes()).unwrap();
            file.write_all(&encoded).unwrap();
            file.sync_all().unwrap();
        }
    }

    #[derive(serde::Serialize)]
    struct ControlPayload {
        url: String,
        total_size: u64,
        downloaded_bytes: u64,
        etag: Option<String>,
        last_modified: Option<String>,
        piece_size: u64,
    }

    pub fn make_control_snapshot(
        url: &str,
        total_size: u64,
        downloaded_bytes: u64,
        etag: Option<&str>,
        last_modified: Option<&str>,
    ) -> FakeControlSnapshot {
        FakeControlSnapshot {
            url: url.to_string(),
            total_size,
            downloaded_bytes,
            etag: etag.map(String::from),
            last_modified: last_modified.map(String::from),
        }
    }
}
