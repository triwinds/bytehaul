# bytehaul

[![Tests](https://img.shields.io/github/actions/workflow/status/triwinds/bytehaul/test.yml?branch=main&logo=githubactions&label=tests)](https://github.com/triwinds/bytehaul/actions/workflows/test.yml)
[![Crates.io](https://img.shields.io/crates/v/bytehaul?logo=rust)](https://crates.io/crates/bytehaul)
[![Docs.rs](https://img.shields.io/docsrs/bytehaul?logo=docs.rs)](https://docs.rs/bytehaul)
[![PyPI](https://img.shields.io/pypi/v/bytehaul?logo=pypi)](https://pypi.org/project/bytehaul/)
[![Python](https://img.shields.io/pypi/pyversions/bytehaul?logo=python&logoColor=white)](https://pypi.org/project/bytehaul/)
[![License](https://img.shields.io/github/license/triwinds/bytehaul)](LICENSE)

Rust 异步 HTTP 下载库，带有 Python 绑定（同时在 PyPI 发布），支持断点续传、多连接并发、回写缓存、限速和校验。

## 文档

- [English README](../README.md)
- [进阶用法（Rust）](advanced.zh-CN.md) | [Advanced Usage](advanced.md)
- [架构说明](architecture.zh-CN.md) | [Architecture](architecture.md)
- [故障排查指南](troubleshooting.zh-CN.md) | [Troubleshooting Guide](troubleshooting.md)
- [性能调优指南](tuning.zh-CN.md) | [Performance Tuning Guide](tuning.md)
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
bytehaul = "0.1.7"
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

    let spec = DownloadSpec::new("https://example.com/largefile.zip")
        .output_path("largefile.zip");

    let handle = downloader.download(spec);
    handle.wait().await?;
    println!("下载完成！");
    Ok(())
}
```

更多配置、进度监控、取消下载等进阶用法，请参阅[进阶用法指南](advanced.zh-CN.md)。

如果省略 `output_path`，bytehaul 会按 `Content-Disposition`、URL 路径最后一段、默认名 `download` 自动选择文件名。可结合 `.output_dir("downloads")` 指定目标目录；在未设置 `output_dir` 时，也仍然支持直接传绝对输出路径。

## 快速开始（Python）

```python
import bytehaul

# 一行代码完成下载
bytehaul.download("https://example.com/file.bin", output_path="output.bin")

# 自动文件名下载到指定目录
bytehaul.download("https://example.com/file.bin", output_dir="downloads")

# 传入下载参数
bytehaul.download(
    "https://example.com/file.bin",
    output_path="output.bin",
    max_connections=8,
    max_download_speed=1_000_000,  # 限速 1 MB/s
)
```

完整的 Python API（对象 API、进度、取消、错误处理等），请参阅 [Python 使用文档](python.zh-CN.md)。

## 覆盖率

CI 中的覆盖率门禁仍然在 Ubuntu 上通过 Tarpaulin 执行：

```bash
cargo tarpaulin --engine llvm --workspace --all-targets --out Stdout --fail-under 95
```

在 Windows 上，如果直接跑 Tarpaulin，常见问题是中断后残留句柄、二进制被锁住，或者终端拿不到完整总结。仓库里提供了专门的 PowerShell 脚本来生成本地报告：

```powershell
rustup component add llvm-tools-preview
cargo install cargo-llvm-cov
powershell -ExecutionPolicy Bypass -File scripts/coverage-windows.ps1 -Scope all-targets -Format html
```

如果你需要机器可读的摘要，可以使用：

```powershell
powershell -ExecutionPolicy Bypass -File scripts/coverage-windows.ps1 -Scope all-targets -Format json
```

该脚本现在默认按与 CI 一致的 `all-targets` 口径执行，并会强制 `CARGO_BUILD_JOBS=1`、为每次运行使用新的隔离 `CARGO_TARGET_DIR`，以减少 Windows 下 `os error 5`、`LNK1104` 以及残留 `cargo` / `rustc` 进程带来的冲突。

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
