# 缺失功能实现计划：Pause/Resume（方案 A）、ETA 计算、文件名自动检测

> 创建日期：2026-03-24
> 备注：本文已按当前代码结构和最新设计取舍修订，避免基于过时前提推进实现。

---

## 一、Pause / Resume 公开 API

状态：已实现（2026-03-25）

### 1.1 现状

- 顶层 Rust 类型当前是 `Downloader`，不是 `DownloadManager`。
- `Downloader::download(spec)` 同步返回 `DownloadHandle`，真正的下载在后台 `tokio::spawn` 任务中执行；它不是 `async fn`，也不在启动时返回 `Result`。
- `DownloadHandle` 目前仅暴露 `progress()`、`subscribe_progress()`、`cancel()`、`wait()`。
- `DownloadState::Paused` 枚举变体已定义，但当前运行路径不会进入该状态。
- 当前停止信号是 `watch<bool>`，session 层只能区分“继续”与“停止”，不能区分“取消”与“暂停”。
- 断点续传实际上已经存在：只要 `spec.resume = true` 且同一输出路径对应的控制文件存在，下一次调用 `Downloader::download(spec)` 时会自动尝试恢复。
- 当前 `cancel()` 的语义是终止当前任务；控制文件保存发生在 session 内部的取消/失败路径中，而不是 handle 层单独维护。
- Python 绑定当前对象 API 是 `Downloader` / `DownloadTask`，没有持久化任务管理器这一层公开抽象。

### 1.2 目标

仅支持方案 A，也就是“有状态地停止”，而不是“挂起并保活原任务”。

- `pause()` 的语义是：请求当前下载任务尽快停止，并尽可能把控制文件落盘。
- 当前 `DownloadHandle` 会结束，`wait()` 应当以“暂停”而不是“取消”结束。
- 恢复下载时不复用原 handle，而是由调用方再次发起一次 `Downloader::download(spec)`。
- 库负责提供“暂停时安全落盘 + 后续续传”的能力；任务列表、恢复时机和生命周期编排由调用方自行管理。

### 1.3 设计方案

最低成本方案不是“pause 后在原 handle 上 resume”，而是：

1. 新增停止原因枚举，替换当前 `watch<bool>`。

   例如：

   ```rust
   #[derive(Debug, Clone, Copy, PartialEq, Eq)]
   enum StopSignal {
       Running,
       Cancel,
       Pause,
   }
   ```

   这样 session 层才能在相同的停止检测点上区分两种行为。

   **涉及的改动清单（`watch<bool>` → `watch<StopSignal>`）：**

   | 文件 | 位置 | 说明 |
   |------|------|------|
   | `src/manager.rs` | `DownloadHandle.cancel_tx` 字段 | 类型从 `watch::Sender<bool>` 改为 `watch::Sender<StopSignal>` |
   | `src/manager.rs` | `DownloadHandle::cancel()` | `send(true)` → `send(StopSignal::Cancel)` |
   | `src/manager.rs` | `DownloadHandle::pause()`（新增） | `send(StopSignal::Pause)` |
   | `src/session.rs` | `run_download()` 签名 | `cancel_rx: watch::Receiver<bool>` → `watch::Receiver<StopSignal>` |
   | `src/session.rs` | `run_download_inner()` 签名 | 同上 |
   | `src/session.rs` | `retry_with_backoff()` 签名及内部检测 | `*cancel_rx.borrow_and_update()` 需改为匹配 `StopSignal::Cancel \| Pause` |
   | `src/session.rs` | `stream_single()` 签名及 cancel 分支 | 需区分 Pause/Cancel 分别设置 `DownloadState` 和返回对应 error |
   | `src/session.rs` | `run_multi_worker()` monitor 循环 | cancel 分支需区分；pause 时同样 abort workers 并落盘控制文件 |
   | `src/session.rs` | `worker_loop()` 签名及内部 `if *cancel_rx.borrow()` | 需改为匹配 `StopSignal` |
   | `src/session.rs` | `run_single_connection()` 的 error 处理 | `matches!(e, DownloadError::Cancelled)` 需补充 `Paused` |

   总计涉及 **2 个文件、7+ 个函数签名**。建议以 `StopSignal` 枚举替换为第一步，编译器会自动标出所有需要适配的位置。

