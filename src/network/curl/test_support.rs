//! Shared fixtures for the libcurl backend tests.
//!
//! Kept outside `driver` so transport-level tests can run a real server without
//! depending on the driver's own test module.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use parking_lot::Mutex;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

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
