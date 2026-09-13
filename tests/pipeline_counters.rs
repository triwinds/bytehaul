//! Verifies the opt-in pipeline counters against real downloads (P1).
//!
//! The counters and the live driver-thread gauge are process-wide, so one test
//! owns this whole test binary: a second test running in parallel could add to
//! the accumulators while this one compares values taken before and after a
//! download.
//!
//! What this proves, in order:
//!
//! 1. with collection disabled a real download records nothing at all;
//! 2. with collection enabled the same download reports the writer, cache,
//!    output-file, fsync, verification and transport work the default path
//!    really did — including the request→head and body-read waits the harness
//!    reports separately from the progress-channel timeline;
//! 3. preallocation and concurrency queueing are recorded as their own phases;
//! 4. releasing every client returns the driver-thread count to its baseline,
//!    which is the residual-resource measurement P2 builds on.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytehaul::bench::{
    bench_counters_enabled, bench_counters_reset, bench_counters_set_enabled,
    bench_counters_snapshot, bench_driver_stats, bench_driver_threads, BenchCounters,
};
use bytehaul::{Checksum, DownloadSpec, Downloader, FileAllocation};
use tokio::sync::Semaphore;
use warp::Filter;

const PIECE_SIZE: usize = 1024 * 1024;
const PIECES: usize = 4;
const SMALL_BYTES: usize = PIECES * PIECE_SIZE;
/// The session only splits a download between connections above
/// `min_split_size` (10 MiB by default), and only the split path writes through
/// the write-back cache. A measured download that must show cache traffic has
/// to be larger than that default, or it is a single-connection download.
const LARGE_BYTES: u64 = 12 * 1024 * 1024;

