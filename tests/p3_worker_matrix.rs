use bytehaul::{DownloadSpec, Downloader, FileAllocation, RangeSchedulingMode, SlowTransferMode};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use warp::Filter;

/// Every mode must consume the exact probe once, cover each byte once, and
/// retain ordinary batching/dynamic planning with performance recovery off.
#[tokio::test]
async fn all_worker_modes_preserve_probe_ranges_and_cross_piece_frames() {
    const PIECE: u64 = 4096;
    const TOTAL: u64 = PIECE * 32;
    for recovery in [
        SlowTransferMode::Disabled,
        SlowTransferMode::Adaptive,
        SlowTransferMode::AdaptiveWithHedging,
    ] {
        for scheduling in [RangeSchedulingMode::Fixed, RangeSchedulingMode::Dynamic] {
            for batch in [0, PIECE * 4] {
                let ranges = Arc::new(Mutex::new(Vec::new()));
                let observed = ranges.clone();
                let route = warp::header::<String>("range").map(move |range: String| {
                    let (start, end) = range
                        .strip_prefix("bytes=")
                        .unwrap()
                        .split_once('-')
                        .unwrap();
                    let start = start.parse::<u64>().unwrap();
                    let end = end.parse::<u64>().unwrap() + 1;
                    assert!(start < end && end <= TOTAL);
                    observed.lock().unwrap().push((start, end));
                    let bytes: Vec<u8> = (start..end).map(|n| (n % 251) as u8).collect();
                    warp::http::Response::builder()
                        .status(206)
                        .header(
                            "Content-Range",
                            format!("bytes {start}-{}/{TOTAL}", end - 1),
                        )
                        .header("Content-Length", end - start)
                        .header("ETag", "\"stable\"")
                        .body(bytes)
                        .unwrap()
                });
                let (addr, server) = warp::serve(route).bind_ephemeral(([127, 0, 0, 1], 0));
                let server = tokio::spawn(server);
                let dir = tempfile::tempdir().unwrap();
                let path = dir.path().join("matrix.bin");
                let spec = DownloadSpec::new(format!("http://{addr}/file"))
                    .output_path(&path)
                    .max_connections(3)
                    .piece_size(PIECE)
                    .min_split_size(1)
                    .min_segment_size(PIECE)
                    .resume(false)
                    .file_allocation(FileAllocation::None)
                    .memory_budget(127)
                    .slow_transfer_mode(recovery)
                    .range_scheduling_mode(scheduling)
                    .request_batch_size(batch)
                    .dynamic_min_split_size(PIECE)
                    .dynamic_max_request_size(PIECE * 8);
                let result = tokio::time::timeout(
                    Duration::from_secs(15),
                    Downloader::builder().build().unwrap().download(spec).wait(),
                )
                .await;
                server.abort();
                result
                    .expect("mode must make progress with a tiny budget")
                    .unwrap();
                assert_eq!(
                    std::fs::read(path).unwrap(),
                    (0..TOTAL).map(|n| (n % 251) as u8).collect::<Vec<_>>()
                );
                let mut ranges = ranges.lock().unwrap().clone();
                assert_eq!(
                    ranges.iter().filter(|range| **range == (0, PIECE)).count(),
                    1
                );
                ranges.sort_unstable();
                let mut next = 0;
                for &(start, end) in &ranges {
                    assert_eq!(
                        start, next,
                        "gap or duplicate in {recovery:?}/{scheduling:?}/{batch}: {ranges:?}"
                    );
                    next = end;
                }
                assert_eq!(next, TOTAL);
                if scheduling == RangeSchedulingMode::Dynamic || batch > 0 {
                    assert!(ranges.iter().any(|(start, end)| end - start > PIECE));
                } else {
                    assert!(ranges.iter().all(|(start, end)| end - start <= PIECE));
                }
            }
        }
    }
}
