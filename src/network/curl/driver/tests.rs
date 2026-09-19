use super::*;
use crate::http::{MAX_BODY_BUDGET_BYTES, MIN_BODY_BUDGET_BYTES};
use crate::network::curl::test_support::DualAddressServer;
use std::net::SocketAddr;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// How one test server answers each request.
#[derive(Clone, Debug)]
struct ResponsePlan {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    head_delay: Duration,
    chunk_delay: Duration,
    chunk_size: usize,
    /// Close the socket in the middle of the body instead of finishing it.
    truncate_at: Option<usize>,
    /// Keep the connection open for another request after a full body.
    keep_alive: bool,
}

impl ResponsePlan {
    fn body(body: Vec<u8>) -> Self {
        Self {
            status: 200,
            headers: Vec::new(),
            body,
            head_delay: Duration::ZERO,
            chunk_delay: Duration::ZERO,
            chunk_size: 8 * 1024,
            truncate_at: None,
            keep_alive: true,
        }
    }
}

struct TestServer {
    addr: SocketAddr,
    accepted: Arc<AtomicUsize>,
    /// Connections the server has not seen close yet. The count drops when the
    /// peer closes, which is how a test observes a cached socket going away.
    live: Arc<AtomicUsize>,
    requests: Arc<Mutex<Vec<String>>>,
    shutdown: Option<oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<()>,
}

impl TestServer {
    async fn start(plan: ResponsePlan) -> Self {
        Self::start_with(Arc::new(move |_| plan.clone())).await
    }

    /// Starts a server that picks the response plan per request target.
    async fn start_with(plan_for: Arc<dyn Fn(&str) -> ResponsePlan + Send + Sync>) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let accepted = Arc::new(AtomicUsize::new(0));
        let live = Arc::new(AtomicUsize::new(0));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let (shutdown, mut shutdown_rx) = oneshot::channel::<()>();
        let task_accepted = accepted.clone();
        let task_live = live.clone();
        let task_requests = requests.clone();
        let task = tokio::spawn(async move {
            loop {
                let accept = listener.accept();
                let connection = tokio::select! {
                    result = accept => result,
                    _ = &mut shutdown_rx => break,
                };
                let Ok((stream, _)) = connection else {
                    break;
                };
                task_accepted.fetch_add(1, Ordering::SeqCst);
                task_live.fetch_add(1, Ordering::SeqCst);
                let plan_for = plan_for.clone();
                let requests = task_requests.clone();
                let live = task_live.clone();
                tokio::spawn(async move {
                    serve_connection(stream, plan_for, requests).await;
                    live.fetch_sub(1, Ordering::SeqCst);
                });
            }
        });
        Self {
            addr,
            accepted,
            live,
            requests,
            shutdown: Some(shutdown),
            task,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.addr.port())
    }

    fn accepted(&self) -> usize {
        self.accepted.load(Ordering::SeqCst)
    }

    fn live(&self) -> usize {
        self.live.load(Ordering::SeqCst)
    }

    /// Waits until exactly `expected` connections are still open.
    async fn wait_for_live(&self, expected: usize, timeout: Duration) -> usize {
        let started = Instant::now();
        while started.elapsed() < timeout {
            if self.live() == expected {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        self.live()
    }

    fn request_heads(&self) -> Vec<String> {
        self.requests.lock().clone()
    }

    async fn stop(mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        let _ = tokio::time::timeout(Duration::from_secs(5), self.task).await;
    }
}

async fn serve_connection(
    mut stream: tokio::net::TcpStream,
    plan_for: Arc<dyn Fn(&str) -> ResponsePlan + Send + Sync>,
    requests: Arc<Mutex<Vec<String>>>,
) {
    loop {
        let Some(head) = read_request_head(&mut stream).await else {
            return;
        };
        let plan = plan_for(request_target(&head));
        requests.lock().push(head);
        if !plan.head_delay.is_zero() {
            tokio::time::sleep(plan.head_delay).await;
        }
        let mut response = format!("HTTP/1.1 {} OK\r\n", plan.status);
        for (name, value) in &plan.headers {
            response.push_str(&format!("{name}: {value}\r\n"));
        }
        let body_len = plan.truncate_at.unwrap_or(plan.body.len());
        response.push_str(&format!("Content-Length: {}\r\n", plan.body.len()));
        if plan.keep_alive {
            response.push_str("Connection: keep-alive\r\n");
        } else {
            response.push_str("Connection: close\r\n");
        }
        response.push_str("\r\n");
        if stream.write_all(response.as_bytes()).await.is_err() {
            return;
        }

        let mut written = 0;
        let chunk_size = plan.chunk_size.max(1);
        while written < body_len {
            let end = (written + chunk_size).min(body_len);
            if stream.write_all(&plan.body[written..end]).await.is_err() {
                return;
            }
            written = end;
            if !plan.chunk_delay.is_zero() {
                tokio::time::sleep(plan.chunk_delay).await;
            }
        }
        if plan.truncate_at.is_some() {
            let _ = stream.shutdown().await;
            return;
        }
        if !plan.keep_alive {
            let _ = stream.shutdown().await;
            return;
        }
    }
}

async fn read_request_head(stream: &mut tokio::net::TcpStream) -> Option<String> {
    let mut buffer = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match stream.read(&mut byte).await {
            Ok(0) => return None,
            Ok(_) => {
                buffer.push(byte[0]);
                if buffer.ends_with(b"\r\n\r\n") {
                    return Some(String::from_utf8_lossy(&buffer).to_string());
                }
                if buffer.len() > 64 * 1024 {
                    return None;
                }
            }
            Err(_) => return None,
        }
    }
}

/// The request target of a request line, for per-path response plans.
fn request_target(head: &str) -> &str {
    head.lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("/")
}

async fn collect_body(transfer: &mut Transfer, read_timeout: Duration) -> Vec<u8> {
    let mut collected = Vec::new();
    while let Some(chunk) = transfer.body.next_chunk(read_timeout).await.unwrap() {
        collected.extend_from_slice(&chunk);
    }
    collected
}

/// Local HTTPS fixture: a test CA, a leaf certificate for `127.0.0.1`, and a
/// server that answers keep-alive range requests until it reaches `requests`.
struct TlsFixture {
    addr: SocketAddr,
    ca_pem: std::path::PathBuf,
    accepted: Arc<AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
    _dir: tempfile::TempDir,
}

impl TlsFixture {
    async fn start(requests: usize) -> Self {
        use rcgen::{
            BasicConstraints, CertificateParams, DistinguishedName, DnType,
            ExtendedKeyUsagePurpose, IsCa, KeyPair, KeyUsagePurpose,
        };
        use tokio_rustls::rustls::{self, pki_types::PrivatePkcs8KeyDer};
        use tokio_rustls::TlsAcceptor;

        let ca_key = KeyPair::generate().unwrap();
        let mut ca_params = CertificateParams::new(Vec::<String>::new()).unwrap();
        // rcgen's default distinguished name is the same for every certificate,
        // which would make the leaf's issuer equal to its own subject and let
        // OpenSSL treat it as self-signed instead of chaining it to this CA.
        let mut ca_name = DistinguishedName::new();
        ca_name.push(DnType::CommonName, "bytehaul-test-ca");
        ca_params.distinguished_name = ca_name;
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca_params.key_usages = vec![KeyUsagePurpose::KeyCertSign];
        let ca = ca_params.self_signed(&ca_key).unwrap();

        let key = KeyPair::generate().unwrap();
        let mut leaf_params = CertificateParams::new(vec!["127.0.0.1".to_string()]).unwrap();
        // Schannel rejects a server certificate without the server-auth EKU.
        leaf_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        leaf_params.key_usages = vec![
            KeyUsagePurpose::DigitalSignature,
            KeyUsagePurpose::KeyEncipherment,
        ];
        let cert = leaf_params.signed_by(&key, &ca, &ca_key).unwrap();

        let dir = tempfile::tempdir().unwrap();
        let ca_pem = dir.path().join("bytehaul-test-ca.pem");
        std::fs::write(&ca_pem, ca.pem()).unwrap();

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
        let accepted = Arc::new(AtomicUsize::new(0));
        let task_accepted = accepted.clone();
        let task = tokio::spawn(async move {
            let mut served = 0usize;
            while served < requests {
                let Ok((tcp, _)) = listener.accept().await else {
                    return;
                };
                task_accepted.fetch_add(1, Ordering::SeqCst);
                let Ok(mut stream) = acceptor.accept(tcp).await else {
                    // A client that rejects our certificate aborts the
                    // handshake; that is a valid outcome for the caller.
                    continue;
                };
                loop {
                    let mut request = Vec::new();
                    loop {
                        let mut byte = [0u8];
                        match stream.read(&mut byte).await {
                            Ok(0) | Err(_) => return,
                            Ok(_) => request.push(byte[0]),
                        }
                        if request.ends_with(b"\r\n\r\n") {
                            break;
                        }
                        assert!(request.len() < 8192, "oversized request head");
                    }
                    let head = String::from_utf8_lossy(&request).to_ascii_lowercase();
                    assert!(head.starts_with("get /range "), "got: {head}");
                    let range = head
                        .lines()
                        .find_map(|line| line.strip_prefix("range: bytes="))
                        .expect("range header");
                    let (start, end) = range.split_once('-').expect("range spec");
                    let start: usize = start.trim().parse().unwrap();
                    let end: usize = end.trim().parse().unwrap();
                    let headers = format!(
                        "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {start}-{end}/8\r\nConnection: keep-alive\r\n\r\n",
                        end - start + 1
                    );
                    if stream.write_all(headers.as_bytes()).await.is_err() {
                        return;
                    }
                    if stream.write_all(&b"abcdefgh"[start..=end]).await.is_err() {
                        return;
                    }
                    if stream.flush().await.is_err() {
                        return;
                    }
                    served += 1;
                    if served == requests {
                        let _ = stream.shutdown().await;
                        return;
                    }
                }
            }
        });

        Self {
            addr,
            ca_pem,
            accepted,
            task,
            _dir: dir,
        }
    }

    fn url(&self, host: &str) -> String {
        format!("https://{host}:{}/range", self.addr.port())
    }

    fn options(&self, url: &str, start: usize) -> RequestOptions {
        let mut options = RequestOptions::new(url);
        options.range = Some(format!("{start}-{}", start + 3));
        options.ca_info = Some(self.ca_pem.clone());
        options
    }

    async fn stop(self) {
        self.task.abort();
    }
}

/// A `CURLOPT_RESOLVE` entry pointing one name at the loopback address the
/// fixture listens on, with a TTL long enough for a single test.
fn local_resolve(host: &str, port: u16) -> ResolveEntry {
    ResolveEntry::new(
        host,
        port,
        &[std::net::IpAddr::from([127, 0, 0, 1])],
        Duration::from_secs(600),
    )
}

/// These HTTPS tests need a TLS-capable environment. They are ignored because
/// the agent sandbox this was developed in denies the Windows credential
/// store: Schannel then fails with `AcquireCredentialsHandle failed:
/// SEC_E_NO_CREDENTIALS` before any handshake, which the system `curl.exe`
/// reproduces (`curl: (35) schannel: ...`). Run them with
/// `cargo test --features curl-backend -- --ignored` on a CI runner.
#[tokio::test]
#[ignore = "requires a TLS-capable environment (Schannel credentials unavailable in the dev sandbox)"]
async fn local_https_range_requests_reuse_one_verified_connection() {
    let fixture = TlsFixture::start(2).await;
    let driver = DriverHandle::spawn(DriverConfig::default());

    for start in [0usize, 4] {
        let mut transfer = driver
            .get(fixture.options(&fixture.url("127.0.0.1"), start), 1024)
            .await
            .unwrap();
        assert_eq!(transfer.head.status, 206);
        assert_eq!(
            transfer.head.header("content-range"),
            Some(format!("bytes {start}-{}/8", start + 3).as_str())
        );
        let body = collect_body(&mut transfer, Duration::from_secs(5)).await;
        assert_eq!(body, &b"abcdefgh"[start..=start + 3]);
    }

    assert_eq!(
        fixture.accepted.load(Ordering::SeqCst),
        1,
        "a verified keep-alive connection must be reused"
    );
    assert!(wait_for_idle(&driver, Duration::from_secs(2)).await);
    fixture.stop().await;
}

#[tokio::test]
#[ignore = "requires a TLS-capable environment (Schannel credentials unavailable in the dev sandbox)"]
async fn local_https_without_a_trusted_ca_fails_verification() {
    let fixture = TlsFixture::start(1).await;
    let driver = DriverHandle::spawn(DriverConfig::default());

    let mut options = RequestOptions::new(fixture.url("127.0.0.1"));
    options.connect_timeout = Duration::from_secs(5);
    options.head_timeout = Duration::from_secs(5);
    let error = match driver.get(options, 1024).await {
        Ok(_) => panic!("an untrusted certificate must not produce a response"),
        Err(error) => error,
    };

    match error {
        DownloadError::Transport(transport) => {
            assert_eq!(transport.kind(), TransportErrorKind::Other);
            assert!(
                !DownloadError::Transport(transport).is_retryable(),
                "a rejected certificate is a deterministic failure"
            );
        }
        other => panic!("expected a transport error, got {other:?}"),
    }
    assert!(wait_for_idle(&driver, Duration::from_secs(2)).await);
    fixture.stop().await;
}

#[tokio::test]
#[ignore = "requires a TLS-capable environment (Schannel credentials unavailable in the dev sandbox)"]
async fn local_https_hostname_mismatch_fails_verification() {
    let fixture = TlsFixture::start(1).await;
    let driver = DriverHandle::spawn(DriverConfig::default());

    // The certificate is issued for 127.0.0.1 only; pinning `localhost` to the
    // same socket must still fail verification.
    let mut options = fixture.options("https://localhost/range", 0);
    options.resolve = Some(local_resolve("localhost", fixture.addr.port()));
    let error = match driver.get(options, 1024).await {
        Ok(_) => panic!("a hostname mismatch must not produce a response"),
        Err(error) => error,
    };

    match error {
        DownloadError::Transport(transport) => {
            assert!(!DownloadError::Transport(transport).is_retryable());
        }
        other => panic!("expected a transport error, got {other:?}"),
    }
    fixture.stop().await;
}

