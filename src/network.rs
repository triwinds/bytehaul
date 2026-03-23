use std::net::SocketAddr;
use std::time::Duration;

use hickory_resolver::config::{
    LookupIpStrategy, NameServerConfig, NameServerConfigGroup, ResolverConfig,
};
use hickory_resolver::proto::xfer::Protocol;
use hickory_resolver::{name_server::TokioConnectionProvider, TokioResolver};
use reqwest::dns::{Addrs, Name, Resolve, Resolving};

use crate::error::DownloadError;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

#[derive(Debug, Clone)]
pub(crate) struct ClientNetworkConfig {
    pub connect_timeout: Duration,
    pub all_proxy: Option<String>,
    pub http_proxy: Option<String>,
    pub https_proxy: Option<String>,
    pub dns_servers: Vec<SocketAddr>,
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
        if self.dns_servers.is_empty() && self.enable_ipv6 {
            return Ok(None);
        }

        BytehaulDnsResolver::new(&self.dns_servers, self.enable_ipv6).map(Some)
    }
}

#[derive(Clone)]
struct BytehaulDnsResolver {
    resolver: TokioResolver,
}

impl BytehaulDnsResolver {
    fn new(dns_servers: &[SocketAddr], enable_ipv6: bool) -> Result<Self, DownloadError> {
        let mut builder = if dns_servers.is_empty() {
            TokioResolver::builder_tokio().map_err(|err| {
                DownloadError::Other(format!("failed to read system DNS configuration: {err}"))
            })?
        } else {
            TokioResolver::builder_with_config(
                ResolverConfig::from_parts(None, vec![], build_name_server_group(dns_servers)),
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
        })
    }
}

impl Resolve for BytehaulDnsResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let resolver = self.resolver.clone();
        let host = name.as_str().to_string();

        Box::pin(async move {
            let lookup = resolver
                .lookup_ip(host.clone())
                .await
                .map_err(|err| -> BoxError { Box::new(err) })?;

            let addrs: Vec<SocketAddr> = lookup.iter().map(|ip| SocketAddr::new(ip, 0)).collect();
            if addrs.is_empty() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("no DNS records found for {host}"),
                )
                .into());
            }

            Ok(Box::new(addrs.into_iter()) as Addrs)
        })
    }
}

fn build_name_server_group(dns_servers: &[SocketAddr]) -> NameServerConfigGroup {
    let mut group = NameServerConfigGroup::new();
    for server in dns_servers {
        for protocol in [Protocol::Udp, Protocol::Tcp] {
            group.push(NameServerConfig::new(*server, protocol));
        }
    }
    group
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
        let group = build_name_server_group(&[server]);

        assert_eq!(group.len(), 2);
        assert!(group
            .iter()
            .any(|cfg| cfg.socket_addr == server && cfg.protocol == Protocol::Udp));
        assert!(group
            .iter()
            .any(|cfg| cfg.socket_addr == server && cfg.protocol == Protocol::Tcp));
    }

    #[test]
    fn test_invalid_proxy_fails_client_build() {
        let mut config = ClientNetworkConfig::default();
        config.all_proxy = Some("not a proxy url".into());

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
        let mut config = ClientNetworkConfig::default();
        config.http_proxy = Some("http://127.0.0.1:8080".into());
        config.https_proxy = Some("http://127.0.0.1:8443".into());
        config.enable_ipv6 = false;

        config.build_client().unwrap();
    }

    #[test]
    fn test_build_dns_resolver_supports_system_and_custom_servers() {
        let mut ipv4_only = ClientNetworkConfig::default();
        ipv4_only.enable_ipv6 = false;
        assert!(ipv4_only.build_dns_resolver().unwrap().is_some());

        let custom =
            BytehaulDnsResolver::new(&[SocketAddr::from(([1, 1, 1, 1], 53))], true).unwrap();
        drop(custom);
    }

    #[tokio::test]
    async fn test_dns_resolver_resolves_localhost() {
        let resolver = BytehaulDnsResolver::new(&[], false).unwrap();
        let name = Name::from_str("localhost").unwrap();
        let addrs: Vec<_> = resolver.resolve(name).await.unwrap().collect();

        assert!(!addrs.is_empty());
        assert!(addrs.iter().all(|addr| addr.port() == 0));
    }
}
