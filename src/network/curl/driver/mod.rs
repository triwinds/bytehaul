//! libcurl transfer driver (P2 driver, P3 compatibility work).
//!
//! One dedicated thread owns every `Multi` and `Easy2` handle; Tokio only holds
//! a command queue and the bounded body queues. The invariants the session
//! layer relies on:
//!
//! * `Multi`/`Easy2` handles never leave the driver thread.
//! * Response headers are published as soon as the header block is complete,
//!   without waiting for the body.
//! * The body reaches Tokio through one bounded byte budget; exhausting it
//!   pauses libcurl through the write callback and never enqueues a partial
//!   chunk (a paused chunk is redelivered by libcurl after `unpause`).
//! * Cancelling or dropping a transfer removes the handle from its pool, so no
//!   transfer is left behind.
//! * Driver failures surface as explicit errors instead of a silent EOF.
//!
//! P3 adds the pieces the migration plan (§4.3, §4.4) requires before the
//! default backend can flip:
//!
//! * **Pools.** One `Multi` per `(origin, proxy route)`, because libcurl
//!   caches connections and resolved addresses per multi handle. The pool key
//!   is what keeps `pool_max_idle_per_host` / `pool_idle_timeout` meaningful:
//!   `CURLMOPT_MAXCONNECTS` is set to `max_idle_per_host`, which makes libcurl
//!   close the oldest idle connection whenever a transfer finishes and the
//!   cache is above the bound. A pool whose last transfer finished is destroyed
//!   once `pool_idle_timeout` elapsed; a pool that keeps running a long
//!   transfer closes just its idle connections then, through
//!   `CURLMOPT_NETWORK_CHANGED`.
//! * **Resolved addresses.** `CURLOPT_RESOLVE` entries are injected per pool
//!   with the TTL Hickory reported, refreshed when the TTL expires or the
//!   answer changes. A changed answer forces a fresh connection so a transfer
//!   never rides a socket pinned to the previous address.
//! * **Diagnostics.** Command-queue latency, submitted/completed transfers,
//!   real connection count (`CURLINFO_NUM_CONNECTS`) and pause/resume counts
//!   are recorded and reported through [`DriverStats`].
//!
//! See `docs/libcurl-migration-plan.zh-CN.md` and
//! `docs/libcurl-pool-semantics.zh-CN.md`.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use bytes::Bytes;
use curl::easy::{Easy2, Handler, HttpVersion, List, WriteError};
use curl::multi::{Easy2Handle, Multi};
use parking_lot::{Condvar, Mutex};
use tokio::sync::{oneshot, Notify};

use crate::error::{DownloadError, TransportError, TransportErrorKind};

/// Maximum time the driver blocks in `Multi::wait` before it re-checks its
/// command queue. Bounded waiting keeps command latency low without a
/// per-transfer socket registration.
const DRIVER_WAIT_SLICE: Duration = Duration::from_millis(20);

/// How long the driver blocks on the command queue when it has no transfer
/// and no idle pool deadline to wait for.
const DRIVER_IDLE_WAIT: Duration = Duration::from_secs(30);

/// Default `CURLOPT_MAXAGE_CONN`: a second line of defence so an idle socket
/// is not reused indefinitely when the pool's own reclamation is coarse.
const DEFAULT_MAX_AGE_CONN: Duration = Duration::from_secs(118);

/// Identifies one transfer submitted to a driver.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct TransferId(u64);

impl TransferId {
    fn next() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Self(NEXT.fetch_add(1, Ordering::Relaxed))
    }

    fn raw(self) -> usize {
        self.0 as usize
    }
}

/// Pool identity: connections (and resolved addresses) are cached per pool.
///
/// libcurl caches connections and `CURLOPT_RESOLVE` entries on the multi
/// handle, so a pool may only ever contain transfers that are allowed to share
/// a connection: the same origin over the same proxy route. Network
/// configuration differences never meet here because one driver belongs to one
/// `ClientNetworkConfig`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct PoolKey {
    /// `scheme://host:port`, lowercased.
    origin: String,
    /// Proxy endpoint for this route, `None` for a direct connection.
    proxy: Option<String>,
}

impl PoolKey {
    /// Derives the pool a request belongs to from its URL and proxy route.
    pub(crate) fn for_request(url: &str, proxy: Option<&str>) -> Self {
        let origin = match url::Url::parse(url) {
            Ok(parsed) => match (parsed.host_str(), parsed.port_or_known_default()) {
                (Some(host), Some(port)) => format!(
                    "{}://{}:{port}",
                    parsed.scheme().to_ascii_lowercase(),
                    host.to_ascii_lowercase()
                ),
                _ => url.to_string(),
            },
            Err(_) => url.to_string(),
        };
        Self {
            origin,
            proxy: proxy.map(|value| value.to_string()),
        }
    }

    #[cfg(test)]
    pub(crate) fn origin(&self) -> &str {
        &self.origin
    }
}

/// Connection-pool policy for one driver.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DriverConfig {
    /// `pool_max_idle_per_host`: idle connections kept per pool. `0` disables
    /// connection reuse entirely (the pre-P3 contract for that setting).
    pub max_idle_per_host: usize,
    /// How long a pool may stay fully idle before its sockets are closed.
    pub pool_idle_timeout: Duration,
    /// Upper bound for `CURLOPT_MAXAGE_CONN`; `None` leaves libcurl's default.
    pub max_age_conn: Option<Duration>,
}

impl Default for DriverConfig {
    fn default() -> Self {
        Self {
            max_idle_per_host: crate::config::DEFAULT_HTTP_IDLE_POOL_MAX_PER_HOST,
            pool_idle_timeout: crate::config::DEFAULT_HTTP_IDLE_POOL_TIMEOUT,
            max_age_conn: None,
        }
    }
}

impl DriverConfig {
    /// `CURLOPT_MAXAGE_CONN` is in whole seconds; below one second the age check
    /// cannot be expressed, so the pool's own reclamation stays in charge.
    fn max_age_conn_secs(&self) -> Option<u32> {
        let max_age = self
            .max_age_conn
            .unwrap_or(self.pool_idle_timeout)
            .min(DEFAULT_MAX_AGE_CONN);
        let secs = max_age.as_secs();
        (secs > 0).then_some(secs.min(u32::MAX as u64) as u32)
    }
}

/// One resolved address set injected into a pool's DNS cache.
#[derive(Clone, Debug, PartialEq, Eq)]
struct InjectedResolve {
    /// `host:port:addr[,addr]` exactly as it was handed to libcurl.
    spec: String,
    injected_at: Instant,
    ttl: Duration,
}

impl InjectedResolve {
    fn is_expired(&self, now: Instant) -> bool {
        now.duration_since(self.injected_at) >= self.ttl
    }
}

/// A `CURLOPT_RESOLVE` entry prepared by the transport from one Hickory answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ResolveEntry {
    /// `host:port:addr[,addr]`, the wire form libcurl expects. IPv6 addresses
    /// stay in brackets.
    pub spec: String,
    /// `host:port` cache key, used to replace or remove the previous answer.
    pub key: String,
    /// How long Hickory considers the answer valid.
    pub ttl: Duration,
}

impl ResolveEntry {
    /// Builds an entry for one `host:port` hop.
    pub(crate) fn new(host: &str, port: u16, addrs: &[std::net::IpAddr], ttl: Duration) -> Self {
        let rendered: Vec<String> = addrs
            .iter()
            .map(|addr| match addr {
                std::net::IpAddr::V4(ip) => ip.to_string(),
                std::net::IpAddr::V6(ip) => format!("[{ip}]"),
            })
            .collect();
        Self {
            spec: format!("{host}:{port}:{}", rendered.join(",")),
            key: format!("{host}:{port}"),
            ttl,
        }
    }
}

/// What has to be done to one `Easy2` before it joins a pool.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct ResolvePlan {
    /// `CURLOPT_RESOLVE` list: first any removal, then the new answer.
    list: Vec<String>,
    /// The answer changed, so the transfer must not reuse a cached connection.
    force_fresh_connect: bool,
}

/// Everything the driver needs to start one transfer.
#[derive(Clone, Debug)]
pub(crate) struct RequestOptions {
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub range: Option<String>,
    pub connect_timeout: Duration,
    /// Deadline handed to the Tokio caller for the first response head.
    pub head_timeout: Duration,
    /// Mirrors `pool_max_idle_per_host == 0`: never reuse a connection.
    pub forbid_connection_reuse: bool,
    /// Resolved addresses for this hop, injected as `CURLOPT_RESOLVE`.
    pub resolve: Option<ResolveEntry>,
    /// Explicit proxy URL. `None` disables proxies, matching a task that has
    /// no proxy configuration.
    pub proxy: Option<String>,
    /// Extra trust anchors (`CURLOPT_CAINFO`). Certificate and hostname
    /// verification always stay on.
    pub ca_info: Option<std::path::PathBuf>,
    /// Hashed CA directory (`CURLOPT_CAPATH`).
    pub ca_path: Option<std::path::PathBuf>,
    /// Client certificate and private key for mutual TLS.
    pub client_cert: Option<std::path::PathBuf>,
    pub client_key: Option<std::path::PathBuf>,
}