2. 在 `DownloadHandle` 上新增 `pub fn pause(&self)`。

   - 行为上与当前 `cancel()` 类似，都是向后台任务发停止信号。
   - 但它发送的是 `StopSignal::Pause`，不是 `Cancel`。
   - 仅在 handle 层修改 `ProgressSnapshot::state` 不够，因为当前最终状态和返回结果都由 session 层决定。

3. 在 `session.rs` 的单连接、多连接、重试等待等所有停止分支里统一处理 `Pause`。

   需要保证：

   - `ProgressSnapshot::state` 写成 `Paused`，而不是 `Cancelled`。
   - 在可恢复场景下尽量先 flush writer，再保存控制文件。
   - `wait()` 返回值能区分 “pause” 和 “cancel”。

4. 为 `DownloadError` 增加单独的 `Paused` 变体，避免复用 `Cancelled`。

   这是必要的，因为当前 `wait()` 只返回 `Result<(), DownloadError>`。如果 pause 仍然返回 `Cancelled`，那么 Rust/Python 两侧都会把“暂停”误判成“取消”。
   **语义澄清：** `Paused` 和 `Cancelled` 严格来说都不是"错误"，而是用户主动发起的停止行为。但为了保持与当前 `Cancelled` 一致的建模方式（即复用 `Result<(), DownloadError>` 而非引入新的 `DownloadOutcome` 枚举），仍然放在 `DownloadError` 中。API 文档中应明确标注这两者属于 "user-initiated stop"，而非真正的运行时错误。
5. 不建议新增 `Downloader::resume_download(...)`。

   原因：

   - 当前恢复逻辑已经内建在 `Downloader::download(spec)` 中。
   - 只要 `spec.resume = true` 且解析出的最终输出路径不变，就会自动读取控制文件。
   - 新增一个单独的 `resume_download()` 只会重复已有语义，反而增加 API 面和文档负担。

6. Python 侧新增 `DownloadTask.pause()`，但不需要引入新的 manager/spec 包装类型。

   更符合现有绑定结构的做法是：

   - `DownloadTask.pause()`
   - `DownloadTask.wait()` 在暂停时抛出单独的 `PausedError`（继承 `BytehaulError`，与 `CancelledError` 平级）
   - 恢复时继续调用现有的 `Downloader.download(...)` / 顶层 `download(...)`

   **Python 侧典型使用流程：**

   ```python
   from bytehaul import download, PausedError, CancelledError

   # 启动下载
   task = download(url, output_path="file.zip", resume=True)

   # 某个时刻暂停
   task.pause()
   try:
       task.wait()
   except PausedError:
       print("下载已暂停，稍后可恢复")
   except CancelledError:
       print("下载已取消")

   # 恢复下载 — 创建新任务，自动从控制文件续传
   task2 = download(url, output_path="file.zip", resume=True)
   task2.wait()  # 继续下载直到完成
   ```

**测试**

7. 新增集成测试 `tests/m8_pause_resume.rs`：

   - 下载中途 `pause()`，验证 `progress().state == Paused`
   - `wait()` 返回的是 `Paused` 语义，不是 `Cancelled`
   - 控制文件已落盘
   - 使用相同下载参数再次 `download()` 后能够续传并完成
   - 单连接和多连接路径都至少覆盖一个用例

### 1.4 实现结果

- 已将内部停止信号从 `watch<bool>` 升级为 `StopSignal::{Running, Cancel, Pause}`
- 已新增 `DownloadHandle::pause()`，并在单连接、多连接、重试等待与 worker 停止路径中统一区分 Pause / Cancel
- 已新增 `DownloadError::Paused`，`wait()` 现在能正确区分“暂停”与“取消”
- 已为 Python 绑定新增 `DownloadTask.pause()` 与 `PausedError`
- 已补充 `tests/m8_pause_resume.rs`，覆盖单连接和多连接暂停后续传完成的集成场景

---

## 二、ETA（预计剩余时间）计算

状态：已实现（2026-03-25）

### 2.1 现状

- `ProgressSnapshot` 已有 `speed_bytes_per_sec`、`total_size`、`downloaded`、`start_time`。
- 当前 `speed_bytes_per_sec` 不是“瞬时速度”，而是从 `start_time` 到当前时刻的平均速度。
- 库自身不计算 ETA，用户只能自行 `(total - downloaded) / speed`。
- 直接使用当前平均速度计算 ETA 虽然简单，但在速度变化时会明显滞后；如果再对这个平均值做一层 EWMA，反而可能双重平滑、响应更慢。

