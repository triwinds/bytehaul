//! Diagnostic download entry point for scripts/compare_public.py.
use bytehaul::{DownloadSpec, Downloader, FileAllocation, LogLevel, RangeSchedulingMode};
use std::{collections::HashMap, time::Duration};

fn usage_error(message: impl Into<String>) -> Box<dyn std::error::Error> {
    std::io::Error::new(std::io::ErrorKind::InvalidInput, message.into()).into()
}

fn usage() -> &'static str {
    "usage: public_compare URL OUTPUT CONNECTIONS [--range-scheduling-mode fixed|dynamic] \
     [--request-batch-size BYTES] [--dynamic-min-split-size BYTES] \
     [--dynamic-max-request-size BYTES] [--multi-ip] \
     [--connect-timeout-secs SEC] [--read-timeout-secs SEC] \
     [--headers-timeout-secs SEC] [--dns-server IP:PORT] \
     [--log-level off|error|warn|info|debug|trace]"
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).is_some_and(|arg| arg == "--help") {
        eprintln!("{}", usage());
        return Ok(());
    }
    if args.len() < 4 {
        return Err(usage_error(usage()));
    }

    let mut range_scheduling_mode = RangeSchedulingMode::Dynamic;
    let mut request_batch_size = None;
    let mut dynamic_min_split_size = None;
    let mut dynamic_max_request_size = None;
    let mut multi_ip = false;
    let mut dns_servers = Vec::new();
    let mut log_level = LogLevel::Off;
    let mut connect_timeout_secs = 15;
    let mut read_timeout_secs = 30;
    let mut headers_timeout_secs = 30;
    let mut index = 4;
    while index < args.len() {
        let flag = args[index].as_str();
        if flag == "--multi-ip" {
            multi_ip = true;
            index += 1;
            continue;
        }
        index += 1;
        let value = args
            .get(index)
            .ok_or_else(|| usage_error(format!("missing value for {flag}\n{}", usage())))?;
        match flag {
            "--range-scheduling-mode" => {
                range_scheduling_mode = value
                    .parse()
                    .map_err(|error| usage_error(format!("{error}\n{}", usage())))?;
            }
            "--request-batch-size" => request_batch_size = Some(value.parse()?),
            "--dynamic-min-split-size" => dynamic_min_split_size = Some(value.parse()?),
            "--dynamic-max-request-size" => dynamic_max_request_size = Some(value.parse()?),
            "--connect-timeout-secs" => connect_timeout_secs = value.parse()?,
            "--read-timeout-secs" => read_timeout_secs = value.parse()?,
            "--headers-timeout-secs" => headers_timeout_secs = value.parse()?,
            "--dns-server" => dns_servers.push(value.parse::<std::net::SocketAddr>()?),
            "--log-level" => {
                log_level = value
                    .parse()
                    .map_err(|error| usage_error(format!("{error}\n{}", usage())))?;
            }
            _ => return Err(usage_error(format!("unknown option: {flag}\n{}", usage()))),
        }
        index += 1;
    }

    tracing_subscriber::fmt()
        .with_max_level(log_level.to_tracing_level_filter())
        .with_ansi(false)
        .with_thread_ids(true)
        .with_writer(std::io::stderr)
        .init();

    let downloader = Downloader::builder()
        .enable_ipv6(false)
        .multi_ip(multi_ip)
        .dns_servers(dns_servers)
        .log_level(log_level)
        .build()?;
    let mut spec = DownloadSpec::new(&args[1])
        .output_path(&args[2])
        .max_connections(args[3].parse()?)
        .piece_size(1024 * 1024)
        .min_split_size(1024 * 1024)
        .file_allocation(FileAllocation::None)
        .connect_timeout(Duration::from_secs(connect_timeout_secs))
        .read_timeout(Duration::from_secs(read_timeout_secs))
        .request_headers_timeout(Duration::from_secs(headers_timeout_secs))
        .max_retries(2)
        .headers(HashMap::from([(
            "User-Agent".into(),
            "public-download-compare/1.0".into(),
        )]))
        .range_scheduling_mode(range_scheduling_mode);
    if let Some(bytes) = request_batch_size {
        spec = spec.request_batch_size(bytes);
    }
    if let Some(bytes) = dynamic_min_split_size {
        spec = spec.dynamic_min_split_size(bytes);
    }
    if let Some(bytes) = dynamic_max_request_size {
        spec = spec.dynamic_max_request_size(bytes);
    }
    spec.validate()?;
    let effective_batch = match range_scheduling_mode {
        RangeSchedulingMode::Fixed => spec.get_request_batch_size().to_string(),
        RangeSchedulingMode::Dynamic => "ignored (dynamic)".into(),
    };
    eprintln!(
        "EFFECTIVE_CONFIG multi_ip={} range_scheduling_mode={} request_batch_size_configured={} request_batch_size_effective={} dynamic_min_split_size_configured={} dynamic_min_split_size_effective={} dynamic_max_request_size_configured={} dynamic_max_request_size_effective={} max_request_leases={} connect_timeout_secs={} read_timeout_secs={} headers_timeout_secs={} log_level={}",
        multi_ip,
        spec.get_range_scheduling_mode(),
        spec.get_request_batch_size(),
        effective_batch,
        spec.get_dynamic_min_split_size(),
        spec.get_effective_dynamic_min_split_size(),
        spec.get_dynamic_max_request_size(),
        spec.get_effective_dynamic_max_request_size(),
        64,
        connect_timeout_secs,
        read_timeout_secs,
        headers_timeout_secs,
        log_level,
    );
    let handle = downloader.download(spec);
    let monitor = if log_level >= LogLevel::Info {
        let mut progress = handle.subscribe_progress();
        Some(tokio::spawn(async move {
            while progress.changed().await.is_ok() {
                eprintln!("PROGRESS {:?}", *progress.borrow_and_update());
            }
        }))
    } else {
        None
    };
    let result = handle.wait().await;
    if let Some(monitor) = monitor {
        monitor.abort();
    }
    result?;
    Ok(())
}
