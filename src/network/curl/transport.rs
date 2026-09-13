//! Tokio-facing libcurl transport (P2 driver, P3 compatibility work).
//!
//! Converts a neutral `http::Request` into one driver transfer and wraps the
//! response head plus its body stream back into an `http::Response`. Everything
//! libcurl-specific (handles, callbacks, the transfer thread) stays inside
//! `driver`; the session layer keeps seeing `HttpResponse`.
//!
//! P3 adds the routing half of the plan (§4.3):
//!
//! * the shared Hickory resolver runs on the Tokio side and its answers reach
//!   libcurl through `CURLOPT_RESOLVE`, so custom name servers, DoH endpoints
//!   and the IPv6 switch apply to the libcurl backend too;
//! * a proxied request is *not* resolved locally: the proxy resolves the origin
//!   hostname, which is what "proxy resolves the target" means in practice. The
//!   proxy endpoint itself is resolved with the same resolver;
//! * the proxy route is applied with an explicit `CURLOPT_PROXY` and an
//!   explicit bypass list, so libcurl never re-reads `HTTP_PROXY`/`NO_PROXY`
//!   from the environment behind the caller's back.

use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::error::{DownloadError, TransportError, TransportErrorKind};
use crate::http::{
    BodyBudget, HttpBody, HttpRequestBody, HttpResponse, MAX_BODY_BUDGET_BYTES,
    MIN_BODY_BUDGET_BYTES,
};
use crate::network::dns::BytehaulDnsResolver;
use crate::network::ClientNetworkConfig;

use super::driver::{
    DriverConfig, DriverHandle, DriverStats, RequestOptions, ResolveEntry, Transfer,
};

/// Head deadline used by the test-only deadline-less request helper, so it
/// cannot wait forever on a silent server.
///
/// Production callers always pass their configured
/// `request_headers_timeout` (falling back to the body `read_timeout`) through
/// `request_with_timeout`; this constant must never shorten that deadline.
#[cfg(test)]
const HEAD_DEADLINE_BACKSTOP: Duration = Duration::from_secs(300);

/// One proxy endpoint, with the host it has to resolve.
#[derive(Clone, Debug, PartialEq, Eq)]
struct ProxyEndpoint {
    /// URL handed to `CURLOPT_PROXY`, credentials included.
    url: String,
    /// Host to resolve for the proxy connection; `None` for an IP literal.
    host: Option<String>,
    port: u16,
}

impl ProxyEndpoint {
    fn parse(url: &str) -> Result<Self, DownloadError> {
        let parsed = url::Url::parse(url).map_err(|error| {
            DownloadError::InvalidConfig(format!("invalid proxy URL '{url}': {error}"))
        })?;
        let port = parsed.port_or_known_default().ok_or_else(|| {
            DownloadError::InvalidConfig(format!("proxy URL '{url}' has no usable port"))
        })?;
        let host = parsed.host_str().ok_or_else(|| {
            DownloadError::InvalidConfig(format!("proxy URL '{url}' has no host"))
        })?;
        let literal = host
            .strip_prefix('[')
            .and_then(|value| value.strip_suffix(']'))
            .unwrap_or(host)
            .parse::<IpAddr>()
            .is_ok();
        Ok(Self {
            url: url.to_string(),
            host: (!literal).then(|| host.to_string()),
            port,
        })
    }
}

/// One libcurl driver shared by every request of a cached client.
pub(crate) struct CurlTransport {
    driver: DriverHandle,
    resolver: Arc<BytehaulDnsResolver>,
    connect_timeout: Duration,
    forbid_connection_reuse: bool,
    http_proxy: Option<ProxyEndpoint>,
    https_proxy: Option<ProxyEndpoint>,
    all_proxy: Option<ProxyEndpoint>,
    ca_info: Option<std::path::PathBuf>,
    ca_path: Option<std::path::PathBuf>,
    client_cert: Option<std::path::PathBuf>,
    client_key: Option<std::path::PathBuf>,
}

impl CurlTransport {
    /// Counters reported by this client's driver thread.
    pub(crate) fn driver_stats(&self) -> DriverStats {
        self.driver.stats()
    }