### 2.2 修订后的目标

在 `ProgressSnapshot` 中增加 `eta_secs: Option<f64>`，由核心下载循环基于“最近一段时间的吞吐样本”计算，而不是直接对“全程平均速度”二次平滑。

### 2.3 设计方案

**Rust 侧**

1. 建议新增 ETA 估算器模块，例如 `src/eta.rs`。

   更适合当前实现的接口是基于增量样本：

   ```rust
   pub struct EtaEstimator {
       ewma_speed: Option<f64>,
       alpha: f64,
   }

   impl EtaEstimator {
       pub fn new(alpha: f64) -> Self;
       pub fn update_sample(&mut self, delta_bytes: u64, delta_secs: f64);
       pub fn estimate(&self, remaining_bytes: u64) -> Option<f64>;
   }
   ```

   这里输入的是“最近一段时间的样本速度”，不是 `downloaded / total_elapsed`。

2. 不应写成“集成到 Session struct”。

   当前没有公开的 `Session` 结构体；实现实际位于 `src/session.rs` 的几个下载循环中。因此更准确的落点是：

   - `stream_single(...)` 内维护单连接下载的 ETA 状态
   - `run_multi_worker(...)` 的 progress timer 内维护多连接下载的 ETA 状态

3. 采样建议：

   **推荐默认 alpha = 0.3。** 选择理由：alpha 越大对最新样本越敏感，越小越平滑。0.3 在典型网络场景下兼顾响应性和稳定性，等效于大约最近 3 个样本的加权窗口。可在 `EtaEstimator::new()` 中硬编码，暂不暴露给用户。

   - **单连接路径：** 不在每个 chunk 到达时立即更新 EWMA（chunk 大小差异太大，会引入噪声）。改为按固定时间窗口（建议 1s）累积 `delta_bytes`，窗口结束时计算一次样本速度并调用 `update_sample()`。实现上可在 `stream_single()` 内维护一个 `last_sample_time: Instant` 和 `bytes_since_last_sample: u64`，当 `elapsed >= 1s` 时触发一次采样。
   - **多连接路径：** 沿用现有 200ms progress tick，基于两次 tick 间的 `downloaded` 增量计算样本速度。因为 200ms 间隔已经足够稳定，无需额外累积窗口。
   - **边界条件（需在文档中固定）：**
     - `delta_secs <= 0`：跳过本次采样，保留上一个 EWMA 值
     - `delta_bytes == 0`：正常更新（样本速度 = 0），EWMA 会自然衰减
     - `ewma_speed < 1.0` bytes/sec：`estimate()` 返回 `None`（避免天文数字 ETA）
     - 尚无任何样本数据时：`estimate()` 返回 `None`

4. 扩展 `ProgressSnapshot`：

   ```rust
   pub struct ProgressSnapshot {
       // ... existing fields
       pub eta_secs: Option<f64>,
   }
   ```

5. Python 绑定同步扩展 `PyProgressSnapshot.eta_secs`，直接映射即可。

**测试**

6. `src/eta.rs` 单元测试建议覆盖：

   - 恒定吞吐样本下 ETA 稳定收敛
   - 速度从高到低变化时 ETA 平滑上升
   - `delta_bytes == 0` 或速度趋近于 0 时返回 `None`
   - `total_size == None` 时返回 `None`

7. 集成测试建议验证：

   - 下载过程中 `eta_secs` 会从 `None` 变为合理数值
   - 下载接近完成时 `eta_secs` 逐步下降
   - **完成态语义（已固定）：** `eta_secs = Some(0.0)`，表示"剩余 0 秒"。`None` 保留给"无法估算"的情况（如 `total_size` 未知、尚无样本数据、速度过低）

### 2.4 实现结果

- 已新增 `src/eta.rs`，使用 EWMA 吞吐样本估算剩余时间
- 已为 `ProgressSnapshot` 增加 `eta_secs: Option<f64>` 字段
- 单连接路径已按 1 秒采样窗口估算 ETA，多连接路径已按现有 200ms progress tick 估算 ETA
- 完成态的 `eta_secs` 固定为 `Some(0.0)`，未知或速度过低场景保持 `None`
- Python 绑定的 `PyProgressSnapshot.eta_secs` 已同步可用
- 已补充单元测试和单连接、多连接 ETA 集成测试

---

## 三、文件名自动检测

