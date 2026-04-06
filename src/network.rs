use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::sync::Arc;
use std::time::{Duration, Instant};

use hickory_resolver::config::{
    LookupIpStrategy, NameServerConfig, NameServerConfigGroup, ResolverConfig,
};
use hickory_resolver::proto::xfer::Protocol;
use hickory_resolver::{name_server::TokioConnectionProvider, TokioResolver};
use parking_lot::Mutex;
use reqwest::dns::{Addrs, Name, Resolve, Resolving};

use crate::error::DownloadError;

type BoxError = Box<dyn std::error::Error + Send + Sync>;
type SharedDnsCache = Arc<Mutex<HashMap<String, CachedDnsLookup>>>;

#[derive(Debug, Clone)]
pub(crate) struct ClientNetworkConfig {
    pub connect_timeout: Duration,
    pub all_proxy: Option<String>,
    pub http_proxy: Option<String>,
    pub https_proxy: Option<String>,
    pub dns_servers: Vec<SocketAddr>,
    pub doh_servers: Vec<String>,
    pub enable_ipv6: bool,
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
    pub(crate) fn build_client(&self) -> Result<reqwest::Client, DownloadError> {
        #[cfg(not(tarpaulin))]
        tracing::debug!(connect_timeout_ms = self.connect_timeout.as_millis() as u64,
            has_proxy = self.all_proxy.is_some() || self.http_proxy.is_some() || self.https_proxy.is_some(),
            custom_dns = !self.dns_servers.is_empty(),
            custom_doh = !self.doh_servers.is_empty(),
            enable_ipv6 = self.enable_ipv6,
            "building HTTP client");
        let mut builder = reqwest::Client::builder().connect_timeout(self.connect_timeout);

        // Add scheme-specific proxies first so they win over a later catch-all proxy.
        if let Some(proxy) = &self.http_proxy {
            builder = builder.proxy(reqwest::Proxy::http(proxy)?);
        }
        if let Some(proxy) = &self.https_proxy {
            builder = builder.proxy(reqwest::Proxy::https(proxy)?);
        }
        if let Some(proxy) = &self.all_proxy {
            builder = builder.proxy(reqwest::Proxy::all(proxy)?);
        }

        if let Some(resolver) = self.build_dns_resolver()? {
            builder = builder.dns_resolver2(resolver);
        }

        builder.build().map_err(Into::into)
    }

    pub(crate) fn with_connect_timeout(&self, connect_timeout: Duration) -> Self {
        let mut updated = self.clone();
        updated.connect_timeout = connect_timeout;
        updated
    }

    fn build_dns_resolver(&self) -> Result<Option<BytehaulDnsResolver>, DownloadError> {
        if self.dns_servers.is_empty() && self.doh_servers.is_empty() && self.enable_ipv6 {
            return Ok(None);
        }

        BytehaulDnsResolver::new(&self.dns_servers, &self.doh_servers, self.enable_ipv6)
            .map(Some)
    }
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
    // Mirror successful lookups so cache hits are visible in bytehaul's debug logs.
    cache: SharedDnsCache,
}

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
}

