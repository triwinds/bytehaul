use super::*;
use crate::config::{FileAllocation, SlowTransferMode};
use std::sync::atomic::AtomicBool;
use warp::Filter;

const PIECE: u64 = 32;
const TOTAL: u64 = PIECE * 12;
type Requests = Arc<parking_lot::Mutex<Vec<(u64, u64, Option<String>)>>>;

fn bytes(start: u64, end: u64) -> Vec<u8> {
    (start..end).map(|i| (i % 251) as u8).collect()
}

struct Origin {
    url: String,
    requests: Requests,
    server: tokio::task::JoinHandle<()>,
}
impl Drop for Origin {
    fn drop(&mut self) {
        self.server.abort();
    }
}

fn origin(
    fail_prefix: bool,
    changed: bool,
    overlong: bool,
    validator: bool,
    repeat: bool,
) -> Origin {
    let requests: Requests = Arc::default();
    let recorded = requests.clone();
    let failed = Arc::new(AtomicBool::new(false));
    let route = warp::header::<String>("range")
        .and(warp::header::optional::<String>("if-match"))
        .map(move |range: String, condition: Option<String>| {
            let (start, end) = range
                .strip_prefix("bytes=")
                .unwrap()
                .split_once('-')
                .unwrap();
            let start = start.parse::<u64>().unwrap();
            let end = end.parse::<u64>().unwrap() + 1;
            recorded.lock().push((start, end, condition.clone()));
            let fail = fail_prefix
                && ((start == 0 && !failed.swap(true, Ordering::SeqCst))
                    || (repeat && start < 2 * PIECE && start != 0 && start != PIECE));
            let is_changed =
                changed && failed.load(Ordering::SeqCst) && !fail && start > 0 && start < 2 * PIECE;
            let status = if is_changed && condition.as_deref() == Some("\"v1\"") {
                412
            } else {
                206
            };
            let (mut sender, body) = warp::hyper::Body::channel();
            tokio::spawn(async move {
                if status == 412 {
                    return;
                }
                let body_end = if fail {
                    start + (end - start).min(40).min((end - start) / 2 + 1)
                } else {
                    end
                };
                let _ = sender.send_data(bytes(start, body_end).into()).await;
                if fail || overlong {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                if overlong {
                    let _ = sender.send_data(bytes(end, end + 1).into()).await;
                }
            });
            let mut response = warp::http::Response::builder().status(status).header(
                "content-range",
                format!("bytes {start}-{}/{TOTAL}", end - 1),
            );
            if !overlong {
                response = response.header(
                    "content-length",
                    if status == 412 { 0 } else { end - start },
                );
            }
            if validator {
                response = response.header("etag", if is_changed { "\"v2\"" } else { "\"v1\"" });
            }
            response.body(body).unwrap()
        });
    let (address, server) = warp::serve(route).bind_ephemeral(([127, 0, 0, 1], 0));
    Origin {
        url: format!("http://{address}/file"),
        requests,
        server: tokio::spawn(server),
    }
}

async fn run(
    origin: &Origin,
    mode: SlowTransferMode,
    batch: u64,
    validator: bool,
    completed: &[usize],
    retries: u32,
    memory: usize,
) -> (
    Result<(), DownloadError>,
    Vec<u8>,
    ProgressSnapshot,
    SchedulerState,
) {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("file");
    let mut pieces = PieceMap::new(TOTAL, PIECE);
    let mut initial = vec![0xFF; TOTAL as usize];
    for &id in completed {
        pieces.mark_complete(id);
        let start = id as u64 * PIECE;
        initial[start as usize..(start + PIECE) as usize]
            .copy_from_slice(&bytes(start, start + PIECE));
    }
    if !completed.is_empty() {
        tokio::fs::write(&output, initial).await.unwrap();
    }
    let spec = DownloadSpec::new(&origin.url)
        .output_path(&output)
        .resume(true)
        .max_connections(2)
        .piece_size(PIECE)
        .min_segment_size(PIECE)
        .min_split_size(1)
        .request_batch_size(batch)
        .slow_transfer_mode(mode)
        .max_retries(retries)
        .retry_base_delay(Duration::from_millis(1))
        .retry_max_delay(Duration::from_millis(2))
        .memory_budget(memory)
        .channel_buffer(1)
        .file_allocation(FileAllocation::None);
    let meta = ResponseMeta {
        content_length: Some(TOTAL),
        content_range_start: Some(0),
        content_range_end: Some(PIECE - 1),
        content_range_total: Some(TOTAL),
        accept_ranges: true,
        etag: validator.then(|| "\"v1\"".into()),
        last_modified: None,
        content_disposition: None,
        content_encoding: None,
    };
    let (progress, _) = watch::channel(ProgressSnapshot::default());
    let (_stop, stop) = watch::channel(StopSignal::Running);
    let control = ControlSnapshot::control_path(&output);
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        run_multi_worker(
            crate::network::ClientNetworkConfig::default()
                .build_client()
                .unwrap(),
            &spec,
            &origin.url,
            &output,
            &meta,
            TOTAL,
            pieces,
            None,
            &progress,
            stop,
            &control,
            SpeedLimit::new(0),
            LogLevel::Off,
            0,
        ),
    )
    .await
    .expect("bounded fixture must finish");
    let saved = if control.exists() {
        let snapshot = ControlSnapshot::load(&control).await.unwrap();
        // Reconstruct only authoritative completion bits, never runtime prefixes.
        let mut map = PieceMap::new(TOTAL, PIECE);
        for id in 0..12 {
            if snapshot.completed_bitset[id / 8] & (1 << (id % 8)) != 0 {
                map.mark_complete(id);
            }
        }
        SchedulerState::new(map)
    } else {
        SchedulerState::new(PieceMap::new(TOTAL, PIECE))
    };
    let snapshot = progress.borrow().clone();
    (
        result,
        tokio::fs::read(output).await.unwrap(),
        snapshot,
        saved,
    )
}

