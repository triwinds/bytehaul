# Rust libcurl 公网对照实验

日期：2026-09-12。

Rust 有 [`curl`](https://docs.rs/curl/latest/curl/)（curl-rust）crate。它是
libcurl C 库的 Rust 绑定，底层 FFI 由 `curl-sys` 提供，并不是另一套纯 Rust
HTTP 实现。绑定的源码见 [curl-rust 仓库](https://github.com/alexcrichton/curl-rust)。

本记录把 libcurl 加入前一轮公网下载对照，用来观察 bytehaul、aria2 和 libcurl
在同一 URL 上的实际差异。此次没有把 libcurl 接入 bytehaul 生产后端，只编译了
独立测试程序。

## 构建与测试实现

测试程序使用：

- `curl` `0.4.50`；`curl-sys` `0.4.90+curl-8.21.0`。
- Cargo 特性 `static-curl`，静态编译 vendored libcurl；运行时报告为
  `libcurl 8.21.0-DEV`、TLS `Schannel`。
- 每个请求使用 libcurl `Easy` handle，固定 IPv4、禁用代理、使用与前一轮相同的
  `public-download-compare/1.0` User-Agent，并设置 30 秒建连、180 秒总请求期限。
- 单连接直接下载完整文件。8 路测试由 8 个 Rust 线程各持有一个 Easy handle，
  对文件发起 8 个固定 Range 请求，再按顺序合并；这不是 bytehaul 或 aria2 的
  动态分片策略，因此只作探索性对照。
- aria2 基线使用 `D:\py\ns-emu-tools\src-tauri\target\release\aria2c.exe`
  （版本 `1.37.0`）。

目标 URL：

```text
https://nsa2.e6ex.com/gh/citron-neo/CI/releases/download/nightly-windows/Citron-windows-nightly-91bbce723-x64-clangtron.zip
```

文件大小为 `64,469,455` 字节。成功产物的 SHA-256 均为：

```text
2132eda841919e4de7c8a77a0a47ab9f117310a785562905b932bebc141c2c7c
```

ZIP 内部 CRC 也逐个通过。测试程序源码和原始输出保存在本机的
`target/libcurl-bench/`、`target/public-compare/20260912-174000-libcurl/` 及
`target/public-compare/20260912-174000-libcurl-ip/`；这些 `target` 目录不纳入版本控制。

## 单连接结果

bytehaul 与 aria2 的基线来自本机公网对比运行
`target/public-compare/20260912-171925/report.md`，
每个工具 3 轮，计时包括各自进程启动、下载落盘和详细日志写入。libcurl 的计时从
测试程序进入传输函数后开始，未开启调试日志，因此下表用于判断量级和方向，不能
当作完全同条件的吞吐基准。

| 客户端 | 连接数 | 成功轮数 | 中位耗时 | 中位速度 |
| --- | ---: | ---: | ---: | ---: |
| bytehaul | 1 | 3/3 | 16.016 s | 3.839 MiB/s |
| aria2 1.37.0 | 1 | 3/3 | 9.253 s | 6.645 MiB/s |
| Rust libcurl Easy | 1 | 3/3 | 7.189 s | 8.552 MiB/s |

libcurl 三轮实际耗时为 `7.189 s`、`4.033 s`、`7.687 s`，三轮都连接到
`104.25.244.197`，HTTP 状态为 200。相对于这次 aria2 基线，libcurl 中位速度高约
29%；相对于 bytehaul 约为 2.23 倍。由于 IP、日志和时间窗口没有完全对齐，这个
差异不能直接归因于某一个实现细节。

## 8 路探索性结果

固定 8 个 Range 的成功样本如下：

| 轮次 | 结果 | 总耗时 | 速度 | 连接 IP |
| --- | --- | ---: | ---: | --- |
| 1 | 失败：对端重置（curl code 56） | — | — | `104.25.244.197` |
| 2 | 成功，8 个响应均为 206 | 2.198 s | 27.967 MiB/s | 全部 `104.25.244.197` |
| 3 | 成功，8 个响应均为 206 | 3.173 s | 19.374 MiB/s | 全部 `104.25.244.197` |
| 4 | 失败：180 秒超时；一个分段收到 4,177,920/8,058,682 字节（curl code 28） | — | — | — |

成功样本的文件哈希和 ZIP CRC 均正确。两次成功都比前一轮 8 连接 aria2
（中位 `4.256 s`）和 bytehaul（中位 `4.284 s`）快，但分片数量、分片大小、
重试策略和日志条件不同，不能据此判断 libcurl 的稳定多连接性能。

## 固定 IP 对照

为观察 IP 选择本身的影响，使用 libcurl `CURLOPT_RESOLVE` 将同一域名固定到两个
候选地址，各执行 3 次单连接：

| 固定地址 | 3 次耗时 | 中位耗时 | 中位速度 |
| --- | --- | ---: | ---: |
| `104.25.244.197` | 7.422 s、8.847 s、7.931 s | 7.931 s | 7.752 MiB/s |
| `172.64.153.74` | 3.795 s、6.604 s、9.157 s | 6.604 s | 9.310 MiB/s |

同一 IP 内仍有很大波动；本组固定测试中 `172.64.153.74` 反而略快于
`104.25.244.197`。因此“aria2 命中快 IP 的次数更多”可以是原因之一，但不能
单独解释单连接差异。CDN 节点当时的缓存、拥塞和路由，以及客户端的 HTTP/TLS
栈、缓冲和日志开销，都可能参与结果。

## 结论与后续复测条件

这次样本支持以下判断：

1. Rust 可以通过 curl-rust 使用完整 libcurl；在 Windows 上可以用 vendored
   libcurl + Schannel，避免依赖系统 libcurl 安装。
2. 在这条 URL 的本次单连接窗口内，libcurl 比 aria2 和 bytehaul 都快，但结果仍
   受 IP 和时间窗口影响。
3. IP 探索值得纳入多 IP 策略，但不能把一次命中地址永久标记为“快 IP”；同一地址
   的多轮速度差异已经足够大。
4. 若要继续做因果归因，应让三个客户端交错随机运行，统一进程启动计时、日志开关、
   IP 固定方式和重试规则，再比较同一 IP 上的分布，而不是只比较中位数。
