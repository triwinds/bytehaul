use bytehaul::{DownloadError, DownloadSpec, DownloadState, Downloader, SlowTransferMode};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use warp::Filter;

const PIECE: u64 = 4096;
const TOTAL: u64 = PIECE * 128;
fn bytes(start: u64, end: u64) -> Vec<u8> {
    (start..end).map(|n| (n % 251) as u8).collect()
}
struct Arrival {
    start: u64,
    end: u64,
    condition: Option<String>,
    release: oneshot::Sender<()>,
}
struct Fixture {
    url: String,
    arrivals: mpsc::UnboundedReceiver<Arrival>,
    server: tokio::task::JoinHandle<()>,
    peak_active: Arc<AtomicUsize>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}
fn fixture(etag: Option<&'static str>, total: u64, challenger_status: u16) -> Fixture {
    fixture_size(etag, total, challenger_status, PIECE, total - PIECE)
}
fn fixture_size(
    etag: Option<&'static str>,
    total: u64,
    challenger_status: u16,
    piece: u64,
    slow_start: u64,
) -> Fixture {
    fixture_behavior(etag, total, challenger_status, piece, slow_start, true)
}
fn fixture_behavior(
    etag: Option<&'static str>,
    total: u64,
    challenger_status: u16,
    piece: u64,
    slow_start: u64,
    drip: bool,
) -> Fixture {
    let (events, arrivals) = mpsc::unbounded_channel();
    let requests = Arc::new(AtomicUsize::new(0));
    let calls = requests.clone();
    let first = Arc::new(AtomicUsize::new(0));
    let active = Arc::new(AtomicUsize::new(0));
    let peak_active = Arc::new(AtomicUsize::new(0));
    let peak = peak_active.clone();
    let route = warp::header::optional::<String>("range")
        .and(warp::header::optional::<String>("if-match"))
        .map(move |range: Option<String>, condition: Option<String>| {
            calls.fetch_add(1, Ordering::SeqCst);
            let range = range.unwrap();
            let (start,end) = range.strip_prefix("bytes=").unwrap().split_once('-').unwrap();
            let start = start.parse::<u64>().unwrap();
            let end = end.parse::<u64>().unwrap().min(total-1)+1;
            // Reclaimed/subdivided ranges are gated; the request barrier
            // proves actual takeover independently of elapsed download time.
            let gated = start < slow_start+piece && end > slow_start;
            let original = gated && first.fetch_add(1, Ordering::SeqCst) == 0;
            let (mut sender, body) = warp::hyper::Body::channel();
            let (release, mut gate) = oneshot::channel();
            if gated { events.send(Arrival { start,end,condition,release }).unwrap(); }
            let variant = if gated && !original { challenger_status } else { 206 };
            let status = if (293..=299).contains(&variant) { 206 } else { variant };
            let active = active.clone();
            let count = active.fetch_add(1, Ordering::SeqCst) + 1;
            peak.fetch_max(count, Ordering::SeqCst);
            tokio::spawn(async move {
                struct Active(Arc<AtomicUsize>);
                impl Drop for Active { fn drop(&mut self) { self.0.fetch_sub(1, Ordering::SeqCst); } }
                let _active = Active(active);
                if status != 206 { return; }
                let mut offset = start;
                if original {
                    if offset < slow_start {
                        if sender.send_data(bytes(offset,slow_start).into()).await.is_err() { return; }
                        offset = slow_start;
                    }
                    loop {
                        tokio::select! {
                            _ = &mut gate => break,
                            _ = tokio::time::sleep(Duration::from_millis(10)) => {
                                if !drip { continue; }
                                if offset >= end { return; }
                                if sender.send_data(bytes(offset,offset+1).into()).await.is_err() { return; }
                                offset += 1;
                            }
                        }
                    }
                } else if gated { let _ = gate.await; }
                let body_end = match variant { 297 => end-1, 296 => end+1, _ => end };
                let _ = sender.send_data(bytes(offset,body_end).into()).await;
            });
            let mut response = warp::http::Response::builder().status(status)
                .header("content-range", format!("bytes {}-{}/{}", if variant == 299 {start+1} else {start},end-1,if variant==294 {total+1} else {total}))
                .header("accept-ranges", "bytes");
            if ![293, 296].contains(&variant) { response = response.header("content-length", if status == 206 {end-start} else {0}); }
            if variant == 295 { response = response.header("content-encoding","gzip"); }
            if let Some(etag) = etag { response = response.header("etag",if [294,298].contains(&variant) { "\"changed\"" } else {etag}); }
            response.body(body).unwrap()
        });
    let (addr, server) = warp::serve(route).bind_ephemeral(([127, 0, 0, 1], 0));
    Fixture {
        url: format!("http://{addr}/file"),
        arrivals,
        server: tokio::spawn(server),
        peak_active,
    }
}
fn spec(url: &str, path: &std::path::Path, mode: SlowTransferMode) -> DownloadSpec {
    DownloadSpec::new(url)
        .output_path(path)
        .resume(false)
        .piece_size(PIECE)
        .min_segment_size(PIECE)
        .min_split_size(1)
        .max_connections(4)
        .max_retries(0)
        .read_timeout(Duration::from_secs(5))
        .slow_transfer_mode(mode)
        .low_speed_limit(1024 * 1024)
        .slow_start_grace(Duration::from_millis(20))
        .slow_sample_window(Duration::from_millis(30))
        .low_speed_duration(Duration::from_millis(60))
}
async fn next(fixture: &mut Fixture) -> Arrival {
    tokio::time::timeout(Duration::from_secs(10), fixture.arrivals.recv())
        .await
        .expect("expected takeover request")
        .unwrap()
}
async fn finish(handle: bytehaul::DownloadHandle, path: &std::path::Path, total: u64) {
    let progress = handle.subscribe_progress();
    tokio::time::timeout(Duration::from_secs(10), handle.wait())
        .await
        .expect("download must terminate")
        .unwrap();
    assert_eq!(std::fs::read(path).unwrap(), bytes(0, total));
    assert_eq!(progress.borrow().state, DownloadState::Completed);
    assert_eq!(progress.borrow().downloaded, total);
    assert_eq!(
        std::fs::read_dir(path.parent().unwrap()).unwrap().count(),
        1,
        "no challenger temporary files remain"
    );
}
#[tokio::test]
async fn default_policy_recovers_tail_in_both_modes() {
    for mode in [
        SlowTransferMode::Adaptive,
        SlowTransferMode::AdaptiveWithHedging,
    ] {
        for batch in [0, PIECE * 4] {
            // The scheduler leaves three independent pieces for other workers.
            // Slow the last constituent of the preceding batch, after its healthy
            // prefix, while those independent final pieces complete normally.
            let slow_start = TOTAL - if batch == 0 { PIECE } else { PIECE * 4 };
            let mut server = fixture_size(Some("\"stable\""), TOTAL, 206, PIECE, slow_start);
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("out");
            // Leave all detection durations and the absolute floor at their defaults.
            let config = DownloadSpec::new(&server.url)
                .output_path(&path)
                .resume(false)
                .piece_size(PIECE)
                .min_segment_size(PIECE)
                .min_split_size(1)
                .max_connections(4)
                .max_retries(0)
                .read_timeout(Duration::from_secs(15))
                .slow_transfer_mode(mode)
                .request_batch_size(batch);
            let handle = Downloader::builder().build().unwrap().download(config);
            let original = next(&mut server).await;
            if batch > 0 {
                assert!(original.start < slow_start,
                "fixture must slow a retained body after an earlier constituent: {} >= {slow_start}", original.start);
            }
            assert_eq!(original.end, slow_start + PIECE);
            // The original stays gated; a replacement must arrive before the
            // ordinary 5-second window plus 15-second sustained gate can elapse.
            let replacement = next(&mut server).await;
            assert_eq!(replacement.end, slow_start + PIECE);
            if mode == SlowTransferMode::Adaptive {
                assert!(
                    replacement.start > slow_start && replacement.start < slow_start + PIECE,
                    "recovery must preserve the confirmed slow prefix"
                );
            } else {
                assert_eq!(replacement.start, slow_start);
            }
            assert_eq!(replacement.condition.as_deref(), Some("\"stable\""));
            replacement.release.send(()).unwrap();
            finish(handle, &path, TOTAL).await;
            drop(original);
        }
    }
}
#[tokio::test]
async fn drip_recovery_reclaims_and_splits_for_idle_workers() {
    let mut server = fixture_size(Some("\"stable\""), TOTAL, 206, PIECE, 0);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("out");
    let handle = Downloader::builder()
        .build()
        .unwrap()
        .download(spec(&server.url, &path, SlowTransferMode::Adaptive).min_segment_size(1024));
    let original = next(&mut server).await;
    assert_eq!((original.start, original.end), (0, PIECE));
    let mut replacements = Vec::new();
    for _ in 0..3 {
        replacements.push(next(&mut server).await);
    }
    let mut ranges: Vec<_> = replacements.iter().map(|r| (r.start, r.end)).collect();
    ranges.sort_unstable();
    let retained = ranges[0].0;
    assert!(retained > 0 && retained < 1024);
    assert_eq!(
        ranges,
        [
            (retained, retained + 1024),
            (retained + 1024, retained + 2048),
            (retained + 2048, 4096)
        ]
    );
    for replacement in replacements {
        replacement.release.send(()).unwrap();
    }
    finish(handle, &path, TOTAL).await;
    drop(original);
}
#[tokio::test]
async fn challenger_wins_without_duplicate_progress_with_tiny_budget() {
    for memory in [1, 100, 4097] {
        let total = 128 * 128;
        let mut server = fixture_size(Some("\"stable\""), total, 206, 128, total - 128);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out");
        let handle = Downloader::builder().build().unwrap().download(
            spec(&server.url, &path, SlowTransferMode::AdaptiveWithHedging)
                .piece_size(128)
                .min_segment_size(128)
                .memory_budget(memory)
                .channel_buffer(1),
        );
        let original = next(&mut server).await;
        let challenger = next(&mut server).await;
        assert_eq!(
            (challenger.start, challenger.end),
            (original.start, original.end)
        );
        assert_eq!(challenger.condition.as_deref(), Some("\"stable\""));
        challenger.release.send(()).unwrap();
        finish(handle, &path, total).await;
        drop(original);
    }
}
#[tokio::test]
async fn primary_wins_and_cancels_staged_challenger() {
    let mut server = fixture(Some("\"stable\""), TOTAL, 206);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("out");
    let handle = Downloader::builder().build().unwrap().download(spec(
        &server.url,
        &path,
        SlowTransferMode::AdaptiveWithHedging,
    ));
    let original = next(&mut server).await;
    let challenger = next(&mut server).await;
    original.release.send(()).unwrap();
    finish(handle, &path, TOTAL).await;
    drop(challenger);
}
#[tokio::test]
async fn failed_challenger_keeps_primary_alive_but_changed_object_fails() {
    for status in [503, 200, 299, 298, 297, 296, 295, 294, 293, 412] {
        let mut server = fixture(Some("\"stable\""), TOTAL, status);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out");
        let handle = Downloader::builder().build().unwrap().download(spec(
            &server.url,
            &path,
            SlowTransferMode::AdaptiveWithHedging,
        ));
        let original = next(&mut server).await;
        let challenger = next(&mut server).await;
        if ![412, 298, 294].contains(&status) {
            let _ = challenger.release.send(());
            // Let the malformed optional response be consumed while the
            // authoritative original remains deliberately withheld.
            tokio::time::sleep(Duration::from_millis(100)).await;
            if handle.progress().state == DownloadState::Failed {
                panic!(
                    "candidate {status} killed primary: {:?}",
                    handle.wait().await
                );
            }
            assert_eq!(handle.progress().state, DownloadState::Downloading);
            original.release.send(()).unwrap();
            finish(handle, &path, TOTAL).await;
        } else {
            let error = tokio::time::timeout(Duration::from_secs(5), handle.wait())
                .await
                .unwrap()
                .unwrap_err();
            assert!(matches!(
                (status, error),
                (412, DownloadError::HttpStatus { status: 412, .. })
                    | (298 | 294, DownloadError::ResumeMismatch(_))
            ));
        }
    }
}
#[tokio::test]
async fn disabled_small_file_and_rate_limit_do_not_replace_primary() {
    for case in ["disabled", "small", "limited"] {
        let total = if case == "small" { PIECE * 2 } else { TOTAL };
        // Without a usable validator the small-file cancellation fallback must
        // still reserve replay bytes and remain denied by the original budget.
        let etag = if case == "small" {
            "W/\"stable\""
        } else {
            "\"stable\""
        };
        let mut server = fixture_size(Some(etag), total, 206, PIECE, 0);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out");
        let mode = if case == "disabled" {
            SlowTransferMode::Disabled
        } else {
            SlowTransferMode::AdaptiveWithHedging
        };
        let mut config = spec(&server.url, &path, mode);
        if case == "limited" {
            config = config.max_download_speed(100);
        }
        let handle = Downloader::builder().build().unwrap().download(config);
        let original = next(&mut server).await;
        // Several complete policy intervals with no recovery; this is a
        // suppression check, not a speed comparison between machines.
        assert!(
            tokio::time::timeout(Duration::from_millis(350), server.arrivals.recv())
                .await
                .is_err(),
            "unexpected replacement in {case}"
        );
        if case == "limited" {
            handle.cancel();
            assert!(matches!(handle.wait().await, Err(DownloadError::Cancelled)));
        } else {
            original.release.send(()).unwrap();
            finish(handle, &path, total).await;
        }
    }
}
#[tokio::test]
async fn pause_and_cancel_during_hedge_clean_temporary_files() {
    for pause in [false, true] {
        let mut server = fixture(Some("\"stable\""), TOTAL, 206);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out");
        let handle = Downloader::builder().build().unwrap().download(spec(
            &server.url,
            &path,
            SlowTransferMode::AdaptiveWithHedging,
        ));
        let original = next(&mut server).await;
        let challenger = next(&mut server).await;
        if pause {
            handle.pause();
        } else {
            handle.cancel();
        }
        let error = tokio::time::timeout(Duration::from_secs(5), handle.wait())
            .await
            .unwrap()
            .unwrap_err();
        assert!(matches!(
            (pause, error),
            (true, DownloadError::Paused) | (false, DownloadError::Cancelled)
        ));
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
        drop((original, challenger));
    }
}
#[test]
fn configuration_defaults_and_validation_are_public() {
    let config = DownloadSpec::new("http://example.invalid");
    assert_eq!(config.get_slow_transfer_mode(), SlowTransferMode::Adaptive);
    assert_eq!(config.get_low_speed_limit(), None);
    assert_eq!(config.get_low_speed_duration(), Duration::from_secs(15));
    assert_eq!(config.get_slow_start_grace(), Duration::from_secs(5));
    assert_eq!(config.get_slow_sample_window(), Duration::from_secs(5));
    for invalid in [
        config.clone().low_speed_limit(0),
        config.clone().low_speed_duration(Duration::ZERO),
        config.clone().slow_sample_window(Duration::ZERO),
        config.clone().slow_start_grace(Duration::from_secs(86401)),
    ] {
        assert!(matches!(
            invalid.validate(),
            Err(DownloadError::InvalidConfig(_))
        ));
    }
    assert!(config
        .slow_sample_window(Duration::from_nanos(1))
        .validate()
        .is_ok());
}

