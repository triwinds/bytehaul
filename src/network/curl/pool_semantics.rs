//! P0 pool-semantics experiments for the libcurl backend.
//!
//! `docs/libcurl-migration-plan.zh-CN.md` §4.4 makes connection-pool semantics a
//! switch gate: the public `pool_max_idle_per_host` / `pool_idle_timeout`
//! contract must not be silently approximated by unrelated libcurl options.
//! These tests measure how the `curl` crate's `Multi` options behave against a
//! local server that counts accepted connections, requests per connection, and
//! how long sockets stay open, so the P3 mapping is based on observations
//! instead of documentation guesses.
//!
//! Measurements from this module are written up in
//! `docs/libcurl-pool-semantics.zh-CN.md`.
//!
//! Run with `cargo test --features curl-backend curl::pool_semantics`.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use curl::easy::{Easy2, Handler, HttpVersion, List, WriteError};
use curl::multi::{Easy2Handle, Multi};
use parking_lot::Mutex;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::oneshot;

/// Server-side observation of one connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ConnectionObservation {
    pub id: usize,
    pub requests: usize,
    pub closed: bool,
}

/// A local HTTP/1.1 server that counts accepted connections, requests per
/// connection, and whether a socket is still open.
struct CountingServer {
    addr: SocketAddr,
    accepted: Arc<AtomicUsize>,
    live: Arc<AtomicUsize>,
    peak_live: Arc<AtomicUsize>,
    observations: Arc<Mutex<HashMap<usize, ConnectionObservation>>>,
    shutdown: Option<oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<()>,
}

impl CountingServer {
    async fn start(response_delay: Duration) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let accepted = Arc::new(AtomicUsize::new(0));
        let live = Arc::new(AtomicUsize::new(0));
        let peak_live = Arc::new(AtomicUsize::new(0));
        let observations = Arc::new(Mutex::new(HashMap::new()));
        let (shutdown, mut shutdown_rx) = oneshot::channel::<()>();

        let task_accepted = accepted.clone();
        let task_live = live.clone();
        let task_peak = peak_live.clone();
        let task_observations = observations.clone();
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
                let id = task_accepted.fetch_add(1, Ordering::SeqCst);
                let now_live = task_live.fetch_add(1, Ordering::SeqCst) + 1;
                task_peak.fetch_max(now_live, Ordering::SeqCst);
                task_observations.lock().insert(
                    id,
                    ConnectionObservation {
                        id,
                        requests: 0,
                        closed: false,
                    },
                );
                let observations = task_observations.clone();
                let live = task_live.clone();
                tokio::spawn(async move {
                    serve(stream, observations.clone(), id, response_delay).await;
                    observations
                        .lock()
                        .entry(id)
                        .and_modify(|entry| entry.closed = true);
                    live.fetch_sub(1, Ordering::SeqCst);
                });
            }
        });

        Self {
            addr,
            accepted,
            live,
            peak_live,
            observations,
            shutdown: Some(shutdown),
            task,
        }
    }

    fn url(&self, path: &str, host: &str) -> String {
        format!("http://{host}:{}{path}", self.addr.port())
    }

    fn accepted(&self) -> usize {
        self.accepted.load(Ordering::SeqCst)
    }

    fn live(&self) -> usize {
        self.live.load(Ordering::SeqCst)
    }

    /// Shared liveness counter, so a blocking experiment can observe the
    /// server while it holds a `!Send` multi handle.
    fn live_handle(&self) -> Arc<AtomicUsize> {
        self.live.clone()
    }

    fn peak_live(&self) -> usize {
        self.peak_live.load(Ordering::SeqCst)
    }

    fn requests_on_connection(&self, id: usize) -> usize {
        self.observations
            .lock()
            .get(&id)
            .map(|entry| entry.requests)
            .unwrap_or(0)
    }

    /// Waits until only `expected` connections remain open.
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

    async fn stop(mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        let _ = tokio::time::timeout(Duration::from_secs(5), self.task).await;
    }
}

