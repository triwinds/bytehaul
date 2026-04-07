use std::collections::HashMap;
use std::future::Future;
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::pin::Pin;
use std::sync::{Arc, OnceLock};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use hickory_resolver::config::{
    LookupIpStrategy, NameServerConfig, NameServerConfigGroup, ResolverConfig,
};
use hickory_resolver::proto::xfer::Protocol;
use hickory_resolver::{name_server::TokioConnectionProvider, TokioResolver};
use hyper::Uri;
use hyper_http_proxy::{Intercept, Proxy, ProxyConnector};
use hyper_rustls::{HttpsConnector, HttpsConnectorBuilder};
use hyper_util::client::legacy::{
    connect::{dns::Name as DnsName, HttpConnector},
    Client,
};
use hyper_util::rt::TokioExecutor;
use parking_lot::Mutex;
use tower_service::Service;
use url::Url;

use crate::error::{BoxError, DownloadError, TransportError};
use crate::http::{HttpRequestBody, HttpResponse};

type SharedDnsCache = Arc<Mutex<HashMap<String, CachedDnsLookup>>>;
type DirectHttpConnector = HttpConnector<BytehaulDnsResolver>;
type DirectRustlsConnector = HttpsConnector<DirectHttpConnector>;
type DirectRustlsClient = Client<DirectRustlsConnector, HttpRequestBody>;
type ProxyRustlsConnector = ProxyConnector<DirectRustlsConnector>;
type ProxyRustlsClient = Client<ProxyRustlsConnector, HttpRequestBody>;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct ClientNetworkConfig {
    pub connect_timeout: Duration,
    pub all_proxy: Option<String>,
    pub http_proxy: Option<String>,
    pub https_proxy: Option<String>,
    pub dns_servers: Vec<SocketAddr>,
    pub doh_servers: Vec<String>,
    pub enable_ipv6: bool,
}

#[derive(Clone)]
pub(crate) enum BytehaulClient {
    Direct(BytehaulDirectClient),
    Proxy(BytehaulProxyClient),
}

#[derive(Clone)]
pub(crate) struct BytehaulDirectClient {
    client: DirectRustlsClient,
    resolver: BytehaulDnsResolver,
}

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
    pub(crate) async fn request(
        &self,
        req: hyper::Request<HttpRequestBody>,
    ) -> Result<HttpResponse, DownloadError> {
        match self {
            Self::Direct(client) => client
                .client
                .request(req)
                .await
                .map_err(|error| TransportError::from(error).into()),
            Self::Proxy(client) => {
                let mut req = req;
                let uri = req.uri().clone();
                if let Some(headers) = client.connector.http_headers(&uri) {
                    req.headers_mut().extend(headers.clone().into_iter());
                }
                client
                    .client
                    .request(req)
                    .await
                    .map_err(|error| TransportError::from(error).into())
            }
        }
    }

    pub(crate) async fn request_with_timeout(
        &self,
        req: hyper::Request<HttpRequestBody>,
        timeout: Duration,
    ) -> Result<HttpResponse, DownloadError> {
        tokio::time::timeout(timeout, self.request(req))
            .await
            .map_err(|_| DownloadError::timeout("request timed out"))?
    }

    pub(crate) async fn warm_resolution_for_url(&self, url: &str) -> Result<(), DownloadError> {
        let Self::Direct(client) = self else {
            return Ok(());
        };

        let Ok(parsed) = Url::parse(url) else {
            return Ok(());
        };
        let Some(host) = parsed.host_str() else {
            return Ok(());
        };

        if let Err(error) = client.resolver.warm_lookup(host).await {
            #[cfg(not(tarpaulin))]
            tracing::debug!(host, error = %error, "DNS warmup skipped after lookup failure");
        }
        Ok(())
    }
}

impl Default for ClientNetworkConfig {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(30),
            all_proxy: None,
            http_proxy: None,
            https_proxy: None,
            dns_servers: Vec::new(),
            doh_servers: Vec::new(),
            enable_ipv6: true,
        }
    }
}

