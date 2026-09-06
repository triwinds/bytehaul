//! Local diagnostic: `cargo run --release --example pool_compare -- [rounds] [request_delay_ms]`.
//! Unset HTTP_PROXY, HTTPS_PROXY, ALL_PROXY and their lowercase variants first.
//! Compares real downloads with 32 MiB of deterministic data, four workers and
//! 1 MiB pieces. The server delays every request equally and counts accepted
//! TCP connections. This does not model WAN latency, TLS or proxy reliability.

use std::io;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytehaul::{DownloadSpec, Downloader, FileAllocation};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinSet;

const WORKERS: u32 = 4;
const PIECE_SIZE: u64 = 1024 * 1024;

async fn serve_connection(
    socket: TcpStream,
    data: Arc<Vec<u8>>,
    requests: Arc<AtomicUsize>,
    delay: Duration,
) -> io::Result<()> {
    socket.set_nodelay(true)?;
    let (read, mut write) = socket.into_split();
    let mut reader = BufReader::new(read);
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).await? == 0 {
            return Ok(());
        }
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
        let (start, end) = match range.as_deref() {
            Some(value) => {
                let bounds = value
                    .strip_prefix("bytes=")
                    .and_then(|value| value.split_once('-'))
                    .ok_or_else(|| io::Error::other("invalid range"))?;
                let start = bounds.0.parse::<usize>().map_err(io::Error::other)?;
                let end = bounds.1.parse::<usize>().map_err(io::Error::other)?;
                if start > end || end >= data.len() {
                    return Err(io::Error::other("range outside test data"));
                }
                (start, end)
            }
            None => (0, data.len() - 1),
        };
        requests.fetch_add(1, Ordering::SeqCst);
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        let status = if range.is_some() {
            "206 Partial Content"
        } else {
            "200 OK"
        };
        let mut headers = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nConnection: keep-alive\r\n",
            end - start + 1,
        );
        if range.is_some() {
            headers.push_str(&format!(
                "Content-Range: bytes {start}-{end}/{}\r\n",
                data.len()
            ));
        }
        headers.push_str("\r\n");
        write.write_all(headers.as_bytes()).await?;
        write.write_all(&data[start..=end]).await?;
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    let rounds = args
        .get(1)
        .map(|value| value.parse())
        .transpose()?
        .unwrap_or(3);
    let delay_ms = args
        .get(2)
        .map(|value| value.parse())
        .transpose()?
        .unwrap_or(2);
    let delay = Duration::from_millis(delay_ms);
    let data: Arc<Vec<u8>> = Arc::new((0..32 * 1024 * 1024).map(|i| (i % 251) as u8).collect());
    let output = tempfile::tempdir()?;
    println!("32 MiB, {WORKERS} workers, 1 MiB pieces, {delay_ms} ms delay/request");
    println!("round,pool_max_idle,elapsed_ms,requests,accepted_tcp,verified_bytes");

    for round in 1..=rounds {
        // Alternate order to reduce systematic warm-machine bias.
        let modes = if round % 2 == 1 { [0, 4] } else { [4, 0] };
        for pool_size in modes {
            let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
            let addr = listener.local_addr()?;
            let accepted = Arc::new(AtomicUsize::new(0));
            let requests = Arc::new(AtomicUsize::new(0));
            let (server_data, server_accepted, server_requests) =
                (data.clone(), accepted.clone(), requests.clone());
            let server = tokio::spawn(async move {
                let mut connections = JoinSet::new();
                loop {
                    tokio::select! {
                        socket = listener.accept() => {
                            let (socket, _) = socket?;
                            server_accepted.fetch_add(1, Ordering::SeqCst);
                            connections.spawn(serve_connection(
                                socket, server_data.clone(), server_requests.clone(), delay,
                            ));
                        }
                        Some(result) = connections.join_next() => {
                            // A range probe may drop its body and reset the connection.
                            if let Err(error) = result? {
                                if !matches!(error.kind(), io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset) {
                                    return Err::<(), io::Error>(error);
                                }
                            }
                        }
                    }
                }
            });
            let path = output.path().join(format!("{round}-{pool_size}.bin"));
            let downloader = Downloader::builder().build()?;
            let spec = DownloadSpec::new(format!("http://{addr}/data"))
                .output_path(&path)
                .max_connections(WORKERS)
                .piece_size(PIECE_SIZE)
                .http_idle_pool(pool_size, Duration::from_secs(30))
                .resume(false)
                .file_allocation(FileAllocation::None);
            let start = Instant::now();
            tokio::time::timeout(Duration::from_secs(30), downloader.download(spec).wait())
                .await??;
            let elapsed = start.elapsed();
            let actual = tokio::fs::read(&path).await?;
            if actual != *data {
                return Err(io::Error::other("downloaded bytes differ from source").into());
            }
            println!(
                "{round},{pool_size},{:.3},{},{},{}",
                elapsed.as_secs_f64() * 1000.0,
                requests.load(Ordering::SeqCst),
                accepted.load(Ordering::SeqCst),
                actual.len(),
            );
            server.abort(); // Dropping JoinSet also aborts all accepted connections.
            let _ = server.await;
            tokio::fs::remove_file(path).await?;
        }
    }
    Ok(())
}
