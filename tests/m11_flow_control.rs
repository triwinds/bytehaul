use std::convert::Infallible;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use bytehaul::{DownloadError, DownloadSpec, Downloader, FileAllocation};
use tokio::sync::Notify;
use warp::Filter;

fn range_server(
    content: Vec<u8>,
    body_started: Arc<Notify>,
) -> (
    std::net::SocketAddr,
    Arc<AtomicUsize>,
    Arc<AtomicUsize>,
    impl std::future::Future<Output = ()>,
) {
    let data = Arc::new(content);
    let range_requests = Arc::new(AtomicUsize::new(0));
    let counter = range_requests.clone();
    let longest = Arc::new(AtomicUsize::new(0));
    let request_length = longest.clone();
    let route = warp::path("data")
        .and(warp::header::optional::<String>("range"))
        .map(move |range: Option<String>| {
            let total = data.len();
            let (start, end) = if let Some(value) = range.as_deref() {
                counter.fetch_add(1, Ordering::Relaxed);
                let (start, end) = value
                    .strip_prefix("bytes=")
                    .unwrap()
                    .split_once('-')
                    .unwrap();
                (
                    start.parse::<usize>().unwrap(),
                    end.parse::<usize>().unwrap().min(total - 1),
                )
            } else {
                (0, total - 1)
            };
            request_length.fetch_max(end - start + 1, Ordering::SeqCst);
            let payload = data[start..=end].to_vec();
            let started = body_started.clone();
            let stream = futures::stream::once(async move {
                started.notify_one();
                Ok::<_, Infallible>(payload)
            });
            let mut response = warp::http::Response::builder()
                .status(if range.is_some() { 206 } else { 200 })
                .header("content-length", end - start + 1)
                .header("etag", "\"flow-control\"")
                .header("accept-ranges", "bytes");
            if range.is_some() {
                response = response.header("content-range", format!("bytes {start}-{end}/{total}"));
            }
            response
                .body(warp::hyper::Body::wrap_stream(stream))
                .unwrap()
        });
    let (addr, server) = warp::serve(route).bind_ephemeral(([127, 0, 0, 1], 0));
    (addr, range_requests, longest, server)
}

#[tokio::test]
async fn tiny_and_nondivisible_budgets_preserve_single_and_multi_downloads() {
    for connections in [1, 4] {
        for budget in [1, 100, 4097] {
            let size = if budget == 1 { 31 } else { 32 * 1024 + 13 };
            let expected: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
            let (addr, requests, _, server) =
                range_server(expected.clone(), Arc::new(Notify::new()));
            let server = tokio::spawn(server);
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("data.bin");
            let spec = DownloadSpec::new(format!("http://{addr}/data"))
                .output_path(&path)
                .file_allocation(FileAllocation::None)
                .max_connections(connections)
                .min_split_size(1)
                .piece_size(if budget == 1 { 7 } else { 8191 })
                .min_segment_size(1)
                .memory_budget(budget)
                .channel_buffer(1)
                .resume(false);
            let downloader = Downloader::builder().build().unwrap();
            let outcome =
                tokio::time::timeout(Duration::from_secs(15), downloader.download(spec).wait())
                    .await;
            server.abort();
            outcome
                .unwrap_or_else(|_| panic!("stalled: connections={connections}, budget={budget}"))
                .unwrap();
            assert_eq!(tokio::fs::read(path).await.unwrap(), expected);
            if connections > 1 {
                assert!(
                    requests.load(Ordering::Relaxed) > 1,
                    "must exercise multiple range leases"
                );
            }
        }
    }
}

#[tokio::test]
async fn pause_and_cancel_interrupt_rate_limited_body_waits() {
    for (connections, batch) in [(1, 0), (4, 0), (4, 65536)] {
        for pause in [false, true] {
            let size = if batch > 0 { 262145 } else { 65537 };
            let expected: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
            let started = Arc::new(Notify::new());
            let (addr, _, longest, server) = range_server(expected.clone(), started.clone());
            let server = tokio::spawn(server);
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("stoppable.bin");
            let spec = DownloadSpec::new(format!("http://{addr}/data"))
                .output_path(&path)
                .file_allocation(FileAllocation::None)
                .max_connections(connections)
                .min_split_size(1)
                .piece_size(16384)
                .request_batch_size(batch)
                .memory_budget(97)
                .channel_buffer(1)
                .max_download_speed(64);
            let downloader = Downloader::builder().build().unwrap();
            let handle = downloader.download(spec.clone());
            tokio::time::timeout(Duration::from_secs(5), started.notified())
                .await
                .unwrap();
            // Wait for actual forwarded data before stopping the next limited
            // chunk. This also exercises a nonzero durable prefix for single.
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    if tokio::fs::metadata(&path)
                        .await
                        .is_ok_and(|meta| meta.len() > 0)
                        && (batch == 0 || longest.load(Ordering::SeqCst) > 16384)
                    {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .expect("initial limited chunk never reached the writer");
            if batch > 0 {
                assert!(
                    longest.load(Ordering::SeqCst) > 16384,
                    "stop must exercise a batched body"
                );
            }
            if pause {
                handle.pause();
            } else {
                handle.cancel();
            }
            let error = tokio::time::timeout(Duration::from_secs(2), handle.wait())
                .await
                .expect("stop must interrupt limiter/budget waits")
                .unwrap_err();
            assert!(if pause {
                matches!(error, DownloadError::Paused)
            } else {
                matches!(error, DownloadError::Cancelled)
            });
            if connections == 1 {
                let prefix = tokio::fs::read(&path).await.unwrap();
                assert!(!prefix.is_empty());
                assert!(prefix.len() < expected.len());
                assert_eq!(prefix, expected[..prefix.len()]);
            }
            tokio::time::timeout(
                Duration::from_secs(15),
                downloader.download(spec.max_download_speed(0)).wait(),
            )
            .await
            .expect("resumed transfer stalled")
            .unwrap();
            assert_eq!(tokio::fs::read(path).await.unwrap(), expected);
            server.abort();
        }
    }
}
