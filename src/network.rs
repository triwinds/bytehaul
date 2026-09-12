use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use http::Uri;
#[cfg(feature = "hyper-backend")]
use hyper_http_proxy::{Intercept, Proxy, ProxyConnector};
#[cfg(feature = "hyper-backend")]
use hyper_rustls::{HttpsConnector, HttpsConnectorBuilder};
#[cfg(feature = "hyper-backend")]
use hyper_util::client::legacy::{connect::HttpConnector, Client};
#[cfg(feature = "hyper-backend")]
use hyper_util::rt::{TokioExecutor, TokioTimer};
use url::Url;

#[cfg(feature = "curl-backend")]
pub(crate) mod curl;
pub(crate) mod dns;
#[cfg(all(test, feature = "hyper-backend"))]
mod multi_ip_prototype;
#[cfg(all(test, feature = "hyper-backend"))]
mod tls_tests;

pub(crate) use dns::BytehaulDnsResolver;

use crate::config::{DEFAULT_HTTP_IDLE_POOL_MAX_PER_HOST, DEFAULT_HTTP_IDLE_POOL_TIMEOUT};
use crate::error::DownloadError;
#[cfg(feature = "hyper-backend")]
use crate::error::TransportError;
#[cfg(feature = "hyper-backend")]
use crate::http::HttpBody;
use crate::http::{HttpRequestBody, HttpResponse};

#[cfg(feature = "curl-backend")]
use self::curl::CurlTransport;

#[cfg(feature = "hyper-backend")]
type DirectHttpConnector = HttpConnector<BytehaulDnsResolver>;
#[cfg(feature = "hyper-backend")]
type DirectRustlsConnector = HttpsConnector<DirectHttpConnector>;
#[cfg(feature = "hyper-backend")]
type DirectRustlsClient = Client<DirectRustlsConnector, HttpRequestBody>;
#[cfg(feature = "hyper-backend")]
type ProxyRustlsConnector = ProxyConnector<DirectRustlsConnector>;
#[cfg(feature = "hyper-backend")]
type ProxyRustlsClient = Client<ProxyRustlsConnector, HttpRequestBody>;

/// Which transport a client uses.
///
/// `Default` keeps the compile-time choice: Hyper while both backends are
/// built (until P5 flips the default), libcurl when it is the only backend in
/// the build. The explicit variants let internal tests and benchmarks exercise
/// both backends from one build; a single-backend build can only name the
/// variant it compiled, hence the dead-code allowance.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub(crate) enum TransportBackend {
    #[default]
    Default,
    #[cfg(feature = "hyper-backend")]
    Hyper,
    #[cfg(feature = "curl-backend")]
    Curl,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ResolvedBackend {
    #[cfg(feature = "hyper-backend")]
    Hyper,
    #[cfg(feature = "curl-backend")]
    Curl,
}

#[cfg(feature = "hyper-backend")]
const COMPILE_TIME_BACKEND: ResolvedBackend = ResolvedBackend::Hyper;
#[cfg(all(feature = "curl-backend", not(feature = "hyper-backend")))]
const COMPILE_TIME_BACKEND: ResolvedBackend = ResolvedBackend::Curl;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct ClientNetworkConfig {
    pub connect_timeout: Duration,
    pub pool_max_idle_per_host: usize,
    pub pool_idle_timeout: Duration,
    pub all_proxy: Option<String>,
    pub http_proxy: Option<String>,
    pub https_proxy: Option<String>,
    pub dns_servers: Vec<SocketAddr>,
    pub doh_servers: Vec<String>,
    pub enable_ipv6: bool,
    pub backend: TransportBackend,
}

#[derive(Clone)]
pub(crate) enum BytehaulClient {
    #[cfg(feature = "hyper-backend")]
    Direct(Arc<DirectRustlsClient>),
    #[cfg(feature = "hyper-backend")]
    Proxy(Arc<BytehaulProxyClient>),
    #[cfg(feature = "curl-backend")]
    Curl(Arc<CurlTransport>),
}

#[cfg(feature = "hyper-backend")]
#[derive(Clone)]
pub(crate) struct BytehaulProxyClient {
    client: ProxyRustlsClient,
    connector: ProxyRustlsConnector,
}

