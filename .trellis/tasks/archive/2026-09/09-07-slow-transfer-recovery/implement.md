# 执行计划

状态：用户已明确授权实现，A/B/C 已落地；全部质量门禁通过，提交已获用户确认。实际方案使用 `src/session/multi/adaptive.rs` 和现有 lease 锁内仲裁，未新增持久化状态格式。

## 实现顺序

### A. 观测、检测与受控恢复

- [x] 定稿 Rust/Python 模式与参数默认，补验证和所有入口镜像清单。
- [x] 在 session 层增加小型纯策略状态机（`src/session/multi/adaptive.rs`），显式传入时间与样本，独立测试窗口、宽限、基线失效、恢复重置和预算。
- [x] 将读取、首部、限速、内存、队列、writer barrier、退避等待区分；monitor 独立 tick，保护取消链路。
- [x] 复用 RetryState 硬错误决策；增加局部“性能恢复/被取代”结果，与全任务停止区分。
- [x] 加入 lineage 恢复预算与一次性进度回退，验证同一范围反复回收和拆分无法重置或复制额度。
- [x] 增加默认事件日志及 opt-in debug 快照，不输出 URL query/认证头。

### B. 尾部接管

- [x] 调度器支持暂时空闲等待、恢复队列、lease 仲裁和无丢失唤醒。
- [x] 完成与取消竞争时只执行一个结果；停止 producer → discard ack → reclaim → split → wake。
- [x] 明确终止条件：全部完成成功；单个被取代请求不导致失败；致命错误及时收敛，不能因 idle workers 永久等待而挂住。
- [x] 保留稀疏调度和原分片几何，不增加全文件扫描。
- [x] 确认恢复任务仍沿用原 lineage 的重试范围及 elapsed 起点。

### C. 有预算竞速

- [x] 实现尾部资格、ETA 获益判定、唯一 challenger、全局连接硬上限和发起前字节预留。
- [x] validator 获取、条件 Range、冲突/缺失抑制和版本变化失败。
- [x] 备用请求临时暂存，逐 chunk 使用内存与网络限速；复制回主 writer 不重复限速。
- [x] CAS/generation 或锁内等价仲裁；暂存完整后才允许替换主写入者。
- [x] 所有 loser、存储失败、取消、暂停路径释放 permit、临时文件；流量预留不退还，以保持保守上界。
- [x] 主文件与控制文件只表达唯一获胜范围；重复流量单独统计。

### D. 验证与交付

- [x] 原单/多连接、429/503、Pause/Cancel、writer 失败和 resume 回归。
- [x] Rust/Python 文档与 defaults、校验、所有便捷入口一致。
- [x] 本地基准记录中位数和尾部分位耗时、重复字节、请求数、最大并发、CPU/内存/临时磁盘开销；稳定快网、慢连接、全体慢、背压至少各一组。
- [x] 完成下面质量门禁及 spec 更新，再按实际实现重写最终说明。
- [x] 无真实地址时明确性能结论限于可控场景，不声称已复现用户源站问题。

## 测试矩阵

