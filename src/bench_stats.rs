//! Opt-in counters for the local pipeline harness (`benches/pipeline_bench.rs`).
//!
//! The measurements this crate needs (write batches, seek+write pairs, body
//! bytes copied through the write-back cache, queueing wait, preallocation,
//! final sync and verification time) are not observable from outside the
//! process, and the point of the harness is to measure the *default* download
//! path rather than a benchmark-only code path.
//!
//! Every counter is therefore recorded in production code, guarded by one
//! relaxed load of [`ENABLED`]. Collection is off unless the harness turns it
//! on, so a normal download pays a single predictable branch per site and
//! nothing else; no behaviour depends on any value here. The one site that
//! cannot ask the clock first — the request→head duration, which the caller
//! already measured for its own diagnostics — checks [`ENABLED`] itself.
//!
//! Recorded states are the only numbers the harness trusts:
//!
//! * `writer_blocks` / `writer_bytes` — what the writer actually handed to the
//!   file, one block per seek+write pair.
//! * `cache_copied_bytes` / `cache_evicted_bytes` — body bytes copied into the
//!   write-back cache and bytes leaving it again.
//! * `fsync_calls` / `fsync_micros` — flush+sync pairs performed by the writer.
//! * `queue_waits` / `queue_micros` — time between task start and the
//!   concurrency permit (the only "queueing" a download can experience).
//! * `preallocs` / `prealloc_micros` — output-file creation and preallocation.
//! * `response_heads` / `response_head_micros` — HTTP requests that received a
//!   response head, and the request→head time each one took. Redirects, probes
//!   and retried attempts each count, so the sum exceeds wall time when a
//!   download made several requests.
//! * `body_reads` / `body_read_micros` — body reads that waited for bytes, and
//!   how long they waited. This is the streaming wait of every attempt, without
//!   the time the caller spends writing what it received.
//! * `checksums` / `checksum_micros` — post-download verification.
//! * `driver_threads` — live libcurl driver threads, incremented by the thread
//!   itself so a leaked driver is visible as a count that never returns to its
//!   baseline.

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::time::{Duration, Instant};

static ENABLED: AtomicBool = AtomicBool::new(false);

static WRITER_BLOCKS: AtomicU64 = AtomicU64::new(0);
static WRITER_BYTES: AtomicU64 = AtomicU64::new(0);
static CACHE_COPIED_BYTES: AtomicU64 = AtomicU64::new(0);
static CACHE_EVICTED_BYTES: AtomicU64 = AtomicU64::new(0);
static FSYNC_CALLS: AtomicU64 = AtomicU64::new(0);
static FSYNC_MICROS: AtomicU64 = AtomicU64::new(0);
static QUEUE_WAITS: AtomicU64 = AtomicU64::new(0);
static QUEUE_MICROS: AtomicU64 = AtomicU64::new(0);
static PREALLOCS: AtomicU64 = AtomicU64::new(0);
static PREALLOC_MICROS: AtomicU64 = AtomicU64::new(0);
static CHECKSUMS: AtomicU64 = AtomicU64::new(0);
static CHECKSUM_MICROS: AtomicU64 = AtomicU64::new(0);
static RESPONSE_HEADS: AtomicU64 = AtomicU64::new(0);
static RESPONSE_HEAD_MICROS: AtomicU64 = AtomicU64::new(0);
static BODY_READS: AtomicU64 = AtomicU64::new(0);
static BODY_READ_MICROS: AtomicU64 = AtomicU64::new(0);
static DRIVER_THREADS: AtomicI64 = AtomicI64::new(0);

/// One consistent-enough read of every counter.
///
/// The values are independent atomics, so a snapshot taken while a download is
/// running may mix two moments. The harness only takes snapshots before and
/// after a download has stopped.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Snapshot {
    pub writer_blocks: u64,
    pub writer_bytes: u64,
    pub cache_copied_bytes: u64,
    pub cache_evicted_bytes: u64,
    pub fsync_calls: u64,
    pub fsync_micros: u64,
    pub queue_waits: u64,
    pub queue_micros: u64,
    pub preallocs: u64,
    pub prealloc_micros: u64,
    pub checksums: u64,
    pub checksum_micros: u64,
    pub response_heads: u64,
    pub response_head_micros: u64,
    pub body_reads: u64,
    pub body_read_micros: u64,
    pub driver_threads: i64,
}

/// Whether collection is on. Checked once per recording site.
#[inline]
pub(crate) fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

pub(crate) fn set_enabled(enabled: bool) {
    ENABLED.store(enabled, Ordering::SeqCst);
}

