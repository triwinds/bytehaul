//! # Bytehaul
//!
//! An async HTTP download library with multi-connection acceleration,
//! automatic resume, rate limiting, and checksum verification.
//!
//! ## Quick Start
//!
//! ```no_run
//! # async fn example() -> Result<(), bytehaul::DownloadError> {
//! use bytehaul::{Downloader, DownloadSpec};
//!
//! let downloader = Downloader::builder().build()?;
//! let spec = DownloadSpec::new("https://example.com/file.bin")
//!     .output_path("/tmp/file.bin")
//!     .max_connections(8);
//! let handle = downloader.download(spec);
//! handle.wait().await?;
//! # Ok(())
//! # }
//! ```
//!
//! ## Features
//!
//! - **Multi-connection downloads** – splits large files into pieces and
//!   fetches them in parallel using HTTP Range requests.
//! - **Automatic resume** – persists download progress to a control file
//!   so interrupted downloads can continue where they left off.
//! - **Rate limiting** – configurable per-download speed cap via a
//!   token-bucket rate limiter.
//! - **Checksum verification** – optional post-download integrity check
//!   with SHA-256, SHA-512, SHA-1, or MD5.
//! - **Progress monitoring** – real-time progress snapshots with speed
//!   and ETA, available via polling, watch channel, or callback.
//! - **Proxy & custom DNS** – supports HTTP/HTTPS proxies and
//!   custom DNS / DNS-over-HTTPS resolvers.

// The production transport is selected at build time. Keeping the feature
// explicit lets downstream builds opt out of the native libcurl dependency
// intentionally, while a normal build always uses the default feature.
#[cfg(not(feature = "curl-backend"))]
compile_error!("bytehaul requires the `curl-backend` feature");

mod bench_stats;
mod checksum;
mod config;
mod error;
mod eta;
mod filename;
#[macro_use]
mod logging;
mod http;
mod manager;
mod network;
mod progress;
mod rate_limiter;
mod scheduler;
mod session;
mod storage;

pub use config::{
    Checksum, DownloadSpec, FileAllocation, LogLevel, RangeSchedulingMode, SlowTransferMode,
};
pub use error::DownloadError;
pub use manager::{DownloadHandle, Downloader, DownloaderBuilder};
pub use progress::{DownloadState, ProgressSnapshot};

/// Re-exports for benchmarking. Not part of the public API.
#[doc(hidden)]
pub mod bench {
    use crate::scheduler::SchedulerState;
    use crate::storage::segment::LeaseKey;
    use std::time::Duration;

    pub use crate::progress::bench_progress_reporting;
    pub use crate::storage::cache::WriteBackCache;
    pub use crate::storage::control::ControlSnapshot;
    pub use crate::storage::piece_map::PieceMap;

    pub struct BenchScheduler(SchedulerState);

    pub fn bench_scheduler_new(total_size: u64, piece_size: u64) -> BenchScheduler {
        BenchScheduler(SchedulerState::new(PieceMap::new(total_size, piece_size)))
    }

    pub fn bench_scheduler_assign_complete(
        scheduler: &mut BenchScheduler,
        assignments: usize,
        max_active_leases: usize,
        min_segment_size: u64,
    ) -> usize {
        let mut completed = 0;
        for _ in 0..assignments {
            let Some(segment) =
                scheduler
                    .0
                    .assign_to_with_split(0, max_active_leases, min_segment_size)
            else {
                break;
            };
            assert!(scheduler.0.complete(segment.lease_key()));
            completed += 1;
        }
        completed
    }

    pub fn bench_scheduler_snapshot(total_size: u64, piece_size: u64) -> (usize, u64) {
        let mut scheduler = SchedulerState::new(PieceMap::new(total_size, piece_size));
        while let Some(segment) = scheduler.assign() {
            scheduler.complete(segment.lease_key());
        }

        let snapshot = ControlSnapshot {
            url: "https://example.com/bench".into(),
            total_size,
            piece_size: scheduler.piece_size(),
            piece_count: scheduler.piece_count(),
            completed_bitset: scheduler.snapshot_bitset(),
            downloaded_bytes: scheduler.completed_bytes(),
            etag: None,
            last_modified: None,
        };

        (snapshot.completed_bitset.len(), snapshot.downloaded_bytes)
    }