#[tokio::test]
async fn headers_are_published_before_the_body_finishes() {
    let plan = ResponsePlan {
        head_delay: Duration::from_millis(30),
        chunk_delay: Duration::from_millis(40),
        chunk_size: 4,
        ..ResponsePlan::body(vec![b'x'; 64])
    };
    let server = TestServer::start(plan).await;
    let driver = DriverHandle::spawn(DriverConfig::default());

    let started = Instant::now();
    let mut transfer = driver
        .get(RequestOptions::new(server.url("/head-first")), 1024)
        .await
        .unwrap();
    let head_elapsed = started.elapsed();

    assert_eq!(transfer.head.status, 200);
    assert_eq!(transfer.head.content_length(), Some(64));
    // Headers must arrive before the body finishes streaming: the body
    // alone takes at least 15 * 40 ms.
    assert!(
        head_elapsed < Duration::from_millis(300),
        "headers took {head_elapsed:?}"
    );

    let body = collect_body(&mut transfer, Duration::from_secs(5)).await;
    assert_eq!(body, vec![b'x'; 64]);
    assert_eq!(driver.completed(), 1);
    assert!(wait_for_idle(&driver, Duration::from_secs(2)).await);

    server.stop().await;
}

#[tokio::test]
async fn bounded_queue_pauses_without_duplicating_bytes() {
    // 256 KiB of unique bytes with a 16 KiB budget forces many pauses.
    let body: Vec<u8> = (0..262_144u32).map(|index| (index % 251) as u8).collect();
    let plan = ResponsePlan {
        chunk_size: 8 * 1024,
        ..ResponsePlan::body(body.clone())
    };
    let server = TestServer::start(plan).await;
    let driver = DriverHandle::spawn(DriverConfig::default());

    let mut transfer = driver
        .get(RequestOptions::new(server.url("/bounded")), 16 * 1024)
        .await
        .unwrap();

    let mut collected = Vec::new();
    while let Some(chunk) = transfer
        .body
        .next_chunk(Duration::from_secs(5))
        .await
        .unwrap()
    {
        // Slow consumer: let libcurl catch up before reading more.
        collected.extend_from_slice(&chunk);
        assert!(
            transfer.body.buffered() <= 16 * 1024,
            "the queue must never exceed the configured budget"
        );
        tokio::time::sleep(Duration::from_millis(1)).await;
    }

    assert_eq!(collected.len(), body.len());
    assert_eq!(collected, body, "paused bytes must not be duplicated");
    assert!(
        transfer.body.pause_count() > 0,
        "a 16 KiB budget must pause the transfer"
    );
    assert_eq!(transfer.body.accepted_bytes(), body.len());
    assert_eq!(transfer.body.buffered(), 0);
    assert!(wait_for_idle(&driver, Duration::from_secs(2)).await);

    server.stop().await;
}

#[tokio::test]
async fn dropping_an_unread_body_cancels_the_transfer() {
    let plan = ResponsePlan {
        chunk_delay: Duration::from_millis(20),
        chunk_size: 4,
        ..ResponsePlan::body(vec![b'y'; 4096])
    };
    let server = TestServer::start(plan).await;
    let driver = DriverHandle::spawn(DriverConfig::default());

    {
        let _transfer = driver
            .get(RequestOptions::new(server.url("/drop")), 8 * 1024)
            .await
            .unwrap();
        // Dropped without reading the body.
    }

    assert!(
        wait_for_idle(&driver, Duration::from_secs(5)).await,
        "dropping the transfer must remove it from the multi handle"
    );
    assert_eq!(driver.active_transfers(), 0);

    server.stop().await;
}

#[tokio::test]
async fn cancelling_while_streaming_leaves_no_active_transfer() {
    let plan = ResponsePlan {
        chunk_delay: Duration::from_millis(10),
        chunk_size: 1024,
        ..ResponsePlan::body(vec![b'z'; 64 * 1024])
    };
    let server = TestServer::start(plan).await;
    let driver = DriverHandle::spawn(DriverConfig::default());

    let mut transfer = driver
        .get(RequestOptions::new(server.url("/cancel")), 4 * 1024)
        .await
        .unwrap();
    let first = transfer.body.next_chunk(Duration::from_secs(5)).await;
    assert!(matches!(first, Ok(Some(_))));
    drop(transfer);

    assert!(
        wait_for_idle(&driver, Duration::from_secs(5)).await,
        "cancel must not leave the transfer in the multi handle"
    );
    assert_eq!(driver.active_transfers(), 0);
    assert_eq!(driver.submitted(), 1);
    assert_eq!(
        driver.completed(),
        0,
        "a cancelled transfer must not be reported as a completed one"
    );
    assert!(driver.is_alive());

    server.stop().await;
}

#[tokio::test]
async fn a_slow_consumer_does_not_block_another_transfer() {
    let slow_plan = ResponsePlan {
        chunk_delay: Duration::from_millis(15),
        chunk_size: 4096,
        ..ResponsePlan::body(vec![b's'; 256 * 1024])
    };
    let slow_server = TestServer::start(slow_plan).await;
    let fast_plan = ResponsePlan {
        chunk_delay: Duration::ZERO,
        ..ResponsePlan::body(vec![b'f'; 128])
    };
    let fast_server = TestServer::start(fast_plan).await;

    let driver = DriverHandle::spawn(DriverConfig::default());
    let slow = driver
        .get(RequestOptions::new(slow_server.url("/slow")), 8 * 1024)
        .await
        .unwrap();

    // The slow transfer fills its budget and pauses immediately; the fast
    // transfer must still complete promptly on the same driver.
    let started = Instant::now();
    let mut fast = driver
        .get(RequestOptions::new(fast_server.url("/fast")), 64 * 1024)
        .await
        .unwrap();
    let body = collect_body(&mut fast, Duration::from_secs(5)).await;
    let fast_elapsed = started.elapsed();

    assert_eq!(body, vec![b'f'; 128]);
    assert!(
        fast_elapsed < Duration::from_secs(2),
        "fast transfer waited {fast_elapsed:?} behind a paused one"
    );
    // The slow transfer fills its budget once the server has sent enough
    // chunks; observe the pause instead of racing it.
    let started = Instant::now();
    while slow.body.pause_count() == 0 && started.elapsed() < Duration::from_secs(5) {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        slow.body.pause_count() > 0,
        "the slow consumer must have paused its transfer"
    );

    drop(slow);
    assert!(wait_for_idle(&driver, Duration::from_secs(5)).await);

    slow_server.stop().await;
    fast_server.stop().await;
}

