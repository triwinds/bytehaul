# 公网下载对比实验

`scripts/compare_public.py` 使用当前源码的 release 示例与本机 aria2c，逐次下载，避免两种工具同时争用带宽。每个连接档位默认重复十轮；每轮会对配置矩阵做确定性随机交错，aria2 作为同轮基线。

```powershell
cargo build --release --locked --example public_compare
python scripts/compare_public.py --aria2 '路径/aria2c.exe' --url 'https://服务器/文件.zip'
# 同轮比较 Fixed 4/8/16 MiB 与 Dynamic；低日志计时
python scripts/compare_public.py --aria2 '路径/aria2c.exe' --matrix --log-level off
# 单独跑诊断轮；保留 TRACE 调度字段和完整候选份额
python scripts/compare_public.py --aria2 '路径/aria2c.exe' --matrix --log-level trace
python scripts/analyze_public.py target/public-compare/具体运行目录
```

省略 `--url` 使用 Alpine 镜像源。`--rounds` 设置重复轮数，`--connections` 设置连接档位，`--seed` 固定随机交错顺序；`--timeout` 设置单次进程总超时（默认 180 秒）。有可信发布方摘要时可传 `--sha256`。不使用 `--matrix` 时，可用 `--strategy dynamic` 或 `--range-scheduling-mode` 等参数跑单一配置。

实验配置：

- 默认测试 1、8 个最大连接；可用 `--connections 1 2 4 8 16` 扩展；1 MiB 分片和最小拆分设置；关闭文件预分配；各工具保留自身调度策略。
- IPv4；清除子进程代理环境变量，aria2 不加载用户配置或 netrc；保留操作系统实际路由。
- 连接超时 15 秒，读超时 30 秒；bytehaul 每请求响应头超时 30 秒、最多重试 2 次，aria2 最多尝试 3 次。两者重试范围和退避语义不完全相同。
- `--log-level off` 是低日志计时轮：bytehaul 不输出 tracing，aria2 使用 error，且不输出进度；`--log-level trace` 是诊断轮：bytehaul 输出完整 TRACE，aria2 使用 DEBUG。两轮必须分开解读，计时仍包含实际日志写入；脚本本身不增加依赖，调用当前下载实现。
- 每次使用独立目录，计时包括进程初始化、下载落盘和日志写入，不包括后续校验。超时保留部分文件，它的文件长度不能当作已成功下载字节数。

每次运行在 `target/public-compare/<时间>/` 保存 `metadata.json`、`results.csv`、`report.md`、示例和脚本副本、源码差异、响应头、每次调用的 `command.json`、下载文件及日志。分析脚本补充 `diagnostics.json` 和 `diagnostics.md`，提取请求数、响应码、连接 IP、重试和恢复事件。

ZIP 会完整验证内部 CRC。有可信摘要时核对 SHA-256；没有时，运行结束后检查两种工具产物 SHA-256 一致且 ZIP CRC 通过。这只能证明内容一致和内部校验通过，不能证明发布方身份。运行中的 CSV 尚未完成跨工具校验，最终结果以运行结束后的 CSV 为准。

公网带宽、DNS/CDN 节点、缓存及 TLS 实现会影响结果。全日志模式的性能不能直接代表关闭日志时的吞吐量。统计只计入校验通过的下载，失败次数必须同时查看；不要把失败运行的耗时或部分文件长度算作下载速度。

aria2 参数含义见[官方手册](https://aria2.github.io/manual/en/html/aria2c.html)。

Rust libcurl 的独立对照见 [Rust libcurl 公网对照实验](libcurl-comparison.zh-CN.md)。