impl RequestOptions {
    pub(crate) fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            headers: Vec::new(),
            range: None,
            connect_timeout: Duration::from_secs(10),
            head_timeout: Duration::from_secs(10),
            forbid_connection_reuse: false,
            resolve: None,
            proxy: None,
            ca_info: None,
            ca_path: None,
            client_cert: None,
            client_key: None,
        }
    }

    /// The pool this request belongs to.
    pub(crate) fn pool_key(&self) -> PoolKey {
        PoolKey::for_request(&self.url, self.proxy.as_deref())
    }
}

/// Response head published before any body byte is read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ResponseHead {
    pub status: u16,
    /// Duplicate-preserving header list, in wire order.
    pub headers: Vec<(String, String)>,
}

impl ResponseHead {
    #[cfg(test)]
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    #[cfg(test)]
    pub(crate) fn content_length(&self) -> Option<u64> {
        self.header("content-length")?.trim().parse().ok()
    }
}

/// Which part of a transfer ended, used for diagnostics and error messages.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TransferPhase {
    /// The failure happened before the origin response head was published,
    /// which is where connect, TLS and proxy negotiation failures land.
    BeforeHeaders,
    /// The head was published: a failure here truncated the body.
    AfterHeaders,
}

impl std::fmt::Display for TransferPhase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::BeforeHeaders => "before response headers",
            Self::AfterHeaders => "after response headers",
        })
    }
}

/// Terminal state of a body stream, as seen by the Tokio consumer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Terminal {
    Eof,
    Cancelled,
    Failed {
        kind: TransportErrorKind,
        code: Option<u32>,
        message: String,
    },
}

impl Terminal {
    pub(crate) fn into_error(self) -> Option<DownloadError> {
        match self {
            Self::Eof => None,
            Self::Cancelled => Some(DownloadError::Cancelled),
            Self::Failed { kind, message, .. } => Some(DownloadError::Transport(
                TransportError::new(kind, std::io::Error::other(message)),
            )),
        }
    }
}

/// Driver-wide counters, reported through [`DriverHandle::stats`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct DriverStats {
    /// Transfers handed to the driver thread.
    pub submitted: u64,
    /// Transfers libcurl finished on its own (successfully or with an error).
    pub completed: u64,
    /// Transfers cancelled before libcurl finished them.
    pub cancelled: u64,
    /// Real TCP/TLS connections opened, summed from `CURLINFO_NUM_CONNECTS`.
    /// This is deliberately separate from `submitted`: request invocations and
    /// connection establishments are different counts.
    pub connections: u64,
    /// Commands processed by the driver thread.
    pub commands: u64,
    /// Pause/unpause round trips caused by body backpressure.
    pub pauses: u64,
    pub resumes: u64,
    /// Times the driver closed the idle connections of a pool that was still
    /// running a transfer, because they had been unused for
    /// `pool_idle_timeout`.
    pub idle_clears: u64,
    /// Longest time a command waited between submission and handling.
    pub max_command_latency: Duration,
    /// Transfers currently in flight.
    pub active: usize,
    /// Pools currently holding connections.
    pub pools: usize,
}

#[derive(Default)]
struct DriverCounters {
    submitted: AtomicU64,
    completed: AtomicU64,
    cancelled: AtomicU64,
    connections: AtomicU64,
    commands: AtomicU64,
    pauses: AtomicU64,
    resumes: AtomicU64,
    idle_clears: AtomicU64,
    max_command_latency_us: AtomicU64,
}

struct SinkState {
    queue: VecDeque<Bytes>,
    buffered: usize,
    terminal: Option<Terminal>,
    paused: bool,
    /// Set when the consumer pops a chunk after a pause, which is the signal
    /// that the driver may unpause the handle again.
    drained_since_pause: bool,
}

/// One atomic observation of the body channel.
enum SinkPoll {
    /// The next chunk, in wire order.
    Chunk(Bytes),
    /// The transfer finished and no data is queued.
    Terminal(Terminal),
    /// Nothing queued, transfer still running.
    Pending,
}

/// Bounded byte channel between the libcurl write callback (driver thread) and
/// one Tokio consumer.
///
/// The budget is measured in bytes, and the callback either enqueues a whole
/// chunk or pauses without touching the queue. Because libcurl redelivers a
/// chunk that returned `WriteError::Pause`, this keeps the byte stream exact.
pub(crate) struct BodySink {
    budget: usize,
    state: Mutex<SinkState>,
    notify: Notify,
    resume: std::sync::Weak<CommandQueue>,
    id: TransferId,
    pause_count: AtomicUsize,
    accepted_bytes: AtomicUsize,
}

impl BodySink {
    fn new(id: TransferId, budget: usize, resume: std::sync::Weak<CommandQueue>) -> Arc<Self> {
        Arc::new(Self {
            budget: budget.max(1),
            state: Mutex::new(SinkState {
                queue: VecDeque::new(),
                buffered: 0,
                terminal: None,
                paused: false,
                drained_since_pause: false,
            }),
            notify: Notify::new(),
            resume,
            id,
            pause_count: AtomicUsize::new(0),
            accepted_bytes: AtomicUsize::new(0),
        })
    }

    /// Write callback body. Runs on the driver thread and never blocks, never
    /// touches Tokio and never waits for backpressure beyond returning pause.
    fn write(&self, data: &[u8]) -> Result<usize, WriteError> {
        if data.is_empty() {
            return Ok(0);
        }
        let mut state = self.state.lock();
        if state.terminal.is_some() {
            // The consumer is gone or the transfer already failed: pause the
            // transfer instead of buffering bytes nobody will read. The driver
            // removes the handle as soon as it sees the cancelled state.
            self.pause_count.fetch_add(1, Ordering::Relaxed);
            return Err(WriteError::Pause);
        }
        if !state.queue.is_empty() && state.buffered + data.len() > self.budget {
            state.paused = true;
            state.drained_since_pause = false;
            self.pause_count.fetch_add(1, Ordering::Relaxed);
            return Err(WriteError::Pause);
        }
        // Either the queue is empty (a single oversized chunk must still be
        // accepted, otherwise the transfer could never make progress) or the
        // chunk fits the remaining budget.
        state.buffered += data.len();
        state.queue.push_back(Bytes::copy_from_slice(data));
        self.accepted_bytes.fetch_add(data.len(), Ordering::Relaxed);
        drop(state);
        self.notify.notify_one();
        Ok(data.len())
    }

    fn finish(&self, terminal: Terminal) {
        {
            let mut state = self.state.lock();
            if state.terminal.is_some() {
                return;
            }
            state.terminal = Some(terminal);
        }
        self.notify.notify_one();
    }

    /// Drops buffered bytes; used on the cancel path so a cancelled transfer
    /// cannot leak queued body data.
    fn discard_buffer(&self) {
        let mut state = self.state.lock();
        state.queue.clear();
        state.buffered = 0;
    }

    #[cfg(test)]
    fn take_terminal(&self) -> Option<Terminal> {
        self.state.lock().terminal.clone()
    }

    #[cfg(test)]
    fn pop_chunk(&self) -> Option<Bytes> {
        let mut state = self.state.lock();
        let chunk = state.queue.pop_front()?;
        state.buffered -= chunk.len();
        if state.paused {
            state.drained_since_pause = true;
        }
        Some(chunk)
    }

    /// Observes the channel under a single lock.
    ///
    /// Queued data always wins over the terminal state. The driver can enqueue
    /// the last chunk and finish the transfer back to back, so a reader that
    /// checked "is the queue empty" and "is there a terminal state" as two
    /// separate steps could observe both and drop the final bytes — or turn a
    /// failure into an apparently clean EOF.
    fn poll(&self) -> SinkPoll {
        let mut state = self.state.lock();
        if let Some(chunk) = state.queue.pop_front() {
            state.buffered -= chunk.len();
            if state.paused {
                state.drained_since_pause = true;
            }
            return SinkPoll::Chunk(chunk);
        }
        match state.terminal.clone() {
            Some(terminal) => SinkPoll::Terminal(terminal),
            None => SinkPoll::Pending,
        }
    }

    /// Reports whether the consumer freed space since the last pause, and
    /// clears the flag so the driver is asked to unpause only once.
    fn needs_resume(&self) -> bool {
        let mut state = self.state.lock();
        if state.paused && state.drained_since_pause {
            state.paused = false;
            state.drained_since_pause = false;
            true
        } else {
            false
        }
    }

    #[cfg(test)]
    fn buffered(&self) -> usize {
        self.state.lock().buffered
    }

    /// Bytes libcurl had to redeliver after a pause: the backpressure cost of
    /// this transfer, reported through [`DriverStats`].
    fn pause_count(&self) -> usize {
        self.pause_count.load(Ordering::Relaxed)
    }

    /// Bytes the write callback accepted from libcurl.
    fn accepted_bytes(&self) -> usize {
        self.accepted_bytes.load(Ordering::Relaxed)
    }

    async fn next_chunk(&self, read_timeout: Duration) -> Result<Option<Bytes>, DownloadError> {
        let deadline = tokio::time::sleep(read_timeout);
        tokio::pin!(deadline);
        loop {
            match self.poll() {
                SinkPoll::Chunk(chunk) => {
                    if self.needs_resume() {
                        self.request_resume();
                    }
                    return Ok(Some(chunk));
                }
                SinkPoll::Terminal(terminal) => {
                    return match terminal.into_error() {
                        Some(error) => Err(error),
                        None => Ok(None),
                    };
                }
                SinkPoll::Pending => {}
            }
            // `Notify` keeps one permit, so a notification that races with the
            // check above still wakes this wait.
            tokio::select! {
                () = self.notify.notified() => {}
                () = &mut deadline => {
                    return Err(DownloadError::timeout("response body timed out"));
                }
            }
        }
    }