状态：已实现（2026-03-25）

### 3.1 现状

- 当前 Rust API 要求显式提供 `output_path`：`DownloadSpec::new(url, output_path)`。
- `DownloadSpec.output_path` 当前是公开的 `PathBuf` 字段，不是 `Option<PathBuf>`。
- 当前 API 还没有单独的 `output_dir` 语义。
- HTTP 响应头中的 `Content-Disposition` 还没有进入 `ResponseMeta`。
- 当前恢复流程在真正发起 fresh 下载前就会根据 `output_path` 推导控制文件路径并尝试加载控制文件。

### 3.2 修订后的目标

- 支持 `output_path: Option<PathBuf>`，但其语义改为类似 aria2 的 `--out`。
- `output_path` 表示输出文件名或相对输出路径，而不是最终完整路径。
- 支持 `output_dir: Option<PathBuf>`，语义类似 aria2 的 `--dir`。
- 当 `output_path == None` 时，自动推断文件名，再与 `output_dir` 组合出最终路径。
- 推断优先级仍然是：`Content-Disposition` > URL path 最后一段 > 默认名 `download`。

### 3.3 设计方案

**Rust 侧**

1. 新增文件名解析模块 `src/filename.rs`。

   ```rust
   pub fn parse_content_disposition(header_value: &str) -> Option<String>;
   pub fn filename_from_url(url: &str) -> Option<String>;
   pub fn detect_filename(content_disposition: Option<&str>, url: &str) -> String;
   ```

   解析规则保持原计划方向，但需要额外强调：

   - 先尝试 `filename*=`，再尝试 `filename=`
   - 对结果做路径安全清理，禁止目录穿越和控制字符
   - Windows 保留名、尾随点/空格等平台细节也应一并清理
   - 空文件名 / 纯空格文件名 → 回退到 URL 文件名
   - 非 UTF-8 `filename=`（RFC 5987 之外的旧浏览器行为）：尝试 Latin-1 解码，失败则回退到 URL 文件名
   - 超长文件名截断：Windows 下单个文件名组件不超过 255 字符，超出部分截断并保留原扩展名

2. 扩展 `ResponseMeta`，加入：

   ```rust
   pub content_disposition: Option<String>
   ```

   这样现有的 range probe / GET 响应都能携带文件名候选信息。

3. 调整 `DownloadSpec` 公开字段语义：

   ```rust
   pub struct DownloadSpec {
       pub url: String,
       pub output_path: Option<PathBuf>,
       pub output_dir: Option<PathBuf>,
       // ...
   }
   ```

   推荐规则：

   - `output_dir` 表示目标目录；`output_path` 表示相对该目录的输出名，语义对齐 aria2 的 `--dir` / `--out`
   - `output_path = Some(path)`：表示调用方显式指定输出名或相对输出路径，不再做自动文件名检测
   - `output_path = None`：表示需要自动文件名检测
   - `output_dir = Some(dir)`：最终路径始终落在该目录下
   - `output_dir = None`：等价于使用当前工作目录
   - `output_path = Some(path)` 且 `output_dir = Some(dir)`：最终路径为 `dir.join(normalized_relative_output_path)`
   - `output_path = Some(path)` 且 `output_dir = None`：最终路径默认为 `cwd.join(normalized_relative_output_path)`；为了兼容既有调用方，实现上也接受绝对 `output_path`
   - `output_path = None` 且 `output_dir = Some(dir)`：最终路径为 `dir.join(detected_filename)`
   - `output_path = None` 且 `output_dir = None`：最终路径为 `cwd.join(detected_filename)`
   - 若 `output_path` 含有根路径、盘符或目录穿越片段，需要在解析阶段拒绝或归一化为安全的相对路径；文档与实现需固定一致策略

