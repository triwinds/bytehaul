# bytehaul Python 使用文档

本文介绍已发布的 **bytehaul 0.2.4** Python 绑定，以及从源码构建的方法。

[English Python Guide](../bindings/python/README.md)

## 环境要求

- Python 3.9+
- 从源码构建时另需 Rust toolchain 和 `uv`；安装已提供的 wheel 不需要这两项。

## 安装发布版本

```bash
pip install "bytehaul==0.2.4"
```

参阅 [0.2.4 发布说明](https://github.com/triwinds/bytehaul/releases/tag/v0.2.4)。下面的源码构建命令均假定从仓库根目录开始执行。

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

bytehaul.download("https://example.com/file.bin", output_path="output.bin")

# 自动根据响应头或 URL 推断文件名，并落到目标目录
bytehaul.download("https://example.com/file.bin", output_dir="downloads")
```

### 传入下载参数

```python
from pathlib import Path

import bytehaul

bytehaul.download(
    "https://example.com/file.bin",
    output_path=Path("output.bin"),
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
    output_path="output.bin",
    proxy="http://127.0.0.1:7890",
    dns_servers=["1.1.1.1", "8.8.8.8:53"],
    doh_servers=["https://dns.google/dns-query"],
    enable_ipv6=False,
)
```

`doh_servers` 需要传 HTTPS URL。如果使用像 `dns.google` 这样的域名，bytehaul 会在构建客户端时先用一次系统解析器来 bootstrap 这个 DoH 端点的 IP 地址。

对象 API 下，`dns_servers`、`doh_servers`、`enable_ipv6` 仍放在 `Downloader(...)` 构造器上；`proxy`、`http_proxy`、`https_proxy` 既可以放在 `Downloader(...)` 上作为默认值，也可以在 `downloader.download(...)` 时按任务覆盖：

```python
from bytehaul import Downloader

downloader = Downloader(
    connect_timeout=15.0,
    dns_servers=["1.1.1.1"],
    doh_servers=["https://dns.google/dns-query"],
    enable_ipv6=False,
)

task = downloader.download(
    "https://example.com/file.bin",
    output_path="file.bin",
    proxy="http://127.0.0.1:7890",
)
```

### 日志

```python
# 在便捷函数中启用调试日志
bytehaul.download(
    "https://example.com/file.bin",
    output_path="output.bin",
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
    output_path="large.bin",
    proxy="http://127.0.0.1:7890",
    max_connections=8,
    resume=True,
)

snap = task.progress()
print(
    f"state={snap.state} downloaded={snap.downloaded} "
    f"speed={snap.speed:.0f} B/s eta={snap.eta_secs}"
)

# 如有需要可暂停或取消
# task.pause()
# task.cancel()

task.wait()
```

注意：

- `task.wait()` 会消费当前任务句柄，不能重复调用。
- `task.wait()` 返回后，不能再调用 `task.progress()` 获取进度。

### 错误处理

```python
from bytehaul import ConfigError, CancelledError, PausedError, DownloadFailedError, download

try:
    download("https://example.com/file.bin", output_path="output.bin")
except ConfigError as exc:
    print(f"参数无效: {exc}")
except PausedError:
    print("下载已暂停")
except CancelledError:
    print("下载已取消")
except DownloadFailedError as exc:
    print(f"下载失败: {exc}")
```

响应体超时、连接重置或已知大小的响应体提前结束会受 `max_retries` 控制；该值表示初次尝试后的额外重试次数，`0` 表示禁用。已知总大小且对象校验信息匹配时，从 writer 确认的持久化前缀续传；服务端忽略 Range 或对象信息改变时先清空文件再重下。写盘或同步错误不会作为网络错误反复重试。

## API 概览

### `download(url, output_path=None, output_dir=None, **options)`

阻塞式便捷函数。调用后会一直等待，直到下载完成或失败。

- `output_path`：显式文件名或相对输出路径
- `output_dir`：输出目录
- 省略 `output_path` 时，会按 `Content-Disposition` → URL 路径 → `download` 自动选择文件名
- 若未设置 `output_dir`，仍可直接传绝对 `output_path`

### `Downloader(connect_timeout=None, proxy=None, http_proxy=None, https_proxy=None, dns_servers=None, doh_servers=None, enable_ipv6=None, log_level=None)`

可复用的下载器实例。

- `downloader.download(url, output_path=None, output_dir=None, **options) -> DownloadTask`

传给 `Downloader(...)` 的代理参数会作为默认值保留。若需要按任务切换代理，可在 `downloader.download(...)` 上继续传 `proxy`、`http_proxy` 或 `https_proxy` 覆盖本次下载。

### `DownloadTask`

运行中下载任务的句柄。

- `task.progress()`：获取当前进度快照
- `task.pause()`：暂停下载并尽量落盘控制文件
- `task.cancel()`：取消下载
- `task.wait()`：阻塞等待下载结束

### `ProgressSnapshot`

进度快照对象，包含以下常用属性：

| 属性 | 类型 | 说明 |
| --- | --- | --- |
| `total_size` | `int \| None` | 文件总大小，未知时为 `None` |
| `downloaded` | `int` | 面向界面的已接收字节数，不是持久化续传偏移 |
| `state` | `str` | 当前状态，如 `pending`、`downloading`、`completed`、`failed`、`cancelled`、`paused` |
| `speed` | `float` | 最近窗口内的下载速度，单位为字节/秒 |
| `eta_secs` | `float \| None` | 预计剩余秒数 |
| `elapsed_secs` | `float \| None` | 已耗时秒数 |

`downloaded` 可能在重试时回退，也可能在最终同步失败时已经等于 `total_size`。最终写入或同步失败会显示为 `failed`，控制文件只保留已确认持久化的进度；请以 `task.wait()` 的返回或异常判断任务结果，不要把显示字节数当作可恢复偏移。

`speed` 和 `eta_secs` 使用同一条最近吞吐窗口。`speed` 不是全程平均速度；在最近样本不足或总大小未知时，`eta_secs` 会保持为 `None`。

## 常用参数

| 参数 | 类型 | 默认值 | 说明 |
| --- | --- | --- | --- |
| `output_path` | `str \| Path \| None` | `None` | 显式文件名或相对输出路径 |
| `output_dir` | `str \| Path \| None` | `None` | 输出目录 |
| `headers` | `dict[str, str]` | `{}` | 自定义请求头 |
| `max_connections` | `int` | `4` | 最大并发连接数 |
| `connect_timeout` | `float` | `30.0` | 连接超时，单位秒 |
| `read_timeout` | `float` | `60.0` | 读取超时，单位秒 |
| `request_headers_timeout` | `float` | `None` | 单次请求到响应头期限（含建连），省略时继承 `read_timeout` |
| `memory_budget` | `int` | `67108864` | 内存预算，单位字节 |
| `file_allocation` | `"none" \| "prealloc"` | `"prealloc"` | 文件预分配策略 |
| `resume` | `bool` | `True` | 是否启用断点续传 |
| `piece_size` | `int` | `1048576` | 分片大小，单位字节 |
| `min_split_size` | `int` | `10485760` | 文件大于该值时才拆分 |
| `max_retries` | `int` | `5` | 初次请求/传输失败后的额外重试次数；`0` 表示不重试 |
| `retry_base_delay` | `float` | `1.0` | 重试基础退避时间，单位秒 |
| `retry_max_delay` | `float` | `30.0` | 重试最大退避时间，单位秒 |
| `max_retry_elapsed` | `float \| None` | `None` | 总重试时间预算，单位秒 |
| `control_save_interval` | `float` | `5.0` | 检查断点保存条件的间隔，单位秒 |
| `autosave_sync_every` | `int` | `2` | 存在未保存进度时，每 N 次检查尝试持久化 |
| `max_download_speed` | `int` | `0` | 最大下载速度，`0` 表示不限速 |
| `checksum_sha256` | `str \| None` | `None` | 下载完成后的 SHA-256 校验值 |
| `log_level` | `str \| None` | `None`（`"off"`） | 日志级别 |

`log_level` 用于便捷函数 `download(...)` 或 `Downloader(...)` 构造器。

## 网络层参数

对象 API 中，`proxy`、`http_proxy`、`https_proxy` 既可用于 `Downloader(...)` 默认值，也可用于 `downloader.download(...)` 的单次覆盖；`dns_servers`、`doh_servers`、`enable_ipv6` 仍通过 `Downloader(...)` 配置。

| 参数 | 类型 | 默认值 | 说明 |
| --- | --- | --- | --- |
| `proxy` | `str \| None` | `None` | 为所有 HTTP/HTTPS 请求设置统一代理 |
| `http_proxy` | `str \| None` | `None` | 仅为 HTTP 请求设置代理 |
| `https_proxy` | `str \| None` | `None` | 仅为 HTTPS 请求设置代理 |
| `dns_servers` | `list[str] \| None` | `None` | 自定义 DNS 服务器，支持 `IP` 或 `IP:PORT` |
| `doh_servers` | `list[str] \| None` | `None` | 自定义 DNS-over-HTTPS 端点，传入 `https://...` URL |
| `enable_ipv6` | `bool \| None` | `True` | 是否允许解析和连接 IPv6 地址 |

## 运行测试

```bash
cd bindings/python
uv run --no-sync --project . pytest
```

如果刚拉起环境，建议先执行一次：

```bash
uv sync --project bindings/python
cd bindings/python
uv run --project . maturin develop -m Cargo.toml
uv run --no-sync --project . pytest
```

在 `maturin develop` 之后运行测试时使用 `--no-sync`，避免 uv 再次同步时替换刚构建的开发扩展。

## 慢请求恢复

从 0.2.2 起提供。`download(...)` 和 `Downloader.download(...)` 均支持以下参数；`None` 沿用 Rust 默认值。

| 参数 | 默认值 | 含义 |
| --- | --- | --- |
| `slow_transfer_mode` | `"adaptive"` | `"disabled"`、`"adaptive"` 或 `"adaptive_with_hedging"`，不区分大小写 |
| `low_speed_limit` | `None` | 可选绝对速率下限，字节/秒，必须大于零 |
| `low_speed_duration` | `15.0` | 持续低速判定时间，秒 |
| `slow_start_grace` | `5.0` | 启动宽限，秒 |
| `slow_sample_window` | `5.0` | 采样窗口，秒 |

时间参数必须有限、大于零且不超过 86,400 秒。多连接 Range 下载默认自动恢复慢请求并接管尾部分片；竞速需要显式开启，且要求强 ETag 与空闲并发槽。所有请求合计不超过 `max_connections`，共用限速预算，备用数据不重复计入进度。

```python
from bytehaul import Downloader

task = Downloader(log_level="debug").download(
    "https://example.com/file.bin",
    "file.bin",
    slow_transfer_mode="adaptive_with_hedging",
)
task.wait()
```

自动恢复与竞速共享 `min(total_size / 100, 16 MiB)` 的额外 body 工作预算，不够则跳过。强校验对象取消续传只为可能丢弃的已读取数据预留，必需后缀不算额外流量；竞速仍需预留完整范围。预算不包含未消费的传输缓冲及 TCP/TLS 开销。批次中途接管后释放的分片共享原有重试预算与 Retry-After；普通错误重试沿用原策略。主动限速和本地背压不计为网络慢。使用 `"disabled"` 关闭性能恢复；单连接与不支持 Range 的回退行为保持不变。详见 [Rust 慢请求恢复说明](advanced.zh-CN.md)。

当 `max_download_speed` 非零时，自动低速恢复和竞速会暂停，避免将主动限速误判为网络故障；普通超时与错误重试仍然生效。

### 请求响应头期限

Rust 的 `DownloadSpec::request_headers_timeout(Duration)` 与 Python 两个下载 API
末尾新增的 `request_headers_timeout`（秒，默认 `None`）设置单次请求从发起到收到
响应头的期限，包含连接池等待、DNS/TCP/TLS 建连，并非纯服务端 TTFB。省略时继续
使用原来的 `read_timeout` 响应头期限；响应体读取仍由 `read_timeout` 控制。
值必须为正数且能表示为单调时钟期限。连接超时可能更早生效。

每次重试和每个重定向跳转分别计时，probe、GET fallback、续传和普通 Range 均适用。
这不是整个重定向链或整个下载的总期限；原有重试次数、`max_retry_elapsed` 检查边界
和 429/503 的 `Retry-After` 保持不变。未配置时不会自动缩短期限或发起响应头 hedge。

现有行为中，probe 的传输错误（含超时）可进入 GET fallback；两者保留各自的重试预算。
`max_retry_elapsed` 不是整个下载（或两阶段合计）的硬期限。