#[tokio::test]
async fn small_budget_recovers_first_middle_and_last_piece_of_early_batch() {
    const LARGE_PIECE: u64 = 1024 * 1024;
    const LARGE_TOTAL: u64 = 64_469_455;
    for (mode, constituent) in [
        SlowTransferMode::Adaptive,
        SlowTransferMode::AdaptiveWithHedging,
    ]
    .into_iter()
    .flat_map(|mode| [0, 1, 3].map(|constituent| (mode, constituent)))
    {
        // The exact probe consumes piece 0; the first grouped request owns
        // pieces 1..5. Hold one constituent until safe takeover is observed.
        let slow_start = (1 + constituent) * LARGE_PIECE;
        let mut server = fixture_size(
            Some("\"stable\""),
            LARGE_TOTAL,
            206,
            LARGE_PIECE,
            slow_start,
        );
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out");
        let config = DownloadSpec::new(&server.url)
            .output_path(&path)
            .resume(false)
            .piece_size(LARGE_PIECE)
            .min_segment_size(LARGE_PIECE)
            .min_split_size(1)
            .max_connections(4)
            .max_retries(0)
            .slow_transfer_mode(mode)
            .read_timeout(Duration::from_secs(20))
            .request_batch_size(LARGE_PIECE * 4);
        let handle = Downloader::builder().build().unwrap().download(config);
        let original = next(&mut server).await;
        assert_eq!(
            (original.start, original.end),
            (LARGE_PIECE, LARGE_PIECE * 5)
        );
        let replacement = next(&mut server).await;
        // Even the final constituent in hedge mode must use a suffix Range:
        // a parallel full-piece challenger cannot fit this file's 1% budget.
        assert!(replacement.start > slow_start && replacement.start < slow_start + LARGE_PIECE);
        assert_eq!(replacement.end, slow_start + LARGE_PIECE);
        assert_eq!(replacement.condition.as_deref(), Some("\"stable\""));
        replacement.release.send(()).unwrap();
        // Never release the original: exact completion proves cancellation,
        // prefix retention and release of unstarted batch leases together.
        finish(handle, &path, LARGE_TOTAL).await;
        assert!(server.peak_active.load(Ordering::SeqCst) <= 4);
        drop(original);
    }
}

