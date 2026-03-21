mod checksum;
mod config;
mod error;
mod http;
mod manager;
mod progress;
mod rate_limiter;
mod scheduler;
mod session;
mod storage;

pub use config::{Checksum, DownloadSpec, FileAllocation};
pub use error::DownloadError;
pub use manager::{DownloadHandle, Downloader, DownloaderBuilder};
pub use progress::{DownloadState, ProgressSnapshot};