#[tokio::test]
async fn a_fully_paused_transfer_does_not_spin_the_driver() {
    // A write callback paused by backpressure leaves the transfer without
    // read interest: libcurl registers no socket that could make progress
    // for it. The wait then has nothing but the multi's wakeup socket, so a
    // wait that does not honour its timeout would spin the loop (this is the
    // pure-idle shape of P1 §4.4 with a live transfer attached). The driver
    // must sleep out its wait slice instead.
    let body: Vec<u8> = (0..262_144u32).map(|index| (index % 251) as u8).collect();
    let plan = ResponsePlan {
        chunk_size: 8 * 1024,
        ..ResponsePlan::body(body.clone())
    };
    let server = TestServer::start(plan).await;
    let driver = DriverHandle::spawn(DriverConfig::default());

    let mut transfer = driver
        .get(RequestOptions::new(server.url("/paused")), 8 * 1024)
        .await
        .unwrap();

    // Reading nothing fills the budget and pauses the transfer; it stays
    // paused as long as this test keeps not reading.
    let started = Instant::now();
    while transfer.body.pause_count() == 0 && started.elapsed() < Duration::from_secs(5) {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(
        transfer.body.pause_count() > 0,
        "an 8 KiB budget must pause the transfer"
    );
    // Let the driver reach its wait with the transfer already paused.
    tokio::time::sleep(Duration::from_millis(50)).await;

    let before = driver.stats().loops;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let loops = driver.stats().loops.saturating_sub(before);
    // A spinning driver runs hundreds of thousands of loops here; a bounded
    // wait runs about one per DRIVER_WAIT_SLICE.
    assert!(
        loops < 50,
        "a fully paused driver ran {loops} loops inside a 200 ms hold"
    );

    // The paused bytes are still delivered exactly once afterwards.
    let collected = collect_body(&mut transfer, Duration::from_secs(5)).await;
    assert_eq!(collected, body);
    assert_eq!(transfer.body.accepted_bytes(), body.len());
    assert_eq!(driver.completed(), 1);
    assert!(wait_for_idle(&driver, Duration::from_secs(2)).await);

    server.stop().await;
}

#[tokio::test]
async fn a_stalled_transfer_next_to_an_idle_pool_keeps_the_loop_bounded() {
    // The P1 §4.4 shape: one pool that finished its work (and now only keeps
    // cached connections) next to a pool whose transfer waits for a slow
    // origin. Waiting on the idle pool used to return immediately and spin
    // the loop in about half the rounds.
    let idle_plan = ResponsePlan::body(vec![b'i'; 4096]);
    let idle_server = TestServer::start(idle_plan).await;
    let stalled_plan = ResponsePlan {
        head_delay: Duration::from_millis(500),
        ..ResponsePlan::body(vec![b's'; 4096])
    };
    let stalled_server = TestServer::start(stalled_plan).await;

    let driver = DriverHandle::spawn(DriverConfig::default());
    // Complete one transfer first: its pool stays in the driver with a cached
    // connection and no active transfer.
    let mut idle_transfer = driver
        .get(RequestOptions::new(idle_server.url("/idle")), 64 * 1024)
        .await
        .unwrap();
    let idle_body = collect_body(&mut idle_transfer, Duration::from_secs(5)).await;
    assert_eq!(idle_body, vec![b'i'; 4096]);

    // The stalled request is in flight while its head is still delayed; the
    // holder keeps the driver reference alive for the transfer.
    let stalled = tokio::spawn({
        let driver = driver.clone();
        let url = stalled_server.url("/stalled");
        async move { driver.get(RequestOptions::new(url), 64 * 1024).await }
    });
    let started = Instant::now();
    while stalled_server.request_heads().is_empty() && started.elapsed() < Duration::from_secs(5) {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(
        !stalled_server.request_heads().is_empty(),
        "the stalled request never reached the origin"
    );

    let before = driver.stats().loops;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let loops = driver.stats().loops.saturating_sub(before);
    assert!(
        loops < 50,
        "the driver ran {loops} loops while an idle pool could be waited on"
    );

    // The stalled transfer still completes normally.
    let mut transfer = tokio::time::timeout(Duration::from_secs(5), stalled)
        .await
        .expect("the stalled transfer must resolve")
        .expect("the task must not panic")
        .unwrap();
    let stalled_body = collect_body(&mut transfer, Duration::from_secs(5)).await;
    assert_eq!(stalled_body, vec![b's'; 4096]);
    assert!(wait_for_idle(&driver, Duration::from_secs(2)).await);
    assert_eq!(driver.completed(), 2);

    idle_server.stop().await;
    stalled_server.stop().await;
}

#[tokio::test]
async fn body_errors_arrive_after_the_bytes_already_delivered() {
    let plan = ResponsePlan {
        truncate_at: Some(1024),
        chunk_delay: Duration::ZERO,
        ..ResponsePlan::body(vec![b't'; 8192])
    };
    let server = TestServer::start(plan).await;
    let driver = DriverHandle::spawn(DriverConfig::default());

    let mut transfer = driver
        .get(RequestOptions::new(server.url("/truncated")), 64 * 1024)
        .await
        .unwrap();
    assert_eq!(transfer.head.content_length(), Some(8192));

    let mut collected = Vec::new();
    let error = loop {
        match transfer.body.next_chunk(Duration::from_secs(5)).await {
            Ok(Some(chunk)) => collected.extend_from_slice(&chunk),
            Ok(None) => panic!("a truncated body must not look like a clean EOF"),
            Err(error) => break error,
        }
    };

    assert_eq!(collected, vec![b't'; 1024]);
    match error {
        DownloadError::Transport(transport) => {
            assert_eq!(transport.kind(), TransportErrorKind::Body);
            assert!(DownloadError::Transport(transport).is_retryable());
        }
        other => panic!("expected a body transport error, got {other:?}"),
    }

    server.stop().await;
}

#[tokio::test]
async fn options_are_applied_per_request() {
    let plan = ResponsePlan::body(b"hello".to_vec());
    let server = TestServer::start(plan).await;
    let driver = DriverHandle::spawn(DriverConfig::default());

    let mut options = RequestOptions::new(server.url("/options"));
    options.headers = vec![
        ("X-Trace".to_string(), "abc".to_string()),
        ("Accept-Language".to_string(), "zh-CN".to_string()),
    ];
    options.range = Some("0-3".to_string());
    let mut transfer = driver.get(options, 1024).await.unwrap();
    let body = collect_body(&mut transfer, Duration::from_secs(5)).await;

    let head = server.request_heads().join("\n").to_ascii_lowercase();
    assert!(head.contains("range: bytes=0-3"), "got: {head}");
    assert!(head.contains("x-trace: abc"), "got: {head}");
    assert!(head.contains("accept-language: zh-cn"), "got: {head}");
    assert!(head.contains("accept-encoding: identity"), "got: {head}");
    // The driver must not add headers the caller did not ask for.
    assert!(!head.contains("user-agent:"), "got: {head}");
    assert_eq!(body, b"hello");

    server.stop().await;
}

#[tokio::test]
async fn resolve_injects_this_hop_address_without_changing_the_host_header() {
    let plan = ResponsePlan::body(b"resolved".to_vec());
    let server = TestServer::start(plan).await;
    let driver = DriverHandle::spawn(DriverConfig::default());
    let port = server.addr.port();

    let mut options = RequestOptions::new(format!("http://spike.invalid:{port}/resolve"));
    options.resolve = Some(local_resolve("spike.invalid", port));
    let mut transfer = driver.get(options, 1024).await.unwrap();
    let body = collect_body(&mut transfer, Duration::from_secs(5)).await;

    let head = server.request_heads().join("\n").to_ascii_lowercase();
    assert!(head.contains("host: spike.invalid"), "got: {head}");
    assert_eq!(body, b"resolved");

    server.stop().await;
}

#[tokio::test]
async fn connect_stage_failure_is_reported_before_headers() {
    // Reserve a port and release it, so the connect attempt targets a
    // closed port without depending on a fixed one. Whether the socket is
    // refused immediately or only after the connect deadline depends on
    // the host, so both outcomes are accepted - but the failure has to
    // arrive as a typed transport error from the driver.
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let closed_port = listener.local_addr().unwrap().port();
    drop(listener);

    let driver = DriverHandle::spawn(DriverConfig::default());
    let mut options = RequestOptions::new(format!("http://spike.invalid:{closed_port}/fail"));
    options.resolve = Some(local_resolve("spike.invalid", closed_port));
    options.connect_timeout = Duration::from_millis(500);
    options.head_timeout = Duration::from_secs(5);

    let error = match driver.get(options, 1024).await {
        Ok(_) => panic!("a refused connection must not produce a response"),
        Err(error) => error,
    };
    match error {
        DownloadError::Transport(transport) => {
            assert!(
                matches!(
                    transport.kind(),
                    TransportErrorKind::Connect | TransportErrorKind::Timeout
                ),
                "unexpected transport error: {transport}"
            );
            // The driver's own failure message must arrive, not the
            // "headers timed out" path and not a silent EOF.
            let message = format!("{transport}");
            assert!(message.contains("transfer "), "got: {message}");
            assert!(!message.contains("headers timed out"), "got: {message}");
        }
        other => panic!("expected a connect-stage error, got {other:?}"),
    }
    assert!(wait_for_idle(&driver, Duration::from_secs(2)).await);
    assert_eq!(driver.active_transfers(), 0);
}

#[tokio::test]
async fn deterministic_pre_header_failures_are_not_retryable() {
    let driver = DriverHandle::spawn(DriverConfig::default());
    // `telnet` is compiled out of the vendored libcurl build, which gives a
    // deterministic, non-transient failure before any header arrives.
    let options = RequestOptions::new("telnet://127.0.0.1:23/");
    let error = match driver.get(options, 1024).await {
        Ok(_) => panic!("an unsupported protocol must not produce a response"),
        Err(error) => error,
    };
    match error {
        DownloadError::Transport(transport) => {
            assert_eq!(transport.kind(), TransportErrorKind::Other);
            assert!(
                !DownloadError::Transport(transport).is_retryable(),
                "an unsupported protocol must not be retried"
            );
        }
        other => panic!("expected a non-retryable transport error, got {other:?}"),
    }
    assert!(wait_for_idle(&driver, Duration::from_secs(2)).await);
    assert_eq!(driver.active_transfers(), 0);
}

#[tokio::test]
async fn forbid_connection_reuse_opens_a_connection_per_request() {
    let plan = ResponsePlan::body(b"pooled".to_vec());
    let server = TestServer::start(plan).await;
    let driver = DriverHandle::spawn(DriverConfig::default());

    for _ in 0..2 {
        let mut options = RequestOptions::new(server.url("/no-reuse"));
        options.forbid_connection_reuse = true;
        let mut transfer = driver.get(options, 1024).await.unwrap();
        let body = collect_body(&mut transfer, Duration::from_secs(5)).await;
        assert_eq!(body, b"pooled");
    }
    assert_eq!(server.accepted(), 2);

    for _ in 0..2 {
        let mut transfer = driver
            .get(RequestOptions::new(server.url("/reuse")), 1024)
            .await
            .unwrap();
        let _ = collect_body(&mut transfer, Duration::from_secs(5)).await;
    }
    assert_eq!(
        server.accepted(),
        3,
        "pooled requests must reuse the socket"
    );

    server.stop().await;
}

#[tokio::test]
async fn proxy_option_routes_absolute_form_requests() {
    let plan = ResponsePlan::body(b"via-proxy".to_vec());
    let proxy = TestServer::start(plan).await;
    let driver = DriverHandle::spawn(DriverConfig::default());

    let mut options = RequestOptions::new("http://origin.invalid/proxied");
    options.proxy = Some(format!("http://127.0.0.1:{}", proxy.addr.port()));
    let mut transfer = driver.get(options, 4096).await.unwrap();
    let body = collect_body(&mut transfer, Duration::from_secs(5)).await;

    let head = proxy.request_heads().join("\n");
    assert!(
        head.starts_with("GET http://origin.invalid/proxied"),
        "proxy must receive an absolute-form request line, got: {head}"
    );
    assert_eq!(body, b"via-proxy");

    proxy.stop().await;
}

#[tokio::test]
async fn driver_failure_fails_waiters_instead_of_truncating() {
    let plan = ResponsePlan {
        chunk_delay: Duration::from_millis(50),
        chunk_size: 4096,
        ..ResponsePlan::body(vec![b'q'; 256 * 1024])
    };
    let server = TestServer::start(plan).await;
    let driver = DriverHandle::spawn(DriverConfig::default());

    let mut transfer = driver
        .get(RequestOptions::new(server.url("/driver-fails")), 1024)
        .await
        .unwrap();

    // Simulate the driver thread dying: fail every registered waiter the
    // same way `ExitGuard` does.
    driver.shared.fail_all(Terminal::Failed {
        kind: TransportErrorKind::Other,
        code: None,
        message: "libcurl driver exited".into(),
    });

    let error = loop {
        match transfer.body.next_chunk(Duration::from_secs(1)).await {
            Ok(Some(_)) => continue,
            Ok(None) => panic!("driver exit must not look like a clean EOF"),
            Err(error) => break error,
        }
    };
    assert!(format!("{error}").contains("driver exited"), "got: {error}");

    server.stop().await;
}

#[test]
fn terminal_mapping_is_explicit() {
    assert!(Terminal::Eof.into_error().is_none());
    let cancelled = Terminal::Cancelled.into_error().unwrap();
    assert!(matches!(cancelled, DownloadError::Cancelled));
    assert!(
        !cancelled.is_retryable(),
        "a cancelled transfer must never be retried as a transient failure"
    );
    let failed = Terminal::Failed {
        kind: TransportErrorKind::Body,
        code: Some(56),
        message: "recv failure".into(),
    }
    .into_error()
    .unwrap();
    assert!(failed.is_retryable());
}

#[test]
fn curl_failure_classification_covers_retryable_stages() {
    // Transient stages: a retry can succeed.
    assert_eq!(classify_curl_failure(5), TransportErrorKind::Connect);
    assert_eq!(classify_curl_failure(6), TransportErrorKind::Connect);
    assert_eq!(classify_curl_failure(7), TransportErrorKind::Connect);
    assert_eq!(classify_curl_failure(35), TransportErrorKind::Connect);
    assert_eq!(classify_curl_failure(28), TransportErrorKind::Timeout);
    assert_eq!(classify_curl_failure(18), TransportErrorKind::Body);
    assert_eq!(classify_curl_failure(55), TransportErrorKind::Body);
    assert_eq!(classify_curl_failure(56), TransportErrorKind::Body);

    // Deterministic failures must stay non-retryable: a rejected certificate,
    // a hostname mismatch, a bad URL, an unsupported protocol and a transfer
    // aborted by a callback cannot succeed on a second attempt.
    for code in [1, 2, 3, 9, 42, 51, 58, 59, 60, 64, 66, 80, 82, 83, 90, 91] {
        assert_eq!(
            classify_curl_failure(code),
            TransportErrorKind::Other,
            "curl code {code} must not be retryable"
        );
        let error = DownloadError::Transport(TransportError::new(
            classify_curl_failure(code),
            std::io::Error::other("deterministic"),
        ));
        assert!(!error.is_retryable(), "curl code {code} must not retry");
    }

    // Unknown codes are not guessed into the retryable set either.
    assert_eq!(classify_curl_failure(9999), TransportErrorKind::Other);
}

#[test]
fn header_parser_publishes_duplicate_headers_once() {
    let (tx, mut rx) = oneshot::channel();
    let mut parser = HeadParser::new(tx, false);
    parser.push(b"HTTP/1.1 206 Partial Content\r\n");
    parser.push(b"Content-Range: bytes 0-9/100\r\n");
    parser.push(b"Set-Cookie: a=1\r\n");
    parser.push(b"Set-Cookie: b=2\r\n");
    parser.push(b"\r\n");
    // Trailers arriving later must not publish a second head.
    parser.push(b"X-Trailer: 1\r\n");
    parser.push(b"\r\n");

    let head = rx.try_recv().unwrap().unwrap();
    assert_eq!(head.status, 206);
    assert_eq!(head.header("content-range"), Some("bytes 0-9/100"));
    assert_eq!(
        head.headers
            .iter()
            .filter(|(name, _)| name == "set-cookie")
            .count(),
        2
    );
    assert!(rx.try_recv().is_err());
}

#[test]
fn header_parser_skips_informational_responses() {
    let (tx, mut rx) = oneshot::channel();
    let mut parser = HeadParser::new(tx, false);
    parser.push(b"HTTP/1.1 100 Continue\r\n");
    parser.push(b"\r\n");
    parser.push(b"HTTP/1.1 103 Early Hints\r\n");
    parser.push(b"Link: </style.css>; rel=preload\r\n");
    parser.push(b"\r\n");
    assert!(
        rx.try_recv().is_err(),
        "1xx must not become the response head"
    );
    parser.push(b"HTTP/1.1 200 OK\r\n");
    parser.push(b"Content-Length: 4\r\n");
    parser.push(b"\r\n");

    let head = rx.try_recv().unwrap().unwrap();
    assert_eq!(head.status, 200);
    assert_eq!(head.content_length(), Some(4));
}

#[test]
fn header_parser_skips_the_proxy_connect_block() {
    let (tx, mut rx) = oneshot::channel();
    let mut parser = HeadParser::new(tx, true);
    parser.push(b"HTTP/1.1 200 Connection established\r\n");
    parser.push(b"\r\n");
    assert!(rx.try_recv().is_err(), "the CONNECT answer is not the file");
    parser.push(b"HTTP/1.1 206 Partial Content\r\n");
    parser.push(b"Content-Range: bytes 0-3/8\r\n");
    parser.push(b"\r\n");

    let head = rx.try_recv().unwrap().unwrap();
    assert_eq!(head.status, 206);
}

#[test]
fn header_parser_bounds_the_published_header_block() {
    let (tx, mut rx) = oneshot::channel();
    let mut parser = HeadParser::new(tx, false);
    parser.push(b"HTTP/1.1 200 OK\r\n");
    // Feed a block larger than the bound without ever finishing it.
    let line = format!("X-Padding: {}\r\n", "a".repeat(8 * 1024));
    let mut published = None;
    for _ in 0..24 {
        parser.push(line.as_bytes());
        if let Ok(result) = rx.try_recv() {
            published = Some(result);
            break;
        }
    }

    let error = published
        .expect("an oversized header block must fail the head")
        .unwrap_err();
    assert!(
        format!("{error}").contains("headers exceeded"),
        "got: {error}"
    );
    // Once the head failed, further lines are ignored instead of published.
    parser.push(b"\r\n");
    assert!(parser.published());
    assert!(rx.try_recv().is_err());
}

#[test]
fn header_parser_rejects_an_unparsable_status_line() {
    let (tx, mut rx) = oneshot::channel();
    let mut parser = HeadParser::new(tx, false);
    // A stray blank line outside any block is ignored, not an error.
    parser.push(b"\r\n");
    assert!(rx.try_recv().is_err());
    parser.push(b"NOT-HTTP garbage\r\n");
    parser.push(b"\r\n");

    let error = rx.try_recv().unwrap().unwrap_err();
    assert!(
        format!("{error}").contains("could not be parsed"),
        "got: {error}"
    );
}

#[test]
fn body_sink_pause_keeps_the_chunk_out_of_the_queue() {
    let queue = CommandQueue::new();
    let sink = BodySink::new(TransferId(1), 8, Arc::downgrade(&queue));

    assert_eq!(sink.write(b"123456").unwrap(), 6);
    // The next chunk does not fit the remaining budget: pause without
    // enqueueing anything, so libcurl can redeliver it later.
    assert!(matches!(sink.write(b"7890"), Err(WriteError::Pause)));
    assert_eq!(sink.buffered(), 6);
    assert_eq!(sink.pause_count(), 1);
    assert_eq!(sink.pop_chunk().unwrap(), Bytes::from_static(b"123456"));
    // An oversized chunk is still accepted when nothing is buffered,
    // otherwise the transfer could never make progress.
    assert_eq!(sink.write(&[9u8; 64]).unwrap(), 64);
    assert_eq!(sink.buffered(), 64);
    assert_eq!(sink.pop_chunk().unwrap().len(), 64);
    assert!(queue.take_commands().is_empty());
}

#[test]
fn body_sink_resume_is_requested_once_the_queue_drains() {
    let queue = CommandQueue::new();
    let sink = BodySink::new(TransferId(7), 8, Arc::downgrade(&queue));

    assert_eq!(sink.write(b"123456").unwrap(), 6);
    assert!(matches!(sink.write(b"7890"), Err(WriteError::Pause)));
    assert!(!sink.needs_resume(), "budget is still full");
    assert_eq!(sink.pop_chunk().unwrap(), Bytes::from_static(b"123456"));
    assert!(sink.needs_resume());
    assert!(!sink.needs_resume(), "resume is only reported once");

    assert!(
        queue.take_commands().is_empty(),
        "the sink must not send a resume command before the consumer drains"
    );
    sink.request_resume();
    assert!(
        matches!(queue.take_commands().as_slice(), [Command::Resume(id)] if *id == TransferId(7)),
        "the consumer must request a resume after draining the queue"
    );
}

#[test]
fn body_sink_terminal_state_is_reported_after_queued_bytes() {
    let queue = CommandQueue::new();
    let sink = BodySink::new(TransferId(2), 1024, Arc::downgrade(&queue));
    sink.write(b"tail").unwrap();
    sink.finish(Terminal::Eof);

    // One atomic observation: queued data wins over the terminal state.
    assert!(matches!(sink.poll(), SinkPoll::Chunk(chunk) if chunk == Bytes::from_static(b"tail")));
    assert!(matches!(sink.poll(), SinkPoll::Terminal(Terminal::Eof)));
    assert!(matches!(sink.poll(), SinkPoll::Terminal(Terminal::Eof)));
    // A second terminal state must not overwrite the first one.
    sink.finish(Terminal::Cancelled);
    assert_eq!(sink.take_terminal(), Some(Terminal::Eof));
}

#[test]
fn body_sink_reports_a_failure_only_after_the_queued_prefix() {
    let queue = CommandQueue::new();
    let sink = BodySink::new(TransferId(10), 1024, Arc::downgrade(&queue));
    // The driver accepted these bytes and then failed: the contract is
    // "valid prefix first, then the error", never a bare error.
    sink.write(b"prefix").unwrap();
    sink.finish(Terminal::Failed {
        kind: TransportErrorKind::Body,
        code: Some(56),
        message: "recv failure".into(),
    });

    assert!(
        matches!(sink.poll(), SinkPoll::Chunk(chunk) if chunk == Bytes::from_static(b"prefix"))
    );
    assert!(matches!(
        sink.poll(),
        SinkPoll::Terminal(Terminal::Failed { code: Some(56), .. })
    ));
}

/// The exact interleaving that a split check cannot survive.
///
/// The window between "queue is empty" and "is there a terminal state" is a few
/// instructions wide, so it cannot be reproduced by racing two tasks reliably.
/// This test writes it out step by step instead: it shows what the previous
/// two-step read returned at that point (EOF, dropping `tail`) and asserts what
/// the single atomic observation returns (the bytes, then the EOF).
#[test]
fn a_two_step_read_would_lose_the_tail_that_poll_delivers() {
    let queue = CommandQueue::new();
    let sink = BodySink::new(TransferId(12), 1024, Arc::downgrade(&queue));

    // Step 1 of the old shape: the consumer looks at the queue first.
    assert!(sink.pop_chunk().is_none());

    // The driver now enqueues the last chunk and finishes the transfer.
    sink.write(b"tail").unwrap();
    sink.finish(Terminal::Eof);

    // Step 2 of the old shape: it looks at the terminal state and stops there,
    // so the chunk that arrived in between is never delivered.
    assert_eq!(sink.take_terminal(), Some(Terminal::Eof));

    // The production observation decides both questions under one lock, so the
    // accepted bytes are delivered before the EOF.
    assert!(matches!(sink.poll(), SinkPoll::Chunk(chunk) if chunk == Bytes::from_static(b"tail")));
    assert!(matches!(sink.poll(), SinkPoll::Terminal(Terminal::Eof)));
}

/// Production interleaves with reads, so this exercises the read loop against a
/// concurrently writing driver: every accepted byte must still arrive, in
/// order, before the EOF.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_production_never_loses_the_tail_before_eof() {
    const CHUNKS: usize = 16;
    const CHUNK: &[u8] = b"0123456789";

    for iteration in 0..300 {
        let queue = CommandQueue::new();
        let sink = BodySink::new(TransferId(11), 4096, Arc::downgrade(&queue));
        let mut stream = BodyStream {
            sink: sink.clone(),
            id: TransferId(11),
            queue: queue.clone(),
            saw_eof: false,
        };

        // The consumer starts first so it is parked or mid-check when the
        // producer writes the tail chunk and the terminal state.
        let consumer = tokio::spawn(async move {
            let mut seen = Vec::new();
            loop {
                match stream.next_chunk(Duration::from_secs(5)).await {
                    Ok(Some(chunk)) => seen.extend_from_slice(&chunk),
                    Ok(None) => break,
                    Err(error) => panic!("unexpected body error: {error}"),
                }
            }
            seen
        });

        for _ in 0..CHUNKS {
            sink.write(CHUNK).unwrap();
            tokio::task::yield_now().await;
        }
        sink.finish(Terminal::Eof);

        let seen = consumer.await.unwrap();
        assert_eq!(
            seen.len(),
            CHUNKS * CHUNK.len(),
            "iteration {iteration} lost bytes before EOF"
        );
        assert!(seen.chunks(CHUNK.len()).all(|chunk| chunk == CHUNK));
    }
}

#[test]
fn body_sink_discards_buffered_bytes_on_cancel() {
    let queue = CommandQueue::new();
    let sink = BodySink::new(TransferId(3), 1024, Arc::downgrade(&queue));
    sink.write(b"queued").unwrap();
    sink.discard_buffer();
    sink.finish(Terminal::Cancelled);

    assert_eq!(sink.buffered(), 0);
    assert!(sink.pop_chunk().is_none());
    assert_eq!(sink.take_terminal(), Some(Terminal::Cancelled));
}

#[test]
fn body_sink_rejects_writes_after_a_terminal_state() {
    let queue = CommandQueue::new();
    let sink = BodySink::new(TransferId(4), 1024, Arc::downgrade(&queue));
    sink.discard_buffer();
    sink.finish(Terminal::Cancelled);

    assert!(matches!(sink.write(b"late"), Err(WriteError::Pause)));
    assert_eq!(sink.buffered(), 0);
}

#[tokio::test]
async fn body_stream_reports_a_clean_eof() {
    let queue = CommandQueue::new();
    let sink = BodySink::new(TransferId(5), 1024, Arc::downgrade(&queue));
    sink.write(b"done").unwrap();
    sink.finish(Terminal::Eof);
    let mut stream = BodyStream {
        sink,
        id: TransferId(5),
        queue: queue.clone(),
        saw_eof: false,
    };

    assert_eq!(
        stream.next_chunk(Duration::from_millis(50)).await.unwrap(),
        Some(Bytes::from_static(b"done"))
    );
    assert!(stream
        .next_chunk(Duration::from_millis(50))
        .await
        .unwrap()
        .is_none());
    assert!(stream.saw_eof());
}

#[tokio::test]
async fn body_stream_times_out_without_any_progress() {
    let queue = CommandQueue::new();
    let sink = BodySink::new(TransferId(6), 1024, Arc::downgrade(&queue));
    let mut stream = BodyStream {
        sink,
        id: TransferId(6),
        queue: queue.clone(),
        saw_eof: false,
    };

    let error = stream
        .next_chunk(Duration::from_millis(30))
        .await
        .unwrap_err();
    match error {
        DownloadError::Transport(transport) => {
            assert_eq!(transport.kind(), TransportErrorKind::Timeout);
        }
        other => panic!("expected a timeout transport error, got {other:?}"),
    }
}

#[tokio::test]
async fn requests_against_a_dead_driver_fail_explicitly() {
    let driver = DriverHandle::spawn(DriverConfig::default());
    // Model a driver thread that already left its loop: the queue is closed
    // and the liveness flag is down, exactly what `ExitGuard` leaves behind.
    driver.queue.close();
    driver.shared.alive.store(false, Ordering::SeqCst);

    let error = match driver
        .get(RequestOptions::new("http://127.0.0.1:1/"), 1024)
        .await
    {
        Ok(_) => panic!("a stopped driver cannot serve a request"),
        Err(error) => error,
    };
    assert!(format!("{error}").contains("driver is not running"));
}

// ---------------------------------------------------------------------------
// P3: pools, resolved addresses and diagnostics
// ---------------------------------------------------------------------------

/// Serves one body on every request of every accepted connection.
async fn serve_body_forever(listener: TcpListener, body: Vec<u8>, accepted: Arc<AtomicUsize>) {
    loop {
        let Ok((mut stream, _)) = listener.accept().await else {
            return;
        };
        accepted.fetch_add(1, Ordering::SeqCst);
        let body = body.clone();
        tokio::spawn(async move {
            loop {
                if read_request_head(&mut stream).await.is_none() {
                    return;
                }
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n",
                    body.len()
                );
                if stream.write_all(response.as_bytes()).await.is_err() {
                    return;
                }
                if stream.write_all(&body).await.is_err() {
                    return;
                }
                if stream.flush().await.is_err() {
                    return;
                }
            }
        });
    }
}

