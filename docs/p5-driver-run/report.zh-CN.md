# P5 多 pool 等待修复报告

2026-09-14，macOS aarch64（Darwin 25.5.0），Rust 1.96.0，`bench`（release-like）配置，
全部夹具在本机回环。对照运行：修改前 driver 保持旧的“只等待 `pools.values().next()` 选出的
那一个 pool、且用 `Multi::wait`”实现；修改后为 P5 改动（只等待有活动传输的 pool、
`Multi::poll` 有界等待、命令经 wakeup socket 打断等待）。两次运行都建立在 `0862100`
之上、含 P4 的未提交改动（报告中的 `worktree` 为 `dirty`），场景与轮数完全相同。

复现命令（本机设置了对内网代理的 `ALL_PROXY`/`http_proxy` 等变量，运行前清除）：

```bash
cargo bench --bench pipeline_bench -- --filter driver --rounds 10 --out target/p5-before
cargo bench --bench pipeline_bench -- --filter driver --rounds 10 --out target/p5-after
```

原始数据：[修改前逐轮样本](driver-before-samples.csv)、[修改后逐轮样本](driver-after-samples.csv)、
[修改前完整报告](driver-before-report.md)、[修改后完整报告](driver-after-report.md)。

## 1. 缺陷

“停滞 pool + 空闲 pool”场景（一次已完成的下载留下空闲连接池，另有一个传输等待慢 origin
的响应头）：等待时长取所有 pool 超时的最小值，但等待动作只发生在 `HashMap` 迭代顺序选出的
一个 pool 上。选中空闲 pool 时 `curl_multi_wait` 立即返回（无传输 ⇒ 无描述符、无内部超时），
循环空转。修改前逐轮“静止期循环/秒”呈双峰（同一进程内 5 轮高档、5 轮低档）：

| 轮 | 0 | 1 | 2 | 3 | 4 | 5 | 6 | 7 | 8 | 9 |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 修改前 | 611521 | 46 | 46 | 50 | 50 | 611846 | 614729 | 618800 | 509110 | 50 |
| 修改后 | 43 | 40 | 43 | 46 | 53 | 40 | 40 | 40 | 46 | 40 |

修改后每一轮都落在 20 ms 分片一级（约 40–53 次/秒），P1 §4.4 的修复判据满足。

同一缺陷也把**所有命令和 Resume 的排队延迟钉在分片上限**：driver 阻塞在某个 pool 的
`wait` 里时，命令队列的 condvar 通知没有等待者（`sent` 只通知 condvar，不打断 libcurl
等待），命令只能等 `wait` 超时返回。修改前 `max_command_latency` 在多个场景稳定在
**≈25 ms**；body 背压场景（256 KiB 预算、55 次 pause/resume）的总耗时中位数 **758.9 ms**
正是约 55 × 14 ms 的 Resume 等待，而不是数据传输时间。

## 2. 修复

`src/network/curl/driver/mod.rs` 第 5 步（等待）与命令通道（`CommandQueue`）：

- 等待目标只在**有活动传输的池**中选择；空闲池没有可等待描述符，选中它必然立即返回。
- 使用 `Multi::poll`（`curl` crate 的 `poll_7_68_0` feature，静态 libcurl 8.21.0 提供）：
  没有可等待描述符时也按传入超时有界等待；保留 `min(libcurl timer, 20 ms)` 的时长上限，
  未缩短轮询间隔。
- 多个活动池之间优先选择还能产生 socket 事件的池：写回调暂停的传输会被 libcurl 摘除读
  兴趣（`Curl_req_want_recv` 为假），全暂停的池只有超时可等；该偏好避免另一个有数据的
  池屈服于分片级节流。判定用 `BodySink` 的 `paused_for_wait` 原子镜像（非权威状态，
  权威暂停状态仍在 `SinkState`）。
- `CommandQueue` 增加 `poll_waker` 槽位：等待前登记当前池的 `Multi::waker()`，`send`/
  `wake`/`close` 都调用 `wake_poll()` 经 wakeup socket 打断 `poll`。评审修复后，
  登记 waker 后重查命令队列及关闭状态，有待处理状态就跳过 `poll`；检查后到达的
  命令或关闭通过已登记的 waker 唤醒。粘性 wakeup 只适用于实际调用了 wakeup 的情况，
  原测量版本仍存在先入队、后登记时额外等待一个分片的竞态，不能从上述样本推断其不存在。
