# P4 存储优化与实验记录

日期：2026-09-14（2026-09-15 更新验证状态）。writer 优化及 macOS 实验已完成；Windows 原生空间预留已在 CI 的 Windows job 实机运行（debug 构建，10 轮 `verified=1`），Linux 覆盖率门槛已由 CI 通过（95.29%，7700/8081 行）。多连接预分配场景已在最终 P5 driver 上用隔离配对 release 测量复测，未复现稳定回退，见[专节](#p5-驱动上的-p4-writer-隔离复测2026-09-15)；debug CI 结果不代替 release 性能验收。

## 实际采用的改动

- writer 独占文件句柄并跟踪当前偏移，连续写入省略 seek；首次写入、断点起始位置和重试回退仍定位。没有改变输出数据、lease 连续性检查或对象验证。
- 单连接合并相邻小块，在 **256 KiB 刷出阈值**或会话 watermark 处写出。阈值不是 RSS 或分配容量硬上限，追加最后一块可能越过阈值；较大输入直接写出，避免额外聚合复制。小预算沿用“留出下一块空间”的几何规则。
- `OwnedSemaphorePermit` 随普通响应和 staged challenger 数据入队，writer 持有到 flush/discard。队列关闭、迟到数据、当前写入失败、尚未写出的缓存及未消费队列均通过所有权释放预算，删除原来的手动 `forget + add_permits` 结算。`None` 仅供不模拟预算的内部测试使用，生产 producer 始终携带许可。
- `FlushAll`、关闭 writer、停止及 checkpoint 仍排空缓冲并执行原有同步；不增加逐块 fsync。多连接最终校验仍读取输出文件。未知长度、非 Range、重试和暂停续传保留原有流程。
- 新增 `writer_seeks` 独立计数。`writer_blocks` 现在只表示交给文件的写块，不能再当作 seek 次数；`cache_copied_bytes` 同时包含新增的单连接聚合复制。

**保留的实现**：预分配默认值、Windows 写零路径、Tokio 文件后端及多连接 `BytesMut` 缓存均未替换，没有新增专用 writer 线程或公开配置。macOS 原有 `set_len` 只扩展逻辑长度，文档已删除其保证物理空间预留的暗示。

## 同机测量方法与来源

基线生产代码为 `086210083409d83511e942848e223296e7999cd2`。在 `target/p4-before-source` 提取该提交，仅加入当前基准、macOS CPU/RSS 仪表和 seek 计数；旧 writer 的每次 seek 都记录一次，未加入缓冲、跳过 seek 或预算所有权改动。候选为本轮工作区。两者均为 **Rust 1.96.0 / aarch64-apple-darwin / release bench profile**，vendored libcurl 8.21.0-DEV / OpenSSL 3.6.3，使用相同 localhost 夹具，清除子进程代理变量。

先完成两个二进制的构建，再用 [compare_writer_bench.py](../../scripts/compare_writer_bench.py) 交替运行；偶数轮先基线、奇数轮先候选，测量期间不运行编译或测试。每轮新进程覆盖相同五种 writer 场景，各 **20 轮**，共 200 次完整下载，逐次检查输出。前后原始数据：[before.csv](paired/before.csv)、[after.csv](paired/after.csv)；[metadata.json](paired/metadata.json) 保存顺序、时间及实际二进制 SHA-256。[源码指纹](source-hashes.json)记录最终交付源码与加入仪表后的基线源码；后续仅补测用例及基准入口，未改变已测生产行为。

`total` 是真实下载终态等待窗口；CPU 和 `process_peak_rss_bytes` 包含夹具、校验及收尾，CPU 窗口为完整 round。Darwin `getrusage` 返回进程累计峰值 RSS，**不能归因到单一缓存，也不是瞬时峰值差**。这里不报告稳定 P95，不以公网速度断言 CI 成败。

## 默认 writer 路径（20 轮中位数）

表内箭头为基线 → 候选。

| 场景 | 写块 | seek | 总耗时 ms | round CPU ms | 进程峰值 RSS MiB |
| --- | ---: | ---: | ---: | ---: | ---: |
| split_4conns_16KiB_chunks | 12.00 → 12.00 | 12.00 → 8.50 | 110.34 → 114.27 | 18.72 → 18.96 | 47.30 → 47.77 |
| split_4conns_1MiB_chunks | 12.00 → 12.00 | 12.00 → 10.00 | 80.71 → 81.56 | 16.14 → 15.29 | 55.45 → 55.02 |
| split_tiny_budget_1MiB | 58.00 → 50.50 | 58.00 → 41.00 | 170.54 → 146.59 | 21.06 → 23.05 | 55.80 → 55.32 |
| split_prealloc_on | 12.00 → 12.00 | 12.00 → 10.00 | 123.27 → 144.23 | 24.64 → 30.16 | 57.45 → 57.45 |
| single_conn_4MiB | 257.00 → 16.00 | 257.00 → 1.00 | 83.33 → 43.24 | 21.13 → 13.14 | 64.12 → 65.57 |

4 MiB 单连接写块减少约 **93.8%**，seek 从约 257 次降至 1 次，总耗时中位数减少 **48.1%**、CPU 减少 **37.8%**。代价是每轮新增 4 MiB 的聚合复制；其开销已进入上述 CPU 和总耗时。多连接普通预算仍为每个完整 piece 写一块，不宣称减少了其写块次数。

### 预分配场景的不确定性

首次 20 轮配对中 `split_prealloc_on` 总耗时中位数 123.27 → 144.23 ms，不能略去这个偏慢结果。对此用相同二进制单独交替复测 **40 轮**：[基线](prealloc-recheck/before.csv)、[候选](prealloc-recheck/after.csv)、[执行顺序](prealloc-recheck/metadata.json)。

复测总耗时中位数仍为 **108.67 → 136.97 ms**；但 round CPU 为 **16.67 → 16.87 ms**、预分配 **3.407 → 3.319 ms**、最终同步 **6.522 → 5.825 ms**，均没有对应的存储阶段变慢。body wait 为 148.58 → 143.12 ms（并行请求时间之和，不等于关键路径），连接数保持 4。逐轮数据有较大波动，40 个配对总耗时差的均值为 +9.76 ms、标准差 91.89 ms；固定随机种子 1、10,000 次配对 bootstrap 的均值差 95% 区间约为 **[-18.74, +37.40] ms**。

这些结果不足以确认稳定的退化，也不足以证明没有退化；不能把变化归因到 P5，更不能把该场景写成性能提升。本轮保留原有分配策略，没有用更改默认预分配掩盖结果。这些数字的适用边界是当时的 pre-P5 驱动；评审要求后的最终驱动隔离复测见下一节，本节数据原样保留、不改写、不重定义统计口径。

## P5 驱动上的 P4 writer 隔离复测（2026-09-15）

评审要求把 `split_prealloc_on` 的未定项放到最终 P5 driver 上隔离复核：只改变 P4 的实现，其余源码、工具链与计时边界相同。复测使用仓库已有的 [compare_writer_bench.py](../../scripts/compare_writer_bench.py)（交替顺序、每轮校验输出），两侧各 40 轮，不做超采或选择性重跑。

**来源与方法**

- 候选（after）是最终提交 `5a4f274` 的 `git archive` 快照；基线（before）是同一快照反向应用 P4 提交 `6d9bdfe` 的全部源码改动（writer、预算所有权与相关 session 代码，不含基准、`bench_stats`、`Cargo.toml` 与文档），并在旧 `writer::write_block` 的 `bench_stats` 计数块内补一行 `record_writer_seek()`，使 seek 计数语义与候选一致；其余文件在构建前经目录对比确认逐字节相同。
- 两个二进制均为 **Rust 1.96.0、bench（release）profile、aarch64-apple-darwin**，同机回环夹具，独立 `CARGO_TARGET_DIR`。SHA-256：before `6146c75586c977f7e93ac2ad8a8962946d0b2c5d026fe4352f12eb591c3e8e83`，after `fb032fc79e81d59c2e101eedee2844c644c1e63c388b1a4fdaeac6a4b419634d`（原始记录见 [metadata.json](prealloc-p5-driver/metadata.json)）。
- 40 个配对轮，80 次下载两侧全部 `verified=1`，无失败或被丢弃的轮次；逐轮原始数据：[before.csv](prealloc-p5-driver/before.csv)、[after.csv](prealloc-p5-driver/after.csv)。统计为配对差（after − before）均值与固定随机种子 1、10,000 次重采样的 95% 区间。

**结果（40 配对轮，毫秒）**

| 指标 | before 中位 (p25–p75) | after 中位 (p25–p75) | 配对差均值（标准差） | 95% bootstrap 区间 | after 更快的轮数 |
| --- | ---: | ---: | ---: | ---: | ---: |
| total | 17.889 (16.500–19.415) | 17.426 (16.436–19.085) | −0.964 (6.612) | [−3.298, +0.629] | 23/40 |
| cpu_ms | 28.477 (25.563–32.563) | 29.081 (26.589–31.523) | −0.174 (4.706) | [−1.624, +1.216] | 18/40 |
| prealloc_ms | 3.850 (3.454–4.258) | 3.811 (3.335–4.145) | −0.308 (1.294) | [−0.738, +0.055] | 23/40 |
| fsync_ms | 6.517 (5.481–7.464) | 6.538 (5.377–7.120) | +0.039 (2.947) | [−0.852, +0.953] | 18/40 |
| body_wait_ms | 7.085 (6.043–7.813) | 6.976 (6.290–8.137) | −0.115 (2.541) | [−0.986, +0.532] | 16/40 |
| writer_seeks | 12 (12–12) | 11 (4.75–12) | −3.075 (3.812) | [−4.275, −1.950] | 25/40 |

`writer_blocks` 两侧恒为 12、`cache_copied_bytes` 恒为 12 MiB，多连接路径的写块结构未变；`process_peak_rss_bytes` 中位 49.22 → 48.38 MiB（差均值 −0.71 MiB，区间 [−1.40, −0.01] MiB），没有增加。

**结论与边界**

- 在最终 P5 driver 上只回退 P4 实现，未复现稳定回退：total 中位差 −0.46 ms（−2.6%），配对差均值 −0.96 ms 由单轮 before 57.0 ms 的离群点主导（去掉该轮后均值约 0.0 ms），该区间描述配对均值差，不能直接用于判断中位数回退是否低于 10%；本次样本的 total 与 CPU 中位数变化分别为 −2.6% 与 +2.1%，未观察到超过调查门槛的回退；更快/更慢的轮次约各半，CPU、预分配、最终同步与 body 等待均无对应变化，seek 计数按 P4 设计下降。据此关闭"P4 writer 在最终驱动上造成 `split_prealloc_on` 稳定回退"这一验收项。
- 本项结论只覆盖该场景、该配置与最终源码；两侧都含 P5，差异只来自 P4，**没有**用 P5 的整体收益代替对照测量；也不反推 pre-P5 数字的成因。上文 pre-P5 原始数据保留在 [paired](paired/) 与 [prealloc-recheck](prealloc-recheck/) 目录，未删除或改写。
- 与 CI 的关系：Windows CI 的 `storage/allocation/windows_reserve` 等基准是 debug 构建，与本节的 release 配对测量不是同一证据类别。

## 缓冲未写出时取消（前后各 10 轮）

新增 `writer_stop/single_buffered_cancel`：单连接以 16 KiB/50 ms 接收数据，进度出现后立即取消。候选取消前通常已接收 80 KiB、文件写入计数仍为 **0**；取消 `wait()` 包含缓冲排空、同步和控制文件保存。

取消中位数 **12.159 → 12.014 ms**。每轮均返回 `Cancelled`，读取并核对实际文件前缀，断点文件存在；持久化前缀中位数均为 80 KiB，所有 driver 最后归零。它证明新缓冲进入真实停止收尾路径，既不是取消一个未开始的请求，也不是只测发出取消信号的时间。原始数据：[基线](cancel/before.csv)、[候选](cancel/after.csv)、[执行顺序及二进制身份](cancel/metadata.json)。

## 存储候选实验（各 10 轮）

[实验源码](../../benches/pipeline_bench/storage_experiments.rs)、[逐轮数据](experiments-final/samples.csv)、[自动报告](experiments-final/report.md)。`storage/` 实验中的写操作和复制计数来自候选自身（含 `cache_copied_bytes`），不应与真实下载的 `BenchCounters` 混合；实际 writer 对照仍使用生产仪表。每轮 64 MiB 非常量数据，计时包含建文件、候选操作和最后 sync；计时外读取并逐字节验证。输入生成在场景计时外，缓存实验模拟 body 块的构造属于总耗时，但不计入 `assembly_ms`。

| 候选 | 总耗时中位数 ms | round CPU ms | 结论 |
| --- | ---: | ---: | --- |
| io/tokio_seek_16KiB | 99.311 | 91.647 | 4096 个写块及 seek，对照原逐块路径 |
| io/tokio_sequential_16KiB | 48.737 | 49.142 | 4096 个写块，单独显示省略 seek 的影响 |
| io/tokio_batch_256KiB | 14.480 | 16.123 | 256 个写块；支持采用批量缓冲 |
| io/blocking_batch_256KiB | 13.766 | 16.101 | 256 个阻塞池任务，无专用线程；与 Tokio 批量路径差距小，保留 Tokio |
| cache/copy_then_write | 19.243 | 17.448 | 保留现有多连接缓存表示 |
| cache/retain_then_vectored_write | 15.308 | 15.070 | 微基准有收益；未证明默认下载被复制限制，不进入生产 |
| allocation/grow | 14.926 | 14.704 | 按需增长，不保证提前占用空间 |
| allocation/zero_fill | 28.462 | 18.978 | 完整写零并 sync 后再写数据 |
| allocation/logical_length | 19.188 | 15.319 | 仅逻辑长度对照，不宣称物理预留 |

复制与引用实验的 assembly 中位数为 **2.202 → 0.047 ms / 64 MiB**。该实验把 64 个 1 MiB piece 先聚合再写，与真实 worker 随 lease 完成刷出、默认 32 MiB watermark 不同；只说明候选的上限方向。引用方案还可能因 `Bytes` 切片保留更大的底层分配，Tokio 路径也不能直接获得这里的同步 vectored 写入收益。故未把微基准中的少复制等同于整个下载器的收益，连续性和 lease 隔离代码保持原样。

阻塞池批量路径和 Tokio 批量路径的 CPU 几乎相同。本轮不为小幅时间差新增文件后端、专用线程或停止协议；真实下载的线程模型保持不变。

### Windows 空间预留

实验入口 `storage/allocation/windows_reserve` 仅在 Windows 构建中列出，调用 `SetFileInformationByHandle(FileAllocationInfo)`，记录预留后的逻辑长度，再单独扩展 EOF 并同步。依据 [Microsoft FILE_ALLOCATION_INFO](https://learn.microsoft.com/en-us/windows/win32/api/winbase/ns-winbase-file_allocation_info) 与 [Microsoft 关于预留和可读长度的说明](https://devblogs.microsoft.com/oldnewthing/20160714-00/?p=93875)。

2026-09-14 的 CI 运行 [34859938228](https://github.com/triwinds/bytehaul/actions/runs/34859938228)（Windows job [104029167854](https://github.com/triwinds/bytehaul/actions/runs/34859938228/job/104029167854)）已在 windows-latest 上编译并执行该入口：`cargo test -p bytehaul --all-targets` 会运行基准目标，`storage/allocation/windows_reserve` **10 轮全部 `verified=1`**，中位 `total` 129.409 ms（p25–p75 118.808–149.655）、`allocation_ms` 0.280（0.265–0.317）；同一运行的对照为 `grow` 126.311 ms / 0.000 ms、`zero_fill` 314.507 ms / 129.266 ms、`logical_length` 130.392 ms / 0.276 ms（分别为 `total` / `allocation_ms`）。这里使用 `total` 指标，不使用包含夹具校验的整轮耗时列。

该运行在 debug 测试构建与托管 runner 上完成，只证明候选能在真实 Windows 上编译、执行并产出通过校验的文件，且预留本身耗时与按需增长同级、明显低于写零路径；**不代替 release 性能验收**，也不据此改变默认分配策略。需要 release 数字时可在 Windows 上运行 `cargo bench --locked --bench pipeline_bench -- --filter storage/allocation/ --rounds 10 --archive docs/p4-storage-run/windows`，再比较预留时间、总完成时间、实际文件及磁盘空间语义。

## 复现

普通候选与微基准：

```bash
cargo bench --locked --bench pipeline_bench --no-run
cargo bench --locked --bench pipeline_bench -- --filter writer/ --rounds 10 --archive target/p4-writer
cargo bench --locked --bench pipeline_bench -- --filter storage/ --rounds 10 --archive target/p4-storage
cargo bench --locked --bench pipeline_bench -- --filter writer_stop/ --rounds 10 --archive target/p4-stop
```

配对比较先从基线提交提取独立源码目录，复制本轮 `Cargo.toml`（只增加 macOS bench 的 libc）、`src/bench_stats.rs`、`src/lib.rs` 和整个 `benches` 基准目录；在旧 `writer::write_block` 的已启用计数块内增加 `record_writer_seek()`。使用独立 `CARGO_TARGET_DIR` 编译两个 benchmark，二进制路径取自 Cargo 输出。然后：

```bash
python3 scripts/compare_writer_bench.py --before /absolute/before/pipeline_bench --after /absolute/after/pipeline_bench --out target/p4-paired --rounds 20
python3 scripts/compare_writer_bench.py --before /absolute/before/pipeline_bench --after /absolute/after/pipeline_bench --out target/p4-prealloc --rounds 40 --filter writer/split_prealloc_on
python3 scripts/compare_writer_bench.py --before /absolute/before/pipeline_bench --after /absolute/after/pipeline_bench --out target/p4-cancel --rounds 10 --filter writer_stop/
```

目录 `before`/`after`/`experiments` 为最初探索样本，`before-final`/`after-final` 为首次同仪表的整批独立运行，保留以便审计；它们不是主表来源。主表采用后续 `paired` 交替样本，预分配问题另列 `prealloc-recheck`，不选择性删除偏慢轮次。

P5 驱动上的隔离复测（见前文专节）只在上面的脚本之外多一步基线构造：取 `git archive HEAD` 快照，对该快照反向应用 `6d9bdfe` 的全部生产源码改动（`git diff 6d9bdfe^ 6d9bdfe -- src/... | patch -R -p1`，排除 `bench_stats.rs`、`lib.rs`、`Cargo.toml` 与 `benches/`），再在旧 `write_block` 的计数块内补 `record_writer_seek()`；两个快照各自用独立 `CARGO_TARGET_DIR` 构建，之后仍由 `compare_writer_bench.py --rounds 40 --filter writer/split_prealloc_on` 交替运行，数据归档于 `prealloc-p5-driver/`。

## 验证

- `cargo fmt --all -- --check`、`git diff --check` 通过。
- `cargo clippy --workspace --locked --all-targets -- -D warnings` 通过。
- `cargo test -p bytehaul --locked --tests --no-fail-fast`：**lib 530 通过 / 3 忽略，集成 105 通过**。测试时清除子进程代理环境变量。
- `cargo test -p bytehaul --locked --doc`：1 项通过；`RUSTDOCFLAGS=-Dwarnings cargo doc --workspace --no-deps --locked` 通过。
- 新增预算为 1/3/9 字节时的单连接缓冲、回退和持久化屏障；写入失败、连续性错误、队列丢弃、writer abort、缓存 discard 和迟到数据均检查许可回收。真实单连接计数测试同时校验文件内容、写块及 seek。
- 原取消续传用例原本等待 `metadata.len > 0`，对小于缓冲阈值的文件会直到正常完成才触发取消；现在等待已接收进度，明确断言返回 `Cancelled`，再验证持久化前缀和续传。首次该用例的失败已由这一更新解决，未放宽返回结果或断点校验。
- 生产路径 20 轮配对 writer、40 轮预分配复测、P5 驱动上的 40 轮隔离复测、10 轮缓冲取消及九类存储候选各十轮均已运行并保存样本。未重复运行无关 scheduler/client/driver 基准，也不把 Clippy 的全目标编译表述为 `cargo test --all-targets` 通过。
- 在 `bindings/python` 目录执行 `maturin develop --bindings pyo3 --no-default-features --features curl-backend` 重建最终扩展，再运行 `pytest tests -q`：**157 项全部通过**。
- Windows 原生预留候选已由 CI 的 Windows job 实机运行（debug 构建，10 轮 `verified=1`）；Linux 95% 覆盖率门槛已由同一提交的 CI 通过（95.29%）。两者均为 debug/CI 证据，不代替 release 性能验收；release 侧的隔离复测见前文专节。
