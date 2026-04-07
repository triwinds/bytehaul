# bytehaul：用 hyper 替换 reqwest 迁移方案

> 依据：`experiments/hyper-dl` 实验结果（§10）。  
> 相同 hyper 底层，通过 reqwest 封装后吞吐从 ~529 MiB/s 下降到 ~129 MiB/s（差 ~4×），
> 因此将 reqwest 换成直接使用 hyper 是值得追求的优化路线。

---

## 一、hyper-dl 实现总结

`experiments/hyper-dl/src/main.rs` 中经过性能调优的完整参数如下：

### 1.1 依赖版本

```toml
hyper          = { version = "1.9",  features = ["client", "http1"] }
hyper-rustls   = { version = "0.27", default-features = false,
                   features = ["http1", "native-tokio", "ring"] }
hyper-util     = { version = "0.1",  features = ["client", "client-legacy", "http1", "tokio"] }
http-body-util = "0.1"
tokio          = { version = "1",    features = ["full"] }
```

### 1.2 `HttpConnector` socket 调参（对标 libcurl 默认值）

| 参数 | 值 | 对应 curl 选项 |
|---|---|---|
| `set_nodelay(true)` | 启用 TCP_NODELAY | `CURLOPT_TCP_NODELAY=1`（7.63 起默认） |
| `set_recv_buffer_size(Some(512 * 1024))` | SO_RCVBUF = 512 KiB | curl 下载大文件时实际生效的接收缓冲区 |
| `set_connect_timeout(Some(Duration::from_secs(30)))` | 连接超时 30 s | `CURLOPT_CONNECTTIMEOUT=30` |
| `enforce_http(false)` | 允许 HTTPS URI 透传 | TLS 由上层 `HttpsConnector` 负责 |

### 1.3 TLS 连接器

```rust
HttpsConnectorBuilder::new()
    .with_native_roots()   // 使用 OS 证书根（与 curl 信任链一致）
    .expect("native roots")
    .https_or_http()       // 同时支持 http:// 和 https://
    .enable_http1()        // 仅启用 HTTP/1.1（禁止 H2 连接合并）
    .wrap_connector(http)
```

### 1.4 连接池策略

```rust
Client::builder(TokioExecutor::new())
    .pool_max_idle_per_host(0)   // 禁止空闲连接复用
    .build(tls)
```

每个 range worker 调用 `build_client()` 拿到独立的 `Client` 实例，
每个实例维护自己的 TCP 连接，完全镜像 curl 多 easy-handle 的行为。

### 1.5 Range 请求参数

```rust
Request::builder()
    .method("GET")
    .uri(&url)
    .version(hyper::Version::HTTP_11)          // 强制 HTTP/1.1
    .header("Range", "bytes={start}-{end}")
    .header("Accept-Encoding", "identity")     // 禁止压缩，避免解码开销
    .header("User-Agent", "hyper-dl-experiment/0.1")
    .body(Empty::<Bytes>::new())
```

### 1.6 重定向处理

hyper legacy client 不自动跟随重定向。hyper-dl 手动实现：
- 循环最多 10 次，检查 `3xx` 状态码；
- 读取 `Location` 响应头，支持绝对 URL 和相对路径；
- 最终返回 `(Content-Length, final_url)` 供 worker 直接访问 CDN 地址。

### 1.7 DNS 预热

在启动并发 worker 前，通过 `ToSocketAddrs` 对 final_url 的主机名做一次同步 DNS 解析，
确保系统 resolver 的缓存已填充，worker 建立连接时只需 TCP+TLS 握手。

> 这个技巧只适用于“直连 + 系统解析器”路径。bytehaul 当前已经支持自定义 DNS/DoH（`hickory-resolver`）：
> - 若迁移后仍保留自定义 DNS/DoH，预热必须走同一个 resolver（例如先对目标主机执行一次 `lookup_ip`），否则只会预热系统 resolver，不会命中 hickory 的缓存；
> - 若请求经 HTTP/HTTPS 代理转发，则应预热代理主机而不是源站主机，因为源站解析由代理负责。

---

## 二、bytehaul 当前 reqwest 使用盘点