#[derive(Debug, Clone, Default)]
struct EffectiveProxyConfig {
    all_proxy: Option<Uri>,
    http_proxy: Option<Uri>,
    https_proxy: Option<Uri>,
}

impl EffectiveProxyConfig {
    fn has_proxy(&self) -> bool {
        self.all_proxy.is_some() || self.http_proxy.is_some() || self.https_proxy.is_some()
    }
}

impl BytehaulClient {
    /// Send one request without a caller-supplied deadline.
    ///
    /// Production code always goes through [`Self::request_with_timeout`]; this
    /// entry point backs the Hyper deadline wrapper and the tests, so a
    /// libcurl-only build does not carry it.
    #[cfg(any(feature = "hyper-backend", test))]
    pub(crate) async fn request(
        &self,
        req: http::Request<HttpRequestBody>,
    ) -> Result<HttpResponse, DownloadError> {
        match self {
            #[cfg(feature = "hyper-backend")]
            Self::Direct(client) => client
                .request(req)
                .await
                .map(neutral_response)
                .map_err(|error| TransportError::from(error).into()),
            #[cfg(feature = "hyper-backend")]
            Self::Proxy(client) => {
                let mut req = req;
                let uri = req.uri().clone();
                if let Some(headers) = client.connector.http_headers(&uri) {
                    req.headers_mut().extend(headers.clone());
                }
                client
                    .client
                    .request(req)
                    .await
                    .map(neutral_response)
                    .map_err(|error| TransportError::from(error).into())
            }
            #[cfg(feature = "curl-backend")]
            Self::Curl(transport) => transport.request_with_backstop(req).await,
        }
    }

    pub(crate) async fn request_with_timeout(
        &self,
        req: http::Request<HttpRequestBody>,
        timeout: Duration,
    ) -> Result<HttpResponse, DownloadError> {
        match self {
            // The libcurl driver enforces the caller's deadline itself, so it
            // can remove the handle at the moment the deadline expires. The
            // caller's value is the only deadline: a shorter internal default
            // would silently override a longer `request_headers_timeout`.
            #[cfg(feature = "curl-backend")]
            Self::Curl(transport) => transport.request(req, timeout).await,
            // The Hyper client has no deadline of its own, so the caller's
            // value wraps the whole request future (connect, TLS, headers).
            #[cfg(feature = "hyper-backend")]
            _ => tokio::time::timeout(timeout, self.request(req))
                .await
                .map_err(|_| DownloadError::timeout("request timed out"))?,
        }
    }
}

/// Hand the Hyper response to the session layer as the neutral body, so no
/// `hyper::body::Incoming` crosses the transport boundary (P1 of the libcurl
/// migration plan). The libcurl adapter does the same in `curl::transport`.
#[cfg(feature = "hyper-backend")]
fn neutral_response(response: http::Response<hyper::body::Incoming>) -> HttpResponse {
    let (parts, body) = response.into_parts();
    http::Response::from_parts(parts, HttpBody::from(body))
}

impl Default for ClientNetworkConfig {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(30),
            pool_max_idle_per_host: DEFAULT_HTTP_IDLE_POOL_MAX_PER_HOST,
            pool_idle_timeout: DEFAULT_HTTP_IDLE_POOL_TIMEOUT,
            all_proxy: None,
            http_proxy: None,
            https_proxy: None,
            dns_servers: Vec::new(),
            doh_servers: Vec::new(),
            enable_ipv6: true,
            backend: TransportBackend::default(),
        }
    }
}

impl ClientNetworkConfig {
    /// Select a transport explicitly; `TransportBackend::Default` keeps the
    /// build-time choice. Internal tests use this to compare both backends
    /// from a single build; the public selection API is not part of P2.
    #[cfg(all(test, feature = "hyper-backend", feature = "curl-backend"))]
    pub(crate) fn with_backend(&self, backend: TransportBackend) -> Self {
        let mut updated = self.clone();
        updated.backend = backend;
        updated
    }

