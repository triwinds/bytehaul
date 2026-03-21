mod config;
mod error;
mod http;
mod manager;
mod progress;
mod session;
mod storage;

pub use config::{DownloadSpec, FileAllocation};
pub use error::DownloadError;
pub use manager::{DownloadHandle, Downloader, DownloaderBuilder};
pub use progress::{DownloadState, ProgressSnapshot};