| 文件 | 使用点 | 说明 |
|---|---|---|
| `src/network.rs` | `reqwest::Client`、`reqwest::Proxy`、`reqwest::Url`、`reqwest::dns::{Addrs, Name, Resolve, Resolving}` | 客户端构建、代理、DoH URL 解析、自定义 DNS 解析器接口 |
| `src/http/request.rs` | `reqwest::Client`、`reqwest::RequestBuilder`、`reqwest::Version` | GET / Range 请求构造 |
| `src/http/response.rs` | `reqwest::Response` | 响应元数据解析（Content-Length、Content-Range 等） |
| `src/http/worker.rs` | `reqwest::Client`、`reqwest::Response` | 请求发送与响应处理 |
| `src/manager.rs` | `reqwest::Client`（`client_cache` HashMap 的 value 类型） | 客户端缓存 |
| `src/session/{mod.rs,single.rs,multi.rs,resume.rs}` | `reqwest::Client`、`reqwest::Response`、`.bytes_stream()` | 首次探测、续传、单连接/多连接 body 消费，以及 range worker 后续请求 |
| `src/error.rs` | `reqwest::Error` | 传输错误类型、`is_retryable()` 分类逻辑 |
| `src/filename.rs` | `reqwest::Url` | 自动文件名推断里的 URL 解析 |
| `bindings/python/src/lib.rs` | `use_native_tls`、代理/DNS 参数映射 | Python 绑定公开参数与 Rust builder 同步 |
| `README.md`、`docs/architecture*.md`、`docs/troubleshooting*.md`、`bindings/python/README.md`、`docs/python.zh-CN.md` | `reqwest`、环境变量代理、SOCKS/native TLS 描述 | 迁移后需要同步更新对外文档与行为承诺 |

---

## 三、迁移方案

### 3.1 总体原则

- **公共 API 基本保持不变，但建议删除 `use_native_tls`**：其性能收益已被实验排除，继续保留只会扩大 connector 组合和测试矩阵。
- **保留 hickory-resolver，但把它封装成统一 resolver 类型**：`hyper_util::client::legacy::connect::HttpConnector` 的 resolver 本质上是 `tower_service::Service<Name, Response = impl Iterator<Item = SocketAddr>>`；应通过 `HttpConnector::new_with_resolver(...)` 注入，而不是假设存在 `set_resolver(...)`。
- **TLS 主线统一使用 `hyper-rustls`**：迁移方案不再保留独立的 native TLS 路径。
- **兼容性优先使用 `rustls-platform-verifier`，并显式保留 `native-tokio` / `tls12`**：前者尽量贴近系统 TLS 校验行为，后两者分别保留平台证书源与 TLS 1.2 兼容性，避免迁移后缩小可连接站点范围。
- **`BytehaulClient` 不要一开始写死成单一 `type alias`**：即使只保留 rustls，代理接入后 connector 的具体类型仍会变化；内部应使用 wrapper / enum 隔离 direct/proxy 差异。
- **代理优先采用 `hyper-http-proxy`（hyper 1.x）或自定义 connector**：`hyper-proxy 0.9.x` 仍依赖 `hyper 0.14`，不适合本次迁移。
- **明确 DNS/DoH 与代理的边界**：直连时 `dns_servers` / `doh_servers` 负责解析源站；启用 HTTP/HTTPS 代理后，本地 resolver 只解析代理主机，源站域名由代理解析。
- **下载链路与 DoH 链路统一保持 rustls**：当前 `hickory-resolver` 已使用 `https-ring` + `rustls-platform-verifier`；迁移后不再引入额外的 TLS 后端组合。
- **SOCKS 单独立项**：当前 `Cargo.toml` 并未启用 reqwest 的 `socks` feature；本次迁移先对齐现有已验证的 HTTP/HTTPS 代理能力。若后续补 SOCKS5，必须显式决定是“本地解析 + 代理转发”还是“代理远端解析”。
- **重定向解析必须在 GET / Range / resume 路径上共享同一个 final URL**：reqwest 以前自动跟随重定向；迁移后如果只有首个 GET 会 resolve redirect，而后续分片/续传仍请求原始 URL，就会出现重复跳转、鉴权头丢失或行为不一致。
- **保留或显式替代环境变量代理语义**：当前公开文档说明 bytehaul 会读取 `HTTP_PROXY` / `HTTPS_PROXY` / `ALL_PROXY`；hyper 方案如果不主动实现这层逻辑，会产生静默兼容性回归。
- **错误抽象必须脱离 `reqwest::Error`**：`DownloadError::Http`、`is_retryable()` 以及超时/连接/body 错误映射要一起迁移，不能只替换 client 类型。
- **若删除 `use_native_tls`，Rust / Python API 与文档要同一阶段同步删改或废弃**：否则迁移过程中会出现实现、绑定和文档脱节。
- **测试不变**：现有基于 `warp` 的单元/集成测试继续保留，仅更新 response 解析方式。