#[test]
fn pool_keys_separate_origins_and_proxy_routes() {
    let direct = RequestOptions::new("http://Example.test:8080/a").pool_key();
    let same_origin = RequestOptions::new("http://example.test:8080/another/path").pool_key();
    assert_eq!(
        direct, same_origin,
        "the pool key is origin-level: paths share one pool"
    );
    assert_eq!(direct.origin(), "http://example.test:8080");

    let other_port = RequestOptions::new("http://example.test:8081/a").pool_key();
    assert_ne!(direct, other_port);

    let https = RequestOptions::new("https://example.test:8080/a").pool_key();
    assert_ne!(
        direct, https,
        "the scheme and its default port are part of it"
    );

    let mut proxied = RequestOptions::new("http://example.test:8080/a");
    proxied.proxy = Some("http://proxy.test:3128/".into());
    let mut other_proxy = RequestOptions::new("http://example.test:8080/a");
    other_proxy.proxy = Some("http://proxy2.test:3128/".into());
    assert_ne!(direct, proxied.pool_key(), "a proxy route is its own pool");
    assert_ne!(
        proxied.pool_key(),
        other_proxy.pool_key(),
        "different proxy endpoints never share cached connections"
    );
}

#[test]
fn resolve_entries_render_ipv6_addresses_in_brackets() {
    let entry = ResolveEntry::new(
        "v6.test",
        443,
        &[std::net::IpAddr::from([0, 0, 0, 0, 0, 0, 0, 1])],
        Duration::from_secs(1),
    );
    assert_eq!(entry.spec, "v6.test:443:[::1]");
    assert_eq!(entry.key, "v6.test:443");
}

#[test]
fn connect_to_entries_name_one_address_of_the_request_origin() {
    let ipv4 = ConnectToEntry::new("download.test", 8080, [127, 0, 0, 2].into(), 8080);
    assert_eq!(ipv4.spec, "download.test:8080:127.0.0.2:8080");
    assert_eq!(ipv4.address, std::net::IpAddr::from([127, 0, 0, 2]));

    // The target port is the port of the address, not of the origin: a proxy
    // or a re-mapped port must survive the rendering.
    let remapped = ConnectToEntry::new("download.test", 443, [127, 0, 0, 1].into(), 8443);
    assert_eq!(remapped.spec, "download.test:443:127.0.0.1:8443");

    let ipv6 = ConnectToEntry::new("download.test", 80, [0, 0, 0, 0, 0, 0, 0, 1].into(), 80);
    assert_eq!(ipv6.spec, "download.test:80:[::1]:80");
}

#[test]
fn resolve_plans_inject_once_and_refresh_stale_or_changed_answers() {
    let mut pool = Pool::new(&DriverConfig::default());
    let now = Instant::now();
    let entry = ResolveEntry::new(
        "cache.test",
        80,
        &[std::net::IpAddr::from([127, 0, 0, 1])],
        Duration::from_secs(30),
    );

    let first = pool.plan_resolve(Some(&entry), now);
    assert_eq!(first.list, vec!["cache.test:80:127.0.0.1".to_string()]);
    assert!(!first.force_fresh_connect);
    pool.commit_resolve(Some(&entry), true, now);

    // Still fresh: libcurl holds the entry in this pool's DNS cache, so
    // re-sending it on every request would be pointless traffic.
    assert_eq!(
        pool.plan_resolve(Some(&entry), now + Duration::from_secs(5)),
        ResolvePlan::default()
    );

    // The TTL passed: the answer is replaced, keyed by `host:port` so libcurl
    // drops the previous entry instead of keeping both.
    let refreshed = pool.plan_resolve(Some(&entry), now + Duration::from_secs(31));
    assert_eq!(
        refreshed.list,
        vec![
            "-cache.test:80".to_string(),
            "cache.test:80:127.0.0.1".to_string()
        ]
    );
    assert!(
        !refreshed.force_fresh_connect,
        "the same address may keep its pooled connection"
    );
    pool.commit_resolve(Some(&entry), true, now + Duration::from_secs(31));

    // A different answer must not ride a connection opened to the old one.
    let moved = ResolveEntry::new(
        "cache.test",
        80,
        &[std::net::IpAddr::from([127, 0, 0, 2])],
        Duration::from_secs(30),
    );
    let changed = pool.plan_resolve(Some(&moved), now + Duration::from_secs(32));
    assert_eq!(
        changed.list,
        vec![
            "-cache.test:80".to_string(),
            "cache.test:80:127.0.0.2".to_string()
        ]
    );
    assert!(changed.force_fresh_connect);
}

#[test]
fn an_ip_literal_needs_no_resolve_entry() {
    let pool = Pool::new(&DriverConfig::default());
    assert_eq!(
        pool.plan_resolve(None, Instant::now()),
        ResolvePlan::default()
    );
}

#[test]
fn an_uncommitted_plan_does_not_claim_the_answer_was_injected() {
    let mut pool = Pool::new(&DriverConfig::default());
    let now = Instant::now();
    let entry = ResolveEntry::new(
        "never-added.test",
        80,
        &[std::net::IpAddr::from([127, 0, 0, 1])],
        Duration::from_secs(30),
    );

    // `add2` failed, so nothing reached the multi's DNS cache; the next
    // request has to plan the injection again.
    pool.commit_resolve(Some(&entry), false, now);
    assert_eq!(pool.plan_resolve(Some(&entry), now).list.len(), 1);
}