    /// Asks the driver to unpause the handle. Called without holding the sink
    /// lock because `unpause_write` may re-enter the write callback
    /// synchronously on the driver thread.
    fn request_resume(&self) {
        if let Some(queue) = self.resume.upgrade() {
            queue.send(Command::Resume(self.id));
        }
    }
}

/// Tokio-side body reader for one transfer.
///
/// Dropping the stream before EOF cancels the transfer, which is what the
/// session layer relies on when a lease is replaced or a download is paused.
/// Holding it keeps the driver thread alive, so a session that still owns a
/// response body is never cut off by a dropped client handle.
pub(crate) struct BodyStream {
    sink: Arc<BodySink>,
    id: TransferId,
    queue: Arc<CommandQueue>,
    saw_eof: bool,
}

impl BodyStream {
    pub(crate) async fn next_chunk(
        &mut self,
        read_timeout: Duration,
    ) -> Result<Option<Bytes>, DownloadError> {
        let chunk = self.sink.next_chunk(read_timeout).await?;
        if chunk.is_none() {
            self.saw_eof = true;
        }
        Ok(chunk)
    }

    #[cfg(test)]
    fn saw_eof(&self) -> bool {
        self.saw_eof
    }

    /// Diagnostic: buffered bytes not yet delivered to the consumer.
    #[cfg(test)]
    pub(crate) fn buffered(&self) -> usize {
        self.sink.buffered()
    }

    /// Diagnostic: accepted bytes that libcurl had to redeliver after a pause.
    #[cfg(test)]
    pub(crate) fn pause_count(&self) -> usize {
        self.sink.pause_count()
    }

    /// Diagnostic: bytes the write callback accepted from libcurl.
    #[cfg(test)]
    pub(crate) fn accepted_bytes(&self) -> usize {
        self.sink.accepted_bytes()
    }
}

impl Drop for BodyStream {
    fn drop(&mut self) {
        if !self.saw_eof {
            self.queue.send(Command::Cancel(self.id));
        }
        // Sending the cancel also wakes the driver; do it explicitly so a body
        // dropped after EOF still releases a driver that is waiting on the
        // command queue, and so the last body closes the driver down.
        CommandQueue::release_or_close(&self.queue);
    }
}

/// Response head plus its body stream; dropping the body cancels the transfer.
pub(crate) struct Transfer {
    pub head: ResponseHead,
    pub body: BodyStream,
}

enum Command {
    Submit(Box<SubmitRequest>),
    Resume(TransferId),
    Cancel(TransferId),
}

/// A command plus the moment it was enqueued, for latency diagnostics.
struct Enqueued {
    at: Instant,
    command: Command,
}

#[derive(Default)]
struct CommandState {
    commands: VecDeque<Enqueued>,
    closed: bool,
}

/// Command channel between Tokio tasks and the driver thread.
///
/// A condvar queue instead of a Tokio channel because the driver has to block
/// *with a deadline* (idle pool reclamation must run even when nothing else
/// happens) while a Tokio task must be able to enqueue without awaiting.
struct CommandQueue {
    state: Mutex<CommandState>,
    ready: Condvar,
}

impl CommandQueue {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(CommandState::default()),
            ready: Condvar::new(),
        })
    }

    /// Enqueues a command. After [`Self::close`] the command is dropped, which
    /// fails any waiter it carries instead of leaving it hanging.
    fn send(&self, command: Command) {
        let mut state = self.state.lock();
        if state.closed {
            return;
        }
        state.commands.push_back(Enqueued {
            at: Instant::now(),
            command,
        });
        drop(state);
        self.ready.notify_one();
    }

    fn drain(&self, into: &mut Vec<Enqueued>) {
        let mut state = self.state.lock();
        into.extend(state.commands.drain(..));
    }

    /// Test-only view of the pending commands.
    #[cfg(test)]
    fn take_commands(&self) -> Vec<Command> {
        let mut state = self.state.lock();
        state
            .commands
            .drain(..)
            .map(|entry| entry.command)
            .collect()
    }

    /// Waits up to `timeout` for a command. Returns `true` when a command is
    /// available (or the queue closed, which the caller re-checks).
    fn wait(&self, timeout: Duration) -> bool {
        let mut state = self.state.lock();
        if !state.commands.is_empty() || state.closed {
            return true;
        }
        self.ready.wait_for(&mut state, timeout);
        !state.commands.is_empty() || state.closed
    }

    /// Closes the queue and drops pending commands. Their waiters see the
    /// dropped oneshot senders and report the driver as gone.
    ///
    /// Closing is the only shutdown signal the driver cannot miss: it is
    /// recorded under the lock `wait` uses, so a driver that is already
    /// blocked wakes up and sees it, while a plain notification could arrive
    /// while the last reference still exists.
    fn close(&self) {
        let mut state = self.state.lock();
        state.closed = true;
        state.commands.clear();
        drop(state);
        self.ready.notify_all();
    }

    fn is_closed(&self) -> bool {
        self.state.lock().closed
    }

    /// Signals the driver that a reference to it is going away.
    ///
    /// Every type that keeps the driver alive calls this while releasing its
    /// own reference. The last one closes the queue, which is a state change
    /// recorded under the lock `wait` uses, so a driver that is already blocked
    /// cannot miss it; the others only nudge the driver to re-evaluate.
    /// Counting is exact at this point: the driver thread holds one reference
    /// and the caller still holds the one it is about to release, so a count of
    /// two means no other handle, body stream or pending head is left.
    fn release_or_close(queue: &Arc<Self>) {
        if Arc::strong_count(queue) == 2 {
            queue.close();
        } else {
            queue.wake();
        }
    }

    /// Wakes the driver so it can re-evaluate its shutdown condition.
    fn wake(&self) {
        self.ready.notify_one();
    }
}

/// Cancels a submitted transfer when the caller goes away before its head
/// arrives. Disarmed once the body stream takes over both cancellation and
/// keeping the driver alive.
struct PendingHead {
    id: TransferId,
    queue: Arc<CommandQueue>,
    armed: bool,
}

impl PendingHead {
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for PendingHead {
    fn drop(&mut self) {
        if self.armed {
            self.queue.send(Command::Cancel(self.id));
        }
        CommandQueue::release_or_close(&self.queue);
    }
}

struct SubmitRequest {
    id: TransferId,
    options: RequestOptions,
    sink: Arc<BodySink>,
    head: oneshot::Sender<Result<ResponseHead, DownloadError>>,
}

/// Error raised when the driver thread is gone.
fn driver_gone_error() -> DownloadError {
    DownloadError::Internal("libcurl driver is not running".into())
}

/// Shared driver state used to fail waiters when the driver thread exits.
///
/// The driver thread owns every `Multi` and `Easy2` handle; this structure
/// holds only what a waiter needs after the thread is gone.
struct DriverShared {
    sinks: Mutex<HashMap<TransferId, Arc<BodySink>>>,
    active: AtomicUsize,
    counters: DriverCounters,
    alive: AtomicBool,
    pools: AtomicUsize,
    /// Set once a libcurl without `CURLMOPT_NETWORK_CHANGED` was detected, so
    /// the driver reports that limitation a single time.
    idle_clear_unsupported: AtomicBool,
}

impl DriverShared {
    fn fail_all(&self, terminal: Terminal) {
        let sinks: Vec<Arc<BodySink>> = {
            let mut sinks = self.sinks.lock();
            sinks.drain().map(|(_, sink)| sink).collect()
        };
        self.active.store(0, Ordering::SeqCst);
        for sink in sinks {
            sink.finish(terminal.clone());
        }
    }

    fn count_command(&self, latency: Duration) {
        self.counters.commands.fetch_add(1, Ordering::SeqCst);
        let micros = latency.as_micros().min(u64::MAX as u128) as u64;
        self.counters
            .max_command_latency_us
            .fetch_max(micros, Ordering::SeqCst);
    }

    fn count_transfer(&self, connections: u64, pauses: u64) {
        self.counters.completed.fetch_add(1, Ordering::SeqCst);
        self.counters
            .connections
            .fetch_add(connections, Ordering::SeqCst);
        self.counters.pauses.fetch_add(pauses, Ordering::SeqCst);
    }

    /// One pass closed the idle connections of a pool that was still running a
    /// transfer.
    fn count_idle_clear(&self) {
        self.counters.idle_clears.fetch_add(1, Ordering::SeqCst);
    }

    /// A cancelled transfer is not a completed one: `submitted == completed +
    /// cancelled + active` has to hold for every driver.
    fn count_cancelled(&self, pauses: u64) {
        self.counters.cancelled.fetch_add(1, Ordering::SeqCst);
        self.counters.pauses.fetch_add(pauses, Ordering::SeqCst);
    }

    #[cfg(test)]
    fn active_transfers(&self) -> usize {
        self.active.load(Ordering::SeqCst)
    }

