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
    pub use crate::storage::cache::WriteBackCache;
    pub use crate::storage::control::ControlSnapshot;
    pub use crate::storage::piece_map::PieceMap;
}