    pub fn bench_cached_client_lookup(
        downloader: &crate::manager::Downloader,
        connect_timeout: Duration,
        lookups: usize,
    ) -> usize {
        for _ in 0..lookups {
            downloader
                .bench_cached_client_lookup(connect_timeout)
                .expect("client lookup should succeed during benchmark setup");
        }
        downloader.bench_cached_client_count()
    }

    pub fn bench_cache_new() -> WriteBackCache {
        WriteBackCache::new()
    }

    pub fn bench_cache_insert(
        cache: &mut WriteBackCache,
        piece_id: usize,
        lease_id: u64,
        offset: u64,
        data: bytes::Bytes,
    ) {
        cache
            .insert(LeaseKey { piece_id, lease_id }, offset, data)
            .expect("benchmark writes must be contiguous");
    }

    pub fn bench_cache_total_bytes(cache: &WriteBackCache) -> usize {
        cache.total_bytes()
    }

    pub fn bench_cache_drain_lease_len(
        cache: &mut WriteBackCache,
        piece_id: usize,
        lease_id: u64,
    ) -> usize {
        cache.drain_lease(LeaseKey { piece_id, lease_id }).len()
    }

    /// Counters recorded by the production download path.
    ///
    /// Collection is off by default; `bench_counters_set_enabled(true)` turns
    /// it on for a measurement run. Values are the ones the default path
    /// really produced, not a benchmark-only reimplementation.
    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
    pub struct BenchCounters {
        /// Blocks the writer flushed to disk, one seek+write pair each.
        pub writer_blocks: u64,
        /// Bytes those blocks contained.
        pub writer_bytes: u64,
        /// Body bytes copied into the write-back cache.
        pub cache_copied_bytes: u64,
        /// Bytes the write-back cache released again.
        pub cache_evicted_bytes: u64,
        /// `flush`+`sync` pairs issued by the writer task.
        pub fsync_calls: u64,
        /// Time spent inside those pairs.
        pub fsync_micros: u64,
        /// Downloads that waited for a concurrency permit.
        pub queue_waits: u64,
        /// Time those downloads waited.
        pub queue_micros: u64,
        /// Output files created (including preallocation).
        pub preallocs: u64,
        /// Time spent creating and preallocating them.
        pub prealloc_micros: u64,
        /// Checksum verifications run.
        pub checksums: u64,
        /// Time spent verifying.
        pub checksum_micros: u64,
        /// HTTP requests that received a response head (redirects, probes and
        /// retried attempts each count).
        pub response_heads: u64,
        /// Summed request→response-head time over those requests.
        pub response_head_micros: u64,
        /// Body reads that waited for bytes.
        pub body_reads: u64,
        /// Summed wait for body bytes.
        pub body_read_micros: u64,
        /// Live libcurl driver threads.
        pub driver_threads: i64,
    }

    impl From<crate::bench_stats::Snapshot> for BenchCounters {
        fn from(snapshot: crate::bench_stats::Snapshot) -> Self {
            Self {
                writer_blocks: snapshot.writer_blocks,
                writer_bytes: snapshot.writer_bytes,
                cache_copied_bytes: snapshot.cache_copied_bytes,
                cache_evicted_bytes: snapshot.cache_evicted_bytes,
                fsync_calls: snapshot.fsync_calls,
                fsync_micros: snapshot.fsync_micros,
                queue_waits: snapshot.queue_waits,
                queue_micros: snapshot.queue_micros,
                preallocs: snapshot.preallocs,
                prealloc_micros: snapshot.prealloc_micros,
                checksums: snapshot.checksums,
                checksum_micros: snapshot.checksum_micros,
                response_heads: snapshot.response_heads,
                response_head_micros: snapshot.response_head_micros,
                body_reads: snapshot.body_reads,
                body_read_micros: snapshot.body_read_micros,
                driver_threads: snapshot.driver_threads,
            }
        }
    }

    pub fn bench_counters_enabled() -> bool {
        crate::bench_stats::enabled()
    }

    pub fn bench_counters_set_enabled(enabled: bool) {
        crate::bench_stats::set_enabled(enabled);
    }

