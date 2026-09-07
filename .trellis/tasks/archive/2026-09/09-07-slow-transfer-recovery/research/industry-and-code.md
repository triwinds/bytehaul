# 业内实现与仓库证据

调研日期：2026-09-07。仓库基线：`9196599`。已阅读以下指定版本源码和官方文档；版本用于复核算法，不声称它们是最新发行版。仅提炼机制，不复制第三方实现。

## 参考实现

| 来源 | 已核实的机制 | 对 Bytehaul 的取舍 |
| --- | --- | --- |
| [curl 8.10.1 speedcheck.c](https://github.com/curl/curl/blob/curl-8_10_1/lib/speedcheck.c#L41) | 速度低于阈值时累计持续时间，恢复后重置；接收暂停时跳过；独立安排下一次检查 | 采用持续窗口、恢复重置、独立定时检查；本项目还需显式排除限速、内存和写入等待 |
| [curl 低速选项](https://curl.se/libcurl/c/CURLOPT_LOW_SPEED_LIMIT.html) | 速度阈值和持续时间成对工作，默认关闭 | 可配置绝对下限，不能把示例阈值作为所有网络的默认 |
| [aria2 1.37.0 DownloadCommand.cc](https://github.com/aria2/aria2/blob/release-1.37.0/src/DownloadCommand.cc#L302) | 启动宽限后检查 peer 速率；过低会中止 | 借鉴宽限和按请求诊断；其具体判断不等同于 curl 的连续低速计时 |
| [aria2 1.37.0 SegmentMan.cc](https://github.com/aria2/aria2/blob/release-1.37.0/src/SegmentMan.cc#L254) | owner 已 idle 时取消并重新领取；取消时处理缓存并记住写入长度 | 借鉴先交还所有权再领取；这段代码不是“随意抢占正在传输的范围”，也不能直接移植其写入长度到 Bytehaul |
| [BEP 3 endgame](https://www.bittorrent.org/beps/bep_0003.html) | 所有未完成块都已有请求时，重复请求帮助收尾，收到后取消其他请求 | 采用有预算的尾部备用请求；不采用向所有来源广播 |
| [libtorrent streaming](https://www.libtorrent.org/streaming.html) | 按预计队列完成时间分配，参考历史耗时及偏差判断追加请求 | 用预计剩余耗时与重新请求成本比较；保留健康请求，避免接近完成时误杀 |

BT 不同 peer 的路径和容量可能独立，同一 HTTP 源的请求往往共享瓶颈。因此竞速是否有用必须通过同一未完成范围的对照验证，不能承诺绕过源站或代理限速。

## 当前实现与约束

- `src/http/mod.rs:17`：每次等待 body frame 的 timeout；少量持续出数可以长期不超时。
- `src/session/multi.rs:375`：当前公开速率来自整任务累计接收量，不能定位单 worker。
- `src/session/multi.rs:535`：无可分配范围即退出，不区分暂时无任务与全部完成。
- `src/scheduler.rs:411`：只拆分 missing/available 范围，不会缩小正在使用的 lease。
- `src/session/multi.rs:642`：错误回退进度、discard ack、重试判断、renew/reclaim。重分配不能不断创建新 RetryState 绕过预算。
- `src/session/retry.rs:42`：现有计数和 elapsed budget 在决策时检查；它不是正在传输请求的总超时。本任务不偷偷改变这一既有语义。
- `src/session/flow.rs:38`：限速、semaphore 和 channel 等待当前包在同一 forwarding 阶段；要明确上报阻塞原因和时间。
- `src/storage/writer.rs:93`：按 lease 过滤数据，但不同有效 lease 不防止覆盖同一文件范围。
- `src/storage/writer.rs:123`：达到缓存水位会提前写盘；discard 只能丢弃尚在缓存中的字节，不能撤销已写盘内容。因此不能让两个竞速副本都写主文件，然后简单丢弃输家。
- `src/session/multi.rs:399` 与 transfer-storage spec：checkpoint 先冻结完成态，再等待 sync；部分写入不等于恢复完成位。
- `src/http/request.rs:31`：显式 HTTP/1.1；`src/network.rs:191`、`:206` 仅启用 HTTP/1；`src/config.rs:166` 默认 idle pool 为 0。当前不存在 HTTP/2 复用诊断方向。
- `src/config.rs:177`：默认 piece 1 MiB、最小 segment 256 KiB；若还有大量可分配分片就整体变慢，不能只归因于尾部并发下降。
- `src/session/range_validate.rs:30` 校验状态、范围、长度和编码；`src/http/request.rs:22` 不自动加入对象 validator。新增竞速须明确对象一致性门槛。
- `bindings/python/src/lib.rs:213` 的统一 build_download_spec 是配置镜像入口；还需更新对象接口和便捷函数签名。

## 历史决策

已阅读归档任务 `09-06-aria2-simplification` 的 PRD/design 和 `09-06-dynamic-split-regression/prd.md`，无需重复检索对话历史：

1. 保留稀疏调度状态，不能每次分配扫描全文件。
2. 不重构缓存为任意重叠片段合并器。
3. 已有动态拆分性能测试曾因 Windows 调度抖动失败，改为屏障验证实际 Range 并发。新测试必须继续采用行为断言。
4. HTTP pooling 保持显式配置，不能借本任务改变默认。

## 根因验证计划

记录活跃网络请求数、可领取范围数、逐请求速率、近期健康基线、读取/首部/本地等待时间、连接是否新建及请求 attempt。以事件级默认诊断和可选周期 debug 快照控制日志量，不打印 URL query/鉴权头。

可复现场景分别覆盖：个别连接持续慢、全体连接同时慢、尾部只剩一个慢请求、稳定限速、慢磁盘、暂停续传、进程重启续传、从头重下。真实站点的重复尝试若无 URL/参数仅列为后续验证；本地可控 fixture 不依赖用户提供这些信息。
