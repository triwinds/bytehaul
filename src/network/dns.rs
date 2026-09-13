//! Shared Hickory-backed name resolution (P3 of the libcurl migration plan).
//!
//! The libcurl transport injects these answers with `CURLOPT_RESOLVE`. Keeping
//! resolution here makes custom name servers, DoH endpoints, the IPv6 switch
//! and the TTL cache explicit and testable before each transfer.
//!
//! The resolver returns *addresses plus a validity deadline*: the deadline is
//! what lets the libcurl transport refresh an injected `CURLOPT_RESOLVE` entry
//! instead of turning a per-request lookup into a permanent DNS override.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use hickory_resolver::config::{
    LookupIpStrategy, NameServerConfig, NameServerConfigGroup, ResolverConfig,
};
use hickory_resolver::proto::xfer::Protocol;
use hickory_resolver::{name_server::TokioConnectionProvider, TokioResolver};
use parking_lot::Mutex;
use url::Url;

use crate::error::{BoxError, DownloadError};

/// One lookup answer: the addresses to use and the deadline of that answer.
///
/// Hickory owns the bounded TTL cache these answers come from, so the deadline
/// is the authoritative lifetime: a caller that keeps the addresses longer has
/// to re-resolve. The libcurl transport uses this deadline when refreshing its
/// `CURLOPT_RESOLVE` entries.
#[derive(Clone, Debug)]
pub(crate) struct DnsAnswer {
    addresses: Vec<IpAddr>,
    valid_until: Instant,
}

impl DnsAnswer {
    pub(crate) fn addresses(&self) -> &[IpAddr] {
        &self.addresses
    }

    /// Time left before this answer must not be used any more.
    pub(crate) fn time_to_live(&self) -> Duration {
        self.valid_until.saturating_duration_since(Instant::now())
    }
}

/// Hickory-backed resolver used by the libcurl transport.
#[derive(Clone)]
pub(crate) struct BytehaulDnsResolver {
    resolver: TokioResolver,
}

impl BytehaulDnsResolver {
    pub(crate) fn new(
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
        })
    }

    /// Resolves `host` to the addresses of this hop.
    pub(crate) async fn resolve(&self, host: &str) -> Result<DnsAnswer, BoxError> {
        // Hickory owns the bounded TTL cache shared by resolver clones.
        let lookup = self
            .resolver
            .lookup_ip(host.to_string())
            .await
            .map_err(|error| {
                #[cfg(not(tarpaulin))]
                tracing::debug!(host = %host, error = %error, "DNS lookup failed");
                let boxed: BoxError = Box::new(error);
                boxed
            })?;

        let valid_until = lookup.valid_until();
        let addresses: Vec<IpAddr> = lookup.iter().collect();
        if addresses.is_empty() {
            #[cfg(not(tarpaulin))]
            tracing::debug!(host = %host, "DNS lookup returned no IP addresses");
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("no DNS records found for {host}"),
            )
            .into());
        }

        #[cfg(not(tarpaulin))]
        tracing::debug!(
            host = %host,
            addrs = ?addresses,
            ttl_remaining_ms = duration_to_u64_millis(
                valid_until.saturating_duration_since(Instant::now())
            ),
            "resolved host via DNS lookup"
        );

        Ok(DnsAnswer {
            addresses,
            valid_until,
        })
    }
}

pub(crate) fn duration_to_u64_millis(duration: Duration) -> u64 {
    duration.as_millis().min(u64::MAX as u128) as u64
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

    let config = parse_doh_server_uncached(server, enable_ipv6, resolve_doh_host)?;
    doh_config_cache()
        .lock()
        .insert((server.to_string(), enable_ipv6), config.clone());
    Ok(config)
}

fn parse_doh_server_uncached<F>(
    server: &str,
    enable_ipv6: bool,
    resolve_host: F,
) -> Result<DohServerConfig, DownloadError>
where
    F: FnOnce(&str, u16) -> std::io::Result<Vec<SocketAddr>>,
{
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
        DownloadError::InvalidConfig(format!("DoH server URL '{server}' is missing a valid port"))
    })?;

    let host_for_resolution = host_display
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
        .unwrap_or(host_display.as_str());
    let parsed_ip = host_for_resolution.parse::<IpAddr>().ok();

    let mut socket_addrs: Vec<SocketAddr> = if let Some(ip) = parsed_ip {
        vec![SocketAddr::new(ip, port)]
    } else {
        resolve_host(host_for_resolution, port).map_err(|err| {
            DownloadError::InvalidConfig(format!(
                "failed to resolve DoH host '{host_display}' from '{server}': {err}"
            ))
        })?
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
    })
}

