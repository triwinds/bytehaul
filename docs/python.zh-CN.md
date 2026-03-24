# bytehaul Python 使用文档

本文介绍如何在当前仓库中构建、安装并使用 `bytehaul` 的 Python 绑定。

[English Python Guide](../bindings/python/README.md)

## 环境要求

- Python 3.9+
- Rust toolchain
- `uv`

下面的命令默认在仓库根目录执行。

## 初始化开发环境

先同步 Python 侧的开发依赖：

```bash
uv sync --project bindings/python
```

## 安装开发版本

将 Rust 扩展编译并安装到当前 `uv` 管理的环境中：

```bash
cd bindings/python
uv run --project . maturin develop -m Cargo.toml
```

安装完成后，就可以直接在 Python 中导入 `bytehaul`。

## 构建 Wheel

如果你需要分发或本地验证 wheel，可以执行：

```bash
cd bindings/python
uv run --project . maturin build --release -m Cargo.toml
```

`maturin` 会在命令输出中打印 wheel 的生成路径。

## 快速开始

### 阻塞式下载

```python
import bytehaul

bytehaul.download("https://example.com/file.bin", "output.bin")
```

### 传入下载参数

```python
from pathlib import Path

import bytehaul

bytehaul.download(
    "https://example.com/file.bin",
    Path("output.bin"),
    max_connections=8,
    max_download_speed=1_000_000,
    headers={"Authorization": "Bearer token"},
    checksum_sha256="e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
)
```

`output_path` 同时支持 `str` 和 `pathlib.Path`。

### 配置代理、DNS 和 IPv6

```python
import bytehaul

bytehaul.download(
    "https://example.com/file.bin",
    "output.bin",
    proxy="http://127.0.0.1:7890",
    dns_servers=["1.1.1.1", "8.8.8.8:53"],
    enable_ipv6=False,
)
```

对象 API 下，这些网络层参数应放在 `Downloader(...)` 构造器上：

```python
from bytehaul import Downloader

downloader = Downloader(
    connect_timeout=15.0,
    dns_servers=["1.1.1.1"],
    enable_ipv6=False,
)
```

### 日志

```python
# 在便捷函数中启用调试日志
bytehaul.download(
    "https://example.com/file.bin",
    "output.bin",
    log_level="debug",
)

# 或在 Downloader 对象上设置
from bytehaul import Downloader

downloader = Downloader(log_level="info")
```

可选级别：`"off"`（默认）、`"error"`、`"warn"`、`"info"`、`"debug"`、`"trace"`（不区分大小写）。

### 使用对象 API 获取进度和取消下载

```python
from bytehaul import Downloader

downloader = Downloader(connect_timeout=15.0)
task = downloader.download(
    "https://example.com/large.bin",
    "large.bin",
    max_connections=8,
    resume=True,
)

snap = task.progress()
print(
    f"state={snap.state} downloaded={snap.downloaded} "
    f"speed={snap.speed:.0f} B/s"
)

# 如有需要可取消
# task.cancel()

task.wait()
```

注意：

- `task.wait()` 会消费当前任务句柄，不能重复调用。
- `task.wait()` 返回后，不能再调用 `task.progress()` 获取进度。

### 错误处理

```python
from bytehaul import ConfigError, CancelledError, DownloadFailedError, download

try:
    download("https://example.com/file.bin", "output.bin")
except ConfigError as exc:
    print(f"参数无效: {exc}")
except CancelledError:
    print("下载已取消")
except DownloadFailedError as exc:
    print(f"下载失败: {exc}")
```

## API 概览

### `download(url, output_path, **options)`

阻塞式便捷函数。调用后会一直等待，直到下载完成或失败。

### `Downloader(connect_timeout=None, proxy=None, http_proxy=None, https_proxy=None, dns_servers=None, enable_ipv6=None)`

可复用的下载器实例。

- `downloader.download(url, output_path, **options) -> DownloadTask`

### `DownloadTask`

运行中下载任务的句柄。

- `task.progress()`：获取当前进度快照
- `task.cancel()`：取消下载
- `task.wait()`：阻塞等待下载结束

### `ProgressSnapshot`

进度快照对象，包含以下常用属性：

| 属性 | 类型 | 说明 |
| --- | --- | --- |
| `total_size` | `int \| None` | 文件总大小，未知时为 `None` |
| `downloaded` | `int` | 已下载字节数 |
| `state` | `str` | 当前状态，如 `pending`、`downloading`、`completed` |
| `speed` | `float` | 当前下载速度，单位为字节/秒 |
| `elapsed_secs` | `float \| None` | 已耗时秒数 |

## 常用参数

| 参数 | 类型 | 默认值 | 说明 |
| --- | --- | --- | --- |
| `headers` | `dict[str, str]` | `{}` | 自定义请求头 |
| `max_connections` | `int` | `4` | 最大并发连接数 |
| `connect_timeout` | `float` | `30.0` | 连接超时，单位秒 |
| `read_timeout` | `float` | `60.0` | 读取超时，单位秒 |
| `memory_budget` | `int` | `67108864` | 内存预算，单位字节 |
| `file_allocation` | `"none" \| "prealloc"` | `"prealloc"` | 文件预分配策略 |
| `resume` | `bool` | `True` | 是否启用断点续传 |
| `piece_size` | `int` | `1048576` | 分片大小，单位字节 |
| `min_split_size` | `int` | `10485760` | 文件大于该值时才拆分 |
| `max_retries` | `int` | `5` | 最大重试次数 |
| `retry_base_delay` | `float` | `1.0` | 重试基础退避时间，单位秒 |
| `retry_max_delay` | `float` | `30.0` | 重试最大退避时间，单位秒 |
| `max_download_speed` | `int` | `0` | 最大下载速度，`0` 表示不限速 |
| `checksum_sha256` | `str \| None` | `None` | 下载完成后的 SHA-256 校验值 |
| `log_level` | `str \| None` | `None`（`"off"`） | 日志级别 |

## 网络层参数

| 参数 | 类型 | 默认值 | 说明 |
| --- | --- | --- | --- |
| `proxy` | `str \| None` | `None` | 为所有 HTTP/HTTPS 请求设置统一代理 |
| `http_proxy` | `str \| None` | `None` | 仅为 HTTP 请求设置代理 |
| `https_proxy` | `str \| None` | `None` | 仅为 HTTPS 请求设置代理 |
| `dns_servers` | `list[str] \| None` | `None` | 自定义 DNS 服务器，支持 `IP` 或 `IP:PORT` |
| `enable_ipv6` | `bool \| None` | `True` | 是否允许解析和连接 IPv6 地址 |

## 运行测试

```bash
cd bindings/python
uv run --project . pytest
```

如果刚拉起环境，建议先执行一次：

```bash
uv sync --project bindings/python
cd bindings/python
uv run --project . maturin develop -m Cargo.toml
uv run --project . pytest
```
