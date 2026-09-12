# 多 IP 连接策略：其他下载器实现调研

调研日期：2026-09-12。对应[多 IP 建连与连接复用收敛计划](multi-ip-connection-plan.zh-CN.md)。本次查阅官方文档与上游源码，没有进行下载性能实测。

## 结论

有可以借鉴的组成机制，但在本次核对的 aria2、curl、Axel 和 IDM 公开资料中，没有确认完整实现“同域名 DNS 多 IP 初始分散 → 正常下载被动测量 → 按 IP 吞吐收敛 → 定向复用”的先例。不能由此断言所有下载器都没有实现。

最接近的调度参考是 **aria2 的自适应镜像选择**；最直接的传输层参考是 **curl 将连接目标与 URL/TLS 身份分离**。IDM 的动态分片说明快连接可自然承担更多工作，但不足以证明存在 IP 排名。

| 下载器 | 已核实机制 | 与本方案的区别 |
| --- | --- | --- |
| aria2 | 实际下载反馈、镜像择优、未测镜像探索和历史重测；DNS 失败回退 | 评分键为 hostname + protocol，不区分域名背后的 IP |
| curl/libcurl | 多地址建连竞速、连接池、指定连接目标但保留原域名身份 | 竞速选择建连成功者，不选择持续下载吞吐最高者 |
| Axel | 多 URL 分配连接、顺序尝试解析地址；可选镜像搜索路径有探测排序 | 没有在所查路径看到按 DNS IP 的被动吞吐收敛；探测时间不是正文吞吐 |
| IDM | 动态分片，完成分片的连接继续承担工作并复用连接 | 官方资料没有公开同域名多 IP 选择和评分算法 |

以下源码链接固定到实际读取的提交；产品文档链接会随上游更新。

## aria2：借鉴镜像调度，而不是认为已经支持 IP 收敛

源码快照：`9e7273583f83e881e3ec067b523ba88724088d2f`。

### 选择与反馈怎样衔接

`AdaptiveURISelector::getServerStats()` 从 URI 提取 host 和 scheme，调用 `serverStatMan_->find(host, protocol)`。同一域名下不同 DNS IP 因而共享一份成绩，不能直接用它挑选该域名最快的 IP。

`selectOne()` 先给未测试镜像机会；其代码有优先尝试至少三个镜像的规则，并使用由 `split - 1` 初始化的评估计数。后续机会可用于未测试、需要重测或已有较好成绩的镜像。这些“测试”是为下载选择 URI，并非这里另发一套专用测速分片。

`getBestMirror()` 取单连接/多连接历史平均速度中的较大值比较：将速度高于最高值约 75% 的镜像组成候选集合，集合足够大时随机选择，否则选择最快镜像。因此它实现的是“接近最快的一组”，并非固定 Top 2。重测判断使用 `2^counter` 天，且 counter 大于 8 时跳过该重测分支。这是历史镜像策略的时间尺度，不适合直接照搬到短期波动的 CDN IP。