impl ClientNetworkConfig {
    pub(crate) fn build_client(&self) -> Result<BytehaulClient, DownloadError> {
        let effective_proxies = self.effective_proxies()?;
        #[cfg(not(tarpaulin))]
        tracing::debug!(connect_timeout_ms = self.connect_timeout.as_millis() as u64,
            has_proxy = effective_proxies.has_proxy(),
            custom_dns = !self.dns_servers.is_empty(),
            custom_doh = !self.doh_servers.is_empty(),
            enable_ipv6 = self.enable_ipv6,
            "building HTTP client");

        let resolver = self.build_dns_resolver()?;
        let https = self.build_https_connector(resolver.clone())?;
        let mut builder = Client::builder(TokioExecutor::new());
        builder.pool_max_idle_per_host(0);

        if effective_proxies.has_proxy() {
            let mut proxy_connector = ProxyConnector::new(https).map_err(|error| {
                DownloadError::InvalidConfig(format!("failed to configure proxy connector: {error}"))
            })?;
            if let Some(proxy) = effective_proxies.http_proxy {
                proxy_connector.add_proxy(Proxy::new(Intercept::Http, proxy));
            }
            if let Some(proxy) = effective_proxies.https_proxy {
                proxy_connector.add_proxy(Proxy::new(Intercept::Https, proxy));
            }
            if let Some(proxy) = effective_proxies.all_proxy {
                proxy_connector.add_proxy(Proxy::new(Intercept::All, proxy));
            }
            let client = builder.build(proxy_connector.clone());
            Ok(BytehaulClient::Proxy(BytehaulProxyClient {
                client,
                connector: proxy_connector,
            }))
        } else {
            let client = builder.build(https);
            Ok(BytehaulClient::Direct(BytehaulDirectClient { client, resolver }))
        }
    }

    pub(crate) fn with_connect_timeout(&self, connect_timeout: Duration) -> Self {
        let mut updated = self.clone();
        updated.connect_timeout = connect_timeout;
        updated
    }

