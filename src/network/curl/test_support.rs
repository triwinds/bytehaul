//! Shared fixtures for the libcurl backend tests.
//!
//! Kept outside `driver` so transport-level tests can run a real server without
//! depending on the driver's own test module.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::oneshot;

use super::ip_policy::CandidateSet;

/// A loopback HTTP/1.1 server that answers every request with one body.
///
/// Dropping the handle stops the server.
pub(crate) struct ScriptedHttpServer {
    pub(crate) port: u16,
    requests: Arc<AtomicUsize>,
    head: Arc<Mutex<String>>,
    task: tokio::task::JoinHandle<()>,
}

impl ScriptedHttpServer {
    /// The first request head received, lowercased, for request-form asserts.
    pub(crate) fn request_head(&self) -> String {
        self.head.lock().clone()
    }

    pub(crate) fn requests(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
    }
}

impl Drop for ScriptedHttpServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Starts a keep-alive server that always answers `200 OK` with `body`.
pub(crate) async fn scripted_http_server(body: Vec<u8>) -> ScriptedHttpServer {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let requests = Arc::new(AtomicUsize::new(0));
    let head = Arc::new(Mutex::new(String::new()));

    let task_requests = requests.clone();
    let task_head = head.clone();
    let task = tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let requests = task_requests.clone();
            let head = task_head.clone();
            let body = body.clone();
            tokio::spawn(async move {
                loop {
                    let mut buffer = Vec::new();
                    let mut byte = [0u8; 1];
                    loop {
                        match stream.read(&mut byte).await {
                            Ok(0) | Err(_) => return,
                            Ok(_) => buffer.push(byte[0]),
                        }
                        if buffer.ends_with(b"\r\n\r\n") {
                            break;
                        }
                        if buffer.len() > 64 * 1024 {
                            return;
                        }
                    }
                    requests.fetch_add(1, Ordering::SeqCst);
                    {
                        let mut recorded = head.lock();
                        if recorded.is_empty() {
                            *recorded = String::from_utf8_lossy(&buffer).to_ascii_lowercase();
                        }
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
    });

    ScriptedHttpServer {
        port,
        requests,
        head,
        task,
    }
}

/// Serializes tests that mutate proxy environment variables.
pub(crate) fn env_guard() -> &'static std::sync::Mutex<()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
}

/// A proxy that serves only requests carrying `Proxy-Authorization`.
///
/// Credentials in the proxy URL make libcurl send `Proxy-Authorization`
/// preemptively, so this fixture accepts that first attempt and answers `407`
/// otherwise.
pub(crate) struct AuthenticatingProxy {
    pub(crate) port: u16,
    heads: Arc<Mutex<Vec<String>>>,
    task: tokio::task::JoinHandle<()>,
}

impl AuthenticatingProxy {
    /// Every request head the proxy saw, lowercased and in order.
    pub(crate) fn heads(&self) -> Vec<String> {
        self.heads.lock().clone()
    }
}

impl Drop for AuthenticatingProxy {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub(crate) async fn authenticating_proxy(body: Vec<u8>) -> AuthenticatingProxy {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let heads = Arc::new(Mutex::new(Vec::new()));
    let task_heads = heads.clone();
    let task = tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let heads = task_heads.clone();
            let body = body.clone();
            tokio::spawn(async move {
                loop {
                    let Some(head) = read_lowercased_head(&mut stream).await else {
                        return;
                    };
                    let authorized = head.contains("proxy-authorization:");
                    heads.lock().push(head);
                    let response = if authorized {
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n",
                            body.len()
                        )
                    } else {
                        "HTTP/1.1 407 Proxy Authentication Required\r\nProxy-Authenticate: Basic realm=\"bytehaul-test\"\r\nContent-Length: 0\r\nConnection: keep-alive\r\n\r\n".to_string()
                    };
                    if stream.write_all(response.as_bytes()).await.is_err() {
                        return;
                    }
                    if authorized && stream.write_all(&body).await.is_err() {
                        return;
                    }
                    if stream.flush().await.is_err() {
                        return;
                    }
                }
            });
        }
    });

    AuthenticatingProxy { port, heads, task }
}

/// Reads one request head from a raw stream, lowercased.
async fn read_lowercased_head(stream: &mut tokio::net::TcpStream) -> Option<String> {
    let mut buffer = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match stream.read(&mut byte).await {
            Ok(0) | Err(_) => return None,
            Ok(_) => buffer.push(byte[0]),
        }
        if buffer.ends_with(b"\r\n\r\n") {
            return Some(String::from_utf8_lossy(&buffer).to_ascii_lowercase());
        }
        if buffer.len() > 64 * 1024 {
            return None;
        }
    }
}