依据：[AdaptiveURISelector.cc](https://github.com/aria2/aria2/blob/9e7273583f83e881e3ec067b523ba88724088d2f/src/AdaptiveURISelector.cc)。

实际下载先积累 `PeerStat`；`ProcessStoppedRequestGroup::collectStat()` 在请求组停止处理阶段读取平均下载速度，更新域名/协议的 `ServerStat`，并区分单连接与多连接场景。不能把这一条历史反馈路径描述成每个 Range 完成后立刻更新 IP 排名。

依据：[RequestGroupMan.cc](https://github.com/aria2/aria2/blob/9e7273583f83e881e3ec067b523ba88724088d2f/src/RequestGroupMan.cc)。

`ServerStat` 在前几个样本使用按计数平均，计数达到 5 后采用 `0.8 × 旧均值 + 0.2 × 新速度` 更新。单连接均值明显下降时还有重置计数逻辑。本项目可借鉴平滑和退化后的重新学习，但需要增加实际 IP、样本新鲜度和本地背压污染判断。

依据：[ServerStat.cc](https://github.com/aria2/aria2/blob/9e7273583f83e881e3ec067b523ba88724088d2f/src/ServerStat.cc)。

### DNS 与连接不是同一套评分

`AbstractCommand::resolveHostname()` 命中缓存后取地址列表首项；`DNSCache::getGoodAddr()` 返回首个 good 地址，`markBad()` 标记失败地址。`InitiateConnectionCommand` 还包含 IPv6 建连时准备 IPv4 备用连接的逻辑。这些是地址选择和连通性回退，并未读取上述镜像吞吐成绩来给同域名 IP 排序。

依据：[AbstractCommand.cc](https://github.com/aria2/aria2/blob/9e7273583f83e881e3ec067b523ba88724088d2f/src/AbstractCommand.cc)、[DNSCache.cc](https://github.com/aria2/aria2/blob/9e7273583f83e881e3ec067b523ba88724088d2f/src/DNSCache.cc)、[InitiateConnectionCommand.cc](https://github.com/aria2/aria2/blob/9e7273583f83e881e3ec067b523ba88724088d2f/src/InitiateConnectionCommand.cc)。

上游还有直接要求“同域名多个解析 IP 并行下载”的 [issue #2261](https://github.com/aria2/aria2/issues/2261)。它是需求讨论，不是实现证据；以上结论以源码为主。

## curl：控制连接目的地与复用，建连竞速不能代替吞吐反馈

Happy Eyeballs 在多个地址之间错开启动建连，使用成功的连接。curl 作者说明，从 8.16.0 起，尚未成功时还会继续对后续地址启动并行尝试。它解决地址不可达、某类网络较慢等建连问题，没有通过持续接收正文来判断最快下载 IP。

依据：[curl 作者对 8.16.0 建连变化的说明](https://daniel.haxx.se/blog/2025/08/04/even-happier-eyeballs/)。

libcurl 在 DNS 解析前检查可复用连接，并检查域名、端口、协议等兼容条件。因而单纯改变 DNS 顺序，不保证下一次传输切换到新 IP。这直接支持原计划“必须控制请求进入哪个连接池”的判断。

依据：[Connection reuse](https://everything.curl.dev/transfers/conn/reuse.html)。

`CURLOPT_CONNECT_TO` 可以将原 host:port 的连接定向到另一 host/IP:port，同时保持原 URL 对应的 TLS SNI、证书验证和应用协议身份。它提供了本方案所需的基础能力范例，但评分、探索和预算仍需应用层实现。不能把 API 能指定 IP 理解成 curl 自动按 IP 吞吐择优。

依据：[CURLOPT_CONNECT_TO 官方文档](https://curl.se/libcurl/c/CURLOPT_CONNECT_TO.html)。

对 Hyper 的“origin + IP 分组 client”是本项目的工程推导，不是上述文档推荐的 Hyper 架构。仍需原型确认物理连接预算、响应体生命周期和实际 peer IP。

## Axel：多 URL 轮转与探测排序

源码快照：`4679ed7657abeebfc92f083692f9f666a129db14`。

`axel_start()` 沿 URL 链表为连接设置下载 URL；`tcp_connect()` 每次通过 `getaddrinfo()` 取得候选，沿 `ai_next` 顺序尝试并在满足代码的成功条件时停止。所查路径没有 IP 吞吐表，也没有初始按 IP 占用均衡的选择器。DNS 返回顺序变化可能让连接落到不同地址，但这不等于下载器主动实现 IP 分散。

依据：[axel.c](https://github.com/axel-download-accelerator/axel/blob/4679ed7657abeebfc92f083692f9f666a129db14/src/axel.c)、[tcp.c](https://github.com/axel-download-accelerator/axel/blob/4679ed7657abeebfc92f083692f9f666a129db14/src/tcp.c)。

镜像搜索代码的 `search_speedtest()` 单独执行连接初始化和文件信息查询，检查文件大小，将耗时毫秒数记入名为 `speed` 的字段，然后断开连接；排序偏好较小值。这是额外探测的响应耗时，不是以正文有效字节/时间计算的吞吐，也不是正常下载中的被动统计。该代码属于可选搜索功能路径，不能泛化为每次普通下载都会运行。

依据：[search.c](https://github.com/axel-download-accelerator/axel/blob/4679ed7657abeebfc92f083692f9f666a129db14/src/search.c)。

## IDM：快连接多做工作，但 IP 算法未公开

官方说明：新连接可切分最大的剩余分片；连接完成分片后，可接手尚未开始的分片，或切分较慢连接负责的工作；已经完成分片的连接会被复用，省去重新连接过程。

由此可以推导，较快连接有机会处理更多文件数据，而不需要先建立 IP 排名表。但官方说明不足以确认它是否主动覆盖 DNS IP、以什么键记录成绩、是否收敛到少量 IP。不能把“动态分片”和宣传中的加速效果当作这些能力的证据。

依据：[IDM Dynamic Segmentation and Performance](https://www.internetdownloadmanager.com/support/segmentation.html)。

## 对当前计划的建议

以下是基于调研的设计建议，不代表上述项目已经采用完整方案。

1. **保留连接定向与被动统计的实施顺序。** aria2 的镜像策略可以参考，但 DNS IP 归属、Hyper 池控制和预算需要本项目自己验证。
2. **把固定 Top 2 作为实验策略。** 增加“最高速度一定比例内的健康候选集合”作为对照，结合占用分配；三个 IP 接近时，强行排除第三个未必有收益。aria2 的 75% 阈值仅作先例，不应直接当成最佳参数。
3. **设置无需排名的基线。** 比较现状、仅初始分散并让空闲连接接续正常工作、分散加 IP 评分三种策略。这样能确认额外收益究竟来自多 IP 覆盖，还是来自评分收敛；不要求改动分片算法。
4. **使用下载过程中的短期反馈。** 原计划要求任务内逐步收敛，因此不能照搬 aria2 请求组停止时写入历史成绩的时机，也不能照搬按天重测。
5. **继续区分建连时延、响应头时延和正文吞吐。** curl 竞速与 Axel 探测都说明，快速连接/响应不代表持续下载快。保持没有额外测速请求的约束。
6. **保留原计划的本地背压、迟滞、TTL 和网络配置隔离。** 本次资料未提供可以直接替代这些设计的完整实现；仍由受控实验验证。

建议最先对照阅读 aria2 的 `AdaptiveURISelector.cc`、`ServerStat.cc` 和 `RequestGroupMan.cc`，再以 curl 的连接目标/域名身份分离语义审视 Hyper 原型。最终是否采用固定两 IP、近优集合或仅初始分散，应依据同预算下的耗时、慢尾和失败率决定。
