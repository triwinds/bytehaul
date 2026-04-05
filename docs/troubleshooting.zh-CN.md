# bytehaul 故障排查指南

本文汇总常见问题及对应的排查方式。

[English Version](troubleshooting.md)

## 控制文件损坏

**现象：** 续传时下载失败，并报出 `ControlFileCorrupted` 错误。

**原因：** `.bytehaul` 控制文件只写入了一部分（例如保存过程中断电），或者被外部程序修改过。

**处理方法：** 删除控制文件后重新下载。控制文件默认位于输出文件旁边，扩展名为 `.bytehaul`：

```bash
rm /path/to/your-file.zip.bytehaul
```

删除后，下载会从头开始。

## 代理配置

**现象：** 在代理网络后出现连接失败，或者日志里有 `ProxyError`。

**处理方法：**

1. 通过 API 配置 SOCKS5 或 HTTP 代理：

   ```python
   import bytehaul

   dl = bytehaul.Downloader(
       proxy="http://proxy.example.com:8080"  # 所有协议共用
       # 或者：
       # http_proxy="http://proxy:8080",
       # https_proxy="http://proxy:8080",
   )
   ```

2. 使用环境变量：bytehaul（经由 reqwest）同样会读取 `HTTP_PROXY`、`HTTPS_PROXY` 和 `ALL_PROXY`。

3. 单独验证代理本身是否可用：

   ```bash
   curl -x http://proxy:8080 https://example.com
   ```

## DNS 解析失败

**现象：** 错误消息中包含 `DnsError` 或 `ResolveError`。

**可能原因与处理方法：**

1. 自定义 DNS 服务器不可达：

   如果你配置了自定义 DNS，请先确认它们可访问：

   ```python
   dl = bytehaul.Downloader(dns_servers=["8.8.8.8:53", "1.1.1.1:53"])
   ```

2. IPv6 网络存在问题：

   某些网络环境下 IPv6 表面可用、实际不可达。可以显式关闭 IPv6 解析：

   ```python
   dl = bytehaul.Downloader(enable_ipv6=False)
   ```

3. 企业防火墙拦截 DNS：

   尝试回退到系统默认解析器（不要设置 `dns_servers`），同时确认防火墙允许 DNS 流量。

## 日志级别

开启日志通常是定位问题最快的方式。bytehaul 内部使用 `tracing` 框架。

### Python

```python
bytehaul.download(url, log_level="debug")
# 或者使用 Downloader：
dl = bytehaul.Downloader(log_level="debug")
```

### Rust

```rust
use bytehaul::{Downloader, LogLevel};

let dl = Downloader::builder()
    .log_level(LogLevel::Debug)
    .build()?;
```

可用级别（从最详细到最简略）：`trace`、`debug`、`info`、`warn`、`error`、`off`（默认）。

**推荐选择：**

- `debug`：可查看 HTTP 请求/响应细节、分片调度、控制文件操作
- `trace`：会包含按 chunk 级别的数据流细节，日志非常多，只适合深度调试
- `info`：仅输出较高层级的进度事件

## 下载卡住或速度偏慢

**现象：** 下载看起来没有继续推进，或者速度明显低于预期。

**排查清单：**

1. 是否设置了限速：检查 `max_download_speed` 是否被设成了较低值。

2. 内存预算是否过小：如果磁盘较慢而 `memory_budget` 又偏小，Worker 可能会长时间等待 flush。可以尝试提高内存预算：

   ```python
   bytehaul.download(url, memory_budget=128 * 1024 * 1024)  # 128 MiB
   ```

3. 并发连接是否过多：某些服务器会对过多并行连接限速。可以尝试把 `max_connections` 降到 1 或 2。

4. 服务端是否不支持 Range：如果服务端不支持 Range 请求，bytehaul 会自动回退到单连接模式。可在 `debug` 日志中查找类似 “server does not support range” 的信息。

## 无法续传

**现象：** 下载没有从断点恢复，而是从头开始。

**可能原因：**

1. `resume` 被关闭：确认 `resume=True`（默认即开启）。

2. 服务端文件已经变化：如果相较上次下载，服务端返回的 `ETag` 或 `Last-Modified` 发生变化，bytehaul 会丢弃旧控制文件并重下，以避免数据损坏。

3. 控制文件丢失：`.bytehaul` 文件被删除了，或者输出文件被移动到别的目录。

4. 服务端不支持 Range 请求：续传依赖服务端接受 `Range` 头。单连接模式虽然会跟踪 `downloaded_bytes`，但真正恢复仍然需要 Range 支持。

## Python 导入失败

**现象：** 报错 `ImportError: No module named 'bytehaul'` 或类似错误。

**处理方法：** 确认本地原生扩展已经构建并安装：

```bash
cd bindings/python
uv sync
uv run maturin develop
```

如果是生产环境，请直接安装 wheel：

```bash
pip install bytehaul
```

## Windows 下的覆盖率报告

**现象：** 在 Windows 上执行 `cargo tarpaulin` 时，可能拿不到完整输出，或者出现 `os error 232`、`os error 5`、`LNK1104`，并残留 `cargo` / `rustc` 进程，导致二进制和日志文件持续被占用。

**原因：** Windows 下的覆盖率构建对进程清理和 target 目录复用比较敏感。如果上一次覆盖率任务被中断，或者两个覆盖率命令共用了同一个 target 目录，后续任务就可能在清理旧产物或链接带插桩的测试二进制时失败。

**处理方法：**

1. 开始新的覆盖率任务前，先确认没有残留的 `cargo` / `rustc` 进程。
2. 不要在 Windows 上并行执行多个覆盖率命令。
3. 使用仓库内置的 PowerShell 脚本，它会强制单线程构建并隔离 target 目录：

```powershell
rustup component add llvm-tools-preview
cargo install cargo-llvm-cov
powershell -ExecutionPolicy Bypass -File scripts/coverage-windows.ps1 -Scope tests -Format html
```

如果你需要机器可读的摘要报告，可以改用：

```powershell
powershell -ExecutionPolicy Bypass -File scripts/coverage-windows.ps1 -Scope tests -Format json
```

仓库的最终覆盖率门禁仍然在 Linux CI 上通过 Tarpaulin 校验。