    /// Clears every accumulator. Driver threads are a live gauge, not a
    /// counter, and are deliberately left alone.
    pub fn bench_counters_reset() {
        crate::bench_stats::reset();
    }

    pub fn bench_counters_snapshot() -> BenchCounters {
        crate::bench_stats::snapshot().into()
    }

    /// Live libcurl driver threads in this process.
    pub fn bench_driver_threads() -> i64 {
        crate::bench_stats::driver_threads()
    }

    /// Driver counters of the downloader's default client, if it has one.
    pub fn bench_driver_stats(downloader: &crate::manager::Downloader) -> Option<BenchDriverStats> {
        let client = downloader.bench_default_client().ok()?;
        client.driver_stats().map(BenchDriverStats::from)
    }

    /// Counters reported by the libcurl driver thread.
    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
    pub struct BenchDriverStats {
        pub submitted: u64,
        pub completed: u64,
        pub cancelled: u64,
        pub connections: u64,
        pub commands: u64,
        /// Turns of the driver's main loop.
        pub loops: u64,
        pub pauses: u64,
        pub resumes: u64,
        pub idle_clears: u64,
        pub max_command_latency_micros: u64,
        pub active: usize,
        pub pools: usize,
    }

    impl From<crate::network::curl::driver::DriverStats> for BenchDriverStats {
        fn from(stats: crate::network::curl::driver::DriverStats) -> Self {
            Self {
                submitted: stats.submitted,
                completed: stats.completed,
                cancelled: stats.cancelled,
                connections: stats.connections,
                commands: stats.commands,
                loops: stats.loops,
                pauses: stats.pauses,
                resumes: stats.resumes,
                idle_clears: stats.idle_clears,
                max_command_latency_micros: stats.max_command_latency.as_micros() as u64,
                active: stats.active,
                pools: stats.pools,
            }
        }
    }

    /// Number of clients the downloader has cached.
    pub fn bench_cached_client_count(downloader: &crate::manager::Downloader) -> usize {
        downloader.bench_cached_client_count()
    }

    /// Builds a scheduler whose completed pieces are already marked.
    ///
    /// Lets a scenario describe the shape it is measuring (a contiguous hole, a
    /// resumed suffix, a partial checkpoint) instead of only a fresh file.
    pub fn bench_scheduler_from_piece_map(piece_map: PieceMap) -> BenchScheduler {
        BenchScheduler(SchedulerState::new(piece_map))
    }

    /// The planning parameters the default download path passes.
    ///
    /// Defaults mirror `DownloadSpec`'s defaults for a 1 MiB piece size, so a
    /// measurement run describes the code that actually runs.
    #[derive(Debug, Clone, Copy)]
    pub struct BenchPlanParams {
        pub mode: crate::config::RangeSchedulingMode,
        /// Requests the caller keeps in flight at the same time.
        pub in_flight_requests: usize,
        pub min_segment_size: u64,
        pub request_batch_size: u64,
        pub dynamic_min_split_size: u64,
        pub dynamic_max_request_size: u64,
    }

    impl Default for BenchPlanParams {
        fn default() -> Self {
            Self {
                mode: crate::config::RangeSchedulingMode::Dynamic,
                in_flight_requests: 4,
                min_segment_size: 256 * 1024,
                request_batch_size: 4 * 1024 * 1024,
                dynamic_min_split_size: 1024 * 1024,
                dynamic_max_request_size: 64 * 1024 * 1024,
            }
        }
    }

    /// What one planning run produced.
    #[derive(Debug, Clone, Default, PartialEq, Eq)]
    pub struct BenchPlanResult {
        /// HTTP requests the planner would issue.
        pub requests: u64,
        /// Piece leases those requests own.
        pub leases: u64,
        /// Bytes the requests cover.
        pub bytes: u64,
        /// Requests whose boundary scan had to walk a fresh run.
        pub boundary_scans: u64,
        /// Ranges the planner considered per request, summed.
        pub planned_ranges: u64,
        /// Candidate slots the dynamic planner saw, summed.
        pub candidate_slots: u64,
        /// Requests the planner had to truncate.
        pub truncated_requests: u64,
        /// Requests that own exactly one lease.
        pub single_lease_requests: u64,
        /// Requests that own more than one lease.
        pub multi_lease_requests: u64,
        /// Largest number of leases one request owned.
        pub max_leases_per_request: u64,
        /// Smallest request size, in bytes.
        pub min_request_bytes: u64,
        /// Largest request size, in bytes.
        pub max_request_bytes: u64,
    }