    fn stats(&self) -> DriverStats {
        DriverStats {
            submitted: self.counters.submitted.load(Ordering::SeqCst),
            completed: self.counters.completed.load(Ordering::SeqCst),
            cancelled: self.counters.cancelled.load(Ordering::SeqCst),
            connections: self.counters.connections.load(Ordering::SeqCst),
            commands: self.counters.commands.load(Ordering::SeqCst),
            pauses: self.counters.pauses.load(Ordering::SeqCst),
            resumes: self.counters.resumes.load(Ordering::SeqCst),
            idle_clears: self.counters.idle_clears.load(Ordering::SeqCst),
            max_command_latency: Duration::from_micros(
                self.counters.max_command_latency_us.load(Ordering::SeqCst),
            ),
            active: self.active.load(Ordering::SeqCst),
            pools: self.pools.load(Ordering::SeqCst),
        }
    }
}

/// Handle to a driver thread. Cloning shares the same driver; the last clone
/// stops it.
#[derive(Clone)]
pub(crate) struct DriverHandle {
    queue: Arc<CommandQueue>,
    shared: Arc<DriverShared>,
}

impl DriverHandle {
    /// Starts a driver thread with the given pool policy.
    pub(crate) fn spawn(config: DriverConfig) -> Self {
        let queue = CommandQueue::new();
        let shared = Arc::new(DriverShared {
            sinks: Mutex::new(HashMap::new()),
            active: AtomicUsize::new(0),
            counters: DriverCounters::default(),
            alive: AtomicBool::new(true),
            pools: AtomicUsize::new(0),
            idle_clear_unsupported: AtomicBool::new(false),
        });
        let thread_queue = queue.clone();
        let thread_shared = shared.clone();
        // Only the driver thread itself keeps a strong reference to the queue;
        // the exit guard watches it weakly, otherwise the count could never
        // reach one and the thread would outlive its last handle.
        let guard_queue = Arc::downgrade(&queue);
        thread::Builder::new()
            .name("bytehaul-libcurl-driver".into())
            .spawn(move || {
                #[cfg(test)]
                let _live = live_threads::LiveThreadGuard::new();
                let guard = ExitGuard {
                    shared: thread_shared.clone(),
                    queue: guard_queue,
                };
                run_driver(thread_queue, thread_shared, config);
                drop(guard);
            })
            .expect("spawning the libcurl driver thread must succeed");
        Self { queue, shared }
    }

    /// Starts one GET transfer and resolves as soon as the response head is
    /// complete.
    pub(crate) async fn get(
        &self,
        options: RequestOptions,
        body_budget: usize,
    ) -> Result<Transfer, DownloadError> {
        let id = TransferId::next();
        if !self.shared.alive.load(Ordering::SeqCst) {
            return Err(driver_gone_error());
        }
        let sink = BodySink::new(id, body_budget, Arc::downgrade(&self.queue));
        let (head_tx, head_rx) = oneshot::channel();
        self.shared.sinks.lock().insert(id, sink.clone());
        let submit = SubmitRequest {
            id,
            options: options.clone(),
            sink: sink.clone(),
            head: head_tx,
        };
        self.shared
            .counters
            .submitted
            .fetch_add(1, Ordering::SeqCst);
        self.queue.send(Command::Submit(Box::new(submit)));

        // Dropping this future before the head lands (caller timeout, task
        // abort, a losing `select!` arm) must not leave the transfer running.
        let mut pending = PendingHead {
            id,
            queue: self.queue.clone(),
            armed: true,
        };

        let head = match tokio::time::timeout(options.head_timeout, head_rx).await {
            Ok(Ok(Ok(head))) => head,
            Ok(Ok(Err(error))) => {
                self.shared.sinks.lock().remove(&id);
                return Err(error);
            }
            Ok(Err(_)) => {
                // The head sender was dropped without a value: the driver
                // thread exited (or the handle was removed) before headers.
                self.shared.sinks.lock().remove(&id);
                return Err(driver_gone_error());
            }
            Err(_) => {
                self.shared.sinks.lock().remove(&id);
                // The caller's request-headers deadline; the message matches
                // the request timeout contract used by the transport.
                return Err(DownloadError::timeout("request timed out"));
            }
        };

        pending.disarm();
        Ok(Transfer {
            head,
            body: BodyStream {
                sink,
                id,
                queue: self.queue.clone(),
                saw_eof: false,
            },
        })
    }

    /// Counters reported by the driver thread.
    ///
    /// Used by the transport when it is released and by the driver tests.
    pub(crate) fn stats(&self) -> DriverStats {
        self.shared.stats()
    }

    #[cfg(test)]
    pub(crate) fn active_transfers(&self) -> usize {
        self.shared.active_transfers()
    }

    #[cfg(test)]
    pub(crate) fn submitted(&self) -> u64 {
        self.shared.counters.submitted.load(Ordering::SeqCst)
    }

    #[cfg(test)]
    pub(crate) fn completed(&self) -> u64 {
        self.shared.counters.completed.load(Ordering::SeqCst)
    }

    #[cfg(test)]
    pub(crate) fn is_alive(&self) -> bool {
        self.shared.alive.load(Ordering::SeqCst)
    }
}

impl Drop for DriverHandle {
    fn drop(&mut self) {
        CommandQueue::release_or_close(&self.queue);
    }
}

/// Fails every waiter when the driver thread leaves its loop, including when
/// it unwinds after a panic.
struct ExitGuard {
    shared: Arc<DriverShared>,
    /// Weak on purpose. The command queue is the driver's liveness token
    /// (`Arc::strong_count == 1` means "nothing outside this thread references
    /// the driver"), so a strong reference held *inside* the thread would keep
    /// the count at two forever: the loop would never stop and every client
    /// that was created and dropped would leave a thread behind. The queue is
    /// still reachable here whenever a waiter exists, because every waiter
    /// holds a strong reference of its own.
    queue: std::sync::Weak<CommandQueue>,
}

impl Drop for ExitGuard {
    fn drop(&mut self) {
        self.shared.alive.store(false, Ordering::SeqCst);
        self.shared.fail_all(Terminal::Failed {
            kind: TransportErrorKind::Other,
            code: None,
            message: "libcurl driver exited".into(),
        });
        if let Some(queue) = self.queue.upgrade() {
            queue.close();
        }
    }
}

/// Counts live driver threads so a test can observe that they stop.
///
/// `thread::Builder::spawn` offers no way to join or observe the end of the
/// thread, and the regression this guards against - a driver thread that never
/// exits because it holds a reference to its own liveness token - is only
/// visible as a thread count that keeps growing.
#[cfg(test)]
mod live_threads {
    use std::sync::atomic::{AtomicIsize, Ordering};

    static LIVE: AtomicIsize = AtomicIsize::new(0);

    pub(super) struct LiveThreadGuard;

    impl LiveThreadGuard {
        pub(super) fn new() -> Self {
            LIVE.fetch_add(1, Ordering::SeqCst);
            Self
        }
    }

    impl Drop for LiveThreadGuard {
        fn drop(&mut self) {
            LIVE.fetch_sub(1, Ordering::SeqCst);
        }
    }

    pub(super) fn count() -> isize {
        LIVE.load(Ordering::SeqCst)
    }
}

/// Maximum size of one published header block.
///
/// A peer that streams an unbounded header block would otherwise grow the
/// driver's memory without limit; the plan (§4.1) requires a bound on the
/// reconstructed block. 128 KiB is far above any real response head.
const MAX_HEADER_BLOCK_BYTES: usize = 128 * 1024;

/// One header block as it arrives from libcurl.
struct HeaderBlock {
    status_line: String,
    headers: Vec<(String, String)>,
    /// Bytes accumulated in this block, for the size bound.
    bytes: usize,
}

/// Header state machine: publishes the origin response head exactly once.
///
/// libcurl hands every header line of a transfer to the callback, including
/// blocks that are not the origin response:
///
/// * `1xx` informational responses (`100 Continue`, `103 Early Hints`);
/// * the proxy's `CONNECT` response when the request tunnels HTTPS;
/// * trailers after a chunked body, which arrive once the head is published.
///
/// Only the origin response is published, and only once. Trailers are ignored
/// because publication already happened when the body started.
struct HeadParser {
    /// HTTPS through a proxy makes libcurl emit a `CONNECT` block first.
    expect_connect: bool,
    connect_seen: bool,
    block: Option<HeaderBlock>,
    published: bool,
    sender: Option<oneshot::Sender<Result<ResponseHead, DownloadError>>>,
}

impl HeadParser {
    fn new(
        sender: oneshot::Sender<Result<ResponseHead, DownloadError>>,
        expect_connect: bool,
    ) -> Self {
        Self {
            expect_connect,
            connect_seen: false,
            block: None,
            published: false,
            sender: Some(sender),
        }
    }