#[tokio::test]
async fn a_changed_resolve_answer_opens_a_new_connection() {
    let first_listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let port = first_listener.local_addr().unwrap().port();
    // A second loopback address is available on Linux and Windows; macOS only
    // configures 127.0.0.1, where this part of the contract cannot be exercised.
    let Ok(second_listener) = TcpListener::bind(("127.0.0.2", port)).await else {
        return;
    };

    let first_accepted = Arc::new(AtomicUsize::new(0));
    let second_accepted = Arc::new(AtomicUsize::new(0));
    let first_task = tokio::spawn(serve_body_forever(
        first_listener,
        b"from-127-0-0-1".to_vec(),
        first_accepted.clone(),
    ));
    let second_task = tokio::spawn(serve_body_forever(
        second_listener,
        b"from-127-0-0-2".to_vec(),
        second_accepted.clone(),
    ));

    let driver = DriverHandle::spawn(DriverConfig::default());
    let url = format!("http://switch.test:{port}/file");
    for (address, expected) in [
        ([127u8, 0, 0, 1], &b"from-127-0-0-1"[..]),
        ([127, 0, 0, 2], &b"from-127-0-0-2"[..]),
    ] {
        let mut options = RequestOptions::new(&url);
        options.resolve = Some(ResolveEntry::new(
            "switch.test",
            port,
            &[std::net::IpAddr::from(address)],
            Duration::from_secs(600),
        ));
        let mut transfer = driver.get(options, 4096).await.unwrap();
        assert_eq!(
            collect_body(&mut transfer, Duration::from_secs(5)).await,
            expected,
            "the transfer must reach the address it was told to use"
        );
    }

    assert_eq!(
        first_accepted.load(Ordering::SeqCst),
        1,
        "the connection pooled for the old address must not serve the new one"
    );
    assert_eq!(second_accepted.load(Ordering::SeqCst), 1);
    first_task.abort();
    second_task.abort();
}

#[tokio::test]
async fn idle_pools_close_their_sockets_after_the_idle_timeout() {
    // `CURLOPT_MAXAGE_CONN` only checks the age when a connection is reused,
    // so the pool itself has to be reclaimed for the socket to close with no
    // further request. This server observes exactly that close.
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (closed_tx, closed_rx) = oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        let Ok((mut stream, _)) = listener.accept().await else {
            return;
        };
        if read_request_head(&mut stream).await.is_none() {
            return;
        }
        if stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: keep-alive\r\n\r\npong",
            )
            .await
            .is_err()
        {
            return;
        }
        let _ = stream.flush().await;
        let mut byte = [0u8];
        loop {
            match stream.read(&mut byte).await {
                Ok(0) | Err(_) => {
                    let _ = closed_tx.send(());
                    return;
                }
                Ok(_) => continue,
            }
        }
    });

    let driver = DriverHandle::spawn(DriverConfig {
        max_idle_per_host: 1,
        pool_idle_timeout: Duration::from_millis(150),
        max_age_conn: None,
    });
    let mut transfer = driver
        .get(RequestOptions::new(format!("http://{addr}/idle")), 4096)
        .await
        .unwrap();
    assert_eq!(
        collect_body(&mut transfer, Duration::from_secs(5)).await,
        b"pong"
    );

    // The driver stays alive: the close has to come from idle reclamation.
    tokio::time::timeout(Duration::from_secs(5), closed_rx)
        .await
        .expect("an idle pool must close its sockets after pool_idle_timeout")
        .unwrap();
    drop(transfer);
    server.abort();
}

#[tokio::test]
async fn a_pool_that_may_not_reuse_connections_keeps_none_idle() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let closed = Arc::new(AtomicUsize::new(0));
    let closed_task = closed.clone();
    let server = tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let closed = closed_task.clone();
            tokio::spawn(async move {
                if read_request_head(&mut stream).await.is_none() {
                    return;
                }
                let _ = stream
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\npong")
                    .await;
                let _ = stream.flush().await;
                let mut byte = [0u8];
                loop {
                    match stream.read(&mut byte).await {
                        Ok(0) | Err(_) => {
                            closed.fetch_add(1, Ordering::SeqCst);
                            return;
                        }
                        Ok(_) => continue,
                    }
                }
            });
        }
    });

    // `pool_max_idle_per_host == 0` keeps the pre-P3 "no reuse" contract.
    let driver = DriverHandle::spawn(DriverConfig {
        max_idle_per_host: 0,
        pool_idle_timeout: Duration::from_secs(30),
        max_age_conn: None,
    });
    for _ in 0..2 {
        let mut options = RequestOptions::new(format!("http://{addr}/fresh"));
        options.forbid_connection_reuse = true;
        let mut transfer = driver.get(options, 4096).await.unwrap();
        assert_eq!(
            collect_body(&mut transfer, Duration::from_secs(5)).await,
            b"pong"
        );
    }

    // The contract is that no socket remains idle. Observe that at the peer
    // instead of using `stats().pools` as a synchronization barrier: the body
    // EOF is published just before the driver's next pass updates that
    // diagnostic counter, and the full parallel suite can widen that window.
    let reclaimed = tokio::time::timeout(Duration::from_secs(2), async {
        while closed.load(Ordering::SeqCst) != 2 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
    assert!(
        reclaimed.is_ok(),
        "a pool with reuse disabled must close every completed connection; closed={}",
        closed.load(Ordering::SeqCst)
    );
    server.abort();
}

#[tokio::test]
async fn driver_stats_separate_requests_from_connections() {
    let plan = ResponsePlan::body(b"stats".to_vec());
    let server = TestServer::start(plan).await;
    let driver = DriverHandle::spawn(DriverConfig::default());

    for _ in 0..2 {
        let mut transfer = driver
            .get(RequestOptions::new(server.url("/stats")), 4096)
            .await
            .unwrap();
        assert_eq!(
            collect_body(&mut transfer, Duration::from_secs(5)).await,
            b"stats"
        );
    }

    let stats = driver.stats();
    assert_eq!(stats.submitted, 2);
    assert_eq!(stats.completed, 2);
    assert_eq!(
        stats.connections, 1,
        "the second request must reuse the pooled connection, so it opens none"
    );
    assert_eq!(stats.pools, 1);
    assert!(stats.commands >= 2);
    assert!(
        stats.max_command_latency < Duration::from_secs(2),
        "command latency: {:?}",
        stats.max_command_latency
    );
    server.stop().await;
}

#[tokio::test]
async fn pools_are_counted_per_origin() {
    let first = TestServer::start(ResponsePlan::body(b"one".to_vec())).await;
    let second = TestServer::start(ResponsePlan::body(b"two".to_vec())).await;
    let driver = DriverHandle::spawn(DriverConfig::default());

    for (server, expected) in [(&first, &b"one"[..]), (&second, &b"two"[..])] {
        let mut transfer = driver
            .get(RequestOptions::new(server.url("/origin")), 4096)
            .await
            .unwrap();
        assert_eq!(
            collect_body(&mut transfer, Duration::from_secs(5)).await,
            expected
        );
    }

    let stats = driver.stats();
    assert_eq!(stats.pools, 2, "each origin keeps its own connection pool");
    assert_eq!(stats.connections, 2);
    first.stop().await;
    second.stop().await;
}

/// Waits for the live driver-thread count to fall back to `baseline`.
///
/// The count is process-wide, so it is compared with `<=`: other tests of this
/// binary may add driver threads while this one waits.
async fn wait_for_live_threads(baseline: i64, timeout: Duration) -> i64 {
    let deadline = Instant::now() + timeout;
    let mut live = crate::bench_stats::driver_threads();
    while live > baseline && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(5)).await;
        live = crate::bench_stats::driver_threads();
    }
    live
}

/// The driver thread is kept alive by the references to its command queue, so
/// it has to stop as soon as the last one is gone.
///
/// Regression: the exit guard used to hold a *strong* reference to the queue
/// from inside the thread, which made `Arc::strong_count == 1` unreachable.
/// The thread then never stopped, and a program that created and dropped
/// clients accumulated one driver thread per client.
#[tokio::test]
async fn the_driver_thread_stops_with_the_last_reference() {
    let baseline = crate::bench_stats::driver_threads();
    let driver = DriverHandle::spawn(DriverConfig::default());
    let shared = driver.shared.clone();
    assert!(shared.alive.load(Ordering::SeqCst));
    drop(driver);

    let stopped = tokio::time::timeout(Duration::from_secs(5), async {
        while shared.alive.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
    assert!(
        stopped.is_ok(),
        "the driver loop must leave as soon as nothing references it"
    );
    let live = wait_for_live_threads(baseline, Duration::from_secs(5)).await;
    assert!(
        live <= baseline,
        "the driver thread must have terminated, {live} driver threads are live"
    );
}

/// The pool keeps one idle date per cached connection.
///
/// Regression: the driver kept a single timestamp per pool and reset it on
/// every admission and completion, so a request that reused one cached
/// connection postponed the reclamation of the connections nobody touched, and
/// a pool with a long transfer could keep its idle sockets forever.
#[test]
fn reusing_one_connection_keeps_the_other_idle_deadlines() {
    let config = DriverConfig {
        max_idle_per_host: 4,
        pool_idle_timeout: Duration::from_millis(100),
        max_age_conn: None,
    };
    let mut pool = Pool::new(&config);
    let generation = pool.clear_generation;
    let start = Instant::now();

    // Two connections went idle 10 ms apart.
    pool.add_idle_connections(1, start);
    pool.add_idle_connections(1, start + Duration::from_millis(10));
    assert_eq!(pool.idle_connections(), 2);
    assert_eq!(pool.oldest_idle(), Some(start));

    // A request joins the pool and reuses the older one.
    let claim = pool.claim_idle_connection();
    assert_eq!(claim.map(|claim| claim.since), Some(start));
    assert_eq!(pool.idle_connections(), 1);

    // It finishes later: the connection it used is cached again with a fresh
    // date, while the one nobody touched keeps its own.
    pool.settle_connection(
        generation,
        claim,
        0,
        TransferExit::Completed,
        start + Duration::from_millis(50),
    );
    assert_eq!(pool.idle_connections(), 2);
    assert_eq!(
        pool.oldest_idle(),
        Some(start + Duration::from_millis(10)),
        "a request must not postpone the deadline of the connections it did not use"
    );

    // A request that opens its own connection instead of reusing one hands the
    // claim back, so that deadline is not lost either.
    let claim = pool.claim_idle_connection();
    assert_eq!(
        claim.map(|claim| claim.since),
        Some(start + Duration::from_millis(10))
    );
    pool.settle_connection(
        generation,
        claim,
        1,
        TransferExit::Completed,
        start + Duration::from_millis(60),
    );
    assert_eq!(pool.idle_connections(), 3);
    assert_eq!(pool.oldest_idle(), Some(start + Duration::from_millis(10)));

    // A failed transfer closes the connection it was using, so its entry is
    // gone; an aborted one gives a claim it never used back.
    let failed = pool.claim_idle_connection();
    pool.settle_connection(
        generation,
        failed,
        0,
        TransferExit::Failed,
        start + Duration::from_millis(70),
    );
    assert_eq!(pool.idle_connections(), 2);
    let aborted = pool.claim_idle_connection();
    pool.settle_connection(
        generation,
        aborted,
        0,
        TransferExit::Cancelled,
        start + Duration::from_millis(80),
    );
    assert_eq!(pool.idle_connections(), 2);
    assert_eq!(
        pool.oldest_idle(),
        Some(start + Duration::from_millis(50)),
        "the entry of the connection the failed transfer closed is gone, and \
         the aborted transfer kept its own"
    );

    // Clearing the cache closes every cached connection and invalidates the
    // claims of the transfers that were running at that moment.
    pool.clear_idle_connections().expect("libcurl 8.21+");
    assert_eq!(pool.idle_connections(), 0);
    assert_eq!(pool.oldest_idle(), None);
    let stale = IdleClaim {
        since: start,
        generation,
    };
    pool.settle_connection(generation, Some(stale), 0, TransferExit::Completed, start);
    assert_eq!(
        pool.idle_connections(),
        0,
        "a transfer from before the clear must not claim a cached connection"
    );
}

/// A transfer forced onto a new socket must never claim an old idle socket,
/// including when the fresh connection is required by a changed DNS answer.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fresh_connections_preserve_the_deadline_of_unused_cached_connections() {
    for forbid_reuse in [false, true] {
        let payload = vec![b's'; 96 * 1024];
        let expected = payload.clone();
        let server = TestServer::start_with(Arc::new(move |target: &str| {
            if target.starts_with("/slow") {
                ResponsePlan {
                    chunk_size: 2048,
                    chunk_delay: Duration::from_millis(25),
                    ..ResponsePlan::body(payload.clone())
                }
            } else {
                ResponsePlan::body(b"quick".to_vec())
            }
        }))
        .await;
        let driver = DriverHandle::spawn(DriverConfig {
            max_idle_per_host: 4,
            pool_idle_timeout: Duration::from_millis(200),
            max_age_conn: None,
        });
        let port = server.addr.port();
        let mut options = RequestOptions::new(format!("http://fresh.test:{port}/quick"));
        options.resolve = Some(ResolveEntry::new(
            "fresh.test",
            port,
            &[std::net::IpAddr::from([127, 0, 0, 1])],
            Duration::from_secs(60),
        ));
        let mut quick = driver.get(options.clone(), 4096).await.unwrap();
        assert_eq!(
            collect_body(&mut quick, Duration::from_secs(5)).await,
            b"quick"
        );

        options.url = format!("http://fresh.test:{port}/slow");
        options.forbid_connection_reuse = forbid_reuse;
        if !forbid_reuse {
            // The first address still works; changing the candidate list
            // exercises the production DNS-refresh path that forces a new TCP
            // connection without depending on a second loopback listener.
            options.resolve = Some(ResolveEntry::new(
                "fresh.test",
                port,
                &[
                    std::net::IpAddr::from([127, 0, 0, 1]),
                    std::net::IpAddr::from([127, 0, 0, 2]),
                ],
                Duration::from_secs(60),
            ));
        }
        let mut slow = driver.get(options, 256 * 1024).await.unwrap();
        assert_eq!(server.accepted(), 2, "the request must open a fresh socket");
        assert_eq!(
            server.wait_for_live(1, Duration::from_millis(700)).await,
            1,
            "the unused socket must expire during the fresh transfer (forbid_reuse={forbid_reuse})"
        );
        assert_eq!(driver.active_transfers(), 1);
        assert_eq!(driver.stats().idle_clears, 1);
        assert_eq!(
            collect_body(&mut slow, Duration::from_secs(5)).await,
            expected
        );
        server.stop().await;
    }
}

/// An idle connection is reclaimed on its own deadline even when the pool keeps
/// taking new requests.
///
/// The reviewer's case: a pool with a long transfer and two idle connections, a
/// new long request that reuses only one of them. The other one has to be
/// closed at its own deadline, while a pool-wide timestamp would be refreshed
/// by the admission and the completion of the new request.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_request_does_not_postpone_the_deadline_of_connections_it_does_not_use() {
    let idle_timeout = Duration::from_millis(300);
    let slow_body = vec![b's'; 96 * 1024];
    let medium_body = vec![b'm'; 16 * 1024];
    let plan_for = {
        let slow_body = slow_body.clone();
        let medium_body = medium_body.clone();
        Arc::new(move |target: &str| {
            if target.starts_with("/slow") {
                // 96 KiB in 2 KiB chunks every 25 ms: outlives several idle
                // timeouts.
                ResponsePlan {
                    chunk_size: 2 * 1024,
                    chunk_delay: Duration::from_millis(25),
                    ..ResponsePlan::body(slow_body.clone())
                }
            } else if target.starts_with("/medium") {
                // Reuses a cached connection and finishes 400 ms later, which
                // is after the deadline of the connection nobody reused.
                ResponsePlan {
                    chunk_size: 2 * 1024,
                    chunk_delay: Duration::from_millis(50),
                    ..ResponsePlan::body(medium_body.clone())
                }
            } else {
                // A delayed head so both of them are in flight together and
                // leave two idle connections behind.
                ResponsePlan {
                    head_delay: Duration::from_millis(150),
                    ..ResponsePlan::body(b"short".to_vec())
                }
            }
        })
    };
    let server = TestServer::start_with(plan_for).await;
    let driver = Arc::new(DriverHandle::spawn(DriverConfig {
        max_idle_per_host: 4,
        pool_idle_timeout: idle_timeout,
        max_age_conn: None,
    }));

    // One long transfer holds the first connection.
    let mut slow = driver
        .get(RequestOptions::new(server.url("/slow")), 256 * 1024)
        .await
        .unwrap();
    let slow_first = slow
        .body
        .next_chunk(Duration::from_secs(5))
        .await
        .unwrap()
        .expect("the long transfer must have started");

    // Two concurrent short transfers leave two idle connections.
    let mut shorts = Vec::new();
    for _ in 0..2 {
        let driver = driver.clone();
        let url = server.url("/short");
        shorts.push(tokio::spawn(async move {
            let mut transfer = driver.get(RequestOptions::new(url), 4096).await.unwrap();
            collect_body(&mut transfer, Duration::from_secs(5)).await
        }));
    }
    for short in shorts {
        assert_eq!(short.await.unwrap(), b"short");
    }
    assert_eq!(server.live(), 3, "one streaming and two idle connections");

    // A new transfer reuses one of them; the other stays idle.
    let deadline_started = Instant::now();
    let mut medium = driver
        .get(RequestOptions::new(server.url("/medium")), 256 * 1024)
        .await
        .unwrap();
    let medium_first = medium
        .body
        .next_chunk(Duration::from_secs(5))
        .await
        .unwrap()
        .expect("the reusing transfer must have started");

    assert_eq!(
        server.wait_for_live(2, Duration::from_secs(3)).await,
        2,
        "the connection nobody reused must be closed while both transfers run"
    );
    let elapsed = deadline_started.elapsed();
    assert!(
        elapsed >= idle_timeout.saturating_sub(Duration::from_millis(80)),
        "the close must wait for pool_idle_timeout, took {elapsed:?}"
    );
    assert!(
        elapsed <= idle_timeout + Duration::from_millis(250),
        "the deadline of the untouched connection must not move to the \
         admission or the completion of the new request, took {elapsed:?}"
    );
    assert_eq!(
        driver.stats().idle_clears,
        1,
        "the close must come from idle reclamation"
    );

    // Both transfers keep streaming on their own connections.
    let slow_rest = collect_body(&mut slow, Duration::from_secs(10)).await;
    assert_eq!(slow_first.len() + slow_rest.len(), slow_body.len());
    let medium_rest = collect_body(&mut medium, Duration::from_secs(10)).await;
    assert_eq!(medium_first.len() + medium_rest.len(), medium_body.len());
    assert_eq!(
        driver.stats().connections,
        3,
        "one connection for the long transfer and one per short transfer: the \
         new request reused a cached connection"
    );
    server.stop().await;
}