    fn build_dns_resolver(&self) -> Result<BytehaulDnsResolver, DownloadError> {
        BytehaulDnsResolver::new(&self.dns_servers, &self.doh_servers, self.enable_ipv6)
    }

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
                let builder = HttpsConnectorBuilder::new()
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
            all_proxy: proxy_uri(self.all_proxy.as_deref(), &["ALL_PROXY", "all_proxy"], "all_proxy")?,
            http_proxy: proxy_uri(self.http_proxy.as_deref(), &["HTTP_PROXY", "http_proxy"], "http_proxy")?,
            https_proxy: proxy_uri(self.https_proxy.as_deref(), &["HTTPS_PROXY", "https_proxy"], "https_proxy")?,
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

fn doh_config_cache() -> &'static Mutex<HashMap<(String, bool), DohServerConfig>> {
    static CACHE: OnceLock<Mutex<HashMap<(String, bool), DohServerConfig>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DohServerConfig {
    socket_addrs: Vec<SocketAddr>,
    tls_dns_name: String,
    http_endpoint: Option<String>,
}

#[derive(Debug, Clone)]
struct CachedDnsLookup {
    valid_until: Instant,
    addrs: Vec<SocketAddr>,
}

#[derive(Clone)]
struct BytehaulDnsResolver {
    resolver: TokioResolver,
    cache: SharedDnsCache,
}

type ResolverFuture = Pin<
    Box<dyn Future<Output = Result<std::vec::IntoIter<SocketAddr>, BoxError>> + Send>,
>;

impl BytehaulDnsResolver {
    fn new(
        dns_servers: &[SocketAddr],
        doh_servers: &[String],
        enable_ipv6: bool,
    ) -> Result<Self, DownloadError> {
        let mut builder = if dns_servers.is_empty() && doh_servers.is_empty() {
            TokioResolver::builder_tokio().map_err(|err| {
                DownloadError::Internal(format!("failed to read system DNS configuration: {err}"))
            })?
        } else {
            TokioResolver::builder_with_config(
                ResolverConfig::from_parts(
                    None,
                    vec![],
                    build_name_server_group(dns_servers, doh_servers, enable_ipv6)?,
                ),
                TokioConnectionProvider::default(),
            )
        };

        builder.options_mut().ip_strategy = if enable_ipv6 {
            LookupIpStrategy::Ipv4AndIpv6
        } else {
            LookupIpStrategy::Ipv4Only
        };

        Ok(Self {
            resolver: builder.build(),
            cache: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    async fn warm_lookup(&self, host: &str) -> Result<(), BoxError> {
        self.lookup_host(host.to_string()).await.map(|_| ())
    }

    async fn lookup_host(&self, host: String) -> Result<Vec<SocketAddr>, BoxError> {
        if let Some(cached) = load_cached_lookup(&self.cache, &host) {
            #[cfg(not(tarpaulin))]
            tracing::debug!(
                host = %host,
                addrs = ?cached.addrs,
                cache_hit = true,
                ttl_remaining_ms = duration_to_u64_millis(
                    cached.valid_until.saturating_duration_since(Instant::now())
                ),
                "resolved host via DNS cache"
            );
            return Ok(cached.addrs);
        }

        let lookup = self.resolver.lookup_ip(host.clone()).await.map_err(|error| {
            #[cfg(not(tarpaulin))]
            tracing::debug!(host = %host, cache_hit = false, error = %error, "DNS lookup failed");
            let boxed: BoxError = Box::new(error);
            boxed
        })?;

        let valid_until = lookup.valid_until();
        let addrs: Vec<SocketAddr> = lookup.iter().map(|ip| SocketAddr::new(ip, 0)).collect();
        if addrs.is_empty() {
            #[cfg(not(tarpaulin))]
            tracing::debug!(host = %host, cache_hit = false, "DNS lookup returned no IP addresses");
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("no DNS records found for {host}"),
            )
            .into());
        }

        store_cached_lookup(&self.cache, host.clone(), addrs.clone(), valid_until);

        #[cfg(not(tarpaulin))]
        tracing::debug!(
            host = %host,
            addrs = ?addrs,
            cache_hit = false,
            ttl_remaining_ms = duration_to_u64_millis(
                valid_until.saturating_duration_since(Instant::now())
            ),
            "resolved host via DNS lookup"
        );

        Ok(addrs)
    }
}

impl Service<DnsName> for BytehaulDnsResolver {
    type Response = std::vec::IntoIter<SocketAddr>;
    type Error = BoxError;
    type Future = ResolverFuture;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, name: DnsName) -> Self::Future {
        let resolver = self.clone();
        let host = name.as_str().to_string();
        Box::pin(async move { resolver.lookup_host(host).await.map(|addrs| addrs.into_iter()) })
    }
}

fn load_cached_lookup(cache: &SharedDnsCache, host: &str) -> Option<CachedDnsLookup> {
    let mut cache = cache.lock();
    let cached = cache.get(host).cloned()?;
    if cached.valid_until > Instant::now() {
        Some(cached)
    } else {
        cache.remove(host);
        None
    }
}

fn store_cached_lookup(
    cache: &SharedDnsCache,
    host: String,
    addrs: Vec<SocketAddr>,
    valid_until: Instant,
) {
    cache.lock().insert(host, CachedDnsLookup { valid_until, addrs });
}

fn duration_to_u64_millis(duration: Duration) -> u64 {
    duration.as_millis().min(u64::MAX as u128) as u64
}

fn build_name_server_group(
    dns_servers: &[SocketAddr],
    doh_servers: &[String],
    enable_ipv6: bool,
) -> Result<NameServerConfigGroup, DownloadError> {
    let mut group = NameServerConfigGroup::new();
    for server in dns_servers {
        for protocol in [Protocol::Udp, Protocol::Tcp] {
            group.push(NameServerConfig::new(*server, protocol));
        }
    }

    for server in doh_servers {
        let config = parse_doh_server(server, enable_ipv6)?;
        for socket_addr in config.socket_addrs {
            let mut name_server = NameServerConfig::new(socket_addr, Protocol::Https);
            name_server.tls_dns_name = Some(config.tls_dns_name.clone());
            name_server.http_endpoint = config.http_endpoint.clone();
            group.push(name_server);
        }
    }

    Ok(group)
}

fn parse_doh_server(server: &str, enable_ipv6: bool) -> Result<DohServerConfig, DownloadError> {
    let server = server.trim();
    if server.is_empty() {
        return Err(DownloadError::InvalidConfig(
            "DoH server URLs cannot be empty".into(),
        ));
    }
    if let Some(authority) = server.strip_prefix("https://") {
        if authority.is_empty()
            || authority.starts_with('/')
            || authority.starts_with('?')
            || authority.starts_with('#')
        {
            return Err(DownloadError::InvalidConfig(format!(
                "DoH server URL '{server}' is missing a host"
            )));
        }
    }

    if let Some(cached) = doh_config_cache()
        .lock()
        .get(&(server.to_string(), enable_ipv6))
        .cloned()
    {
        return Ok(cached);
    }

    let url = Url::parse(server).map_err(|error| {
        if error.to_string() == "empty host" {
            DownloadError::InvalidConfig(format!("DoH server URL '{server}' is missing a host"))
        } else {
            DownloadError::InvalidConfig(format!("invalid DoH server URL '{server}': {error}"))
        }
    })?;

    if url.scheme() != "https" {
        return Err(DownloadError::InvalidConfig(format!(
            "DoH server URL '{server}' must use https"
        )));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(DownloadError::InvalidConfig(format!(
            "DoH server URL '{server}' cannot include credentials"
        )));
    }
    if url.fragment().is_some() {
        return Err(DownloadError::InvalidConfig(format!(
            "DoH server URL '{server}' cannot include a fragment"
        )));
    }

    let host = url.host().ok_or_else(|| {
        DownloadError::InvalidConfig(format!("DoH server URL '{server}' is missing a host"))
    })?;
    let host_display = host.to_string();
    let port = url.port_or_known_default().ok_or_else(|| {
        DownloadError::InvalidConfig(format!(
            "DoH server URL '{server}' is missing a valid port"
        ))
    })?;

    let host_for_resolution = host_display
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
        .unwrap_or(host_display.as_str());
    let parsed_ip = host_for_resolution.parse::<IpAddr>().ok();

    let mut socket_addrs: Vec<SocketAddr> = if let Some(ip) = parsed_ip {
        vec![SocketAddr::new(ip, port)]
    } else {
        (host_for_resolution, port)
            .to_socket_addrs()
            .map_err(|err| {
                DownloadError::InvalidConfig(format!(
                    "failed to resolve DoH host '{host_display}' from '{server}': {err}"
                ))
            })?
            .collect()
    };

    if !enable_ipv6 {
        socket_addrs.retain(SocketAddr::is_ipv4);
    }
    socket_addrs.sort_unstable();
    socket_addrs.dedup();

    if socket_addrs.is_empty() {
        return Err(DownloadError::InvalidConfig(format!(
            "DoH server URL '{server}' did not resolve to any {} address",
            if enable_ipv6 { "IP" } else { "IPv4" }
        )));
    }

    let mut http_endpoint = url.path().to_string();
    if http_endpoint.is_empty() || http_endpoint == "/" {
        http_endpoint.clear();
    }
    if let Some(query) = url.query() {
        if http_endpoint.is_empty() {
            http_endpoint.push('/');
        }
        http_endpoint.push('?');
        http_endpoint.push_str(query);
    }

    let config = DohServerConfig {
        socket_addrs,
        tls_dns_name: match parsed_ip {
            Some(IpAddr::V4(ipv4)) => ipv4.to_string(),
            Some(IpAddr::V6(ipv6)) => format!("[{ipv6}]"),
            None => host_display,
        },
        http_endpoint: if http_endpoint.is_empty() || http_endpoint == "/dns-query" {
            None
        } else {
            Some(http_endpoint)
        },
    };
    doh_config_cache()
        .lock()
        .insert((server.to_string(), enable_ipv6), config.clone());
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::BodyExt;
    use std::str::FromStr;
    use std::sync::Mutex as StdMutex;
    use std::{io::{Read, Write}, net::TcpListener, thread};

    fn env_lock() -> &'static StdMutex<()> {
        static LOCK: OnceLock<StdMutex<()>> = OnceLock::new();
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
        assert!(config.all_proxy.is_none());
        assert!(config.http_proxy.is_none());
        assert!(config.https_proxy.is_none());
        assert!(config.dns_servers.is_empty());
        assert!(config.doh_servers.is_empty());
        assert!(config.enable_ipv6);
    }