    fn push(&mut self, line: &[u8]) {
        if self.published {
            return;
        }
        let trimmed = strip_crlf(line);
        if trimmed.is_empty() {
            self.finish_block();
            return;
        }
        match self.block.as_mut() {
            None => {
                self.block = Some(HeaderBlock {
                    status_line: String::from_utf8_lossy(trimmed).to_string(),
                    headers: Vec::new(),
                    bytes: trimmed.len(),
                });
            }
            Some(block) => {
                if let Some((name, value)) = split_header(trimmed) {
                    // Repeated field names are kept in wire order; the
                    // consumer decides how to combine them.
                    block.bytes += name.len() + value.len();
                    block.headers.push((name, value));
                } else {
                    block.bytes += trimmed.len();
                }
            }
        }
        if self
            .block
            .as_ref()
            .is_some_and(|block| block.bytes > MAX_HEADER_BLOCK_BYTES)
        {
            self.block = None;
            self.published = true;
            if let Some(sender) = self.sender.take() {
                let _ = sender.send(Err(DownloadError::Internal(format!(
                    "libcurl response headers exceeded {MAX_HEADER_BLOCK_BYTES} bytes"
                ))));
            }
        }
    }

    fn finish_block(&mut self) {
        let Some(block) = self.block.take() else {
            // A blank line outside a block: nothing to publish.
            return;
        };
        if self.published {
            return;
        }

        let Some(status) = parse_status_line(&block.status_line) else {
            self.published = true;
            if let Some(sender) = self.sender.take() {
                let _ = sender.send(Err(DownloadError::Internal(format!(
                    "libcurl response status line could not be parsed: {}",
                    block.status_line
                ))));
            }
            return;
        };

        if (100..200).contains(&status) {
            // Informational response: the origin head is still to come.
            return;
        }
        if self.expect_connect && !self.connect_seen {
            // The tunnel is established; the origin response follows.
            self.connect_seen = true;
            return;
        }

        let Some(sender) = self.sender.take() else {
            return;
        };
        self.published = true;
        let _ = sender.send(Ok(ResponseHead {
            status,
            headers: block.headers,
        }));
    }

    /// Fails the head if the transfer ends before a header block arrived.
    fn fail(&mut self, error: DownloadError) {
        if let Some(sender) = self.sender.take() {
            let _ = sender.send(Err(error));
        }
    }

    fn published(&self) -> bool {
        self.published
    }
}

/// Parses `HTTP/1.1 200 OK` (or an HTTP/2-style status line) into its code.
fn parse_status_line(status_line: &str) -> Option<u16> {
    let rest = status_line
        .strip_prefix("HTTP/")
        .or_else(|| status_line.strip_prefix("http/"))?;
    let (_, code) = rest.split_once(char::is_whitespace)?;
    code.trim_start()
        .split(char::is_whitespace)
        .next()?
        .parse()
        .ok()
}

fn strip_crlf(line: &[u8]) -> &[u8] {
    let line = line.strip_suffix(b"\n").unwrap_or(line);
    line.strip_suffix(b"\r").unwrap_or(line)
}

/// Whether libcurl will report a proxy `CONNECT` response before the origin
/// head: only an HTTPS request through a proxy opens a tunnel.
fn expects_proxy_tunnel(options: &RequestOptions) -> bool {
    options.proxy.is_some()
        && options
            .url
            .as_bytes()
            .get(..8)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"https://"))
}

fn split_header(line: &[u8]) -> Option<(String, String)> {
    let text = String::from_utf8_lossy(line);
    let (name, value) = text.split_once(':')?;
    Some((name.trim().to_ascii_lowercase(), value.trim().to_string()))
}

struct TransferHandler {
    head: HeadParser,
    sink: Arc<BodySink>,
}

impl Handler for TransferHandler {
    fn write(&mut self, data: &[u8]) -> Result<usize, WriteError> {
        self.sink.write(data)
    }

    fn header(&mut self, data: &[u8]) -> bool {
        self.head.push(data);
        true
    }
}

/// The cache entry a transfer takes when it joins a pool.
///
/// libcurl only reports how many connections a transfer *created*
/// (`CURLINFO_NUM_CONNECTS`), so the driver keeps its own estimate of what a
/// pool caches: a joining transfer takes the entry of the connection it will
/// most likely reuse, and hands it back when it turns out to have opened its
/// own connection instead. Keeping the entry per connection - and not one
/// timestamp per pool - is what stops a new request from postponing the
/// deadline of the connections nobody touched.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct IdleClaim {
    /// When the claimed connection became idle.
    since: Instant,
    /// Pool generation at admission. A clear closes every cached connection, so
    /// a transfer that was running then owns nothing that survives.
    generation: u64,
}

/// How a transfer left its pool, which decides how its cache entry is settled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TransferExit {
    /// Finished cleanly: libcurl put the connection it used back into the
    /// cache.
    Completed,
    /// Failed: libcurl closes the connection it could not finish with.
    Failed,
    /// Aborted by the caller. The connection is dropped, and a claim the
    /// transfer never had a chance to use goes back to the pool.
    Cancelled,
}

struct ActiveTransfer {
    handle: Easy2Handle<TransferHandler>,
    /// The cache entry this transfer took when it was admitted.
    claim: Option<IdleClaim>,
    /// Pool generation at admission, for [`Pool::settle_connection`].
    generation: u64,
}

/// One connection pool: its own `Multi`, active transfers and DNS overrides.
struct Pool {
    multi: Multi,
    active: HashMap<TransferId, ActiveTransfer>,
    /// Set while the pool has no active transfer, for idle reclamation.
    idle_since: Option<Instant>,
    /// Connections this pool caches that no transfer is using, each dated by
    /// the moment it became idle. The oldest entry decides when the pool's idle
    /// connections are due, so a connection nobody reused keeps its own
    /// deadline no matter how much other traffic the pool sees.
    idle: BinaryHeap<Reverse<Instant>>,
    /// Bumped whenever every cached connection of this pool was closed, which
    /// invalidates the claims of the transfers that were running then.
    clear_generation: u64,
    /// Answers injected into this multi's DNS cache, keyed by `host:port`.
    resolve: HashMap<String, InjectedResolve>,
}

impl Pool {
    fn new(config: &DriverConfig) -> Self {
        let mut multi = Multi::new();
        if let Err(error) = apply_cache_bound(&mut multi, config) {
            // Never fatal: without the bound the pool still works, it just
            // keeps libcurl's default cache size.
            tracing::debug!(%error, "could not set the libcurl connection cache bound");
        }
        Self {
            multi,
            active: HashMap::new(),
            // A pool starts idle: a pool whose first submission fails is still
            // reclaimed instead of being kept forever.
            idle_since: Some(Instant::now()),
            idle: BinaryHeap::new(),
            clear_generation: 0,
            resolve: HashMap::new(),
        }
    }

    /// Takes the cache entry a joining transfer will reuse.
    ///
    /// Which connection libcurl picks is its own business; what matters here is
    /// that one cached connection is now in use, so the pool must not act on
    /// its deadline while that is true. Removing the oldest entry keeps every
    /// other deadline - the ones that describe connections nobody is using -
    /// exactly where it was.
    fn claim_idle_connection(&mut self) -> Option<IdleClaim> {
        self.idle.pop().map(|Reverse(since)| IdleClaim {
            since,
            generation: self.clear_generation,
        })
    }

    /// Hands back an entry a transfer turned out not to use.
    fn release_claim(&mut self, claim: IdleClaim) {
        if claim.generation == self.clear_generation {
            self.idle.push(Reverse(claim.since));
        }
    }

    /// Records the connections a transfer put back into the cache.
    fn add_idle_connections(&mut self, count: u64, since: Instant) {
        for _ in 0..count {
            self.idle.push(Reverse(since));
        }
    }

    /// Settles a transfer that is leaving this pool.
    ///
    /// `created` is `CURLINFO_NUM_CONNECTS`: zero means it reused the cached
    /// connection it claimed when it was admitted, a positive value means it
    /// opened its own connection(s) and never touched the claim.
    ///
    /// This stays an estimate: libcurl never reports how many connections it
    /// actually keeps, so a connection the peer closed (`Connection: close`)
    /// leaves one entry behind. The cost of that is at most one extra clear,
    /// which resets the estimate.
    fn settle_connection(
        &mut self,
        admitted_generation: u64,
        claim: Option<IdleClaim>,
        created: u64,
        exit: TransferExit,
        now: Instant,
    ) {
        if admitted_generation != self.clear_generation {
            // The cached connections were closed while this transfer ran, and
            // libcurl will close this transfer's connection too (it was marked
            // non-reusable), so the pool's cache did not change.
            return;
        }
        if created > 0 || exit == TransferExit::Cancelled {
            // Nothing was reused, or the transfer was aborted before it could
            // use what it claimed: the cached connection is still there.
            if let Some(claim) = claim {
                self.release_claim(claim);
            }
        }
        if exit == TransferExit::Completed {
            // The connections this transfer leaves behind are cached again with
            // a fresh clock; a reused one only replaces the claim it took.
            self.add_idle_connections(created.max(1), now);
        }
    }

    /// The number of cached connections no transfer is using, as far as the
    /// driver can tell from `CURLINFO_NUM_CONNECTS`.
    #[cfg(test)]
    fn idle_connections(&self) -> usize {
        self.idle.len()
    }

    /// When the longest-idle cached connection became idle, if any.
    #[cfg(test)]
    fn oldest_idle(&self) -> Option<Instant> {
        self.idle.peek().map(|Reverse(since)| *since)
    }

