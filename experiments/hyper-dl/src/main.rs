//! hyper-dl — minimal multi-connection parallel Range-download experiment.
//!
//! Purpose: benchmark raw hyper 1.x throughput vs the aria2 / libcurl baseline
//! established in the performance-investigation doc (§9).  All received bytes are
//! discarded (no disk I/O) so the result reflects pure HTTP-stack throughput.
//!
//! Socket tuning mirrors curl's defaults:
//!   - TCP_NODELAY = true   (curl sets CURLOPT_TCP_NODELAY=1 by default since 7.63)
//!   - SO_RCVBUF  = 512 KiB (curl's CURL_MAX_WRITE_SIZE × default buffer)
//!   - Each range worker gets its *own* independent hyper connection
//!     (pool_max_idle_per_host=0, no idle keep-alive reuse between workers)
//!   - HTTP/1.1 forced — avoids H2 multiplexing which would collapse workers
//!     onto a single TCP connection
//!
//! Usage:
//!   cargo run --release -- <URL> [connections]
//!   # connections defaults to 8

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use http_body_util::{BodyExt, Empty};
use hyper::body::Bytes;
use hyper::Request;
use hyper_rustls::HttpsConnectorBuilder;
use hyper_util::client::legacy::{connect::HttpConnector, Client};
use hyper_util::rt::TokioExecutor;
use tokio::task::JoinSet;

// curl default: CURLOPT_BUFFERSIZE = 16 KiB, but effective TCP recv buffer is
// typically 64–512 KiB set via SO_RCVBUF.  We choose 512 KiB to match the
// value curl uses when the server sends large objects.
const RECV_BUF_BYTES: usize = 512 * 1024;

type HttpsClient = Client<hyper_rustls::HttpsConnector<HttpConnector>, Empty<Bytes>>;

fn build_client() -> HttpsClient {
    let mut http = HttpConnector::new();

    // Mirror curl's CURLOPT_TCP_NODELAY (true by default since curl 7.63.0).
    http.set_nodelay(true);

    // Mirror curl's aggressive receive-buffer sizing.
    http.set_recv_buffer_size(Some(RECV_BUF_BYTES));

    // Connect timeout = 30 s (curl default CURLOPT_CONNECTTIMEOUT).
    http.set_connect_timeout(Some(std::time::Duration::from_secs(30)));

    // Use native OS certificate roots (same trust store as curl on most systems).
    let tls = HttpsConnectorBuilder::new()
        .with_native_roots()
        .expect("native roots")
        .https_or_http()
        .enable_http1()
        .wrap_connector(http);

    // Disable the shared idle-connection pool so every range worker gets its
    // own fresh TCP connection — identical to what curl does with N easy handles.
    Client::builder(TokioExecutor::new())
        .pool_max_idle_per_host(0)
        .build(tls)
}

/// Issue a HEAD request and return the `Content-Length` in bytes.
async fn get_content_length(client: &HttpsClient, url: &str) -> u64 {
    let req = Request::builder()
        .method("HEAD")
        .uri(url)
        .header("User-Agent", "hyper-dl-experiment/0.1")
        .body(Empty::<Bytes>::new())
        .expect("HEAD request");

    let resp = client.request(req).await.expect("HEAD failed");
    let status = resp.status();
    assert!(
        status.is_success() || status.as_u16() == 206,
        "HEAD returned {status}"
    );

    resp.headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
        .expect("no Content-Length in HEAD response")
}

/// Download byte range [start, end] (inclusive), counting bytes and discarding
/// data.  Returns the number of bytes received.
async fn download_range(
    client: HttpsClient,
    url: String,
    start: u64,
    end: u64,
) -> u64 {
    let range_hdr = format!("bytes={start}-{end}");
    let req = Request::builder()
        .method("GET")
        .uri(&url)
        // Force HTTP/1.1 so this worker uses a dedicated TCP connection.
        .version(hyper::Version::HTTP_11)
        .header("Range", &range_hdr)
        .header("Accept-Encoding", "identity")
        .header("User-Agent", "hyper-dl-experiment/0.1")
        .body(Empty::<Bytes>::new())
        .expect("range GET request");

    let resp = client.request(req).await.expect("range request failed");
    let status = resp.status();
    assert_eq!(
        status.as_u16(),
        206,
        "expected 206 Partial Content for range {range_hdr}, got {status}"
    );

    let mut body = resp.into_body();
    let mut received: u64 = 0;
    while let Some(frame) = body.frame().await {
        if let Ok(chunk) = frame.expect("body frame error").into_data() {
            received += chunk.len() as u64;
        }
    }
    received
}

fn resolve_first(host: &str, port: u16) -> SocketAddr {
    use std::net::ToSocketAddrs;
    format!("{host}:{port}")
        .to_socket_addrs()
        .expect("DNS lookup failed")
        .next()
        .expect("no address resolved")
}

fn parse_host_port(url: &str) -> (String, u16) {
    let is_http = url.starts_with("http://");
    let stripped = url.trim_start_matches("https://").trim_start_matches("http://");
    let host_path = stripped.split('/').next().unwrap_or(stripped);
    if let Some((h, p)) = host_path.split_once(':') {
        (h.to_string(), p.parse().unwrap_or(443))
    } else if is_http {
        (host_path.to_string(), 80)
    } else {
        (host_path.to_string(), 443)
    }
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: hyper-dl <URL> [connections]");
        std::process::exit(1);
    }
    let url = args[1].clone();
    let connections: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(8);

    let client = build_client();

    println!("Probing {url} with HEAD …");
    let total_size = get_content_length(&client, &url).await;
    println!(
        "File size: {:.2} MiB ({total_size} bytes), connections: {connections}",
        total_size as f64 / (1024.0 * 1024.0)
    );

    // Pre-resolve DNS so connection latency below is just TCP+TLS.
    let (host, port) = parse_host_port(&url);
    let _addr = resolve_first(&host, port); // warm DNS cache

    let chunk = total_size / connections;
    let total_downloaded = Arc::new(AtomicU64::new(0));
    let mut set = JoinSet::new();

    let start = Instant::now();
    for i in 0..connections {
        let seg_start = i * chunk;
        let seg_end = if i == connections - 1 {
            total_size - 1
        } else {
            (i + 1) * chunk - 1
        };
        let c = build_client(); // fresh client = fresh TCP connection
        let u = url.clone();
        let counter = total_downloaded.clone();
        set.spawn(async move {
            let n = download_range(c, u, seg_start, seg_end).await;
            counter.fetch_add(n, Ordering::Relaxed);
        });
    }

    while let Some(res) = set.join_next().await {
        res.expect("worker panicked");
    }

    let elapsed = start.elapsed().as_secs_f64();
    let total = total_downloaded.load(Ordering::Relaxed);
    let mib_s = (total as f64 / (1024.0 * 1024.0)) / elapsed;
    println!(
        "Downloaded {:.2} MiB in {:.3} s  →  {mib_s:.1} MiB/s",
        total as f64 / (1024.0 * 1024.0),
        elapsed,
    );
}
