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

`max_retries` 表示初次请求/传输之后允许的额外重试次数；`max_retry_elapsed` 则补充了“总共最多重试多久”的时间预算。单连接 body 失败会在同一个 transfer retry scope 内从 flush barrier 的连续前缀续传，后续 Range 建连和响应校验不会重新计数。如果继续退避会超出这个预算，请求会以 `DownloadError::RetryBudgetExceeded` 结束，而不是只看次数上限。

如果省略 `.output_path(...)`，bytehaul 会依次按 `Content-Disposition`、URL 路径最后一段、默认名 `download` 自动选择文件名。若未设置 `.output_dir(...)`，仍可继续直接传绝对输出路径。

## 连续请求与连接复用

0.2.3 默认启用跨 piece 的连续 Range 请求：

```rust
use bytehaul::DownloadSpec;
use std::time::Duration;

let spec = DownloadSpec::new("https://example.com/file.bin")
    .piece_size(1024 * 1024)
    .request_batch_size(4 * 1024 * 1024)
    .http_idle_pool(4, Duration::from_secs(30));
```

`request_batch_size` 默认 4 MiB；设置为零可关闭合并。合并同时受字节数和最多 64 个
租约限制，piece 与检查点粒度保持不变；小于 piece 的值不会将它切小。遇到已完成、
已分配或部分处理过的 piece 时停止合并，并为其他 worker 留出工作。该选项适用于已知
大小的多连接下载，包括关闭慢请求恢复的模式。连接池默认每个 host 保留最多 4 条空闲
连接，超时 30 秒；使用 `disable_http_idle_pool()` 或将空闲连接上限设为零可关闭连接池。
服务端主动关闭连接时，无法获得空闲连接复用的收益。

当强 ETag 和兼容的条件请求头能够保护对象一致性时，多连接断流重试和慢请求重新
分配可保留写入器确认的前缀，只请求剩余后缀。未完成的 piece 不会因此写入检查点
完成位，因此进程重启后仍可能重新下载它。重试次数和时间预算保持原有范围；缺少
验证器或只有弱验证器时仍重下整个范围，格式错误的响应不适用前缀复用。

可用[本地对比示例](../examples/http_efficiency_compare.rs)测量请求数、连接数，以及
受控延迟和断流下的耗时。实际收益取决于源站。

## 慢请求恢复

该功能从 0.2.2 起提供。

多连接 Range 下载默认使用 `SlowTransferMode::Adaptive`：识别持续低速请求后，可取消并重新分配分片，尾部空闲 worker 也可接管。速度判断排除主动限速和本地转发等待；短暂波动或即将完成的请求不会直接触发恢复。`Disabled` 关闭因性能原因触发的取消和对冲。

```rust
use bytehaul::{DownloadSpec, SlowTransferMode};
use std::time::Duration;

let spec = DownloadSpec::new("https://example.com/file.bin")
    .slow_transfer_mode(SlowTransferMode::AdaptiveWithHedging)
    .low_speed_duration(Duration::from_secs(15))
    .slow_start_grace(Duration::from_secs(5))
    .slow_sample_window(Duration::from_secs(5));
```

`low_speed_limit(bytes_per_second)` 可设置大于零的绝对速率下限；默认不设绝对下限，参考健康请求速率。三个时间参数必须大于零且不超过 86,400 秒，默认分别是持续低速 15 秒、启动宽限 5 秒、采样窗口 5 秒。

0.2.3 增加尾段快速检测：范围不超过 1 MiB、没有待分配工作且有空闲请求槽位时，可提前恢复。独立检测器将配置的采样窗口、启动宽限和持续低速时间分别限制为最多 1、1、2 秒。它必须有近期健康速率参考，且预计能明显节省时间；仅有绝对速率下限不够。其他活动请求的证据缺失或尚未成熟、集体降速、本地背压都会阻止加速。普通检测仍使用配置的时间参数。两种 adaptive 模式都可使用，`Disabled` 关闭该行为。

竞速需显式开启，只为较小的尾部分片增加至多一个备用请求，要求强 ETag 和兼容的条件请求头。备用响应先独立暂存，完整校验后才能切换写入权。主请求与备用请求合计不超过 `max_connections`，所有网络数据共用 `max_download_speed`。自动恢复与竞速共享 `min(total_size / 100, 16 MiB)` 的额外工作预算。强校验对象先取消再续传时，只为已读取、可能丢弃的预读数据预留预算；必需后缀不是额外工作。不能安全保留前缀的重放，以及并行竞速，仍按保守的范围成本预留。因此竞速超出预算时，安全的取消续传仍可能允许。该预算只计应用已消费的 HTTP body，不包含尚未读取的传输缓冲及 TCP/TLS 开销。普通网络错误继续遵守独立的既有重试策略。

