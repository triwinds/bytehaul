# P4 存储优化与实验记录

日期：2026-09-14。writer 优化及 macOS 实验已完成；Windows 原生空间预留尚未实机运行，Linux 覆盖率未执行。多连接预分配场景的总耗时波动仍需更稳定的性能环境复核，不能将 P4 全部验收项标为通过。

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

这些结果不足以确认稳定的退化，也不足以证明没有退化；不能把变化归因到 P5，更不能把该场景写成性能提升。本轮保留原有分配策略，没有用更改默认预分配掩盖结果。此项端到端性能验收仍需稳定负载环境复核。

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

当前只有 macOS，**Windows 候选尚未编译和实机运行**。不能用本机 logical-length/zero-fill 对照代替 Windows 三路径结果，也不能据此改变默认值。Windows 后续运行 `cargo bench --locked --bench pipeline_bench -- --filter storage/allocation/ --rounds 10 --archive docs/p4-storage-run/windows`；再比较预留时间、总完成时间、实际文件及磁盘空间语义。

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

## 验证

- `cargo fmt --all -- --check`、`git diff --check` 通过。
- `cargo clippy --workspace --locked --all-targets -- -D warnings` 通过。
- `cargo test -p bytehaul --locked --tests --no-fail-fast`：**lib 530 通过 / 3 忽略，集成 105 通过**。测试时清除子进程代理环境变量。
- `cargo test -p bytehaul --locked --doc`：1 项通过；`RUSTDOCFLAGS=-Dwarnings cargo doc --workspace --no-deps --locked` 通过。
- 新增预算为 1/3/9 字节时的单连接缓冲、回退和持久化屏障；写入失败、连续性错误、队列丢弃、writer abort、缓存 discard 和迟到数据均检查许可回收。真实单连接计数测试同时校验文件内容、写块及 seek。
- 原取消续传用例原本等待 `metadata.len > 0`，对小于缓冲阈值的文件会直到正常完成才触发取消；现在等待已接收进度，明确断言返回 `Cancelled`，再验证持久化前缀和续传。首次该用例的失败已由这一更新解决，未放宽返回结果或断点校验。
- 生产路径 20 轮配对 writer、40 轮预分配复测、10 轮缓冲取消及九类存储候选各十轮均已运行并保存样本。未重复运行无关 scheduler/client/driver 基准，也不把 Clippy 的全目标编译表述为 `cargo test --all-targets` 通过。
- 在 `bindings/python` 目录执行 `maturin develop --bindings pyo3 --no-default-features --features curl-backend` 重建最终扩展，再运行 `pytest tests -q`：**157 项全部通过**。
- Windows 原生预留候选和 Linux 95% 覆盖率门槛未验证；本报告不将其标记为通过。