    /// Drives the planner exactly as the adaptive worker loop does.
    ///
    /// Every in-flight request occupies one request slot and keeps its leases
    /// until it is completed, oldest first, which is the shape the default
    /// multi-connection path creates. This is the path the existing
    /// `assign_to_with_split` benchmark does not cover.
    pub fn bench_scheduler_plan(
        scheduler: &mut BenchScheduler,
        params: &BenchPlanParams,
    ) -> BenchPlanResult {
        let mut result = BenchPlanResult::default();
        let mut in_flight: std::collections::VecDeque<Vec<crate::storage::segment::Segment>> =
            std::collections::VecDeque::new();
        let mut occupied = 0usize;

        loop {
            while in_flight.len() < params.in_flight_requests {
                let Some(assignment) = scheduler.0.assign_request_with_diagnostics(
                    0,
                    params.in_flight_requests,
                    params.min_segment_size,
                    occupied,
                    params.mode,
                    params.request_batch_size,
                    params.dynamic_min_split_size,
                    params.dynamic_max_request_size,
                    true,
                    true,
                ) else {
                    break;
                };

                let request_bytes: u64 = assignment
                    .segments
                    .iter()
                    .map(|segment| segment.end - segment.start)
                    .sum();
                result.requests += 1;
                result.leases += assignment.segments.len() as u64;
                result.bytes += request_bytes;
                result.boundary_scans += u64::from(assignment.candidate_boundary_scanned);
                result.planned_ranges += assignment.planned_ranges as u64;
                result.candidate_slots += assignment.candidate_slots as u64;
                result.truncated_requests += u64::from(assignment.truncation_reason != "none");
                if assignment.segments.len() == 1 {
                    result.single_lease_requests += 1;
                } else {
                    result.multi_lease_requests += 1;
                }
                result.max_leases_per_request = result
                    .max_leases_per_request
                    .max(assignment.segments.len() as u64);
                result.max_request_bytes = result.max_request_bytes.max(request_bytes);
                result.min_request_bytes = if result.min_request_bytes == 0 {
                    request_bytes
                } else {
                    result.min_request_bytes.min(request_bytes)
                };
                occupied += 1;
                in_flight.push_back(assignment.segments);
            }

            let Some(oldest) = in_flight.pop_front() else {
                break;
            };
            for segment in &oldest {
                assert!(
                    scheduler.0.complete(segment.lease_key()),
                    "a planned lease must still be owned when its request completes"
                );
            }
            occupied -= 1;
        }

        result
    }
}

#[cfg(test)]
mod tests {
    use super::bench::{
        bench_cached_client_count, bench_cached_client_lookup, bench_counters_enabled,
        bench_counters_set_enabled, bench_driver_stats, bench_scheduler_assign_complete,
        bench_scheduler_from_piece_map, bench_scheduler_new, bench_scheduler_plan,
        bench_scheduler_snapshot, BenchPlanParams,
    };
    use super::storage::piece_map::PieceMap;
    use super::Downloader;
    use std::time::Duration;

    #[test]
    fn test_bench_scheduler_snapshot_marks_all_bytes_complete() {
        let (bitset_len, downloaded_bytes) = bench_scheduler_snapshot(1024, 256);

        assert_eq!(downloaded_bytes, 1024);
        assert!(bitset_len >= 1);
    }

    #[test]
    fn test_bench_scheduler_assign_complete_drives_split_path() {
        let mut scheduler = bench_scheduler_new(1_024, 1_024);
        assert_eq!(
            bench_scheduler_assign_complete(&mut scheduler, 2, 4, 256),
            2
        );
        assert_eq!(
            bench_scheduler_assign_complete(&mut scheduler, 4, 4, 256),
            2
        );
        assert_eq!(
            bench_scheduler_assign_complete(&mut scheduler, 1, 4, 256),
            0
        );
    }

