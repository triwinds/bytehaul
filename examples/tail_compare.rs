//! Controlled 128 MiB loopback comparison using production policy defaults.
//! `cargo run --example tail_compare -- <aria2c-path-or-dash> [rounds] [filter]`
//! A dash skips aria2. Filters: normal-only, tail-only, recovery-only,
//! hedge-only, aria-only.
//! CSV goes to stdout; request/recovery diagnostics go to stderr.
//! Matches the external tail report fixture; not a WAN throughput benchmark.
use bytehaul::{DownloadSpec, Downloader, FileAllocation, LogLevel, SlowTransferMode};
use std::{
    io,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
    task::JoinSet,
};

const SIZE: usize = 128 * 1024 * 1024;
const TAIL: usize = SIZE - 1024 * 1024;
#[derive(Default)]
struct Stats {
    requests: AtomicUsize,
    tails: AtomicUsize,
    started: Option<Instant>,
}

async fn serve(socket: TcpStream, stats: Arc<Stats>, slow: bool) -> io::Result<()> {
    socket.set_nodelay(true)?;
    let mut reader = BufReader::new(socket);
    let mut line = String::new();
    reader.read_line(&mut line).await?;
    let head = line.starts_with("HEAD ");
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
                range = Some(value.trim().to_owned());
            }
        }
    }
    let (start, end) = if let Some(value) = &range {
        let (a, b) = value
            .strip_prefix("bytes=")
            .and_then(|v| v.split_once('-'))
            .ok_or_else(|| io::Error::other("bad range"))?;
        (
            a.parse::<usize>().map_err(io::Error::other)?,
            if b.is_empty() {
                SIZE - 1
            } else {
                b.parse::<usize>().map_err(io::Error::other)?.min(SIZE - 1)
            },
        )
    } else {
        (0, SIZE - 1)
    };
    if start > end {
        return Err(io::Error::other("invalid range"));
    }
    let request_id = stats.requests.fetch_add(1, Ordering::SeqCst) + 1;
    if let Some(started) = stats.started {
        eprintln!(
            "SERVER {:.3}s request={request_id} range={range:?} start={start} end={end}",
            started.elapsed().as_secs_f64()
        );
    }
    let status = if range.is_some() {
        "206 Partial Content"
    } else {
        "200 OK"
    };
    let content_range = if range.is_some() {
        format!("Content-Range: bytes {start}-{end}/{SIZE}\r\n")
    } else {
        String::new()
    };
    reader.get_mut().write_all(format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\n{content_range}Accept-Ranges: bytes\r\nETag: \"tail-test-v1\"\r\nConnection: close\r\n\r\n",end-start+1).as_bytes()).await?;
    if head {
        return Ok(());
    }
    let mut offset = start;
    let mut tail_slow = None;
    while offset <= end {
        if offset >= TAIL && tail_slow.is_none() {
            tail_slow = Some(stats.tails.fetch_add(1, Ordering::SeqCst) == 0 && slow);
            if let Some(started) = stats.started {
                eprintln!(
                    "SERVER {:.3}s request={request_id} tail_offset={offset} slow={tail_slow:?}",
                    started.elapsed().as_secs_f64()
                );
            }
        }
        let len = if tail_slow == Some(true) {
            4096
        } else {
            64 * 1024
        };
        let next = (offset + len)
            .min(end + 1)
            .min(if offset < TAIL { TAIL } else { SIZE });
        tokio::time::sleep(if tail_slow == Some(true) {
            Duration::from_millis(125)
        } else {
            Duration::from_millis(1)
        })
        .await;
        let data: Vec<u8> = (offset..next).map(|i| (i % 251) as u8).collect();
        reader.get_mut().write_all(&data).await?;
        offset = next;
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();
    let aria = std::env::args().nth(1).ok_or("pass aria2c path or -")?;
    let rounds: usize = std::env::args().nth(2).unwrap_or("3".into()).parse()?;
    if rounds == 0 {
        return Err("rounds must be positive".into());
    }
    let filter = std::env::args().nth(3);
    if !matches!(
        filter.as_deref(),
        None | Some("normal-only" | "tail-only" | "recovery-only" | "hedge-only" | "aria-only")
    ) {
        return Err("unknown filter".into());
    }
    let directory = tempfile::tempdir()?;
    let modes = ["disabled", "adaptive", "hedging", "aria2"];
    println!("scenario,round,mode,seconds,requests,tail_attempts,verified_bytes");
    for slow in [false, true] {
        for round in 0..rounds {
            for i in 0..modes.len() {
                let mode = modes[(i + round) % modes.len()];
                if (aria == "-" && mode == "aria2")
                    || (filter.as_deref() == Some("normal-only") && slow)
                    || (filter.as_deref() == Some("tail-only") && !slow)
                    || (filter.as_deref() == Some("recovery-only")
                        && (!slow || !matches!(mode, "adaptive" | "hedging")))
                {
                    continue;
                }
                if filter.as_deref() == Some("hedge-only") && (!slow || mode != "hedging") {
                    continue;
                }
                let aria_diagnostic = filter.as_deref() == Some("aria-only");
                if aria_diagnostic && (!slow || mode != "aria2") {
                    continue;
                }
                let listener = TcpListener::bind("127.0.0.1:0").await?;
                let url = format!("http://{}/data.bin", listener.local_addr()?);
                let stats = Arc::new(Stats {
                    started: Some(Instant::now()),
                    ..Stats::default()
                });
                let shared = stats.clone();
                let server = tokio::spawn(async move {
                    let mut jobs = JoinSet::new();
                    loop {
                        tokio::select! {
                            socket = listener.accept() => {
                                let (socket,_) = socket?;
                                jobs.spawn(serve(socket, shared.clone(), slow));
                            }
                            Some(result) = jobs.join_next() => {
                                if let Err(error) = result? {
                                    if !matches!(error.kind(), io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset | io::ErrorKind::ConnectionAborted) { return Err::<(),io::Error>(error); }
                                }
                            }
                        }
                    }
                });
                let output = directory.path().join("data.bin");
                let started = Instant::now();
                if mode == "aria2" {
                    let result = tokio::time::timeout(
                        Duration::from_secs(120),
                        tokio::process::Command::new(&aria)
                            .args([
                                "--no-conf=true",
                                "--all-proxy=",
                                "--split=4",
                                "--max-connection-per-server=4",
                                "--min-split-size=4M",
                                "--file-allocation=none",
                                "--allow-overwrite=true",
                                "--auto-file-renaming=false",
                                "--summary-interval=0",
                                "--console-log-level=warn",
                                "--download-result=hide",
                                "--connect-timeout=30",
                                "--timeout=60",
                            ])
                            .arg(format!("--dir={}", directory.path().display()))
                            .args(if aria_diagnostic {
                                vec!["--log=target/tail-aria-internal.log", "--log-level=debug"]
                            } else {
                                vec![]
                            })
                            .arg("--out=data.bin")
                            .arg(&url)
                            .kill_on_drop(true)
                            .output(),
                    )
                    .await??;
                    if !result.status.success() {
                        return Err(format!(
                            "aria2 failed: {} {}",
                            String::from_utf8_lossy(&result.stdout),
                            String::from_utf8_lossy(&result.stderr)
                        )
                        .into());
                    }
                } else {
                    let policy = match mode {
                        "disabled" => SlowTransferMode::Disabled,
                        "adaptive" => SlowTransferMode::Adaptive,
                        _ => SlowTransferMode::AdaptiveWithHedging,
                    };
                    let spec = DownloadSpec::new(&url)
                        .output_path(&output)
                        .max_connections(4)
                        .min_split_size(4 * 1024 * 1024)
                        .resume(false)
                        .file_allocation(FileAllocation::None)
                        .slow_transfer_mode(policy);
                    tokio::time::timeout(
                        Duration::from_secs(120),
                        Downloader::builder()
                            .log_level(LogLevel::Info)
                            .build()?
                            .download(spec)
                            .wait(),
                    )
                    .await??;
                }
                let elapsed = started.elapsed().as_secs_f64();
                server.abort();
                let _ = server.await;
                let actual = tokio::fs::read(&output).await?;
                if actual.len() != SIZE
                    || actual
                        .iter()
                        .enumerate()
                        .any(|(i, b)| *b != (i % 251) as u8)
                {
                    return Err("output mismatch".into());
                }
                println!(
                    "{},{},{mode},{elapsed:.3},{},{},{}",
                    if slow { "tail" } else { "normal" },
                    round + 1,
                    stats.requests.load(Ordering::SeqCst),
                    stats.tails.load(Ordering::SeqCst),
                    actual.len()
                );
                tokio::fs::remove_file(&output).await?;
            }
        }
    }
    Ok(())
}
