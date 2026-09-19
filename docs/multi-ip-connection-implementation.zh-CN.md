# 多 IP 建连实现（M1–M3）：开关、选路、采样与收敛

日期：2026-09-19。状态：M1–M3 已实现，默认关闭；M4 的本地兼容性验证已完成，
公网复测**未执行**（本机无公网环境），结论与未覆盖项见第 7 节。

本文记录 [多 IP 建连与连接复用收敛计划](multi-ip-connection-plan.zh-CN.md) §5–§9 的落地：
模块划分、内部参数、可观测性，以及每一项验收对应的测试。M0 的机制原型与实测结论见
[M0 实验结论](multi-ip-connection-m0.zh-CN.md)。

## 1. 启用方式与默认行为

```rust
let downloader = bytehaul::Downloader::builder()
    .multi_ip(true)
    .build()?;
```

Python：

```python
download(url, multi_ip=True)
Downloader(multi_ip=True)
```

* 默认 `false`：所有请求走原路径（`CURLOPT_RESOLVE` 注入整个答案，libcurl 自行选地址），
  行为与实现前完全一致。
* 开启后只有**直连 HTTP/HTTPS、域名 URL** 走新路径：代理请求仍由代理解析源站，
  字面 IP URL 本来就有明确目标，两者都不进入策略。
* 开关进入 `ClientNetworkConfig`，因此它是客户端缓存身份的一部分：只差这一个开关的
  两个 downloader 不会共享 client，也就不会共享策略历史（一个 driver 属于一个配置）。
* 不新增任何探测请求、不新增分片、不改变分片划分；候选只由正常下载请求覆盖。

## 2. 模块与数据流

| 位置 | 职责 |
| --- | --- |
| `src/network/curl/ip_policy.rs` | 策略状态机：候选表、预留、冷却、排名、迟滞、探索、采样判定。**不依赖 libcurl 和 Tokio**，所有方法显式接收 `now`，可用可控时钟测试 |
| `src/network/curl/driver/mod.rs` | 在 driver 线程内持有策略；为每个 transfer 选一个地址并写成 `CURLOPT_CONNECT_TO`；结算每次尝试；统计窗口；有界串行回退 |
| `src/network/curl/transport.rs` | 判定本跳是否走策略；从一次 Hickory 答案构造候选快照（含绝对有效期） |
| `src/network.rs`、`src/manager.rs` | 客户端级开关、默认值、builder |
| `bindings/python/src/lib.rs` | `multi_ip` 参数（构造函数与顶层 `download`） |

一次直连请求的顺序：

1. transport 解析域名，构造 `CandidateSet { host, port, addresses, valid_until }`；
2. driver 合并快照（代次、现存地址、历史），检查有效期，**原子地**选一个地址并占用；
3. 该 transfer 带上 `CURLOPT_CONNECT_TO`；URL、`Host`、SNI、证书校验都保持原样；
4. 完成 / 取消 / 提交失败各自结算一次占用；实际连接地址与所选地址不符时记录异常，不记到所选地址名下；
5. 消费者读完正文（或丢弃）后，driver 把窗口折进该地址的排名。

## 3. 选择规则与参数

规则见计划 §7；实现参数都在 `ip_policy.rs` 顶部，逐条注明依据：

| 常量 | 值 | 作用 |
| --- | --- | --- |
| `PREFERRED_SET` | 2 | 后续请求主要使用前两个地址 |
| `MIN_SAMPLES` | 3 | 达到样本数才参与排名 |
| `MIN_WINDOW` / `MIN_SAMPLE_BYTES` | 200 ms / 64 KiB | 太短太小的窗口不构成样本 |
| `MAX_BACKPRESSURE_RATIO` | 10 % | 背压占比超过该值即丢弃窗口（不做时间扣减） |
| `EWMA_ALPHA` | 0.3 | 新样本权重 |
| `SAMPLE_TTL` / `SAMPLE_HALF_LIFE` | 60 s / 30 s | 样本过期与衰减 |
| `SWITCH_GAP` / `MIN_DWELL` | 25 % / 5 s | 迟滞：小幅更快不切换，切换有最小驻留 |
| `EXPLORE_PERIOD` | 16 | 每 16 次选择做一次低频探索（用计数器而非随机，保持可复现） |
| `CONNECT_FAILURE_COOLDOWN` | 3 s | 连接失败的短期冷却 |
| `REMOVED_HISTORY` | 120 s | DNS 移除后的历史保留 |

