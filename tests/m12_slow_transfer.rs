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
    let (events, arrivals) = mpsc::unbounded_channel();
    let requests = Arc::new(AtomicUsize::new(0));
    let calls = requests.clone();
    let first = Arc::new(AtomicUsize::new(0));
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
            let gated = start >= slow_start && start < slow_start+piece;
            let original = gated && first.fetch_add(1, Ordering::SeqCst) == 0;
            let (mut sender, body) = warp::hyper::Body::channel();
            let (release, mut gate) = oneshot::channel();
            if gated { events.send(Arrival { start,end,condition,release }).unwrap(); }
            let variant = if gated && !original { challenger_status } else { 206 };
            let status = if (294..=299).contains(&variant) { 206 } else { variant };
            tokio::spawn(async move {
                if status != 206 { return; }
                let mut offset = start;
                if original {
                    loop {
                        tokio::select! {
                            _ = &mut gate => break,
                            _ = tokio::time::sleep(Duration::from_millis(10)) => {
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
            if variant != 296 { response = response.header("content-length", if status == 206 {end-start} else {0}); }
            if variant == 295 { response = response.header("content-encoding","gzip"); }
            if let Some(etag) = etag { response = response.header("etag",if [294,298].contains(&variant) { "\"changed\"" } else {etag}); }
            response.body(body).unwrap()
        });
    let (addr, server) = warp::serve(route).bind_ephemeral(([127, 0, 0, 1], 0));
    Fixture {
        url: format!("http://{addr}/file"),
        arrivals,
        server: tokio::spawn(server),
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
        let mut server = fixture(Some("\"stable\""), TOTAL, 206);
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
            .slow_transfer_mode(mode);
        let handle = Downloader::builder().build().unwrap().download(config);
        let original = next(&mut server).await;
        // The original stays gated; a replacement must arrive before the
        // ordinary 5-second window plus 15-second sustained gate can elapse.
        let replacement = next(&mut server).await;
        assert_eq!((replacement.start, replacement.end), (TOTAL - PIECE, TOTAL));
        replacement.release.send(()).unwrap();
        finish(handle, &path, TOTAL).await;
        drop(original);
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
    for _ in 0..4 {
        replacements.push(next(&mut server).await);
    }
    let mut ranges: Vec<_> = replacements.iter().map(|r| (r.start, r.end)).collect();
    ranges.sort_unstable();
    assert_eq!(
        ranges,
        [(0, 1024), (1024, 2048), (2048, 3072), (3072, 4096)]
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
    for status in [503, 200, 299, 298, 297, 296, 295, 294, 412] {
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
        let mut server = fixture_size(Some("\"stable\""), total, 206, PIECE, 0);
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