#[tokio::test]
async fn prefix_budget_requires_strong_identity_and_handles_zero_data_restart() {
    for (etag, drip, status) in [
        (Some("\"stable\""), false, 206),
        (Some("\"stable\""), true, 298),
        (Some("W/\"weak\""), true, 206),
        (None, true, 206),
    ] {
        let total = PIECE * 2;
        let mut server = fixture_behavior(etag, total, status, PIECE, 0, drip);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out");
        let handle = Downloader::builder()
            .build()
            .unwrap()
            .download(spec(&server.url, &path, SlowTransferMode::Adaptive).request_batch_size(0));
        let original = next(&mut server).await;
        if etag == Some("\"stable\"") {
            let replacement = next(&mut server).await;
            assert_eq!(replacement.condition.as_deref(), etag);
            if !drip {
                assert_eq!(replacement.start, 0);
            } else {
                assert!(replacement.start > 0);
            }
            let _ = replacement.release.send(());
            if status == 298 {
                assert!(matches!(
                    handle.wait().await,
                    Err(DownloadError::ResumeMismatch(_))
                ));
            } else {
                finish(handle, &path, total).await;
            }
        } else {
            assert!(
                tokio::time::timeout(Duration::from_millis(350), server.arrivals.recv())
                    .await
                    .is_err()
            );
            original.release.send(()).unwrap();
            finish(handle, &path, total).await;
            continue;
        }
        drop(original);
    }
}

