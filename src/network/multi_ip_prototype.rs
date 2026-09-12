//! Executable architecture spike; deliberately not wired into production.
//! The outer TLS connector receives the origin URI, while only TCP is pinned.

use super::*;
use crate::http::HttpBody;
use hyper::rt::{Read, ReadBufCursor, Write};
use hyper_util::client::legacy::connect::{Connected, Connection};
use hyper_util::rt::TokioIo;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tower_service::Service;

#[derive(Clone, Debug, PartialEq, Eq)]
struct Identity {
    id: u64,
    peer: SocketAddr,
}

struct TaggedIo {
    io: TokioIo<TcpStream>,
    identity: Identity,
    counts: Arc<ConnectionCounts>,
}

#[derive(Default)]
struct ConnectionCounts {
    opened: AtomicU64,
    live: AtomicU64,
}

impl Drop for TaggedIo {
    fn drop(&mut self) {
        self.counts.live.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Connection for TaggedIo {
    fn connected(&self) -> Connected {
        self.io.connected().extra(self.identity.clone())
    }
}

impl Read for TaggedIo {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: ReadBufCursor<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io).poll_read(cx, buf)
    }
}

impl Write for TaggedIo {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.io).poll_write(cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io).poll_shutdown(cx)
    }
}

#[derive(Clone)]
struct PinnedConnector {
    peer: SocketAddr,
    ids: Arc<ConnectionCounts>,
}

impl Service<Uri> for PinnedConnector {
    type Response = TaggedIo;
    type Error = io::Error;
    type Future = Pin<Box<dyn Future<Output = io::Result<TaggedIo>> + Send>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, _: Uri) -> Self::Future {
        let peer = self.peer;
        let ids = self.ids.clone();
        Box::pin(async move {
            let tcp = TcpStream::connect(peer).await?;
            let identity = Identity {
                id: ids.opened.fetch_add(1, Ordering::SeqCst),
                peer: tcp.peer_addr()?,
            };
            ids.live.fetch_add(1, Ordering::SeqCst);
            Ok(TaggedIo {
                io: TokioIo::new(tcp),
                identity,
                counts: ids,
            })
        })
    }
}

type Group = Client<HttpsConnector<PinnedConnector>, HttpRequestBody>;

fn group(
    peer: SocketAddr,
    ids: Arc<ConnectionCounts>,
    roots: tokio_rustls::rustls::RootCertStore,
    idle: usize,
) -> Group {
    let tls = tokio_rustls::rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let connector = HttpsConnectorBuilder::new()
        .with_tls_config(tls)
        .https_or_http()
        .enable_http1()
        .wrap_connector(PinnedConnector { peer, ids });
    Client::builder(TokioExecutor::new())
        .pool_max_idle_per_host(idle)
        .pool_timer(TokioTimer::new())
        .pool_idle_timeout(Duration::from_secs(30))
        .build(connector)
}

// A body owner, not a response extension: into_body must retain the permit.
struct LeasedBody {
    body: HttpBody,
    permit: Option<OwnedSemaphorePermit>,
}

impl LeasedBody {
    async fn finish(mut self) {
        // Drain through the neutral body interface; the fixture releases each
        // held body on demand, so bound the wait explicitly.
        loop {
            match self.body.next_chunk(Duration::from_secs(30)).await {
                Ok(Some(_)) => {}
                Ok(None) => break,
                Err(error) => panic!("leased body drain failed: {error}"),
            }
        }
        self.permit.take();
    }
}

async fn request(group: &Group, slots: Arc<Semaphore>, uri: &str) -> (Identity, LeasedBody) {
    let permit = slots.acquire_owned().await.unwrap();
    let response = group
        .request(
            http::Request::builder()
                .uri(uri)
                .body(HttpRequestBody::new())
                .unwrap(),
        )
        .await
        .unwrap();
    let identity = response.extensions().get::<Identity>().unwrap().clone();
    (
        identity,
        LeasedBody {
            body: HttpBody::from(response.into_body()),
            permit: Some(permit),
        },
    )
}