async fn serve(
    mut stream: tokio::net::TcpStream,
    observations: Arc<Mutex<HashMap<usize, ConnectionObservation>>>,
    id: usize,
    response_delay: Duration,
) {
    let mut buffer = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match stream.read(&mut byte).await {
            Ok(0) => return,
            Ok(_) => {
                buffer.push(byte[0]);
                if buffer.ends_with(b"\r\n\r\n") {
                    observations
                        .lock()
                        .entry(id)
                        .and_modify(|entry| entry.requests += 1);
                    if !response_delay.is_zero() {
                        tokio::time::sleep(response_delay).await;
                    }
                    let body = format!("conn-{id}");
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n{body}",
                        body.len()
                    );
                    if stream.write_all(response.as_bytes()).await.is_err() {
                        return;
                    }
                    buffer.clear();
                } else if buffer.len() > 64 * 1024 {
                    return;
                }
            }
            Err(_) => return,
        }
    }
}

/// Collects one response body so a trivial handler can drive libcurl.
struct Collector {
    body: Vec<u8>,
}

impl Handler for Collector {
    fn write(&mut self, data: &[u8]) -> Result<usize, WriteError> {
        self.body.extend_from_slice(data);
        Ok(data.len())
    }
}

/// Options exercised by the pool experiments.
#[derive(Clone, Copy, Debug, Default)]
struct PoolOptions {
    max_connects: Option<usize>,
    max_host_connections: Option<usize>,
    max_age_conn: Option<Duration>,
    forbid_reuse: bool,
}

/// One GET request for the sequential helper.
#[derive(Clone)]
struct Request {
    url: String,
    resolve: Option<String>,
}

fn new_multi(options: PoolOptions) -> Multi {
    let mut multi = Multi::new();
    if let Some(max_connects) = options.max_connects {
        multi.set_max_connects(max_connects).unwrap();
    }
    if let Some(max_host) = options.max_host_connections {
        multi.set_max_host_connections(max_host).unwrap();
    }
    multi
}

fn new_request(multi: &Multi, request: &Request, options: PoolOptions) -> Easy2Handle<Collector> {
    let mut easy = Easy2::new(Collector { body: Vec::new() });
    easy.url(&request.url).unwrap();
    easy.http_version(HttpVersion::V11).unwrap();
    easy.noproxy("*").unwrap();
    easy.connect_timeout(Duration::from_secs(5)).unwrap();
    easy.http_content_decoding(false).unwrap();
    if options.forbid_reuse {
        easy.fresh_connect(true).unwrap();
        easy.forbid_reuse(true).unwrap();
    }
    if let Some(max_age) = options.max_age_conn {
        easy.maxage_conn(max_age).unwrap();
    }
    if let Some(resolve) = request.resolve.as_deref() {
        let mut list = List::new();
        list.append(resolve).unwrap();
        easy.resolve(list).unwrap();
    }
    multi.add2(easy).unwrap()
}

/// Drives one handle to completion and returns its body and connect count.
fn drive_until_done(multi: &Multi, handle: Easy2Handle<Collector>) -> (Vec<u8>, u64) {
    loop {
        multi.perform().unwrap();
        let mut finished = false;
        multi.messages(|message| {
            if message.is_for2(&handle) {
                if let Some(result) = message.result() {
                    result.unwrap();
                    finished = true;
                }
            }
        });
        if finished {
            break;
        }
        let wait = multi
            .get_timeout()
            .ok()
            .flatten()
            .unwrap_or(Duration::from_millis(20))
            .min(Duration::from_millis(20));
        multi.wait(&mut [], wait).unwrap();
    }

    let connects = handle.num_connects().unwrap();
    let mut easy = multi.remove2(handle).unwrap();
    (std::mem::take(&mut easy.get_mut().body), connects)
}

