//! Network-level TLS pooling tests. The custom root is confined to these clients;
//! this does not exercise Downloader's platform trust-store configuration.

use super::*;
use http_body_util::BodyExt;
use rcgen::{BasicConstraints, CertificateParams, IsCa, KeyPair, KeyUsagePurpose};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_rustls::rustls::{self, pki_types::PrivatePkcs8KeyDer, RootCertStore};
use tokio_rustls::TlsAcceptor;

struct TlsFixture {
    addr: SocketAddr,
    root: rustls::pki_types::CertificateDer<'static>,
    server: tokio::task::JoinHandle<(usize, usize)>,
}

impl Drop for TlsFixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}

impl TlsFixture {
    async fn start(close: bool, reject: bool) -> Self {
        let ca_key = KeyPair::generate().unwrap();
        let mut ca_params = CertificateParams::new(Vec::<String>::new()).unwrap();
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca_params.key_usages = vec![KeyUsagePurpose::KeyCertSign];
        let ca = ca_params.self_signed(&ca_key).unwrap();
        let key = KeyPair::generate().unwrap();
        let cert = CertificateParams::new(vec!["127.0.0.1".into()])
            .unwrap()
            .signed_by(&key, &ca, &ca_key)
            .unwrap();
        let config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(
                vec![cert.der().clone()],
                PrivatePkcs8KeyDer::from(key.serialize_der()).into(),
            )
            .unwrap();
        let acceptor = TlsAcceptor::from(Arc::new(config));
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let mut connections = 0;
            let mut requests = 0;
            while requests < 2 {
                let (tcp, _) = listener.accept().await.unwrap();
                connections += 1;
                let handshake = acceptor.accept(tcp).await;
                if reject {
                    assert!(handshake.is_err(), "untrusted handshake must fail");
                    return (connections, requests);
                }
                let mut stream = handshake.unwrap();
                loop {
                    let mut request = Vec::new();
                    loop {
                        let mut byte = [0];
                        if stream.read(&mut byte).await.unwrap() == 0 {
                            break;
                        }
                        request.push(byte[0]);
                        assert!(request.len() < 8192);
                        if request.ends_with(b"\r\n\r\n") {
                            break;
                        }
                    }
                    if request.is_empty() {
                        break;
                    }
                    let start = requests * 4;
                    let end = start + 3;
                    let request = String::from_utf8(request).unwrap().to_ascii_lowercase();
                    assert!(request.starts_with("get /range http/1.1\r\n"));
                    assert!(request.contains(&format!("\r\nrange: bytes={start}-{end}\r\n")));
                    let connection = if close { "close" } else { "keep-alive" };
                    let headers = format!(
                        "HTTP/1.1 206 Partial Content\r\nContent-Length: 4\r\nContent-Range: bytes {start}-{end}/8\r\nConnection: {connection}\r\n\r\n"
                    );
                    stream.write_all(headers.as_bytes()).await.unwrap();
                    stream.write_all(&b"abcdefgh"[start..=end]).await.unwrap();
                    stream.flush().await.unwrap();
                    requests += 1;
                    if close || requests == 2 {
                        stream.shutdown().await.unwrap();
                        break;
                    }
                }
            }
            (connections, requests)
        });
        Self {
            addr,
            root: ca.der().clone(),
            server,
        }
    }

    fn client(&self, trusted: bool) -> BytehaulClient {
        let mut roots = RootCertStore::empty();
        if trusted {
            roots.add(self.root.clone()).unwrap();
        }
        let tls = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let network = ClientNetworkConfig::default();
        let connector = HttpsConnectorBuilder::new()
            .with_tls_config(tls)
            .https_only()
            .enable_http1()
            .wrap_connector(network.build_http_connector(network.build_dns_resolver().unwrap()));
        // Match build_client's explicit pooling knobs without consulting proxy
        // environment or changing the machine's trusted certificate store.
        let mut builder = Client::builder(TokioExecutor::new());
        builder.pool_max_idle_per_host(1);
        builder.pool_idle_timeout(Duration::from_secs(30));
        BytehaulClient::Direct(Arc::new(builder.build(connector)))
    }

    fn request(&self, start: usize) -> hyper::Request<HttpRequestBody> {
        hyper::Request::builder()
            .uri(format!("https://{}/range", self.addr))
            .header("range", format!("bytes={start}-{}", start + 3))
            .body(HttpRequestBody::new())
            .unwrap()
    }
}

async fn verify_ranges(close: bool, expected_connections: usize) {
    tokio::time::timeout(Duration::from_secs(10), async {
        let mut fixture = TlsFixture::start(close, false).await;
        let client = fixture.client(true);
        for start in [0, 4] {
            let response = client.request(fixture.request(start)).await.unwrap();
            assert_eq!(response.status(), hyper::StatusCode::PARTIAL_CONTENT);
            assert_eq!(response.version(), hyper::Version::HTTP_11);
            assert_eq!(
                response.headers()["content-range"],
                format!("bytes {start}-{}/8", start + 3)
            );
            let body = response.into_body().collect().await.unwrap().to_bytes();
            assert_eq!(body.as_ref(), &b"abcdefgh"[start..start + 4]);
        }
        assert_eq!(
            (&mut fixture.server).await.unwrap(),
            (expected_connections, 2)
        );
    })
    .await
    .expect("TLS range test timed out");
}

#[tokio::test]
async fn trusted_tls_pool_reuses_connection_for_consumed_ranges() {
    verify_ranges(false, 1).await;
}

#[tokio::test]
async fn trusted_tls_pool_reconnects_after_connection_close() {
    verify_ranges(true, 2).await;
}

#[tokio::test]
async fn tls_pool_rejects_untrusted_certificate_before_http() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let mut fixture = TlsFixture::start(false, true).await;
        let error = fixture
            .client(false)
            .request(fixture.request(0))
            .await
            .unwrap_err();
        assert!(
            matches!(&error, DownloadError::Transport(transport)
                if transport.kind() == crate::error::TransportErrorKind::Connect),
            "{error:?}"
        );
        let mut source: Option<&(dyn std::error::Error + 'static)> = Some(&error);
        let mut unknown_issuer = false;
        while let Some(current) = source {
            unknown_issuer |= matches!(
                current.downcast_ref::<rustls::Error>(),
                Some(rustls::Error::InvalidCertificate(
                    rustls::CertificateError::UnknownIssuer
                ))
            );
            source = match current.downcast_ref::<std::io::Error>() {
                Some(io) => io.get_ref().map(|inner| inner as &dyn std::error::Error),
                None => current.source(),
            };
        }
        assert!(
            unknown_issuer,
            "expected certificate trust failure: {error:?}"
        );
        assert_eq!((&mut fixture.server).await.unwrap(), (1, 0));
    })
    .await
    .expect("untrusted TLS test timed out");
}
