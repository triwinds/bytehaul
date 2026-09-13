# P2/P3 本机端到端回归样本

2026-09-14，macOS aarch64，Rust 1.96.0，`cargo test -p bytehaul --locked --all-targets --no-fail-fast` / 最终 `--all-targets` 调用的 debug 基准。每种形态十轮，均使用本地回环夹具。修改前采自本轮步骤 1 后、资源复用前；修改后生产源码对应 `b748d3e`。生成器的原始 profile 固定写作 bench，本报告按实际命令校正为 debug。Windows P1 数据不参与此表计算。

| 场景 | 修改前中位 ms | 修改后中位 ms | 变化 |
| --- | ---: | ---: | ---: |
| e2e/cut_stream_12MiB | 1275.39 | 1294.50 | +1.5% |
| e2e/delayed_response_300ms | 1346.25 | 1061.82 | -21.1% |
| e2e/large_64MiB_4conns | 1925.76 | 1764.44 | -8.4% |
| e2e/slow_tail_12MiB | 760.39 | 520.24 | -31.6% |
| e2e/small_256KiB_single_connection | 20.14 | 15.04 | -25.3% |
| e2e/split_12MiB_4conns | 618.83 | 442.56 | -28.5% |
| e2e/unknown_length_12MiB | 916.22 | 565.84 | -38.2% |

未观察到超过计划 10% 调查门槛的中位耗时回退。此处用于检查明显回退，不把改善归因于本次重构：debug 构建、主机负载和夹具开销均影响结果；macOS 基准未提供进程 CPU 指标，也没有采集独立 RSS 峰值。P3 的采用依据是删除重复执行路径并通过行为矩阵，不能据此宣称 release 吞吐或 CPU 改善。

[修改前逐轮样本](before-samples.csv)和[修改后逐轮样本](after-samples.csv)保留原始成功、失败与重复流量指标。断流形态的失败是既有夹具行为，不能把“基准命令退出成功”解释为每轮下载成功。32 种超时由 33 个 client/驱动降为 1 个的独立资源比较见 [P2 报告](../p2-client-run/report.md)。