    fn resolved_backend(&self) -> ResolvedBackend {
        match self.backend {
            #[cfg(feature = "hyper-backend")]
            TransportBackend::Hyper => ResolvedBackend::Hyper,
            #[cfg(feature = "curl-backend")]
            TransportBackend::Curl => ResolvedBackend::Curl,
            TransportBackend::Default => COMPILE_TIME_BACKEND,
        }
    }

    pub(crate) fn build_client(&self) -> Result<BytehaulClient, DownloadError> {
        let effective_proxies = self.effective_proxies()?;
        // Record which libcurl/TLS/resolver build is linked when the libcurl
        // backend is compiled in; the Hyper path logs its own connector facts.
        #[cfg(feature = "curl-backend")]
        crate::network::curl::log_runtime_features(crate::config::LogLevel::Debug);
        #[cfg(not(tarpaulin))]
        tracing::debug!(
            connect_timeout_ms = self.connect_timeout.as_millis() as u64,
            pool_max_idle_per_host = self.pool_max_idle_per_host,
            pool_idle_timeout_ms = self.pool_idle_timeout.as_millis() as u64,
            has_proxy = effective_proxies.has_proxy(),
            custom_dns = !self.dns_servers.is_empty(),
            custom_doh = !self.doh_servers.is_empty(),
            enable_ipv6 = self.enable_ipv6,
            backend = ?self.resolved_backend(),
            "building HTTP client"
        );

        match self.resolved_backend() {
            #[cfg(feature = "curl-backend")]
            ResolvedBackend::Curl => Ok(BytehaulClient::Curl(Arc::new(CurlTransport::new(self)?))),
            #[cfg(feature = "hyper-backend")]
            ResolvedBackend::Hyper => self.build_hyper_client(&effective_proxies),
        }
    }

    #[cfg(feature = "hyper-backend")]
    fn build_hyper_client(
        &self,
        effective_proxies: &EffectiveProxyConfig,
    ) -> Result<BytehaulClient, DownloadError> {
        let resolver = self.build_dns_resolver()?;
        let https = self.build_https_connector(resolver)?;
        let mut builder = Client::builder(TokioExecutor::new());
        builder.pool_max_idle_per_host(self.pool_max_idle_per_host);
        if self.pool_max_idle_per_host > 0 {
            builder.pool_timer(TokioTimer::new());
            builder.pool_idle_timeout(self.pool_idle_timeout);
        }

        if effective_proxies.has_proxy() {
            let mut proxy_connector = ProxyConnector::new(https).map_err(|error| {
                DownloadError::InvalidConfig(format!(
                    "failed to configure proxy connector: {error}"
                ))
            })?;
            if let Some(proxy) = effective_proxies.http_proxy.clone() {
                proxy_connector.add_proxy(Proxy::new(Intercept::Http, proxy));
            }
            if let Some(proxy) = effective_proxies.https_proxy.clone() {
                proxy_connector.add_proxy(Proxy::new(Intercept::Https, proxy));
            }
            if let Some(proxy) = effective_proxies.all_proxy.clone() {
                proxy_connector.add_proxy(Proxy::new(Intercept::All, proxy));
            }
            let client = builder.build(proxy_connector.clone());
            Ok(BytehaulClient::Proxy(Arc::new(BytehaulProxyClient {
                client,
                connector: proxy_connector,
            })))
        } else {
            let client = builder.build(https);
            Ok(BytehaulClient::Direct(Arc::new(client)))
        }
    }

    pub(crate) fn with_connect_timeout(&self, connect_timeout: Duration) -> Self {
        let mut updated = self.clone();
        updated.connect_timeout = connect_timeout;
        updated
    }

    /// Builds the shared Hickory resolver (P3: both backends resolve here).
    fn build_dns_resolver(&self) -> Result<BytehaulDnsResolver, DownloadError> {
        BytehaulDnsResolver::new(&self.dns_servers, &self.doh_servers, self.enable_ipv6)
    }