fn resolve_doh_host(host: &str, port: u16) -> std::io::Result<Vec<SocketAddr>> {
    (host, port).to_socket_addrs().map(|addrs| addrs.collect())
}

/// A scripted UDP DNS server used by the libcurl resolver tests.
///
/// It answers any name with `127.0.0.<query number>`, so the first lookup of a
/// fresh resolver yields `127.0.0.1`.
#[cfg(test)]
pub(crate) async fn spawn_dns_test_server(
    ttl: u32,
) -> (
    SocketAddr,
    std::sync::Arc<std::sync::atomic::AtomicUsize>,
    tokio::task::JoinHandle<()>,
) {
    spawn_dns_test_server_with(ttl, std::time::Duration::ZERO, true).await
}

/// Same as [`spawn_dns_test_server`], but every answer is delayed.
///
/// A slow resolver is what a connect-phase budget has to survive, so this
/// variant always answers `127.0.0.1`: a test measuring the connect phase has
/// to know the address it will end up connecting to.
#[cfg(test)]
pub(crate) async fn spawn_dns_test_server_with_delay(
    ttl: u32,
    delay: std::time::Duration,
) -> (
    SocketAddr,
    std::sync::Arc<std::sync::atomic::AtomicUsize>,
    tokio::task::JoinHandle<()>,
) {
    spawn_dns_test_server_with(ttl, delay, false).await
}