pub(crate) fn reset() {
    for counter in [
        &WRITER_BLOCKS,
        &WRITER_BYTES,
        &CACHE_COPIED_BYTES,
        &CACHE_EVICTED_BYTES,
        &FSYNC_CALLS,
        &FSYNC_MICROS,
        &QUEUE_WAITS,
        &QUEUE_MICROS,
        &PREALLOCS,
        &PREALLOC_MICROS,
        &CHECKSUMS,
        &CHECKSUM_MICROS,
        &RESPONSE_HEADS,
        &RESPONSE_HEAD_MICROS,
        &BODY_READS,
        &BODY_READ_MICROS,
    ] {
        counter.store(0, Ordering::SeqCst);
    }
}

pub(crate) fn snapshot() -> Snapshot {
    Snapshot {
        writer_blocks: WRITER_BLOCKS.load(Ordering::SeqCst),
        writer_bytes: WRITER_BYTES.load(Ordering::SeqCst),
        cache_copied_bytes: CACHE_COPIED_BYTES.load(Ordering::SeqCst),
        cache_evicted_bytes: CACHE_EVICTED_BYTES.load(Ordering::SeqCst),
        fsync_calls: FSYNC_CALLS.load(Ordering::SeqCst),
        fsync_micros: FSYNC_MICROS.load(Ordering::SeqCst),
        queue_waits: QUEUE_WAITS.load(Ordering::SeqCst),
        queue_micros: QUEUE_MICROS.load(Ordering::SeqCst),
        preallocs: PREALLOCS.load(Ordering::SeqCst),
        prealloc_micros: PREALLOC_MICROS.load(Ordering::SeqCst),
        checksums: CHECKSUMS.load(Ordering::SeqCst),
        checksum_micros: CHECKSUM_MICROS.load(Ordering::SeqCst),
        response_heads: RESPONSE_HEADS.load(Ordering::SeqCst),
        response_head_micros: RESPONSE_HEAD_MICROS.load(Ordering::SeqCst),
        body_reads: BODY_READS.load(Ordering::SeqCst),
        body_read_micros: BODY_READ_MICROS.load(Ordering::SeqCst),
        driver_threads: driver_threads(),
    }
}

/// One block the writer wrote to disk, after seeking to its offset.
pub(crate) fn record_write_block(bytes: u64) {
    WRITER_BLOCKS.fetch_add(1, Ordering::Relaxed);
    WRITER_BYTES.fetch_add(bytes, Ordering::Relaxed);
}

/// Body bytes copied into the write-back cache by a lease append.
pub(crate) fn record_cache_copy(bytes: usize) {
    CACHE_COPIED_BYTES.fetch_add(bytes as u64, Ordering::Relaxed);
}

/// Bytes the write-back cache released back to the memory budget.
pub(crate) fn record_cache_evict(bytes: usize) {
    CACHE_EVICTED_BYTES.fetch_add(bytes as u64, Ordering::Relaxed);
}

/// One `flush` + `sync` pair issued by the writer task.
pub(crate) fn record_fsync(elapsed: Duration) {
    FSYNC_CALLS.fetch_add(1, Ordering::Relaxed);
    FSYNC_MICROS.fetch_add(elapsed.as_micros() as u64, Ordering::Relaxed);
}

/// Time spent between task start and the concurrency permit.
pub(crate) fn record_queue_wait(elapsed: Duration) {
    QUEUE_WAITS.fetch_add(1, Ordering::Relaxed);
    QUEUE_MICROS.fetch_add(elapsed.as_micros() as u64, Ordering::Relaxed);
}

/// Output-file creation and preallocation.
pub(crate) fn record_prealloc(elapsed: Duration) {
    PREALLOCS.fetch_add(1, Ordering::Relaxed);
    PREALLOC_MICROS.fetch_add(elapsed.as_micros() as u64, Ordering::Relaxed);
}

/// Post-download checksum verification.
pub(crate) fn record_checksum(elapsed: Duration) {
    CHECKSUMS.fetch_add(1, Ordering::Relaxed);
    CHECKSUM_MICROS.fetch_add(elapsed.as_micros() as u64, Ordering::Relaxed);
}

/// One HTTP request that received its response head, and the request→head time.
///
/// The caller has already measured that duration for its own diagnostics, so
/// there is nothing to skip and the enabled check lives here: a disabled run
/// pays one relaxed load.
pub(crate) fn record_response_head(elapsed: Duration) {
    if !enabled() {
        return;
    }
    RESPONSE_HEADS.fetch_add(1, Ordering::Relaxed);
    RESPONSE_HEAD_MICROS.fetch_add(elapsed.as_micros() as u64, Ordering::Relaxed);
}