### 3.2 依赖变更（`Cargo.toml`）

**新增：**

```toml
hyper          = { version = "1.9",  features = ["client", "http1"] }
hyper-rustls   = { version = "0.27", default-features = false,
                   features = ["http1", "native-tokio", "rustls-platform-verifier", "tls12", "ring"] }
hyper-util     = { version = "0.1",  features = ["client", "client-legacy", "http1", "tokio"] }
http-body-util = "0.1"
tower-service  = "0.3"
url            = "2"     # 替代 reqwest::Url，用于 DoH URL 解析

# 后续代理阶段再引入，避免第一阶段把 direct/proxy 混在一起
hyper-http-proxy = { version = "1.1", default-features = false }
```

**移除：**

```toml
# reqwest 及其所有 features 全部删除
```

> **注**：
> - `hyper-proxy 0.9.x` 依赖 `hyper 0.14`，不能直接用于本方案；
> - `hyper-rustls` 在 `default-features = false` 下若漏掉 `tls12`，将只支持 TLS 1.3；bytehaul 面向通用下载场景，迁移时应显式保留 `tls12`；
> - `hyper-http-proxy` 的组合验证放到“代理阶段”完成，第一阶段只做直连 rustls 路径。

### 3.3 阶段一：类型别名与 client 构建（`src/network.rs`）

**目标**：`ClientNetworkConfig::build_client()` 返回 hyper 客户端，隔离所有 reqwest 依赖。

**步骤：**

1. 定义统一的客户端类型别名：

```rust
type DirectHttpConnector =
    hyper_util::client::legacy::connect::HttpConnector<BytehaulDnsResolver>;

// 默认路径：直连 + rustls
type DirectRustlsConnector = hyper_rustls::HttpsConnector<DirectHttpConnector>;
type DirectRustlsClient =
    hyper_util::client::legacy::Client<DirectRustlsConnector, http_body_util::Empty<bytes::Bytes>>;

pub(crate) enum BytehaulClient {
    Direct(DirectRustlsClient),
    // 代理变体在代理阶段再加入，避免一开始把 connector type 写死
}
```

2. 修改 `build_client()` 实现：

```rust
pub(crate) fn build_client(&self) -> Result<BytehaulClient, DownloadError> {
    let resolver = self.build_dns_resolver()?;
    let mut http = HttpConnector::new_with_resolver(resolver);
    http.set_nodelay(true);
    http.set_recv_buffer_size(Some(512 * 1024));
    http.set_connect_timeout(Some(self.connect_timeout));
    http.enforce_http(false);

    let tls = HttpsConnectorBuilder::new()
        .try_with_platform_verifier()
        .or_else(|_| HttpsConnectorBuilder::new().with_native_roots())?
        .https_or_http()
        .enable_http1()
        .wrap_connector(http);

    let mut builder = Client::builder(TokioExecutor::new());
    builder.pool_max_idle_per_host(0);

    Ok(BytehaulClient::Direct(builder.build(tls)))
}
```

3. 将 DNS 解析器接口从 `reqwest::dns::Resolve` 改为 `tower_service::Service<Name>`（满足 `HttpConnector` 的 resolver 约束）：

```rust
// 旧接口
impl reqwest::dns::Resolve for BytehaulDnsResolver { ... }

// 新接口
impl tower_service::Service<Name> for BytehaulDnsResolver {
    type Response = std::vec::IntoIter<SocketAddr>;
    type Error = BoxError;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, name: Name) -> Self::Future { ... }
}
```

`build_dns_resolver()` 不再返回 `Option`，而是统一返回 `BytehaulDnsResolver`；
内部继续根据 `dns_servers` / `doh_servers` / `enable_ipv6` 选择系统配置或自定义配置，
这样 `HttpConnector` 的泛型参数在所有代码路径上都保持稳定。

4. DoH URL 解析中的 `reqwest::Url::parse(...)` 改为 `url::Url::parse(...)`，接口相同，无逻辑变化。