#[tokio::test]
async fn released_batch_leases_share_retry_budget_and_retry_after() {
    enum Reply {
        Normal,
        Slow,
        RetryAfter,
    }
    let (requests, mut arrivals) = mpsc::unbounded_channel::<(u64, u64, oneshot::Sender<Reply>)>();
    let route = warp::header::<String>("range").and_then(move |range: String| {
        let requests = requests.clone();
        async move {
            let (start, end) = range
                .strip_prefix("bytes=")
                .unwrap()
                .split_once('-')
                .unwrap();
            let start = start.parse::<u64>().unwrap();
            let end = end.parse::<u64>().unwrap() + 1;
            let (reply, receive) = oneshot::channel();
            let _ = requests.send((start, end, reply));
            let response = match receive.await.unwrap_or(Reply::RetryAfter) {
                Reply::RetryAfter => warp::http::Response::builder()
                    .status(503)
                    .header("retry-after", "1")
                    .body(warp::hyper::Body::empty())
                    .unwrap(),
                reply => {
                    let (mut sender, body) = warp::hyper::Body::channel();
                    tokio::spawn(async move {
                        if matches!(reply, Reply::Slow) {
                            for offset in start..end {
                                tokio::time::sleep(Duration::from_millis(10)).await;
                                if sender
                                    .send_data(bytes(offset, offset + 1).into())
                                    .await
                                    .is_err()
                                {
                                    return;
                                }
                            }
                        } else {
                            let _ = sender.send_data(bytes(start, end).into()).await;
                        }
                    });
                    warp::http::Response::builder()
                        .status(206)
                        .header("content-length", end - start)
                        .header(
                            "content-range",
                            format!("bytes {start}-{}/{TOTAL}", end - 1),
                        )
                        .header("accept-ranges", "bytes")
                        .header("etag", "\"stable\"")
                        .body(body)
                        .unwrap()
                }
            };
            Ok::<_, std::convert::Infallible>(response)
        }
    });
    let (addr, server) = warp::serve(route).bind_ephemeral(([127, 0, 0, 1], 0));
    let server = tokio::spawn(server);
    let dir = tempfile::tempdir().unwrap();
    let config = DownloadSpec::new(format!("http://{addr}/file"))
        .output_path(dir.path().join("out"))
        .resume(false)
        .piece_size(PIECE)
        .min_segment_size(PIECE)
        .min_split_size(1)
        .max_connections(4)
        .max_retries(1)
        .read_timeout(Duration::from_secs(15))
        .request_batch_size(PIECE * 4);
    let handle = Downloader::builder().build().unwrap().download(config);
    let completion = handle.wait();
    tokio::pin!(completion);
    let mut original_seen = false;
    let mut piece_two_calls = 0;
    let mut piece_three_calls = 0;
    let mut first_failure = None;
    let mut held_piece_three: Option<oneshot::Sender<Reply>> = None;
    let mut first_retry_seen = false;
    let result = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            tokio::select! {
                result = &mut completion => break result,
                request = arrivals.recv() => {
                    let (start, end, reply) = request.expect("server stays alive");
                    if start == PIECE && end == PIECE * 5 {
                        assert!(!original_seen);
                        original_seen = true;
                        reply.send(Reply::Slow).ok();
                    } else if start == PIECE * 2 && end == PIECE * 3 {
                        assert!(original_seen, "standalone queued work follows midbatch cancellation");
                        piece_two_calls += 1;
                        if piece_two_calls == 1 {
                            first_failure = Some(std::time::Instant::now());
                            reply.send(Reply::RetryAfter).ok();
                        } else {
                            assert_eq!(piece_two_calls, 2);
                            assert!(first_failure.unwrap().elapsed() >= Duration::from_secs(1), "Retry-After cannot be reset by released leases");
                            first_retry_seen = true;
                            reply.send(Reply::Normal).ok();
                            if let Some(held) = held_piece_three.take() {
                                let _: Result<(), Reply> = held.send(Reply::RetryAfter);
                            }
                        }
                    } else if start == PIECE * 3 && end == PIECE * 4 {
                        piece_three_calls += 1;
                        if first_retry_seen { reply.send(Reply::RetryAfter).ok(); }
                        else { held_piece_three = Some(reply); }
                    } else { reply.send(Reply::Normal).ok(); }
                }
            }
        }
    }).await.expect("shared lineage must terminate on the second ordinary failure");
    assert!(matches!(
        result,
        Err(DownloadError::HttpStatus { status: 503, .. })
    ));
    assert_eq!(
        piece_two_calls, 2,
        "the first released piece gets the single retry"
    );
    assert_eq!(
        piece_three_calls, 1,
        "a different released piece must not receive a fresh retry budget"
    );
    server.abort();
}