impl Resolve for BytehaulDnsResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let resolver = self.resolver.clone();
        let cache = self.cache.clone();
        let host = name.as_str().to_string();

        Box::pin(async move {
            if let Some(cached) = load_cached_lookup(&cache, &host) {
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

                return Ok(Box::new(cached.addrs.into_iter()) as Addrs);
            }

            let lookup = resolver
                .lookup_ip(host.clone())
                .await
                .map_err(|err| {
                    #[cfg(not(tarpaulin))]
                    tracing::debug!(host = %host, cache_hit = false, error = %err, "DNS lookup failed");
                    let boxed: BoxError = Box::new(err);
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

            store_cached_lookup(&cache, host.clone(), addrs.clone(), valid_until);

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

            Ok(Box::new(addrs.into_iter()) as Addrs)
        })
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

    let url = reqwest::Url::parse(server).map_err(|err| {
        DownloadError::InvalidConfig(format!("invalid DoH server URL '{server}': {err}"))
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

    let host = url.host_str().ok_or_else(|| {
        DownloadError::InvalidConfig(format!("DoH server URL '{server}' is missing a host"))
    })?;
    let port = url.port_or_known_default().ok_or_else(|| {
        DownloadError::InvalidConfig(format!(
            "DoH server URL '{server}' is missing a valid port"
        ))
    })?;

    let mut socket_addrs: Vec<SocketAddr> = if let Ok(ip) = host.parse::<IpAddr>() {
        vec![SocketAddr::new(ip, port)]
    } else {
        (host, port)
            .to_socket_addrs()
            .map_err(|err| {
                DownloadError::InvalidConfig(format!(
                    "failed to resolve DoH host '{host}' from '{server}': {err}"
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

    Ok(DohServerConfig {
        socket_addrs,
        tls_dns_name: host.to_string(),
        http_endpoint: if http_endpoint.is_empty() || http_endpoint == "/dns-query" {
            None
        } else {
            Some(http_endpoint)
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

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
            .any(|cfg| cfg.socket_addr == server && cfg.protocol == Protocol::Udp));
        assert!(group
            .iter()
            .any(|cfg| cfg.socket_addr == server && cfg.protocol == Protocol::Tcp));
    }

    #[test]
    fn test_build_name_server_group_adds_doh_servers() {
        let group = build_name_server_group(&[], &["https://127.0.0.1/dns-query".into()], false)
            .unwrap();

        assert_eq!(group.len(), 1);
        let cfg = group.iter().next().unwrap();
        assert_eq!(cfg.socket_addr, SocketAddr::from(([127, 0, 0, 1], 443)));
        assert_eq!(cfg.protocol, Protocol::Https);
        assert_eq!(cfg.tls_dns_name.as_deref(), Some("127.0.0.1"));
        assert!(cfg.http_endpoint.is_none());
    }

    #[test]
    fn test_parse_doh_server_resolves_hostnames_and_custom_paths() {
        let config = parse_doh_server("https://localhost/custom-dns?ct=application/dns-message", false)
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

        let err = config.build_client().unwrap_err().to_string();
        assert!(err.contains("builder") || err.contains("URL"));
    }

    #[test]
    fn test_build_dns_resolver_short_circuits_for_default_behavior() {
        let config = ClientNetworkConfig::default();
        assert!(config.build_dns_resolver().unwrap().is_none());
    }

    #[test]
    fn test_build_client_accepts_http_and_https_proxies() {
        let config = ClientNetworkConfig {
            http_proxy: Some("http://127.0.0.1:8080".into()),
            https_proxy: Some("http://127.0.0.1:8443".into()),
            enable_ipv6: false,
            ..ClientNetworkConfig::default()
        };

        config.build_client().unwrap();
    }

    #[test]
    fn test_build_dns_resolver_supports_system_and_custom_servers() {
        let ipv4_only = ClientNetworkConfig {
            enable_ipv6: false,
            ..ClientNetworkConfig::default()
        };
        assert!(ipv4_only.build_dns_resolver().unwrap().is_some());

        let custom =
            BytehaulDnsResolver::new(&[SocketAddr::from(([1, 1, 1, 1], 53))], &[], true)
                .unwrap();
        drop(custom);

        let doh = BytehaulDnsResolver::new(&[], &["https://127.0.0.1/dns-query".into()], false)
            .unwrap();
        drop(doh);
    }

    #[tokio::test]
    async fn test_dns_resolver_resolves_localhost() {
        let resolver = BytehaulDnsResolver::new(&[], &[], false).unwrap();
        let name = Name::from_str("localhost").unwrap();
        let addrs: Vec<_> = resolver.resolve(name).await.unwrap().collect();

        assert!(!addrs.is_empty());
        assert!(addrs.iter().all(|addr| addr.port() == 0));
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
}
