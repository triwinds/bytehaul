//! Local diagnostic: `cargo run --release --example slow_transfer_compare -- [rounds]`.
//! Isolate proxy environment variables first. CSV measures a 4 MiB HTTP/1.1
//! transfer with 128 pieces and four workers. Test thresholds are shortened;
//! these results do not estimate WAN performance or tune production defaults.
//! `requested_overhead_bytes` includes an initial probe that may be discarded.
//! `peak_server_handlers` can include a cancelled probe until its socket closes;
//! it is not the count of active client requests. `extra_tail_attempts` isolates
//! additional requests for the slow trailing piece.

use std::io;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytehaul::{DownloadSpec, Downloader, FileAllocation, SlowTransferMode};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinSet;

const PIECE: usize = 32 * 1024;
const SIZE: usize = 128 * PIECE;

#[derive(Default)]
struct Stats {
    requests: AtomicUsize,
    requested_bytes: AtomicU64,
    active: AtomicUsize,
    peak: AtomicUsize,
    tail_attempts: AtomicUsize,
    tail_started: Mutex<Option<Instant>>,
}

struct Active(Arc<Stats>);

impl Drop for Active {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::SeqCst);
    }
}

async fn serve(socket: TcpStream, stats: Arc<Stats>, scenario: &'static str) -> io::Result<()> {
    socket.set_nodelay(true)?;
    let mut reader = BufReader::new(socket);
    let mut line = String::new();
    let mut range = None;
    loop {
        line.clear();
        if reader.read_line(&mut line).await? == 0 {
            return Ok(());
        }
        if line == "\r\n" {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("range") {
                range = Some(value.trim().to_string());
            }
        }
    }
    let (start, end) = range
        .as_deref()
        .and_then(|s| s.strip_prefix("bytes="))
        .and_then(|s| s.split_once('-'))
        .ok_or_else(|| io::Error::other("expected Range request"))?;
    let start = start.parse::<usize>().map_err(io::Error::other)?;
    let end = end.parse::<usize>().map_err(io::Error::other)?;
    if start > end || end >= SIZE {
        return Err(io::Error::other("invalid Range bounds"));
    }
    stats.requests.fetch_add(1, Ordering::SeqCst);
    stats
        .requested_bytes
        .fetch_add((end - start + 1) as u64, Ordering::SeqCst);
    let active = stats.active.fetch_add(1, Ordering::SeqCst) + 1;
    stats.peak.fetch_max(active, Ordering::SeqCst);
    let _active = Active(stats.clone());
    let is_tail = start >= SIZE - PIECE;
    let first_tail = if is_tail {
        stats
            .tail_started
            .lock()
            .unwrap()
            .get_or_insert_with(Instant::now);
        stats.tail_attempts.fetch_add(1, Ordering::SeqCst) == 0
    } else {
        false
    };
    reader.get_mut().write_all(format!(
        "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {start}-{end}/{SIZE}\r\nETag: \"benchmark-v1\"\r\nConnection: close\r\n\r\n",
        end - start + 1,
    ).as_bytes()).await?;
    let slow_tail = scenario == "tail" && first_tail;
    let chunk = if slow_tail { 256 } else { 4096 };
    for offset in (start..=end).step_by(chunk) {
        if slow_tail {
            tokio::time::sleep(Duration::from_millis(50)).await;
        } else if scenario == "all_slow" {
            tokio::time::sleep(Duration::from_millis(4)).await;
        }
        let bytes: Vec<_> = (offset..(offset + chunk).min(end + 1))
            .map(|i| (i % 251) as u8)
            .collect();
        reader.get_mut().write_all(&bytes).await?;
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let rounds = std::env::args()
        .nth(1)
        .map(|s| s.parse::<usize>())
        .transpose()?
        .unwrap_or(3);
    if rounds == 0 {
        return Err(io::Error::other("rounds must be positive").into());
    }
    println!("scenario,round,mode,elapsed_ms,tail_ms,requests,requested_overhead_bytes,peak_server_handlers,extra_tail_attempts,verified_bytes");
    let directory = tempfile::tempdir()?;
    for scenario in ["fast", "tail", "all_slow", "limited"] {
        for round in 0..rounds {
            let modes = [
                SlowTransferMode::Disabled,
                SlowTransferMode::Adaptive,
                SlowTransferMode::AdaptiveWithHedging,
            ];
            for index in 0..modes.len() {
                let mode = modes[(index + round) % modes.len()];
                let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
                let address = listener.local_addr()?;
                let stats = Arc::new(Stats::default());
                let server_stats = stats.clone();
                let server = tokio::spawn(async move {
                    let mut connections = JoinSet::new();
                    loop {
                        tokio::select! {
                            socket = listener.accept() => {
                                let (socket, _) = socket?;
                                connections.spawn(serve(socket, server_stats.clone(), scenario));
                            }
                            Some(result) = connections.join_next() => {
                                if let Err(error) = result? {
                                    if !matches!(error.kind(), io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset) {
                                        return Err::<(), io::Error>(error);
                                    }
                                }
                            }
                        }
                    }
                });
                let output = directory.path().join("download.bin");
                let mut spec = DownloadSpec::new(format!("http://{address}/data"))
                    .output_path(&output)
                    .piece_size(PIECE as u64)
                    .min_split_size(1)
                    .max_connections(4)
                    .resume(false)
                    .file_allocation(FileAllocation::None)
                    .slow_transfer_mode(mode)
                    .slow_start_grace(Duration::from_millis(250))
                    .slow_sample_window(Duration::from_millis(250))
                    .low_speed_duration(Duration::from_millis(500));
                if scenario == "tail" {
                    spec = spec.low_speed_limit(32 * 1024);
                }
                if scenario == "limited" {
                    spec = spec.max_download_speed(1024 * 1024).memory_budget(4097);
                }
                let started = Instant::now();
                let result = tokio::time::timeout(
                    Duration::from_secs(60),
                    Downloader::builder().build()?.download(spec).wait(),
                )
                .await;
                let finished = Instant::now();
                server.abort();
                let _ = server.await;
                result??;
                let actual = tokio::fs::read(&output).await?;
                if actual.len() != SIZE
                    || actual
                        .iter()
                        .enumerate()
                        .any(|(i, b)| *b != (i % 251) as u8)
                {
                    return Err(io::Error::other("output mismatch").into());
                }
                let tail_ms = stats
                    .tail_started
                    .lock()
                    .unwrap()
                    .map(|t| finished.saturating_duration_since(t).as_secs_f64() * 1000.0)
                    .unwrap_or(0.0);
                println!(
                    "{scenario},{},{mode:?},{:.3},{tail_ms:.3},{},{},{},{},{}",
                    round + 1,
                    finished.duration_since(started).as_secs_f64() * 1000.0,
                    stats.requests.load(Ordering::SeqCst),
                    stats
                        .requested_bytes
                        .load(Ordering::SeqCst)
                        .saturating_sub(SIZE as u64),
                    stats.peak.load(Ordering::SeqCst),
                    stats.tail_attempts.load(Ordering::SeqCst).saturating_sub(1),
                    actual.len()
                );
                tokio::fs::remove_file(output).await?;
            }
        }
    }
    Ok(())
}