- 不采用完整事件机制（socket 回调聚合多个 `Multi`）：测量显示简单修复已消除空转并把
  命令延迟降到 µs 级，跨池事件仍受“一次只等待一个 Multi”的既有约束，见限制一节。

## 3. 对照结果

driver 组中位数（括号内为 p25–p75，10 轮）：

| 场景 | 指标 | 修改前 | 修改后 |
| --- | --- | ---: | ---: |
| stalled_pool_with_idle_pool | 静止期循环/秒 | 254580.1 | 41.4 |
| stalled_pool_with_idle_pool | 静止期循环次数 | 76741 | 12.5 |
| stalled_pool_with_idle_pool | cpu_ms | 142.367（7.732–309.736） | 9.375（9.214–10.160） |
| stalled_pool_with_idle_pool | 命令最大延迟 ms | 9.098（0.081–19.223） | 0.208（0.176–0.272） |
| stalled_pool_with_idle_pool | 取消 ms | 15.221（14.247–15.629） | 16.666（14.369–17.227） |
| body_backpressure_256KiB_budget | total ms | 758.930（732.639–802.952） | **21.240（15.286–37.599）** |
| body_backpressure_256KiB_budget | cpu_ms | 57.056（50.159–62.018） | 18.131（17.536–19.623） |
| body_backpressure_256KiB_budget | 命令最大延迟 ms | 25.081（25.035–25.209） | 0.177（0.152–20.491） |
| single_origin_split_download | total ms | 120.334（87.131–162.608） | 13.180（12.511–13.456） |
| single_origin_split_download | cpu_ms | 27.005（25.743–32.990） | 13.437（12.923–14.282） |
| single_origin_split_download | 命令最大延迟 ms | 24.993（24.712–25.031） | 0.096（0.052–0.183） |
| idle_pool_loop_rate | 静止期循环 | 0 | 0 |
| idle_pool_loop_rate | total ms（300 ms 窗口） | 302.120（301.557–302.294） | 302.019（301.674–302.093） |
| two_origins_one_idle_pool | total ms（300 ms 窗口） | 301.924（301.412–302.102） | 301.841（301.706–302.158） |
| two_origins_one_idle_pool | cpu_ms | 56.858（49.483–67.874） | 48.196（46.388–53.061） |
| cancel_latency | 取消 ms | 13.710（11.413–14.635） | 13.246（11.221–15.578） |

背压场景逐轮 total（ms）：

| 轮 | 0 | 1 | 2 | 3 | 4 | 5 | 6 | 7 | 8 | 9 |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 修改前 | 710.1 | 795.8 | 974.9 | 736.8 | 805.3 | 776.0 | 731.3 | 828.3 | 741.8 | 686.5 |
| 修改后 | 20.6 | 15.4 | 41.0 | 37.4 | 14.5 | 15.2 | 14.6 | 199.5 | 37.7 | 21.9 |

结论：

- 停滞+空闲场景的空转双峰消失（每一轮都在 20 ms 档），其 cpu_ms 从带高档离群的
  142 ms 降至 9.4 ms 且分布收窄。
- Resume/命令延迟从分片级（≈25 ms）降到 µs 级，背压场景 total 中位数下降 **36 倍**；
  修改后仍偶见单轮 199.5 ms（主机噪声，IQR 15.3–37.6）。取消延迟中位数持平
  （13.7 → 13.2 ms，取消路径的耗时在 session 侧收尾）。
- 空闲窗口场景保持 0 次静止期循环；两个 origin 的场景 total 不变、cpu 降低；
  没有出现超过计划 10% 调查门槛的中位耗时或 CPU 回退。

## 4. 行为回归测试

- `a_stalled_transfer_next_to_an_idle_pool_keeps_the_loop_bounded`：复刻 P1 §4.4 的
  形状（一个已完成下载留下的空闲池 + 一个 500 ms 延迟响应头的传输），在 200 ms 停滞
  窗口内断言循环次数有界。对修改前实现运行 4 次：2 次以约 105k–108k 循环失败（双峰），
  修改后连续 6 次全部通过且每轮循环约 10 次。
- `a_fully_paused_transfer_does_not_spin_the_driver`：所有 body 暂停（写回调因背压暂停、
  无读兴趣）时 200 ms 窗口内循环次数有界，之后续读仍逐字节正确。修改前后均通过
  （该形态在修改前已由 libcurl 的 wakeup socket 保持有界），作为不变式固定。
