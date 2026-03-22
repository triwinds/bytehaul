# bytehaul

Rust 异步 HTTP 下载库，支持断点续传、多连接并发、回写缓存、限速和校验。

## 文档

- [English README](../README.md)
- [Python 使用文档](python.zh-CN.md)

## 功能特性

- **单连接与多连接下载**：自动探测是否支持 `Range`，并在必要时回退
- **断点续传**：通过原子化状态保存控制下载进度持久化
- **回写缓存**：基于分片聚合写入，减少随机 I/O
- **内存预算与背压控制**：基于信号量限制内存占用与生产速度
- **指数退避重试**：支持配置最大重试次数，并尊重 `Retry-After`
- **下载限速**：所有工作线程共享令牌桶限速
- **SHA-256 校验**：下载完成后进行完整性校验
- **取消下载**：通过 watch channel 协作取消
- **进度订阅**：实时获取速度、已下载字节数与状态

## 快速开始

```rust
use bytehaul::{DownloadSpec, Downloader};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let downloader = Downloader::builder().build()?;

    let spec = DownloadSpec::new(
        "https://example.com/largefile.zip",
        "largefile.zip",
    );

    let handle = downloader.download(spec);
    handle.wait().await?;
    println!("下载完成！");
    Ok(())
}
```

## 配置示例

```rust
use std::time::Duration;
use bytehaul::{Checksum, DownloadSpec, FileAllocation};

let mut spec = DownloadSpec::new("https://example.com/file.bin", "file.bin");
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

## 进度监控

```rust
use bytehaul::{DownloadSpec, Downloader};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dl = Downloader::builder().build()?;
    let handle = dl.download(DownloadSpec::new(
        "https://example.com/file.bin", "file.bin"
    ));

    let mut rx = handle.subscribe_progress();
    tokio::spawn(async move {
        while rx.changed().await.is_ok() {
            let snap = rx.borrow().clone();
            println!(
                "state={:?} downloaded={} speed={:.0} B/s",
                snap.state, snap.downloaded, snap.speed_bytes_per_sec
            );
        }
    });

    handle.wait().await?;
    Ok(())
}
```

## 取消下载

```rust
let handle = downloader.download(spec);
// 可以在其他任务中，或超时后触发取消
handle.cancel();
let result = handle.wait().await; // 返回 Err(DownloadError::Cancelled)
```

## 架构概览

```text
DownloadManager
  └─ DownloadSession
       ├─ Scheduler (分片分配、区段回收)
       ├─ HttpWorker × N (Range 请求、失败重试)
       ├─ channel -> Writer (WriteBackCache -> FileWriter)
       └─ ControlStore (原子化保存 / 加载 / 删除)
```

## 许可证

MIT
