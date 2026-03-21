# Python PyO3 绑定接入计划

## 1. 背景

当前仓库的 `bytehaul` 已经是一个职责较清晰的 Rust 下载核心库，核心能力集中在：

- 下载配置与公开 API
- 调度、重试、限速、校验
- 控制文件、缓存、落盘

如果直接把 `PyO3` 代码混进当前根 crate，会把 Python 打包、解释器生命周期、异常映射、运行时管理这些绑定层问题一起带进核心库，后续维护会越来越重。

因此本计划采用：

- 同一仓库内实现
- Rust 核心与 Python 绑定分 crate 隔离
- 先做最小可用 Python 绑定，再逐步补齐进度、取消和打包发布

## 2. 目标

- 保持根 crate 继续作为纯 Rust 核心库存在
- 在本仓库内新增独立的 Python binding crate
- 使用 `PyO3` + `maturin` 产出可安装的 Python 扩展模块
- 一期先让 Python 能稳定调用下载能力
- 后续再补进度查询、取消、错误细分和 wheel 构建

## 3. 非目标

- 一期不新开独立仓库
- 一期不把 `PyO3` 直接加入根 crate
- 一期不强求把所有 Rust 内部类型 1:1 暴露给 Python
- 一期不优先接入 Python `asyncio`
- 一期不追求复杂的多平台发布流水线

## 4. 总体方案

推荐结构如下：

```text
rust-aria2/
  Cargo.toml
  src/
  docs/
  bindings/
    python/
      Cargo.toml
      pyproject.toml
      src/
        lib.rs
      python/
        bytehaul/__init__.py
      tests/
```

其中：

- 根 crate 继续提供下载核心逻辑
- `bindings/python` 负责 Python 类型包装、异常映射、运行时托管和打包
- 根目录 `Cargo.toml` 增加 workspace 配置，把 `bindings/python` 纳入同仓库构建

## 5. 关键设计决策

### 5.1 仓库组织

采用“同仓库，多 crate”模式：

- 根 crate：`bytehaul`
- Python 绑定 crate：建议命名为 `bytehaul-python` 或 `bytehaul-py`
- Python import 名：建议使用 `bytehaul`

这样可以同时保证：

- Rust 核心不会被 Python 专属依赖污染
- 绑定层可以独立演进
- 版本发布时仍然可以在同一仓库协同管理

### 5.2 Python 版本策略

一期明确支持：

- `CPython 3.9+`

暂不承诺：

- Python 3.8 及以下
- `PyPy`
- free-threaded CPython（后续如有需要再单独评估）

落地建议：

- 在 `bindings/python/pyproject.toml` 中声明 `requires-python = ">=3.9"`
- 如无特殊兼容性阻碍，优先采用 `pyo3` 的 `abi3-py39` 特性，减少按 Python 小版本分别构建 wheel 的复杂度
- 发布与测试矩阵一期先覆盖常规 `CPython 3.9+`

这样做的原因是：

- Python 3.9 仍能覆盖较广泛的用户环境
- 可以避免继续兼容更老版本解释器带来的额外维护成本
- 与 `PyO3` / `maturin` 的常见打包路径更一致，便于后续补齐 wheel 构建矩阵

### 5.3 运行时策略

当前核心库内部已经依赖 `tokio` 并使用异步任务模型，因此 Python 绑定不能假设调用方已经准备好了 Rust runtime。

推荐策略：

- 由 Python binding crate 在内部持有自己的 `tokio::runtime::Runtime`
- Python 的阻塞型 API 在进入真正等待逻辑时释放 GIL
- 一期先提供“同步可用”的 Python 调用方式
- 如后续确实有需求，再评估 `pyo3-async-runtimes`（`pyo3-asyncio` 已在 PyO3 0.21 后废弃）

一期不建议直接把 Python `asyncio` 集成作为默认路线，因为这会显著增加运行时整合复杂度。

### 5.4 API 暴露策略

一期优先做 Python 友好的包装，而不是把 Rust API 原封不动搬过去。

建议分两层：

#### M1 最小可用 API

- `download(url, output_path, **options) -> None`

特点：

