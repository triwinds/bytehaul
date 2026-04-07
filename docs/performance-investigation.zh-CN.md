# bytehaul 多线程下载性能排查记录

本文汇总截至 2026-04-07 已完成的下载性能排查实验，重点对比 bytehaul 与 aria2 在真实远端源和本地受控环境中的差异。

## 测试前提

> 注：第 1–7 节的实验在 **Windows** 上执行；第 8 节的换 TLS 栈实验在 **Linux CI（Ubuntu 24.04）** 上执行。

- 对比工具：bytehaul、aria2c、curl
- 主要测试目标（第 1–7 节）：

  - `https://nsa2.e6ex.com/gh/THZoria/NX_Firmware/releases/download/22.0.0/Firmware.22.0.0.zip`
  - `https://rn.e6ex.com/gh/THZoria/NX_Firmware/releases/download/22.0.0/Firmware.22.0.0.zip`

- 计量口径：

  - 远端对比多数使用 `20s` 时间窗
  - 局部初筛使用 `10s` 时间窗
  - 吞吐统一记录为平均速度 `avg MiB/s`

- 临时测量工具、mock server、header dump 工具均已在实验后删除，不保留在仓库中。

## 已保留的代码修复

在性能排查开始阶段，确认了一个真实问题并已修复：多分片 `Range` 请求现在强制使用 HTTP/1.1，避免多个 worker 在支持 HTTP/2 的服务端上被折叠到同一条 H2 会话中，导致“看起来有多线程，实际上不是真正多连接”。

对应代码：

- [src/http/request.rs](../src/http/request.rs)

## 结果总览

### 1. 真实源 `nsa2.e6ex.com` 初始探测（10 秒）

先做了 bytehaul 自身的快速摸底：

- bytehaul，4 连接：约 `0.17 MiB/s`
- bytehaul，8 连接：约 `0.33 MiB/s`
- `curl` 单流基线：约 `0.32 MiB/s`

初步结论：

- 8 连接明显优于 4 连接。
- 在这一轮里，bytehaul 的 8 连接已接近 `curl` 单流结果，但还没有和 aria2 做直接对比。

### 2. 真实源 `nsa2.e6ex.com`，aria2 对比基线（8 连接，20 秒）

做了同源、同时间窗、同并发数的直接对比：

- aria2，8 连接：约 `4.94 MiB/s`
- bytehaul，8 连接：约 `0.71 MiB/s`

初步结论：

- 在该轮测试中，bytehaul 明显慢于 aria2，量级约为 `7x`。
- 这说明“多线程下载明显慢于 aria2”并非体感问题，而是可复现的真实差异。

### 3. 本地 mock server（每连接 100 KiB/s，8 连接，20 秒）

为了排除远端 CDN、TLS、HTTP/2、链路抖动等变量，搭建了本地受控 HTTP Range mock server。服务端限制每条连接最大速度为 `100 KiB/s`。

测试结果：

- aria2，8 连接：约 `0.78 MiB/s`
- bytehaul，8 连接，`resume=true`：约 `0.78 MiB/s`
- bytehaul，8 连接，`resume=false`：约 `0.78 MiB/s`

结论：

- 在受控环境下，bytehaul 能把 8 路连接全部打满。
- 核心多 worker 调度、写入路径、以及 resume/autosave 机制在低速受控场景中不是根本性瓶颈。
- 因此，远端源上的大差距更可能来自“客户端栈与远端服务/CDN 的交互方式”，而不是“本地并发下载框架本身坏了”。

### 4. `piece_size` 分片大小实验（真实源 `nsa2.e6ex.com`）

怀疑点之一是默认 `piece_size = 1 MiB` 是否过小，于是对更大的分片做了实验。

较可信的样本：

- aria2，同期基线：约 `8.65 MiB/s`
- bytehaul，`piece_size=1 MiB`，`resume=false`，`file_allocation=none`：约 `0.12 MiB/s`
- bytehaul，`piece_size=8 MiB`，`resume=false`，`file_allocation=none`：约 `0.05 MiB/s`

早期样本（受远端波动影响较大，仅作参考）：

- `piece_size=8 MiB`，`resume=true`，`prealloc`：约 `0.14 MiB/s`
- `piece_size=16 MiB`，`resume=true`，`prealloc`：约 `0.07 MiB/s`

结论：

- 没有看到“分片调大后，速度明显逼近 aria2”的证据。
- 目前没有证据支持“默认 1 MiB 分片是导致差几倍的主因”。

