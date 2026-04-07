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
确保内核 DNS 缓存已填充，worker 建立连接时只需 TCP+TLS 握手。

---

## 二、bytehaul 当前 reqwest 使用盘点

| 文件 | 使用点 | 说明 |
|---|---|---|
| `src/network.rs` | `reqwest::Client`、`reqwest::Proxy`、`reqwest::Url`、`reqwest::dns::{Addrs, Name, Resolve, Resolving}` | 客户端构建、代理、DoH URL 解析、自定义 DNS 解析器接口 |
| `src/http/request.rs` | `reqwest::Client`、`reqwest::RequestBuilder`、`reqwest::Version` | GET / Range 请求构造 |
| `src/http/response.rs` | `reqwest::Response` | 响应元数据解析（Content-Length、Content-Range 等） |
| `src/http/worker.rs` | `reqwest::Client`、`reqwest::Response` | 请求发送与响应处理 |
| `src/manager.rs` | `reqwest::Client`（`client_cache` HashMap 的 value 类型） | 客户端缓存 |

---

## 三、迁移方案

### 3.1 总体原则

- **保持公共 API 不变**：`Downloader`、`DownloaderBuilder`、`DownloadSpec` 等对外接口无需更改。
- **保留 hickory-resolver**：自定义 DNS（包括 DoH）仍通过 hickory 实现，只需将 DNS 解析器接口从 `reqwest::dns::Resolve` 迁移到 hyper-util 的对应 trait。
- **代理通过 `hyper-proxy` 实现**：hyper 本身不内置代理支持，需引入 `hyper-proxy` 或手写 `CONNECT` tunneling connector。
- **native TLS 通过 `hyper-tls` 实现**：在 `use_native_tls=true` 时切换 TLS 后端。
- **测试不变**：现有基于 `warp` 的单元/集成测试继续保留，仅更新 response 解析方式。

### 3.2 依赖变更（`Cargo.toml`）

**新增：**

```toml
hyper          = { version = "1.9",  features = ["client", "http1"] }
hyper-rustls   = { version = "0.27", default-features = false,
                   features = ["http1", "native-tokio", "ring"] }
hyper-util     = { version = "0.1",  features = ["client", "client-legacy", "http1", "tokio"] }
http-body-util = "0.1"
hyper-proxy    = "0.1"   # 代理支持（http/https/SOCKS5）
hyper-tls      = { version = "0.6", optional = true }  # native TLS 路径
url            = "2"     # 替代 reqwest::Url，用于 DoH URL 解析
```

**移除：**

```toml
# reqwest 及其所有 features 全部删除
```

> **注**：`hyper-proxy` 当前最新版本需验证与 hyper 1.x 的兼容性；
> 如不可用，可改用 `hyper-http-proxy`（基于 hyper 1.x 的 fork）或自行实现 `CONNECT` tunnel connector。

### 3.3 阶段一：类型别名与 client 构建（`src/network.rs`）

**目标**：`ClientNetworkConfig::build_client()` 返回 hyper 客户端，隔离所有 reqwest 依赖。

**步骤：**

1. 定义统一的客户端类型别名：

```rust
// 默认路径：rustls
type RustlsConnector = hyper_rustls::HttpsConnector<
    hyper_util::client::legacy::connect::HttpConnector
>;
pub(crate) type BytehaulClient =
    hyper_util::client::legacy::Client<RustlsConnector, http_body_util::Empty<bytes::Bytes>>;

// native TLS 路径（cfg feature）
#[cfg(feature = "native-tls")]
type NativeTlsConnector = hyper_tls::HttpsConnector<
    hyper_util::client::legacy::connect::HttpConnector
>;
```

2. 修改 `build_client()` 实现：

```rust
pub(crate) fn build_client(&self) -> Result<BytehaulClient, DownloadError> {
    let mut http = HttpConnector::new();
    http.set_nodelay(true);
    http.set_recv_buffer_size(Some(512 * 1024));
    http.set_connect_timeout(Some(self.connect_timeout));
    http.enforce_http(false);

    // 自定义 DNS 解析器
    if let Some(resolver) = self.build_dns_resolver()? {
        http.set_resolver(resolver);  // hyper-util HttpConnector 支持泛型 Resolver
    }

    let tls = HttpsConnectorBuilder::new()
        .with_native_roots()?
        .https_or_http()
        .enable_http1()
        .wrap_connector(http);

    let mut builder = Client::builder(TokioExecutor::new());
    builder.pool_max_idle_per_host(0);

    Ok(builder.build(tls))
}
```