    #[cfg(feature = "hyper-backend")]
    fn build_https_connector(
        &self,
        resolver: BytehaulDnsResolver,
    ) -> Result<DirectRustlsConnector, DownloadError> {
        match HttpsConnectorBuilder::new().try_with_platform_verifier() {
            Ok(builder) => Ok(builder
                .https_or_http()
                .enable_http1()
                .wrap_connector(self.build_http_connector(resolver))),
            Err(error) => {
                #[cfg(not(tarpaulin))]
                tracing::debug!(error = %error, "platform verifier unavailable, falling back to native roots");
                let builder =
                    HttpsConnectorBuilder::new()
                        .with_native_roots()
                        .map_err(|fallback_error| {
                            DownloadError::Internal(format!(
                                "failed to initialize native TLS roots: {fallback_error}"
                            ))
                        })?;
                Ok(builder
                    .https_or_http()
                    .enable_http1()
                    .wrap_connector(self.build_http_connector(resolver)))
            }
        }
    }

    #[cfg(feature = "hyper-backend")]
    fn build_http_connector(&self, resolver: BytehaulDnsResolver) -> DirectHttpConnector {
        let mut http = HttpConnector::new_with_resolver(resolver);
        http.set_nodelay(true);
        http.set_recv_buffer_size(Some(512 * 1024));
        http.set_connect_timeout(Some(self.connect_timeout));
        http.enforce_http(false);
        http
    }

    fn effective_proxies(&self) -> Result<EffectiveProxyConfig, DownloadError> {
        Ok(EffectiveProxyConfig {
            all_proxy: proxy_uri(
                self.all_proxy.as_deref(),
                &["ALL_PROXY", "all_proxy"],
                "all_proxy",
            )?,
            http_proxy: proxy_uri(
                self.http_proxy.as_deref(),
                &["HTTP_PROXY", "http_proxy"],
                "http_proxy",
            )?,
            https_proxy: proxy_uri(
                self.https_proxy.as_deref(),
                &["HTTPS_PROXY", "https_proxy"],
                "https_proxy",
            )?,
        })
    }
}

fn env_proxy_value(names: &[&str]) -> Option<String> {
    names.iter().find_map(|name| {
        std::env::var(name)
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    })
}