/// Two loopback addresses serving the same port, for multi-address tests.
///
/// The shared port is what lets one origin name be pinned to either address
/// with `CURLOPT_CONNECT_TO`, which matches on the URL's `host:port`. The URL
/// host is `dual.test`, which nothing resolves, so a request reaches a listener
/// only through the connect target its driver picked.
///
/// Both addresses live in `127.0.0.0/8`, which every supported platform routes
/// back to the local host, so no interface alias is needed.
/// One loopback address of a [`DualAddressServer`]: what this address alone
/// accepted, kept open, and served.
pub(crate) struct AddressObserver {
    /// Short label the served bodies start with, so a response says which
    /// address answered it.
    tag: &'static str,
    accepted: AtomicUsize,
    /// Behind an `Arc` so a blocking experiment can watch this address while it
    /// holds a `!Send` multi handle.
    live: Arc<AtomicUsize>,
    peak_live: AtomicUsize,
    /// Requests served per accepted connection, keyed by connection ordinal:
    /// the observation that tells a reused socket from a new one.
    pub(crate) requests: Mutex<HashMap<usize, usize>>,
    /// First request head this address received, lowercased.
    head: Mutex<String>,
}

impl AddressObserver {
    pub(crate) fn new(tag: &'static str) -> Self {
        Self {
            tag,
            accepted: AtomicUsize::new(0),
            live: Arc::new(AtomicUsize::new(0)),
            peak_live: AtomicUsize::new(0),
            requests: Mutex::new(HashMap::new()),
            head: Mutex::new(String::new()),
        }
    }
}

/// Two loopback addresses serving the *same* port, so one origin name can be
/// pinned to either of them with `CURLOPT_CONNECT_TO`.
///
/// The shared port is the point: `CONNECT_TO` matches the URL's `host:port`, so
/// two targets of one origin differ only by address - exactly the choice a
/// multi-IP policy makes. The URL host is `dual.test`, which nothing resolves:
/// every request in these experiments reaches the server only through its
/// connect target, so a broken entry shows up as a DNS failure instead of a
/// silent connection to the wrong place.
///
/// Both addresses are in `127.0.0.0/8`, which every supported platform routes
/// back to the local host, so no interface alias is needed.
pub(crate) struct DualAddressServer {
    port: u16,
    addresses: [std::net::IpAddr; 2],
    observers: Vec<Arc<AddressObserver>>,
    shutdown: Vec<oneshot::Sender<()>>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl DualAddressServer {
    pub(crate) async fn start(response_delay: Duration) -> Self {
        let addresses = [
            std::net::IpAddr::from([127, 0, 0, 1]),
            std::net::IpAddr::from([127, 0, 0, 2]),
        ];
        let (listeners, port) = bind_loopback_pair(addresses).await;

        let mut observers = Vec::new();
        let mut shutdown = Vec::new();
        let mut tasks = Vec::new();
        for (index, listener) in listeners.into_iter().enumerate() {
            let observer = Arc::new(AddressObserver::new(if index == 0 { "a" } else { "b" }));
            let (stop, mut stop_rx) = oneshot::channel::<()>();
            let task_observer = observer.clone();
            let task = tokio::spawn(async move {
                let mut ordinal = 0usize;
                loop {
                    let accept = listener.accept();
                    let connection = tokio::select! {
                        result = accept => result,
                        _ = &mut stop_rx => break,
                    };
                    let Ok((stream, _)) = connection else {
                        break;
                    };
                    let id = ordinal;
                    ordinal += 1;
                    task_observer.accepted.fetch_add(1, Ordering::SeqCst);
                    let live = task_observer.live.fetch_add(1, Ordering::SeqCst) + 1;
                    task_observer.peak_live.fetch_max(live, Ordering::SeqCst);
                    let observer = task_observer.clone();
                    tokio::spawn(async move {
                        serve_address(stream, observer.clone(), id, response_delay).await;
                        observer.live.fetch_sub(1, Ordering::SeqCst);
                    });
                }
            });
            observers.push(observer);
            shutdown.push(stop);
            tasks.push(task);
        }

        Self {
            port,
            addresses,
            observers,
            shutdown,
            tasks,
        }
    }

    /// The URL host. Nothing resolves it: the connect target is what reaches a
    /// listener.
    pub(crate) fn host(&self) -> &'static str {
        "dual.test"
    }

    pub(crate) fn port(&self) -> u16 {
        self.port
    }