4. 直接修改 `DownloadSpec::new` 签名，无需向后兼容。

   ```rust
   impl DownloadSpec {
       /// 自动文件名模式：`output_path = None`，由库从响应头/URL 推断。
       pub fn new(url: impl Into<String>) -> Self {
           Self {
               url: url.into(),
               output_path: None,
               output_dir: None,
               // ... 其余字段使用 default
           }
       }
   }
   ```

   通过 builder 风格的 setter 提供 `output_path` 和 `output_dir`：

   ```rust
   pub fn output_path(mut self, path: impl Into<PathBuf>) -> Self {
       self.output_path = Some(path.into());
       self
   }
   pub fn output_dir(mut self, dir: impl Into<PathBuf>) -> Self {
       self.output_dir = Some(dir.into());
       self
   }
   ```

   典型用法：

   ```rust
   // 自动文件名
   let spec = DownloadSpec::new("https://example.com/file.zip");
   // 显式文件名
   let spec = DownloadSpec::new("https://example.com/file.zip")
       .output_path("my_file.zip");
   // 指定目录 + 显式文件名
   let spec = DownloadSpec::new("https://example.com/file.zip")
       .output_dir("/tmp")
       .output_path("my_file.zip");
   ```

   所有调用 `DownloadSpec::new` 的现有代码（包括集成测试、Python 绑定）需一并更新。
   `session.rs` 内部不再直接使用 `spec.output_path`，改为使用 `resolved_output_path`（见下方第 6a 点）。
5. 不建议额外引入 HEAD 请求作为主方案。

   HEAD 在很多服务上并不可靠，和 GET 的响应头也可能不一致。更符合当前实现的做法是复用“已经必须发出的第一个响应”：

   - fresh multi 路径：复用初始 `Range` probe 的响应头
   - fresh single 路径：复用 fallback GET 的响应头

6. 需要明确处理“文件名解析 + resume”的流程重排。

   这是该功能真正复杂的地方。当前 resume 是先根据 `output_path` 算出控制文件路径，再尝试加载控制文件；但改成 aria2 风格后，控制文件路径应基于最终解析出的 `resolved_output_path`，而不是原始 `output_path` 字段。

   因此至少要在设计上明确两条路径：

   - 显式 `output_path`：先将 `output_dir + output_path` 解析为 `resolved_output_path`，再沿用现有 resume 流程
   - 自动文件名：先拿到第一个响应头，解析出 `detected_filename` 与最终 `resolved_output_path`，再基于该路径决定控制文件位置和后续 resume 行为

   这意味着文件名能力不是“只改 `ResponseMeta` 和 `DownloadSpec`”就能完成，而是会引入一个统一的“路径解析阶段”，并影响 `run_download_inner(...)` 的启动顺序。
   **6a. `resolved_output_path` 解析阶段详细流程：**

   在 `run_download_inner()` 入口处引入统一的路径解析函数：

   ```rust
   /// 解析最终输出路径，不涉及网络请求。
   /// 仅当 output_path = Some 时可在发起请求前调用。
   fn resolve_output_path_static(spec: &DownloadSpec) -> Option<PathBuf> {
       let out_name = spec.output_path.as_ref()?;
       let dir = spec.output_dir.clone().unwrap_or_else(|| std::env::current_dir().unwrap());
       Some(dir.join(sanitize_relative_path(out_name)))
   }

   /// 自动文件名模式下，需要等拿到 HTTP 响应头后才能解析。
   fn resolve_output_path_auto(
       spec: &DownloadSpec,
       content_disposition: Option<&str>,
   ) -> PathBuf {
       let filename = detect_filename(content_disposition, &spec.url);
       let dir = spec.output_dir.clone().unwrap_or_else(|| std::env::current_dir().unwrap());
       dir.join(filename)
   }
   ```

   **6b. `run_download_inner()` 重排后的流程：**

   ```text
   ┌─────────────────────────────────────────────────────┐
   │ 1. 检查 output_path 是否为 Some                      │
   │    ├─ Some → resolve_output_path_static()            │
   │    │         得到 resolved_path                       │
   │    │         → Phase A: 尝试 resume（现有逻辑）       │
   │    │         → Phase B: fresh download（现有逻辑）    │
   │    └─ None → Phase C: 自动文件名流程                  │
   │              → 发出首个 HTTP 请求（Range probe 或 GET）│
   │              → 从响应头解析 Content-Disposition        │
   │              → resolve_output_path_auto()             │
   │              → 得到 resolved_path                     │
   │              → 基于 resolved_path 尝试 resume         │
   │              → 若控制文件存在且匹配，走 resume 路径    │
   │              → 否则走 fresh 路径（复用已有响应）       │
   └─────────────────────────────────────────────────────┘
   ```

   **注意：** Phase C 中 resume 检查发生在首个请求之后，这与现有流程（先 resume 再请求）不同。这是自动文件名模式的固有限制——必须先知道文件名才能定位控制文件。

   **6c. 控制文件路径策略（resume + 自动文件名）：**

   控制文件路径始终基于 `resolved_output_path`（即 `{resolved_path}.bytehaul`），而非原始 `output_path` 字段。这意味着：

   - **显式 `output_path` 模式：** 行为与现有完全一致
   - **自动文件名模式：** 控制文件路径在首次确定文件名后才能计算

   **风险：服务器在不同请求中返回不同的 `Content-Disposition`。** 缓解策略：

   - 控制文件内部已保存 `url` 字段，加载后可用于验证
   - 建议在 `ControlSnapshot` 中新增 `resolved_filename: Option<String>` 字段，首次保存时记录
   - resume 加载时若发现当前检测到的文件名与控制文件记录不一致，视为 resume 失败，丢弃控制文件重新开始
   - 这样可以避免旧控制文件遗弃导致的磁盘泄漏