- 调用简单
- 便于尽快验证打包链路和基础功能
- 更容易处理 runtime 生命周期

#### M2 面向长期的对象 API

- `Downloader(connect_timeout=30.0)` — 对应 Rust 侧 `DownloaderBuilder` + `Downloader`，Python 构造函数直接接收 builder 参数
- `task = downloader.download(url, output_path, **options)` — 返回 `DownloadTask`
- `task.wait()` — 对应 `DownloadHandle::wait()`
- `task.cancel()` — 对应 `DownloadHandle::cancel()`
- `task.progress()` — 对应 `DownloadHandle::progress()`，返回 Python 友好的进度对象
- `task.on_progress(callback)` — 基于 `DownloadHandle::subscribe_progress()` 实现回调式进度通知（可选）

 Python 侧 `DownloadTask` 对应 Rust 侧的 `DownloadHandle`，更名是为了让 Python 用户更直观理解其语义。

这一层可以逐步对齐 Rust 侧的 `Downloader` / `DownloadHandle` 能力，但不要求在第一阶段全部实现。

## 6. Python 与 Rust 类型映射

### 6.1 DownloadSpec 参数映射

| Python 参数 | Python 类型 | Rust 字段 | Rust 类型 | 默认值 |
|---|---|---|---|---|
| `url` | `str` | `url` | `String` | （必填） |
| `output_path` | `str \| pathlib.Path` | `output_path` | `PathBuf` | （必填） |
| `headers` | `dict[str, str]` | `headers` | `HashMap<String, String>` | `{}` |
| `max_connections` | `int` | `max_connections` | `u32` | `4` |
| `connect_timeout` | `float`（秒） | `connect_timeout` | `Duration` | `30.0` |
| `read_timeout` | `float`（秒） | `read_timeout` | `Duration` | `60.0` |
| `memory_budget` | `int` | `memory_budget` | `usize` | `67108864`（64 MiB） |
| `file_allocation` | `"none" \| "prealloc"` | `file_allocation` | `FileAllocation` | `"prealloc"` |
| `resume` | `bool` | `resume` | `bool` | `True` |
| `piece_size` | `int` | `piece_size` | `u64` | `1048576`（1 MiB） |
| `min_split_size` | `int` | `min_split_size` | `u64` | `10485760`（10 MiB） |
| `max_retries` | `int` | `max_retries` | `u32` | `5` |
| `retry_base_delay` | `float`（秒） | `retry_base_delay` | `Duration` | `1.0` |
| `retry_max_delay` | `float`（秒） | `retry_max_delay` | `Duration` | `30.0` |
| `max_download_speed` | `int` | `max_download_speed` | `u64` | `0`（无限制） |
| `checksum_sha256` | `str \| None` | `checksum` | `Option<Checksum>` | `None` |

> **注意：** Rust 侧 `DownloadSpec` 还有 `channel_buffer: usize`（默认 `64`），此为内部调度参数，Python 侧不暴露。

### 6.2 ProgressSnapshot 映射

| Python 属性 | Python 类型 | Rust 字段 | Rust 类型 | 备注 |
|---|---|---|---|---|
| `total_size` | `int \| None` | `total_size` | `Option<u64>` | 文件总大小（未知时为 `None`） |
| `downloaded` | `int` | `downloaded` | `u64` | 已下载字节数 |
| `state` | `str` | `state` | `DownloadState` | 见下方枚举映射 |
| `speed` | `float` | `speed_bytes_per_sec` | `f64` | 当前速度（字节/秒） |
| `elapsed_secs` | `float \| None` | `start_time` | `Option<Instant>` | 绑定层转换为已用秒数 |

> **注意：** Rust 的 `Instant` 无法直接传给 Python。绑定层应在取快照时计算 `start_time.map(|t| t.elapsed().as_secs_f64())`，转为 `float` 秒数暴露给 Python。

### 6.3 DownloadState 枚举映射

| Rust 变体 | Python 字符串 |
|---|---|
| `Pending` | `"pending"` |
| `Downloading` | `"downloading"` |
| `Completed` | `"completed"` |
| `Failed` | `"failed"` |
| `Cancelled` | `"cancelled"` |
| `Paused` | `"paused"` |

