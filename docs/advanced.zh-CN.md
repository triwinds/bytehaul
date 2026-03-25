# bytehaul — 进阶用法（Rust）

本文档介绍 Rust API 的进阶配置、进度监控、取消下载和网络设置。

基础用法请参阅[主 README](../README.md)。
[English Version](advanced.md)

## 配置示例

```rust
use std::time::Duration;
use bytehaul::{Checksum, DownloadSpec, FileAllocation};

let mut spec = DownloadSpec::new("https://example.com/file.bin")
    .output_dir("downloads")
    .output_path("file.bin");
spec.max_connections = 8;                // 并发连接数
spec.piece_size = 2 * 1024 * 1024;       // 分片大小：2 MiB
spec.min_split_size = 10 * 1024 * 1024;  // 文件大于 10 MiB 时才拆分
spec.file_allocation = FileAllocation::Prealloc;
spec.resume = true;                      // 启用断点续传
spec.max_retries = 5;
spec.retry_base_delay = Duration::from_secs(1);
spec.max_download_speed = 1024 * 1024;   // 限速 1 MB/s
spec.checksum = Some(Checksum::Sha256(
    "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855".into(),
));
```

如果省略 `.output_path(...)`，bytehaul 会依次按 `Content-Disposition`、URL 路径最后一段、默认名 `download` 自动选择文件名。若未设置 `.output_dir(...)`，仍可继续直接传绝对输出路径。

## 网络层配置

网络相关配置位于共享的下载器客户端上：

```rust
use std::net::SocketAddr;
use bytehaul::Downloader;

let downloader = Downloader::builder()
    .all_proxy("http://127.0.0.1:7890")
    .dns_servers([
        SocketAddr::from(([1, 1, 1, 1], 53)),
        SocketAddr::from(([8, 8, 8, 8], 53)),
    ])
    .enable_ipv6(false)
    .build()?;
```

`DownloadSpec::connect_timeout` 仍然可用；如果单个任务覆盖了它，bytehaul 会为该任务临时构建一个等价 client。

## 进度监控

```rust
use bytehaul::{DownloadSpec, DownloadState, Downloader};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dl = Downloader::builder().build()?;
    let handle = dl.download(
        DownloadSpec::new("https://example.com/file.bin").output_path("file.bin")
    );

    let mut rx = handle.subscribe_progress();
    tokio::spawn(async move {
        while rx.changed().await.is_ok() {
            let snap = rx.borrow().clone();
            println!(
                "state={:?} downloaded={} speed={:.0} B/s eta={:?}",
                snap.state, snap.downloaded, snap.speed_bytes_per_sec, snap.eta_secs
            );
        }
    });

    handle.wait().await?;
    Ok(())
}
```

## 日志

bytehaul 内部使用 `tracing`。日志**默认关闭**，可通过 builder 设置日志级别开启：

```rust
use bytehaul::{Downloader, LogLevel};

let downloader = Downloader::builder()
    .log_level(LogLevel::Debug)
    .build()?;
```

可选级别（由少到多）：`Off`（默认）、`Error`、`Warn`、`Info`、`Debug`、`Trace`。

要在终端看到日志输出，还需要在应用中初始化 `tracing-subscriber`：

```rust
tracing_subscriber::fmt::init();

let downloader = Downloader::builder()
    .log_level(LogLevel::Info)
    .build()?;
```

## 取消下载

```rust
let handle = downloader.download(spec);
// 可以在其他任务中，或超时后触发取消
handle.cancel();
let result = handle.wait().await; // 返回 Err(DownloadError::Cancelled)
```