7. Python 侧同步支持可选 `output_path` 与 `output_dir`。

   更贴近现状的方案是：

   - 顶层 `download(url, output_path=None, output_dir=None, ...)`
   - `Downloader.download(url, output_path=None, output_dir=None, ...)`
   - `output_path` 的文档语义同步改为“文件名/相对输出路径”，而不是“完整目标路径”

8. 如果 Python/UI 需要展示最终确定的文件名，优先考虑给 `DownloadTask` 增加只读 getter，例如 `resolved_output_path()`；不建议为了展示路径而强行把该信息混进纯进度字段，除非后续确认这对订阅模型更合适。

**测试**

9. `src/filename.rs` 单元测试继续覆盖主要用例，并补充：

   - Windows 非法文件名字符和保留名处理
   - `filename*=` 与 `filename=` 同时存在时的优先级
   - header 存在但解析失败时回退到 URL 文件名
   - `output_path` 作为相对输出名时的归一化与安全校验
   - `output_path` / `output_dir` 组合后的最终路径解析

10. 集成测试建议覆盖：

   - 服务器返回 `Content-Disposition`，最终文件名正确
   - 无 `Content-Disposition` 时从 URL path 推断
   - `output_path + output_dir` 组合时，最终输出路径正确
   - `output_path` 含相对层级时，最终路径与文档语义一致
   - 自动文件名模式下与 resume 共存时，能正确找到控制文件并续传
   - 目标文件已存在时的冲突策略行为明确且可验证

---

## 四、实施优先级与依赖关系

修订后的复杂度评估如下：

```text
优先级    功能                     复杂度      前置依赖
────────────────────────────────────────────────────────
P0       ETA 计算                  低          无
P1       Pause/Resume 方案 A      中          无
P2       文件名自动检测            中-高       需梳理启动顺序与 resume 路径
```

### 建议实施顺序

1. **ETA 计算**

   改动面最小，只涉及 `ProgressSnapshot`、Python 映射以及 `session.rs` 内部采样逻辑。

   **Exit Criteria：**
   - `src/eta.rs` 单元测试全部通过
   - `ProgressSnapshot.eta_secs` 字段可用
   - Python `PyProgressSnapshot.eta_secs` 可用
   - `cargo tarpaulin --engine llvm --workspace --all-targets --out Stdout --fail-under 95` 通过
   - 现有集成测试不因新字段引入而回归

2. **Pause/Resume 方案 A**

   当前库已经具备控制文件续传能力，只缺少"暂停"这个明确语义和结果区分，投入产出比高。

   **Exit Criteria：**
   - `StopSignal` 枚举替换 `watch<bool>`，编译通过
   - `DownloadHandle::pause()` 可用
   - `DownloadError::Paused` 变体已实现，`wait()` 返回正确
   - Python `DownloadTask.pause()` 和 `PausedError` 可用
   - `tests/m8_pause_resume.rs` 集成测试通过（单连接 + 多连接）
   - `cargo tarpaulin --engine llvm --workspace --all-targets --out Stdout --fail-under 95` 通过
   - 现有 m1–m7 测试全部不回归

3. **文件名自动检测**

   该功能表面看像 API 小改动，实际上会触及启动顺序、控制文件定位和公开接口设计，建议在前两项稳定后再做。

   **Exit Criteria：**
   - `src/filename.rs` 单元测试全部通过（含 Windows 保留名、路径穿越等安全用例）
   - `ResponseMeta.content_disposition` 可用
   - `DownloadSpec::new(url)` 新签名可用，`.output_path()` / `.output_dir()` setter 可用
   - `run_download_inner()` 的路径解析阶段正确处理显式/自动两条路径
   - 自动文件名 + resume 场景能正确定位控制文件
   - Python `download(url, output_path=None)` 可用
   - 集成测试覆盖 `Content-Disposition`、URL 推断、`output_dir` 组合、resume 共存
   - `cargo tarpaulin --engine llvm --workspace --all-targets --out Stdout --fail-under 95` 通过