选择顺序：单候选 → 直接使用；未被尝试过的候选 → 先覆盖；否则在有效候选中按在途占用分散
（不为主选等待而饿死其他请求）；到期触发探索；全部被冷却时做有界回退。

只有**建连失败**（且没有已发布的响应头或已发送的 HTTP 请求字节）才会冷却地址并触发串行回退；
429/503、`Retry-After`、TLS 校验失败、正文错误、用户取消都各自处理，从不靠换地址绕过。
代理 IP 不参与源站评分。

## 4. 预算、回退与生命周期

* **任务额度**：内部回退发生在一次 `driver.get` 之内，不回到 session，不重新申请
  slot/额度，`submitted` 只加一次 —— 会话看到的仍然是"一次请求"。
  `DriverStats::connect_retries` 单独记录多花的连接数。
* **期限**：回退不重置任何期限；从首次尝试开始，连接阶段用
  `min(connect_timeout, 剩余响应头预算)`，并把剩余预算按剩余尝试次数切片
  （下限 2 s，但不超过剩余预算，见 `connect_slice`），给其他候选留机会。
  后续切片使用原始配置上限，不把前一次切片误当作配置。
* **次数**：`min(候选数, MAX_INTERNAL_CONNECT_ATTEMPTS = 3)`。
* **安全条件**：错误 5/6/7/35 需要同时满足未发布响应头、`CURLINFO_REQUEST_SIZE = 0`。
  超时错误 28 还要求 HTTP 的 `CONNECT_TIME` 或 HTTPS 的 `APPCONNECT_TIME` 为零，
  证明 TCP / TLS 尚未完成；缺失信息时不回退。不能使用 `PRETRANSFER_TIME`，因为
  libcurl 8.21 在提前失败进入 COMPLETED 时也会补填它。其他 IP 的空闲连接不影响判断。
* **空闲记账**：策略连接不在提交时猜测领取哪条 socket。每轮 `perform` 后用
  `CONN_ID` 移除实际在用的记录，完成时只更新同一 ID；缓存清理时清空记录并换代。
  同 IP 的其他连接和其他 IP 的连接都保留原期限，池短暂完全空闲时也按最早期限回收。
  libcurl 内部淘汰而未报告的记录可能使清理提前；若无法取得 ID，则保留最早未知期限，
  采用保守回收，不让新请求推迟它。
* **取消**：取消只释放占用，不冷却地址；提交失败同样只释放。
* **响应头前失败**：最终失败直接删除 `AttemptRecord`；此时尚未创建 `BodyStream`，
  不能等待不存在的 `BodyDone`。内部回退保留同一记录，直到最终完成或取消。
* **DNS 代次**：旧代次的结算只释放自己的占用，不会把已移除的候选重新激活。

## 5. 采样与归属（M2）

* 窗口 = 写回调**实际接受**的第一个字节到最后一个字节（单调时钟），包含等待远端数据的时间。
  暂停期间没有字节被接受，因此暂停时间天然落在窗口内。
* `BodySink` 另外累计"因消费者落后而暂停"的时长；窗口里背压占比超过 10 % 即整窗丢弃，
  并计入 `ip_polluted_samples`。**不做时间扣减**，不用 `SPEED_DOWNLOAD_T`，也不用整次下载平均速率。
* 两层终态在 driver 内合并：libcurl 的结果（driver 线程）与消费者的结果
  （`Command::BodyDone { consumed, sample }`）。只有"正常结束 + 正文被读完"才产生样本；
  取消、hedge 败者、正文被丢弃、实际地址与所选地址不符，都不计入。
* pause 重投的字节不会重复计数：`accepted_bytes` 只在写回调真正接受时累加，
  pause 的 chunk 由 libcurl 重投，第二次才被接受。
* 限速 / 内存等待 / 写背压都表现为消费者侧背压，因此都会被"背压占比"捕获。

## 6. 可观测性

* TRACE：`selected a candidate address`（pool、地址、代次、选择理由、第几次尝试）。
  DEBUG：连接失败后的回退、实际地址与所选地址不符、无候选可回退。
* `DriverStats` 新增：`ip_selections`（被固定到选定地址的 transfer 数）、
  `connect_retries`（为同一请求多花的连接数）、`ip_samples`（记账的有效窗口）、
  `ip_polluted_samples`（因本地约束被丢弃的窗口）。策略关闭时前两项恒为 0。