async fn headers<S: tokio::io::AsyncRead + Unpin>(stream: &mut S) -> Option<String> {
    let mut bytes = Vec::new();
    loop {
        let mut byte = [0];
        if stream.read(&mut byte).await.unwrap() == 0 {
            return None;
        }
        bytes.push(byte[0]);
        assert!(bytes.len() < 8192);
        if bytes.ends_with(b"\r\n\r\n") {
            return Some(String::from_utf8(bytes).unwrap().to_ascii_lowercase());
        }
    }
}

struct Server {
    addr: SocketAddr,
    task: tokio::task::JoinHandle<()>,
    release_bodies: Arc<Semaphore>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn http_server(ip: &str) -> Server {
    let listener = TcpListener::bind((ip, 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let release_bodies = Arc::new(Semaphore::new(0));
    let release = release_bodies.clone();
    let task = tokio::spawn(async move {
        let mut connections = tokio::task::JoinSet::new();
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            let release = release.clone();
            connections.spawn(async move {
                while let Some(request) = headers(&mut stream).await {
                    assert!(request.contains("\r\nhost: origin.example\r\n"));
                    if request.starts_with("get /held ") {
                        stream
                            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\n")
                            .await
                            .unwrap();
                        release.acquire().await.unwrap().forget();
                        stream.write_all(b"pong").await.unwrap();
                        continue;
                    }
                    if request.starts_with("get /stall ") {
                        stream
                            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1000000\r\n\r\nx")
                            .await
                            .unwrap();
                        let mut buf = [0];
                        let _ = stream.read(&mut buf).await;
                        return;
                    }
                    stream
                        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\npong")
                        .await
                        .unwrap();
                }
            });
        }
    });
    Server {
        addr,
        task,
        release_bodies,
    }
}

#[tokio::test]
async fn pinned_groups_preserve_host_peer_identity_and_reuse() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let a = http_server("127.0.0.1").await;
        let b = http_server("127.0.0.2").await;
        let ids = Arc::new(ConnectionCounts::default());
        let slots = Arc::new(Semaphore::new(2));
        // Allocate one idle slot per group from a total of two, not two each.
        let ga = group(
            a.addr,
            ids.clone(),
            tokio_rustls::rustls::RootCertStore::empty(),
            1,
        );
        let gb = group(
            b.addr,
            ids.clone(),
            tokio_rustls::rustls::RootCertStore::empty(),
            1,
        );
        let mut previous = None;
        for _ in 0..3 {
            let ((ia, ba), (ib, bb)) = tokio::join!(
                request(&ga, slots.clone(), "http://origin.example/range"),
                request(&gb, slots.clone(), "http://origin.example/range")
            );
            assert_eq!(ia.peer, a.addr);
            assert_eq!(ib.peer, b.addr);
            assert_ne!(ia.id, ib.id);
            if let Some(ref pair) = previous {
                assert_eq!(pair, &(ia.clone(), ib.clone()));
            }
            previous = Some((ia, ib));
            assert_eq!(slots.available_permits(), 0);
            tokio::join!(ba.finish(), bb.finish());
            assert_eq!(slots.available_permits(), 2);
        }
        assert_eq!(ids.opened.load(Ordering::SeqCst), 2);
        assert_eq!(ids.live.load(Ordering::SeqCst), 2);
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn body_drop_and_cancelled_pool_wait_release_request_budget() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let server = http_server("127.0.0.1").await;
        let ids = Arc::new(ConnectionCounts::default());
        let client = group(
            server.addr,
            ids.clone(),
            tokio_rustls::rustls::RootCertStore::empty(),
            1,
        );
        let slots = Arc::new(Semaphore::new(1));
        let (first, body) = request(&client, slots.clone(), "http://origin.example/stall").await;
        assert_eq!(slots.available_permits(), 0);
        assert!(tokio::time::timeout(
            Duration::from_millis(20),
            request(&client, slots.clone(), "http://origin.example/range")
        )
        .await
        .is_err());
        assert_eq!(ids.opened.load(Ordering::SeqCst), 1);
        drop(body);
        assert_eq!(slots.available_permits(), 1);
        let (second, body) = request(&client, slots.clone(), "http://origin.example/range").await;
        assert_ne!(first.id, second.id);
        body.finish().await;
        assert_eq!(slots.available_permits(), 1);
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn group_idle_quota_and_eviction_close_actual_sockets() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let server = http_server("127.0.0.1").await;
        let counts = Arc::new(ConnectionCounts::default());
        let client = group(
            server.addr,
            counts.clone(),
            tokio_rustls::rustls::RootCertStore::empty(),
            1,
        );
        let slots = Arc::new(Semaphore::new(3));
        // Incomplete bodies force distinct sockets; a one-idle pool is NOT
        // a one-connection concurrency limit.
        let mut bodies = Vec::new();
        for _ in 0..3 {
            let (_, body) = request(&client, slots.clone(), "http://origin.example/held").await;
            bodies.push(body);
        }
        assert_eq!(counts.live.load(Ordering::SeqCst), 3);
        assert_eq!(slots.available_permits(), 0);
        server.release_bodies.add_permits(3);
        for body in bodies {
            body.finish().await;
        }
        assert_eq!(slots.available_permits(), 3);
        while counts.live.load(Ordering::SeqCst) != 1 {
            tokio::task::yield_now().await;
        }
        let (_, body) = request(&client, slots.clone(), "http://origin.example/range").await;
        body.finish().await;
        assert_eq!(counts.live.load(Ordering::SeqCst), 1);
        drop(client);
        while counts.live.load(Ordering::SeqCst) != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn pinned_tls_uses_origin_sni_and_rejects_wrong_name() {
    use rcgen::{CertificateParams, KeyPair};
    use tokio_rustls::rustls::{self, pki_types::PrivatePkcs8KeyDer, RootCertStore};
    tokio::time::timeout(Duration::from_secs(10), async {
        let key = KeyPair::generate().unwrap();
        let cert = CertificateParams::new(vec!["origin.example".into()])
            .unwrap()
            .self_signed(&key)
            .unwrap();
        let config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(
                vec![cert.der().clone()],
                PrivatePkcs8KeyDer::from(key.serialize_der()).into(),
            )
            .unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut tls = acceptor.accept(tcp).await.unwrap();
            assert_eq!(tls.get_ref().1.server_name(), Some("origin.example"));
            assert!(headers(&mut tls)
                .await
                .unwrap()
                .contains("\r\nhost: origin.example\r\n"));
            tls.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\npong")
                .await
                .unwrap();
            tls.shutdown().await.unwrap();
            let (tcp, _) = listener.accept().await.unwrap();
            assert!(acceptor.accept(tcp).await.is_err());
        });
        let mut fixture = Server {
            addr,
            task,
            release_bodies: Arc::new(Semaphore::new(0)),
        };
        let mut roots = RootCertStore::empty();
        roots.add(cert.der().clone()).unwrap();
        let client = group(addr, Arc::new(ConnectionCounts::default()), roots, 1);
        let (identity, body) = request(
            &client,
            Arc::new(Semaphore::new(1)),
            "https://origin.example/range",
        )
        .await;
        assert_eq!(identity.peer, addr);
        body.finish().await;
        let error = client
            .request(
                http::Request::builder()
                    .uri("https://wrong.example/range")
                    .body(HttpRequestBody::new())
                    .unwrap(),
            )
            .await
            .unwrap_err();
        assert!(error.is_connect());
        (&mut fixture.task).await.unwrap();
    })
    .await
    .unwrap();
}
