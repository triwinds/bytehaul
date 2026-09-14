use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use http::Uri;
use url::Url;

pub(crate) mod curl;
pub(crate) mod dns;

pub(crate) use dns::BytehaulDnsResolver;

use crate::config::{DEFAULT_HTTP_IDLE_POOL_MAX_PER_HOST, DEFAULT_HTTP_IDLE_POOL_TIMEOUT};
use crate::error::DownloadError;
use crate::http::{HttpRequestBody, HttpResponse};

use self::curl::CurlTransport;

/// Per-request connection budget, including DNS resolution.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ConnectTimeout(pub Duration);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct ClientNetworkConfig {
    pub connect_timeout: Duration,
    pub pool_max_idle_per_host: usize,
    pub pool_idle_timeout: Duration,
    pub all_proxy: Option<String>,
    pub http_proxy: Option<String>,
    pub https_proxy: Option<String>,
    pub ca_info: Option<PathBuf>,
    pub ca_path: Option<PathBuf>,
    pub client_cert: Option<PathBuf>,
    pub client_key: Option<PathBuf>,
    pub dns_servers: Vec<SocketAddr>,
    pub doh_servers: Vec<String>,
    pub enable_ipv6: bool,
}

/// Shared handle to one transport client.
///
/// libcurl is the only production transport: the crate rejects builds without
/// `curl-backend`, so this is a handle to the concrete client rather than a
/// backend-selecting enum. Requests and responses still cross this type as
/// neutral `http` types, which is what keeps libcurl details out of the
/// manager and session layers.
#[derive(Clone)]
pub(crate) struct BytehaulClient(Arc<CurlTransport>);

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
    /// Production code always goes through [`Self::request_with_timeout`];
    /// this deadline-backed helper exists for transport tests.
    #[cfg(test)]
    pub(crate) async fn request(
        &self,
        req: http::Request<HttpRequestBody>,
    ) -> Result<HttpResponse, DownloadError> {
        self.0.request_with_backstop(req).await
    }

    pub(crate) async fn request_with_timeout(
        &self,
        req: http::Request<HttpRequestBody>,
        timeout: Duration,
    ) -> Result<HttpResponse, DownloadError> {
        // The libcurl driver enforces the caller's deadline itself, so it can
        // remove the handle at the moment the deadline expires. The caller's
        // value is the only deadline: a shorter internal default would
        // silently override a longer `request_headers_timeout`.
        self.0.request(req, timeout).await
    }

    /// Counters reported by this client's driver thread.
    pub(crate) fn driver_stats(&self) -> self::curl::driver::DriverStats {
        self.0.driver_stats()
    }

    /// Whether both handles share one transport (test-only cache assertion).
    #[cfg(test)]
    pub(crate) fn same_transport(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
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
            ca_info: None,
            ca_path: None,
            client_cert: None,
            client_key: None,
            dns_servers: Vec::new(),
            doh_servers: Vec::new(),
            enable_ipv6: true,
        }
    }
}

impl ClientNetworkConfig {
    pub(crate) fn build_client(&self) -> Result<BytehaulClient, DownloadError> {
        let effective_proxies = self.effective_proxies()?;
        // Record which libcurl/TLS/resolver build is linked.
        self::curl::log_runtime_features(crate::config::LogLevel::Debug);
        #[cfg(not(tarpaulin))]
        tracing::debug!(
            connect_timeout_ms = self.connect_timeout.as_millis() as u64,
            pool_max_idle_per_host = self.pool_max_idle_per_host,
            pool_idle_timeout_ms = self.pool_idle_timeout.as_millis() as u64,
            has_proxy = effective_proxies.has_proxy(),
            custom_dns = !self.dns_servers.is_empty(),
            custom_doh = !self.doh_servers.is_empty(),
            enable_ipv6 = self.enable_ipv6,
            "building HTTP client"
        );

        Ok(BytehaulClient(Arc::new(CurlTransport::new(self)?)))
    }

    pub(crate) fn with_connect_timeout(&self, connect_timeout: Duration) -> Self {
        let mut updated = self.clone();
        updated.connect_timeout = connect_timeout;
        updated
    }

    /// Builds the shared Hickory resolver used before each libcurl transfer.
    fn build_dns_resolver(&self) -> Result<BytehaulDnsResolver, DownloadError> {
        BytehaulDnsResolver::new(&self.dns_servers, &self.doh_servers, self.enable_ipv6)
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
}