    pub(crate) fn new(config: &ClientNetworkConfig) -> Result<Self, DownloadError> {
        let proxies = config.effective_proxies()?;
        // Use the shared resolver so a custom name server, a DoH endpoint or
        // `enable_ipv6` has one consistent configuration boundary.
        let resolver = Arc::new(config.build_dns_resolver()?);
        Ok(Self {
            driver: DriverHandle::spawn(DriverConfig {
                max_idle_per_host: config.pool_max_idle_per_host,
                pool_idle_timeout: config.pool_idle_timeout,
                max_age_conn: Some(config.pool_idle_timeout),
            }),
            resolver,
            connect_timeout: config.connect_timeout,
            // `pool_max_idle_per_host = 0` keeps the old "no connection reuse"
            // contract; the pool accounting implements the `k > 0` case.
            forbid_connection_reuse: config.pool_max_idle_per_host == 0,
            http_proxy: proxy_endpoint(proxies.http_proxy)?,
            https_proxy: proxy_endpoint(proxies.https_proxy)?,
            all_proxy: proxy_endpoint(proxies.all_proxy)?,
            ca_info: config.ca_info.clone(),
            ca_path: config.ca_path.clone(),
            client_cert: config.client_cert.clone(),
            client_key: config.client_key.clone(),
        })
    }

    /// Start one transfer; resolves when the response head is complete.
    ///
    /// The connect phase - the shared Hickory lookup plus the connection
    /// libcurl then establishes - draws on a single budget:
    /// `min(connect_timeout, head_deadline)`. Spending it on the lookup leaves
    /// libcurl only what is left of it, so a slow or hanging resolver cannot
    /// hand the driver a second, full connect timeout. `head_deadline` is still
    /// the caller's own deadline for the first response head, which is
    /// enforced by the driver.
    pub(crate) async fn request(
        &self,
        req: http::Request<HttpRequestBody>,
        head_deadline: Duration,
    ) -> Result<HttpResponse, DownloadError> {
        let started = Instant::now();
        let mut options = self.options_for(&req, head_deadline)?;
        let connect_budget = self.connect_timeout.min(head_deadline);

        options.resolve = self
            .resolve_for(&options, connect_budget.saturating_sub(started.elapsed()))
            .await?;

        let spent = started.elapsed();
        let remaining_connect = connect_budget.saturating_sub(spent);
        if remaining_connect.is_zero() {
            // The lookup used the whole connect phase; libcurl must not start a
            // connection the caller has already stopped waiting for.
            return Err(DownloadError::timeout("request timed out"));
        }
        options.connect_timeout = remaining_connect;

        let remaining_head = head_deadline.saturating_sub(spent);
        if remaining_head.is_zero() {
            return Err(DownloadError::timeout("request timed out"));
        }
        options.head_timeout = remaining_head;

        let budget = body_budget(&req);
        let transfer = self.driver.get(options, budget).await?;
        response_from_transfer(transfer)
    }

    /// Start one transfer when the caller has no deadline of its own.
    #[cfg(test)]
    pub(crate) async fn request_with_backstop(
        &self,
        req: http::Request<HttpRequestBody>,
    ) -> Result<HttpResponse, DownloadError> {
        self.request(req, HEAD_DEADLINE_BACKSTOP).await
    }

    fn options_for(
        &self,
        req: &http::Request<HttpRequestBody>,
        head_timeout: Duration,
    ) -> Result<RequestOptions, DownloadError> {
        let uri = req.uri();
        let scheme = uri.scheme_str().unwrap_or_default();
        if !matches!(scheme, "http" | "https") {
            return Err(DownloadError::InvalidConfig(format!(
                "libcurl transport requires an http or https URL, got '{uri}'"
            )));
        }

        let mut options = RequestOptions::new(uri.to_string());
        options.connect_timeout = self.connect_timeout;
        options.head_timeout = head_timeout;
        options.forbid_connection_reuse = self.forbid_connection_reuse;
        options.proxy = self.proxy_for(scheme).map(|endpoint| endpoint.url.clone());
        options.ca_info = self.ca_info.clone();
        options.ca_path = self.ca_path.clone();
        options.client_cert = self.client_cert.clone();
        options.client_key = self.client_key.clone();

        for (name, value) in req.headers() {
            let name = name.as_str();
            let value = value.to_str().map_err(|_| {
                DownloadError::InvalidConfig(format!("header '{name}' has a non-text value"))
            })?;
            if name.eq_ignore_ascii_case("range") {
                match byte_range_spec(value) {
                    // `CURLOPT_RANGE` takes `start-end[,...]` and adds the
                    // `bytes=` unit itself; passing the header verbatim would
                    // send `Range: bytes=bytes=0-9`.
                    Some(spec) => options.range = Some(spec.to_string()),
                    // A non-byte range unit is forwarded unchanged: bytehaul
                    // never asks for one, and rewriting it would be a silent
                    // request change.
                    None => options.headers.push((name.to_string(), value.to_string())),
                }
                continue;
            }
            if name.eq_ignore_ascii_case("accept-encoding") {
                // The driver always sends `identity`; a duplicate header would
                // be a hidden request change.
                continue;
            }
            options.headers.push((name.to_string(), value.to_string()));
        }

        Ok(options)
    }