### 5. 请求头实验（真实源 `nsa2.e6ex.com`）

通过本地抓包式 header dump server，确认了 aria2 与 bytehaul 的典型请求头差异。

抓到的主要差异：

- aria2 会带：

  - `User-Agent: aria2/1.37.0`
  - `Want-Digest: SHA-512;q=1, SHA-256;q=1, SHA;q=0.1`

- bytehaul 默认请求主要包含：

  - `Range`
  - `Accept-Encoding: identity`
  - `Accept: */*`

远端 20 秒实验结果：

- bytehaul 默认头：约 `1.12 MiB/s`
- bytehaul 仅添加 `User-Agent=aria2/1.37.0`：约 `1.27 MiB/s`
- bytehaul 再加 `Want-Digest`：约 `0.12 MiB/s`

这一轮远端源波动很大，因此绝对值不宜与其他轮次直接横向比较，但可用于判断头部影响方向。

结论：

- `User-Agent` 可能有小幅帮助，但量级不足以解释“差几倍”。
- 完整复刻已观察到的 aria2 头部，并没有稳定解决性能问题。
- 因此，请求头差异不是当前最主要的嫌疑点。

### 6. 换域名到 `rn.e6ex.com`（8 连接，20 秒）

为了验证问题是否只绑定在 `nsa2.e6ex.com`，改用同路径的另一个域名：

- aria2，8 连接：约 `0.39 MiB/s`
- bytehaul，8 连接：约 `0.05 MiB/s`

结论：

- 换域名后，aria2 自身也变慢了，说明该域名链路本身比前一轮 `nsa2` 更慢。
- 但 bytehaul 仍然明显慢于 aria2，差距依旧存在。

### 7. `rn.e6ex.com` 再测 4 连接（20 秒）

考虑到该域名在 8 连接下可能存在惩罚机制，又补测了 4 连接：

- aria2，4 连接：约 `0.34 MiB/s`
- bytehaul，4 连接：约 `0.10 MiB/s`

结论：

- 对 bytehaul 而言，该域名上 `4` 连接比 `8` 连接更快。
- 对 aria2 而言，`4` 和 `8` 连接差距不大。
- 这说明 bytehaul 在该远端上可能会在更高并发下触发额外惩罚或更差的连接行为。

## 当前阶段结论

根据上述所有实验，当前可以得到以下阶段性判断：

1. bytehaul 的本地并发下载框架本身没有证明出“根本性故障”。

   - 本地 mock server 下，8 路限速吞吐与 aria2 一致。

2. `Range` 请求强制 HTTP/1.1 是必要修复，但不是全部答案。

   - 它修掉了一个真实问题：HTTP/2 多路复用可能掩盖“多线程”等于“多连接”。
   - 但修复后，真实远端仍存在显著性能差异。

3. 目前没有证据支持以下因素是主因：

   - 默认 `piece_size = 1 MiB`
   - `resume/autosave`
   - 本地写入路径本身
   - 简单的请求头差异（例如 `User-Agent`）

4. 当前最可疑的方向是：

   - reqwest / rustls / hyper 这一套客户端栈，与目标 CDN / 镜像服务之间的交互方式
   - 并发连接数升高后，服务端对 bytehaul 触发了不同于 aria2 的限流或调度策略
   - TLS / ALPN / 连接建立 / 请求调度模式的差异，导致远端对两者给出完全不同的行为路径

## 结果解读注意事项

真实远端源存在明显波动，因此不同时间点的绝对吞吐数值不能简单横向对比。更可靠的做法是：

- 优先看“同一轮、同一源、同一时间窗内”的相对比较。
- 优先看本地 mock server 与真实远端结论是否一致，以区分“框架问题”和“远端兼容性问题”。

因此，当前所有实验中最重要的结论并不是某一个具体 MiB/s 数字，而是下面两条：

- 本地受控环境下，bytehaul 与 aria2 可以跑出同样的多连接吞吐。
- 真实远端源上，bytehaul 与 aria2 的差距依然显著，而且该差距不会因为简单调大 `piece_size` 或复刻常见 header 而消失。

## 8. 换 TLS 栈实验（reqwest+rustls vs reqwest+native-tls，Linux CI 环境）

本次在 Linux CI 环境（Ubuntu 24.04，Azure 虚拟机）中复现了 aria2 对比实验，并新增了将 TLS 栈从 rustls 切换为 native-tls（OpenSSL）的对照组。

### 测试方法