    #[test]
    fn test_parse_doh_server_caches_results() {
        let server = "https://localhost/dns-query?cache=network-test";
        assert!(doh_config_cache()
            .lock()
            .get(&(server.to_string(), true))
            .is_none());

        let first = parse_doh_server(server, true).unwrap();
        let second = parse_doh_server(server, true).unwrap();

        assert_eq!(first, second);
        assert!(doh_config_cache()
            .lock()
            .get(&(server.to_string(), true))
            .is_some());
    }

    #[test]
    fn test_with_connect_timeout_clones_config() {
        let config = ClientNetworkConfig::default();
        let updated = config.with_connect_timeout(Duration::from_secs(9));

        assert_eq!(config.connect_timeout, Duration::from_secs(30));
        assert_eq!(updated.connect_timeout, Duration::from_secs(9));
        assert!(updated.enable_ipv6);
    }

    #[test]
    fn test_build_name_server_group_adds_udp_and_tcp() {
        let server = SocketAddr::from(([1, 1, 1, 1], 53));
        let group = build_name_server_group(&[server], &[], true).unwrap();

        assert_eq!(group.len(), 2);
        assert!(group
            .iter()
            .any(|config| config.socket_addr == server && config.protocol == Protocol::Udp));
        assert!(group
            .iter()
            .any(|config| config.socket_addr == server && config.protocol == Protocol::Tcp));
    }

