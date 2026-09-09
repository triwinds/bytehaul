# bytehaul

[![Tests](https://img.shields.io/github/actions/workflow/status/triwinds/bytehaul/test.yml?branch=master&logo=githubactions&label=tests)](https://github.com/triwinds/bytehaul/actions/workflows/test.yml)
[![Crates.io](https://img.shields.io/crates/v/bytehaul?logo=rust)](https://crates.io/crates/bytehaul)
[![Docs.rs](https://img.shields.io/docsrs/bytehaul?logo=docs.rs)](https://docs.rs/bytehaul)
[![PyPI](https://img.shields.io/pypi/v/bytehaul?logo=pypi)](https://pypi.org/project/bytehaul/)
[![Python](https://img.shields.io/pypi/pyversions/bytehaul?logo=python&logoColor=white)](https://pypi.org/project/bytehaul/)
[![License](https://img.shields.io/github/license/triwinds/bytehaul)](../LICENSE)

Rust 异步 HTTP 下载库，带有 Python 绑定（同时在 PyPI 发布），支持断点续传、多连接并发、回写缓存、限速和校验。

本文示例适用于 **0.2.3**，该版本优化连续 Range 请求和慢请求尾段恢复，并保留已有断点文件格式兼容性。参阅 [0.2.3 发布说明](https://github.com/triwinds/bytehaul/releases/tag/v0.2.3)。

0.2.3 新增连续请求合并、已确认前缀复用和有界尾段恢复，详见[高级用法](advanced.zh-CN.md)。

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
- **指数退避重试**：单/多连接共用重试策略，响应体失败可安全续传，并尊重 `Retry-After`
- **下载限速**：所有工作线程共享令牌桶限速
- **SHA-256 校验**：下载完成后进行完整性校验
- **取消下载**：通过 watch channel 协作取消
- **进度订阅**：实时获取速度、已下载字节数与状态

## 安装

### Rust

通过 Cargo 添加依赖：

```bash
cargo add bytehaul@0.2.3
```

或手动添加到 `Cargo.toml`：

```toml
[dependencies]
bytehaul = "0.2.3"
```

### Python

```bash
pip install "bytehaul==0.2.3"
```

需要 Python 3.9+。每个平台只需一个 wheel 即可覆盖所有支持的 Python 版本（abi3）。

## 快速开始（Rust）

```rust
use bytehaul::{DownloadSpec, Downloader};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let downloader = Downloader::builder().build()?;

    let spec = DownloadSpec::new("https://example.com/largefile.zip")
        .output_path("largefile.zip")
        .all_proxy("http://127.0.0.1:7890");

    let handle = downloader.download(spec);
    handle.wait().await?;
    println!("下载完成！");
    Ok(())
}
```

更多配置、进度监控、取消下载等进阶用法，请参阅[进阶用法指南](advanced.zh-CN.md)。如果要设置默认网络参数，可放在 `Downloader::builder()` 上；如果要只影响当前下载任务，则把代理等覆盖项放在 `DownloadSpec` 上。

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

CI 与本地 Linux 使用同一个入口。在 Ubuntu 24.04 x86_64 上安装 Python 3、rustup、C 编译器、pkg-config 和 OpenSSL 开发头文件后运行：

```bash
python3 scripts/coverage.py --install
```

`--install` 安装 [coverage-config.json](../scripts/coverage-config.json) 中固定的 Rust 和 Tarpaulin 版本；安装后可省略，脚本会校验工具版本。门禁使用 LLVM、`-p bytehaul --all-targets`、锁定的依赖和 **95% 行覆盖率门槛**，不增加源码排除规则。脚本忽略额外的 Tarpaulin 配置，并隔离可能干扰 localhost 测试的代理环境变量。

源码目录需要可写：即使构建目录位于其他位置，LLVM 插桩的构建脚本仍可能在源码目录写入 profile 文件。在内存较小的 Linux 虚拟机中，可减少同时运行的编译任务。链接仍可能超出 2 GB 虚拟机的内存；链接器被系统终止时需要更多内存：

```bash
CARGO_BUILD_JOBS=1 python3 scripts/coverage.py --install
```

这只调整构建并发，不改变测试范围和覆盖率门槛。报告元数据会记录 Linux 发行版，以及显式设置的 `CARGO_BUILD_JOBS` 值。

每次运行的构建和报告目录均独立，位于 `target/coverage/`。`target/coverage/reports/<run>/` 保留 JSON、HTML、完整日志、工具版本、提交号及工作区是否有修改的信息。测试失败会标记为“覆盖率未完整收集”；测得的比例低于 95% 则标记为“未达到门槛”，两者都会返回失败。Actions 在失败时仍显示摘要并上传诊断文件。

提交 Rust 改动前，在待提交的相同代码上运行此门禁。`cargo test`、Clippy 通过，或不带门槛的报告生成成功，都**不代表覆盖率达标**。macOS/Windows 编译的代码路径不同，本地平台报告不能代替 Linux 门禁；可使用 Ubuntu x86_64 虚拟机、WSL 环境或 GitHub 覆盖率任务进行基准验证。升级工具时统一修改配置，并重新运行完整门禁。

Windows 的补充诊断仍使用独立脚本：

```powershell
rustup component add llvm-tools-preview
cargo install cargo-llvm-cov
powershell -ExecutionPolicy Bypass -File scripts/coverage-windows.ps1 -Scope all-targets -Format html
# 机器可读报告：
powershell -ExecutionPolicy Bypass -File scripts/coverage-windows.ps1 -Scope all-targets -Format json
```

Windows 脚本使用 cargo-llvm-cov，显式检查共享的 **95% 行覆盖率门槛**，默认范围是 `all-targets`；`tests`、`lib` 的统计范围更窄。成功仅证明所选 Windows 范围达标，其分母与 Linux Tarpaulin 不同。每次运行使用新的构建目录及默认报告路径，并限制单任务构建，减少文件锁和旧报告混淆。

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
