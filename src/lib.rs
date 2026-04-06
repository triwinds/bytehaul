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
//! - **Proxy & custom DNS** – supports HTTP/HTTPS/SOCKS proxies and
//!   custom DNS / DNS-over-HTTPS resolvers.

mod checksum;
mod config;
mod error;
mod eta;
mod filename;
mod http;
#[macro_use]
mod logging;
mod manager;
mod network;
mod progress;
mod rate_limiter;
mod scheduler;
mod session;
mod storage;

pub use config::{Checksum, DownloadSpec, FileAllocation, LogLevel};
pub use error::DownloadError;
pub use manager::{DownloadHandle, Downloader, DownloaderBuilder};
pub use progress::{DownloadState, ProgressSnapshot};

/// Re-exports for benchmarking. Not part of the public API.
#[doc(hidden)]
pub mod bench {
    use crate::scheduler::SchedulerState;
    use std::time::Duration;

    pub use crate::progress::bench_progress_reporting;
    pub use crate::storage::cache::WriteBackCache;
    pub use crate::storage::control::ControlSnapshot;
    pub use crate::storage::piece_map::PieceMap;

    pub fn bench_scheduler_snapshot(total_size: u64, piece_size: u64) -> (usize, u64) {
        let mut scheduler = SchedulerState::new(PieceMap::new(total_size, piece_size));
        while let Some(segment) = scheduler.assign() {
            scheduler.complete(segment.piece_id);
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
}

#[cfg(test)]
mod tests {
    use super::bench::{bench_cached_client_lookup, bench_scheduler_snapshot};
    use super::Downloader;
    use std::time::Duration;

    #[test]
    fn test_bench_scheduler_snapshot_marks_all_bytes_complete() {
        let (bitset_len, downloaded_bytes) = bench_scheduler_snapshot(1024, 256);

        assert_eq!(downloaded_bytes, 1024);
        assert!(bitset_len >= 1);
    }

    #[test]
    fn test_bench_cached_client_lookup_reuses_existing_clients() {
        let downloader = Downloader::builder().build().unwrap();

        let cached_count = bench_cached_client_lookup(&downloader, Duration::from_secs(30), 3);
        let distinct_count = bench_cached_client_lookup(&downloader, Duration::from_secs(3), 1);

        assert_eq!(cached_count, 1);
        assert_eq!(distinct_count, 2);
    }
}