DoH endpoint 的 bootstrap 逻辑暂时保持不变：如果 `doh_server` 主机本身是域名，仍在 client 构建时通过系统解析器做一次 bootstrap。
这一点与下载流量的 proxy 设置解耦，不应误认为“开启 DoH 后 bootstrap 也会自动走 DoH”。

5. DNS 预热只在“直连路径”保留：

```rust
// 仅在直连路径对源站做预热
// 自定义 DNS/DoH 时，应调用 BytehaulDnsResolver 本身，而不是 ToSocketAddrs
warm_direct_resolution(&mut resolver, host).await?;
```

6. `log_os_socket_buffer_sizes()` 中的 reqwest 注释更新为说明已直接通过 `HttpConnector` 控制 `SO_RCVBUF`。

### 3.4 阶段二：请求构造（`src/http/request.rs`）

**目标**：将 `reqwest::RequestBuilder` 替换为 `hyper::Request`。

旧接口：
```rust
pub(crate) fn build_range_request(
    client: &reqwest::Client, url: &str, ..., start: u64, end: u64
) -> reqwest::RequestBuilder
```

新接口：
```rust
use http_body_util::Empty;
use hyper::body::Bytes;
use hyper::Request;

pub(crate) fn build_range_request(
    url: &str,
    headers: &HashMap<String, String>,
    start: u64,
    end: u64,
) -> Request<Empty<Bytes>> {
    let mut builder = Request::builder()
        .method("GET")
        .uri(url)
        .version(hyper::Version::HTTP_11)
        .header("Range", format!("bytes={start}-{end}"))
        .header("Accept-Encoding", "identity");
    for (k, v) in headers {
        builder = builder.header(k.as_str(), v.as_str());
    }
    builder.body(Empty::new()).expect("range request")
}

pub(crate) fn build_get_request(
    url: &str,
    headers: &HashMap<String, String>,
) -> Request<Empty<Bytes>> {
    let mut builder = Request::builder()
        .method("GET")
        .uri(url)
        .version(hyper::Version::HTTP_11);
    for (k, v) in headers {
        builder = builder.header(k.as_str(), v.as_str());
    }
    builder.body(Empty::new()).expect("get request")
}
```

注意：`timeout` 参数不再在请求构建时传入，改为在 `HttpWorker::send_*` 中用 `tokio::time::timeout` 包裹。

### 3.5 阶段三：响应解析（`src/http/response.rs`）

**目标**：将 `ResponseMeta::from_response(response: &reqwest::Response)` 迁移到 hyper 响应类型。

响应头解析逻辑完全相同，仅入参类型变化：

```rust
use hyper::Response;
use http_body_util::BodyExt;

impl ResponseMeta {
    pub fn from_parts(
        status: hyper::StatusCode,
        headers: &hyper::HeaderMap,
        content_length_override: Option<u64>,
    ) -> Self {
        // 原有头部解析逻辑不变，改从 &hyper::HeaderMap 读取
        ...
        // content_length: 从 content-length 头部解析，或使用 content_length_override
        let content_length = content_length_override.or_else(|| {
            headers.get("content-length")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse().ok())
        });
        ...
    }
}
```

### 3.6 阶段四：HTTP worker（`src/http/worker.rs`）

**目标**：`HttpWorker` 使用 `BytehaulClient` 发送请求，并手动处理重定向和 body 流。

```rust
pub(crate) struct HttpWorker {
    client: BytehaulClient,
    url: String,
    headers: HashMap<String, String>,
    timeout: Duration,
}

impl HttpWorker {
    pub async fn send_get(&self) -> Result<(impl Stream<Item=Bytes>, ResponseMeta), DownloadError> {
        let final_url = self.resolve_redirects(&self.url).await?;
        let req = build_get_request(&final_url, &self.headers);
        let resp = tokio::time::timeout(self.timeout, self.client.request(req))
            .await
            .map_err(|_| DownloadError::Timeout)?
            .map_err(DownloadError::from)?;
        let status = resp.status();
        if !status.is_success() {
            return Err(make_http_error(resp.headers(), status.as_u16()).await);
        }
        let meta = ResponseMeta::from_parts(status, resp.headers(), None);
        Ok((resp.into_body(), meta))
    }

    pub async fn send_range(&self, start: u64, end: u64)
        -> Result<(impl Stream<Item=Bytes>, ResponseMeta), DownloadError>
    {
        let req = build_range_request(&self.url, &self.headers, start, end);
        let resp = tokio::time::timeout(self.timeout, self.client.request(req))
            .await
            .map_err(|_| DownloadError::Timeout)?
            .map_err(DownloadError::from)?;
        let status = resp.status();
        // 与现有逻辑相同：200 时允许全量响应，206 为正常分片
        ...
    }

    async fn resolve_redirects(&self, url: &str) -> Result<String, DownloadError> {
        // 直接复用 hyper-dl 中已验证的手动重定向跟随逻辑（最多10跳）
        ...
    }
}
```