/// One body read, and the time it waited for the next bytes.
pub(crate) fn record_body_read(elapsed: Duration) {
    BODY_READS.fetch_add(1, Ordering::Relaxed);
    BODY_READ_MICROS.fetch_add(elapsed.as_micros() as u64, Ordering::Relaxed);
}

/// Live libcurl driver threads.
pub(crate) fn driver_threads() -> i64 {
    DRIVER_THREADS.load(Ordering::SeqCst)
}

/// Counts one driver thread for as long as it runs.
///
/// Constructed by the driver thread itself, so the count reaches its baseline
/// exactly when the thread returns: a driver that never exits is visible as a
/// count that keeps growing.
pub(crate) struct DriverThreadGuard;

impl DriverThreadGuard {
    pub(crate) fn new() -> Self {
        DRIVER_THREADS.fetch_add(1, Ordering::SeqCst);
        Self
    }
}

impl Drop for DriverThreadGuard {
    fn drop(&mut self) {
        DRIVER_THREADS.fetch_sub(1, Ordering::SeqCst);
    }
}

/// A start instant, taken only while collection is on.
///
/// Recording sites read it as `enabled().then(Instant::now)` and hand it to
/// [`record_phase`], so a disabled run never calls the clock at all.
#[inline]
pub(crate) fn phase_start() -> Option<Instant> {
    enabled().then(Instant::now)
}

/// Report a phase duration captured with [`phase_start`].
#[inline]
pub(crate) fn record_phase(started: Option<Instant>, record: impl FnOnce(Duration)) {
    if let Some(started) = started {
        record(started.elapsed());
    }
}

/// Serialises every test that touches the process-wide counters.
///
/// The statics are shared by the whole test binary, and a test that enables
/// collection while another thread's writer happens to flush would make an
/// "off means off" assertion flaky. Holding this guard is what makes the
/// counter tests independent of each other; entering it also restores the
/// default (disabled, empty) state.
#[cfg(test)]
pub(crate) fn test_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let guard = LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    set_enabled(false);
    reset();
    guard
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lock() -> std::sync::MutexGuard<'static, ()> {
        test_lock()
    }

    /// The production guarantee that matters for cost: a disabled run never
    /// looks at the clock, so it records no duration at all.
    #[test]
    fn collection_is_off_until_the_harness_enables_it() {
        let _guard = lock();

        assert!(!enabled());
        assert!(phase_start().is_none());
        record_phase(phase_start(), |_| {
            panic!("a disabled phase must not be recorded")
        });
        assert_eq!(snapshot().preallocs, 0);
    }

    /// Queueing and verification are recorded by this crate's download path
    /// only, so no other unit test of this binary can contribute to them and
    /// the expected values are exact. The recorders reached by unit tests that
    /// run in parallel (the writer, the cache and the output file) are checked
    /// through real downloads in `tests/pipeline_counters.rs` instead.
    #[test]
    fn enabled_counters_accumulate_and_reset() {
        let _guard = lock();
        set_enabled(true);

        record_queue_wait(Duration::from_micros(250));
        record_checksum(Duration::from_micros(75));

        let collected = snapshot();
        assert_eq!(collected.queue_waits, 1);
        assert_eq!(collected.queue_micros, 250);
        assert_eq!(collected.checksums, 1);
        assert_eq!(collected.checksum_micros, 75);

        reset();
        assert_eq!(snapshot().queue_waits, 0);
        assert_eq!(snapshot().checksums, 0);
        set_enabled(false);
    }

    #[test]
    fn record_phase_reports_the_elapsed_time_only_when_started() {
        let _guard = lock();

        record_phase(None, |_| panic!("a missing start means no phase"));

        set_enabled(true);
        record_phase(phase_start(), record_prealloc);
        assert_eq!(snapshot().preallocs, 1);
        set_enabled(false);
    }

    /// The request and body recorders are reached by other tests in this binary
    /// (worker tests drive `send_request`, body tests read a real transport), so
    /// this asserts the deltas it caused instead of absolute totals.
    #[test]
    fn response_head_and_body_recorders_accumulate_their_own_time() {
        let _guard = lock();
        set_enabled(true);

        let before = snapshot();
        record_response_head(Duration::from_micros(1_200));
        record_body_read(Duration::from_micros(800));
        let after = snapshot();

        assert_eq!(after.response_heads - before.response_heads, 1);
        assert_eq!(
            after.response_head_micros - before.response_head_micros,
            1_200
        );
        assert_eq!(after.body_reads - before.body_reads, 1);
        assert_eq!(after.body_read_micros - before.body_read_micros, 800);

        reset();
        assert_eq!(snapshot().response_heads, 0);
        assert_eq!(snapshot().body_reads, 0);
        set_enabled(false);
    }
}