建议在 Python `bytehaul` 模块中暴露对应字符串常量，方便用户比较。

### 6.4 映射原则

- Python 侧尽量使用基础类型和字符串枚举
- 不把 Rust 的 `Duration`、`PathBuf`、`HashMap`、`Instant`、枚举直接暴露给 Python 用户
- 绑定层负责把 Python 输入转换为 `DownloadSpec`，把 Rust 输出转换为 Python 友好类型

## 7. 错误模型

一期先建立清晰的异常边界。

### 7.1 异常层次

```text
BytehaulError (基类)
├── CancelledError          ← DownloadError::Cancelled
├── ConfigError              ← 参数校验失败（绑定层自行产生）
└── DownloadFailedError      ← 其余所有 DownloadError 变体
```

### 7.2 Rust DownloadError 各变体映射

| Rust 变体 | Python 异常 | 备注 |
|---|---|---|
| `Cancelled` | `CancelledError` | |
| `HttpStatus { status, message }` | `DownloadFailedError` | 消息中包含 HTTP 状态码 |
| `Http(reqwest::Error)` | `DownloadFailedError` | 网络 / 超时类错误 |
| `Io(std::io::Error)` | `DownloadFailedError` | 文件 I/O 错误 |
| `ResumeMismatch(String)` | `DownloadFailedError` | 恢复元数据不匹配 |
| `ControlFileCorrupted(String)` | `DownloadFailedError` | 控制文件损坏 |
| `ChecksumMismatch { expected, actual }` | `DownloadFailedError` | 校验失败 |
| `ChannelClosed` | `DownloadFailedError` | 内部通道异常关闭 |
| `Other(String)` | `DownloadFailedError` | 兜底错误 |

所有 `DownloadFailedError` 实例应携带可读的 `message` 属性，内容从 Rust 侧 `Display` 实现转换而来。

### 7.3 后续细分（可选）

如果 Python 用户确实需要按错误类型做不同处理，可以在 `DownloadFailedError` 下进一步细分子类：

- `ResumeMismatchError`
- `HttpStatusError`
- `ChecksumMismatchError`
- `ControlFileCorruptedError`

但一期不强求，统一用 `DownloadFailedError` 即可。

Rust 的 `DownloadError` 在绑定层统一转换为 Python 异常，不把内部错误枚举细节直接泄漏出去。

## 8. 代码边界要求

为避免绑定层反向污染核心层，执行时应遵守以下边界：

- 根 crate 不引入 `pyo3`
- 根 crate 不出现任何 Python 专属类型
- 如果需要为绑定层补辅助函数，这些辅助函数也应保持“通用嵌入友好”，而不是只为 Python 服务
- Python 绑定层负责：
  - 参数解析
  - runtime 管理
  - GIL 处理
  - 异常映射
  - 包发布配置

## 9. 建议实施步骤

### M0 ✅ 已完成

- 在根 `Cargo.toml` 中引入 workspace
- 新建 `bindings/python/`
- 新增 `bindings/python/Cargo.toml`
- 新增 `bindings/python/pyproject.toml`
- 新增最小 `PyO3` 模块
- 明确 `requires-python = ">=3.9"`，并完成 `pyo3` 版本特性选择（优先 `abi3-py39`）

已落地：

- 根仓库已切换为 workspace，并纳入 `bindings/python`
- 已新增 `bytehaul-python` crate，采用 mixed project 结构
- 已新增最小 `_bytehaul` 扩展模块与 `python/bytehaul/__init__.py`
- 已配置 `PyO3 0.23`、`abi3-py39`、`maturin` 与 `requires-python = ">=3.9"`
- 已完成 `cargo check -p bytehaul-python`
- 已完成 `maturin build` 以及本地 `import bytehaul` 验证
- Python 开发依赖与虚拟环境改为统一使用 uv 管理

验收标准：

- `maturin develop` 可以成功安装模块
- Python 中可以成功 `import bytehaul`
- 在 Python 3.9+ 环境下可重复完成本地安装

### M1 ✅ 已完成