---

## 五、风险与缓解

| 风险 | 缓解措施 |
|------|---------|
| 方案 A 的 pause 实际上会结束当前任务，和“原地恢复”语义不完全一致 | 在 API 文档中明确：这是“可恢复停止”，resume 会创建新任务，任务编排由调用方自行管理 |
| pause 若仍复用 `Cancelled` 返回结果，会让上层无法区分暂停与取消 | 增加 `DownloadError::Paused`，Python 侧同步单独映射 |
| `StopSignal` 替换 `watch<bool>` 涉及 7+ 个函数签名，改动面大 | 以枚举替换为第一步，依赖编译器报错驱动修改，逐函数适配；改动清单已在 §1.3.1 列出 |
| `run_multi_worker` 中 pause 时 abort workers 后 writer 可能未 flush | pause 分支与 cancel 分支共用 abort 逻辑后，统一执行 `save_multi_control`；writer 落盘依赖已写入的数据，abort 不影响已提交的 `WriterCommand` |
| ETA 基于全程平均速度会明显滞后 | 基于增量吞吐样本做 EWMA，不对全程平均值二次平滑 |
| 单连接路径每个 chunk 更新 EWMA 会引入噪声 | 改为 1s 固定窗口累积后采样，避免小 chunk 噪声 |
| `Content-Disposition` 解析存在路径穿越和平台兼容风险 | 严格做文件名清理，并覆盖 Windows 特殊规则；补充空文件名、非 UTF-8、超长文件名处理 |
| 自动文件名与当前 resume 流程存在先后依赖冲突 | 明确区分显式输出名和自动文件名两条启动路径，引入统一的 `resolved_output_path` 解析阶段，必要时重排 `run_download_inner(...)` |
| 自动文件名模式下服务器返回不同 `Content-Disposition` 导致控制文件遗弃 | 在 `ControlSnapshot` 中新增 `resolved_filename` 字段，resume 时校验文件名一致性，不匹配则丢弃控制文件重新开始 |
| `output_path: Option<PathBuf>` + `output_dir` 是公开 API 变更，且 `output_path` 名称容易让人误解成完整路径 | 在 Rust/Python 文档中固定语义：`output_dir` 对齐 aria2 `--dir`，`output_path` 对齐 aria2 `--out`，始终按相对输出名处理并做安全归一化 |
| `output_path` 从 `PathBuf` 改为 `Option<PathBuf>` 会影响既有调用方 | `DownloadSpec::new` 已切换为新签名；同时保留“未设置 `output_dir` 时允许绝对 `output_path`”的兼容路径，降低迁移成本 |

### 3.4 实现结果

- 已新增 `src/filename.rs`，实现 `Content-Disposition` / URL 文件名解析、Windows 保留名与路径安全清理、超长文件名截断
- 已将 `ResponseMeta` 扩展为包含 `content_disposition`
- 已将 `DownloadSpec::new(...)` 调整为仅接收 `url`，并提供 `.output_path(...)` / `.output_dir(...)`
- `src/session.rs` 已引入统一的 `resolved_output_path` 解析阶段，分别处理显式输出路径与自动文件名流程
- 自动文件名模式现已支持 resume，控制文件路径基于最终解析出的输出文件名定位
- Rust 集成测试已覆盖 `Content-Disposition`、URL 回退、默认名 `download`、`output_dir` 组合、自动文件名 + pause/resume 共存
- Python 绑定已支持 `download(url, output_path=None, output_dir=None, ...)` 与 `Downloader.download(...)` 的同等语义
- 实现上保留了未设置 `output_dir` 时直接传绝对 `output_path` 的兼容路径，避免无谓破坏既有调用方式
- 验证结果：`cargo test --workspace --all-targets`、Python `pytest` 全部通过，`cargo tarpaulin --engine llvm --workspace --all-targets --out Stdout --fail-under 95` 达到 `95.09%`
| 目标文件名冲突时行为不明确 | 在设计阶段先固定策略，推荐默认报错，后续再考虑自动编号 |