    /// Decides what has to reach this pool's DNS cache for one transfer.
    ///
    /// The entry is refreshed when the TTL Hickory reported has passed or the
    /// answer changed. A changed answer also forces a fresh connection,
    /// because an existing socket is pinned to the address it connected to.
    fn plan_resolve(&self, entry: Option<&ResolveEntry>, now: Instant) -> ResolvePlan {
        let Some(entry) = entry else {
            return ResolvePlan::default();
        };
        match self.resolve.get(&entry.key) {
            None => ResolvePlan {
                list: vec![entry.spec.clone()],
                force_fresh_connect: false,
            },
            Some(injected) => {
                let changed = injected.spec != entry.spec;
                if !changed && !injected.is_expired(now) {
                    // libcurl still holds this answer in the pool's cache.
                    return ResolvePlan::default();
                }
                ResolvePlan {
                    // `-host:port` removes the previous answer before the new
                    // one is added; without it a refresh would be ignored.
                    list: vec![format!("-{}", entry.key), entry.spec.clone()],
                    force_fresh_connect: changed,
                }
            }
        }
    }

    /// Records an applied plan; only called after the handle joined the pool,
    /// so a failed submission cannot leave a phantom cache entry behind.
    fn commit_resolve(&mut self, entry: Option<&ResolveEntry>, applied: bool, now: Instant) {
        let Some(entry) = entry else {
            return;
        };
        if !applied {
            return;
        }
        self.resolve.insert(
            entry.key.clone(),
            InjectedResolve {
                spec: entry.spec.clone(),
                injected_at: now,
                ttl: entry.ttl,
            },
        );
    }

    /// The instant this pool's cached sockets have to be closed because it is
    /// completely idle, if any.
    ///
    /// Only pools without a transfer are dropped here; a busy pool reclaims its
    /// *idle* connections through [`Pool::idle_connection_deadline`] instead,
    /// because dropping the `Multi` would abort the transfer it is running.
    fn idle_deadline(&self, timeout: Duration, reuse_disabled: bool) -> Option<Instant> {
        if !self.active.is_empty() {
            return None;
        }
        if reuse_disabled {
            // A pool that may not reuse connections is dropped immediately.
            return Some(Instant::now());
        }
        self.idle_since.map(|since| since + timeout)
    }

    /// The instant this busy pool's idle connections have to be closed, if any.
    ///
    /// The oldest cached connection decides. A connection that gets reused
    /// leaves the idle set through [`Pool::claim_idle_connection`] and comes
    /// back with a fresh date, so a request neither postpones nor brings
    /// forward the deadline of the connections nobody touched.
    fn idle_connection_deadline(&self, timeout: Duration, reuse_disabled: bool) -> Option<Instant> {
        if self.active.is_empty() || reuse_disabled {
            // A fully idle pool is dropped by `idle_deadline`, and a pool that
            // may not reuse connections never caches one.
            return None;
        }
        self.idle.peek().map(|Reverse(since)| *since + timeout)
    }

    /// Closes every connection of this pool that no transfer is using.
    ///
    /// `CURLMOPT_NETWORK_CHANGED` with `CURLMNWC_CLEAR_CONNS` is the only
    /// public libcurl API that closes cached sockets of a multi handle that
    /// keeps running: "Connections that are idle are closed. Ongoing transfers
    /// do continue with the connection they have" (`curl/multi.h`). It also
    /// marks the connections still in use as non-reusable, which is what we
    /// want here - the pool's cache has been unused past `pool_idle_timeout`,
    /// so none of it should outlive the transfers that are still running.
    ///
    /// The option was added in libcurl 8.21.0; older libraries answer
    /// `CURLM_UNKNOWN_OPTION`, which the caller reports once and then ignores.
    fn clear_idle_connections(&mut self) -> std::result::Result<(), String> {
        let code = unsafe {
            curl_sys::curl_multi_setopt(
                self.multi.raw(),
                CURLMOPT_NETWORK_CHANGED,
                CURLMNWC_CLEAR_CONNS,
            )
        };
        if code != curl_sys::CURLM_OK {
            return Err(format!(
                "curl_multi_setopt(CURLMOPT_NETWORK_CHANGED) failed with code {code}"
            ));
        }
        // Nothing the pool cached survives, and the transfers that are running
        // hold connections libcurl will close instead of caching, so their
        // claims are stale.
        self.idle.clear();
        self.clear_generation += 1;
        Ok(())
    }
}

/// `CURLMOPT_NETWORK_CHANGED` (libcurl 8.21.0) and its `CURLMNWC_CLEAR_CONNS`
/// bit, spelled out from the public header because the `curl-sys` release this
/// crate builds against does not bind them yet. Both values are part of
/// libcurl's append-only option ABI, and an older library simply rejects the
/// option, which [`Pool::clear_idle_connections`] reports.
const CURLMOPT_NETWORK_CHANGED: curl_sys::CURLMoption = curl_sys::CURLOPTTYPE_LONG + 17;
const CURLMNWC_CLEAR_CONNS: std::ffi::c_long = 1 << 1;

/// Applies `pool_max_idle_per_host` to one pool through `CURLMOPT_MAXCONNECTS`.
///
/// libcurl counts every connection it keeps in a multi handle's cache - the
/// ones carrying a transfer *and* the idle ones - and checks the bound every
/// time a connection becomes idle, that is, whenever a transfer finishes:
/// while the cache is larger than the bound it closes the oldest connection
/// that no transfer is using. Putting `pool_max_idle_per_host` there is what
/// makes the bound mean "cache at most this many idle connections per pool":
///
/// * a connection in use is never closed by it, so it cannot limit
///   concurrency, unlike `CURLMOPT_MAX_HOST_CONNECTIONS` (which serializes
///   transfers) or `CURLMOPT_MAX_TOTAL_CONNECTIONS` (which caps simultaneously
///   open connections and makes further transfers wait);
/// * the shrink happens at transfer completion, because that is when a
///   connection becomes idle.
///
/// Lowering the bound alone does not close anything, which is why a pool that
/// keeps a long transfer and idle connections is reclaimed by
/// [`Pool::clear_idle_connections`] instead.
///
/// `0` means "pick a default" to libcurl, so `pool_max_idle_per_host == 0`
/// must never be passed here; that case is expressed with
/// `CURLOPT_FRESH_CONNECT`/`CURLOPT_FORBID_REUSE` per transfer plus dropping
/// the pool as soon as it is idle.
fn apply_cache_bound(multi: &mut Multi, config: &DriverConfig) -> Result<(), curl::MultiError> {
    if config.max_idle_per_host == 0 {
        return Ok(());
    }
    multi.set_max_connects(config.max_idle_per_host)
}

fn run_driver(queue: Arc<CommandQueue>, shared: Arc<DriverShared>, config: DriverConfig) {
    let mut pools: HashMap<PoolKey, Pool> = HashMap::new();
    let mut pending: Vec<Enqueued> = Vec::new();
    let mut draining: Vec<Enqueued> = Vec::new();
    let mut shutting_down = false;

    while !shutting_down {
        // 1. Commands first: they are both work and the loop's wakeup source.
        draining.clear();
        queue.drain(&mut draining);
        pending.append(&mut draining);
        for enqueued in pending.drain(..) {
            shared.count_command(enqueued.at.elapsed());
            match enqueued.command {
                Command::Submit(submit) => {
                    start_transfer(&mut pools, &shared, &config, *submit);
                }
                Command::Resume(id) => {
                    shared.counters.resumes.fetch_add(1, Ordering::SeqCst);
                    resume_transfer(&pools, id);
                }
                Command::Cancel(id) => cancel_transfer(&mut pools, &shared, id),
            }
        }

        // 2. The driver stops once nothing but this thread references it: the
        // command queue is the liveness token, so a live body stream keeps the
        // driver (and its pooled sockets) alive. `ExitGuard` watches the queue
        // through a `Weak` and the last handle closes the queue before
        // releasing its reference, so both conditions are exact: the count
        // reaches one only when no reference outside this thread is left.
        if queue.is_closed() || Arc::strong_count(&queue) == 1 {
            shutting_down = true;
        }

        // 3. Drive every pool once and collect completions.
        let mut completions: Vec<(PoolKey, TransferId, CurlResult)> = Vec::new();
        let mut broken: Vec<(PoolKey, String)> = Vec::new();
        for (key, pool) in pools.iter_mut() {
            if let Err(error) = pool.multi.perform() {
                broken.push((
                    key.clone(),
                    format!("libcurl multi perform failed: {error}"),
                ));
                continue;
            }
            pool.multi.messages(|message| {
                if let Some(result) = message.result() {
                    if let Ok(token) = message.token() {
                        completions.push((
                            key.clone(),
                            TransferId(token as u64),
                            result.map_err(|error| (error.code() as u32, error.to_string())),
                        ));
                    }
                }
            });
        }
        for (key, message) in broken {
            // A multi handle that cannot perform is not reusable: fail what it
            // was running and drop the pool so its sockets are closed.
            tracing::debug!(error = %message, "dropping a broken libcurl pool");
            if let Some(mut pool) = pools.remove(&key) {
                fail_pool(&shared, &mut pool, &message);
            }
        }
        for (key, id, result) in completions {
            complete_transfer(&mut pools, &shared, &key, id, result);
        }

        // 4. Reclaim pools that stayed idle for `pool_idle_timeout`, and close
        // the idle connections of pools that are still running a transfer.
        reclaim_idle_pools(&mut pools, &config);
        reclaim_idle_connections(&mut pools, &config, &shared);
        shared.pools.store(pools.len(), Ordering::SeqCst);
        shared.active.store(
            pools.values().map(|pool| pool.active.len()).sum(),
            Ordering::SeqCst,
        );

        if shutting_down {
            break;
        }

        // 5. Wait: libcurl's sockets when something is in flight, the command
        // queue (with the nearest idle deadline) when nothing is.
        if pools.values().any(|pool| !pool.active.is_empty()) {
            let wait = pools
                .values()
                .filter_map(|pool| pool.multi.get_timeout().ok().flatten())
                .min()
                .unwrap_or(DRIVER_WAIT_SLICE)
                .min(DRIVER_WAIT_SLICE);
            if let Some(pool) = pools.values().next() {
                let _ = pool.multi.wait(&mut [], wait);
            }
        } else {
            let wait = next_idle_wait(&pools, &config);
            queue.wait(wait);
        }
    }

    // Cancel everything that is still in flight so no waiter is left hanging.
    for (_, pool) in pools.drain() {
        for (id, mut transfer) in pool.active {
            transfer
                .handle
                .get_mut()
                .head
                .fail(DownloadError::Cancelled);
            let sink = transfer.handle.get_ref().sink.clone();
            let _ = pool.multi.remove2(transfer.handle);
            shared.sinks.lock().remove(&id);
            sink.finish(Terminal::Cancelled);
        }
    }
    shared.active.store(0, Ordering::SeqCst);
    shared.pools.store(0, Ordering::SeqCst);
}