3. 将 DNS 解析器接口从 `reqwest::dns::Resolve` 改为 hyper-util 的 `tower::Service`（`HttpConnector` 的泛型参数）：

```rust
// 旧接口
impl reqwest::dns::Resolve for BytehaulDnsResolver { ... }

// 新接口（hyper-util HttpConnector<R> 中 R: Resolve）
// hyper_util::client::legacy::connect::dns::Resolve trait
impl hyper_util::client::legacy::connect::dns::Resolve for BytehaulDnsResolver {
    type Addrs = std::vec::IntoIter<SocketAddr>;
    type Error = BoxError;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Addrs, Self::Error>> + Send>>;
    fn resolve(&self, name: Name) -> Self::Future { ... }
}
```

4. DoH URL 解析中的 `reqwest::Url::parse(...)` 改为 `url::Url::parse(...)`，接口相同，无逻辑变化。

5. 代理支持（`all_proxy` / `http_proxy` / `https_proxy`）：

```rust
// 通过 hyper-proxy 实现
use hyper_proxy::{Intercept, Proxy, ProxyConnector};

let proxy = Proxy::new(Intercept::All, proxy_url.parse()?);
let connector = ProxyConnector::from_proxy(tls_connector, proxy)?;
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

### 3.9 测试更新

| 测试文件 | 变更点 |
|---|---|
| `src/http/worker.rs` 中的 `#[cfg(test)]` | `worker_for()` 改用 `BytehaulClient`；`reqwest::Response::from(...)` mock 改为直接构造 `hyper::Response` |
| `src/http/response.rs` 中的 `#[cfg(test)]` | 将 `reqwest::Response::from(http::Response::builder()...)` 改为直接传 `hyper::HeaderMap` |
| `src/http/request.rs` 中的 `#[cfg(test)]` | 断言对象从 `reqwest::Request` 改为 `hyper::Request` |

warp 测试服务器本身不涉及 reqwest，无需改动。

---

## 四、关键风险与注意事项

| 风险 | 说明 | 缓解措施 |
|---|---|---|
| `hyper-proxy` 兼容性 | hyper 1.x 生态的代理 crate 尚不成熟 | 评估 `hyper-http-proxy`（已支持 hyper 1.x）；代理功能可作为最后一个阶段实施 |
| native TLS | `hyper-tls 0.6` 支持 hyper 1.x，但 `use_native_tls=true` 路径需额外 feature flag | 保留 `use_native_tls` 选项但标记为 `#[cfg(feature = "native-tls")]` |
| 重定向安全 | 手动跟随重定向需防止 SSRF（跳转到内网地址）| 迁移时同步加入 URL scheme 白名单检查（仅允许 http/https） |
| SO_RCVBUF 实际生效 | Linux 内核可能将值钳制在 `rmem_max` 以内 | 保留 `log_os_socket_buffer_sizes()` 便于诊断；在 CI 中增加 socket buffer 日志断言 |
| body 超时 | reqwest 的 `.timeout()` 覆盖整个请求（连接+body）；hyper 版本需手动用 `tokio::time::timeout` 包裹 | 对连接阶段和 body 读取阶段分别设置超时 |
| HTTP/2 支持 | 迁移后默认仅 HTTP/1.1；如需 H2 需在 connector 上额外启用 | 当前 bytehaul 已强制 HTTP/1.1 用于并发分片，无需 H2 |

---

## 五、实施顺序建议

```
阶段一（network.rs）
  └─ 阶段二（request.rs）
       └─ 阶段三（response.rs）
            └─ 阶段四（worker.rs）
                 └─ 阶段五（manager.rs）
                      └─ 阶段六（session/ body 消费）
                           └─ 阶段七（代理 + native TLS）
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