- 三项 `prepare_poll` 竞态边界单测：登记 waker 前已入队的命令或已关闭状态跳过 `poll`
  且不留悬挂 waker；登记之后到达的命令与关闭经 wakeup 让下一次 `poll` 立即返回
  （在 2 s 超时内 < 1 s 返回）。
- 既有 51 项 driver 单元测试（含暂停/恢复、慢消费者、取消、idle 回收、驱动释放）全部通过。

## 5. 全量场景对照

同机 `cargo bench` 全量 52 个场景（scheduler 18、storage 9、e2e 7、driver 6、client 6、
writer 5、writer_stop 1）× 10 轮，修改前后各一次；除 driver 等待实现外两次运行的树内容
相同。逐轮样本：[修改前](full-before-samples.csv)、[修改后](full-after-samples.csv)；
完整报告 [修改前](full-before-report.md)、[修改后](full-after-report.md)。

- **空转判据在全量运行中同样满足**：停滞+空闲场景修改前 10 轮中 8 轮落在 57 万–63 万次/秒
  档（其余 46–50 次/秒），修改后 10 轮全部为 40–53 次/秒。
- **下载类场景普遍受益于“恢复/命令不再等分片”**：`e2e/large_64MiB_4conns` 中位 total
  774.8 → 30.4 ms，其中库内合计 `body_wait_ms` 1372 → 16.4 ms。旧实现下每个
  预算回填周期（body 暂停 → 消费者排空 → Resume 命令）都要等一次约 20 ms 的 wait
  超时才能继续，64 MiB 下载的回填次数正好把 body 等待推到 1.3 s 量级；wakeup 上线后
  该等待消失。writer 组走同一下载管线：12 MiB split 由约 117 ms 降到 11 ms；
  `e2e/unknown_length_12MiB` 103 → 14.5 ms，`e2e/split_12MiB_4conns` 120 → 19.7 ms。
  断流与慢尾形态由夹具节奏主导，基本不变（+2.7% / −6.9%）。
- **无超过计划 10% 调查门槛的中位回退。** 唯一到线的是 `e2e/small_256KiB_single_connection`
  （9.05 → 10.02 ms，+10.7%）：20 轮复测中位 9.55 ms（p25–p75 9.36–9.62），落在两次运行的
  波动带内，且该形态只有 1 个请求、几乎没有暂停/恢复周期，按主机噪声处理。
- scheduler/storage 微基准（本改动不涉及）的差异均在噪声范围（多数 ±5% 内，绝对值为
  µs–ms 级）。

## 6. 限制

- 同机同配置对照，只比较本机驱动的循环数、CPU、命令与取消延迟，不宣称跨平台吞吐。
- 一次等待仍只覆盖一个 `Multi`：多个活动池并存时，未被选中的池其 socket 事件最多延后
  一个 20 ms 分片被处理。测量未显示该约束造成回退（两个 origin、停滞+空闲场景的
  total/cpu 均未变差），因此未进入“聚合活动 socket”的完整事件机制改造；若未来需要，
  `poll + wakeup` 已是其前置条件。
- `cpu_percent` 含夹具与基准自身，是本下载路径的上界；本轮未采集 release 单场景吞吐或
  RSS 峰值。

### 评审修复：登记前的唤醒竞态

等待前新增 `prepare_poll`，先登记 waker，再检查 pending commands/closed。新增三项回归测试覆盖登记前 Resume/Cancel、登记前关闭，以及登记后、进入 poll 前的命令/关闭唤醒。原始性能样本未重新采集，不作为此修复版本的测量结果。

修复后本机验证：Rust 库测试 535 通过 / 3 忽略、集成测试 105 通过；fmt、diff check、workspace 全目标 Clippy（`-D warnings`）、doc test 和 workspace rustdoc（`-D warnings`）通过。本次未重跑性能基准、Python 测试或 Linux 覆盖率。

**最终 CI（2026-09-14，`5a4f274`）**：三平台 Rust 测试、Python 绑定测试与 Linux 覆盖率门槛（95.29%，7700/8081 行）全部通过，详见[计划文档的最终 CI 验证](../simplification-and-optimization-plan.zh-CN.md#最终-ci-验证2026-09-14)。上一段的"未重跑"只描述该修复轮次当时的状态；本报告的驱动对照仍是本机 release 测量，与 CI 的 debug 测试构建属于不同证据类别。
