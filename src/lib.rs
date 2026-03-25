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