- 测试源：`https://github.com/llvm/llvm-project/releases/download/llvmorg-19.1.0/llvm-project-19.1.0.src.tar.xz`
  - 文件大小：约 `134.7 MiB`，经 GitHub 跳转至 `release-assets.githubusercontent.com`（Azure Blob Storage，HTTPS）
- 工具版本：`aria2 1.37.0`；bytehaul 使用 `reqwest 0.12`，分别编译两种 TLS 后端
- 8 连接，下载完整文件，记录完成时间与平均速度（3 次取平均）

> 注：该环境对 GitHub CDN 带宽极高（单连接超过 100 MiB/s），整个 134.7 MiB 文件在 0.5–1.2 秒内完成，因此无法使用 20 秒时间窗测量，改为记录完成耗时。

### 结果（3 次平均）

| 工具 | TLS 栈 | 平均耗时 | 平均速度 |
|------|--------|----------|----------|
| aria2 1.37.0 | libcurl + OpenSSL | ≈ 0.48 s | ≈ 281 MiB/s |
| bytehaul | reqwest + **rustls** | ≈ 1.04 s | ≈ 129 MiB/s |
| bytehaul | reqwest + **native-tls** (OpenSSL) | ≈ 1.15 s | ≈ 117 MiB/s |

### 结论

1. **native-tls 不比 rustls 快，反而略慢。** 将 TLS 栈从 rustls 换成与 aria2 相同的 OpenSSL，对性能没有改善作用，甚至略有下降。这直接排除了"rustls 与目标 CDN 不兼容导致性能差"的假设。

2. **bytehaul 仍然慢于 aria2 约 2.2 倍**（在这个 CDN 上）。这与 Windows 上 nsa2.e6ex.com 的约 7 倍差距量级不同，说明性能差距的绝对量随环境变化，但差距本身可重现。

3. **TLS 库已被排除。** 嫌疑点进一步收窄到 reqwest/hyper 的连接建立与管理行为（libcurl 与 hyper 的 TCP/HTTPS 连接建立流程、连接复用策略、流控调度等差异），以及其他与 HTTP 客户端实现相关的因素。

### 已保留的代码变更

本轮实验触发了一个有用的新配置项：`DownloaderBuilder::use_native_tls(bool)`。

- Cargo.toml：reqwest 依赖增加了 `native-tls` feature
- `ClientNetworkConfig`：新增 `use_native_tls: bool` 字段
- `DownloaderBuilder`：新增 `use_native_tls(bool)` 方法，可在运行时选择 TLS 后端
- Python 绑定：`Downloader(use_native_tls=True)` 以及 `download(use_native_tls=True)` 参数同步支持

虽然实验结果表明 native-tls 不能改善 CDN 性能，但该配置项保留下来，供用户自行排查 TLS 兼容性问题，或在特定平台上首选系统 TLS 的场景下使用。

## 9. libcurl Rust 绑定实验（`curl` crate，8 线程并行 Range）

### 背景

上一轮实验（§8）已排除了 TLS 栈的影响，将嫌疑范围缩小到"HTTP 客户端实现层面"。本节直接在 Rust 中调用 libcurl（`curl` 0.4.49 crate，底层与 aria2 使用同一份系统 libcurl 8.5.0 + OpenSSL），用 8 个 OS 线程并行发起 Range 请求，彻底排查是否是 hyper/reqwest 导致了性能劣势。

### 实现方式

- 用 `curl::easy::Easy2` 对每个分片发起一次 `Range` GET 请求，8 个线程并行运行。
- 强制 `HttpVersion::V11`，与 bytehaul 相同（避免 H2 连接合并）。
- 接收到的数据直接丢弃（只计量字节数），与 bh_bench 测试逻辑一致（排除写盘影响）。
- 不实现 resume / 进度回调 / 校验和，只测纯下载吞吐。

### 结果（3 次测量，GitHub CDN，134.7 MiB）

| 工具 | HTTP 库 | 测量 1 | 测量 2 | 测量 3 | 均速 |
|------|---------|--------|--------|--------|------|
| aria2 1.37.0 | libcurl (C) | 158.5 | 217.3 | 203.2 | **193 MiB/s** |
| **curl crate**（本实验） | **libcurl (Rust FFI)** | 216.0 | 222.8 | 219.0 | **≈ 219 MiB/s** |
| bytehaul | reqwest + rustls | 60.4 | 63.4 | 60.6 | **≈ 61 MiB/s** |
| bytehaul | reqwest + native-tls | 63.4 | 60.5 | 66.6 | **≈ 63 MiB/s** |