**关于 body 消费**：hyper 的 body 通过 `.frame().await` 异步迭代；
调用方（`session/` 层）将 `while let Some(frame) = body.frame().await { ... }` 替换现有的 `response.bytes_stream()` 消费模式。

注意：

- `resolve_redirects()` 不能只服务 `send_get()`。range probe、续传请求以及后续 multi worker 分片，都要共享同一个 resolved final URL；推荐在 `HttpWorker` 内用 `tokio::sync::OnceCell<String>` 缓存，或把 `final_url` 作为返回值显式传给 `session/` 层。
- 如果启用了 HTTP proxy，`ProxyConnector::http_headers(&uri)` 必须针对“最终请求 URI”计算并合并；因此 redirect resolve 与 proxy header 注入应放在同一层完成，避免按原始 URL 算错代理头。

### 3.7 阶段五：manager 更新（`src/manager.rs`）

`client_cache` 的 value 类型从 `reqwest::Client` 改为 `BytehaulClient`，其余逻辑不变：

```rust
pub struct Downloader {
    client_cache: Arc<Mutex<HashMap<ClientNetworkConfig, BytehaulClient>>>,
    ...
}
```

### 3.8 阶段六：session 层 body 消费更新（`src/session/`）

当前通过 `reqwest::Response` 的 `.bytes_stream()` 读取 body；迁移后改为：

```rust
use http_body_util::BodyExt;

let mut body = response.into_body();
while let Some(frame) = body.frame().await {
    match frame?.into_data() {
        Ok(chunk) => { /* 写入存储 */ }
        Err(_)    => { /* trailers，忽略 */ }
    }
}
```

补充：

- `run_fresh_from_response()`、`run_multi_worker()`、`try_resume_download()` 不应再默认 `spec.url` 就是后续 Range 请求地址；需要显式接收 `final_url`，否则只有首次探测命中最终 CDN，后续 worker 仍会回到原始重定向入口。
- `reqwest::RequestBuilder::timeout()` 以前把“等待响应头 + 读取 body”一起包起来；迁移后除了 request 阶段的 timeout，还要在 `body.frame().await` 上继续施加读取超时，避免单个连接在收到响应头后无限挂起。

### 3.9 阶段七：错误与辅助模块收尾（`src/error.rs`、`src/filename.rs`）

**目标**：清掉剩余的 reqwest 类型耦合，避免主链路已切到 hyper，但边角模块仍把 reqwest 留成隐性依赖。

1. `DownloadError::Http(#[from] reqwest::Error)` 改成更中性的传输错误抽象，例如：

```rust
pub enum TransportErrorKind {
    Connect,
    Timeout,
    Request,
    Body,
    Other,
}

pub struct TransportError {
    kind: TransportErrorKind,
    source: BoxError,
}
```

2. `is_retryable()` 改为基于 `TransportErrorKind`、`std::io::ErrorKind` 与 HTTP status 判定，而不是依赖 `reqwest::Error::is_timeout()` / `is_connect()` / `is_body()` 这些 helper。

3. `src/filename.rs` 中的 `reqwest::Url::parse(...)` 改为 `url::Url::parse(...)`，并保留现有自动文件名推断测试，确认 `Content-Disposition -> URL path -> download` 退化链路不变。

### 3.10 阶段八：代理（HTTP/HTTPS + 环境变量，单独阶段）

**目标**：在不改变 `all_proxy` / `http_proxy` / `https_proxy` 公共 API 的前提下，补回 reqwest 当前的 HTTP/HTTPS 代理语义，并明确处理环境变量代理。

```rust
use hyper_http_proxy::{Intercept, Proxy, ProxyConnector};

let mut proxy_connector = ProxyConnector::new(tls_connector)?;
if let Some(proxy) = &self.http_proxy {
    proxy_connector.add_proxy(Proxy::new(Intercept::Http, proxy.parse()?));
}
if let Some(proxy) = &self.https_proxy {
    proxy_connector.add_proxy(Proxy::new(Intercept::Https, proxy.parse()?));
}
if let Some(proxy) = &self.all_proxy {
    proxy_connector.add_proxy(Proxy::new(Intercept::All, proxy.parse()?));
}
```