    fn proxy_for(&self, scheme: &str) -> Option<&ProxyEndpoint> {
        match scheme {
            "https" => self.https_proxy.as_ref().or(self.all_proxy.as_ref()),
            _ => self.http_proxy.as_ref().or(self.all_proxy.as_ref()),
        }
    }

    /// Resolves the hop libcurl is about to connect to.
    ///
    /// With a proxy, that hop is the proxy: the origin hostname travels inside
    /// the request (absolute form) or the `CONNECT` line, so the proxy resolves
    /// it. Without a proxy the origin hostname is resolved here and injected
    /// with `CURLOPT_RESOLVE`, which keeps the URL (and therefore `Host`, SNI
    /// and certificate verification) unchanged.
    ///
    /// `budget` is what is left of the connect phase; a lookup that needs
    /// longer fails here instead of letting the connection start late.
    async fn resolve_for(
        &self,
        options: &RequestOptions,
        budget: Duration,
    ) -> Result<Option<ResolveEntry>, DownloadError> {
        let target = match self.proxy_for(&scheme_of(&options.url)?) {
            Some(proxy) => proxy.host.clone().map(|host| (host, proxy.port)),
            None => origin_target(&options.url)?,
        };
        let Some((host, port)) = target else {
            // An IP literal needs no injection: libcurl connects to it as-is.
            return Ok(None);
        };

        let lookup = self.resolver.resolve(&host);
        let answer = tokio::time::timeout(budget, lookup).await.map_err(|_| {
            // The connect phase covers name resolution: a hanging lookup spends
            // the same budget the connection would have used.
            DownloadError::timeout("request timed out")
        })?;
        let answer = answer.map_err(|error| {
            DownloadError::Transport(TransportError::new(
                TransportErrorKind::Connect,
                std::io::Error::other(format!("DNS lookup for '{host}' failed: {error}")),
            ))
        })?;

        Ok(Some(ResolveEntry::new(
            &host,
            port,
            answer.addresses(),
            answer.time_to_live(),
        )))
    }
}

impl Drop for CurlTransport {
    /// Reports the driver counters once the last reference to this client is
    /// released, which is also when the driver thread stops.
    fn drop(&mut self) {
        let stats = self.driver.stats();
        tracing::debug!(
            submitted = stats.submitted,
            completed = stats.completed,
            cancelled = stats.cancelled,
            connections = stats.connections,
            pools = stats.pools,
            commands = stats.commands,
            pauses = stats.pauses,
            resumes = stats.resumes,
            idle_clears = stats.idle_clears,
            max_command_latency_ms = stats.max_command_latency.as_millis() as u64,
            "releasing the libcurl transport"
        );
    }
}

fn proxy_endpoint(uri: Option<http::Uri>) -> Result<Option<ProxyEndpoint>, DownloadError> {
    uri.map(|uri| ProxyEndpoint::parse(&uri.to_string()))
        .transpose()
}

fn scheme_of(url: &str) -> Result<String, DownloadError> {
    url::Url::parse(url)
        .map(|parsed| parsed.scheme().to_string())
        .map_err(|error| {
            DownloadError::InvalidConfig(format!("invalid request URL '{url}': {error}"))
        })
}

/// The `host:port` an unproxied request connects to; `None` for IP literals.
fn origin_target(url: &str) -> Result<Option<(String, u16)>, DownloadError> {
    let parsed = url::Url::parse(url).map_err(|error| {
        DownloadError::InvalidConfig(format!("invalid request URL '{url}': {error}"))
    })?;
    let Some(host) = parsed.host_str() else {
        return Ok(None);
    };
    let literal = host
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
        .unwrap_or(host)
        .parse::<IpAddr>()
        .is_ok();
    if literal {
        return Ok(None);
    }
    let port = parsed.port_or_known_default().ok_or_else(|| {
        DownloadError::InvalidConfig(format!("request URL '{url}' has no usable port"))
    })?;
    Ok(Some((host.to_string(), port)))
}

