# Repository Evidence

## Current behavior

- `src/session/mod.rs:39-75` — fresh Range probe/plain GET 使用 `retry_with_backoff`，scope 在 response acquisition 结束。
- `src/session/mod.rs:245-430` — fresh strategy 把已打开 response 交给 single runner；multi runner 同时获得 client。
- `src/session/resume.rs:239-291` — single resume probe 自成 retry scope，校验后把 response 交给 single runner。
- `src/session/single.rs:40-188` — single runner 只持有 response/writer/control 生命周期，不持有请求能力。
- `src/session/single.rs:190-353` — body error 直接返回；已知 total 下当前没有 EOF final-offset 或 over-read 校验。
- `src/session/single.rs:355-422` — control snapshot 的 `FlushAll` ack 返回 `written_bytes`，可复用为 retry barrier。
- `src/session/multi.rs:569-711` — segment attempt 覆盖请求、校验和 body，但独立计算无 jitter backoff。
- `src/session/multi.rs:723-875` — 集中 Range 校验与 body 精确长度检查已有可复用模式。
- `src/session/range_validate.rs:1-186` — `ResumeProbe` 严格验证 status、range 和可选 Content-Length。
- `src/storage/writer.rs:81-148` — single 数据直接顺序落盘，`FlushAll` FIFO ack 可确认此前命令处理完成。
- `src/storage/file.rs:14-40` — `create_output_file` 可 recreate/truncate，并保留预分配策略。
- `src/error.rs:163-206` — body transport、Retry-After 与 retryable 分类现状。

## Prior decision record

`trellis mem` session `01a03bbb-987f-73b3-8c9d-1cb6213da13e` 已确认：

- 单/多连接差异来自 bytehaul 当前抽象边界，不是协议要求。
- 不建议为了对齐 aria2 而改变 bytehaul 的次数、HTTP 状态或 backoff 公开语义。
- 正确修复边界是完整 transfer attempt，而不是只在 `stream_single` 内循环。

## Source requirement

`docs/private/single-connection-retry-plan.zh-CN.md` 定义 M1-M4、正确性约束、非目标与测试矩阵，是本任务的产品/技术意图来源。