/// A body stream is what keeps the driver alive while a download runs.
///
/// The client handle may be dropped in the middle of a transfer - the session
/// layer does that when a download is handed to a task - and the transfer has
/// to continue; the last body stream is also what lets the driver stop.
#[tokio::test]
async fn a_live_body_stream_keeps_the_driver_running() {
    let plan = ResponsePlan {
        chunk_size: 1024,
        chunk_delay: Duration::from_millis(20),
        ..ResponsePlan::body(vec![b'b'; 32 * 1024])
    };
    let server = TestServer::start(plan).await;
    let driver = DriverHandle::spawn(DriverConfig::default());
    let shared = driver.shared.clone();

    let mut transfer = driver
        .get(RequestOptions::new(server.url("/slow-body")), 256 * 1024)
        .await
        .unwrap();
    let first = transfer
        .body
        .next_chunk(Duration::from_secs(5))
        .await
        .unwrap()
        .expect("the transfer must have started");

    drop(driver);
    assert!(
        shared.alive.load(Ordering::SeqCst),
        "a live body stream must keep the driver running"
    );
    let rest = collect_body(&mut transfer, Duration::from_secs(10)).await;
    assert_eq!(
        first.len() + rest.len(),
        32 * 1024,
        "dropping the client handle must not truncate a transfer"
    );

    drop(transfer);
    let stopped = tokio::time::timeout(Duration::from_secs(5), async {
        while shared.alive.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
    assert!(
        stopped.is_ok(),
        "the driver must stop once its last reference is gone"
    );
    server.stop().await;
}

/// Repeating the lifecycle must not accumulate threads.
#[tokio::test]
async fn repeated_driver_lifecycles_do_not_accumulate_threads() {
    let baseline = crate::bench_stats::driver_threads();

    let mut flags = Vec::new();
    for _ in 0..4 {
        let driver = DriverHandle::spawn(DriverConfig::default());
        flags.push(driver.shared.clone());
        drop(driver);
    }

    let stopped = tokio::time::timeout(Duration::from_secs(5), async {
        while flags
            .iter()
            .any(|shared| shared.alive.load(Ordering::SeqCst))
        {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
    assert!(stopped.is_ok(), "every driver thread must stop");
    assert!(
        wait_for_live_threads(baseline, Duration::from_secs(5)).await <= baseline,
        "four created-and-dropped drivers must not leave threads behind"
    );
}

/// `pool_max_idle_per_host` is a cache bound: eight concurrent transfers may
/// open eight connections, but once they finish the pool may only keep the
/// configured number of idle ones.
///
/// Regression: the bound went to `CURLMOPT_MAX_TOTAL_CONNECTIONS`, which caps
/// the *simultaneously open* connections (so it also serializes transfers) and
/// does not prune what a multi handle has already cached. Eight finished
/// transfers left seven idle sockets open.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_connection_cache_keeps_at_most_the_configured_idle_connections() {
    let plan = ResponsePlan {
        head_delay: Duration::from_millis(200),
        ..ResponsePlan::body(b"cached".to_vec())
    };
    let server = TestServer::start(plan).await;
    let driver = Arc::new(DriverHandle::spawn(DriverConfig {
        max_idle_per_host: 1,
        pool_idle_timeout: Duration::from_secs(30),
        max_age_conn: None,
    }));

    // All eight have to be in flight together for the cache to hold eight
    // connections: `head_delay` keeps the first completion after the last
    // submission.
    let mut tasks = Vec::new();
    for _ in 0..8 {
        let driver = driver.clone();
        let url = server.url("/cache");
        tasks.push(tokio::spawn(async move {
            let mut transfer = driver.get(RequestOptions::new(url), 4096).await.unwrap();
            collect_body(&mut transfer, Duration::from_secs(5)).await
        }));
    }
    for task in tasks {
        assert_eq!(
            task.await.expect("transfer task"),
            b"cached",
            "every transfer must deliver its body"
        );
    }

    assert_eq!(
        server.accepted(),
        8,
        "eight concurrent transfers need eight connections"
    );
    assert_eq!(
        server.wait_for_live(1, Duration::from_secs(2)).await,
        1,
        "the pool must cache at most pool_max_idle_per_host idle connections"
    );
    server.stop().await;
}

/// A pool that keeps running a long transfer still has to close the
/// connections its other transfers left idle.
///
/// Regression: idle reclamation only looked at pools with no transfer at all,
/// so one long download kept every other connection of that origin open for as
/// long as it ran - `CURLOPT_MAXAGE_CONN` does not close anything, it only
/// refuses to reuse an aged connection.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_busy_pool_closes_connections_left_idle_for_the_idle_timeout() {
    let idle_timeout = Duration::from_millis(250);
    let slow_body = vec![b's'; 96 * 1024];
    let plan_for = {
        let slow_body = slow_body.clone();
        Arc::new(move |target: &str| {
            if target.starts_with("/slow") {
                // 96 KiB in 2 KiB chunks every 25 ms: long enough to outlive
                // several idle timeouts.
                ResponsePlan {
                    chunk_size: 2 * 1024,
                    chunk_delay: Duration::from_millis(25),
                    ..ResponsePlan::body(slow_body.clone())
                }
            } else {
                ResponsePlan::body(b"quick".to_vec())
            }
        })
    };
    let server = TestServer::start_with(plan_for).await;
    let driver = DriverHandle::spawn(DriverConfig {
        max_idle_per_host: 4,
        pool_idle_timeout: idle_timeout,
        max_age_conn: None,
    });

    let mut slow = driver
        .get(RequestOptions::new(server.url("/slow")), 256 * 1024)
        .await
        .unwrap();
    let first = slow
        .body
        .next_chunk(Duration::from_secs(5))
        .await
        .unwrap()
        .expect("the long transfer must have started");

    // The second transfer to the same origin finishes and leaves its
    // connection cached while the first one keeps streaming.
    let cached_at_earliest = Instant::now();
    let mut quick = driver
        .get(RequestOptions::new(server.url("/quick")), 4096)
        .await
        .unwrap();
    assert_eq!(
        collect_body(&mut quick, Duration::from_secs(5)).await,
        b"quick"
    );
    assert_eq!(
        server.live(),
        2,
        "one connection streams the long body, one is cached"
    );

    assert_eq!(
        server.wait_for_live(1, Duration::from_secs(5)).await,
        1,
        "the cached connection must be closed while the long transfer runs"
    );
    assert!(
        cached_at_earliest.elapsed() >= idle_timeout,
        "the close must wait for pool_idle_timeout, took {:?}",
        cached_at_earliest.elapsed()
    );

    // The running transfer is untouched: same connection, complete body.
    let rest = collect_body(&mut slow, Duration::from_secs(10)).await;
    assert_eq!(
        first.len() + rest.len(),
        slow_body.len(),
        "the long transfer must deliver its whole body"
    );
    assert_eq!(
        driver.stats().idle_clears,
        1,
        "the close must come from idle reclamation, not from a pool drop"
    );
    server.stop().await;
}

#[test]
fn commands_before_waker_registration_skip_poll() {
    for command in [
        Command::Resume(TransferId(1)),
        Command::Cancel(TransferId(1)),
    ] {
        let queue = CommandQueue::new();
        let multi = Multi::new();
        queue.drain(&mut Vec::new());
        // Force the interleaving: drain -> send with no waker -> prepare poll.
        queue.send(command);
        assert!(!queue.prepare_poll(multi.waker()));
        assert!(queue.poll_waker.lock().is_none());
        assert_eq!(queue.take_commands().len(), 1);
        // The skipped wait must not prevent the next idle wait from arming.
        assert!(queue.prepare_poll(multi.waker()));
    }
}

#[test]
fn closure_before_waker_registration_skips_poll() {
    let queue = CommandQueue::new();
    let multi = Multi::new();
    queue.close();
    assert!(!queue.prepare_poll(multi.waker()));
    assert!(queue.poll_waker.lock().is_none());
    assert!(queue.is_closed());
}

#[test]
fn commands_and_closure_after_registration_wake_the_next_poll() {
    for close in [false, true] {
        let queue = CommandQueue::new();
        let multi = Multi::new();
        assert!(queue.prepare_poll(multi.waker()));
        // Force the other boundary: prepare -> send/close -> enter poll.
        if close {
            queue.close();
        } else {
            queue.send(Command::Resume(TransferId(1)));
        }
        let started = Instant::now();
        multi.poll(&mut [], Duration::from_secs(2)).unwrap();
        assert!(started.elapsed() < Duration::from_secs(1));
        *queue.poll_waker.lock() = None;
        assert_eq!(queue.is_closed(), close);
        assert_eq!(queue.take_commands().len(), usize::from(!close));
    }
}

// ---------------------------------------------------------------------------
// Multi-IP policy (docs/multi-ip-connection-plan.zh-CN.md, M1/M2/M4)
// ---------------------------------------------------------------------------

/// Options that ask the driver to pick an address from `set`.
fn policy_options(url: &str, set: CandidateSet) -> RequestOptions {
    let mut options = RequestOptions::new(url);
    options.connect_timeout = Duration::from_secs(5);
    options.head_timeout = Duration::from_secs(5);
    options.candidates = Some(set);
    options
}

/// A snapshot that stays valid for the whole test.
fn candidates(host: &str, port: u16, addresses: &[[u8; 4]]) -> CandidateSet {
    CandidateSet {
        host: host.to_string(),
        port,
        addresses: addresses
            .iter()
            .copied()
            .map(std::net::IpAddr::from)
            .collect(),
        valid_until: Instant::now() + Duration::from_secs(600),
    }
}

/// An address with nothing listening: connecting to it is refused at once,
/// which is the connection failure the internal fallback exists for.
const DEAD_ADDRESS: [u8; 4] = [127, 0, 0, 3];

#[test]
fn policy_regression_connect_timeouts_require_pre_request_evidence() {
    let zero = Some(Duration::ZERO);
    for code in [5, 6, 7, 28, 35] {
        assert!(is_safe_connect_failure(code, false, Some(0), zero));
        assert!(!is_safe_connect_failure(code, false, Some(100), zero));
        assert!(!is_safe_connect_failure(code, true, Some(0), zero));
        assert!(!is_safe_connect_failure(code, false, None, zero));
    }
    assert!(!is_safe_connect_failure(28, false, Some(0), None));
    assert!(!is_safe_connect_failure(
        28,
        false,
        Some(0),
        Some(Duration::from_millis(1)),
    ));
    for code in [18, 55, 56, 60] {
        assert!(!is_safe_connect_failure(code, false, Some(0), zero));
    }
}

#[test]
fn policy_regression_unknown_and_other_connections_keep_their_deadlines() {
    let mut pool = Pool::new(&DriverConfig::default());
    let now = Instant::now();
    let generation = pool.clear_generation;
    for id in [11, 12] {
        pool.settle_pinned_connection(generation, Some(id), TransferExit::Completed, now);
    }
    pool.settle_pinned_connection(
        generation,
        Some(12),
        TransferExit::Completed,
        now + Duration::from_secs(1),
    );
    assert_eq!(pool.oldest_idle(), Some(now));
    assert_eq!(pool.idle_connections(), 2);
    pool.idle_since = Some(now + Duration::from_secs(1));
    assert_eq!(
        pool.idle_deadline(Duration::from_secs(2), false),
        Some(now + Duration::from_secs(2)),
        "a fully idle pool must still honour its oldest policy socket"
    );
    pool.settle_pinned_connection(generation, Some(12), TransferExit::Cancelled, now);
    assert_eq!(pool.oldest_idle(), Some(now));
    assert_eq!(pool.idle_connections(), 1);
    pool.settle_pinned_connection(generation, None, TransferExit::Completed, now);
    pool.settle_pinned_connection(
        generation,
        None,
        TransferExit::Completed,
        now + Duration::from_secs(2),
    );
    assert_eq!(pool.oldest_idle(), Some(now));
    pool.clear_idle_connections().unwrap();
    pool.settle_pinned_connection(generation, Some(11), TransferExit::Completed, now);
    assert_eq!(
        pool.idle_connections(),
        0,
        "old generation cannot restore an entry"
    );
}

/// Drive submission and completion on this thread, without opening sockets,
/// so the otherwise private attempt table can be checked after head failures.
#[test]
fn policy_regression_pre_head_failures_release_attempt_records() {
    let shared = Arc::new(DriverShared {
        sinks: Mutex::new(HashMap::new()),
        active: AtomicUsize::new(0),
        counters: DriverCounters::default(),
        alive: AtomicBool::new(true),
        pools: AtomicUsize::new(0),
        idle_clear_unsupported: AtomicBool::new(false),
    });
    let queue = CommandQueue::new();
    let config = DriverConfig::default();
    let mut pools = HashMap::new();
    let mut policy = IpPolicy::new();
    let mut attempts = HashMap::new();
    for (code, address_count) in [(7, 1), (28, 1), (60, 1), (7, 3), (28, 3), (60, 3)] {
        let addresses = [DEAD_ADDRESS, [127, 0, 0, 4], [127, 0, 0, 5]];
        let options = policy_options(
            "https://dual.test:443/file",
            candidates("dual.test", 443, &addresses[..address_count]),
        );
        let key = options.pool_key();
        let id = TransferId::next();
        let sink = BodySink::new(id, 1024, Arc::downgrade(&queue));
        let (head, mut receiver) = oneshot::channel();
        shared.sinks.lock().insert(id, sink.clone());
        start_transfer(
            &mut pools,
            &shared,
            &mut policy,
            &mut attempts,
            &config,
            SubmitRequest {
                id,
                options,
                sink,
                head,
            },
        );
        assert_eq!(attempts.len(), 1);
        let mut completions = 0;
        loop {
            completions += 1;
            assert!(completions <= address_count);
            match complete_transfer(
                &mut pools,
                &shared,
                &mut policy,
                &mut attempts,
                &key,
                id,
                Err((code, "simulated pre-head failure".into())),
            ) {
                Completion::Done => break,
                Completion::Retry(request) => {
                    assert_eq!(attempts.len(), 1, "retry retains the same attempt record");
                    assert!(matches!(
                        receiver.try_recv(),
                        Err(oneshot::error::TryRecvError::Empty)
                    ));
                    start_retry(
                        &mut pools,
                        &shared,
                        &mut policy,
                        &mut attempts,
                        &config,
                        *request,
                    );
                }
            }
        }
        assert_eq!(completions, if code == 60 { 1 } else { address_count });
        assert!(receiver.try_recv().unwrap().is_err());
        assert!(
            attempts.is_empty(),
            "curl {code} left a record with no BodyStream"
        );
        assert!(shared.sinks.lock().is_empty());
        cancel_transfer(&mut pools, &shared, &mut policy, &mut attempts, id);
        assert!(attempts.is_empty());
    }
}

#[test]
fn policy_regression_expired_retry_releases_everything() {
    let shared = Arc::new(DriverShared {
        sinks: Mutex::new(HashMap::new()),
        active: AtomicUsize::new(0),
        counters: DriverCounters::default(),
        alive: AtomicBool::new(true),
        pools: AtomicUsize::new(0),
        idle_clear_unsupported: AtomicBool::new(false),
    });
    let queue = CommandQueue::new();
    let id = TransferId::next();
    let sink = BodySink::new(id, 1024, Arc::downgrade(&queue));
    shared.sinks.lock().insert(id, sink.clone());
    let options = policy_options(
        "http://dual.test:9/file",
        candidates("dual.test", 9, &[DEAD_ADDRESS]),
    );
    let key = options.pool_key();
    let mut policy = IpPolicy::new();
    let pinned = reserve_candidate(&mut policy, &options, &key, Instant::now(), None)
        .unwrap()
        .unwrap();
    let mut attempts = HashMap::from([(
        id,
        AttemptRecord {
            pool: key.clone(),
            claim: pinned.claim,
            outcome: None,
            consumed: None,
            sample: None,
        },
    )]);
    let (head, mut receiver) = oneshot::channel();

    start_retry(
        &mut HashMap::new(),
        &shared,
        &mut policy,
        &mut attempts,
        &DriverConfig::default(),
        RetryRequest {
            id,
            pool: key,
            state: Box::new(RetryState {
                options,
                attempts: 1,
                max_attempts: 2,
                deadline: Instant::now(),
            }),
            sink,
            head,
            error: DownloadError::Cancelled,
        },
    );

    assert!(matches!(
        receiver.try_recv(),
        Ok(Err(DownloadError::Cancelled))
    ));
    assert!(attempts.is_empty());
    assert!(!shared.sinks.lock().contains_key(&id));
}

#[test]
fn policy_regression_releasing_an_unattached_candidate_settles_its_claim() {
    let options = policy_options(
        "http://dual.test:9/file",
        candidates("dual.test", 9, &[DEAD_ADDRESS]),
    );
    let key = options.pool_key();
    let mut policy = IpPolicy::new();
    let pinned = reserve_candidate(&mut policy, &options, &key, Instant::now(), None)
        .unwrap()
        .unwrap();
    let id = TransferId::next();
    let mut attempts = HashMap::from([(
        id,
        AttemptRecord {
            pool: key,
            claim: pinned.claim,
            outcome: None,
            consumed: None,
            sample: None,
        },
    )]);

    release_candidate(&mut policy, &mut attempts, id);

    assert!(attempts.is_empty());
}

#[test]
fn policy_regression_retry_can_restore_a_missing_attempt_record() {
    let shared = Arc::new(DriverShared {
        sinks: Mutex::new(HashMap::new()),
        active: AtomicUsize::new(0),
        counters: DriverCounters::default(),
        alive: AtomicBool::new(true),
        pools: AtomicUsize::new(0),
        idle_clear_unsupported: AtomicBool::new(false),
    });
    let queue = CommandQueue::new();
    let id = TransferId::next();
    let sink = BodySink::new(id, 1024, Arc::downgrade(&queue));
    shared.sinks.lock().insert(id, sink.clone());
    let options = policy_options(
        "http://dual.test:9/file",
        candidates("dual.test", 9, &[DEAD_ADDRESS]),
    );
    let key = options.pool_key();
    let mut pools = HashMap::new();
    let mut policy = IpPolicy::new();
    let mut attempts = HashMap::new();
    let (head, mut receiver) = oneshot::channel();

    start_retry(
        &mut pools,
        &shared,
        &mut policy,
        &mut attempts,
        &DriverConfig::default(),
        RetryRequest {
            id,
            pool: key,
            state: Box::new(RetryState {
                options,
                attempts: 0,
                max_attempts: 1,
                deadline: Instant::now() + Duration::from_secs(5),
            }),
            sink,
            head,
            error: DownloadError::Cancelled,
        },
    );

    assert!(attempts.contains_key(&id));
    cancel_transfer(&mut pools, &shared, &mut policy, &mut attempts, id);
    assert!(matches!(
        receiver.try_recv(),
        Ok(Err(DownloadError::Cancelled))
    ));
    assert!(attempts.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn policy_regression_another_ips_idle_socket_does_not_disable_fallback() {
    let server = DualAddressServer::start(Duration::ZERO).await;
    let driver = DriverHandle::spawn(DriverConfig::default());
    let url = server.url("/cached-fallback");
    let mut warm = driver
        .get(
            policy_options(
                &url,
                server.candidates(&[server.address(0)], Duration::from_secs(60)),
            ),
            1024,
        )
        .await
        .unwrap();
    assert_eq!(collect_body(&mut warm, Duration::from_secs(2)).await, b"a0");
    drop(warm);

    let mut transfer = driver
        .get(
            policy_options(
                &url,
                candidates(
                    server.host(),
                    server.port(),
                    &[DEAD_ADDRESS, [127, 0, 0, 1]],
                ),
            ),
            1024,
        )
        .await
        .expect("an unrelated idle socket must not suppress retry");
    assert_eq!(
        collect_body(&mut transfer, Duration::from_secs(2)).await,
        b"a0"
    );
    assert_eq!(driver.stats().connect_retries, 1);
    assert_eq!(
        server.accepted(0),
        1,
        "the healthy socket is still reusable"
    );
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn policy_regression_busy_a_does_not_refresh_idle_b() {
    let server = DualAddressServer::start(Duration::from_millis(150)).await;
    let driver = DriverHandle::spawn(DriverConfig {
        max_idle_per_host: 8,
        pool_idle_timeout: Duration::from_millis(400),
        max_age_conn: None,
    });
    let url = server.url("/idle");
    // B is older. All later traffic is pinned to A, including several reuses.
    for index in [1, 0, 0, 0, 0, 0] {
        let mut transfer = driver
            .get(
                policy_options(
                    &url,
                    server.candidates(&[server.address(index)], Duration::from_secs(60)),
                ),
                1024,
            )
            .await
            .unwrap();
        collect_body(&mut transfer, Duration::from_secs(2)).await;
    }
    assert_eq!(
        server.wait_for_live(1, 0, Duration::from_millis(100)).await,
        0
    );
    assert!(driver.stats().idle_clears > 0);
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn policy_regression_same_ip_reuse_keeps_the_unused_socket_deadline() {
    let server = TestServer::start_with(Arc::new(|target: &str| {
        if target.starts_with("/slow") {
            ResponsePlan {
                chunk_size: 2048,
                chunk_delay: Duration::from_millis(25),
                ..ResponsePlan::body(vec![b'x'; 96 * 1024])
            }
        } else {
            ResponsePlan {
                head_delay: Duration::from_millis(150),
                ..ResponsePlan::body(b"warm".to_vec())
            }
        }
    }))
    .await;
    let driver = DriverHandle::spawn(DriverConfig {
        max_idle_per_host: 8,
        pool_idle_timeout: Duration::from_millis(400),
        max_age_conn: None,
    });
    let options = policy_options(
        &format!("http://dual.test:{}/warm", server.addr.port()),
        candidates("dual.test", server.addr.port(), &[[127, 0, 0, 1]]),
    );
    let (first, second) = tokio::join!(
        driver.get(options.clone(), 1024),
        driver.get(options.clone(), 1024),
    );
    for mut transfer in [first.unwrap(), second.unwrap()] {
        collect_body(&mut transfer, Duration::from_secs(2)).await;
    }
    assert_eq!(server.live.load(Ordering::SeqCst), 2);
    let mut options = options;
    options.url = format!("http://dual.test:{}/slow", server.addr.port());
    let mut transfer = driver.get(options, 256 * 1024).await.unwrap();
    assert_eq!(server.accepted(), 2, "the long request reused one socket");
    assert_eq!(server.wait_for_live(1, Duration::from_millis(700)).await, 1);
    assert_eq!(
        driver.active_transfers(),
        1,
        "the reused socket is still busy"
    );
    assert!(driver.stats().idle_clears > 0);
    assert_eq!(
        collect_body(&mut transfer, Duration::from_secs(3)).await,
        vec![b'x'; 96 * 1024]
    );
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg_attr(windows, ignore = "requires available Schannel credentials")]
async fn policy_regression_stalled_tls_falls_back_within_the_original_budget() {
    let fixture = TlsFixture::start(1).await;
    let stalled = TcpListener::bind(("127.0.0.2", fixture.addr.port()))
        .await
        .unwrap();
    let stall_task = tokio::spawn(async move {
        let (mut stream, _) = stalled.accept().await.unwrap();
        // Accept TCP and read ClientHello, but never send a TLS response.
        let mut bytes = Vec::new();
        let _ = stream.read_to_end(&mut bytes).await;
        assert!(!bytes.is_empty());
    });
    let driver = DriverHandle::spawn(DriverConfig::default());
    let mut options = fixture.options(&fixture.url("127.0.0.1"), 0);
    options.candidates = Some(candidates(
        "127.0.0.1",
        fixture.addr.port(),
        &[[127, 0, 0, 2], [127, 0, 0, 1]],
    ));
    options.connect_timeout = Duration::from_secs(4);
    options.head_timeout = Duration::from_secs(4);
    let started = Instant::now();
    let mut transfer = driver
        .get(options, 1024)
        .await
        .expect("the initial connect slice must leave time for the healthy IP");
    assert_eq!(
        collect_body(&mut transfer, Duration::from_secs(2)).await,
        b"abcd"
    );
    assert!(started.elapsed() < Duration::from_secs(4));
    assert_eq!(driver.stats().connect_retries, 1);
    assert_eq!(driver.stats().submitted, 1);
    stall_task.await.unwrap();
    fixture.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn policy_transfers_land_on_the_address_the_driver_chose() {
    let server = DualAddressServer::start(Duration::ZERO).await;
    let driver = DriverHandle::spawn(DriverConfig::default());
    let url = server.url("/pinned");
    let set = server.candidates(
        &[server.address(0), server.address(1)],
        Duration::from_secs(600),
    );

    // §7 rule 2: the first two requests cover both candidates.
    let mut bodies = Vec::new();
    for _ in 0..2 {
        let mut transfer = driver
            .get(policy_options(&url, set.clone()), 1024)
            .await
            .unwrap();
        bodies.push(collect_body(&mut transfer, Duration::from_secs(5)).await);
    }

    bodies.sort();
    assert_eq!(
        bodies,
        vec![b"a0".to_vec(), b"b0".to_vec()],
        "each address served the request the driver pinned there"
    );
    assert_eq!(server.accepted(0), 1);
    assert_eq!(server.accepted(1), 1);
    assert_eq!(driver.stats().ip_selections, 2);
    assert!(wait_for_idle(&driver, Duration::from_secs(2)).await);
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_candidate_is_retried_without_a_second_request() {
    let server = DualAddressServer::start(Duration::ZERO).await;
    let driver = DriverHandle::spawn(DriverConfig::default());
    let url = server.url("/fallback");
    // The dead address is first, so coverage sends the request there.
    let set = candidates(
        server.host(),
        server.port(),
        &[DEAD_ADDRESS, [127, 0, 0, 1]],
    );

    let mut transfer = driver
        .get(policy_options(&url, set), 4096)
        .await
        .expect("a refused candidate must not fail the request");
    let body = collect_body(&mut transfer, Duration::from_secs(5)).await;

    assert_eq!(body, b"a0", "the retry reached the healthy address");
    let stats = driver.stats();
    assert_eq!(
        stats.submitted, 1,
        "the session asked for one request and got one"
    );
    assert_eq!(
        stats.connect_retries, 1,
        "the second connection belongs to the same request"
    );
    assert_eq!(stats.completed, 1);
    assert_eq!(stats.ip_selections, 2, "both attempts reserved an address");
    assert_eq!(server.accepted(0), 1);
    assert!(wait_for_idle(&driver, Duration::from_secs(2)).await);
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_single_dead_candidate_reports_a_retryable_transport_failure() {
    let driver = DriverHandle::spawn(DriverConfig::default());
    let set = candidates("dual.test", 9, &[DEAD_ADDRESS]);
    let mut options = policy_options("http://dual.test:9/file", set);
    options.connect_timeout = Duration::from_secs(2);

    let error = match driver.get(options, 1024).await {
        Ok(_) => panic!("nothing listens on the only candidate"),
        Err(error) => error,
    };

    match error {
        DownloadError::Transport(transport) => {
            assert!(
                matches!(
                    transport.kind(),
                    TransportErrorKind::Connect | TransportErrorKind::Timeout
                ),
                "an unbound loopback address may be refused or exhaust its connect budget"
            );
            assert!(
                DownloadError::Transport(transport).is_retryable(),
                "a refused connection is the caller's to retry"
            );
        }
        other => panic!("expected a transport error, got {other:?}"),
    }
    let stats = driver.stats();
    assert_eq!(
        stats.connect_retries, 0,
        "one candidate leaves nothing to retry"
    );
    assert_eq!(stats.completed, 1);
    assert!(wait_for_idle(&driver, Duration::from_secs(2)).await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_internal_retry_never_resets_the_callers_deadline() {
    // The healthy address answers far too late for the caller's 400 ms budget.
    let server = TestServer::start(ResponsePlan {
        head_delay: Duration::from_secs(3),
        ..ResponsePlan::body(b"late".to_vec())
    })
    .await;
    let driver = DriverHandle::spawn(DriverConfig::default());
    let url = format!("http://dual.test:{}/late", server.addr.port());
    let mut options = policy_options(
        &url,
        candidates(
            "dual.test",
            server.addr.port(),
            &[DEAD_ADDRESS, [127, 0, 0, 1]],
        ),
    );
    options.head_timeout = Duration::from_millis(400);

    let started = Instant::now();
    let error = match driver.get(options, 1024).await {
        Ok(_) => panic!("the caller's deadline must bound the retried transfer"),
        Err(error) => error,
    };
    let elapsed = started.elapsed();

    assert!(
        matches!(
            error,
            DownloadError::Transport(ref transport)
                if transport.kind() == TransportErrorKind::Timeout
        ),
        "the caller's deadline decides, not the retry: {error:?}"
    );
    assert!(
        elapsed < Duration::from_secs(2),
        "the deadline must not be extended by the retry, took {elapsed:?}"
    );
    let retries = driver.stats().connect_retries;
    assert!(
        retries == 1 || (cfg!(windows) && retries == 0),
        "the dead target should be retried when the platform reports its failure before the deadline; got {retries} retries"
    );
    assert!(wait_for_idle(&driver, Duration::from_secs(3)).await);
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_body_failure_is_not_retried_inside_the_driver() {
    // The connection succeeds and the body is truncated: the request may have
    // reached the origin, so §6 leaves it to the caller's retry rules.
    let server = TestServer::start(ResponsePlan {
        body: vec![b'x'; 64 * 1024],
        truncate_at: Some(1024),
        ..ResponsePlan::body(vec![b'x'; 64 * 1024])
    })
    .await;
    let driver = DriverHandle::spawn(DriverConfig::default());
    let url = format!("http://dual.test:{}/truncated", server.addr.port());
    let set = candidates(
        "dual.test",
        server.addr.port(),
        &[[127, 0, 0, 1], [127, 0, 0, 2]],
    );

    let mut transfer = driver.get(policy_options(&url, set), 4096).await.unwrap();
    let error = loop {
        match transfer.body.next_chunk(Duration::from_secs(5)).await {
            Ok(Some(_)) => continue,
            Ok(None) => panic!("a truncated body must not look like a clean EOF"),
            Err(error) => break error,
        }
    };

    assert_eq!(
        driver.stats().connect_retries,
        0,
        "a body error is never rotated to another address"
    );
    assert!(
        !error.is_retryable() || matches!(error, DownloadError::Transport(_)),
        "the failure has to stay the session's business, got {error:?}"
    );
    assert!(wait_for_idle(&driver, Duration::from_secs(2)).await);
    server.stop().await;
}

#[tokio::test]
async fn an_expired_candidate_snapshot_is_refused() {
    let driver = DriverHandle::spawn(DriverConfig::default());
    let mut set = candidates("dual.test", 9, &[[127, 0, 0, 1]]);
    set.valid_until = Instant::now();

    let error = match driver
        .get(policy_options("http://dual.test:9/file", set), 1024)
        .await
    {
        Ok(_) => panic!("an expired snapshot must not start a transfer"),
        Err(error) => error,
    };

    assert!(
        matches!(
            error,
            DownloadError::Transport(ref transport)
                if transport.kind() == TransportErrorKind::Connect
        ),
        "§5: a snapshot that expired before the transfer started fails the request, got {error:?}"
    );
    assert_eq!(driver.stats().ip_selections, 0);
    assert!(wait_for_idle(&driver, Duration::from_secs(2)).await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_addresses_share_one_cache_bound() {
    let server = DualAddressServer::start(Duration::ZERO).await;
    let driver = DriverHandle::spawn(DriverConfig {
        max_idle_per_host: 1,
        ..DriverConfig::default()
    });
    let url = server.url("/bound");
    let live = (server.live_handle(0), server.live_handle(1));

    for index in 0..2 {
        if index == 1 {
            // Space the two idle moments apart: libcurl evicts the connection
            // that has been idle longest, and two sockets that went idle in the
            // same millisecond would tie.
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
        let set = candidates(
            server.host(),
            server.port(),
            &[[127, 0, 0, 1 + index as u8]],
        );
        let mut transfer = driver.get(policy_options(&url, set), 1024).await.unwrap();
        collect_body(&mut transfer, Duration::from_secs(5)).await;
    }

    // Socket shutdown is observed by the server task asynchronously after
    // libcurl evicts the connection, so wait for that observation instead of
    // racing it with an immediate counter read.
    let eviction_observed = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if (live.0.load(Ordering::SeqCst), live.1.load(Ordering::SeqCst)) == (0, 1) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;

    // §6: the bound is the origin's, not one per address.
    assert!(
        eviction_observed.is_ok(),
        "the first address's socket is evicted when the second goes idle; live counts: ({}, {})",
        live.0.load(Ordering::SeqCst),
        live.1.load(Ordering::SeqCst)
    );
    assert_eq!(
        live.0.load(Ordering::SeqCst),
        0,
        "the first address's socket is evicted when the second goes idle"
    );
    assert_eq!(live.1.load(Ordering::SeqCst), 1);
    assert!(wait_for_idle(&driver, Duration::from_secs(2)).await);
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_consumed_body_ranks_its_address_and_an_abandoned_one_does_not() {
    let body = vec![b'x'; 256 * 1024];
    let server = TestServer::start(ResponsePlan {
        chunk_delay: Duration::from_millis(10),
        ..ResponsePlan::body(body)
    })
    .await;
    let driver = DriverHandle::spawn(DriverConfig::default());
    let url = format!("http://dual.test:{}/slow", server.addr.port());
    let set = || candidates("dual.test", server.addr.port(), &[[127, 0, 0, 1]]);

    // A body read to EOF is a sample. What the driver counts is the consumer's
    // verdict, so the body has to be released before the counters are read.
    let mut transfer = driver
        .get(policy_options(&url, set()), MAX_BODY_BUDGET_BYTES)
        .await
        .unwrap();
    let read = collect_body(&mut transfer, Duration::from_secs(10)).await;
    assert_eq!(read.len(), 256 * 1024);
    drop(transfer);
    assert_eq!(
        wait_for_windows(&driver, 1).await,
        (1, 0),
        "a completely consumed body has to rank its address"
    );

    // A body the consumer drops is not.
    let mut transfer = driver
        .get(policy_options(&url, set()), MAX_BODY_BUDGET_BYTES)
        .await
        .unwrap();
    let _ = transfer
        .body
        .next_chunk(Duration::from_secs(5))
        .await
        .unwrap();
    drop(transfer);
    assert!(wait_for_idle(&driver, Duration::from_secs(5)).await);
    assert_eq!(
        driver.stats().ip_samples,
        1,
        "an abandoned body must not become a success sample"
    );
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_window_stretched_by_backpressure_is_dropped() {
    let body = vec![b'x'; 128 * 1024];
    let server = TestServer::start(ResponsePlan {
        chunk_delay: Duration::from_millis(5),
        ..ResponsePlan::body(body)
    })
    .await;
    let driver = DriverHandle::spawn(DriverConfig::default());
    let url = format!("http://dual.test:{}/held", server.addr.port());
    let set = candidates("dual.test", server.addr.port(), &[[127, 0, 0, 1]]);

    // A budget far below the body: the write callback has to pause while the
    // consumer deliberately reads nothing, which is local pollution.
    let mut transfer = driver
        .get(policy_options(&url, set), MIN_BODY_BUDGET_BYTES)
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(400)).await;
    let read = collect_body(&mut transfer, Duration::from_secs(10)).await;
    assert_eq!(read.len(), 128 * 1024);
    drop(transfer);

    assert_eq!(
        wait_for_windows(&driver, 1).await,
        (0, 1),
        "a window that is mostly backpressure must not rank the address"
    );
    server.stop().await;
}

/// Waits until the policy has seen `expected` windows, and reports how they
/// were classified: `(usable, polluted)`.
async fn wait_for_windows(driver: &DriverHandle, expected: u64) -> (u64, u64) {
    let started = Instant::now();
    loop {
        let stats = driver.stats();
        if stats.ip_samples + stats.ip_polluted_samples >= expected
            || started.elapsed() > Duration::from_secs(5)
        {
            return (stats.ip_samples, stats.ip_polluted_samples);
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}
