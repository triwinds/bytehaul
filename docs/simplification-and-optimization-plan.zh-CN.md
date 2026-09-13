# bytehaul 简化与优化实施计划

日期：2026-09-13。状态：实施中；P0、P1 已完成，并已按评审意见修正（见[P0/P1 评审修正记录](#p0p1-评审修正记录)），P2 实施中，后续阶段待实施。审查阶段已完成代码审查和四项行为问题的复现，B1–B4 已在 P0 修复并转为回归测试；P1 已完成默认路径与资源基线归档，并在评审后重新采集。

审查基线：`2161fd4388bec05ec7183605e88b7e1c2b939164`，bytehaul 0.2.4，Windows。P0/P1 已完成；后续阶段以各步骤完成记录为准。

## 1. 目标与判断原则

下载器的核心目标是：在资源消耗可控的条件下，把同一远端对象正确地写入文件，可靠地恢复中断，并及时响应停止请求。

据此，优先减少三类成本：

- 同一个生命周期或状态由多个模块分别维护，导致分支重复和结果不一致。
- 每个数据块经历不必要的复制、小块文件操作和额外磁盘读写。
- 线程、resolver、client 和连接池的数量随历史配置组合持续增长。

正确性问题先修复；性能候选先建立基线，再以数据决定实现。以可删除的执行分支、明确的状态归属和实际资源变化衡量简化，不以文件行数作为验收指标。

本计划承接现有 [架构说明](architecture.zh-CN.md)、[HTTP 传输计划](http-transfer-improvement-plan.zh-CN.md)、[Range 调度计划](range-scheduling-improvement-plan.zh-CN.md) 和 [libcurl 连接池语义](libcurl-pool-semantics.zh-CN.md)。这些历史计划的状态以当前实现为准；已有能力不重复立项。

## 2. 已核实事实与证据边界

### 2.1 已复现的行为问题

| 编号 | 触发条件 | 审查时实际结果 | 目标结果 | 现状 |
| --- | --- | --- | --- | --- |
| B1 | 下载结束后校验和不匹配 | `wait()` 返回 `ChecksumMismatch`，订阅进度仍为 `Completed` | 返回错误，最终状态为 `Failed`，不提前发布 `Completed` | 已修复（P0） |
| B2 | 使用空 URL 等非法配置提交下载 | `wait()` 返回 `InvalidConfig`，进度仍为 `Pending` | 返回错误，最终状态为 `Failed` | 已修复（P0） |
| B3 | 并发上限为 1，首任务占用名额，取消排队的第二个任务 | 第二个任务在 250 ms 观察窗口内未结束；释放前一个任务后才继续处理 | 停止结果不依赖其他下载释放名额 | 已修复（P0） |
| B4 | `max_concurrent_downloads(0)` | builder 构建成功 | 构建时返回明确配置错误 | 已修复（P0） |

相关代码：[manager.rs](../src/manager.rs) 的 `download`、`DownloaderBuilder::build`；[session/mod.rs](../src/session/mod.rs) 的 `run_download`；[single.rs](../src/session/single.rs) 的 `complete_single_transfer`；[multi.rs](../src/session/multi.rs) 的完成路径。

四项均已转为长期回归测试，见 [m13_lifecycle.rs](../tests/m13_lifecycle.rs)；这些测试覆盖公开 `wait()` 与 `subscribe_progress()` 的最终结果，并断言每个任务只发布一个终态。B3 的复现改用受控首任务保持占用，不再依赖长时间 sleep。P0 实施期间另有两处实现缺陷由新测试暴露并修复，见第 6 节。

审查期间运行了以下四个现有集成测试目标，共 19 项全部通过；执行时清除了子进程代理环境变量并使用 localhost：

```bash
cargo test -p bytehaul --locked --offline --test m6_features --test m8_pause_resume --test m11_flow_control --test http_header_timeout
```

这次结果只覆盖上述目标，没有执行完整覆盖率门槛或吞吐基准。

### 2.2 代码已确认、收益待测的候选

| 候选 | 当前实现 | 待验证影响 | P1 基线状态 |
| --- | --- | --- | --- |
| client 资源增长 | 完整 `ClientNetworkConfig` 作为无淘汰缓存的 key；不同连接超时可创建不同 client；每个 client 创建 resolver 和驱动线程 | 长期运行时的缓存数量、线程数和复用率 | 已量化：32 个不同超时 = 33 个缓存条目与 33 个驱动线程；同配置 40 次取用 = 1 条 / 1 线程；释放后线程归零 |
| worker 职责耦合 | `Coordinator::new_with_start` 仅在 `Disabled + Fixed + request_batch_size=0` 时返回 `None`，普通动态调度和批处理也依赖 adaptive 执行路径 | 两套 worker 的维护成本及重构后的行为一致性 | 未测（属结构问题，P3 用行为矩阵而非耗时判断） |
| 文件写入成本 | Windows 默认预分配先写零并同步；单连接逐块 `seek + write_all`；多连接缓存追加时复制数据 | 启动时间、CPU、文件操作次数和总完成时间 | 已量化：单连接 4 MiB = 256 次写块（平均 16384 字节）且不经写回缓存，split 12 MiB = 12 块 / 1 MiB；1 MiB 预算 = 48.5 块；预分配 12 MiB ≈ 8.0 ms 对按需增长 0.64 ms |
| driver 等待 | 推进所有 pool，却只等待 `pools.values().next()` 选出的 pool，未保证它有活动请求 | 多 origin 或暂停传输时的空转、命令延迟和事件处理延迟 | 已复现：停滞 pool + 空闲 pool 场景单进程逐轮双峰，静止期 41–47 对 44 万次/秒；纯空闲场景 0 次 |
| 配置与 CI 重复 | Rust/Python 多处声明和验证选项；唯一后端保留单分支包装；CI 完整运行两个等价 feature 组合 | 配置规则漂移、重复构建与测试时间 | 未测（P2/P6 按规则一致性和 CI 时长判断） |
| 基准与默认路径不一致 | scheduler 基准主要调用 `assign_to_with_split`，默认下载还经过动态候选与请求规划 | 默认策略的调度开销是否进入现有测量 | 已解决：`bench_scheduler_plan` 按自适应 worker 的方式驱动 `assign_request_with_diagnostics`；默认 Dynamic 与 Fixed 的请求数差异已可测（4096 piece：64 对 1024） |

driver 空转属于代码和接口语义推导，P1 已实测复现，见[基线报告 4.4](pipeline-baseline.zh-CN.md)：libcurl 在没有可等待描述符时会让 `curl_multi_wait` 立即返回。[官方说明](https://curl.se/libcurl/c/curl_multi_wait.html)

P1 的测量结果、复现命令与判据见[默认路径与资源基线](pipeline-baseline.zh-CN.md)。

## 3. 必须保持的不变量与兼容性

1. 每个响应继续验证状态码、范围、对象身份、编码和长度；重试或竞速不能混入另一个版本的对象。
2. piece 是存储和断点粒度，HTTP 请求与 lease 分别计量。保持有限 Range、probe 精确接管、空洞隔离及每请求 lease 上限。
3. 旧 producer 停止并经过 writer 确认后，才能保留前缀或回收其范围。迟到数据不能污染新 attempt。
4. UI 已接收字节、writer 已确认写入和持久化断点继续区分。控制文件只声明同步边界内的可靠进度，保留 V1/V2 读取兼容。
5. 重试次数、elapsed budget、Retry-After、恢复冷却和额外流量预算不能通过换 lease、切批或更换执行路径重置。
6. 已进入取消或暂停处理的任务仍完成必要的 writer 收尾；必要存储操作失败时返回实际错误，不能报告暂停成功或下载完成。
7. `memory_budget` 保持数据预算的既有含义，不宣称是进程 RSS 上限；缓存调整继续保证小预算下能推进且无循环等待。
8. TLS、代理、DNS 和连接池策略的隔离要求继续成立；淘汰缓存引用不能中止活动请求。
9. 保留 Rust builder、Python 既有参数名及位置参数顺序、默认调度策略和 `curl-backend` feature 入口。内部重组不顺带改变公开参数语义。

本轮不引入新传输后端、HTTP/2、多 IP 策略或新断点格式。逐步实现，避免一次重写整个下载器。

## 4. 阶段与依赖

| 阶段 | 优先级 | 交付目标 | 前置条件 | 状态 |
| --- | --- | --- | --- | --- |
| P0 | 最高 | 生命周期修复和 B1–B4 回归测试 | 无 | 已完成 |
| P1 | 高 | 覆盖默认路径的可复现基线 | 可先准备夹具；正式比较使用 P0 后基线 | 已完成 |
| P2 | 高 | 配置统一解析、请求超时与共享 client 分离、缓存有界 | P0；资源比较使用 P1 | 实施中 |
| P3 | 高 | 多连接传输循环统一，慢速恢复只负责策略 | P0、P1；配置结构复用 P2 | 待实施 |
| P4 | 中 | 减少预分配与 writer 管线成本 | P0、P1；与 P3 分开提交 | 待实施 |
| P5 | 中 | 修正多 pool 等待，按测量决定事件机制改造 | P1；在 P2 后验证资源生命周期 | 待实施 |
| P6 | 中，低成本 | 清理唯一后端包装和重复 CI | CI 去重可独立提前实施 | 待实施 |

### P0：统一生命周期、停止处理和终态

状态：已完成。

改动位置：[manager.rs](../src/manager.rs)、[session/mod.rs](../src/session/mod.rs)、[single.rs](../src/session/single.rs)、[multi.rs](../src/session/multi.rs)、[flow.rs](../src/session/flow.rs)、[checksum.rs](../src/checksum.rs)、[progress.rs](../src/progress.rs)。

实施步骤：

1. 将 B1–B4 加入正式测试，覆盖公开 `wait()` 和 `subscribe_progress()` 的最终结果；补回调不会提前因 `Completed` 停止的检查。
2. 在任务最外层建立统一结果处理路径，纳入配置拒绝、并发排队、client 构建、传输、writer 收尾和校验。子流程返回结果与必要进度，不独立发布公开终态。
3. 仅在 writer 收尾、必要清理及配置的校验全部成功后发布 `Completed`。校验期间继续使用现有非终态，不在这一步扩充公开状态枚举。
4. 获取并发名额时同时等待停止信号，检查等待前已发出的信号。排队暂停也能直接结束，未开始的任务不创建虚假的断点文件。
5. 复用统一的停止等待逻辑，覆盖请求、限速、内存和 channel 等待，并检查 watch sender 关闭后的分支；保持丢弃 handle 不自动取消下载的现有语义。
6. 在 builder 拒绝零并发上限。将校验循环纳入停止响应范围，停止与必要同步之间的优先级写入测试。

验收：B1–B4 全部通过；正常完成、错误、暂停、取消的最终快照与返回值一致，终态只发布一次；排队取消测试通过受控首任务保持占用来证明第二个任务独立结束，不能仅靠长时间 sleep 猜测。覆盖校验失败、writer 失败以及停止和完成竞争，保证原有持久化屏障不退化。

交付：生命周期改动、行为回归测试，以及进度/暂停文档的对应修订。先完成这一阶段，再改变传输执行结构。

### P1：建立默认路径与资源基线

状态：已完成，基线在评审修正后重新采集（见文末 P1 完成记录与[基线报告](pipeline-baseline.zh-CN.md)）。

改动位置：[storage_bench.rs](../benches/storage_bench.rs)、[lib.rs](../src/lib.rs) 的 bench 包装、现有本地 HTTP 夹具和 [公网比较入口](public-comparison.zh-CN.md)。按需要增加专门的本地管线基准。

基准矩阵：

| 对象 | 最小场景集合 | 主要指标 |
| --- | --- | --- |
| scheduler | 默认 Dynamic；Fixed 对照；连续未完成范围、断点空洞、恢复后缀；递增 piece 数 | 每次分配耗时、候选扫描成本、请求数与范围分布 |
| writer | 单/多连接；小块/大块；正常/极小内存预算；预分配开/关 | 完成时间、CPU、实际写入批次、seek 次数、flush/sync 时间 |
| client 生命周期 | 相同网络配置而不同超时；重复使用；大量不同配置后停止任务 | 缓存数、驱动线程数、连接复用、释放后的残留资源 |
| driver | 单/多 origin；活动与闲置 pool 混合；body 暂停；无活动描述符 | CPU、循环次数、命令响应延迟、取消延迟 |
| 端到端 | 小文件、大文件、未知长度、延迟响应、断流、慢尾 | 总耗时、失败率、有效/重复字节、峰值内存和输出校验 |

计时分开记录排队、预分配、响应头、body、最终同步和校验；总完成时间包含调用方真正需要等待的全部阶段。吞吐测量和详细日志诊断分轮执行。

每次结果记录 commit、工具链、平台、libcurl/TLS 特征、配置、计时边界和逐轮数据。正式比较至少保留十轮本地样本；小样本不报告稳定 P95。代表性结果及复现命令归档到 `docs` 下的配套报告，临时大文件保存在 `target`。

验收：现有默认下载实际经过的路径均可测量；结果能够区分减少请求、减少复制和减少磁盘操作带来的收益，不依赖公网速度断言 CI 成败。

### P2：收敛配置并限制共享 client 资源

状态：实施中；网络覆盖解析步骤已完成，资源复用及其余配置收敛继续实施。

改动位置：[config.rs](../src/config.rs)、[manager.rs](../src/manager.rs)、[network.rs](../src/network.rs)、[transport.rs](../src/network/curl/transport.rs)、[Python 绑定](../bindings/python/src/lib.rs)。

拆成两个可独立审查的提交：先配置解析，再资源复用。

- 内部按网络覆盖、重试、调度、恢复和存储职责组织配置。使用可选覆盖值替代值与 `overridden` 标志并存的状态，统一生成经过验证的生效配置。
- 保留公开 builder/getter 的现有行为；Python 负责单位、字符串枚举和类型转换，核心规则由 Rust 统一验证。保留现有关键字、位置参数、错误类别和优先级，避免改为无法检查的任意参数字典。
- 将 `connect_timeout` 等请求级参数从共享 client 身份中移出，经请求选项传递；DNS 与连接建立仍消耗同一连接期限，不能因拆分分别获得完整超时。
- client key 继续包含影响路由、TLS、DNS 和 pool 策略的必要配置。环境代理解析时机属于既有行为，须写成测试后再调整 key，不能因减少字段而跨配置误用连接。
- 缓存采用有限容量的淘汰策略，容量和默认 client 是否计入上限在提交中明确。首版使用内部策略，除非有调用需求，不新增公开调优参数。
- 淘汰只释放缓存持有的引用；活动任务及 body 保持资源存活，最后一个使用者退出后驱动释放。并发 cache miss 避免持续创建重复 client，也不在全局锁内执行耗时网络工作。

验收：不同请求超时能够复用相同网络 client，且各自期限准确；不同安全/路由配置保持隔离；顺序使用超过缓存容量的配置后，缓存数量有界，活动任务结束后线程数回落。明确容量限制的是缓存保留引用，不能把它宣传为任意活动下载量下的进程线程或 socket 硬上限。

### P3：统一多连接传输循环

改动位置：[multi.rs](../src/session/multi.rs)、[adaptive.rs](../src/session/multi/adaptive.rs)、[scheduler.rs](../src/scheduler.rs)、[flow.rs](../src/session/flow.rs)、[retry.rs](../src/session/retry.rs)。

目标职责：scheduler 负责范围所有权和分配；统一 worker 负责请求、读取、转发、确认和失败结算；恢复策略根据观测返回继续、回收或竞速建议。

实施步骤：

1. 先列出两套 worker 的差异，固定 Disabled/Adaptive/AdaptiveWithHedging 与 Fixed/Dynamic 的组合行为；Fixed 另覆盖 batch=0。
2. 抽出普通批处理请求和跨 piece body 消费，解除它们与慢速恢复策略对象的绑定。以内部类型表达职责，不为唯一实现引入通用插件框架。
3. 将恢复决策与执行分开。执行层统一管理 slot、lease、RetryState、前缀确认和失败结算；保留 pending range 的 lineage、退避与已消耗预算。
4. 复用数据转发中的限速、内存和 channel 等待，同时保留阶段观测，避免把本地背压算成网络低速。
5. 迁移各模式后删除旧的独立 worker 循环及仅为双路径存在的转接逻辑。单连接共享稳定的生命周期/转发组件，保留未知长度和非 Range 的专有处理。

验收：模式矩阵、probe 接管、跨 piece frame、截断、多读、身份变化、失败重试、暂停续传、慢尾和 writer 失败全部通过；普通执行只有一个权威实现。禁用慢速恢复时仍能动态调度和批处理，且不启用性能恢复决策；重构不改变请求并发与预算语义。

### P4：降低预分配和 writer 成本

改动位置：[file.rs](../src/storage/file.rs)、[writer.rs](../src/storage/writer.rs)、[cache.rs](../src/storage/cache.rs) 和 P1 基准。

按收益与风险递增顺序实施，每一步单独比较：

1. 连续写入时跟踪当前偏移，省略重复 seek；为单连接合并相邻小块，保留错误、停止和 flush 屏障。
2. 比较 Windows 原生空间预留、当前写零及按需增长。明确空间预留与逻辑长度不同，不能用 `set_len` 替换后声称仍保证提前分配磁盘空间。先保持默认值，只有报告支持才另行调整。
3. 比较当前 `tokio::fs` 路径与批量阻塞文件操作；若采用专用 writer 执行环境，测量多下载时新增线程和停止开销，避免把每块任务切换变成每任务无界线程。
4. 评估保留 `Bytes` 块引用或批量写入以减少 cache 复制。只有复制成本显著时才替换缓存表示，不能通过取消连续性校验或 lease 隔离获得收益。

预算由数据的实际持有者负责释放，所有写入失败、丢弃、取消和关闭路径均需结算。保留为下一块数据留出空间的水位规则；单连接新缓冲也必须纳入预算。除 checkpoint 和最终收尾所需同步外，不增加逐块 fsync。

验收：输出与持久化断点正确，小预算持续推进；代表场景的写操作/复制次数减少并有耗时或 CPU 收益，内存、线程和取消响应没有不可解释回退。多连接文件最终校验继续覆盖实际落盘内容，不为减少读取擅自改成仅校验网络字节。

### P5：改进 libcurl 多 pool 等待

改动位置：[driver/mod.rs](../src/network/curl/driver/mod.rs)、[driver/tests.rs](../src/network/curl/driver/tests.rs) 和 [pool_semantics.rs](../src/network/curl/pool_semantics.rs)。

- 用 P1 的活动/闲置混合场景确认循环频率、CPU 和命令延迟；增加无活动描述符时的检查。
- 首先修正任意选择 pool 等待的问题，并给无描述符场景建立有界等待；有活动请求不等于一定有可等待的 socket。
- 如果剩余开销需要完整事件机制，再比较聚合活动 socket 与命令唤醒，或符合现有池隔离约束的 `poll`/`wakeup` 方案。
- 仅把 `wait` 改成 `poll` 不能自动解决多个 Multi 的事件聚合。保留每 host 空闲上限、idle timeout、DNS 注入刷新、取消移除 handle 和停止驱动的现有契约。

验收：单/多 origin、所有 body 暂停、请求取消、超时、idle 回收和驱动释放均正确；等待不会形成无界空转，也不会让一个 origin 长期阻塞另一个 origin。比较 CPU、命令及取消延迟，不用缩短轮询间隔掩盖问题。

### P6：清理唯一后端结构与重复 CI

改动位置：[Cargo.toml](../Cargo.toml)、[lib.rs](../src/lib.rs)、[network.rs](../src/network.rs)、[Python Cargo 配置](../bindings/python/Cargo.toml)、[test.yml](../.github/workflows/test.yml)。

- 将每个平台两套等价完整 Rust job 合并为一套，保留 Linux、Windows、macOS 的完整测试；显式 `--no-default-features --features curl-backend` 保留轻量编译检查。
- 保留 Python 扩展构建、运行测试与 Linux 覆盖率门槛，不以去重为由删除不同平台实际编译的路径。
- 简化单分支 `BytehaulClient` 和内部重复分发，保留隔离 libcurl 细节所需的 HTTP 类型边界。
- `curl-backend` 名称、默认启用和缺失时的明确错误暂时保留；feature 对外语义调整另做兼容性变更。
- 随实现修订架构和使用文档中的历史后端描述，将仍有价值的迁移实验保留为历史依据。

验收：完整 Rust 平台 job 从六个减少为三个，显式 feature 配置仍有检查；公开用法和 wheel 构建不变。记录 CI 实际总计算时间变化，不将 job 数减半直接宣称为总时间减半。

## 5. 验证与采用门槛

每阶段先运行直接受影响的测试，再执行仓库要求的检查。正式提交 Rust 变更时使用同一 revision 验证：

```bash
cargo fmt --all -- --check
cargo test -p bytehaul --locked --all-targets
cargo test -p bytehaul --locked --doc
cargo clippy --workspace --locked --all-targets -- -D warnings
cargo doc --no-deps --workspace --locked
```

文档构建设置 `RUSTDOCFLAGS=-D warnings`。涉及绑定时，执行 [CI](../.github/workflows/test.yml) 对应的 Python 扩展构建与 pytest。

Rust 变更继续通过 Ubuntu x86_64 的 `python3 scripts/coverage.py` 参考门槛，初次安装工具使用 `--install`；以 [coverage-config.json](../scripts/coverage-config.json) 为准，保持当前 95% 行覆盖要求。Windows 测试或覆盖率结果不能代替该 Linux 门槛。本次计划编写未执行这些全量检查。

性能采用规则：

- 一次只改变一种机制，在相同配置和计时边界下比较，记录所有失败轮次。
- 功能和资源不变量是硬条件；吞吐收益必须同时看完成时间、CPU、内存、线程及重复流量。
- 将代表性场景超过 10% 的中位完成时间或 CPU 回退作为调查门槛：先复测并解释，再决定是否采用。该阈值是计划中的工程规则，不是已测得结果。
- P3 结构简化可以在性能无显著变化时采用，前提是确实删除重复执行路径且所有行为矩阵通过。P4/P5 增加机制复杂度时必须有测量收益。
- 新优化没有稳定收益时保留原实现，记录实验结论；不为完成阶段清单强行增加复杂度。

## 6. 提交与完成记录

每个阶段独立提交；P2 的配置与缓存、P4 的预分配与 writer、P6 的 CI 与类型清理分别提交。阶段报告记录问题、最终行为、验证结果、性能数据和兼容性影响，便于单独回退。

### P0 完成记录

最终行为：

- 每个下载任务由 [manager.rs](../src/manager.rs) 的 `download` 内一个统一出口负责，覆盖配置校验、并发排队、client 构建、传输、writer 收尾和校验和检查。子流程（`session::run_download`、`run_single_with_retry`、`run_multi_worker`）只返回结果并报告字节/速度/ETA，不再发布公开终态。
- 终态由 [progress.rs](../src/progress.rs) 的 `publish_terminal_state` 依据 `wait()` 的返回值发布一次：成功为 `Completed`，停止请求为 `Cancelled`/`Paused`，其余为 `Failed`。因此校验和不匹配以 `Failed` 结束，`Completed` 不会提前出现（B1）。
- 非法配置在任务内部拒绝后同样走该出口，最终状态为 `Failed`，且不创建输出文件或断点文件（B2）。
- 并发名额获取改为同时等待停止信号，并优先检查等待前已发出的信号；排队中的取消/暂停立即结束，不依赖其他下载释放名额（B3）。
- `DownloaderBuilder::build` 拒绝 `max_concurrent_downloads(0)`（B4）。
- 校验和计算按 64 KiB 分块轮询停止信号，停止请求不会让已完成的传输卡在长耗时校验上。
- 单一停止等待 `session::wait_for_stop` 现被请求重试退避、限速、内存预算、writer channel 和并发名额共用，四处的重复分支和 `ProgressUpdate::with_state` 一并删除。

P0 期间由新测试暴露并修复的两处既有缺陷：

1. watch 停止发送端关闭后，单连接与多连接的 `cancel_rx.changed()` 分支会永久返回 `Err`。由于该分支在 `select!` 中位于 `biased` 首位，丢弃 `DownloadHandle` 后传输会空转并停止推进（多连接 segment 读取同样受影响）。现在这两处改用统一的 `wait_for_stop`，发送端关闭后保持 pending，恢复“丢弃 handle 不取消下载”且不阻塞数据。回归测试见 `dropped_handle_neither_cancels_nor_wedges_a_running_download` 与 `test_dropping_the_handle_neither_cancels_nor_wedges_a_queued_download`。
2. 由缺陷 1 修复前，排队任务在丢弃 handle 后无法推进；修复后排队任务在名额释放后正常完成。

验证结果（本机 Windows，`2161fd4` 之上）：

- `cargo fmt --all -- --check`、`cargo clippy --workspace --locked --all-targets -- -D warnings`、`cargo doc --no-deps -p bytehaul --locked`（`RUSTDOCFLAGS=-D warnings`）通过。
- `cargo test -p bytehaul --locked --all-targets --no-fail-fast`：lib 516 项、集成 100 项（含新增 [m13_lifecycle.rs](../tests/m13_lifecycle.rs) 9 项）通过。唯一失败项 `http_header_timeout::header_deadline_includes_tls_handshake` 在该 revision 的干净 worktree 上同样失败（本机 libcurl 报 `SSL connect error`，未走到配置的 header 期限），与本次改动无关，未修改该测试。
- 未执行 Linux 覆盖率门槛；本轮无性能数据，因为 P0 不改变数据路径的复制、写盘或调度次数。

兼容性影响：公开参数语义和状态枚举未变。行为变化限于上文四项缺陷的修复，以及停止任务不再提前发布终态——调用方按文档等待 `wait()` 即可获得一致结果。[advanced.md](advanced.md)、[advanced.zh-CN.md](advanced.zh-CN.md)、[architecture.md](architecture.md)、[architecture.zh-CN.md](architecture.zh-CN.md) 已同步说明生命周期归属和终态一致性。

下一项可直接开始的工作是 **P1：建立默认路径与资源基线**。

### P1 完成记录

状态：已完成。基线数据、复现命令与已知限制见[默认路径与资源基线](pipeline-baseline.zh-CN.md)。

最终行为：

- 新增 [bench_stats.rs](../src/bench_stats.rs)：采集开关默认关闭，每处记录点先做一次 `Relaxed` 读取；打开后记录 writer 写块数与字节数、写回缓存拷贝与释放字节、`flush`+`sync` 次数与耗时、并发排队时长、输出文件创建与预分配耗时、请求→响应头耗时、body 等待耗时、校验和耗时，以及驱动线程数这一实时 gauge。没有行为读取计数器，正常下载只多一次可预测分支（唯一例外是请求→响应头耗时：该时长本就为日志与诊断测量，记录点自己读一次开关）。
- 驱动线程计数改为在 `DriverHandle::spawn` 创建 guard（而不是在线程闭包内），因此刚启动驱动的调用方一定能看到自己启动的线程；测试专用的 `live_threads` 模块与重复计数器被删除。
- [driver/mod.rs](../src/network/curl/driver/mod.rs) 增加主循环轮次计数 `DriverStats::loops`，用于识别空闲等待退化。
- [lib.rs](../src/lib.rs) 的 bench 包装新增计数器访问器、client 的驱动计数器、缓存条目数、可由调用方构造 `PieceMap` 的 scheduler，以及 `bench_scheduler_plan`——它按自适应 worker 循环的方式驱动 `assign_request_with_diagnostics`，覆盖了原有 `assign_to_with_split` 基准没有测到的默认路径。
- 新增 [pipeline_bench.rs](../benches/pipeline_bench.rs)：`harness = false` 的独立基准二进制（不用 criterion），五组共 42 个场景，全部使用本文件内的回环夹具；输出 `report.md` 与 `samples.csv`，记录 commit、工具链、平台、libcurl/TLS 特征、配置与计时边界，并保留逐轮数据。场景本身不作公网断言，也不设通过/失败门槛。`--archive DIR` 把这两份产物复制到 `target` 之外，因为只存在于被忽略目录里的数据无法被文档引用。
- 新增 [pipeline_counters.rs](../tests/pipeline_counters.rs)：用真实下载验证计数器——关闭采集时一次完整下载不记录任何东西；打开后写块数、缓存拷贝、预分配、同步、响应头与 body 等待、校验耗时与真实字节数一致；每个释放的 client 都让驱动线程数回到基线。

P1 由测量本身暴露并修复的三处缺陷：

1. 基准的 scheduler 指标原先只走 `assign_to_with_split`，与默认下载实际经过的候选扫描和请求规划不同。新增 `bench_scheduler_plan` 后，默认 Dynamic 与 Fixed 的差别（4096 piece 时 64 对 1024 个请求）才真正可测。
2. 用进度回调任务观测时间边界会漏掉短下载的全部快照（回调任务被独立调度）。改为在同一任务中同时等待终态与进度通道后，各阶段数值才有意义；报告同时输出 `running_reports`，明确这些边界受进度发布节奏限制。
3. 慢尾夹具把"范围内偏移"与"文件内位置"混用，导致限速分支从不触发（测得 62 ms，与普通形态无异）。改为按绝对位置判断后为 381 ms，与设计的 256 KiB / 20 ms 一致。

验收结果：

- 42 个场景 × 10 轮的完整结果已归档到 [docs/pipeline-baseline-run/](pipeline-baseline-run/)（`report.md` 摘要见[基线报告](pipeline-baseline.zh-CN.md)，逐轮原始数据见其中的 `samples.csv`）。现有默认下载实际经过的路径均可测量：动态请求规划、写回缓存与单连接直写、client 缓存与驱动线程、驱动主循环与取消延迟，以及七种端到端形态。
- 能够区分"减少请求""减少复制""减少磁盘操作"：Dynamic 对 Fixed 的请求数（64 对 1024）、源侧块大小对写块数无影响但对摄入成本有影响、单连接 256 次写块对 split 的 12 次、1 MiB 预算把块数推到 48.5。
- 结果不依赖公网速度断言：全部夹具在本机回环，CI 未加入任何性能门槛。
- 验证命令：`cargo fmt --all -- --check`、`cargo clippy --workspace --locked --all-targets -- -D warnings`、`cargo doc --no-deps -p bytehaul --locked`（`RUSTDOCFLAGS=-D warnings`）、`cargo test -p bytehaul --locked --all-targets`（lib 524 项通过 / 3 项忽略，集成 103 项通过，doc 1 项通过）与 `cargo test -p bytehaul --locked --doc`。`http_header_timeout::header_deadline_includes_tls_handshake` 在本机间歇失败（单独运行该目标 5 次失败 1 次，整轮并行测试更容易失败；本机 libcurl 报 `SSL connect error`），该现象在 P1 之前的 `2161fd4` 上同样存在，与改动无关，未修改。
- 未执行 Linux 覆盖率门槛。基线只在 Windows x86_64 上采集，该限制与主机噪声已写入基线报告。

兼容性影响：无公开 API 与行为变化。新增的计数器、循环计数与 guard 均为增量；删除的 `live_threads` 测试辅助由进程级 gauge 取代，两项驱动线程回归测试仍断言原有内容。

顺便确认的下一阶段判据：

- P2：缓存条目与驱动线程一一对应且无上限（32 个超时 → 33 条 / 33 线程），修复后应在超过容量时保持有界并让线程回落。
- P4：单连接 4 MiB 的 256 次写块（平均 16384 字节、不经缓存）与 12 MiB 预分配的约 8.0 ms 是两个直接可对照的基线数字。
- P5：停滞 pool + 空闲 pool 场景的静止期循环数在同一进程内逐轮双峰（41–47 对 44 万次/秒），修复判据是每一轮都落在 20 ms 一级的一档；纯空闲场景已经是不空转（0 次）。

### P0/P1 评审修正记录

对 `c49d127` 的评审确认 B1–B4 的修复有效，但发现下列 6 个问题，因此当时**不宜标记 P0/P1 全部完成**。修正分两次提交：`ddf8faf`（P0 错误路径）与 `2b53566`（P1 测量缺陷），基线数据在修正后重新采集（`docs/pipeline-baseline-run/`）。

| # | 级别 | 问题 | 最终行为 | 证据 |
| --- | --- | --- | --- | --- |
| 1 | 高（P0） | 断点保存失败仍报告暂停成功：`persist_single_control_snapshot` 与多连接同名函数只记录错误`log_warn`，不向外返回；把断点路径设为目录后暂停，得到 `Err(Paused)`、状态 `Paused`，却没有有效断点 | 两个函数改为返回 `Result<(), DownloadError>`，由调用方决定语义：自动保存（autosave、重试屏障）仍为尽力而为（下一次保存与终态保存都会重试），终止保存失败则返回该存储错误，任务以 `Failed` 结束，不再承诺不存在的恢复点 | [m13_lifecycle.rs](../tests/m13_lifecycle.rs) 的 `paused_download_without_a_writable_checkpoint_fails`、`paused_multi_connection_download_without_a_writable_checkpoint_fails`；修正前这两个测试失败 |
| 2 | 中（P0） | 多连接完成时丢弃 `ControlSnapshot::delete` 的结果，删除失败仍返回 `Ok(())`、状态 `Completed` | 完成路径改为 `ControlSnapshot::delete(control_path).await?`，与单连接一致：删不掉断点就说明"已完成"的描述与磁盘不符，按存储错误结束 | 同一文件的 `multi_connection_cleanup_failure_does_not_report_completed`；修正前该测试失败 |
| 3 | 中（P1） | "响应头延迟"实际延迟的是 body：夹具在独立 body 任务里 `sleep`，响应头立即返回（实测响应头约 0.5 ms，首字节约 308 ms） | 夹具改为 `async` 路由并在**返回响应之前**等待；新增库内计数器 `response_head_ms`/`body_wait_ms` 分别记录请求→头与 body 等待。[基线 4.5](pipeline-baseline.zh-CN.md)：4 个请求合计 1238.7 ms 的响应头延迟，body 等待仅 41.9 ms | `tests/pipeline_counters.rs` 断言真实下载的 `response_heads`/`response_head_micros`/`body_reads`/`body_read_micros` 均大于 0 |
| 4 | 中（P1） | 排队场景在所有轮次间共享同一道闸门，首次 `release()` 后不再复位；三轮实测放行前已接纳任务数为 1、2、2，排队时间 157、1.7、2.0 ms，归档数据同一现象 | 闸门改为可重新上锁（`Gate`），每轮开始前复位；归档运行 10 轮的 `admitted_before_release` 均为 1、`queue_waits` 均为 2、排队中位数 159.4 ms | 归档运行逐轮数据 `docs/pipeline-baseline-run/samples.csv` |
| 5 | 中（P1） | 重复流量指标用"发送总量 − 完整文件大小"，断流场景重复请求首段、重复发送至少 64 KiB，但总发送量不足 12 MiB，指标恒为 0 | 夹具记录每个响应实际发送的 `(起始位置, 字节数)`，由重叠量统计本轮重复发送；断流形态每轮都是 5 个响应 / 327680 字节，其中 65536 字节重复（5 个响应只覆盖 4–5 个不同 Range），其余形态为 0。旧结论"断流没有重复流量"已撤回 | [基线报告第 7 节](pipeline-baseline.zh-CN.md) |
| 6 | 中（P1） | 基准超时后下载仍在后台运行：超时只丢弃 `wait()` 的 future，按库语义既不取消也不等待，实测返回后仍有活动请求并污染后续轮次的全局计数与资源数据 | 每轮改由进度通道观察终态；到期先 `cancel()`，再继续等待**同一个**任务收尾（最多 30 s），不再有轮次在存活下载之上开始测量。取消轮会被显式记为 `harness round timeout` | 临时把 `ROUND_TIMEOUT` 设为 100 ms 时该路径被触发且未污染后续轮；用旧写法重跑同一场景，后续轮 `served_bytes` 与 `writer_bytes` 被前一轮的下载计入，可稳定复现污染 |

同时补齐评审指出的三项 P1 缺口：

- **CPU 指标**：每轮以 `cpu_ms` 与 `cpu_percent` 记录进程 CPU 时间（Windows `GetProcessTimes`，Linux `/proc/self/stat`），窗口与 `millis` 相同；报告说明它含夹具与基准自身，是上界。
- **响应头/body 分别计时**：见问题 3 的 `response_head_ms`/`body_wait_ms`。加上 `queue_ms`、`prealloc_ms`、`fsync_ms`、`checksum_ms`，六个阶段现在各自单独记录。
- **逐轮原始数据持久归档**：新增 `--archive DIR`，归档产物随文档提交到 [docs/pipeline-baseline-run/](pipeline-baseline-run/)（`report.md` 与 523 KB 的 `samples.csv`），不再只留在被忽略的 `target` 下。

修正后按同样命令复验：`cargo fmt --all -- --check`、`cargo clippy --workspace --locked --all-targets -- -D warnings`、`cargo doc --no-deps -p bytehaul --locked`（`RUSTDOCFLAGS=-D warnings`）、`cargo test -p bytehaul --locked --all-targets --no-fail-fast`（lib 524 项通过 / 3 项忽略，集成 `m13_lifecycle` 12 项等共 103 项通过，doc 1 项通过）。既有的 `http_header_timeout::header_deadline_includes_tls_handshake` 在本机间歇失败（单独运行该目标 5 次失败 1 次，整轮并行测试更容易失败），与本次改动无关。

仍待确认的一次性环境问题（与代码无关）：本机 `.git/objects` 的部分目录缺少当前沙箱的写权限，`git add` 偶发 `insufficient permission for adding an object to repository database`；本会话内以逐次提权提交绕过，若不希望继续逐次提权，需要一次管理员操作：`icacls "D:\project\rust-aria2\.git\objects" /reset /T /C`。

- [x] P0：生命周期问题修复并通过回归测试（含评审修正）。
- [x] P1：默认执行路径与资源基线归档（含评审修正后重采）。
- [ ] P2：配置解析统一、超时与 client 身份分离、缓存有界。
- [ ] P3：多连接普通执行统一，恢复策略独立。
- [ ] P4：存储实验完成，采用有收益的改动或记录保留原实现的依据。
- [ ] P5：多 pool 等待验证完成，修复已确认问题。
- [ ] P6：唯一后端结构与重复 CI 清理完成。

## 7. 外部实现依据

- [Tokio 文件 I/O 调优](https://docs.rs/tokio/latest/tokio/fs/index.html#tuning-your-file-io)：文件操作使用阻塞线程池，建议批量处理；支持 P4 的测量方向，不保证特定提速比例。
- [CURLOPT_CONNECTTIMEOUT_MS](https://curl.se/libcurl/c/CURLOPT_CONNECTTIMEOUT_MS.html)：连接期限可配置于 easy handle；P2 同时保留本项目在 Tokio 侧 DNS 查询的期限约束。
- [curl_multi_wait](https://curl.se/libcurl/c/curl_multi_wait.html)：无可等待描述符时立即返回，是 P5 空转风险判断的接口依据。
- [curl_multi_poll](https://curl.se/libcurl/c/curl_multi_poll.html)：支持无描述符等待和跨线程唤醒；采用前仍需解决本项目多个 Multi 的事件组织。

### P2 步骤 1：网络覆盖解析（2026-09-14）

- `NetworkOverrides` 用 `Option` 保存连接超时与连接池覆盖，删除值与 `overridden` 标志并存的状态。网络生效配置由 `DownloadSpec::resolve_network_config` 统一解析，manager 只调用解析结果。
- 保留公开 getter 的默认值、显式设置默认值覆盖 downloader、代理整组替换、TLS 路径逐项继承，以及禁用连接池时继承 downloader idle timeout 的语义。增加 setter 顺序与显式默认值回归测试。Python 参数和校验顺序本步未变。
- macOS / Rust 1.96：fmt、doc test、workspace rustdoc（`-D warnings`）通过；清除子进程代理变量后 lib 523 通过、1 失败、3 忽略。失败的 `a_pool_that_may_not_reuse_connections_keeps_none_idle` 在 `git archive HEAD` 的原始源码上独立复现。集成测试 102 通过，慢尾竞速一项首次失败、独立复测通过，原始源码的慢速恢复 11 项全部通过。
- 全量 `--all-targets --no-fail-fast` 已执行测试并进入 42 场景基准；基准耗时不作为本步结构变更的性能结论。Clippy 被既有 `driver/mod.rs` 的平台类型转换和 `scheduler.rs` 的 checked division 两项告警阻挡。本步不宣称全量检查通过；Linux 95% 覆盖率未执行。