#[tokio::test]
async fn batches_split_frames_at_piece_boundaries_and_skip_resume_holes() {
    for mode in [
        SlowTransferMode::Disabled,
        SlowTransferMode::Adaptive,
        SlowTransferMode::AdaptiveWithHedging,
    ] {
        for memory in [1, 19] {
            let server = origin(false, false, false, true, false);
            let (result, output, progress, _) =
                run(&server, mode, 128, true, &[3, 8], 0, memory).await;
            result.unwrap();
            assert_eq!(output, bytes(0, TOTAL));
            assert_eq!(progress.downloaded, TOTAL);
            let requests = server.requests.lock();
            assert!(
                requests.len() < 10,
                "batching must reduce requests: {requests:?}"
            );
            assert!(requests.iter().any(|(start, end, _)| end - start > PIECE));
            for (start, end, condition) in requests.iter() {
                assert!(*end - *start <= 128);
                assert_eq!(condition.as_deref(), Some("\"v1\""));
                for hole in [3 * PIECE, 8 * PIECE] {
                    assert!(*end <= hole || *start >= hole + PIECE);
                }
            }
        }
    }
}

#[tokio::test]
async fn interrupted_ranges_retry_only_confirmed_suffix_with_same_budget() {
    for mode in [
        SlowTransferMode::Disabled,
        SlowTransferMode::Adaptive,
        SlowTransferMode::AdaptiveWithHedging,
    ] {
        for batch in [0, 128] {
            for validator in [false, true] {
                let server = origin(true, false, false, validator, false);
                let (result, output, progress, _) =
                    run(&server, mode, batch, validator, &[], 1, 19).await;
                result.unwrap();
                assert_eq!(output, bytes(0, TOTAL));
                assert_eq!(progress.downloaded, TOTAL);
                let requests = server.requests.lock();
                let first = requests.iter().find(|(start, _, _)| *start == 0).unwrap();
                let retained = if first.1 > PIECE { 40 } else { 17 };
                let retry_start = if validator {
                    retained
                } else if first.1 > PIECE {
                    PIECE
                } else {
                    0
                };
                assert!(
                    requests
                        .iter()
                        .skip(1)
                        .any(|(start, end, _)| *start == retry_start
                            && *end == if first.1 > PIECE { 2 * PIECE } else { PIECE }),
                    "{requests:?}"
                );
                if validator {
                    assert_eq!(
                        requests.iter().filter(|(start, _, _)| *start == 0).count(),
                        1
                    );
                }
            }
        }
    }
}

#[tokio::test]
async fn failed_batch_preserves_only_completed_piece_and_rejects_changed_identity() {
    for changed in [false, true] {
        let server = origin(true, changed, false, true, false);
        let (result, _, _, mut saved) = run(
            &server,
            SlowTransferMode::Adaptive,
            128,
            true,
            &[],
            u32::from(changed),
            19,
        )
        .await;
        if changed {
            assert!(
                matches!(result, Err(DownloadError::HttpStatus { status: 412, .. })),
                "{result:?}"
            );
        } else {
            assert!(result.is_err());
        }
        let mut missing = Vec::new();
        while let Some(segment) = saved.assign() {
            missing.push(segment.piece_id);
        }
        assert!(
            !missing.contains(&0),
            "first flushed piece must survive truncated batch"
        );
        assert!(
            missing.contains(&1),
            "partial prefix must not become a durable piece bit"
        );
    }
}

#[tokio::test]
async fn oversized_batch_never_publishes_final_piece_completion() {
    let server = origin(false, false, true, true, false);
    let (result, _, _, mut saved) =
        run(&server, SlowTransferMode::Adaptive, 128, true, &[], 0, 19).await;
    assert!(
        matches!(result, Err(DownloadError::ResumeMismatch(_))),
        "{result:?}"
    );
    let first_end = server
        .requests
        .lock()
        .iter()
        .find(|(start, _, _)| *start == 0)
        .unwrap()
        .1;
    assert!(
        first_end > PIECE,
        "fixture must exercise a multi-piece response"
    );
    let mut missing = Vec::new();
    while let Some(segment) = saved.assign() {
        missing.push(segment.piece_id);
    }
    assert!(
        missing.contains(&((first_end / PIECE - 1) as usize)),
        "the malformed response's final piece must await validated EOF"
    );
}

#[tokio::test]
async fn repeated_suffix_failures_do_not_reset_retry_budget() {
    for mode in [
        SlowTransferMode::Disabled,
        SlowTransferMode::Adaptive,
        SlowTransferMode::AdaptiveWithHedging,
    ] {
        for batch in [0, 128] {
            let server = origin(true, false, false, true, true);
            let (result, _, _, _) = run(&server, mode, batch, true, &[], 1, 19).await;
            assert!(result.is_err());
            let requests = server.requests.lock();
            let first = requests.iter().find(|(start, _, _)| *start == 0).unwrap();
            let first_end = if first.1 > PIECE { 2 * PIECE } else { PIECE };
            let attempts: Vec<_> = requests
                .iter()
                .filter(|(start, _, _)| *start < first_end)
                .collect();
            assert_eq!(
                attempts.len(),
                2,
                "suffix retries must share the original retry budget: {requests:?}"
            );
            assert_eq!(attempts[1].0, if batch > 0 { 40 } else { 17 });
        }
    }
}