    #[test]
    fn test_build_name_server_group_adds_doh_servers() {
        let group = build_name_server_group(&[], &["https://127.0.0.1/dns-query".into()], false)
            .unwrap();

        assert_eq!(group.len(), 1);
        let config = group.iter().next().unwrap();
        assert_eq!(config.socket_addr, SocketAddr::from(([127, 0, 0, 1], 443)));
        assert_eq!(config.protocol, Protocol::Https);
        assert_eq!(config.tls_dns_name.as_deref(), Some("127.0.0.1"));
        assert!(config.http_endpoint.is_none());
    }

    #[test]
    fn test_parse_doh_server_resolves_hostnames_and_custom_paths() {
        let config =
            parse_doh_server("https://localhost/custom-dns?ct=application/dns-message", false)
                .unwrap();

        assert!(!config.socket_addrs.is_empty());
        assert!(config.socket_addrs.iter().all(SocketAddr::is_ipv4));
        assert_eq!(config.tls_dns_name, "localhost");
        assert_eq!(
            config.http_endpoint.as_deref(),
            Some("/custom-dns?ct=application/dns-message")
        );
    }

    #[test]
    fn test_parse_doh_server_rejects_invalid_urls() {
        let err = parse_doh_server("http://dns.google/dns-query", true)
            .unwrap_err()
            .to_string();
        assert!(err.contains("must use https"));

        let err = parse_doh_server("https://user:pass@dns.google/dns-query", true)
            .unwrap_err()
            .to_string();
        assert!(err.contains("cannot include credentials"));
    }

    #[test]
    fn test_parse_doh_server_rejects_empty_or_fragment_urls() {
        let err = parse_doh_server("   ", true).unwrap_err().to_string();
        assert!(err.contains("cannot be empty"));

        let err = parse_doh_server("https://dns.google/dns-query#fragment", true)
            .unwrap_err()
            .to_string();
        assert!(err.contains("cannot include a fragment"));
    }