注意：

- `hyper_http_proxy::ProxyConnector::http_headers(&uri)` 返回的头，需要在“明文 HTTP 请求”发出前合并到 `Request`，否则 HTTP proxy 认证/自定义 proxy 头不会完整生效；HTTPS 走 `CONNECT`，不需要额外改 request。
- 代理启用后，本地 resolver 负责解析 proxy 主机；源站域名由 proxy 解析，因此 `dns_servers` / `doh_servers` 不再控制源站解析结果。
- 当前 `docs/troubleshooting*.md` 说明了 `HTTP_PROXY` / `HTTPS_PROXY` / `ALL_PROXY`；如果要保持兼容，这一阶段需要显式在 `ClientNetworkConfig::build_client()` 或独立 helper 中读取环境变量，并保持“显式 builder 配置优先于环境变量”的语义。
- 如需 SOCKS5，不应塞进同一阶段。`hyper-util` 内置了 `SocksV5` helper，但其默认是 remote DNS；若希望继续走 bytehaul 的本地 DNS/DoH，需要显式 `local_dns(true)`，并接受 DNS leak 语义变化。

### 3.11 阶段九：Python 绑定与公开文档同步

**目标**：在迁移期保持 Rust API、Python API 和公开文档一致，不留下“实现已经变了，接口/文档还停在 reqwest 时代”的尾巴。

- 若删除 `use_native_tls`，同步更新 `bindings/python/src/lib.rs`、`bindings/python/README.md`、`docs/python.zh-CN.md`、`docs/performance-investigation.zh-CN.md` 中的参数签名与说明。
- `README.md`、`docs/architecture*.md`、`src/lib.rs` 的架构描述不再写成“基于 reqwest”，并把“SOCKS 代理”措辞改成与真实实现阶段一致。
- `docs/troubleshooting*.md` 中“bytehaul（经由 reqwest）会读取环境变量代理”的表述，要改成迁移后的真实语义：要么显式保留 env 代理支持，要么明确标注为 breaking change。

### 3.12 测试更新

| 测试文件 | 变更点 |
|---|---|
| `src/network.rs` 中的 `#[cfg(test)]` | `BytehaulDnsResolver` 改为 `tower_service::Service<Name>` 后的 resolver 测试、DoH bootstrap 测试、环境变量代理优先级测试 |
| `src/http/worker.rs` 中的 `#[cfg(test)]` | `worker_for()` 改用 `BytehaulClient`；`reqwest::Response::from(...)` mock 改为直接构造 `hyper::Response` |
| `src/http/response.rs` 中的 `#[cfg(test)]` | 将 `reqwest::Response::from(http::Response::builder()...)` 改为直接传 `hyper::HeaderMap` |
| `src/http/request.rs` 中的 `#[cfg(test)]` | 断言对象从 `reqwest::Request` 改为 `hyper::Request` |
| `src/session/` 相关测试 | 覆盖 redirect 后 `final_url` 继续用于 probe / multi worker / resume，以及 body 读取超时 |
| `src/error.rs`、`src/filename.rs` | transport error 分类、retryable 判定、`url::Url` 文件名解析回归测试 |
| `src/manager.rs`、`bindings/python/tests/test_bytehaul.py` | builder / Python 绑定去掉或废弃 `use_native_tls` 后的 API 一致性测试 |

warp 测试服务器本身不涉及 reqwest，无需改动。

---

## 四、关键风险与注意事项

