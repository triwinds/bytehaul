//! Controlled HTTP/1.1 pooling, batching and interrupted-body comparison.
//! cargo run --example http_efficiency_compare -- [rounds] [aria2c-path] [slow-connect|slow-connect-paced]
//! CSV on stdout; exact-byte checks are outside the measured interval.
//! Delays model connection setup / response latency, not a real WAN.
use bytehaul::{DownloadSpec, Downloader, FileAllocation};
use std::{
    io,
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
    task::JoinSet,
};

const PIECE: usize = 1024 * 1024;
const SIZE: usize = 16 * PIECE;
const CUT: usize = SIZE / 2 + PIECE / 2;

#[derive(Clone, Copy)]
struct Scenario {
    name: &'static str,
    setup_ms: u64,
    // Zero delays only the first accepted socket; N delays every Nth socket.
    setup_every: usize,
    body_chunk_ms: u64,
    response_ms: u64,
    interrupt: bool,
    strong_etag: bool,
    close: bool,
}

#[derive(Default)]
struct Stats {
    connections: AtomicUsize,
    requests: AtomicUsize,
    suffixes: AtomicUsize,
    body_bytes: AtomicU64,
    interrupted: AtomicBool,
}

async fn serve(
    stream: TcpStream,
    body: Arc<Vec<u8>>,
    stats: Arc<Stats>,
    scenario: Scenario,
    connection_id: usize,
) -> io::Result<()> {
    stream.set_nodelay(true)?;
    if (scenario.setup_every == 0 && connection_id == 1)
        || (scenario.setup_every > 0 && connection_id.is_multiple_of(scenario.setup_every))
    {
        tokio::time::sleep(Duration::from_millis(scenario.setup_ms)).await;
    }
    let mut reader = BufReader::new(stream);
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).await? == 0 {
            return Ok(());
        }
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
        let (start, end) = match range.as_deref() {
            Some(value) => {
                let (a, b) = value
                    .strip_prefix("bytes=")
                    .and_then(|v| v.split_once('-'))
                    .ok_or_else(|| io::Error::other("bad Range"))?;
                (
                    a.parse::<usize>().map_err(io::Error::other)?,
                    if b.is_empty() {
                        SIZE - 1
                    } else {
                        b.parse::<usize>().map_err(io::Error::other)?.min(SIZE - 1)
                    },
                )
            }
            None => (0, SIZE - 1),
        };
        if start > end {
            return Err(io::Error::other("invalid Range"));
        }
        stats.requests.fetch_add(1, Ordering::SeqCst);
        if start % PIECE != 0 {
            stats.suffixes.fetch_add(1, Ordering::SeqCst);
        }
        tokio::time::sleep(Duration::from_millis(scenario.response_ms)).await;
        let status = if range.is_some() {
            "206 Partial Content"
        } else {
            "200 OK"
        };
        let range_header = if range.is_some() {
            format!("Content-Range: bytes {start}-{end}/{SIZE}\r\n")
        } else {
            String::new()
        };
        let etag = if scenario.strong_etag {
            "\"fixture-v1\""
        } else {
            "W/\"fixture-v1\""
        };
        let connection = if scenario.close {
            "close"
        } else {
            "keep-alive"
        };
        reader.get_mut().write_all(format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\n{range_header}ETag: {etag}\r\nAccept-Ranges: bytes\r\nConnection: {connection}\r\n\r\n",
            end - start + 1,
        ).as_bytes()).await?;
        if !head {
            let cut = scenario.interrupt
                && start < CUT
                && end >= CUT
                && !stats.interrupted.swap(true, Ordering::SeqCst);
            let stop = if cut { CUT } else { end + 1 };
            for chunk in body[start..stop].chunks(64 * 1024) {
                if scenario.body_chunk_ms > 0 {
                    tokio::time::sleep(Duration::from_millis(scenario.body_chunk_ms)).await;
                }
                reader.get_mut().write_all(chunk).await?;
                stats
                    .body_bytes
                    .fetch_add(chunk.len() as u64, Ordering::SeqCst);
            }
            if cut {
                reader.get_mut().shutdown().await?;
                return Ok(());
            }
        }
        if scenario.close {
            return Ok(());
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let rounds = std::env::args()
        .nth(1)
        .map_or(Ok(3), |s| s.parse::<usize>())?;
    if rounds == 0 {
        return Err("rounds must be positive".into());
    }
    let aria = std::env::args().nth(2);
    let filter = std::env::args().nth(3);
    if filter
        .as_deref()
        .is_some_and(|s| !matches!(s, "slow-connect" | "slow-connect-paced"))
    {
        return Err("unknown scenario filter".into());
    }
    let body = Arc::new(
        (0..SIZE)
            .map(|i| ((i * 31 + i / 257) % 251) as u8)
            .collect::<Vec<_>>(),
    );
    let base = Scenario {
        name: "local",
        setup_ms: 0,
        setup_every: 1,
        body_chunk_ms: if filter.as_deref() == Some("slow-connect-paced") {
            5
        } else {
            0
        },
        response_ms: 0,
        interrupt: false,
        strong_etag: true,
        close: false,
    };
    let mut scenarios = vec![
        base,
        Scenario {
            name: "setup100_response25",
            setup_ms: 100,
            response_ms: 25,
            ..base
        },
        Scenario {
            name: "response100",
            response_ms: 100,
            ..base
        },
        Scenario {
            name: "close_setup100",
            setup_ms: 100,
            close: true,
            ..base
        },
        Scenario {
            name: "disconnect_weak",
            response_ms: 25,
            interrupt: true,
            strong_etag: false,
            ..base
        },
        Scenario {
            name: "disconnect_strong",
            response_ms: 25,
            interrupt: true,
            ..base
        },
    ];
    if filter.is_some() {
        scenarios = vec![
            base,
            Scenario {
                name: "all_connections_300ms",
                setup_ms: 300,
                ..base
            },
            Scenario {
                name: "every_fourth_connection_1500ms",
                setup_ms: 1500,
                setup_every: 4,
                ..base
            },
            Scenario {
                name: "first_connection_1500ms",
                setup_ms: 1500,
                setup_every: 0,
                ..base
            },
        ];
    }
    let mut policies = vec![
        ("unpooled", 0, 0),
        ("pooled", 4, 0),
        ("batched", 0, 4 * PIECE as u64),
        ("pooled_batched", 4, 4 * PIECE as u64),
        ("pooled_batch8", 4, 8 * PIECE as u64),
        ("pooled_batch16", 4, 16 * PIECE as u64),
    ];
    if filter.is_some() {
        policies.truncate(4);
    }
    if aria.is_some() {
        policies.push(("aria2", 0, 0));
    }
    println!("scenario,round,policy,seconds,connections,requests,unaligned_range_requests,server_body_bytes,validated_bytes");
    for scenario in scenarios {
        for round in 0..rounds {
            for index in 0..policies.len() {
                let (name, pool, batch) = policies[(index + round) % policies.len()];
                let listener = TcpListener::bind("127.0.0.1:0").await?;
                let address = listener.local_addr()?;
                let stats = Arc::new(Stats::default());
                let server_stats = stats.clone();
                let server_body = body.clone();
                let server = tokio::spawn(async move {
                    let mut clients = JoinSet::new();
                    loop {
                        tokio::select! {
                            result = listener.accept() => {
                                let (stream, _) = result?;
                                let connection_id = server_stats.connections.fetch_add(1, Ordering::SeqCst) + 1;
                                let body = server_body.clone();
                                let stats = server_stats.clone();
                                clients.spawn(async move { let _ = serve(stream, body, stats, scenario, connection_id).await; });
                            }
                            _ = clients.join_next(), if !clients.is_empty() => {}
                        }
                    }
                    #[allow(unreachable_code)]
                    Ok::<(), io::Error>(())
                });
                let temp = tempfile::tempdir()?;
                let output = temp.path().join("output.bin");
                if name == "aria2" {
                    let started = Instant::now();
                    let result = tokio::time::timeout(
                        Duration::from_secs(60),
                        tokio::process::Command::new(aria.as_ref().expect("aria2 path"))
                            .args([
                                "--no-conf=true",
                                "--all-proxy=",
                                "--split=4",
                                "--max-connection-per-server=4",
                                "--min-split-size=1M",
                                "--file-allocation=none",
                                "--allow-overwrite=true",
                                "--auto-file-renaming=false",
                                "--summary-interval=0",
                                "--console-log-level=warn",
                                "--download-result=hide",
                                "--max-tries=3",
                                "--retry-wait=0",
                            ])
                            .arg(format!("--dir={}", temp.path().display()))
                            .arg("--out=output.bin")
                            .arg(format!("http://{address}/file"))
                            .kill_on_drop(true)
                            .output(),
                    )
                    .await;
                    let seconds = started.elapsed().as_secs_f64();
                    server.abort();
                    let _ = server.await;
                    let result = result??;
                    if !result.status.success() {
                        return Err(format!(
                            "aria2 failed: {} {}",
                            String::from_utf8_lossy(&result.stdout),
                            String::from_utf8_lossy(&result.stderr)
                        )
                        .into());
                    }
                    let actual = tokio::fs::read(&output).await?;
                    assert_eq!(actual, *body, "aria2 {} output differs", scenario.name);
                    assert_eq!(stats.interrupted.load(Ordering::SeqCst), scenario.interrupt);
                    println!(
                        "{},{},{},{:.3},{},{},{},{},{}",
                        scenario.name,
                        round + 1,
                        name,
                        seconds,
                        stats.connections.load(Ordering::SeqCst),
                        stats.requests.load(Ordering::SeqCst),
                        stats.suffixes.load(Ordering::SeqCst),
                        stats.body_bytes.load(Ordering::SeqCst),
                        actual.len()
                    );
                    continue;
                }
                let downloader = Downloader::builder().build()?;
                let spec = DownloadSpec::new(format!("http://{address}/file"))
                    .output_path(&output)
                    .max_connections(4)
                    .min_split_size(1)
                    .piece_size(PIECE as u64)
                    .request_batch_size(batch)
                    .http_idle_pool(pool, Duration::from_secs(30))
                    .file_allocation(FileAllocation::None)
                    .resume(false)
                    .retry_policy(2, Duration::from_millis(1), Duration::from_millis(5));
                let started = Instant::now();
                let result =
                    tokio::time::timeout(Duration::from_secs(60), downloader.download(spec).wait())
                        .await;
                let seconds = started.elapsed().as_secs_f64();
                server.abort();
                let _ = server.await;
                result??;
                let actual = tokio::fs::read(&output).await?;
                assert_eq!(actual, *body, "{name} {} output differs", scenario.name);
                assert_eq!(stats.interrupted.load(Ordering::SeqCst), scenario.interrupt);
                println!(
                    "{},{},{},{:.3},{},{},{},{},{}",
                    scenario.name,
                    round + 1,
                    name,
                    seconds,
                    stats.connections.load(Ordering::SeqCst),
                    stats.requests.load(Ordering::SeqCst),
                    stats.suffixes.load(Ordering::SeqCst),
                    stats.body_bytes.load(Ordering::SeqCst),
                    actual.len(),
                );
            }
        }
    }
    Ok(())
}
