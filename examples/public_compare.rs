//! Diagnostic download entry point for scripts/compare_public.py.
use bytehaul::{DownloadSpec, Downloader, FileAllocation, LogLevel};
use std::{collections::HashMap, time::Duration};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .with_thread_ids(true)
        .with_writer(std::io::stderr)
        .init();
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 4 {
        return Err("usage: public_compare URL OUTPUT CONNECTIONS".into());
    }
    let downloader = Downloader::builder()
        .enable_ipv6(false)
        .log_level(LogLevel::Trace)
        .build()?;
    let spec = DownloadSpec::new(&args[1])
        .output_path(&args[2])
        .max_connections(args[3].parse()?)
        .piece_size(1024 * 1024)
        .min_split_size(1024 * 1024)
        .file_allocation(FileAllocation::None)
        .connect_timeout(Duration::from_secs(15))
        .read_timeout(Duration::from_secs(30))
        .request_headers_timeout(Duration::from_secs(30))
        .max_retries(2)
        .headers(HashMap::from([(
            "User-Agent".into(),
            "public-download-compare/1.0".into(),
        )]));
    let handle = downloader.download(spec);
    let mut progress = handle.subscribe_progress();
    let monitor = tokio::spawn(async move {
        while progress.changed().await.is_ok() {
            eprintln!("PROGRESS {:?}", *progress.borrow_and_update());
        }
    });
    let result = handle.wait().await;
    monitor.abort();
    result?;
    Ok(())
}