/// Scripted UDP DNS server: answers `127.0.0.<query number>` when
/// `sequential_addresses` is set (consecutive lookups resolve differently) and
/// `127.0.0.1` otherwise.
#[cfg(test)]
async fn spawn_dns_test_server_with(
    ttl: u32,
    delay: std::time::Duration,
    sequential_addresses: bool,
) -> (
    SocketAddr,
    std::sync::Arc<std::sync::atomic::AtomicUsize>,
    tokio::task::JoinHandle<()>,
) {
    use hickory_resolver::proto::{
        op::{Message, MessageType},
        rr::{rdata::A, RData, Record, RecordType},
    };
    use std::sync::atomic::{AtomicUsize, Ordering};

    let socket = tokio::net::UdpSocket::bind(("127.0.0.1", 0)).await.unwrap();
    let addr = socket.local_addr().unwrap();
    let queries = std::sync::Arc::new(AtomicUsize::new(0));
    let server_queries = queries.clone();
    let server = tokio::spawn(async move {
        let mut buf = [0; 4096];
        loop {
            let (len, peer) = match socket.recv_from(&mut buf).await {
                Ok(value) => value,
                // A client that stopped waiting makes its socket unreachable,
                // which Windows reports on this socket as an error
                // (WSAECONNRESET). Keep serving the next query.
                Err(_) => continue,
            };
            let request = Message::from_vec(&buf[..len]).unwrap();
            let query = request.queries()[0].clone();
            // IPv4-only configuration must never ask for an AAAA record.
            assert_eq!(query.query_type(), RecordType::A);
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            let count = server_queries.fetch_add(1, Ordering::SeqCst) + 1;
            let address = if sequential_addresses {
                A::new(127, 0, 0, count as u8)
            } else {
                A::new(127, 0, 0, 1)
            };
            let mut response = Message::new();
            response
                .set_id(request.id())
                .set_message_type(MessageType::Response)
                .set_recursion_desired(true)
                .set_recursion_available(true)
                .add_query(query.clone())
                .add_answer(Record::from_rdata(
                    query.name().clone(),
                    ttl,
                    RData::A(address),
                ));
            // Answering a client that already gave up is not a server error.
            let _ = socket.send_to(&response.to_vec().unwrap(), peer).await;
        }
    });
    (addr, queries, server)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let group =
            build_name_server_group(&[], &["https://127.0.0.1/dns-query".into()], false).unwrap();

        assert_eq!(group.len(), 1);
        let config = group.iter().next().unwrap();
        assert_eq!(config.socket_addr, SocketAddr::from(([127, 0, 0, 1], 443)));
        assert_eq!(config.protocol, Protocol::Https);
        assert_eq!(config.tls_dns_name.as_deref(), Some("127.0.0.1"));
        assert!(config.http_endpoint.is_none());
    }

    #[test]
    fn test_parse_doh_server_resolves_hostnames_and_custom_paths() {
        let config = parse_doh_server(
            "https://localhost/custom-dns?ct=application/dns-message",
            false,
        )
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
            assert!(
                err.contains("missing a host"),
                "unexpected error for {server}: {err}"
            );
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

        assert_eq!(
            config.socket_addrs,
            vec![SocketAddr::from(([0, 0, 0, 0, 0, 0, 0, 1], 443))]
        );
        assert_eq!(config.tls_dns_name, "[::1]");
        assert!(config.http_endpoint.is_none());
    }

    #[test]
    fn test_parse_doh_server_preserves_root_query_endpoint() {
        let config =
            parse_doh_server("https://127.0.0.1?ct=application/dns-message", true).unwrap();

        assert_eq!(
            config.socket_addrs,
            vec![SocketAddr::from(([127, 0, 0, 1], 443))]
        );
        assert_eq!(config.tls_dns_name, "127.0.0.1");
        assert_eq!(
            config.http_endpoint.as_deref(),
            Some("/?ct=application/dns-message")
        );
    }

    #[test]
    fn test_parse_doh_server_reports_resolution_failures() {
        let err =
            parse_doh_server_uncached("https://resolver-test.invalid/dns-query", true, |_, _| {
                Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "simulated resolution failure",
                ))
            })
            .unwrap_err()
            .to_string();

        assert!(err.contains("failed to resolve DoH host 'resolver-test.invalid'"));
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

    #[test]
    fn test_duration_to_u64_millis_saturates() {
        assert_eq!(duration_to_u64_millis(Duration::MAX), u64::MAX);
    }

    #[test]
    fn test_build_dns_resolver_supports_system_and_custom_servers() {
        let ipv4_only = BytehaulDnsResolver::new(&[], &[], false).unwrap();
        drop(ipv4_only);

        let custom =
            BytehaulDnsResolver::new(&[SocketAddr::from(([1, 1, 1, 1], 53))], &[], true).unwrap();
        drop(custom);

        let doh =
            BytehaulDnsResolver::new(&[], &["https://127.0.0.1/dns-query".into()], false).unwrap();
        drop(doh);
    }

    #[tokio::test]
    async fn test_dns_resolver_resolves_localhost() {
        let resolver = BytehaulDnsResolver::new(&[], &[], false).unwrap();
        let addrs = resolver.resolve("localhost").await.unwrap();

        assert!(!addrs.addresses().is_empty());
    }

    #[tokio::test]
    async fn test_resolve_returns_a_ttl_bounded_answer() {
        let (addr, queries, server) = spawn_dns_test_server(1).await;
        let resolver = BytehaulDnsResolver::new(&[addr], &[], false).unwrap();

        let first = resolver.resolve("cache-test.example.").await.unwrap();
        assert_eq!(first.addresses(), [IpAddr::from([127, 0, 0, 1])]);
        // The TTL of the scripted answer bounds how long it may be reused.
        assert!(first.time_to_live() <= Duration::from_secs(1));

        let cached = resolver.resolve("cache-test.example.").await.unwrap();
        assert_eq!(cached.addresses(), first.addresses());
        assert_eq!(
            queries.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the shared Hickory cache must answer the second lookup"
        );
        server.abort();
    }

    #[tokio::test]
    async fn test_dns_resolver_shares_hickory_cache_and_refreshes_expired_answers() {
        use std::sync::atomic::Ordering;

        let (addr, queries, server) = spawn_dns_test_server(1).await;
        let resolver = BytehaulDnsResolver::new(&[addr], &[], false).unwrap();
        let name = "cache-test.example.";
        let first = resolver.resolve(name).await.unwrap();
        assert_eq!(first.addresses(), [IpAddr::from([127, 0, 0, 1])]);

        let cached = resolver.clone().resolve(name).await.unwrap();
        assert_eq!(cached.addresses(), first.addresses());
        assert_eq!(queries.load(Ordering::SeqCst), 1);

        // Hickory uses std::time::Instant for expiry, so advancing Tokio's
        // paused clock would not exercise the real TTL contract.
        tokio::time::sleep(Duration::from_millis(1100)).await;
        let refreshed = resolver.resolve(name).await.unwrap();
        assert_eq!(refreshed.addresses(), [IpAddr::from([127, 0, 0, 2])]);
        assert_eq!(queries.load(Ordering::SeqCst), 2);
        server.abort();
    }

    #[tokio::test]
    async fn test_dns_resolver_returns_lookup_error_for_invalid_domain() {
        let resolver = BytehaulDnsResolver::new(&[], &[], false).unwrap();
        let result = resolver.resolve("coverage-check.invalid").await;

        let err = match result {
            Ok(_) => panic!("expected DNS lookup to fail for coverage-check.invalid"),
            Err(error) => error.to_string(),
        };

        assert!(!err.is_empty());
    }
}