fn proxy_uri(
    explicit: Option<&str>,
    env_names: &[&str],
    label: &str,
) -> Result<Option<Uri>, DownloadError> {
    let value = explicit
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .or_else(|| env_proxy_value(env_names));
    let Some(value) = value else {
        return Ok(None);
    };

    let parsed = Url::parse(&value).map_err(|error| {
        DownloadError::InvalidConfig(format!("invalid {label} URL '{value}': {error}"))
    })?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(DownloadError::InvalidConfig(format!(
            "{label} URL '{value}' must use http or https"
        )));
    }

    value.parse::<Uri>().map(Some).map_err(|error| {
        DownloadError::InvalidConfig(format!("invalid {label} URL '{value}': {error}"))
    })
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "hyper-backend")]
    use super::dns::spawn_dns_test_server;
    use super::*;
    use std::sync::Mutex as StdMutex;
    use std::{
        io::{Read, Write},
        net::TcpListener,
        thread,
    };

    fn env_lock() -> &'static StdMutex<()> {
        static LOCK: std::sync::OnceLock<StdMutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| StdMutex::new(()))
    }

    fn clear_proxy_env() {
        for key in [
            "ALL_PROXY",
            "all_proxy",
            "HTTP_PROXY",
            "http_proxy",
            "HTTPS_PROXY",
            "https_proxy",
        ] {
            std::env::remove_var(key);
        }
    }

    #[test]
    fn test_network_config_defaults() {
        let config = ClientNetworkConfig::default();
        assert_eq!(config.connect_timeout, Duration::from_secs(30));
        assert_eq!(
            config.pool_max_idle_per_host,
            DEFAULT_HTTP_IDLE_POOL_MAX_PER_HOST
        );
        assert_eq!(config.pool_idle_timeout, DEFAULT_HTTP_IDLE_POOL_TIMEOUT);
        assert!(config.all_proxy.is_none());
        assert!(config.http_proxy.is_none());
        assert!(config.https_proxy.is_none());
        assert!(config.dns_servers.is_empty());
        assert!(config.doh_servers.is_empty());
        assert!(config.enable_ipv6);
    }

    #[test]
    fn test_with_connect_timeout_clones_config() {
        let config = ClientNetworkConfig::default();
        let updated = config.with_connect_timeout(Duration::from_secs(9));

        assert_eq!(config.connect_timeout, Duration::from_secs(30));
        assert_eq!(updated.connect_timeout, Duration::from_secs(9));
        assert_eq!(
            updated.pool_max_idle_per_host,
            DEFAULT_HTTP_IDLE_POOL_MAX_PER_HOST
        );
        assert!(updated.enable_ipv6);
    }

    #[test]
    fn test_invalid_proxy_fails_client_build() {
        let config = ClientNetworkConfig {
            all_proxy: Some("not a proxy url".into()),
            ..ClientNetworkConfig::default()
        };

        let err = match config.build_client() {
            Ok(_) => panic!("expected invalid proxy configuration to fail"),
            Err(error) => error.to_string(),
        };
        assert!(err.contains("proxy") || err.contains("URL"));
    }

    #[test]
    fn test_build_client_accepts_http_and_https_proxies() {
        let _guard = env_lock().lock().unwrap();
        clear_proxy_env();
        let config = ClientNetworkConfig {
            http_proxy: Some("http://127.0.0.1:8080".into()),
            https_proxy: Some("http://127.0.0.1:8443".into()),
            enable_ipv6: false,
            ..ClientNetworkConfig::default()
        };

        config.build_client().unwrap();
        clear_proxy_env();
    }

    #[test]
    fn test_build_client_accepts_doh_servers() {
        let config = ClientNetworkConfig {
            doh_servers: vec!["https://127.0.0.1/dns-query".into()],
            enable_ipv6: false,
            ..ClientNetworkConfig::default()
        };

        config.build_client().unwrap();
    }

    #[test]
    fn test_explicit_proxy_wins_over_environment() {
        let _guard = env_lock().lock().unwrap();
        clear_proxy_env();
        std::env::set_var("HTTP_PROXY", "http://127.0.0.1:8000");

        let config = ClientNetworkConfig {
            http_proxy: Some("http://127.0.0.1:8080".into()),
            ..ClientNetworkConfig::default()
        };
        let proxies = config.effective_proxies().unwrap();

        assert_eq!(
            proxies.http_proxy.unwrap().authority().unwrap().as_str(),
            "127.0.0.1:8080"
        );
        clear_proxy_env();
    }

    #[test]
    fn test_environment_proxy_used_when_builder_proxy_missing() {
        let _guard = env_lock().lock().unwrap();
        clear_proxy_env();
        std::env::set_var("HTTPS_PROXY", "http://127.0.0.1:8443");

        let proxies = ClientNetworkConfig::default().effective_proxies().unwrap();
        assert_eq!(
            proxies.https_proxy.unwrap().authority().unwrap().as_str(),
            "127.0.0.1:8443"
        );
        clear_proxy_env();
    }

    #[test]
    fn test_proxy_uri_rejects_non_http_scheme() {
        let err = proxy_uri(Some("ftp://127.0.0.1:21"), &[], "all_proxy")
            .unwrap_err()
            .to_string();
        assert!(err.contains("must use http or https"), "got: {err}");
    }

    #[test]
    fn test_proxy_uri_discards_fragment_after_url_parse() {
        let proxy = proxy_uri(Some("http://127.0.0.1:8080#frag"), &[], "http_proxy")
            .unwrap()
            .unwrap();
        assert_eq!(proxy.to_string(), "http://127.0.0.1:8080/");
    }

    #[cfg(feature = "hyper-backend")]
    #[tokio::test]
    async fn test_proxy_client_request_uses_proxy_branch() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0u8; 4096];
            let n = stream.read(&mut request).unwrap();
            let text = String::from_utf8_lossy(&request[..n]);
            assert!(text.starts_with("GET "), "request was: {text}");
            let response = b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\npong";
            stream.write_all(response).unwrap();
        });

        let client = {
            let _guard = env_lock().lock().unwrap();
            clear_proxy_env();
            ClientNetworkConfig {
                http_proxy: Some(format!("http://{addr}")),
                ..ClientNetworkConfig::default()
            }
            .build_client()
            .unwrap()
        };

        let req = http::Request::builder()
            .method("GET")
            .uri("http://example.com/proxy-test")
            .body(HttpRequestBody::new())
            .unwrap();
        let response = client.request(req).await.unwrap();
        assert_eq!(response.status(), http::StatusCode::OK);
        let bytes = response.into_body().collect_to_bytes().await.unwrap();
        assert_eq!(&bytes[..], b"pong");

        handle.join().unwrap();
    }

    #[tokio::test]
    async fn test_request_with_timeout_returns_timeout_error() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0u8; 4096];
            let _ = stream.read(&mut request).unwrap();
            thread::sleep(Duration::from_millis(100));
            let response = b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\nslow";
            let _ = stream.write_all(response);
        });

        let client = ClientNetworkConfig::default().build_client().unwrap();
        let req = http::Request::builder()
            .method("GET")
            .uri(format!("http://{addr}/slow"))
            .body(HttpRequestBody::new())
            .unwrap();

        let err = client
            .request_with_timeout(req, Duration::from_millis(20))
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            DownloadError::Transport(ref transport)
                if transport.kind() == crate::error::TransportErrorKind::Timeout
        ));

        handle.join().unwrap();
    }

    #[cfg(feature = "hyper-backend")]
    fn spawn_connection_pool_test_server(
        close_after_response: bool,
        expected_requests: usize,
    ) -> (
        SocketAddr,
        Arc<std::sync::atomic::AtomicUsize>,
        thread::JoinHandle<()>,
    ) {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        let accepted = Arc::new(AtomicUsize::new(0));
        let accepted_for_thread = accepted.clone();

        let handle = thread::spawn(move || {
            let mut served = 0usize;
            while served < expected_requests {
                let (mut stream, _) = listener.accept().unwrap();
                accepted_for_thread.fetch_add(1, Ordering::SeqCst);
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();

                loop {
                    let mut request = Vec::new();
                    let mut byte = [0u8; 1];
                    loop {
                        match stream.read(&mut byte) {
                            Ok(0) => return,
                            Ok(_) => {
                                request.push(byte[0]);
                                if request.ends_with(b"\r\n\r\n") {
                                    break;
                                }
                            }
                            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                                continue;
                            }
                            Err(error) => panic!("failed to read request: {error}"),
                        }
                    }

                    let (body, start, end, total) = match served {
                        0 => (b"pong".as_slice(), 0, 3, 8),
                        _ => (b"more".as_slice(), 4, 7, 8),
                    };
                    let connection_header = if close_after_response {
                        "close"
                    } else {
                        "keep-alive"
                    };
                    let response = format!(
                        concat!(
                            "HTTP/1.1 206 Partial Content\r\n",
                            "Content-Length: {}\r\n",
                            "Content-Range: bytes {}-{}/{}\r\n",
                            "Connection: {}\r\n\r\n",
                        ),
                        body.len(),
                        start,
                        end,
                        total,
                        connection_header,
                    );
                    stream.write_all(response.as_bytes()).unwrap();
                    stream.write_all(body).unwrap();
                    stream.flush().unwrap();

                    served += 1;
                    if close_after_response || served >= expected_requests {
                        break;
                    }
                }
            }
        });

        (addr, accepted, handle)
    }

    #[cfg(feature = "hyper-backend")]
    #[tokio::test]
    async fn test_idle_pool_closes_expired_socket_without_another_request() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            loop {
                let mut byte = [0];
                assert_eq!(stream.read(&mut byte).await.unwrap(), 1);
                request.push(byte[0]);
                if request.ends_with(b"\r\n\r\n") {
                    break;
                }
            }
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\npong")
                .await
                .unwrap();
            let mut byte = [0];
            assert_eq!(stream.read(&mut byte).await.unwrap(), 0);
        });
        let client = {
            let _guard = env_lock().lock().unwrap();
            clear_proxy_env();
            ClientNetworkConfig {
                pool_idle_timeout: Duration::from_millis(20),
                ..ClientNetworkConfig::default()
            }
            .build_client()
            .unwrap()
        };
        let response = client
            .request_with_timeout(
                http::Request::builder()
                    .uri(format!("http://{addr}/"))
                    .body(HttpRequestBody::new())
                    .unwrap(),
                Duration::from_secs(5),
            )
            .await
            .unwrap();
        response.into_body().collect_to_bytes().await.unwrap();
        // Keep the client alive: EOF must come from idle expiry, not pool drop.
        tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .expect("idle socket was not reclaimed")
            .unwrap();
        drop(client);
    }

    #[cfg(feature = "hyper-backend")]
    #[tokio::test]
    async fn test_idle_pool_reuses_keep_alive_connection() {
        use std::sync::atomic::Ordering;

        let (addr, accepted, handle) = spawn_connection_pool_test_server(false, 2);
        let client = ClientNetworkConfig {
            pool_max_idle_per_host: 1,
            pool_idle_timeout: Duration::from_secs(30),
            ..ClientNetworkConfig::default()
        }
        .build_client()
        .unwrap();

        for (expected_start, expected_end) in [(0, 3), (4, 7)] {
            let req = http::Request::builder()
                .method("GET")
                .uri(format!("http://{addr}/range"))
                .header("range", format!("bytes={expected_start}-{expected_end}"))
                .body(HttpRequestBody::new())
                .unwrap();
            let response = client.request(req).await.unwrap();
            assert_eq!(response.status(), http::StatusCode::PARTIAL_CONTENT);
            let bytes = response.into_body().collect_to_bytes().await.unwrap();
            assert_eq!(bytes.len(), 4);
        }

        handle.join().unwrap();
        assert_eq!(accepted.load(Ordering::SeqCst), 1);
    }

    #[cfg(feature = "hyper-backend")]
    #[tokio::test]
    async fn test_connector_reconnects_using_hickory_cached_dns_answer() {
        use std::sync::atomic::Ordering;

        let (dns_addr, queries, dns_server) = spawn_dns_test_server(60).await;
        let (http_addr, accepted, http_server) = spawn_connection_pool_test_server(true, 2);
        let client = {
            let _guard = env_lock().lock().unwrap();
            clear_proxy_env();
            ClientNetworkConfig {
                dns_servers: vec![dns_addr],
                enable_ipv6: false,
                ..ClientNetworkConfig::default()
            }
            .build_client()
            .unwrap()
        };
        for expected in [b"pong", b"more"] {
            let req = http::Request::builder()
                .uri(format!(
                    "http://cache-test.example.:{}/range",
                    http_addr.port()
                ))
                .body(HttpRequestBody::new())
                .unwrap();
            let response = client.request(req).await.unwrap();
            let bytes = response.into_body().collect_to_bytes().await.unwrap();
            assert_eq!(bytes.as_ref(), expected);
        }
        http_server.join().unwrap();
        assert_eq!(accepted.load(Ordering::SeqCst), 2);
        assert_eq!(queries.load(Ordering::SeqCst), 1);
        dns_server.abort();
    }

    #[cfg(feature = "hyper-backend")]
    #[tokio::test]
    async fn test_idle_pool_reconnects_after_connection_close() {
        use std::sync::atomic::Ordering;

        let (addr, accepted, handle) = spawn_connection_pool_test_server(true, 2);
        let client = ClientNetworkConfig {
            pool_max_idle_per_host: 1,
            pool_idle_timeout: Duration::from_secs(30),
            ..ClientNetworkConfig::default()
        }
        .build_client()
        .unwrap();

        for (expected_start, expected_end) in [(0, 3), (4, 7)] {
            let req = http::Request::builder()
                .method("GET")
                .uri(format!("http://{addr}/range"))
                .header("range", format!("bytes={expected_start}-{expected_end}"))
                .body(HttpRequestBody::new())
                .unwrap();
            let response = client.request(req).await.unwrap();
            assert_eq!(response.status(), http::StatusCode::PARTIAL_CONTENT);
            let bytes = response.into_body().collect_to_bytes().await.unwrap();
            assert_eq!(bytes.len(), 4);
        }

        handle.join().unwrap();
        assert_eq!(accepted.load(Ordering::SeqCst), 2);
    }
}