    #[test]
    fn test_parse_doh_server_rejects_missing_host() {
        let err = parse_doh_server("https:///dns-query", true)
            .unwrap_err()
            .to_string();

        assert!(err.contains("missing a host"));
    }

    #[test]
    fn test_parse_doh_server_rejects_empty_query_and_fragment_authorities() {
        for server in ["https://", "https://?dns=1", "https://#fragment"] {
            let err = parse_doh_server(server, true).unwrap_err().to_string();
            assert!(err.contains("missing a host"), "unexpected error for {server}: {err}");
        }
    }

    #[test]
    fn test_parse_doh_server_rejects_ipv6_only_result_when_ipv6_disabled() {
        let err = parse_doh_server("https://[::1]/dns-query", false)
            .unwrap_err()
            .to_string();

        assert!(err.contains("did not resolve to any IPv4 address"));
    }

    #[test]
    fn test_parse_doh_server_accepts_ipv6_literal_when_enabled() {
        let config = parse_doh_server("https://[::1]/dns-query", true).unwrap();

        assert_eq!(config.socket_addrs, vec![SocketAddr::from(([0, 0, 0, 0, 0, 0, 0, 1], 443))]);
        assert_eq!(config.tls_dns_name, "[::1]");
        assert!(config.http_endpoint.is_none());
    }

    #[test]
    fn test_parse_doh_server_preserves_root_query_endpoint() {
        let config = parse_doh_server("https://127.0.0.1?ct=application/dns-message", true)
            .unwrap();

        assert_eq!(config.socket_addrs, vec![SocketAddr::from(([127, 0, 0, 1], 443))]);
        assert_eq!(config.tls_dns_name, "127.0.0.1");
        assert_eq!(
            config.http_endpoint.as_deref(),
            Some("/?ct=application/dns-message")
        );
    }

    #[test]
    fn test_parse_doh_server_reports_resolution_failures() {
        let err = parse_doh_server("https://resolver-test.invalid/dns-query", true)
            .unwrap_err()
            .to_string();

        assert!(err.contains("failed to resolve DoH host 'resolver-test.invalid'"));
    }

    #[test]
    fn test_load_cached_lookup_returns_none_for_missing_host() {
        let cache = Arc::new(Mutex::new(HashMap::new()));
        assert!(load_cached_lookup(&cache, "missing.example.com").is_none());
    }

    #[test]
    fn test_duration_to_u64_millis_saturates() {
        assert_eq!(duration_to_u64_millis(Duration::MAX), u64::MAX);
    }

    #[test]
    fn test_dns_lookup_cache_returns_fresh_entries() {
        let cache = Arc::new(Mutex::new(HashMap::new()));
        let addrs = vec![SocketAddr::from(([127, 0, 0, 1], 0))];

        store_cached_lookup(
            &cache,
            "example.com".into(),
            addrs.clone(),
            Instant::now() + Duration::from_secs(5),
        );

        let cached = load_cached_lookup(&cache, "example.com").unwrap();
        assert_eq!(cached.addrs, addrs);
    }