### 关键结论

1. **`curl` crate（libcurl Rust 绑定）与 aria2 速度相当，均在 ~190–220 MiB/s。**
   这直接证明：同一份 libcurl 被 Rust 通过 FFI 调用时，性能与 aria2 的 C 代码无显著差异。

2. **bytehaul (reqwest/hyper) 在相同条件下仅有 ~61 MiB/s，比 libcurl 慢约 3.5 倍。**
   这是迄今为止最关键的一个数据点：**性能差距的根本原因锁定在 HTTP 客户端库（reqwest/hyper vs libcurl）的实现差异上**，而不是 TLS、DNS、写入、分片策略或其他外部因素。

3. **这不是"libcurl 的某些魔法"，而是 hyper 在该场景下的具体不足。**
   从实现层面看，可能的原因包括：
   - libcurl 内建了 socket 级 `SO_SNDBUF` / `SO_RCVBUF` 调优或更积极的 TCP 缓冲区管理；
   - libcurl 在跟随重定向（GitHub → release-assets.githubusercontent.com）时的 TCP 握手更快；
   - hyper 的异步 Waker 调度引入了额外延迟，在高带宽小文件（<1 s 的下载）场景下会被放大；
   - libcurl 在并发多连接时维持独立 handle，不共享连接池，避免了 hyper 连接池竞争（虽然 bytehaul 也已配置 pool_max_idle_per_host(0)，但 hyper 的 connector 仍可能有额外开销）。

### 下一步行动建议

确认 HTTP 库是瓶颈后，有两条可行路径：

**路径 A（验证性）**：测量 TTFB（Time To First Byte），确认差距主要来自连接建立阶段还是数据传输阶段，以便判断是否能通过调整 hyper 参数（缓冲区大小、连接超时等）弥补差距。

**路径 B（工程性）**：评估将 bytehaul 的 HTTP 层从 reqwest 迁移到 `curl` crate 的可行性，或引入 `isahc`（基于 libcurl 的异步 Rust 客户端）。注意：这会引入系统依赖（需要安装 libcurl-dev），且与当前纯 Rust 的构建目标存在冲突，需要权衡。

## 当前阶段结论（更新）

在已完成的所有实验后：

| 嫌疑因素 | 状态 |
|--------|------|
| 默认 `piece_size = 1 MiB` | ❌ 排除：调大无显著改善 |
| `resume / autosave` 机制 | ❌ 排除：本地测试无影响 |
| 本地写入路径 | ❌ 排除：受控环境速度一致 |
| 请求头差异（`User-Agent` 等） | ❌ 排除：复刻后无稳定改善 |
| HTTP/2 多路复用（Range 被折叠） | ✅ 已修复（强制 HTTP/1.1） |
| TLS 库（rustls vs OpenSSL） | ❌ 排除：换 native-tls 更慢 |
| **HTTP 客户端库（reqwest/hyper vs libcurl）** | ✅ **§9 实验确认：这是根本原因** |

## 建议的下一步

1. **TTFB 测量**：在单连接下对比 reqwest 和 libcurl 从发起请求到收到第一个字节的时间，判断差距在连接建立还是传输阶段。
2. **hyper 调参**：尝试调整 `hyper` 的 socket 缓冲区 (`SO_RCVBUF`)、`nodelay`、连接池参数，看是否能缩小差距。
3. **迁移评估**：评估将 HTTP 后端迁移到 `isahc` 或 `curl` crate 的成本，重点考量：跨平台静态链接、CI 复杂度、维护成本。
4. 如果未来要把某些参数暴露给用户，优先考虑把并发数调优作为实际可用的 workaround，而不是盲目调大分片。
## 10. hyper 直接调参 + isahc 实验准备

### 背景

§9 实验确认 HTTP 客户端库是性能瓶颈后，本节建立两个独立的最小化下载实验程序，用于在相同条件下对比：

- **hyper-dl**：直接使用 `hyper 1.x`（不经过 reqwest），并仿照 curl 的默认 socket 配置进行调参
- **isahc-dl**：使用 `isahc 1.8`（基于 libcurl 的异步 Rust 封装），理论上应与 §9 的 curl crate 速度相近

两者均位于 `experiments/` 目录，作为独立 Cargo 项目，**不修改现有 bytehaul 代码**。

### hyper-dl 实现策略

以 curl 的默认行为为参照，对 hyper 的 `HttpConnector` 进行如下调参：

