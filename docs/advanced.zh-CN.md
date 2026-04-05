# bytehaul — 进阶用法（Rust）

本文档介绍 Rust API 的进阶配置、进度监控、取消下载和网络设置。

基础用法请参阅[主 README](../README.md)。
[English Version](advanced.md)

## 配置示例

```rust
use std::time::Duration;
use bytehaul::{Checksum, DownloadSpec, FileAllocation};

let spec = DownloadSpec::new("https://example.com/file.bin")
    .output_dir("downloads")
    .output_path("file.bin")
    .max_connections(8) // 并发连接数
    .piece_size(2 * 1024 * 1024) // 分片大小：2 MiB
    .min_split_size(10 * 1024 * 1024) // 文件大于 10 MiB 时才拆分
    .file_allocation(FileAllocation::Prealloc)
    .resume(true)
    .retry_policy(5, Duration::from_secs(1), Duration::from_secs(30))
    .max_retry_elapsed(Duration::from_secs(120)) // 总重试预算 2 分钟
    .max_download_speed(1024 * 1024) // 限速 1 MB/s
    .checksum(Checksum::Sha256(
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855".into(),
    ));
```

现在 bytehaul 会在真正发起网络请求前通过 `DownloadSpec::validate()` 统一校验这些任务级配置，避免把约束分散到各个运行时分支里兜底。

`max_retries` 仍然表示最多重试多少次；`max_retry_elapsed` 则补充了“总共最多重试多久”的时间预算。如果继续退避会超出这个预算，请求会以 `DownloadError::RetryBudgetExceeded` 结束，而不是只看次数上限。

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

`speed_bytes_per_sec` 和 `eta_secs` 现在共用同一条最近吞吐窗口：

- `speed_bytes_per_sec` 表示最近窗口内的速度，而不是从下载开始到当前时刻的全程平均值。
- `eta_secs` 直接使用同一窗口速度估算剩余时间，因此它和显示出来的速度会一起变化，而不是各走一套平滑规则。
- `eta_secs == None`：当前最近样本还不足以给出稳定 ETA，或者总大小仍未知。
- `eta_secs == Some(0.0)`：下载已经到达流末尾，状态即将或已经进入 `Completed`。

## 暂停与续传

```rust
let handle = downloader.download(spec.clone());
handle.pause();

match handle.wait().await {
    Err(bytehaul::DownloadError::Paused) => {
        let resumed = downloader.download(spec);
        resumed.wait().await?;
    }
    other => other?,
}
```

pause 不是把同一个 handle 原地挂起后再继续，而是结束当前任务、刷盘并写出控制文件。真正的恢复动作是后续再发起一次新的 `download(spec)` 调用，并且解析出的输出路径必须保持一致。

在信任续传状态前，bytehaul 现在会同时检查：

- 远端元数据是否仍与控制快照一致。
- 本地输出文件是否仍与控制快照记录的进度一致。

任一检查失败时，bytehaul 会丢弃旧控制文件并重新开始下载。

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

`Cancelled`、`Paused`、`Completed` 是三个不同的结束语义：`cancel()` 放弃任务，`pause()` 保留可恢复状态，正常完成则删除控制文件并返回 `Ok(())`。