其他并发槽空闲且存在健康速度历史时，自适应恢复也可接管批次中途的慢请求。
旧 producer 停止、writer 确认前缀交接之后，才释放尚未开始的分片供重新调度。
释放的分片共享原有重试次数、时间预算和 Retry-After，不因切批而增加重试机会。
只有完整且已刷盘的分片会写入断点状态；默认请求批次仍为 4 MiB。

竞速模式下，如果完整备用范围超出预算，可回落到预算内的安全取消续传。
先释放暂取的并发槽，再重新检查预算、冷却和重试限制，不会扩大预算。

进度不累加重复流量，回收未完成尝试时可能回退。打开 debug 日志可观察恢复决策。这些策略不能提高共享源站或磁盘的带宽上限；单连接与不支持 Range 的回退行为保持不变。

当 `max_download_speed` 非零时，自动低速恢复和竞速会暂停，避免将主动限速误判为网络故障；普通超时与错误重试仍然生效。

## 网络层配置

下载器客户端上保存的是默认网络配置。DNS / DoH / IPv6 仍通过 `Downloader::builder()` 设置；单个任务则可以在 `DownloadSpec` 上覆盖 `connect_timeout` 和代理：

```rust
use std::net::SocketAddr;
use std::time::Duration;
use bytehaul::{DownloadSpec, Downloader};

let downloader = Downloader::builder()
    .dns_servers([
        SocketAddr::from(([1, 1, 1, 1], 53)),
        SocketAddr::from(([8, 8, 8, 8], 53)),
    ])
    .doh_server("https://dns.google/dns-query")
    .enable_ipv6(false)
    .build()?;

let spec = DownloadSpec::new("https://example.com/file.bin")
    .output_path("file.bin")
    .all_proxy("http://127.0.0.1:7890")
    .connect_timeout(Duration::from_secs(10));

let handle = downloader.download(spec);
```

builder 上的 `all_proxy(...)`、`http_proxy(...)`、`https_proxy(...)` 仍然适合作为默认值。若任务通过 `DownloadSpec::all_proxy(...)`、`http_proxy(...)`、`https_proxy(...)` 或 `connect_timeout(...)` 做覆盖，bytehaul 会按这组生效配置派生出一个等价 client，并在后续遇到相同配置时复用它。

`doh_server(...)` 和 `doh_servers(...)` 接收 HTTPS URL。如果 DoH 主机是域名而不是字面 IP，bytehaul 会在构建 client 时先用一次系统解析器把它 bootstrap 成目标地址。

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

`downloaded` 是面向界面的已接收字节数，不是控制文件里的持久化续传偏移。重试可能使其回退；即使它已等于总大小，最终写入或同步失败仍会使任务进入 `Failed`。请以 `handle.wait().await` 的结果判断任务是否成功。

`speed_bytes_per_sec` 和 `eta_secs` 现在共用同一条最近吞吐窗口：

- `speed_bytes_per_sec` 表示最近窗口内的速度，而不是从下载开始到当前时刻的全程平均值。
- `eta_secs` 直接使用同一窗口速度估算剩余时间，因此它和显示出来的速度会一起变化，而不是各走一套平滑规则。
- `eta_secs == None`：当前最近样本还不足以给出稳定 ETA，或者总大小仍未知。
- `eta_secs == Some(0.0)`：根据当前字节计数已无剩余时间；这不能单独证明写盘、同步或最终校验成功。

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

pause 不是把同一个 handle 原地挂起后再继续，而是结束当前任务，并在启用续传且存储正常时刷盘、保存控制文件。真正的恢复动作是后续再发起一次新的 `download(spec)` 调用，并且解析出的输出路径必须保持一致。

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

`Cancelled`、`Paused`、`Completed` 是不同的结束状态。`cancel()` 和 `pause()` 都会结束当前任务；启用续传时，两者都会尝试保存可恢复状态。写盘或同步失败时保留此前的持久化断点。正常完成会尝试删除控制文件；单连接将清理失败视为错误，多连接的清理是尽力而为。

### 请求到响应头期限

Rust 的 `DownloadSpec::request_headers_timeout(Duration)` 与两个 Python 下载
API 末尾的 `request_headers_timeout`（秒，默认 `None`）限制每次请求调用到收到
响应头的时间，包含连接池等待及 DNS/TCP/TLS 建连，不是纯服务端 TTFB。
未设置时继续继承 `read_timeout`；body 读取仍使用 `read_timeout`，连接超时
也可能更早触发。值必须大于零且能表示为单调时钟期限，不设额外的 24 小时上限。

每次重试、每一跳重定向都有独立期限，覆盖 probe、GET fallback、续传和普通
Range。它不是整个重定向链或下载的总期限，也不会自动缩短或触发 Headers 竞速。
现有重试次数与 429/503 的 Retry-After 规则保持不变。probe 传输错误（含超时）
可能进入 GET fallback，两阶段仍保留各自原有重试范围；`max_retry_elapsed`
在重试准入时检查，并非下载的硬性总时限。