    pub(crate) fn address(&self, index: usize) -> std::net::IpAddr {
        self.addresses[index]
    }

    pub(crate) fn url(&self, path: &str) -> String {
        format!("http://{}:{}{path}", self.host(), self.port)
    }

    /// The `CURLOPT_CONNECT_TO` entry that sends this origin's `:port` to the
    /// given address.
    pub(crate) fn connect_to(&self, index: usize) -> String {
        super::driver::ConnectToEntry::new(self.host(), self.port, self.address(index), self.port)
            .spec
    }

    pub(crate) fn accepted(&self, index: usize) -> usize {
        self.observers[index].accepted.load(Ordering::SeqCst)
    }

    pub(crate) fn live(&self, index: usize) -> usize {
        self.observers[index].live.load(Ordering::SeqCst)
    }

    /// Shared liveness counter, so a blocking experiment can observe the
    /// server while it holds a `!Send` multi handle.
    pub(crate) fn live_handle(&self, index: usize) -> Arc<AtomicUsize> {
        self.observers[index].live.clone()
    }

    pub(crate) fn peak_live(&self, index: usize) -> usize {
        self.observers[index].peak_live.load(Ordering::SeqCst)
    }

    pub(crate) fn requests_on(&self, index: usize, connection: usize) -> usize {
        self.observers[index]
            .requests
            .lock()
            .get(&connection)
            .copied()
            .unwrap_or(0)
    }

    pub(crate) fn first_head(&self, index: usize) -> String {
        self.observers[index].head.lock().clone()
    }

    pub(crate) async fn wait_for_live(
        &self,
        index: usize,
        expected: usize,
        timeout: Duration,
    ) -> usize {
        let started = Instant::now();
        while started.elapsed() < timeout {
            if self.live(index) == expected {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        self.live(index)
    }

    pub(crate) async fn stop(mut self) {
        for shutdown in self.shutdown.drain(..) {
            let _ = shutdown.send(());
        }
        for task in self.tasks.drain(..) {
            let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
        }
    }
}

/// Binds the same ephemeral port on both addresses.
///
/// The first listener picks the port; the second has to take the same one, so a
/// port already taken on the second address sends the loop back to a fresh
/// pick.
pub(crate) async fn bind_loopback_pair(
    addresses: [std::net::IpAddr; 2],
) -> ([TcpListener; 2], u16) {
    for _ in 0..64 {
        let first = TcpListener::bind(SocketAddr::new(addresses[0], 0))
            .await
            .unwrap();
        let port = first.local_addr().unwrap().port();
        if let Ok(second) = TcpListener::bind(SocketAddr::new(addresses[1], port)).await {
            return ([first, second], port);
        }
    }
    panic!(
        "could not bind port for both {} and {}",
        addresses[0], addresses[1]
    );
}

/// Serves keep-alive requests on one accepted connection of one address.
///
/// The body is `{tag}{ordinal}`: the tag says which address answered, the
/// ordinal says which of that address's sockets did.
pub(crate) async fn serve_address(
    mut stream: tokio::net::TcpStream,
    observer: Arc<AddressObserver>,
    ordinal: usize,
    response_delay: Duration,
) {
    let mut buffer = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match stream.read(&mut byte).await {
            Ok(0) | Err(_) => return,
            Ok(_) => buffer.push(byte[0]),
        }
        if !buffer.ends_with(b"\r\n\r\n") {
            if buffer.len() > 64 * 1024 {
                return;
            }
            continue;
        }
        observer
            .requests
            .lock()
            .entry(ordinal)
            .and_modify(|count| *count += 1)
            .or_insert(1);
        {
            let mut head = observer.head.lock();
            if head.is_empty() {
                *head = String::from_utf8_lossy(&buffer).to_ascii_lowercase();
            }
        }
        if !response_delay.is_zero() {
            tokio::time::sleep(response_delay).await;
        }
        let body = format!("{}{ordinal}", observer.tag);
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n{body}",
            body.len()
        );
        if stream.write_all(response.as_bytes()).await.is_err() {
            return;
        }
        buffer.clear();
    }
}

impl DualAddressServer {
    /// The candidate snapshot a policy-enabled transport would build for this
    /// server, in resolver order.
    pub(crate) fn candidates(&self, addresses: &[std::net::IpAddr], ttl: Duration) -> CandidateSet {
        CandidateSet {
            host: self.host().to_string(),
            port: self.port(),
            addresses: addresses.to_vec(),
            valid_until: Instant::now() + ttl,
        }
    }
}
