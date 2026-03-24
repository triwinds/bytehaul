# bytehaul

Rust 异步 HTTP 下载库，支持断点续传、多连接并发、回写缓存、限速和校验。

## 文档

- [English README](../README.md)
- [进阶用法（Rust）](advanced.zh-CN.md) | [Advanced Usage](advanced.md)
- [Python 使用文档](python.zh-CN.md) | [Python Bindings Guide](../bindings/python/README.md)

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

## 安装

### Rust

通过 Cargo 添加依赖：

```bash
cargo add bytehaul
```

或手动添加到 `Cargo.toml`：

```toml
[dependencies]
bytehaul = "0.1"
```

### Python

```bash
pip install bytehaul
```

需要 Python 3.9+。每个平台只需一个 wheel 即可覆盖所有支持的 Python 版本（abi3）。

## 快速开始（Rust）

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

更多配置、进度监控、取消下载等进阶用法，请参阅[进阶用法指南](advanced.zh-CN.md)。

## 快速开始（Python）

```python
import bytehaul

# 一行代码完成下载
bytehaul.download("https://example.com/file.bin", "output.bin")

# 传入下载参数
bytehaul.download(
    "https://example.com/file.bin",
    "output.bin",
    max_connections=8,
    max_download_speed=1_000_000,  # 限速 1 MB/s
)
```

完整的 Python API（对象 API、进度、取消、错误处理等），请参阅 [Python 使用文档](python.zh-CN.md)。

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