/// Per-transfer body budget: the session's request hint, clamped to the range
/// the transport can honour, or the transport default when there is no hint.
fn body_budget(req: &http::Request<HttpRequestBody>) -> usize {
    req.extensions()
        .get::<BodyBudget>()
        .map(|budget| budget.0)
        .unwrap_or(MAX_BODY_BUDGET_BYTES)
        .clamp(MIN_BODY_BUDGET_BYTES, MAX_BODY_BUDGET_BYTES)
}

/// Extracts the `start-end[,...]` spec of a `bytes=` range header.
///
/// Returns `None` for any other range unit, so the caller can forward the
/// header unchanged instead of guessing what the server should receive.
fn byte_range_spec(value: &str) -> Option<&str> {
    let (unit, spec) = value.split_once('=')?;
    if unit.trim().eq_ignore_ascii_case("bytes") {
        Some(spec.trim())
    } else {
        None
    }
}

fn response_from_transfer(transfer: Transfer) -> Result<HttpResponse, DownloadError> {
    let Transfer { head, body } = transfer;
    let status = http::StatusCode::from_u16(head.status).map_err(|_| {
        DownloadError::Internal(format!(
            "libcurl reported an invalid status code {}",
            head.status
        ))
    })?;

    let mut response = http::Response::new(HttpBody::Curl(body));
    *response.status_mut() = status;
    *response.version_mut() = http::Version::HTTP_11;
    let headers = response.headers_mut();
    for (name, value) in &head.headers {
        let (Ok(name), Ok(value)) = (
            http::header::HeaderName::from_bytes(name.as_bytes()),
            http::header::HeaderValue::from_str(value),
        ) else {
            // A header libcurl accepted but `http` cannot represent is dropped
            // rather than failing the whole download; the body is unaffected.
            continue;
        };
        headers.append(name, value);
    }

    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::net::SocketAddr;

    fn request(url: &str, headers: &[(&str, &str)]) -> http::Request<HttpRequestBody> {
        let mut builder = http::Request::builder().method("GET").uri(url);
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        builder.body(HttpRequestBody::new()).unwrap()
    }

    fn transport(config: &ClientNetworkConfig) -> CurlTransport {
        CurlTransport::new(config).expect("libcurl transport")
    }

    #[test]
    fn a_long_caller_deadline_is_not_clamped_by_an_internal_default() {
        // A configured `request_headers_timeout` above any internal default must
        // reach the driver unchanged.
        let transport = transport(&ClientNetworkConfig::default());
        let req = request("https://example.com/file.bin", &[]);
        let deadline = Duration::from_secs(120);

        assert_eq!(
            transport.options_for(&req, deadline).unwrap().head_timeout,
            deadline
        );
    }

    #[tokio::test]
    async fn the_caller_deadline_bounds_the_wait_for_response_headers() {
        use tokio::net::TcpListener;

        // A server that accepts and then never answers: only the deadline can
        // end the transfer.
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (_stream, _) = listener.accept().await.unwrap();
            std::future::pending::<()>().await;
        });

        let transport = transport(&ClientNetworkConfig::default());
        let req = request(&format!("http://{address}/file"), &[]);
        let started = Instant::now();
        let result = tokio::time::timeout(
            Duration::from_secs(2),
            transport.request(req, Duration::from_millis(150)),
        )
        .await
        .expect("a 150 ms deadline must not wait for an internal default");
        let elapsed = started.elapsed();

        match result.unwrap_err() {
            DownloadError::Transport(transport) => {
                assert_eq!(transport.kind(), crate::error::TransportErrorKind::Timeout);
                assert!(
                    format!("{transport}").contains("request timed out"),
                    "the deadline must reuse the caller-facing message"
                );
            }
            other => panic!("expected a timeout transport error, got {other:?}"),
        }
        assert!(elapsed < Duration::from_secs(2), "took {elapsed:?}");
        server.abort();
    }

    #[test]
    fn range_and_accept_encoding_are_handled_by_options() {
        let config = ClientNetworkConfig::default();
        let transport = transport(&config);
        let req = request(
            "https://example.com/file.bin",
            &[
                ("Range", "bytes=8-15"),
                ("Accept-Encoding", "identity"),
                ("X-Trace", "abc"),
            ],
        );

        let options = transport.options_for(&req, Duration::from_secs(3)).unwrap();
        // `CURLOPT_RANGE` adds the `bytes=` unit itself.
        assert_eq!(options.range.as_deref(), Some("8-15"));
        assert_eq!(
            options.headers,
            vec![("x-trace".to_string(), "abc".to_string())]
        );
        assert_eq!(options.head_timeout, Duration::from_secs(3));
        assert_eq!(options.connect_timeout, config.connect_timeout);
    }

    #[test]
    fn tls_paths_are_carried_to_each_transfer() {
        let config = ClientNetworkConfig {
            ca_info: Some("ca.pem".into()),
            ca_path: Some("certs".into()),
            client_cert: Some("client.pem".into()),
            client_key: Some("client.key".into()),
            ..ClientNetworkConfig::default()
        };
        let options = transport(&config)
            .options_for(
                &request("https://example.com/file.bin", &[]),
                Duration::from_secs(1),
            )
            .unwrap();

        assert_eq!(
            options.ca_info.as_deref(),
            Some(std::path::Path::new("ca.pem"))
        );
        assert_eq!(
            options.ca_path.as_deref(),
            Some(std::path::Path::new("certs"))
        );
        assert_eq!(
            options.client_cert.as_deref(),
            Some(std::path::Path::new("client.pem"))
        );
        assert_eq!(
            options.client_key.as_deref(),
            Some(std::path::Path::new("client.key"))
        );
    }

    #[test]
    fn multi_range_specs_and_case_survive_the_mapping() {
        let transport = transport(&ClientNetworkConfig::default());
        let req = request(
            "https://example.com/file.bin",
            &[("Range", "Bytes=0-9, 20-29")],
        );

        let options = transport.options_for(&req, Duration::from_secs(1)).unwrap();
        assert_eq!(options.range.as_deref(), Some("0-9, 20-29"));
    }

    #[test]
    fn non_byte_range_units_are_forwarded_as_headers() {
        let transport = transport(&ClientNetworkConfig::default());
        let req = request("https://example.com/file.bin", &[("Range", "items=0-9")]);

        let options = transport.options_for(&req, Duration::from_secs(1)).unwrap();
        assert!(options.range.is_none());
        assert_eq!(
            options.headers,
            vec![("range".to_string(), "items=0-9".to_string())]
        );
    }

    #[test]
    fn proxy_selection_follows_the_request_scheme() {
        let config = ClientNetworkConfig {
            http_proxy: Some("http://proxy.test:3128".into()),
            https_proxy: Some("http://secure-proxy.test:3128".into()),
            ..ClientNetworkConfig::default()
        };
        let transport = transport(&config);

        // Proxy values reach libcurl as normalized URIs, exactly like the
        // the libcurl transfer receives them.
        assert_eq!(
            transport.proxy_for("http").map(|p| p.url.as_str()),
            Some("http://proxy.test:3128/")
        );
        assert_eq!(
            transport.proxy_for("https").map(|p| p.url.as_str()),
            Some("http://secure-proxy.test:3128/")
        );
    }

    #[test]
    fn all_proxy_covers_schemes_without_a_specific_proxy() {
        let config = ClientNetworkConfig {
            all_proxy: Some("http://all.test:8080".into()),
            https_proxy: Some("http://secure-proxy.test:3128".into()),
            ..ClientNetworkConfig::default()
        };
        let transport = transport(&config);

        assert_eq!(
            transport.proxy_for("http").map(|p| p.url.as_str()),
            Some("http://all.test:8080/")
        );
        assert_eq!(
            transport.proxy_for("https").map(|p| p.url.as_str()),
            Some("http://secure-proxy.test:3128/")
        );
    }

    #[test]
    fn disabled_pool_forbids_connection_reuse() {
        let config = ClientNetworkConfig {
            pool_max_idle_per_host: 0,
            ..ClientNetworkConfig::default()
        };
        assert!(transport(&config).forbid_connection_reuse);

        let enabled = ClientNetworkConfig {
            pool_max_idle_per_host: 4,
            ..ClientNetworkConfig::default()
        };
        assert!(!transport(&enabled).forbid_connection_reuse);
    }

    #[test]
    fn non_http_schemes_are_rejected() {
        let transport = transport(&ClientNetworkConfig::default());
        let req = request("ftp://example.com/secret", &[]);
        let error = transport
            .options_for(&req, Duration::from_secs(1))
            .unwrap_err();
        assert!(format!("{error}").contains("http or https"));
    }

    #[test]
    fn multipart_style_headers_are_forwarded_verbatim() {
        let transport = transport(&ClientNetworkConfig::default());
        let req = request(
            "http://example.com/file.bin",
            &[
                ("Authorization", "Bearer token"),
                ("If-Match", "\"etag-1\""),
            ],
        );

        let options = transport.options_for(&req, Duration::from_secs(1)).unwrap();
        let headers: HashMap<_, _> = options.headers.into_iter().collect();
        assert_eq!(headers.get("authorization").unwrap(), "Bearer token");
        assert_eq!(headers.get("if-match").unwrap(), "\"etag-1\"");
    }

    #[test]
    fn proxy_endpoints_separate_names_from_ip_literals() {
        let named = ProxyEndpoint::parse("http://user:pass@proxy.test:3128").unwrap();
        assert_eq!(named.host.as_deref(), Some("proxy.test"));
        assert_eq!(named.port, 3128);
        assert!(
            named.url.starts_with("http://user:pass@proxy.test:3128"),
            "proxy credentials travel inside the proxy URL, got: {}",
            named.url
        );

        let literal = ProxyEndpoint::parse("http://127.0.0.1:8080").unwrap();
        assert!(literal.host.is_none(), "an IP literal needs no lookup");

        // The scheme default port is what a lookup has to target.
        let implicit = ProxyEndpoint::parse("https://proxy.test").unwrap();
        assert_eq!(implicit.port, 443);
    }

    #[test]
    fn origin_targets_skip_ip_literals_and_cover_ipv6() {
        assert_eq!(
            origin_target("http://example.test/a/b?c=1").unwrap(),
            Some(("example.test".to_string(), 80))
        );
        assert_eq!(
            origin_target("https://example.test:8443/a").unwrap(),
            Some(("example.test".to_string(), 8443))
        );
        assert_eq!(origin_target("http://127.0.0.1:8080/a").unwrap(), None);
        assert_eq!(origin_target("http://[::1]:8080/a").unwrap(), None);
        assert_eq!(
            origin_target("http://[2001:db8::1]:8080/a").unwrap(),
            None,
            "an IPv6 literal is resolved by libcurl, not by Hickory"
        );
    }

    #[test]
    fn session_body_budget_is_clamped_to_the_transport_range() {
        let mut req = request("http://example.com/file", &[]);
        assert_eq!(body_budget(&req), MAX_BODY_BUDGET_BYTES);

        req.extensions_mut().insert(BodyBudget(32 * 1024));
        assert_eq!(
            body_budget(&req),
            MIN_BODY_BUDGET_BYTES,
            "a tiny session budget must not turn every chunk into a pause"
        );

        req.extensions_mut().insert(BodyBudget(8 * 1024 * 1024));
        assert_eq!(body_budget(&req), MAX_BODY_BUDGET_BYTES);

        req.extensions_mut().insert(BodyBudget(128 * 1024));
        assert_eq!(body_budget(&req), 128 * 1024);
    }

    /// The P3 DNS contract end to end: a name only the shared resolver knows
    /// still connects, and the URL (hence `Host`) keeps the name.
    #[tokio::test]
    async fn resolved_addresses_reach_libcurl_without_changing_the_request() {
        use super::super::test_support::scripted_http_server;

        let server = scripted_http_server(b"resolved-body".to_vec()).await;
        let (dns_addr, _queries, dns_server) = crate::network::dns::spawn_dns_test_server(60).await;

        let config = ClientNetworkConfig {
            dns_servers: vec![dns_addr],
            enable_ipv6: false,
            ..ClientNetworkConfig::default()
        };
        let transport = transport(&config);
        // 127.0.0.1:PORT is what the scripted DNS server answers for this name.
        let req = request(
            &format!("http://bytehaul-resolve.test:{}/file", server.port),
            &[],
        );
        let response = transport
            .request(req, Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(response.status(), http::StatusCode::OK);
        let body = response.into_body().collect_to_bytes().await.unwrap();
        assert_eq!(body.as_ref(), b"resolved-body");

        let head = server.request_head();
        assert!(
            head.contains(&format!("host: bytehaul-resolve.test:{}", server.port)),
            "the Host header must keep the resolved name, got: {head}"
        );
        dns_server.abort();
    }

    /// A proxied request must not need a local answer for the origin host: the
    /// proxy resolves it.
    #[tokio::test]
    async fn proxied_requests_do_not_resolve_the_origin_host_locally() {
        use super::super::test_support::scripted_http_server;

        let proxy = scripted_http_server(b"via-proxy".to_vec()).await;
        let config = ClientNetworkConfig {
            http_proxy: Some(format!("http://127.0.0.1:{}", proxy.port)),
            // A resolver that cannot answer anything: a local lookup would fail.
            dns_servers: vec![SocketAddr::from(([127, 0, 0, 1], 9))],
            enable_ipv6: false,
            ..ClientNetworkConfig::default()
        };
        let transport = transport(&config);
        let req = request("http://origin-that-does-not-resolve.invalid:8080/file", &[]);

        let response = transport
            .request(req, Duration::from_secs(5))
            .await
            .unwrap();
        let body = response.into_body().collect_to_bytes().await.unwrap();
        assert_eq!(body.as_ref(), b"via-proxy");
        let head = proxy.request_head();
        assert!(
            head.starts_with("get http://origin-that-does-not-resolve.invalid:8080/file"),
            "the proxy must receive the absolute-form request line, got: {head}"
        );
    }

    /// The environment must not be able to bypass a configured proxy.
    #[tokio::test]
    async fn an_environment_no_proxy_cannot_divert_a_configured_proxy() {
        use super::super::test_support::{env_guard, scripted_http_server};

        let proxy = scripted_http_server(b"proxied".to_vec()).await;
        // The client reads the proxy environment while it is built, so the lock
        // only has to cover construction: no guard is held across an await.
        let (transport, saved) = {
            let _guard = env_guard().lock().unwrap();
            let saved: Vec<(&str, Option<String>)> = ["NO_PROXY", "no_proxy"]
                .iter()
                .map(|name| (*name, std::env::var(name).ok()))
                .collect();
            std::env::set_var("NO_PROXY", "*");
            std::env::set_var("no_proxy", "*");

            let config = ClientNetworkConfig {
                http_proxy: Some(format!("http://127.0.0.1:{}", proxy.port)),
                enable_ipv6: false,
                ..ClientNetworkConfig::default()
            };
            let transport = transport(&config);

            for (name, value) in saved.iter() {
                match value {
                    Some(value) => std::env::set_var(name, value),
                    None => std::env::remove_var(name),
                }
            }
            (transport, saved)
        };
        let _ = saved;

        let req = request("http://origin.invalid/through-proxy", &[]);
        let body = transport
            .request(req, Duration::from_secs(5))
            .await
            .expect("a configured proxy must be used even when NO_PROXY is set")
            .into_body()
            .collect_to_bytes()
            .await
            .unwrap();
        assert_eq!(body.as_ref(), b"proxied");
        assert_eq!(proxy.requests(), 1);
    }

    /// Proxy credentials travel inside the proxy URL: libcurl uses them for the
    /// proxy hop and the request body is fetched through the authenticated
    /// tunnel.
    #[tokio::test]
    async fn proxy_credentials_are_presented_on_the_proxy_hop_only() {
        use super::super::test_support::authenticating_proxy;

        let proxy = authenticating_proxy(b"authorized".to_vec()).await;
        let config = ClientNetworkConfig {
            http_proxy: Some(format!("http://bytehaul:secret@127.0.0.1:{}", proxy.port)),
            enable_ipv6: false,
            ..ClientNetworkConfig::default()
        };
        let transport = transport(&config);
        let req = request("http://origin.invalid/private", &[]);
        let response = transport
            .request(req, Duration::from_secs(5))
            .await
            .unwrap();
        let body = response.into_body().collect_to_bytes().await.unwrap();
        assert_eq!(body.as_ref(), b"authorized");

        let heads = proxy.heads();
        assert_eq!(
            heads.len(),
            1,
            "credentials from the proxy URL are sent on the first attempt, got: {heads:?}"
        );
        let head = &heads[0];
        assert!(
            head.starts_with("get http://origin.invalid/private"),
            "the credential travels with the absolute-form request, got: {head}"
        );
        assert!(
            head.contains("proxy-authorization: basic ynl0zwhhdww6c2vjcmv0"),
            "the proxy must receive base64(\"bytehaul:secret\"), got: {head}"
        );
    }

    /// A hanging lookup spends the caller's connect budget instead of handing
    /// libcurl a fresh one.
    #[tokio::test]
    async fn a_hanging_lookup_spends_the_caller_deadline() {
        use tokio::net::UdpSocket;

        // A DNS server that never answers.
        let socket = UdpSocket::bind(("127.0.0.1", 0)).await.unwrap();
        let dns_addr = socket.local_addr().unwrap();
        let keeper = tokio::spawn(async move {
            let mut buf = [0u8; 1024];
            loop {
                let _ = socket.recv_from(&mut buf).await;
            }
        });

        let config = ClientNetworkConfig {
            dns_servers: vec![dns_addr],
            enable_ipv6: false,
            ..ClientNetworkConfig::default()
        };
        let transport = transport(&config);
        let req = request("http://never-answers.test/file", &[]);
        let started = Instant::now();
        let error = transport
            .request(req, Duration::from_millis(250))
            .await
            .unwrap_err();
        let elapsed = started.elapsed();

        assert!(
            matches!(
                error,
                DownloadError::Transport(ref transport)
                    if transport.kind() == TransportErrorKind::Timeout
            ),
            "a hanging lookup must surface as a timeout, got {error:?}"
        );
        assert!(
            elapsed < Duration::from_millis(900),
            "the lookup must not exceed the caller's deadline, took {elapsed:?}"
        );
        keeper.abort();
    }

    /// The configured `connect_timeout` covers name resolution too.
    ///
    /// Regression: the lookup was bounded only by the request-headers deadline
    /// and libcurl then received the full `connect_timeout`, so a resolver that
    /// never answers could hold a request for the sum of both instead of the
    /// connect timeout.
    #[tokio::test]
    async fn a_hanging_lookup_cannot_outlast_the_connect_timeout() {
        use tokio::net::UdpSocket;

        let socket = UdpSocket::bind(("127.0.0.1", 0)).await.unwrap();
        let dns_addr = socket.local_addr().unwrap();
        let keeper = tokio::spawn(async move {
            let mut buf = [0u8; 1024];
            loop {
                let _ = socket.recv_from(&mut buf).await;
            }
        });

        let config = ClientNetworkConfig {
            dns_servers: vec![dns_addr],
            enable_ipv6: false,
            connect_timeout: Duration::from_millis(50),
            ..ClientNetworkConfig::default()
        };
        let transport = transport(&config);
        let req = request("http://never-answers.test/file", &[]);
        let started = Instant::now();
        let error = transport
            .request(req, Duration::from_secs(2))
            .await
            .unwrap_err();

        assert!(
            matches!(
                error,
                DownloadError::Transport(ref transport)
                    if transport.kind() == TransportErrorKind::Timeout
            ),
            "a hanging lookup must surface as a timeout, got {error:?}"
        );
        assert!(
            started.elapsed() < Duration::from_millis(600),
            "the 50 ms connect timeout must bound the lookup as well, took {:?}",
            started.elapsed()
        );
        keeper.abort();
    }

    /// A lookup that answers late leaves libcurl only the rest of the connect
    /// budget, so the connect phase never exceeds `connect_timeout`.
    #[tokio::test]
    async fn the_lookup_time_is_deducted_from_the_connect_budget() {
        use super::super::test_support::scripted_http_server;

        let server = scripted_http_server(b"late-lookup".to_vec()).await;
        let (dns_addr, _queries, dns_server) =
            crate::network::dns::spawn_dns_test_server_with_delay(60, Duration::from_millis(150))
                .await;
        let url = format!("http://bytehaul-slow-dns.test:{}/file", server.port);

        // A 100 ms connect budget cannot survive a 150 ms lookup even though
        // the request-headers deadline is generous: the transport must give up
        // instead of starting the connection with a fresh connect timeout.
        let tight = transport(&ClientNetworkConfig {
            dns_servers: vec![dns_addr],
            enable_ipv6: false,
            connect_timeout: Duration::from_millis(100),
            ..ClientNetworkConfig::default()
        });
        let started = Instant::now();
        let error = tight
            .request(request(&url, &[]), Duration::from_secs(5))
            .await
            .unwrap_err();
        assert!(
            matches!(
                error,
                DownloadError::Transport(ref transport)
                    if transport.kind() == TransportErrorKind::Timeout
            ),
            "a spent connect budget must end the request, got {error:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "the answer arrived after the budget, so the request stops there, took {:?}",
            started.elapsed()
        );
        assert_eq!(
            server.requests(),
            0,
            "the connection must not have been attempted"
        );

        // The same late lookup fits a budget that covers it, and the request
        // then completes on the resolved address.
        let generous = transport(&ClientNetworkConfig {
            dns_servers: vec![dns_addr],
            enable_ipv6: false,
            connect_timeout: Duration::from_secs(2),
            ..ClientNetworkConfig::default()
        });
        let body = generous
            .request(request(&url, &[]), Duration::from_secs(5))
            .await
            .expect("a lookup inside the connect budget must succeed")
            .into_body()
            .collect_to_bytes()
            .await
            .unwrap();
        assert_eq!(body.as_ref(), b"late-lookup");
        dns_server.abort();
    }
}