    #[test]
    fn test_bench_cached_client_lookup_reuses_existing_clients() {
        let downloader = Downloader::builder().build().unwrap();

        let cached_count = bench_cached_client_lookup(&downloader, Duration::from_secs(30), 3);
        let distinct_count = bench_cached_client_lookup(&downloader, Duration::from_secs(3), 1);

        assert_eq!(cached_count, 1);
        assert_eq!(distinct_count, 1);
        assert_eq!(bench_cached_client_count(&downloader), 1);
    }

    #[test]
    fn test_bench_counters_round_trip_their_enabled_flag() {
        // The counters are process-wide, so this test holds the same lock the
        // counter tests hold: flipping the flag while another test's writer
        // flushes would make their "off means off" assertions flaky.
        let _guard = crate::bench_stats::test_lock();

        assert!(!bench_counters_enabled(), "collection is off by default");
        bench_counters_set_enabled(true);
        assert!(bench_counters_enabled());
        bench_counters_set_enabled(false);
        assert!(!bench_counters_enabled());
    }

    #[test]
    fn test_bench_scheduler_plan_consumes_every_missing_piece() {
        let total_size = 16 * 1024 * 1024;
        let piece_size = 1024 * 1024;

        for mode in [
            crate::config::RangeSchedulingMode::Dynamic,
            crate::config::RangeSchedulingMode::Fixed,
        ] {
            let mut scheduler =
                bench_scheduler_from_piece_map(PieceMap::new(total_size, piece_size));
            let params = BenchPlanParams {
                mode,
                ..BenchPlanParams::default()
            };

            let plan = bench_scheduler_plan(&mut scheduler, &params);
            assert!(plan.requests > 0, "{mode} must plan at least one request");
            assert_eq!(plan.bytes, total_size, "{mode} must cover the whole file");
            assert!(plan.max_request_bytes >= plan.min_request_bytes);

            // Every planned lease is completed, so a second run has nothing
            // left to hand out.
            let second = bench_scheduler_plan(&mut scheduler, &params);
            assert_eq!(second.requests, 0, "{mode} must leave no piece unplanned");
        }
    }

    #[test]
    fn test_bench_scheduler_plan_leaves_completed_pieces_alone() {
        let total_size = 16 * 1024 * 1024;
        let piece_size = 1024 * 1024;
        let mut piece_map = PieceMap::new(total_size, piece_size);
        // A resumed suffix (the tail is already complete) plus two holes in
        // the prefix, which is what a resumed download with a dirty piece
        // looks like.
        for piece_id in [4, 6, 8, 9, 10, 11, 12, 13, 14, 15] {
            piece_map.mark_complete(piece_id);
        }
        let expected = total_size - 10 * piece_size;

        let mut scheduler = bench_scheduler_from_piece_map(piece_map);
        let plan = bench_scheduler_plan(&mut scheduler, &BenchPlanParams::default());
        assert_eq!(plan.bytes, expected);
        assert_eq!(
            bench_scheduler_plan(&mut scheduler, &BenchPlanParams::default()).requests,
            0
        );
    }

    /// The live driver-thread gauge is process-wide, and every
    /// `Downloader::builder().build()` in this binary starts a driver, so it
    /// is asserted in `tests/pipeline_counters.rs`, where one test owns the
    /// whole process. What is asserted here is the per-client driver state,
    /// which no other test can change.
    #[test]
    fn test_bench_driver_stats_follow_one_client() {
        let downloader = Downloader::builder().build().unwrap();
        let stats = bench_driver_stats(&downloader).expect("the default client has a driver");

        assert_eq!(stats.submitted, 0, "no request was issued");
        assert_eq!(stats.completed, 0);
        assert_eq!(stats.cancelled, 0);
        assert_eq!(stats.active, 0);
        assert_eq!(stats.pools, 0, "a driver without a transfer holds no pool");
        // A live driver has already completed its first loop turn.
        assert!(wait_until(
            || bench_driver_stats(&downloader).is_some_and(|stats| stats.loops > 0),
            Duration::from_secs(5)
        ));
    }

    /// Repeatedly evaluates `predicate` until it holds or `timeout` expires.
    fn wait_until(mut predicate: impl FnMut() -> bool, timeout: Duration) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if predicate() {
                return true;
            }
            if std::time::Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
