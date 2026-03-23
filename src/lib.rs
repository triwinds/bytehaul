mod checksum;
mod config;
mod error;
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
