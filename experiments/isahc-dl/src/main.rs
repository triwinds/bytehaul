//! isahc-dl — minimal multi-connection parallel Range-download experiment.
//!
//! Purpose: benchmark isahc 1.8 (async Rust wrapper around libcurl) throughput
//! vs the aria2 / libcurl baseline established in performance-investigation §9.
//! All received bytes are discarded (no disk I/O).
//!
//! isahc is built on top of libcurl, so it inherits libcurl's socket-level
//! behaviour by default.  Key settings mirrored from the §9 curl-crate experiment:
//!   - HTTP/1.1 forced per request (avoids H2 connection merging)
//!   - Accept-Encoding: identity (no decompression overhead)
//!   - Each range worker uses an *independent* HttpClient instance so there is
//!     no shared connection-pool re-use between workers, matching the §9 setup
//!
//! Usage:
//!   cargo run --release -- <URL> [connections]
//!   # connections defaults to 8

use std::io::Read;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use isahc::config::{RedirectPolicy, VersionNegotiation};
use isahc::prelude::*;
use isahc::{HttpClient, Request};
use tokio::task::JoinSet;

fn build_client() -> HttpClient {
    HttpClient::builder()
        // Force HTTP/1.1 — each worker keeps its own connection, mirrors curl §9.
        .version_negotiation(VersionNegotiation::http11())
        // Follow redirects (GitHub release URLs redirect to CDN).
        .redirect_policy(RedirectPolicy::Follow)
        // 30 s connect timeout, matching curl's CURLOPT_CONNECTTIMEOUT default.
        .connect_timeout(std::time::Duration::from_secs(30))
        // Disable connection reuse between workers (pool size = 1 per client).
        // isahc reuses connections inside a single HttpClient by default; giving
        // each worker its own client avoids that.
        .connection_cache_size(1)
        .build()
        .expect("failed to build isahc client")
}

/// Issue a HEAD request and return `Content-Length`.
/// GitHub release URLs redirect to release-assets.githubusercontent.com;
/// we follow redirects and read Content-Length from the final response.
fn get_content_length(url: &str) -> (u64, String) {
    // Use a GET + immediate disconnect instead of HEAD, because some CDNs
    // return Content-Length on GET but not on HEAD after redirect.
    // Actually use a HEAD-following client to get the final URL + size.
    let client = HttpClient::builder()
        .version_negotiation(VersionNegotiation::http11())
        .redirect_policy(RedirectPolicy::Follow)
        .connect_timeout(std::time::Duration::from_secs(30))
        .build()
        .expect("probe client");
    let req = Request::head(url)
        .body(())
        .expect("HEAD request");
    let resp = client.send(req).expect("HEAD request failed");
    let final_url = resp.effective_uri()
        .map(|u| u.to_string())
        .unwrap_or_else(|| url.to_string());
    let len = resp.headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
        .expect("no Content-Length in HEAD response");
    (len, final_url)
}

/// Download byte range [start, end] (inclusive), discarding data.
/// Returns bytes received.
fn download_range(client: &HttpClient, url: &str, start: u64, end: u64) -> u64 {
    let range_hdr = format!("bytes={start}-{end}");
    let req = Request::get(url)
        .header("Range", &range_hdr)
        .header("Accept-Encoding", "identity")
        .header("User-Agent", "isahc-dl-experiment/0.1")
        .body(())
        .expect("request build");

    let mut resp = client.send(req).expect("range request failed");
    let status = resp.status();
    assert_eq!(
        status.as_u16(),
        206,
        "expected 206 Partial Content for range {range_hdr}, got {status}"
    );

    let mut buf = vec![0u8; 64 * 1024];
    let mut total: u64 = 0;
    loop {
        let n = resp.body_mut().read(&mut buf).expect("read body");
        if n == 0 {
            break;
        }
        total += n as u64;
    }
    total
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: isahc-dl <URL> [connections]");
        std::process::exit(1);
    }
    let url = args[1].clone();
    let connections: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(8);

    println!("Probing {url} with HEAD …");
    let (total_size, final_url) = get_content_length(&url);
    if final_url != url {
        println!("Redirected to: {final_url}");
    }
    println!(
        "File size: {:.2} MiB ({total_size} bytes), connections: {connections}",
        total_size as f64 / (1024.0 * 1024.0)
    );

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
        let u = final_url.clone();
        let counter = total_downloaded.clone();
        // isahc's send() is blocking (it uses curl's multi-handle internally
        // with a background thread), so we run each worker on a Tokio spawn_blocking
        // thread to avoid starving the async executor.
        set.spawn(tokio::task::spawn_blocking(move || {
            let client = build_client();
            let n = download_range(&client, &u, seg_start, seg_end);
            counter.fetch_add(n, Ordering::Relaxed);
        }));
    }

    while let Some(res) = set.join_next().await {
        res.expect("worker task failed").expect("worker panicked");
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