/// How long the driver may block when no transfer is in flight.
fn next_idle_wait(pools: &HashMap<PoolKey, Pool>, config: &DriverConfig) -> Duration {
    let reuse_disabled = config.max_idle_per_host == 0;
    pools
        .values()
        .filter_map(|pool| pool.idle_deadline(config.pool_idle_timeout, reuse_disabled))
        .map(|deadline| deadline.saturating_duration_since(Instant::now()))
        .min()
        .unwrap_or(DRIVER_IDLE_WAIT)
}

/// Closes the connections a still-busy pool has left unused for
/// `pool_idle_timeout`.
///
/// A pool that runs a long transfer keeps its `Multi` alive, so destroying the
/// pool cannot be the way to close the sockets that other transfers left
/// behind: without this pass they would stay open for as long as the long
/// transfer runs (measured: three idle sockets outlived a 2 s download).
/// `CURLOPT_MAXAGE_CONN` does not help either - libcurl only checks the age of
/// a connection when it is about to be reused or when a new connection is
/// being created.
///
/// The driver reaches the deadline only when no new transfer claimed the pool
/// in the meantime, because admitting a transfer clears the timestamp: the
/// cached connections are then in use again, or at least on their way to being
/// reused.
fn reclaim_idle_connections(
    pools: &mut HashMap<PoolKey, Pool>,
    config: &DriverConfig,
    shared: &DriverShared,
) {
    let reuse_disabled = config.max_idle_per_host == 0;
    let now = Instant::now();
    for pool in pools.values_mut() {
        let Some(deadline) =
            pool.idle_connection_deadline(config.pool_idle_timeout, reuse_disabled)
        else {
            continue;
        };
        if now < deadline {
            continue;
        }
        match pool.clear_idle_connections() {
            Ok(()) => {
                shared.count_idle_clear();
                tracing::debug!(
                    active = pool.active.len(),
                    idle_timeout_ms = config.pool_idle_timeout.as_millis() as u64,
                    "closed the idle connections of a busy libcurl pool"
                );
            }
            Err(message) => {
                // An older libcurl rejects the option. The pool keeps its
                // sockets in that case, which is the pre-existing behaviour;
                // reporting it once is enough to explain what was lost.
                if !shared.idle_clear_unsupported.swap(true, Ordering::SeqCst) {
                    tracing::debug!(
                        %message,
                        "libcurl does not support clearing idle connections of a running pool"
                    );
                }
                // The cached connections are still there, so their dates stay
                // valid and the next pass tries again.
                continue;
            }
        }
        // A successful clear removed every cached connection, including the
        // dates of the ones nobody reused; `clear_idle_connections` already
        // reset the estimate and invalidated the running transfers' claims.
    }
}

/// Drops pools that stayed idle for `pool_idle_timeout`.
///
/// Destroying the pool's `Multi` is what actually closes its idle sockets:
/// `CURLOPT_MAXAGE_CONN` only checks the age when a connection is about to be
/// reused, which is why the pool itself has to be reclaimed.
fn reclaim_idle_pools(pools: &mut HashMap<PoolKey, Pool>, config: &DriverConfig) {
    let reuse_disabled = config.max_idle_per_host == 0;
    let now = Instant::now();
    pools.retain(|_, pool| {
        let Some(deadline) = pool.idle_deadline(config.pool_idle_timeout, reuse_disabled) else {
            return true;
        };
        if now >= deadline {
            tracing::debug!(
                idle_ms = config.pool_idle_timeout.as_millis() as u64,
                cached_resolve_entries = pool.resolve.len(),
                "closing an idle libcurl pool"
            );
            return false;
        }
        true
    });
}

fn resume_transfer(pools: &HashMap<PoolKey, Pool>, id: TransferId) {
    for pool in pools.values() {
        if let Some(transfer) = pool.active.get(&id) {
            // `unpause_write` may re-enter the write callback synchronously,
            // which is why the caller never holds the sink lock here.
            let _ = transfer.handle.unpause_write();
            return;
        }
    }
}

/// Result of one libcurl transfer: `Err` carries the curl code and message.
type CurlResult = Result<(), (u32, String)>;

fn build_easy(
    options: &RequestOptions,
    sink: Arc<BodySink>,
    head: HeadParser,
    resolve: &ResolvePlan,
    max_age_conn_secs: Option<u32>,
) -> Easy2<TransferHandler> {
    let mut easy = Easy2::new(TransferHandler { head, sink });
    if let Err(error) = configure_easy(&mut easy, options, resolve, max_age_conn_secs) {
        // Surface configuration failures through the head channel instead of
        // panicking on the driver thread.
        easy.get_mut().head.fail(DownloadError::Internal(format!(
            "libcurl request options rejected: {error}"
        )));
    }
    easy
}

fn configure_easy(
    easy: &mut Easy2<TransferHandler>,
    options: &RequestOptions,
    resolve: &ResolvePlan,
    max_age_conn_secs: Option<u32>,
) -> Result<(), curl::Error> {
    easy.url(&options.url)?;
    // The worker owns redirect handling; libcurl must not follow on its own.
    easy.http_version(HttpVersion::V11)?;
    easy.follow_location(false)?;
    // Range responses must stay byte-exact: no automatic content decoding and
    // no transfer-encoding rewriting.
    easy.http_content_decoding(false)?;
    easy.transfer_encoding(false)?;
    easy.accept_encoding("identity")?;
    easy.connect_timeout(options.connect_timeout)?;
    // No default User-Agent: the caller's headers are the only ones sent, so
    // the libcurl path does not silently change the request the way a library
    // default would.
    easy.fail_on_error(false)?;
    // Second line of defence for connection reuse: the pool's own reclamation
    // closes idle sockets, and this keeps an over-age connection from being
    // reused in the meantime.
    if let Some(secs) = max_age_conn_secs {
        easy.maxage_conn(Duration::from_secs(u64::from(secs)))?;
    }
    if options.forbid_connection_reuse {
        easy.fresh_connect(true)?;
        easy.forbid_reuse(true)?;
    }
    if resolve.force_fresh_connect {
        // The resolved address changed: a cached connection would still point
        // at the previous one.
        easy.fresh_connect(true)?;
    }
    if let Some(range) = options.range.as_deref() {
        easy.range(range)?;
    }
    apply_proxy(easy, options)?;
    if !resolve.list.is_empty() {
        let mut list = List::new();
        for entry in &resolve.list {
            list.append(entry)?;
        }
        easy.resolve(list)?;
    }
    if let Some(ca_info) = options.ca_info.as_deref() {
        // Adds trust anchors; it never turns verification off.
        easy.cainfo(ca_info)?;
    }
    if let Some(ca_path) = options.ca_path.as_deref() {
        easy.capath(ca_path)?;
    }
    if let Some(client_cert) = options.client_cert.as_deref() {
        easy.ssl_cert(client_cert)?;
    }
    if let Some(client_key) = options.client_key.as_deref() {
        easy.ssl_key(client_key)?;
    }
    if !options.headers.is_empty() {
        let mut list = List::new();
        for (name, value) in &options.headers {
            list.append(&format!("{name}: {value}"))?;
        }
        easy.http_headers(list)?;
    }
    Ok(())
}

/// Applies the routing decision to one handle.
///
/// `CURLOPT_*PROXY` is always set explicitly, including when there is no
/// proxy, and the bypass list is always overwritten. Otherwise libcurl would
/// fall back to `HTTP_PROXY`/`ALL_PROXY`/`NO_PROXY` from the process
/// environment, which would make routing depend on the caller's shell rather
/// than on `ClientNetworkConfig`.
fn apply_proxy(
    easy: &mut Easy2<TransferHandler>,
    options: &RequestOptions,
) -> Result<(), curl::Error> {
    match options.proxy.as_deref() {
        Some(proxy) => {
            easy.proxy(proxy)?;
            // An empty bypass list means "the proxy is used for every host on
            // this route"; it cannot be diverted by an environment `NO_PROXY`.
            easy.noproxy("")?;
        }
        None => {
            // An empty proxy string explicitly disables proxy use, even when
            // the environment defines one.
            easy.proxy("")?;
            easy.noproxy("*")?;
        }
    }
    Ok(())
}

