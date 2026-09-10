//! Direct-network comparison: <url> <expected-sha256> [rounds].
//! Run with proxy environment variables cleared. Hashing is outside timing.
use bytehaul::{DownloadSpec, Downloader, FileAllocation};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    let url = args.get(1).ok_or("missing URL")?;
    let expected = args.get(2).ok_or("missing SHA-256")?;
    let rounds: usize = args.get(3).map_or(Ok(3), |s| s.parse())?;
    if rounds == 0 {
        return Err("rounds must be positive".into());
    }
    for key in [
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "http_proxy",
        "https_proxy",
        "all_proxy",
    ] {
        if std::env::var_os(key).is_some() {
            return Err(format!("clear {key} before direct-network measurement").into());
        }
    }
    let policies = [
        ("aria2", 0, 0),
        ("unpooled", 0, 0),
        ("pooled", 4, 0),
        ("batch4", 4, 4 << 20),
        ("batch8", 4, 8 << 20),
        ("batch16", 4, 16 << 20),
    ];
    println!("round,policy,seconds,bytes,sha256");
    for round in 0..rounds {
        for index in 0..policies.len() {
            let (name, pool, batch) = policies[(index + round) % policies.len()];
            let temp = tempfile::tempdir()?;
            let output = temp.path().join("download");
            let started = Instant::now();
            if name == "aria2" {
                let result = tokio::time::timeout(
                    Duration::from_secs(180),
                    tokio::process::Command::new("aria2c")
                        .args([
                            "--no-conf=true",
                            "--all-proxy=",
                            "--disable-ipv6=true",
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
                            "--user-agent=ns-emu-tools/0.6.3",
                        ])
                        .arg(format!("--dir={}", temp.path().display()))
                        .arg("--out=download")
                        .arg(url)
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
                let downloader = Downloader::builder().enable_ipv6(false).build()?;
                let spec = DownloadSpec::new(url)
                    .output_path(&output)
                    .max_connections(4)
                    .piece_size(1 << 20)
                    .min_split_size(4 << 20)
                    .request_batch_size(batch)
                    .http_idle_pool(pool, Duration::from_secs(30))
                    .headers(HashMap::from([(
                        "User-Agent".into(),
                        "ns-emu-tools/0.6.3".into(),
                    )]))
                    .resume(false)
                    .file_allocation(FileAllocation::None);
                tokio::time::timeout(Duration::from_secs(180), downloader.download(spec).wait())
                    .await??;
            }
            let seconds = started.elapsed().as_secs_f64();
            let bytes = tokio::fs::read(output).await?;
            let hash: String = Sha256::digest(&bytes)
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            if !hash.eq_ignore_ascii_case(expected) {
                return Err(format!("{name}: SHA-256 mismatch: {hash}").into());
            }
            println!("{},{name},{seconds:.3},{},{hash}", round + 1, bytes.len());
        }
    }
    Ok(())
}