    #[test]
    fn test_dns_lookup_cache_evicts_expired_entries() {
        let cache = Arc::new(Mutex::new(HashMap::new()));

        store_cached_lookup(
            &cache,
            "expired.example.com".into(),
            vec![SocketAddr::from(([127, 0, 0, 1], 0))],
            Instant::now() - Duration::from_secs(1),
        );

        assert!(load_cached_lookup(&cache, "expired.example.com").is_none());
        assert!(!cache.lock().contains_key("expired.example.com"));
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
    fn test_build_dns_resolver_supports_system_and_custom_servers() {
        let ipv4_only = ClientNetworkConfig {
            enable_ipv6: false,
            ..ClientNetworkConfig::default()
        };
        drop(ipv4_only.build_dns_resolver().unwrap());

        let custom = BytehaulDnsResolver::new(&[SocketAddr::from(([1, 1, 1, 1], 53))], &[], true)
            .unwrap();
        drop(custom);

        let doh = BytehaulDnsResolver::new(&[], &["https://127.0.0.1/dns-query".into()], false)
            .unwrap();
        drop(doh);
    }

    #[tokio::test]
    async fn test_dns_resolver_resolves_localhost() {
        let mut resolver = BytehaulDnsResolver::new(&[], &[], false).unwrap();
        let addrs: Vec<_> = resolver
            .call(DnsName::from_str("localhost").unwrap())
            .await
            .unwrap()
            .collect();

        assert!(!addrs.is_empty());
        assert!(addrs.iter().all(|addr| addr.port() == 0));
    }

    #[tokio::test]
    async fn test_dns_resolver_returns_cached_lookup_without_querying_dns() {
        let mut resolver = BytehaulDnsResolver::new(&[], &[], false).unwrap();
        let cached_addrs = vec![
            SocketAddr::from(([127, 0, 0, 1], 0)),
            SocketAddr::from(([127, 0, 0, 2], 0)),
        ];
        store_cached_lookup(
            &resolver.cache,
            "cached.example.com".into(),
            cached_addrs.clone(),
            Instant::now() + Duration::from_secs(5),
        );

        let addrs: Vec<_> = resolver
            .call(DnsName::from_str("cached.example.com").unwrap())
            .await
            .unwrap()
            .collect();

        assert_eq!(addrs, cached_addrs);
    }

    #[tokio::test]
    async fn test_dns_resolver_returns_lookup_error_for_invalid_domain() {
        let mut resolver = BytehaulDnsResolver::new(&[], &[], false).unwrap();
        let result = resolver
            .call(DnsName::from_str("coverage-check.invalid").unwrap())
            .await;

        let err = match result {
            Ok(_) => panic!("expected DNS lookup to fail for coverage-check.invalid"),
            Err(error) => error.to_string(),
        };

        assert!(!err.is_empty());
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

        assert_eq!(proxies.http_proxy.unwrap().authority().unwrap().as_str(), "127.0.0.1:8080");
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
    async fn test_warm_resolution_for_url_handles_proxy_invalid_and_hostless_urls() {
        let _guard = env_lock().lock().unwrap();
        clear_proxy_env();

        let proxy_client = ClientNetworkConfig {
            http_proxy: Some("http://127.0.0.1:8080".into()),
            ..ClientNetworkConfig::default()
        }
        .build_client()
        .unwrap();
        proxy_client
            .warm_resolution_for_url("http://example.com/file.bin")
            .await
            .unwrap();

        let direct_client = ClientNetworkConfig::default().build_client().unwrap();
        direct_client.warm_resolution_for_url("not a url").await.unwrap();
        direct_client
            .warm_resolution_for_url("file:///tmp/no-host")
            .await
            .unwrap();
        direct_client
            .warm_resolution_for_url("http://coverage-check.invalid/file.bin")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_proxy_client_request_uses_proxy_branch() {
        let _guard = env_lock().lock().unwrap();
        clear_proxy_env();

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

        let client = ClientNetworkConfig {
            http_proxy: Some(format!("http://{addr}")),
            ..ClientNetworkConfig::default()
        }
        .build_client()
        .unwrap();

        let req = hyper::Request::builder()
            .method("GET")
            .uri("http://example.com/proxy-test")
            .body(HttpRequestBody::new())
            .unwrap();
        let response = client.request(req).await.unwrap();
        assert_eq!(response.status(), hyper::StatusCode::OK);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(&bytes[..], b"pong");

        handle.join().unwrap();
    }

    #[test]
    fn test_parse_doh_server_reports_empty_host_parse_error() {
        let err = parse_doh_server("https://:443/dns-query", true)
            .unwrap_err()
            .to_string();
        assert!(err.contains("missing a host"), "got: {err}");
    }

    #[test]
    fn test_parse_doh_server_reports_generic_parse_error() {
        let err = parse_doh_server("https://[::1", true)
            .unwrap_err()
            .to_string();
        assert!(err.contains("invalid DoH server URL"), "got: {err}");
    }
}