/// The fixture body is not a repetition of one byte, so a mis-placed range
/// would produce a different file and the checksum would fail.
fn body(bytes: usize) -> Vec<u8> {
    (0..bytes as u32).map(|index| (index % 251) as u8).collect()
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Range-capable local fixture that counts the requests it answers.
struct Fixture {
    url: String,
    requests: Arc<AtomicUsize>,
    ranges: Arc<Mutex<Vec<String>>>,
    gate: Option<Arc<Semaphore>>,
    server: tokio::task::JoinHandle<()>,
}

impl Fixture {
    fn spawn(path: &'static str, bytes: usize, gated: bool) -> Self {
        let data = Arc::new(body(bytes));
        let requests = Arc::new(AtomicUsize::new(0));
        let ranges = Arc::new(Mutex::new(Vec::new()));
        let gate = gated.then(|| Arc::new(Semaphore::new(0)));

        let route = warp::path(path)
            .and(warp::header::optional::<String>("range"))
            .and_then({
                let data = data.clone();
                let requests = requests.clone();
                let ranges = ranges.clone();
                let gate = gate.clone();
                move |range_header: Option<String>| {
                    let data = data.clone();
                    let requests = requests.clone();
                    let ranges = ranges.clone();
                    let gate = gate.clone();
                    async move {
                        requests.fetch_add(1, Ordering::SeqCst);
                        ranges
                            .lock()
                            .unwrap()
                            .push(range_header.clone().unwrap_or_else(|| "full".to_string()));
                        if let Some(gate) = gate {
                            // Held until `release`: the caller can observe a
                            // download that is running but not progressing.
                            let _held = gate.acquire_owned().await.expect("gate stays open");
                        }

                        let total = data.len();
                        let (start, end) = match range_header.as_deref() {
                            Some(header) => {
                                let range = header.trim_start_matches("bytes=").to_string();
                                let (start, end) = range.split_once('-').unwrap_or((&range, ""));
                                let start: usize = start.parse().unwrap_or(0);
                                let end = if end.is_empty() {
                                    total - 1
                                } else {
                                    end.parse::<usize>().unwrap_or(total - 1).min(total - 1)
                                };
                                (start, end)
                            }
                            None => (0, total - 1),
                        };
                        let slice = data[start..=end].to_vec();

                        Ok::<_, std::convert::Infallible>(
                            warp::http::Response::builder()
                                .status(if range_header.is_some() { 206 } else { 200 })
                                .header("content-length", slice.len().to_string())
                                .header("content-range", format!("bytes {start}-{end}/{total}"))
                                .header("accept-ranges", "bytes")
                                .header("etag", "\"pipeline-counters\"")
                                .header("last-modified", "Sat, 01 Jan 2026 00:00:00 GMT")
                                .body(slice)
                                .unwrap(),
                        )
                    }
                }
            });

        let (addr, server) = warp::serve(route).bind_ephemeral(([127, 0, 0, 1], 0));
        Self {
            url: format!("http://{addr}/{path}"),
            requests,
            ranges,
            gate,
            server: tokio::spawn(server),
        }
    }

    fn request_count(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
    }

    fn observed_ranges(&self) -> Vec<String> {
        self.ranges.lock().unwrap().clone()
    }

    /// Lets every held response continue.
    fn release(&self) {
        if let Some(gate) = &self.gate {
            gate.add_permits(1024);
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}

async fn download_into(
    downloader: &Downloader,
    fixture: &Fixture,
    dir: &tempfile::TempDir,
    name: &str,
    configure: impl FnOnce(DownloadSpec) -> DownloadSpec,
) -> Result<(), bytehaul::DownloadError> {
    let spec = configure(DownloadSpec::new(fixture.url.clone()))
        .output_path(dir.path().join(name))
        .file_allocation(FileAllocation::None);
    downloader.download(spec).wait().await
}

/// Waits until `predicate` holds, then returns whether it did.
async fn wait_until(mut predicate: impl FnMut() -> bool, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if predicate() {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn assert_unchanged(mut before: BenchCounters, mut after: BenchCounters) {
    // The driver-thread field is a live gauge, not an accumulator: the
    // download under test legitimately starts one driver thread while
    // collection is disabled.
    before.driver_threads = 0;
    after.driver_threads = 0;
    assert_eq!(
        after, before,
        "a disabled collection must not record anything"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_pipeline_counters_observe_the_default_download_path() {
    let baseline_threads = bench_driver_threads();
    assert!(
        !bench_counters_enabled(),
        "collection must be off unless the harness asks for it"
    );
    // Small but above the piece size: the session's range probe runs, while the
    // body stays below `min_split_size`, so this is the single-connection path.
    let small = Fixture::spawn("small", SMALL_BYTES, false);
    let large = Fixture::spawn("large", LARGE_BYTES as usize, false);
    let dir = tempfile::tempdir().unwrap();
    let small_digest = sha256_hex(&body(SMALL_BYTES));
    let large_digest = sha256_hex(&body(LARGE_BYTES as usize));

    // ── 1. Disabled: the same download the enabled phases measure records
    //    nothing, which is what keeps the counters free in production.
    {
        let before = bench_counters_snapshot();
        let downloader = Downloader::builder().build().unwrap();
        download_into(&downloader, &small, &dir, "disabled.bin", |spec| {
            spec.max_connections(4)
                .checksum(Checksum::Sha256(small_digest.clone()))
        })
        .await
        .expect("the disabled download must succeed");
        let after = bench_counters_snapshot();
        assert_unchanged(before, after);
        assert!(
            small.request_count() >= 2,
            "a range probe is issued and the body is refetched below min_split_size"
        );
    }

    // ── 2. Enabled: the default path reports its writer, cache and
    //    verification work, and the numbers match the bytes that moved.
    bench_counters_reset();
    bench_counters_set_enabled(true);
    let downloader = Downloader::builder().build().unwrap();
    download_into(&downloader, &large, &dir, "enabled.bin", |spec| {
        spec.max_connections(4)
            .checksum(Checksum::Sha256(large_digest.clone()))
    })
    .await
    .expect("the measured download must succeed");

    let counters = bench_counters_snapshot();
    assert!(
        counters.writer_blocks >= 1,
        "the writer must have written at least one block: {counters:?}"
    );
    assert!(
        counters.writer_bytes >= LARGE_BYTES,
        "every body byte is written to disk: {counters:?}"
    );
    assert!(
        counters.cache_copied_bytes >= LARGE_BYTES,
        "the split path copies every body byte through the cache: {counters:?}"
    );
    assert!(
        counters.cache_evicted_bytes >= LARGE_BYTES,
        "cached bytes are released again: {counters:?}"
    );
    assert!(counters.preallocs >= 1, "{counters:?}");
    assert!(counters.fsync_calls >= 1, "{counters:?}");
    assert!(counters.checksums == 1, "{counters:?}");
    assert!(counters.checksum_micros > 0, "{counters:?}");
    assert!(
        counters.response_heads >= 2,
        "the probe and every range request receive a response head: {counters:?}"
    );
    assert!(
        counters.response_head_micros > 0,
        "a local origin still takes measurable time to answer: {counters:?}"
    );
    assert!(
        counters.body_reads >= 1,
        "the body is read through the shared transport seam: {counters:?}"
    );
    assert!(
        counters.body_read_micros > 0,
        "waiting for {LARGE_BYTES} bytes of body takes measurable time: {counters:?}"
    );

    let stats = bench_driver_stats(&downloader).expect("the client has a driver");
    assert!(
        stats.submitted >= 2,
        "a split download submits a probe and its range requests: {stats:?}"
    );
    assert!(stats.connections >= 1, "{stats:?}");
    assert!(stats.completed >= 1, "{stats:?}");
    assert_eq!(stats.active, 0, "the download has finished: {stats:?}");
    assert!(
        stats.pools >= 1,
        "a used driver keeps the connection pool of its origin: {stats:?}"
    );
    assert!(stats.loops > 0, "{stats:?}");
    assert!(
        large.request_count() >= 2,
        "the split path issues more than one request: {}",
        large.request_count()
    );

    // ── 3. Preallocation is recorded as a phase of its own.
    let before = bench_counters_snapshot();
    let spec = DownloadSpec::new(large.url.clone())
        .output_path(dir.path().join("prealloc.bin"))
        .file_allocation(FileAllocation::Prealloc)
        .max_connections(4);
    downloader
        .download(spec)
        .wait()
        .await
        .expect("the preallocating download must succeed");
    let after = bench_counters_snapshot();
    assert_eq!(
        after.preallocs,
        before.preallocs + 1,
        "one download creates one output file: {after:?}"
    );
    assert!(
        after.prealloc_micros > before.prealloc_micros,
        "preallocating a {LARGE_BYTES}-byte file takes measurable time: {after:?}"
    );

    // ── 4. Queueing is recorded as its own phase.
    let gated = Fixture::spawn("gated", SMALL_BYTES, true);
    let queued_dir = tempfile::tempdir().unwrap();
    bench_counters_reset();
    let limited = Downloader::builder()
        .max_concurrent_downloads(1)
        .build()
        .unwrap();
    let first = limited.download(
        DownloadSpec::new(gated.url.clone())
            .output_path(queued_dir.path().join("first.bin"))
            .max_connections(1),
    );
    // The first request is answered but its body is held, so the first
    // download keeps the only permit while the second one queues.
    assert!(
        wait_until(|| gated.request_count() >= 1, Duration::from_secs(10)).await,
        "the first download must reach the fixture"
    );
    let second = limited.download(
        DownloadSpec::new(gated.url.clone())
            .output_path(queued_dir.path().join("second.bin"))
            .max_connections(1),
    );
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(
        bench_counters_snapshot().queue_waits,
        1,
        "only the admitted download is counted so far; the queued one is not"
    );
    gated.release();
    first.wait().await.expect("the first download succeeds");
    second.wait().await.expect("the queued download succeeds");

    let queued = bench_counters_snapshot();
    assert_eq!(
        queued.queue_waits, 2,
        "both downloads passed the concurrency gate: {queued:?}"
    );
    assert!(
        queued.queue_micros >= 50_000,
        "the second download waited for the first one's permit: {queued:?}"
    );
    assert_eq!(
        gated.observed_ranges().len(),
        gated.request_count(),
        "every answered request is also recorded"
    );
    drop(limited);

    // ── 5. Releasing every client leaves no driver thread behind.
    drop(downloader);
    assert!(
        wait_until(
            || bench_driver_threads() <= baseline_threads,
            Duration::from_secs(10)
        )
        .await,
        "dropping every downloader must stop its driver thread"
    );

    // Leave the process-wide state as this test found it.
    bench_counters_reset();
    bench_counters_set_enabled(false);
    assert!(!bench_counters_enabled());
}