- 在 binding crate 中实现参数到 `DownloadSpec` 的转换
- 提供阻塞型 `download()` API
- 绑定层自行托管 `tokio` runtime
- 完成基础错误转换

验收标准：

- Python 可调用下载单个文件
- 失败时能得到可读异常
- 阻塞等待期间不会长期占住 GIL

已落地：

- 已提供顶层 `download(url, output_path, **options)` API
- 已完成 Python 参数到 `DownloadSpec` 的映射与基础校验
- 已引入共享 `tokio` runtime，并在阻塞等待阶段通过 `allow_threads` 释放 GIL
- 已建立 `BytehaulError`、`CancelledError`、`ConfigError`、`DownloadFailedError` 基础异常层级
- 已通过本地 HTTP 服务完成 Python 下载成功、404 异常映射、参数校验的端到端验证

### M2 进度与任务句柄

- 引入 `Downloader` / `DownloadTask` 的 Python 包装
- 提供 `wait()`、`cancel()`、`progress()` 能力
- 将 `ProgressSnapshot` 转成 Python 友好的对象或字典

验收标准：

- Python 侧可轮询进度
- Python 侧可取消任务
- 不因对象生命周期导致后台任务泄漏

### M3 测试与打包

- 增加 Rust 侧 binding 单测
- 增加 Python 侧导入与端到端测试
- 明确本地开发命令和构建命令
- 补充安装说明与示例

验收标准：

- 本地可重复执行构建与测试
- 文档足够支撑后续继续迭代

### M4 发布准备

- 统一版本策略
- 补齐 wheel 构建矩阵
- 增加发布文档
- 视需要接入 CI

## 10. 技术选型建议

- Python 绑定：`pyo3`
- 构建与发布：`maturin`
- Python 版本：`CPython 3.9+`
- 打包兼容策略：优先 `abi3-py39` + `requires-python = ">=3.9"`
- runtime：沿用 `tokio`
- Python 测试：`pytest`
- Python 开发环境与依赖管理：`uv`

一期暂不建议引入：

- `pyo3-async-runtimes`（原 `pyo3-asyncio`，自 PyO3 0.21 起已更名）
- 复杂的 Python 端回调式进度系统
- 多层 Python 包装框架

## 11. 主要风险

### 11.1 Tokio runtime 生命周期

当前核心下载逻辑依赖异步任务，绑定层如果 runtime 生命周期管理不清楚，很容易出现：

- `spawn` 时没有 runtime
- 任务尚未完成 runtime 已被销毁
- Python 对象提前释放导致后台任务悬空

### 11.2 GIL 与阻塞等待

如果 `wait()` 或同步下载 API 在持有 GIL 时长时间阻塞，会导致 Python 主线程体验很差。

因此需要在阻塞等待阶段使用 `allow_threads` 一类机制释放 GIL。

### 11.3 类型转换复杂度

`Duration`、`Instant`、路径、枚举、headers、校验参数都需要经过绑定层转换，如果直接暴露 Rust 类型，会让 Python API 很不自然。

其中 `ProgressSnapshot.start_time` 类型为 `Option<Instant>`，而 `Instant` 无法跨语言序列化——绑定层应在快照获取时将其转换为已用秒数（`elapsed_secs: float`），而非尝试传递原始时间点。

### 11.4 打包与平台差异

Windows、Linux、macOS 的 Python 扩展打包细节不同，一期先跑通本地开发链路即可，不要过早堆 CI 复杂度。

## 12. 里程碑顺序建议

建议按下面顺序推进：

1. 先改仓库结构和 workspace
2. 再做最小 `import bytehaul`
3. 再做阻塞型 `download()`
4. 再补 `Downloader` / `DownloadTask`
5. 最后补测试、文档和发布脚本

原因是先打通打包链路，能最快验证方向是否正确；如果一开始就做完整对象模型和 `asyncio` 兼容，返工成本会更高。

## 13. 一句话结论

`PyO3` 支持应当在当前仓库内完成，但必须以“独立 binding crate”的方式接入：让 `bytehaul` 继续专注下载核心，把 Python 运行时、API 包装和发布问题收拢到 `bindings/python` 中。