/// Runs `requests` sequentially on one `Multi`.
fn run_sequential(options: PoolOptions, requests: &[Request]) -> Vec<(Vec<u8>, u64)> {
    let multi = new_multi(options);
    requests
        .iter()
        .map(|request| {
            let handle = new_request(&multi, request, options);
            drive_until_done(&multi, handle)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn probe_max_age_close_timing() {
        let server = CountingServer::start(Duration::ZERO).await;
        let url = server.url("/probe", "127.0.0.1");
        let live_probe = server.live_handle();

        tokio::task::spawn_blocking(move || {
            for max_age in [None, Some(Duration::from_secs(1))] {                let multi = Multi::new();
                let mut easy = Easy2::new(Collector { body: Vec::new() });
                easy.url(&url).unwrap();
                easy.noproxy("*").unwrap();
                easy.http_version(HttpVersion::V11).unwrap();
                if let Some(age) = max_age {
                    easy.maxage_conn(age).unwrap();
                }
                let handle = multi.add2(easy).unwrap();
                let (_, connects) = drive_until_done(&multi, handle);
                let immediate = live_probe.load(Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(300));
                let after_300 = live_probe.load(Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(1200));
                let after_1500 = live_probe.load(Ordering::SeqCst);
                println!(
                    "max_age={max_age:?} connects={connects} live: +0ms={immediate} +300ms={after_300} +1500ms={after_1500}"
                );
                drop(multi);
            }
        })
        .await
        .unwrap();

        server.stop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn idle_connection_is_reused_by_the_next_sequential_transfer() {
        let server = CountingServer::start(Duration::ZERO).await;
        let requests = vec![
            Request {
                url: server.url("/reuse", "127.0.0.1"),
                resolve: None,
            };
            3
        ];

        let results =
            tokio::task::spawn_blocking(move || run_sequential(PoolOptions::default(), &requests))
                .await
                .unwrap();

        assert_eq!(results.len(), 3);
        assert_eq!(server.accepted(), 1, "one socket must serve three requests");
        assert_eq!(server.requests_on_connection(0), 3);
        assert_eq!(results[0].1, 1, "the first request must connect");
        assert_eq!(results[1].1, 0, "the second request must reuse the socket");
        assert_eq!(results[2].1, 0, "the third request must reuse the socket");

        server.stop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn forbid_reuse_disables_pooling_and_closes_each_socket() {
        let server = CountingServer::start(Duration::ZERO).await;
        let requests = vec![
            Request {
                url: server.url("/no-pool", "127.0.0.1"),
                resolve: None,
            };
            2
        ];

        let options = PoolOptions {
            forbid_reuse: true,
            ..PoolOptions::default()
        };
        let results = tokio::task::spawn_blocking(move || run_sequential(options, &requests))
            .await
            .unwrap();

        assert_eq!(
            results
                .iter()
                .map(|(_, connects)| *connects)
                .collect::<Vec<_>>(),
            vec![1, 1],
            "pool_max_idle_per_host = 0 must forbid reuse"
        );
        assert_eq!(server.accepted(), 2);
        assert_eq!(
            server.wait_for_live(0, Duration::from_secs(2)).await,
            0,
            "sockets that may not be reused must be closed after the transfer"
        );

        server.stop().await;
    }

    /// Experiment 3: `CURLMOPT_MAXCONNECTS` is a cache bound, and libcurl
    /// applies it when a connection *becomes idle* - not when a new one is
    /// created.
    ///
    /// Two sequential requests to two origins with a bound of one: the first
    /// socket stays open after its own transfer (the cache holds one entry,
    /// which the bound allows) and is evicted when the second one goes idle.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn max_connects_bounds_the_cache_at_the_next_idle_transition() {
        let first = CountingServer::start(Duration::ZERO).await;
        let second = CountingServer::start(Duration::ZERO).await;
        let (first_url, second_url) = (
            first.url("/one", "127.0.0.1"),
            second.url("/two", "127.0.0.1"),
        );
        let options = PoolOptions {
            max_connects: Some(1),
            ..PoolOptions::default()
        };

        let live_probe = (first.live_handle(), second.live_handle());
        let samples = tokio::task::spawn_blocking(move || {
            let multi = new_multi(options);
            let first = drive_until_done(
                &multi,
                new_request(
                    &multi,
                    &Request {
                        url: first_url,
                        resolve: None,
                    },
                    options,
                ),
            );
            let after_first = live_probe.0.load(Ordering::SeqCst);
            // Space the two idle moments apart: libcurl ranks cached
            // connections by how long they have been idle, and two sockets that
            // went idle in the same millisecond would tie.
            std::thread::sleep(Duration::from_millis(150));
            let second = drive_until_done(
                &multi,
                new_request(
                    &multi,
                    &Request {
                        url: second_url,
                        resolve: None,
                    },
                    options,
                ),
            );
            // Sampled while the multi is still alive: dropping it would close
            // every cached socket and hide what the bound did.
            std::thread::sleep(Duration::from_millis(200));
            let after_second = (
                live_probe.0.load(Ordering::SeqCst),
                live_probe.1.load(Ordering::SeqCst),
            );
            ((first.1, second.1), after_first, after_second)
        })
        .await
        .unwrap();

        assert_eq!(samples.0, (1, 1), "two origins need two connections");
        assert_eq!(
            samples.1, 1,
            "the first socket stays cached while it is the only entry"
        );
        assert_eq!(
            samples.2,
            (0, 1),
            "the second connection going idle evicts the one that has been idle longest"
        );

        first.stop().await;
        second.stop().await;
    }

    /// Experiment 3b: the bound is a cache bound, never a concurrency bound.
    ///
    /// Three concurrent transfers may open three connections with a bound of
    /// one - libcurl only ever closes connections that are idle - and once they
    /// all finished the cache is down to the bound.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn max_connects_prunes_idle_sockets_after_concurrent_transfers() {
        let server = CountingServer::start(Duration::from_millis(150)).await;
        let base = server.url("/wave", "127.0.0.1");
        let options = PoolOptions {
            max_connects: Some(1),
            ..PoolOptions::default()
        };
        let live_probe = server.live_handle();

        let outcome = tokio::task::spawn_blocking(move || {
            let multi = new_multi(options);
            let mut pending: Vec<Easy2Handle<Collector>> = (0..3)
                .map(|index| {
                    new_request(
                        &multi,
                        &Request {
                            url: format!("{base}?i={index}"),
                            resolve: None,
                        },
                        options,
                    )
                })
                .collect();
            let mut connects: Vec<u64> = Vec::new();
            let deadline = Instant::now() + Duration::from_secs(10);

            while !pending.is_empty() && Instant::now() < deadline {
                multi.perform().unwrap();
                // Every completion has to be taken in the same pass: keeping
                // only the last one would drop a handle forever.
                let mut finished: Vec<usize> = Vec::new();
                multi.messages(|message| {
                    if let Some(result) = message.result() {
                        result.unwrap();
                        for (index, handle) in pending.iter().enumerate() {
                            if message.is_for2(handle) && !finished.contains(&index) {
                                finished.push(index);
                            }
                        }
                    }
                });
                for index in finished.into_iter().rev() {
                    let handle = pending.remove(index);
                    connects.push(handle.num_connects().unwrap());
                    let _ = multi.remove2(handle);
                }
                let wait = multi
                    .get_timeout()
                    .ok()
                    .flatten()
                    .unwrap_or(Duration::from_millis(20))
                    .min(Duration::from_millis(20));
                multi.wait(&mut [], wait).unwrap();
            }
            // Sampled before the multi is dropped, so the eviction is what the
            // server sees and not the cleanup of the multi handle.
            std::thread::sleep(Duration::from_millis(200));
            (connects, live_probe.load(Ordering::SeqCst))
        })
        .await
        .unwrap();

        assert_eq!(outcome.0.len(), 3, "all three transfers must finish");
        assert_eq!(
            outcome.0,
            vec![1, 1, 1],
            "the cache bound must not queue transfers on one connection"
        );
        assert_eq!(
            server.peak_live(),
            3,
            "a cache bound must not cap simultaneous connections"
        );
        assert_eq!(
            outcome.1, 1,
            "once the wave is over the cache keeps exactly the bound"
        );

        server.stop().await;
    }

    /// Experiment 3c: lowering the bound does not close anything by itself.
    ///
    /// This is the reason `pool_idle_timeout` cannot be implemented with
    /// `CURLMOPT_MAXCONNECTS` alone: an already idle socket is only evicted by
    /// the *next* connection that becomes idle.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn lowering_the_cache_bound_waits_for_a_connection_to_become_idle() {
        let first = CountingServer::start(Duration::ZERO).await;
        let second = CountingServer::start(Duration::ZERO).await;
        let third = CountingServer::start(Duration::ZERO).await;
        let (first_url, second_url, third_url) = (
            first.url("/one", "127.0.0.1"),
            second.url("/two", "127.0.0.1"),
            third.url("/three", "127.0.0.1"),
        );

        let probes = (
            first.live_handle(),
            second.live_handle(),
            third.live_handle(),
        );
        let samples = tokio::task::spawn_blocking(move || {
            let mut multi = new_multi(PoolOptions::default());
            drive_until_done(
                &multi,
                new_request(
                    &multi,
                    &Request {
                        url: first_url,
                        resolve: None,
                    },
                    PoolOptions::default(),
                ),
            );
            // Space the idle moments apart: libcurl evicts the connection that
            // has been idle longest, and two sockets that went idle in the same
            // millisecond would tie.
            std::thread::sleep(Duration::from_millis(150));
            drive_until_done(
                &multi,
                new_request(
                    &multi,
                    &Request {
                        url: second_url,
                        resolve: None,
                    },
                    PoolOptions::default(),
                ),
            );
            let before = (
                probes.0.load(Ordering::SeqCst),
                probes.1.load(Ordering::SeqCst),
            );

            // Two cached sockets, bound lowered to one: the option itself
            // cannot close them.
            multi.set_max_connects(1).unwrap();
            std::thread::sleep(Duration::from_millis(300));
            let after_lowering = (
                probes.0.load(Ordering::SeqCst),
                probes.1.load(Ordering::SeqCst),
            );

            // A third origin opens a third connection; when that one goes idle,
            // libcurl closes the oldest cached socket.
            drive_until_done(
                &multi,
                new_request(
                    &multi,
                    &Request {
                        url: third_url,
                        resolve: None,
                    },
                    PoolOptions::default(),
                ),
            );
            std::thread::sleep(Duration::from_millis(200));
            let after_next_idle = (
                probes.0.load(Ordering::SeqCst),
                probes.1.load(Ordering::SeqCst),
                probes.2.load(Ordering::SeqCst),
            );
            (before, after_lowering, after_next_idle)
        })
        .await
        .unwrap();

        assert_eq!(samples.0, (1, 1), "two origins cache two sockets");
        assert_eq!(
            samples.1, samples.0,
            "lowering CURLMOPT_MAXCONNECTS must not close an idle socket on its own"
        );
        assert_eq!(
            samples.2,
            (0, 1, 1),
            "the next idle transition evicts the oldest cached socket"
        );

        first.stop().await;
        second.stop().await;
        third.stop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn max_host_connections_caps_active_connections() {
        let server = CountingServer::start(Duration::from_millis(150)).await;
        let base = server.url("/concurrent", "127.0.0.1");

        let options = PoolOptions {
            max_host_connections: Some(1),
            ..PoolOptions::default()
        };
        let (elapsed, connects) = tokio::task::spawn_blocking(move || {
            let multi = new_multi(options);
            let started = Instant::now();
            let handles: Vec<_> = (0..3)
                .map(|index| {
                    new_request(
                        &multi,
                        &Request {
                            url: format!("{base}?i={index}"),
                            resolve: None,
                        },
                        options,
                    )
                })
                .collect();

            let mut done = 0;
            while done < handles.len() {
                multi.perform().unwrap();
                multi.messages(|message| {
                    if let Some(result) = message.result() {
                        result.unwrap();
                        done += 1;
                    }
                });
                let wait = multi
                    .get_timeout()
                    .ok()
                    .flatten()
                    .unwrap_or(Duration::from_millis(20))
                    .min(Duration::from_millis(20));
                multi.wait(&mut [], wait).unwrap();
            }
            let connects: Vec<u64> = handles
                .iter()
                .map(|handle| handle.num_connects().unwrap())
                .collect();
            for handle in handles {
                let _ = multi.remove2(handle);
            }
            (started.elapsed(), connects)
        })
        .await
        .unwrap();

        assert_eq!(connects[0], 1, "the first transfer opens the socket");
        assert_eq!(
            connects.iter().skip(1).sum::<u64>(),
            0,
            "queued transfers join the existing socket instead of opening new ones"
        );
        assert_eq!(
            server.peak_live(),
            1,
            "MAX_HOST_CONNECTIONS must cap simultaneous connections"
        );
        assert_eq!(server.accepted(), 1, "connections must queue, not multiply");
        assert!(
            elapsed >= Duration::from_millis(400),
            "three 150 ms responses over one connection must serialize, took {elapsed:?}"
        );

        server.stop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn max_age_conn_is_only_checked_on_reuse() {
        let server = CountingServer::start(Duration::ZERO).await;
        let url = server.url("/max-age", "127.0.0.1");
        // `CURLOPT_MAXAGE_CONN` is a whole-second option, so 1 s is the
        // smallest age that can be expressed.
        let options = PoolOptions {
            max_age_conn: Some(Duration::from_secs(1)),
            ..PoolOptions::default()
        };

        let live_probe = server.live_handle();
        let results = tokio::task::spawn_blocking(move || {
            let multi = new_multi(options);
            let request = Request { url, resolve: None };
            let first = drive_until_done(&multi, new_request(&multi, &request, options));
            std::thread::sleep(Duration::from_millis(1500));
            // Sampled before the next transfer: the aged socket must still be
            // open, because the age check only runs when a reuse is attempted.
            let aged_live = live_probe.load(Ordering::SeqCst);
            let second = drive_until_done(&multi, new_request(&multi, &request, options));
            (first.1, second.1, aged_live)
        })
        .await
        .unwrap();

        assert_eq!(
            (results.0, results.1),
            (1, 1),
            "an aged idle connection must not be reused"
        );
        assert_eq!(
            results.2, 1,
            "MAXAGE_CONN does not close the aged socket on its own"
        );
        assert_eq!(server.accepted(), 2);
        // The stale socket is dropped as part of the reuse attempt; the fresh
        // one replaces it (its close is only observed asynchronously by the
        // server, so the post-transfer socket count is not asserted here).

        server.stop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn resolve_entry_does_not_rebind_an_already_pooled_connection() {
        let server = CountingServer::start(Duration::ZERO).await;
        let port = server.addr.port();
        let requests = vec![
            Request {
                url: format!("http://pinned.invalid:{port}/resolve"),
                resolve: Some(format!("pinned.invalid:{port}:127.0.0.1")),
            };
            2
        ];

        let results =
            tokio::task::spawn_blocking(move || run_sequential(PoolOptions::default(), &requests))
                .await
                .unwrap();

        assert_eq!(
            results
                .iter()
                .map(|(_, connects)| *connects)
                .collect::<Vec<_>>(),
            vec![1, 0],
            "the pooled socket is reused even though RESOLVE is set again"
        );
        assert_eq!(server.accepted(), 1);
        assert_eq!(server.requests_on_connection(0), 2);

        server.stop().await;
    }

    /// P3 question from `docs/libcurl-pool-semantics.zh-CN.md` §4: is it safe
    /// to move `CURLMOPT_MAXCONNECTS` while transfers are in flight?
    ///
    /// The driver sets it once per pool, but the answer still matters for a
    /// configuration that lowers the bound: moving it must not abort a running
    /// transfer or truncate its body. The probe runs the harshest case -
    /// dropping the bound to one while two connections are already open - and
    /// has a hard deadline, so a wedge fails the test instead of hanging the
    /// suite.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn max_connects_can_shrink_while_transfers_are_active() {
        let server = CountingServer::start(Duration::from_millis(150)).await;
        let url = server.url("/dynamic", "127.0.0.1");

        let outcome = tokio::task::spawn_blocking(move || {
            let options = PoolOptions::default();
            let mut multi = new_multi(options);
            let request = Request { url, resolve: None };
            let mut pending = vec![
                new_request(&multi, &request, options),
                new_request(&multi, &request, options),
            ];
            let mut bodies: Vec<Vec<u8>> = Vec::new();
            let mut shrunk = false;
            let deadline = std::time::Instant::now() + Duration::from_secs(10);

            while !pending.is_empty() {
                if std::time::Instant::now() > deadline {
                    return bodies;
                }
                multi.perform().unwrap();
                // Every completion has to be taken in the same pass: two
                // simultaneous responses arrive as two messages, and keeping
                // only the last one would drop a handle forever.
                let mut finished: Vec<usize> = Vec::new();
                multi.messages(|message| {
                    if let Some(result) = message.result() {
                        result.unwrap();
                        for (index, handle) in pending.iter().enumerate() {
                            if message.is_for2(handle) && !finished.contains(&index) {
                                finished.push(index);
                            }
                        }
                    }
                });
                for index in finished.into_iter().rev() {
                    let mut easy = multi.remove2(pending.remove(index)).unwrap();
                    bodies.push(std::mem::take(&mut easy.get_mut().body));
                }
                if !shrunk {
                    // Both transfers are running on their own connection: drop
                    // the cache bound below them, which is harsher than any
                    // update the driver performs.
                    multi.set_max_connects(1).unwrap();
                    shrunk = true;
                }
                let wait = multi
                    .get_timeout()
                    .ok()
                    .flatten()
                    .unwrap_or(Duration::from_millis(20))
                    .min(Duration::from_millis(20));
                let _ = multi.wait(&mut [], wait);
            }
            bodies
        })
        .await
        .unwrap();

        assert_eq!(
            outcome.len(),
            2,
            "an in-flight transfer must survive a MAXCONNECTS update"
        );
        assert!(
            outcome
                .iter()
                .all(|body| String::from_utf8_lossy(body).starts_with("conn-")),
            "both bodies must be complete, got: {outcome:?}"
        );
        let mut distinct = outcome.clone();
        distinct.sort();
        distinct.dedup();
        assert_eq!(
            distinct.len(),
            2,
            "each concurrent transfer ran on its own connection, got: {outcome:?}"
        );
        assert_eq!(
            server.peak_live(),
            2,
            "the two transfers have to overlap for this to test anything"
        );

        server.stop().await;
    }

    /// Experiment 8: `CURLMOPT_NETWORK_CHANGED` with `CURLMNWC_CLEAR_CONNS`
    /// closes the sockets a multi handle has cached without touching the
    /// transfer that is still running (`curl/multi.h`: "Connections that are
    /// idle are closed. Ongoing transfers do continue with the connection they
    /// have").
    ///
    /// This is the mechanism the driver uses to implement `pool_idle_timeout`
    /// for a pool that keeps running a long transfer: `CURLMOPT_MAXCONNECTS`
    /// only prunes at an idle transition (experiment 3c) and
    /// `CURLOPT_MAXAGE_CONN` only refuses a reuse.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn clear_conns_closes_idle_sockets_and_keeps_a_running_transfer() {
        use std::ffi::c_long;

        // Connection 0 streams a body slowly, connection 1 answers at once.
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let live = Arc::new(AtomicUsize::new(0));
        let closed = Arc::new(AtomicUsize::new(0));
        let server_live = live.clone();
        let server_closed = closed.clone();
        let server = tokio::spawn(async move {
            let mut id = 0usize;
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                server_live.fetch_add(1, Ordering::SeqCst);
                let slow = id == 0;
                id += 1;
                let live = server_live.clone();
                let closed = server_closed.clone();
                tokio::spawn(async move {
                    let mut buffer = Vec::new();
                    let mut byte = [0u8; 1];
                    loop {
                        buffer.clear();
                        loop {
                            match stream.read(&mut byte).await {
                                Ok(0) | Err(_) => {
                                    live.fetch_sub(1, Ordering::SeqCst);
                                    closed.fetch_add(1, Ordering::SeqCst);
                                    return;
                                }
                                Ok(_) => buffer.push(byte[0]),
                            }
                            if buffer.ends_with(b"\r\n\r\n") {
                                break;
                            }
                        }
                        let body = if slow {
                            vec![b's'; 96 * 1024]
                        } else {
                            b"fast".to_vec()
                        };
                        let response = format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n",
                            body.len()
                        );
                        if stream.write_all(response.as_bytes()).await.is_err() {
                            live.fetch_sub(1, Ordering::SeqCst);
                            return;
                        }
                        let chunk = if slow { 2 * 1024 } else { body.len() };
                        let mut written = 0;
                        while written < body.len() {
                            let end = (written + chunk).min(body.len());
                            if stream.write_all(&body[written..end]).await.is_err() {
                                live.fetch_sub(1, Ordering::SeqCst);
                                return;
                            }
                            written = end;
                            if slow {
                                tokio::time::sleep(Duration::from_millis(20)).await;
                            }
                        }
                    }
                });
            }
        });

        let slow_url = format!("http://127.0.0.1:{}/slow", addr.port());
        let fast_url = format!("http://127.0.0.1:{}/fast", addr.port());
        let live_probe = live.clone();
        let clear_code = tokio::task::spawn_blocking(move || {
            let multi = Multi::new();
            let mut slow = new_request(
                &multi,
                &Request {
                    url: slow_url,
                    resolve: None,
                },
                PoolOptions::default(),
            );
            let mut slow_body: Vec<u8> = Vec::new();
            // The slow transfer owns connection 0; drive it until its body
            // started before the fast one connects.
            let started = Instant::now();
            while slow_body.is_empty() && started.elapsed() < Duration::from_secs(5) {
                multi.perform().unwrap();
                multi.wait(&mut [], Duration::from_millis(10)).unwrap();
                slow_body.extend_from_slice(&slow.get_ref().body);
                slow.get_mut().body.clear();
            }
            let fast = drive_until_done(
                &multi,
                new_request(
                    &multi,
                    &Request {
                        url: fast_url,
                        resolve: None,
                    },
                    PoolOptions::default(),
                ),
            );
            let before = live_probe.load(Ordering::SeqCst);

            // libcurl 8.21: `CURLMOPT_NETWORK_CHANGED` with `CLEAR_CONNS`.
            let code = unsafe {
                curl_sys::curl_multi_setopt(
                    multi.raw(),
                    curl_sys::CURLOPTTYPE_LONG + 17,
                    2 as c_long,
                )
            };
            // Give the idle socket's close a moment to be observed, without
            // letting the slow body finish first.
            let observing = Instant::now();
            while observing.elapsed() < Duration::from_millis(200) {
                multi.perform().unwrap();
                multi.wait(&mut [], Duration::from_millis(10)).unwrap();
                slow_body.extend_from_slice(&slow.get_ref().body);
                slow.get_mut().body.clear();
            }
            let after = live_probe.load(Ordering::SeqCst);
            let running_bytes = slow_body.len();

            // The running transfer has to finish untouched.
            let deadline = Instant::now() + Duration::from_secs(10);
            while slow_body.len() < 96 * 1024 && Instant::now() < deadline {
                multi.perform().unwrap();
                multi.wait(&mut [], Duration::from_millis(10)).unwrap();
                slow_body.extend_from_slice(&slow.get_ref().body);
                slow.get_mut().body.clear();
            }
            let _ = multi.remove2(slow);
            (code, before, after, fast.0, running_bytes, slow_body.len())
        })
        .await
        .unwrap();

        assert_eq!(
            clear_code.0, 0,
            "CURLMOPT_NETWORK_CHANGED must be accepted by this libcurl"
        );
        assert_eq!(clear_code.1, 2, "one streaming and one idle connection");
        assert_eq!(
            clear_code.2, 1,
            "the idle socket must be closed while the other transfer runs"
        );
        assert_eq!(clear_code.3, b"fast".to_vec());
        assert!(
            clear_code.4 < 96 * 1024,
            "the measured transfer has to be still running, got {} bytes",
            clear_code.4
        );
        assert_eq!(
            clear_code.5,
            96 * 1024,
            "the running transfer must deliver its whole body"
        );

        server.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dropping_the_multi_closes_cached_idle_sockets() {
        let server = CountingServer::start(Duration::ZERO).await;
        let url = server.url("/drop", "127.0.0.1");

        tokio::task::spawn_blocking(move || {
            let options = PoolOptions::default();
            let multi = new_multi(options);
            let request = Request { url, resolve: None };
            let (_, connects) = drive_until_done(&multi, new_request(&multi, &request, options));
            assert_eq!(connects, 1);
            // The socket is now idle inside the multi connection cache.
            drop(multi);
        })
        .await
        .unwrap();

        assert_eq!(
            server.wait_for_live(0, Duration::from_secs(2)).await,
            0,
            "destroying the multi handle must close idle sockets"
        );

        server.stop().await;
    }
}