fn start_transfer(
    pools: &mut HashMap<PoolKey, Pool>,
    shared: &Arc<DriverShared>,
    config: &DriverConfig,
    submit: SubmitRequest,
) {
    let SubmitRequest {
        id,
        options,
        sink,
        head,
    } = submit;
    let key = options.pool_key();
    let now = Instant::now();
    let pool = pools.entry(key).or_insert_with(|| Pool::new(config));
    let plan = pool.plan_resolve(options.resolve.as_ref(), now);
    let applied = !plan.list.is_empty();
    let parser = HeadParser::new(head, expects_proxy_tunnel(&options));
    let easy = build_easy(
        &options,
        sink.clone(),
        parser,
        &plan,
        config.max_age_conn_secs(),
    );
    match pool.multi.add2(easy) {
        Ok(mut handle) => {
            if let Err(error) = handle.set_token(id.raw()) {
                sink.finish(Terminal::Failed {
                    kind: TransportErrorKind::Other,
                    code: None,
                    message: format!("could not tag transfer: {error}"),
                });
                shared.sinks.lock().remove(&id);
                return;
            }
            // A forced fresh connection cannot use any cached socket. Leave
            // those deadlines visible while this transfer runs, rather than
            // hiding one until completion reveals that it was not reused.
            let claim = if options.forbid_connection_reuse || plan.force_fresh_connect {
                None
            } else {
                pool.claim_idle_connection()
            };
            let generation = pool.clear_generation;
            pool.active.insert(
                id,
                ActiveTransfer {
                    handle,
                    claim,
                    generation,
                },
            );
            pool.idle_since = None;
            pool.commit_resolve(options.resolve.as_ref(), applied, now);
        }
        Err(error) => {
            sink.finish(Terminal::Failed {
                kind: TransportErrorKind::Other,
                code: None,
                message: format!("could not add transfer to multi handle: {error}"),
            });
            shared.sinks.lock().remove(&id);
        }
    }
}

/// Returns `true` when the completion changed the pool bookkeeping.
fn complete_transfer(
    pools: &mut HashMap<PoolKey, Pool>,
    shared: &Arc<DriverShared>,
    key: &PoolKey,
    id: TransferId,
    result: CurlResult,
) -> bool {
    let Some(pool) = pools.get_mut(key) else {
        return false;
    };
    let Some(mut transfer) = pool.active.remove(&id) else {
        return false;
    };
    let sink = transfer.handle.get_ref().sink.clone();
    let head_status = {
        let head = &transfer.handle.get_ref().head;
        head.published().then_some(())
    };
    let phase = if head_status.is_some() {
        TransferPhase::AfterHeaders
    } else {
        TransferPhase::BeforeHeaders
    };
    // Real connections opened by this transfer, which is not the same count as
    // the number of request invocations.
    let connections = transfer.handle.num_connects().unwrap_or(0);
    let terminal = match result {
        Ok(()) => Terminal::Eof,
        Err((code, message)) => Terminal::Failed {
            kind: classify_curl_failure(code),
            code: Some(code),
            message: format!(
                "libcurl error {code} {phase}: {message} (transfer {})",
                id.0
            ),
        },
    };
    if let Terminal::Failed { kind, message, .. } = &terminal {
        // A transfer that failed before its header block must fail the request
        // future with the real reason instead of being dropped silently.
        transfer
            .handle
            .get_mut()
            .head
            .fail(DownloadError::Transport(TransportError::new(
                *kind,
                std::io::Error::other(message.clone()),
            )));
    }
    let _ = pool.multi.remove2(transfer.handle);
    shared.sinks.lock().remove(&id);
    shared.count_transfer(connections, sink.pause_count() as u64);
    let now = Instant::now();
    if pool.active.is_empty() {
        pool.idle_since = Some(now);
    }
    // The connection this transfer used either went back into the cache or was
    // closed; the pool's per-connection idle dates follow it either way.
    let exit = if matches!(terminal, Terminal::Eof) {
        TransferExit::Completed
    } else {
        TransferExit::Failed
    };
    pool.settle_connection(transfer.generation, transfer.claim, connections, exit, now);
    tracing::debug!(
        transfer = id.0,
        phase = %phase,
        connections,
        pauses = sink.pause_count(),
        accepted_bytes = sink.accepted_bytes(),
        "libcurl transfer finished"
    );
    sink.finish(terminal);
    true
}

fn cancel_transfer(pools: &mut HashMap<PoolKey, Pool>, shared: &Arc<DriverShared>, id: TransferId) {
    let key = pools
        .iter()
        .find(|(_, pool)| pool.active.contains_key(&id))
        .map(|(key, _)| key.clone());
    let Some(key) = key else {
        return;
    };
    let pool = pools.get_mut(&key).expect("pool located above");
    let Some(mut transfer) = pool.active.remove(&id) else {
        return;
    };
    let sink = transfer.handle.get_ref().sink.clone();
    // The connection count matters for the pool's cache estimate, so it has to
    // be read while the handle is still attached.
    let connections = transfer.handle.num_connects().unwrap_or(0);
    // Cancelling before headers must also fail the request future.
    transfer
        .handle
        .get_mut()
        .head
        .fail(DownloadError::Cancelled);
    let _ = pool.multi.remove2(transfer.handle);
    shared.sinks.lock().remove(&id);
    shared.count_cancelled(sink.pause_count() as u64);
    let now = Instant::now();
    if pool.active.is_empty() {
        pool.idle_since = Some(now);
    }
    // An aborted transfer's connection is dropped; a claim it never got to use
    // goes back to the pool so its deadline is not lost.
    pool.settle_connection(
        transfer.generation,
        transfer.claim,
        connections,
        TransferExit::Cancelled,
        now,
    );
    sink.discard_buffer();
    sink.finish(Terminal::Cancelled);
}

/// Fails every transfer of a pool whose `Multi` can no longer perform.
///
/// Both the body waiters and the request futures are released: a pool failure
/// must never leave a caller waiting for a head that will not arrive.
fn fail_pool(shared: &Arc<DriverShared>, pool: &mut Pool, message: &str) {
    let transfers: Vec<ActiveTransfer> = pool.active.drain().map(|(_, value)| value).collect();
    for mut transfer in transfers {
        let sink = transfer.handle.get_ref().sink.clone();
        transfer
            .handle
            .get_mut()
            .head
            .fail(DownloadError::Internal(message.to_string()));
        shared.sinks.lock().remove(&sink.id);
        sink.discard_buffer();
        sink.finish(Terminal::Failed {
            kind: TransportErrorKind::Other,
            code: None,
            message: message.to_string(),
        });
    }
}

/// Maps a libcurl error code to the public transport error kind.
///
/// The table follows the migration plan (§4.2): timeouts stay timeouts,
/// recoverable connection-establishment failures stay retryable as `Connect`,
/// and failures that truncate an already-started body stay `Body` so the
/// session can retry them with a Range request. Everything else maps to the
/// non-retryable `Other`: a bad URL, an unsupported protocol, a rejected
/// certificate, a hostname mismatch, an aborted-by-callback transfer or an
/// out-of-memory condition cannot be fixed by repeating the request. The
/// transfer phase (before/after the response head) is carried in the error
/// message for diagnostics instead of widening the retryable set.
fn classify_curl_failure(code: u32) -> TransportErrorKind {
    match code {
        // 28 timed out: the connect deadline, a stalled transfer or a
        // low-speed abort.
        28 => TransportErrorKind::Timeout,
        // 5/6 name resolution (proxy or origin), 7 connect failure,
        // 35 generic TLS handshake failure. All are transient enough to retry.
        5 | 6 | 7 | 35 => TransportErrorKind::Connect,
        // 18 partial file, 55 send error, 56 receive error: the body was
        // truncated after the head, and a Range retry can recover it.
        18 | 55 | 56 => TransportErrorKind::Body,
        // Peer-side protocol failures that are not tied to a deterministic
        // defect of the request itself (an empty or unparsable reply, a
        // send-rewind failure): retrying over a fresh connection may succeed.
        8 | 34 | 52 | 61 | 65 => TransportErrorKind::Request,
        // 1 unsupported protocol, 2 failed init, 3 malformed URL, 9 access
        // denied, 42 aborted by callback, 51/60 peer verification,
        // 58/59/64/66/80/82/83/90/91 TLS setup, issuer, revocation, pinning
        // and date failures, and every code without evidence that a retry can
        // succeed.
        _ => TransportErrorKind::Other,
    }
}

/// Diagnostic helper: waits until the driver has no active transfer.
#[cfg(test)]
pub(crate) async fn wait_for_idle(handle: &DriverHandle, timeout: Duration) -> bool {
    let started = Instant::now();
    while started.elapsed() < timeout {
        if handle.active_transfers() == 0 {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    handle.active_transfers() == 0
}

#[cfg(test)]
mod tests;