| 风险 | 说明 | 缓解措施 |
|---|---|---|
| resolver 类型不稳定 | `HttpConnector<R>` 的 `R` 是泛型；如果沿用现在的 `Option<resolver>` 思路，很容易在直连/代理/不同 TLS 路径上把 client type 写炸 | 统一总是返回 `BytehaulDnsResolver`；`BytehaulClient` 用 wrapper / enum 隔离不同 connector |
| 代理与 DNS/DoH 语义冲突 | 启用 HTTP/HTTPS 代理后，本地 DNS/DoH 只解析代理主机，源站域名由代理解析；这与“直连时由 bytehaul 控制 DNS”的语义不同 | 在文档和日志中显式说明；为 `all_proxy` / `http_proxy` / `https_proxy` 增加覆盖测试 |
| proxy crate 选型错误 | `hyper-proxy 0.9.x` 绑定 `hyper 0.14`，且 plain HTTP proxy 还要求显式合并 `http_headers()` | 代理阶段改用 `hyper-http-proxy` 或自定义 connector，并把代理放到独立阶段实现 |
| final URL 没有向后续分片传播 | 如果只在首个 GET/range probe 里解析重定向，而 `session/` 后续 worker 继续使用 `spec.url`，多连接与续传行为会和当前 reqwest 版本不一致 | 将 `final_url` 作为 `HttpWorker` 缓存或响应上下文的一部分，显式传入后续 range 请求 |
| 环境变量代理回归 | reqwest 默认会处理一部分 proxy 环境变量；切到 hyper 后如果不手动实现，用户可能在“不改代码”的情况下失去代理能力 | 单独实现 env proxy 读取逻辑，并增加显式配置覆盖 env 的测试 |
| 平台校验器初始化失败 | `try_with_platform_verifier()` 在部分目标环境上可能初始化失败 | 构建 connector 时回退到 `with_native_roots()`，并在日志中记录回退路径 |
| TLS 1.2 兼容性丢失 | `hyper-rustls` 关闭默认 feature 后，如果遗漏 `tls12`，只能连接 TLS 1.3 站点 | 在依赖声明中显式启用 `tls12`，并增加至少一个 TLS 1.2 站点的兼容性验证 |
| DNS 预热路径失效 | `ToSocketAddrs` 只会预热系统 resolver，不会预热 hickory 的 DoH 缓存；在代理模式下还可能预热错对象 | 直连自定义 DNS/DoH 路径改为对同一个 resolver 预热；代理路径只预热代理主机 |
| 重定向安全 | 手动跟随重定向需防止 SSRF（跳转到内网地址）| 迁移时同步加入 URL scheme 白名单检查（仅允许 http/https） |
| SO_RCVBUF 实际生效 | Linux 内核可能将值钳制在 `rmem_max` 以内 | 保留 `log_os_socket_buffer_sizes()` 便于诊断；在 CI 中增加 socket buffer 日志断言 |
| body 超时 | reqwest 的 `.timeout()` 覆盖整个请求（连接+body）；hyper 版本需手动用 `tokio::time::timeout` 包裹 | 对连接阶段和 body 读取阶段分别设置超时 |
| 错误分类退化 | 从 `reqwest::Error` 换到 `hyper` 后，如果没有保留 timeout/connect/body 分类，现有 retry 逻辑会变钝 | 引入中性的 transport error kind，并为 `is_retryable()` 增加单测 |
| Rust/Python/文档不同步 | 删除 `use_native_tls`、调整代理语义后，如果 bindings 和文档没一起改，用户会遇到“文档能配、运行时报错” | 把 bindings + docs 收到独立阶段，与代码一起验收 |
| HTTP/2 支持 | 迁移后默认仅 HTTP/1.1；如需 H2 需在 connector 上额外启用 | 当前 bytehaul 已强制 HTTP/1.1 用于并发分片，无需 H2 |

---

## 五、实施顺序建议

```
阶段一（network.rs：直连 rustls + 统一 resolver 抽象）
  └─ 阶段二（request.rs）
       └─ 阶段三（response.rs）
            └─ 阶段四（worker.rs：redirect + final_url 传播）
                 └─ 阶段五（manager.rs）
                      └─ 阶段六（session/ body 消费）
                           └─ 阶段七（error.rs + filename.rs 收尾）
                                └─ 阶段八（HTTP/HTTPS 代理 + 环境变量代理）
                                     └─ 阶段九（Python 绑定 + 对外文档同步）
                                          └─ 阶段十（可选：SOCKS，单独立项）
```

每完成一个阶段后运行：

```bash
cargo test -p bytehaul --all-targets
cargo clippy --workspace --all-targets -- -D warnings
cargo tarpaulin --engine llvm -p bytehaul --all-targets --out Stdout --fail-under 95
```

确保覆盖率不低于 95%，CI 通过后再进入下一阶段。

---

## 六、预期收益

基于 §10 实验数据，完成迁移后预期吞吐从 **~129 MiB/s** 提升至接近 **~400–530 MiB/s**（视服务器和网络条件），与 aria2/libcurl 持平或更优。