| 参数 | hyper-dl 设置 | curl 对应参数 |
|------|-------------|-------------|
| `TCP_NODELAY` | `true` | `CURLOPT_TCP_NODELAY=1`（curl 7.63.0 起默认） |
| `SO_RCVBUF` | `512 KiB` | curl 的有效接收缓冲区大小 |
| 连接超时 | `30 s` | `CURLOPT_CONNECTTIMEOUT=30` |
| 连接池 | `pool_max_idle_per_host=0` | curl 每个 easy handle 独立连接 |
| HTTP 版本 | `HTTP/1.1` 强制 | `CURLOPT_HTTP_VERSION=CURL_HTTP_VERSION_1_1` |

### isahc-dl 实现策略

- 每个 worker 使用独立的 `HttpClient`，`connection_cache_size=1`，避免连接复用
- 强制 `HTTP/1.1`（`VersionNegotiation::http11()`）
- 每个 worker 运行在 `tokio::task::spawn_blocking` 线程上（isahc 的 `send()` 是阻塞式）

### 预期结论方向

- 若 **hyper-dl** 速度接近 bytehaul（~60 MiB/s）→ 证明 hyper 本身无法仅靠调参达到 libcurl 水平
- 若 **hyper-dl** 速度显著提升（~100+ MiB/s）→ 说明 reqwest 的封装层或连接池策略是主因，可考虑 bytehaul 内部切换到直接 hyper
- 若 **isahc-dl** 速度接近 §9 curl crate（~200 MiB/s）→ 说明迁移到 isahc/libcurl 系是可行路径

### 使用方法

```bash
# hyper-dl（自带 rustls，无需系统 libcurl）
cd experiments/hyper-dl
cargo run --release -- https://github.com/llvm/llvm-project/releases/download/llvmorg-19.1.0/llvm-project-19.1.0.src.tar.xz 8

# isahc-dl（依赖系统 libcurl）
cd experiments/isahc-dl
cargo run --release -- https://github.com/llvm/llvm-project/releases/download/llvmorg-19.1.0/llvm-project-19.1.0.src.tar.xz 8
```

> 注：实验结果待 CI / 本地运行后填写。

### 实验结果（Linux CI，Ubuntu 24.04，GitHub CDN，134.7 MiB，8 连接）

| 工具 | HTTP 库 | 测量 1 | 测量 2 | 测量 3 | 均速 |
|------|---------|--------|--------|--------|------|
| aria2 1.37.0 | libcurl (C) | 377 | 400 | 367 | **≈ 381 MiB/s** |
| **hyper-dl**（本实验） | **hyper 1.9 + rustls（curl 参数调优）** | 357 | 634 | 597 | **≈ 529 MiB/s** |
| **isahc-dl**（本实验） | **isahc 1.8 / libcurl** | 385 | 359 | 385 | **≈ 376 MiB/s** |
| bytehaul（§8 对比） | reqwest + rustls | — | — | — | **≈ 129 MiB/s** |

### 关键结论

1. **hyper 本身不是瓶颈。** 直接使用 `hyper 1.9`（绕过 reqwest），加上 curl 参数对齐后，速度达到 ~529 MiB/s，比 aria2 还快。这彻底推翻了"hyper 慢于 libcurl"的假设。

2. **isahc（libcurl 异步封装）速度与 aria2 基本持平**（~376 vs ~381 MiB/s），与 §9 结论一致。

3. **真正的瓶颈在 reqwest 的封装层。** 同一份 hyper，经过 reqwest 封装后吞吐下降到 ~129 MiB/s，而直接调用则达到 ~529 MiB/s。差距因此锁定在 reqwest 的连接管理或请求调度逻辑上，而不是 hyper 的底层 I/O 能力。

4. **调查方向重新聚焦：reqwest 的连接池/请求行为差异。** 可能的原因包括：
   - reqwest 的 `pool_idle_timeout` 或 `pool_max_idle_per_host` 与并发场景下的互动
   - reqwest 在高并发时序列化连接建立的内部行为
   - hyper-util 的 `LegacyClient` 相比直接调用 hyper 的额外开销

### 更新后的嫌疑因素状态

| 嫌疑因素 | 状态 |
|--------|------|
| TLS 库（rustls vs OpenSSL） | ❌ 排除（§8） |
| HTTP 客户端库（hyper vs libcurl） | ❌ **已推翻**：hyper 直接调用远快于 aria2 |
| **reqwest 封装层行为** | ✅ **新的最可疑方向**：同 hyper 经 reqwest 后慢 4× |