* 日志不输出代理凭据；地址级统计不冒充任务级数字。

## 7. 验收与未覆盖项

### 已验证（本地、可复现）

| 验收点 | 位置 |
| --- | --- |
| 定向、A→B→A 复用、并发不串目标、`Host`/SNI/证书仍基于 URL、IPv6 目标 | M0 实验 1/3/4/8/9/10；`driver::tests::policy_transfers_land_on_the_address_the_driver_chose` |
| `CURLINFO_CONN_ID` 作为连接身份、跨缓存重号 | M0 实验 2 |
| 覆盖 → 分散 → 排名 → 迟滞 → 衰减 → 探索 → 冷却 → 回退（可控时钟） | `ip_policy::tests`（18 项） |
| 有界回退：一次请求、一条 `connect_retries`、会话只看到一次提交 | `driver::tests::a_refused_candidate_is_retried_without_a_second_request` |
| 其他 IP 的空闲 socket 不禁止回退；健康 socket 继续复用 | `driver::tests::policy_regression_another_ips_idle_socket_does_not_disable_fallback` |
| TLS 握手停滞时首轮切片超时，原预算内回退成功 | `driver::tests::policy_regression_stalled_tls_falls_back_within_the_original_budget` |
| A 忙 B 空闲、同 IP 两条连接只复用一条，未用连接按期回收 | `driver::tests::policy_regression_busy_a_does_not_refresh_idle_b`、`policy_regression_same_ip_reuse_keeps_the_unused_socket_deadline` |
| 单次失败 / 回退耗尽 / 证书失败均清理无正文的尝试记录 | `driver::tests::policy_regression_pre_head_failures_release_attempt_records` |
| 回退不重置调用方期限 | `driver::tests::an_internal_retry_never_resets_the_callers_deadline` |
| 正文错误不换地址 | `driver::tests::a_body_failure_is_not_retried_inside_the_driver` |
| 过期快照不开始新选路 | `driver::tests::an_expired_candidate_snapshot_is_refused` |
| 缓存上界全 origin 共享，不随 IP 数膨胀 | `driver::tests::two_addresses_share_one_cache_bound` |
| 完整消费才产生样本、丢弃正文不产生样本 | `driver::tests::a_consumed_body_ranks_its_address_and_an_abandoned_one_does_not` |
| 背压污染的窗口被丢弃 | `driver::tests::a_window_stretched_by_backpressure_is_dropped` |
| 开关关闭保持原路径；代理与字面 IP 不进策略 | `transport::tests::{the_default_path_still_injects_resolved_addresses, multi_ip_leaves_proxied_hops_on_the_resolve_path, multi_ip_does_not_choose_for_ip_literal_urls}` |
| 开了开关仍下载同一份字节（含恢复、限速、双任务） | `tests/m14_multi_ip.rs`（4 项） |
| 开关进入客户端缓存身份 | `manager::tests::test_downloader_builder_multi_ip_policy` |

`cargo test -p bytehaul` 全绿；`cargo clippy --workspace --all-targets -- -D warnings` 无告警。

### 未覆盖

1. **公网复测（M4）未执行**。计划要求用指定 Citron ZIP 做固定 IP / 交替开关的对比，
   本机没有公网环境，因此**没有**任何公网提速结论，也不把公网吞吐写成断言。
2. **端到端收敛只由算法测试覆盖**。快/慢地址的真实收敛需要 ≥ `MIN_DWELL`(5 s) 的墙钟时间，
   放进门禁会显著拖慢套件；因此策略算法用可控时钟验证，driver 侧验证的是"样本确实流入策略"
   与"选择确实落到 `CONNECT_TO`"这两个接缝。
3. **libcurl 自身重放**：driver 用请求字节数阻止已发送请求的内部回退，但 libcurl
   自身存在缓存 socket 失效后的重连 / 重放行为。bytehaul 当前只发 GET；若将来增加
   非幂等方法，必须重新审查整条路径（`is_safe_connect_failure` 的注释）。
4. **物理连接峰值**只在 driver 测试里用服务器侧 `peak_live` 观察（M0 实验 3/7 与
   `max_connects_prunes_idle_sockets_after_concurrent_transfers`），没有做 socket 级事件采集。
5. **HTTP/2、HTTP/3** 不在范围内：本次仍是 HTTP/1.1，流与连接的关系需要另行设计。