| 场景 | 必须断言的结果 |
| --- | --- |
| 每隔短时间滴少量数据，未达到 read_timeout | 独立低速计时触发一次恢复，输出完整正确 |
| 短暂抖动、启动期、恢复到正常速率 | 不触发错误淘汰，累计低速时间正确清零 |
| 限速/内存/队列/writer 等待超过低速时长 | 不当作慢连接；取消和 writer 关闭仍及时返回 |
| 还有大量普通工作 | 不启动尾部 challenger，快 worker 继续领普通任务 |
| 无普通工作但尚有慢请求 | idle worker 等待并接管；屏障证明恢复请求实际到达 |
| 原请求与恢复同时完成 | 完成/reclaim 只执行一次，无 stale completion |
| challenger 赢、主请求赢、同时完成 | 唯一提交，输出字节一致，重复字节不进入有效进度 |
| challenger 头部快而 body 慢、网络失败、暂存失败 | 不提前杀主请求，不把可选候选错误升级为整任务失败 |
| 强/弱/缺失 ETag、对象变化、200/错误 206/短 body | 合理抑制或类型化失败，不能发布混合版本完成态 |
| max_connections=1、预算不足、预算刚好、最小内存=1 | 无越限、无 permit 泄漏或死锁；不足时跳过优化 |
| 全部连接慢、重分配循环、429/503 Retry-After | 无风暴、无新 RetryState 绕过上限、冷却不被新 worker 绕过 |
| 恢复/竞速过程中 Pause/Cancel、checkpoint sync 失败 | 状态正确、停止后无活跃副本、控制文件不超报完成 |
| pooling 开/关，同一尾部范围新请求 | 明确 TCP 连接归属，不把 worker ID 当连接 ID |
| disabled 和单连接路径 | 既有语义保持，所有输出逐字节校验 |

网络测试使用 ephemeral localhost、临时路径、请求 gate/oneshot barrier，不能假设一次 socket write 等于一个 body frame。检测策略用注入时间，网络 timeout 仅作为死锁保护；不把“快至少 150 ms”之类门槛写进 CI。

## 主要文件与风险点

- `src/session/{multi,flow,retry,range_validate,mod}.rs`：状态、取消、预算和 checkpoint 交错。
- `src/scheduler.rs`、`src/storage/{segment,writer}.rs`：所有权、唯一提交和稀疏计数。
- 新 session 策略模块、暂存 helper：只在有明确边界时新增，不复制单/多路径公共逻辑。
- `src/config.rs`、`src/lib.rs`、`bindings/python/src/{lib,lib_tests}.rs`、Python 测试/导出：公共配置契约。
- `src/http/{request,worker}.rs`、`src/network.rs`：validator、实际连接行为；避免变更共享 Downloader 默认。
- `tests/m3_multiworker.rs`、`tests/m11_flow_control.rs` 及拟新增 `tests/m12_slow_transfer.rs`：沿用 fixture 模式。
- 暂存 helper 如需将 tempfile 从 dev 依赖变成生产依赖，先评估现有文件 helper 与依赖影响，不引入无必要缓存框架。

## 实现阶段验证命令

所有命令按项目 RTK 规则执行，localhost 测试先隔离代理环境。

```sh
rtk proxy env -u HTTP_PROXY -u HTTPS_PROXY -u ALL_PROXY -u http_proxy -u https_proxy -u all_proxy cargo test -p bytehaul --all-targets
rtk cargo test -p bytehaul --doc
rtk cargo clippy --workspace --all-targets -- -D warnings
rtk proxy env RUSTDOCFLAGS='-D warnings' cargo doc --no-deps --workspace
rtk proxy uv sync --project bindings/python
```

在 `bindings/python` 工作目录执行：

```sh
rtk proxy uv run maturin develop --bindings pyo3
rtk proxy env -u HTTP_PROXY -u HTTPS_PROXY -u ALL_PROXY -u http_proxy -u https_proxy -u all_proxy uv run --no-sync pytest
```

Linux 使用 `rtk proxy python3 scripts/coverage.py`（按需先 --install），产生当前改动的新报告并满足 95% 门禁；macOS 功能测试不能替代 Linux 覆盖率。依照 CI 同步检查 Windows/macOS 行为。

## 启动实现前

- [x] 默认启用方式已由用户确认：adaptive 默认开启，竞速显式开启。
- [x] PRD 无阻塞问题，design/implement 与其一致，完成收敛复核。
- [x] 取得用户明确实现授权：“开始实现吧”。
- [x] 根据当前平台 dispatch 配置决定是否需要 curated jsonl；需要时加入真实 spec/research 条目，不能留示例。
- [x] 已运行 task.py start，遵循 trellis-before-dev / 实现与检查流程。
