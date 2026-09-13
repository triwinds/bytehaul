//! Local pipeline harness (P1 of the simplification plan).
//!
//! Measures the paths a normal download actually takes — the dynamic request
//! planner, the writer and its write-back cache, the client cache and driver
//! threads, the libcurl driver loop, and whole downloads against a local
//! fixture — and writes a report plus per-round samples.
//!
//! ```text
//! cargo bench --bench pipeline_bench -- --rounds 10
//! cargo bench --bench pipeline_bench -- --list
//! cargo bench --bench pipeline_bench -- --filter driver --rounds 5
//! ```
//!
//! Design rules this harness follows (P1 acceptance):
//!
//! * No assertion depends on a public network. Every scenario runs against
//!   `127.0.0.1` fixtures in this file.
//! * Nothing here is a pass/fail gate. A scenario that cannot run is reported
//!   with its outcome, and only a harness bug makes the process exit non-zero.
//! * Every number comes from the default download path: the counters in
//!   `bytehaul::bench` are the ones `src/` records, and the fixture's request,
//!   range and byte counts are what a real origin saw.
//! * Timing boundaries are recorded explicitly (see the generated report
//!   header) so two runs compare the same phases.
//! * Temporary files stay under `target/pipeline-bench`.

use std::collections::{BTreeMap, HashSet};
use std::fmt::Write as _;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytehaul::bench::{
    bench_cached_client_count, bench_cached_client_lookup, bench_counters_reset,
    bench_counters_set_enabled, bench_counters_snapshot, bench_driver_stats, bench_driver_threads,
    bench_scheduler_from_piece_map, bench_scheduler_plan, BenchCounters, BenchDriverStats,
    BenchPlanParams, BenchPlanResult, PieceMap,
};
use bytehaul::{Checksum, DownloadSpec, Downloader, FileAllocation, LogLevel, RangeSchedulingMode};
use tokio::sync::Semaphore;
use warp::Filter;

// ──────────────────────────────────────────────────────────────
//  Sizes used by the data-path scenarios
// ──────────────────────────────────────────────────────────────

const PIECE_SIZE: u64 = 1024 * 1024;
/// Above the default `min_split_size` (10 MiB), so the session splits it
/// between connections and writes through the write-back cache.
const SPLIT_BYTES: usize = 12 * 1024 * 1024;
/// Below `min_split_size`, so the session stays on the single-connection path.
const SMALL_BYTES: usize = 256 * 1024;
/// The largest end-to-end shape.
const LARGE_BYTES: usize = 64 * 1024 * 1024;
/// Longest a single measured download may take before the harness stops it.
const ROUND_TIMEOUT: Duration = Duration::from_secs(120);
/// How long a cancelled round may still take to finalize.
const CANCEL_GRACE: Duration = Duration::from_secs(30);

// ──────────────────────────────────────────────────────────────
//  Command line
// ──────────────────────────────────────────────────────────────

struct Config {
    rounds: usize,
    filter: Option<String>,
    out_dir: PathBuf,
    /// A directory outside `target` that the report and the per-round samples
    /// are copied to, so the numbers a document cites can be committed.
    archive: Option<PathBuf>,
    list: bool,
}

impl Config {
    fn from_args() -> Self {
        let mut config = Self {
            rounds: 10,
            filter: None,
            out_dir: default_out_dir(),
            archive: None,
            list: false,
        };
        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--rounds" => {
                    config.rounds = args
                        .next()
                        .and_then(|value| value.parse().ok())
                        .unwrap_or(config.rounds)
                        .max(1);
                }
                "--filter" => config.filter = args.next(),
                "--out" => {
                    if let Some(dir) = args.next() {
                        config.out_dir = PathBuf::from(dir);
                    }
                }
                "--archive" => {
                    if let Some(dir) = args.next() {
                        config.archive = Some(PathBuf::from(dir));
                    }
                }
                "--list" => config.list = true,
                "-h" | "--help" => {
                    println!(
                        "pipeline_bench [--rounds N] [--filter SUBSTR] [--out DIR] [--archive DIR] [--list]"
                    );
                    std::process::exit(0);
                }
                other => eprintln!("ignoring unknown argument: {other}"),
            }
        }
        config
    }

    fn selected(&self, name: &str) -> bool {
        match &self.filter {
            Some(filter) => name.contains(filter),
            None => true,
        }
    }
}

fn default_out_dir() -> PathBuf {
    let target = std::env::var("CARGO_TARGET_DIR").unwrap_or_else(|_| "target".to_string());
    PathBuf::from(target).join("pipeline-bench")
}

// ──────────────────────────────────────────────────────────────
//  Report model
// ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct Sample {
    round: usize,
    /// The whole round body, including the harness's own verification.
    millis: f64,
    metrics: BTreeMap<&'static str, f64>,
    note: Option<String>,
}

#[derive(Debug)]
struct Scenario {
    group: &'static str,
    name: String,
    /// What one round measures, e.g. "one download".
    unit: &'static str,
    /// Fixed facts of the scenario (sizes, configuration), for the report.
    meta: Vec<(&'static str, String)>,
    samples: Vec<Sample>,
}

impl Scenario {
    fn new(
        group: &'static str,
        name: impl Into<String>,
        unit: &'static str,
        meta: Vec<(&'static str, String)>,
    ) -> Self {
        Self {
            group,
            name: name.into(),
            unit,
            meta,
            samples: Vec::new(),
        }
    }

    fn values(&self, metric: &str) -> Vec<f64> {
        if metric == "millis" {
            return self.samples.iter().map(|sample| sample.millis).collect();
        }
        self.samples
            .iter()
            .filter_map(|sample| sample.metrics.get(metric).copied())
            .collect()
    }

    fn median(&self, metric: &str) -> Option<f64> {
        quantile(&self.values(metric), 0.5)
    }

    fn p25(&self, metric: &str) -> Option<f64> {
        quantile(&self.values(metric), 0.25)
    }

    fn p75(&self, metric: &str) -> Option<f64> {
        quantile(&self.values(metric), 0.75)
    }

    /// `median (p25-p75)`, the summary a ten-sample run can support.
    fn spread(&self, metric: &str) -> Option<String> {
        let median = self.median(metric)?;
        match (self.p25(metric), self.p75(metric)) {
            (Some(p25), Some(p75)) => Some(format!("{median:.3} ({p25:.3}-{p75:.3})")),
            _ => Some(format!("{median:.3}")),
        }
    }

    /// Summary cell for one metric: spread for a duration, the median for a
    /// count, because a count's quartiles are usually the same number.
    fn summary_cell(&self, metric: &str) -> Option<String> {
        if is_timed_metric(metric) {
            self.spread(metric)
        } else {
            self.median(metric).map(|value| format!("{value:.3}"))
        }
    }

    fn notes(&self) -> usize {
        self.samples
            .iter()
            .filter(|sample| sample.note.is_some())
            .count()
    }
}

/// Whether a metric is a duration, so its summary shows the spread.
fn is_timed_metric(metric: &str) -> bool {
    matches!(
        metric,
        "millis" | "total" | "finalize" | "report_span" | "to_first_report" | "micros_per_request"
    ) || metric.ends_with("_ms")
}

fn quantile(values: &[f64], q: f64) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    if sorted.len() == 1 {
        return Some(sorted[0]);
    }
    let position = q.clamp(0.0, 1.0) * (sorted.len() - 1) as f64;
    let lower = position.floor() as usize;
    let upper = position.ceil() as usize;
    let weight = position - lower as f64;
    Some(sorted[lower] * (1.0 - weight) + sorted[upper] * weight)
}

fn millis(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

/// One round's outcome: how long the measured operation took within the round,
/// what it produced, and an optional note (a failure shape, a timeout, an
/// unexpected result).
#[derive(Debug, Clone, Default)]
struct Round {
    metrics: BTreeMap<&'static str, f64>,
    note: Option<String>,
}

impl Round {
    fn new() -> Self {
        Self::default()
    }

    fn metric(mut self, name: &'static str, value: f64) -> Self {
        self.metrics.insert(name, value);
        self
    }

    fn optional(self, name: &'static str, value: Option<f64>) -> Self {
        match value {
            Some(value) => self.metric(name, value),
            None => self,
        }
    }

    fn merge(mut self, other: Round) -> Self {
        self.metrics.extend(other.metrics);
        self.note = other.note.or(self.note);
        self
    }

    fn note(mut self, note: impl Into<String>) -> Self {
        self.note = Some(note.into());
        self
    }
}

/// Runs one round per sample and collects them.
///
/// The closure returns a future so a round can await the download it measures.
/// Captures must be cheap to clone (`Arc`, `PathBuf`, `String`), because each
/// round gets its own copy.
async fn collect<F, Fut>(
    config: &Config,
    group: &'static str,
    name: &str,
    unit: &'static str,
    meta: Vec<(&'static str, String)>,
    mut body: F,
) -> Scenario
where
    F: FnMut(usize) -> Fut,
    Fut: Future<Output = Round>,
{
    let mut scenario = Scenario::new(group, name, unit, meta);
    if !config.selected(&scenario.name) {
        println!("skip (filtered): {}", scenario.name);
        return scenario;
    }
    println!("running {} x{}", scenario.name, config.rounds);
    for round_index in 0..config.rounds {
        let cpu_before = cpu_seconds();
        let started = Instant::now();
        let outcome = body(round_index).await;
        let elapsed = started.elapsed();
        let mut round = outcome;
        // CPU is measured over the whole round body, the same window as
        // `millis`, because that is what the process really spent: the fixture,
        // the downloader's threads and the harness's own verification all count.
        if let (Some(before), Some(after)) = (cpu_before, cpu_seconds()) {
            let cpu_ms = (after - before) * 1000.0;
            round = round
                .metric("cpu_ms", cpu_ms)
                .metric("cpu_percent", cpu_ms * 100.0 / millis(elapsed).max(0.001));
        }
        scenario.samples.push(Sample {
            round: round_index,
            millis: millis(elapsed),
            metrics: round.metrics,
            note: round.note,
        });
    }
    if let Some(sample) = scenario.samples.first() {
        println!(
            "  {} rounds, first note: {}",
            scenario.samples.len(),
            sample.note.as_deref().unwrap_or("none")
        );
    }
    scenario
}

// ──────────────────────────────────────────────────────────────
//  Local fixture
// ──────────────────────────────────────────────────────────────

/// How one fixture answers requests.
#[derive(Clone)]
struct FixtureShape {
    bytes: usize,
    /// Answer `Range` requests with 206 responses. When false the fixture
    /// ignores ranges and streams the whole body without `Content-Length`,
    /// which is the unknown-length shape.
    ranges: bool,
    /// Bytes per streamed chunk.
    chunk_size: usize,
    /// Delay before the response head, for the delayed-response shape.
    head_delay: Option<Duration>,
    /// Hold every body until the fixture is released.
    gate: Option<Arc<Gate>>,
    /// Send at most this many bytes per response while announcing the full
    /// range length: the cut-stream shape.
    cut_bytes: Option<usize>,
    /// Drip the tail: `(from_byte, chunk_size, delay)`.
    tail: Option<(usize, usize, Duration)>,
}

/// The gate a fixture holds its response bodies on.
///
/// It can be re-armed, because a gate that stayed open after one round's
/// release would let every later round's body through: the scenario would stop
/// measuring the blocking it exists for.
struct Gate {
    permits: Mutex<Arc<Semaphore>>,
}

impl Gate {
    fn new() -> Self {
        Self {
            permits: Mutex::new(Arc::new(Semaphore::new(0))),
        }
    }

    /// Installs a fresh closed gate for the next round.
    fn arm(&self) {
        *self.permits.lock().unwrap() = Arc::new(Semaphore::new(0));
    }

    /// The gate the current round's bodies wait on. Read per response, so a
    /// re-armed gate only affects requests that arrive after it.
    fn current(&self) -> Arc<Semaphore> {
        self.permits.lock().unwrap().clone()
    }

    /// Lets every body held by the current gate continue.
    fn release(&self) {
        self.current().add_permits(4096);
    }
}

impl FixtureShape {
    fn new(bytes: usize) -> Self {
        Self {
            bytes,
            ranges: true,
            chunk_size: 64 * 1024,
            head_delay: None,
            gate: None,
            cut_bytes: None,
            tail: None,
        }
    }

    fn ranges(mut self, ranges: bool) -> Self {
        self.ranges = ranges;
        self
    }

    fn chunk_size(mut self, chunk_size: usize) -> Self {
        self.chunk_size = chunk_size;
        self
    }

    fn head_delay(mut self, delay: Duration) -> Self {
        self.head_delay = Some(delay);
        self
    }

    fn gated(mut self) -> Self {
        self.gate = Some(Arc::new(Gate::new()));
        self
    }

    fn cut(mut self, bytes: usize) -> Self {
        self.cut_bytes = Some(bytes);
        self
    }

    fn slow_tail(mut self, from: usize, chunk_size: usize, delay: Duration) -> Self {
        self.tail = Some((from, chunk_size, delay));
        self
    }
}

/// The fixture's own view of one round: requests, ranges, bytes served and the
/// bytes it had to send again for a position it had already sent.
#[derive(Debug, Clone, Copy, Default)]
struct FixtureDelta {
    requests: usize,
    distinct_ranges: usize,
    served_bytes: u64,
    /// Bytes this round sent for file positions this same round already sent.
    duplicate_bytes: u64,
}

struct ActiveResponse(Arc<AtomicUsize>);

impl Drop for ActiveResponse {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

fn abort_benchmark(message: &str) -> ! {
    eprintln!("fatal benchmark error: {message}");
    std::process::exit(1);
}

struct ServerGuard(tokio::task::JoinHandle<()>);

impl Drop for ServerGuard {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// A local origin. Cloning shares it; the server stops with the last clone.
#[derive(Clone)]
struct Fixture {
    url: String,
    data: Arc<Vec<u8>>,
    requests: Arc<AtomicUsize>,
    served_bytes: Arc<AtomicU64>,
    ranges: Arc<Mutex<Vec<String>>>,
    /// One `(start, bytes actually sent)` entry per response, in request
    /// order. Retransmission is measured by overlapping a round's entries.
    sent: Arc<Mutex<Vec<(usize, usize)>>>,
    active: Arc<AtomicUsize>,
    gate: Option<Arc<Gate>>,
    _server: Arc<ServerGuard>,
}

impl Fixture {
    fn spawn(path: &'static str, shape: FixtureShape) -> Self {
        let data: Arc<Vec<u8>> = Arc::new(
            (0..shape.bytes as u32)
                .map(|index| (index % 251) as u8)
                .collect(),
        );
        let requests = Arc::new(AtomicUsize::new(0));
        let served_bytes = Arc::new(AtomicU64::new(0));
        let ranges = Arc::new(Mutex::new(Vec::new()));
        let sent = Arc::new(Mutex::new(Vec::new()));
        let active = Arc::new(AtomicUsize::new(0));
        let gate = shape.gate.clone();

        let route = warp::path(path)
            .and(warp::header::optional::<String>("range"))
            .and_then({
                let data = data.clone();
                let requests = requests.clone();
                let served_bytes = served_bytes.clone();
                let ranges = ranges.clone();
                let sent = sent.clone();
                let active = active.clone();
                let shape = shape.clone();
                move |range_header: Option<String>| {
                    let data = data.clone();
                    let requests = requests.clone();
                    let served_bytes = served_bytes.clone();
                    let ranges = ranges.clone();
                    let sent = sent.clone();
                    let active = active.clone();
                    let shape = shape.clone();
                    async move {
                        active.fetch_add(1, Ordering::SeqCst);
                        let active_guard = ActiveResponse(active);
                        requests.fetch_add(1, Ordering::SeqCst);
                        ranges
                            .lock()
                            .unwrap()
                            .push(range_header.clone().unwrap_or_else(|| "full".to_string()));

                        let total = data.len();
                        let ranged = if shape.ranges {
                            range_header.as_deref()
                        } else {
                            None
                        };
                        let (start, end) = match ranged {
                            Some(header) => parse_range(header, total),
                            None => (0, total.saturating_sub(1)),
                        };
                        let range_len = end.saturating_sub(start) + 1;
                        // Reserve in request order, before any asynchronous wait.
                        let record = {
                            let mut records = sent.lock().unwrap();
                            let record = records.len();
                            records.push((start, 0));
                            record
                        };

                        let mut response = warp::http::Response::builder()
                            .status(if ranged.is_some() { 206 } else { 200 });
                        if ranged.is_some() {
                            response = response
                                .header("content-range", format!("bytes {start}-{end}/{total}"));
                        }
                        if shape.ranges {
                            response = response
                                .header("accept-ranges", "bytes")
                                .header("etag", "\"pipeline-bench\"")
                                .header("last-modified", "Sat, 01 Jan 2026 00:00:00 GMT");
                        }
                        // The unknown-length shape announces nothing and streams.
                        if shaped_length_is_announced(&shape, ranged.is_some()) {
                            response = response.header("content-length", range_len.to_string());
                        }

                        // The delayed-response shape delays the head itself.
                        // Delaying the body task instead would let the client
                        // see headers immediately and measure a slow body.
                        if let Some(delay) = shape.head_delay {
                            tokio::time::sleep(delay).await;
                        }

                        let (mut sender, body) = warp::hyper::Body::channel();
                        let task_shape = shape.clone();
                        let data = data.clone();
                        let served = served_bytes.clone();
                        let sent = sent.clone();
                        let gate = shape.gate.clone();
                        tokio::spawn(async move {
                            let _active_guard = active_guard;
                            let shape = task_shape;
                            if let Some(gate) = &gate {
                                // Read per response: the harness re-arms the
                                // gate for the next round.
                                let held = gate.current();
                                let _permit = held.acquire().await;
                            }
                            let send_limit =
                                shape.cut_bytes.map_or(range_len, |cut| cut.min(range_len));
                            let mut offset = 0usize;
                            while offset < send_limit {
                                // `tail` is expressed in file positions, so it is
                                // compared against the absolute position: a request
                                // for a later range has to be the one that drips.
                                let (chunk, delay) = match shape.tail {
                                    Some((from, chunk, delay)) if start + offset >= from => {
                                        (chunk, Some(delay))
                                    }
                                    _ => (shape.chunk_size.max(1), None),
                                };
                                let chunk_end = (offset + chunk).min(send_limit);
                                let slice = data[start + offset..start + chunk_end].to_vec();
                                if sender.send_data(slice.into()).await.is_err() {
                                    break;
                                }
                                served.fetch_add((chunk_end - offset) as u64, Ordering::Relaxed);
                                offset = chunk_end;
                                if let Some(delay) = delay.filter(|_| offset < send_limit) {
                                    tokio::time::sleep(delay).await;
                                }
                            }
                            sent.lock().unwrap()[record] = (start, offset);
                        });

                        Ok::<_, std::convert::Infallible>(response.body(body).unwrap())
                    }
                }
            });

        let (addr, server) = warp::serve(route).bind_ephemeral(([127, 0, 0, 1], 0));
        Self {
            url: format!("http://{addr}/{path}"),
            data,
            requests,
            served_bytes,
            ranges,
            sent,
            active,
            gate,
            _server: Arc::new(ServerGuard(tokio::spawn(server))),
        }
    }

    /// The exact body the fixture serves, for output verification.
    fn expected(&self) -> Arc<Vec<u8>> {
        self.data.clone()
    }

    fn counters(&self) -> (usize, u64, usize, usize) {
        (
            self.requests.load(Ordering::SeqCst),
            self.served_bytes.load(Ordering::Relaxed),
            self.ranges.lock().unwrap().len(),
            self.sent.lock().unwrap().len(),
        )
    }

    fn delta_since(&self, before: (usize, u64, usize, usize)) -> FixtureDelta {
        let now = self.counters();
        let ranges = self.ranges.lock().unwrap();
        let fresh = &ranges[before.2.min(ranges.len())..];
        let distinct: HashSet<&String> = fresh.iter().collect();
        let sent = self.sent.lock().unwrap();
        // Duplicates are measured inside this round, against responses this
        // same round already sent. Comparing with an earlier round would call
        // every byte of a re-download a duplicate, which is what a fresh round
        // does by design.
        let window = &sent[before.3.min(sent.len())..];
        let duplicate_bytes = window
            .iter()
            .enumerate()
            .map(|(index, (start, len))| overlap_with_earlier(&window[..index], *start, *len))
            .sum();
        FixtureDelta {
            requests: now.0.saturating_sub(before.0),
            distinct_ranges: distinct.len(),
            served_bytes: now.1.saturating_sub(before.1),
            duplicate_bytes,
        }
    }

    /// Closes the gate again, so the next round starts blocked.
    fn arm_gate(&self) {
        if let Some(gate) = &self.gate {
            gate.arm();
        }
    }

    fn release(&self) {
        if let Some(gate) = &self.gate {
            gate.release();
        }
    }
}

/// Bytes of `[start, start + len)` an earlier response already sent.
fn overlap_with_earlier(earlier: &[(usize, usize)], start: usize, len: usize) -> u64 {
    let end = start + len;
    let mut intervals: Vec<(usize, usize)> = earlier.to_vec();
    intervals.sort_by_key(|(other_start, _)| *other_start);
    let mut covered = 0u64;
    let mut reach = start;
    for (other_start, other_len) in intervals {
        let from = other_start.max(start).max(reach);
        let to = (other_start + other_len).min(end);
        if to > from {
            covered += (to - from) as u64;
            reach = to;
        }
    }
    covered
}

/// A range response always announces its length — that is what makes a cut
/// stream observable as a short body. A full-body response announces it too,
/// unless the shape is the unknown-length one, which streams chunked instead.
fn shaped_length_is_announced(shape: &FixtureShape, ranged: bool) -> bool {
    ranged || shape.ranges || shape.cut_bytes.is_some()
}

fn parse_range(header: &str, total: usize) -> (usize, usize) {
    let range = header.trim_start_matches("bytes=");
    let (start, end) = range.split_once('-').unwrap_or((range, ""));
    let start: usize = start.parse().unwrap_or(0);
    let end = if end.is_empty() {
        total.saturating_sub(1)
    } else {
        end.parse::<usize>().unwrap_or(total - 1).min(total - 1)
    };
    (start.min(total.saturating_sub(1)), end)
}

// ──────────────────────────────────────────────────────────────
//  Measurement helpers
// ──────────────────────────────────────────────────────────────

#[cfg(windows)]
#[repr(C)]
struct ProcessMemoryCounters {
    cb: u32,
    page_fault_count: u32,
    peak_working_set_size: usize,
    working_set_size: usize,
    quota_peak_paged_pool_usage: usize,
    quota_paged_pool_usage: usize,
    quota_peak_non_paged_pool_usage: usize,
    quota_non_paged_pool_usage: usize,
    pagefile_usage: usize,
    peak_pagefile_usage: usize,
}

#[cfg(windows)]
#[link(name = "psapi")]
extern "system" {
    fn GetProcessMemoryInfo(
        process: *mut std::ffi::c_void,
        counters: *mut ProcessMemoryCounters,
        cb: u32,
    ) -> i32;
}

#[cfg(windows)]
#[repr(C)]
struct FileTime {
    low: u32,
    high: u32,
}

#[cfg(windows)]
#[link(name = "kernel32")]
extern "system" {
    fn GetCurrentProcess() -> *mut std::ffi::c_void;
    fn GetProcessTimes(
        process: *mut std::ffi::c_void,
        creation: *mut FileTime,
        exit: *mut FileTime,
        kernel: *mut FileTime,
        user: *mut FileTime,
    ) -> i32;
}

#[cfg(windows)]
fn file_time_ticks(time: FileTime) -> u64 {
    ((time.high as u64) << 32) | time.low as u64
}

/// CPU time this process has consumed (user + kernel), in seconds.
///
/// Every thread counts, including the fixture's and the libcurl driver's, so a
/// round that spins shows up as CPU time even when it is waiting on nothing.
#[cfg(windows)]
fn cpu_seconds() -> Option<f64> {
    let mut creation = unsafe { std::mem::zeroed::<FileTime>() };
    let mut exit = unsafe { std::mem::zeroed::<FileTime>() };
    let mut kernel = unsafe { std::mem::zeroed::<FileTime>() };
    let mut user = unsafe { std::mem::zeroed::<FileTime>() };
    let ok = unsafe {
        GetProcessTimes(
            GetCurrentProcess(),
            &mut creation,
            &mut exit,
            &mut kernel,
            &mut user,
        )
    };
    if ok == 0 {
        return None;
    }
    // `GetProcessTimes` reports 100 ns units.
    Some((file_time_ticks(kernel) + file_time_ticks(user)) as f64 / 10_000_000.0)
}

/// CPU time this process has consumed (user + kernel), in seconds.
#[cfg(target_os = "linux")]
fn cpu_seconds() -> Option<f64> {
    extern "C" {
        fn sysconf(name: i32) -> i64;
    }
    // `_SC_CLK_TCK`: the number of `utime`/`stime` ticks per second.
    const SC_CLK_TCK: i32 = 2;
    let ticks_per_second = unsafe { sysconf(SC_CLK_TCK) };
    if ticks_per_second <= 0 {
        return None;
    }
    let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
    // Field 2 is the command name and may contain spaces, so parsing starts
    // after its closing parenthesis: index 0 is field 3 (the state).
    let fields: Vec<&str> = stat.rsplit_once(')')?.1.split_whitespace().collect();
    let user: u64 = fields.get(11)?.parse().ok()?;
    let system: u64 = fields.get(12)?.parse().ok()?;
    Some((user + system) as f64 / ticks_per_second as f64)
}

#[cfg(not(any(windows, target_os = "linux")))]
fn cpu_seconds() -> Option<f64> {
    None
}

/// Process memory as `(current working set, peak working set)`.
///
/// The peak is a process-wide high-water mark that only grows, so it describes
/// the largest shape the whole run reached; the current working set before and
/// after a round is what attributes memory to one shape.
#[cfg(windows)]
fn memory_bytes() -> (Option<u64>, Option<u64>) {
    let mut counters = unsafe { std::mem::zeroed::<ProcessMemoryCounters>() };
    counters.cb = std::mem::size_of::<ProcessMemoryCounters>() as u32;
    let ok = unsafe { GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, counters.cb) };
    if ok == 0 {
        return (None, None);
    }
    (
        Some(counters.working_set_size as u64),
        Some(counters.peak_working_set_size as u64),
    )
}

#[cfg(target_os = "linux")]
fn memory_bytes() -> (Option<u64>, Option<u64>) {
    let Ok(status) = std::fs::read_to_string("/proc/self/status") else {
        return (None, None);
    };
    let read = |name: &str| {
        status
            .lines()
            .find_map(|line| line.strip_prefix(name))
            .and_then(|value| value.split_whitespace().next())
            .and_then(|value| value.parse::<u64>().ok())
            .map(|kib| kib * 1024)
    };
    (read("VmRSS:"), read("VmHWM:"))
}

#[cfg(not(any(windows, target_os = "linux")))]
fn memory_bytes() -> (Option<u64>, Option<u64>) {
    (None, None)
}

/// Confirms the output is exactly the body the fixture served.
///
/// The comparison runs after the measured operation, so reading the file back
/// is not part of any timing.
fn verify_output(path: &Path, expected: &[u8]) -> bool {
    match std::fs::read(path) {
        Ok(actual) => actual == expected,
        Err(_) => false,
    }
}

/// Captures the library's debug log so the report can record which libcurl and
/// TLS build the numbers came from.
///
/// The subscriber is installed once as the global default and disabled except
/// during the probe: library debug logging during the measured scenarios would
/// both slow them down and grow this buffer without bound.
struct LogCapture {
    lines: Arc<Mutex<Vec<String>>>,
    capturing: Arc<AtomicBool>,
}

impl LogCapture {
    fn install() -> Self {
        let capture = Self {
            lines: Arc::new(Mutex::new(Vec::new())),
            capturing: Arc::new(AtomicBool::new(false)),
        };
        let subscriber = tracing_subscriber::fmt::Subscriber::builder()
            .with_ansi(false)
            .with_max_level(tracing::Level::DEBUG)
            .with_writer(CaptureWriter {
                lines: capture.lines.clone(),
                capturing: capture.capturing.clone(),
            })
            .finish();
        let _ = tracing::subscriber::set_global_default(subscriber);
        capture
    }

    async fn probe(&self, dir: &Path) -> Option<String> {
        self.capturing.store(true, Ordering::SeqCst);
        let fixture = Fixture::spawn("features", FixtureShape::new(SMALL_BYTES));
        let downloader = Downloader::builder()
            .log_level(LogLevel::Debug)
            .build()
            .ok()?;
        let spec = DownloadSpec::new(fixture.url.clone())
            .output_path(dir.join("features.bin"))
            .max_connections(1)
            .min_split_size(1);
        let _ = downloader.download(spec).wait().await;
        drop(downloader);
        self.capturing.store(false, Ordering::SeqCst);

        let lines = self.lines.lock().unwrap();
        lines
            .iter()
            .find(|line| line.contains("libcurl runtime features"))
            .map(|line| {
                line.split("libcurl runtime features")
                    .nth(1)
                    .map(|rest| rest.trim().to_string())
                    .unwrap_or_else(|| line.clone())
            })
    }
}

struct CaptureWriter {
    lines: Arc<Mutex<Vec<String>>>,
    capturing: Arc<AtomicBool>,
}

impl std::io::Write for CaptureWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.capturing.load(Ordering::Relaxed) {
            if let Ok(text) = std::str::from_utf8(buf) {
                for line in text.lines() {
                    if !line.trim().is_empty() {
                        self.lines.lock().unwrap().push(line.to_string());
                    }
                }
            }
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CaptureWriter {
    type Writer = CaptureWriter;

    fn make_writer(&'a self) -> Self::Writer {
        CaptureWriter {
            lines: self.lines.clone(),
            capturing: self.capturing.clone(),
        }
    }
}

/// Waits until `predicate` holds, returning whether it did.
async fn wait_until(mut predicate: impl FnMut() -> bool, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if predicate() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// Waits for released clients to stop their driver threads.
async fn wait_for_driver_threads(baseline: i64, timeout: Duration) -> bool {
    wait_until(|| bench_driver_threads() <= baseline, timeout).await
}

// ──────────────────────────────────────────────────────────────
//  Timeline and download rounds
// ──────────────────────────────────────────────────────────────

/// Where a download's time went, observed from the caller's side.
///
/// The boundaries come from the progress channel, so their resolution is the
/// library's own reporting cadence (`PROGRESS_REPORT_BYTES` or
/// `PROGRESS_REPORT_INTERVAL`) and the sample count is reported as
/// `running_reports`. A download that finishes inside one reporting interval
/// therefore shows one running sample, `body == 0`, and all of its body time
/// inside `to_first_report`.
#[derive(Default)]
struct Timeline {
    first: Mutex<Option<f64>>,
    last: Mutex<Option<f64>>,
    reports: AtomicUsize,
}

impl Timeline {
    fn observe(&self, started: Instant, downloaded: u64, state: bytehaul::DownloadState) {
        if is_terminal(state) || downloaded == 0 {
            return;
        }
        let elapsed = millis(started.elapsed());
        self.reports.fetch_add(1, Ordering::Relaxed);
        let mut first = self.first.lock().unwrap();
        if first.is_none() {
            *first = Some(elapsed);
        }
        drop(first);
        *self.last.lock().unwrap() = Some(elapsed);
    }

    fn metrics(&self, total_millis: f64) -> Round {
        let first = *self.first.lock().unwrap();
        let last = *self.last.lock().unwrap();
        let mut round = Round::new().metric(
            "running_reports",
            self.reports.load(Ordering::Relaxed) as f64,
        );
        if let Some(first) = first {
            let last = last.unwrap_or(first);
            round = round
                .metric("to_first_report", first)
                .metric("report_span", last - first)
                .metric("finalize", (total_millis - last).max(0.0));
        }
        round
    }
}

fn is_terminal(state: bytehaul::DownloadState) -> bool {
    use bytehaul::DownloadState;
    matches!(
        state,
        DownloadState::Completed
            | DownloadState::Failed
            | DownloadState::Cancelled
            | DownloadState::Paused
    )
}

struct DownloadOutcome {
    result: Result<(), bytehaul::DownloadError>,
    counters: BenchCounters,
    timeline: Round,
    fixture: FixtureDelta,
    elapsed_millis: f64,
    file_bytes: u64,
    verified: bool,
    timed_out: bool,
}

#[allow(clippy::too_many_arguments)]
async fn measure_download(
    downloader: &Downloader,
    fixture: &Fixture,
    output: &Path,
    expected: &Arc<Vec<u8>>,
    configure: impl FnOnce(DownloadSpec) -> DownloadSpec,
    verify: bool,
) -> DownloadOutcome {
    let timeline = Timeline::default();
    bench_counters_reset();
    let before = fixture.counters();

    let spec = configure(DownloadSpec::new(fixture.url.clone())).output_path(output);
    let handle = downloader.download(spec);
    let started = Instant::now();

    // The progress channel is observed from this task, and the download is
    // followed through it rather than through `wait()`. `wait()` consumes the
    // handle, and dropping it does not cancel anything, so a round that has to
    // be stopped needs the handle to stay callable — and the next round must not
    // start measuring process-wide counters while this download still runs.
    let mut progress = handle.subscribe_progress();
    let deadline = tokio::time::Instant::now() + ROUND_TIMEOUT;
    let mut cancel_started: Option<Instant> = None;
    let mut timed_out = false;
    loop {
        if is_terminal(progress.borrow().state) {
            break;
        }
        let waiting_until = match cancel_started {
            // After a cancel the task only has the grace period left: if it
            // still has not published a terminal state, the round is reported
            // as one that never finished.
            Some(cancelled_at) if Instant::now() >= cancelled_at + CANCEL_GRACE => break,
            Some(cancelled_at) => tokio::time::Instant::from_std(cancelled_at + CANCEL_GRACE),
            None => deadline,
        };
        match tokio::time::timeout_at(waiting_until, progress.changed()).await {
            Ok(Ok(())) => {
                let snapshot = progress.borrow().clone();
                timeline.observe(started, snapshot.downloaded, snapshot.state);
            }
            // The task dropped its sender without publishing a terminal state;
            // `wait()` below returns whatever it produced.
            Ok(Err(_)) => break,
            Err(_) if cancel_started.is_none() => {
                timed_out = true;
                cancel_started = Some(Instant::now());
                handle.cancel();
            }
            Err(_) => break,
        }
    }
    let elapsed_millis = millis(started.elapsed());
    let result = match tokio::time::timeout(CANCEL_GRACE, handle.wait()).await {
        Ok(result) => result,
        Err(_) => abort_benchmark("download did not finalize after cancel; counters are unsafe"),
    };
    if !wait_until(|| fixture.active.load(Ordering::SeqCst) == 0, CANCEL_GRACE).await {
        abort_benchmark("fixture responses did not settle; counters are unsafe");
    }
    let counters = bench_counters_snapshot();
    let fixture = fixture.delta_since(before);
    let file_bytes = std::fs::metadata(output)
        .map(|meta| meta.len())
        .unwrap_or(0);
    let verified = verify && result.is_ok() && verify_output(output, expected);

    DownloadOutcome {
        result,
        counters,
        timeline: timeline.metrics(elapsed_millis),
        fixture,
        elapsed_millis,
        file_bytes,
        verified,
        timed_out,
    }
}

fn download_round(outcome: &DownloadOutcome, expect_failure: bool) -> Round {
    let counters = outcome.counters;
    let served = outcome.fixture.served_bytes;
    let duplicate = outcome.fixture.duplicate_bytes.min(served);
    let mut round = outcome
        .timeline
        .clone()
        .metric("total", outcome.elapsed_millis)
        .metric("file_bytes", outcome.file_bytes as f64)
        .metric("requests", outcome.fixture.requests as f64)
        .metric("distinct_ranges", outcome.fixture.distinct_ranges as f64)
        .metric("served_bytes", served as f64)
        .metric("duplicate_bytes", duplicate as f64)
        .metric("unique_bytes", served.saturating_sub(duplicate) as f64)
        .metric("verified", f64::from(outcome.verified))
        .metric("failed", f64::from(outcome.result.is_err()))
        .metric("writer_blocks", counters.writer_blocks as f64)
        .metric("writer_bytes", counters.writer_bytes as f64)
        .metric(
            "avg_block_bytes",
            if counters.writer_blocks == 0 {
                0.0
            } else {
                counters.writer_bytes as f64 / counters.writer_blocks as f64
            },
        )
        .metric("cache_copied_bytes", counters.cache_copied_bytes as f64)
        .metric("cache_evicted_bytes", counters.cache_evicted_bytes as f64)
        .metric("fsync_calls", counters.fsync_calls as f64)
        .metric("fsync_ms", counters.fsync_micros as f64 / 1000.0)
        .metric("prealloc_ms", counters.prealloc_micros as f64 / 1000.0)
        .metric("response_heads", counters.response_heads as f64)
        .metric(
            "response_head_ms",
            counters.response_head_micros as f64 / 1000.0,
        )
        .metric("body_reads", counters.body_reads as f64)
        .metric("body_wait_ms", counters.body_read_micros as f64 / 1000.0)
        .metric("checksum_ms", counters.checksum_micros as f64 / 1000.0);

    // A failure the shape deliberately provokes is a result, not a problem:
    // it is reported through `failed`, and only an unexpected one is noted.
    if let Err(error) = &outcome.result {
        if outcome.timed_out {
            round = round.note("harness round timeout (the download was cancelled)");
        } else if !expect_failure {
            round = round.note(error.to_string());
        }
    } else if expect_failure {
        round = round.note("expected this shape to fail, but it succeeded");
    }
    round
}

// ──────────────────────────────────────────────────────────────
//  Group 1: the request planner
// ──────────────────────────────────────────────────────────────

/// Piece patterns the planner has to handle: nothing done, holes left by a
/// checkpoint, and a resumed suffix.
fn piece_map(pieces: usize, pattern: &str) -> PieceMap {
    let total = pieces as u64 * PIECE_SIZE;
    let mut map = PieceMap::new(total, PIECE_SIZE);
    match pattern {
        "holes" => {
            // Every second piece of the first half is done: interleaved holes
            // are what a checkpoint with dirty pieces really looks like.
            for piece in (0..pieces / 2).step_by(2) {
                map.mark_complete(piece);
            }
        }
        "resumed_suffix" => {
            for piece in (pieces * 3 / 4)..pieces {
                map.mark_complete(piece);
            }
            map.mark_complete(0);
        }
        _ => {}
    }
    map
}

fn plan_round(plan: &BenchPlanResult, elapsed_millis: f64) -> Round {
    let requests = plan.requests.max(1) as f64;
    Round::new()
        .metric("total", elapsed_millis)
        .metric("micros_per_request", elapsed_millis * 1000.0 / requests)
        .metric("requests", plan.requests as f64)
        .metric("leases", plan.leases as f64)
        .metric("planned_bytes", plan.bytes as f64)
        .metric("boundary_scans", plan.boundary_scans as f64)
        .metric("planned_ranges", plan.planned_ranges as f64)
        .metric("candidate_slots", plan.candidate_slots as f64)
        .metric("truncated_requests", plan.truncated_requests as f64)
        .metric("single_lease_requests", plan.single_lease_requests as f64)
        .metric("multi_lease_requests", plan.multi_lease_requests as f64)
        .metric("max_leases_per_request", plan.max_leases_per_request as f64)
        .metric("max_request_bytes", plan.max_request_bytes as f64)
}

async fn run_scheduler(config: &Config) -> Vec<Scenario> {
    const GROUP: &str = "scheduler";
    let mut scenarios = Vec::new();

    for pieces in [64usize, 512, 4096] {
        for pattern in ["fresh", "holes", "resumed_suffix"] {
            for mode in [RangeSchedulingMode::Dynamic, RangeSchedulingMode::Fixed] {
                let name = format!("scheduler/{mode}/{pattern}/{pieces}");
                let params = BenchPlanParams {
                    mode,
                    ..BenchPlanParams::default()
                };
                let scenario = collect(
                    config,
                    GROUP,
                    &name,
                    "one planning run",
                    vec![
                        ("pieces", pieces.to_string()),
                        ("piece_size", PIECE_SIZE.to_string()),
                        ("mode", mode.to_string()),
                        ("pattern", pattern.to_string()),
                        ("in_flight_requests", params.in_flight_requests.to_string()),
                    ],
                    move |_| {
                        let params = params;
                        async move {
                            let mut scheduler =
                                bench_scheduler_from_piece_map(piece_map(pieces, pattern));
                            let started = Instant::now();
                            let plan = bench_scheduler_plan(&mut scheduler, &params);
                            let elapsed = millis(started.elapsed());
                            plan_round(&plan, elapsed)
                        }
                    },
                )
                .await;
                scenarios.push(scenario);
            }
        }
    }

    scenarios
}

// ──────────────────────────────────────────────────────────────
//  Group 2: the write path
// ──────────────────────────────────────────────────────────────

struct WriterVariant {
    name: &'static str,
    shape: FixtureShape,
    connections: u32,
    memory_budget: Option<usize>,
    allocation: FileAllocation,
    extra_meta: Vec<(&'static str, String)>,
}

fn writer_variants() -> Vec<WriterVariant> {
    vec![
        WriterVariant {
            name: "writer/split_4conns_16KiB_chunks",
            shape: FixtureShape::new(SPLIT_BYTES).chunk_size(16 * 1024),
            connections: 4,
            memory_budget: None,
            allocation: FileAllocation::None,
            extra_meta: vec![("chunk_bytes", (16 * 1024).to_string())],
        },
        WriterVariant {
            name: "writer/split_4conns_1MiB_chunks",
            shape: FixtureShape::new(SPLIT_BYTES).chunk_size(1024 * 1024),
            connections: 4,
            memory_budget: None,
            allocation: FileAllocation::None,
            extra_meta: vec![("chunk_bytes", (1024 * 1024).to_string())],
        },
        WriterVariant {
            name: "writer/split_tiny_budget_1MiB",
            shape: FixtureShape::new(SPLIT_BYTES).chunk_size(64 * 1024),
            connections: 4,
            memory_budget: Some(1024 * 1024),
            allocation: FileAllocation::None,
            extra_meta: vec![("memory_budget", (1024 * 1024).to_string())],
        },
        WriterVariant {
            name: "writer/split_prealloc_on",
            shape: FixtureShape::new(SPLIT_BYTES).chunk_size(64 * 1024),
            connections: 4,
            memory_budget: None,
            allocation: FileAllocation::Prealloc,
            extra_meta: vec![("allocation", "prealloc".into())],
        },
        WriterVariant {
            name: "writer/single_conn_4MiB",
            shape: FixtureShape::new(4 * 1024 * 1024).chunk_size(64 * 1024),
            connections: 1,
            memory_budget: None,
            allocation: FileAllocation::None,
            extra_meta: vec![("connections", "1".into())],
        },
    ]
}

async fn run_writer(config: &Config, dir: &Path) -> Vec<Scenario> {
    const GROUP: &str = "writer";
    let mut scenarios = Vec::new();

    for variant in writer_variants() {
        let bytes = variant.shape.bytes;
        let fixture = Fixture::spawn("writer", variant.shape);
        let expected = fixture.expected();
        let out_dir = dir.join(variant.name.replace('/', "_"));
        let _ = std::fs::create_dir_all(&out_dir);
        let connections = variant.connections;
        let memory_budget = variant.memory_budget;
        let allocation = variant.allocation;
        let mut meta = vec![
            ("bytes", bytes.to_string()),
            ("connections", connections.to_string()),
        ];
        meta.extend(variant.extra_meta.clone());

        let scenario = collect(
            config,
            GROUP,
            variant.name,
            "one download",
            meta,
            move |round_index| {
                let fixture = fixture.clone();
                let expected = expected.clone();
                let out_dir = out_dir.clone();
                async move {
                    let Ok(downloader) = Downloader::builder().build() else {
                        return Round::new().note("client build failed");
                    };
                    let output = out_dir.join(format!("round-{round_index}.bin"));
                    let outcome = measure_download(
                        &downloader,
                        &fixture,
                        &output,
                        &expected,
                        move |spec| {
                            let mut spec = spec
                                .max_connections(connections)
                                .file_allocation(allocation);
                            if let Some(budget) = memory_budget {
                                spec = spec.memory_budget(budget);
                            }
                            spec
                        },
                        true,
                    )
                    .await;
                    let round = download_round(&outcome, false);
                    let _ = std::fs::remove_file(&output);
                    drop(downloader);
                    round
                }
            },
        )
        .await;
        scenarios.push(scenario);
    }

    scenarios
}

// ──────────────────────────────────────────────────────────────
//  Group 3: client cache and driver lifetime
// ──────────────────────────────────────────────────────────────

async fn run_client(config: &Config, dir: &Path) -> Vec<Scenario> {
    const GROUP: &str = "client";
    let mut scenarios = Vec::new();

    for distinct in [2usize, 8, 32] {
        let name = format!("client/distinct_connect_timeouts/{distinct}");
        let scenario = collect(
            config,
            GROUP,
            &name,
            "one client cache session",
            vec![("distinct_configs", distinct.to_string())],
            move |_| async move {
                let baseline = bench_driver_threads();
                let session_started = Instant::now();
                let Ok(downloader) = Downloader::builder().build() else {
                    return Round::new().note("client build failed");
                };
                for index in 0..distinct {
                    let timeout = Duration::from_millis(500 + index as u64);
                    bench_cached_client_lookup(&downloader, timeout, 1);
                }
                let entries = bench_cached_client_count(&downloader);
                let threads = bench_driver_threads();
                // `total` is the measured operation only: creating the
                // downloader and looking up every distinct configuration. The
                // wait for released threads is reported on its own.
                let total_ms = millis(session_started.elapsed());
                drop(downloader);
                let released = wait_for_driver_threads(baseline, Duration::from_secs(10)).await;
                Round::new()
                    .metric("total", total_ms)
                    .metric("cache_entries", entries as f64)
                    .metric("driver_threads", threads as f64)
                    .metric("driver_threads_after_drop", bench_driver_threads() as f64)
                    .metric("residual_released", f64::from(released))
            },
        )
        .await;
        scenarios.push(scenario);
    }

    let scenario = collect(
        config,
        GROUP,
        "client/reuse_same_config",
        "one client cache session",
        vec![("lookups", "40".into()), ("distinct_configs", "1".into())],
        |_| async move {
            let baseline = bench_driver_threads();
            let session_started = Instant::now();
            let Ok(downloader) = Downloader::builder().build() else {
                return Round::new().note("client build failed");
            };
            for _ in 0..40 {
                bench_cached_client_lookup(&downloader, Duration::from_secs(30), 1);
            }
            let entries = bench_cached_client_count(&downloader);
            let threads = bench_driver_threads();
            let total_ms = millis(session_started.elapsed());
            drop(downloader);
            let released = wait_for_driver_threads(baseline, Duration::from_secs(10)).await;
            Round::new()
                .metric("total", total_ms)
                .metric("cache_entries", entries as f64)
                .metric("driver_threads", threads as f64)
                .metric("driver_threads_after_drop", bench_driver_threads() as f64)
                .metric("residual_released", f64::from(released))
        },
    )
    .await;
    scenarios.push(scenario);

    // Queueing: with one concurrency permit, the second download waits for the
    // first one's permit. This is the only "queueing" a download can see, and
    // `queue_ms` is where it is recorded.
    {
        let fixture = Fixture::spawn("client_queue", FixtureShape::new(SMALL_BYTES).gated());
        let expected = fixture.expected();
        let out_dir = dir.join("client_queue");
        let _ = std::fs::create_dir_all(&out_dir);
        let scenario = collect(
            config,
            GROUP,
            "client/queue_with_one_permit",
            "two downloads against one permit",
            vec![
                ("bytes_each", SMALL_BYTES.to_string()),
                ("hold_ms", "150".into()),
            ],
            move |round_index| {
                let fixture = fixture.clone();
                let expected = expected.clone();
                let out_dir = out_dir.clone();
                async move {
                    let baseline = bench_driver_threads();
                    let Ok(downloader) = Downloader::builder().max_concurrent_downloads(1).build()
                    else {
                        return Round::new().note("client build failed");
                    };
                    // A fresh closed gate per round: the previous round's
                    // release would otherwise let both downloads through and
                    // stop measuring the queueing this scenario is about.
                    fixture.arm_gate();
                    bench_counters_reset();
                    let session_started = Instant::now();
                    let spec = |name: &str| {
                        DownloadSpec::new(fixture.url.clone())
                            .output_path(out_dir.join(name))
                            .max_connections(1)
                    };
                    let first = downloader.download(spec(&format!("round-{round_index}-a.bin")));
                    let before = fixture.counters();
                    if !wait_until(
                        || fixture.delta_since(before).requests > 0,
                        Duration::from_secs(10),
                    )
                    .await
                    {
                        return Round::new().note("the fixture never saw the first request");
                    }
                    let second = downloader.download(spec(&format!("round-{round_index}-b.bin")));
                    tokio::time::sleep(Duration::from_millis(150)).await;
                    let admitted = bench_counters_snapshot().queue_waits;
                    fixture.release();
                    let first_result = first.wait().await;
                    let second_result = second.wait().await;
                    let counters = bench_counters_snapshot();
                    let total_ms = millis(session_started.elapsed());
                    let verified = first_result.is_ok()
                        && second_result.is_ok()
                        && verify_output(
                            &out_dir.join(format!("round-{round_index}-a.bin")),
                            expected.as_slice(),
                        )
                        && verify_output(
                            &out_dir.join(format!("round-{round_index}-b.bin")),
                            expected.as_slice(),
                        );
                    for name in ["a", "b"] {
                        let _ = std::fs::remove_file(
                            out_dir.join(format!("round-{round_index}-{name}.bin")),
                        );
                    }
                    drop(downloader);
                    let released = wait_for_driver_threads(baseline, Duration::from_secs(10)).await;
                    let mut round = Round::new()
                        .metric("total", total_ms)
                        .metric("queue_waits", counters.queue_waits as f64)
                        .metric("queue_ms", counters.queue_micros as f64 / 1000.0)
                        .metric("admitted_before_release", admitted as f64)
                        .metric("residual_released", f64::from(released))
                        .metric("verified", f64::from(verified));
                    if let Err(error) = first_result.as_ref().and(second_result.as_ref()) {
                        round = round.note(error.to_string());
                    }
                    if counters.queue_waits != 2 {
                        round = round.note(format!(
                            "expected two admissions, saw {}",
                            counters.queue_waits
                        ));
                    }
                    round
                }
            },
        )
        .await;
        scenarios.push(scenario);
    }

    // Connection reuse: two sequential downloads from one downloader and one
    // origin must open one connection, not two.
    let fixture = Fixture::spawn("reuse", FixtureShape::new(SPLIT_BYTES));
    let expected = fixture.expected();
    let out_dir = dir.join("client_reuse");
    let _ = std::fs::create_dir_all(&out_dir);
    let scenario = collect(
        config,
        GROUP,
        "client/connection_reuse_two_downloads",
        "two sequential downloads",
        vec![("bytes_each", SPLIT_BYTES.to_string())],
        move |round_index| {
            let fixture = fixture.clone();
            let expected = expected.clone();
            let out_dir = out_dir.clone();
            async move {
                let baseline = bench_driver_threads();
                let Ok(downloader) = Downloader::builder().build() else {
                    return Round::new().note("client build failed");
                };
                let session_started = Instant::now();
                let mut connections = 0u64;
                for index in 0..2 {
                    let output = out_dir.join(format!("round-{round_index}-{index}.bin"));
                    measure_download(
                        &downloader,
                        &fixture,
                        &output,
                        &expected,
                        |spec| spec.max_connections(4),
                        true,
                    )
                    .await;
                    let _ = std::fs::remove_file(&output);
                    if let Some(stats) = bench_driver_stats(&downloader) {
                        connections = stats.connections;
                    }
                }
                let pools = bench_driver_stats(&downloader).map_or(0.0, |stats| stats.pools as f64);
                let total_ms = millis(session_started.elapsed());
                drop(downloader);
                let released = wait_for_driver_threads(baseline, Duration::from_secs(10)).await;
                Round::new()
                    .metric("total", total_ms)
                    .metric("connections", connections as f64)
                    .metric("pools", pools)
                    .metric("residual_released", f64::from(released))
            }
        },
    )
    .await;
    scenarios.push(scenario);

    scenarios
}

// ──────────────────────────────────────────────────────────────
//  Group 4: the libcurl driver
// ──────────────────────────────────────────────────────────────

/// A driver round's operation time, so every group records `total` the same
/// way: how long the thing the scenario is about took.
fn timed(total_millis: f64) -> Round {
    Round::new().metric("total", total_millis)
}

fn driver_round(stats: Option<BenchDriverStats>) -> Round {
    match stats {
        Some(stats) => Round::new()
            .metric("loops", stats.loops as f64)
            .metric("commands", stats.commands as f64)
            .metric("submitted", stats.submitted as f64)
            .metric("completed", stats.completed as f64)
            .metric("cancelled", stats.cancelled as f64)
            .metric("connections", stats.connections as f64)
            .metric("pauses", stats.pauses as f64)
            .metric("resumes", stats.resumes as f64)
            .metric("pools", stats.pools as f64)
            .metric("active", stats.active as f64)
            .metric(
                "max_command_latency_ms",
                stats.max_command_latency_micros as f64 / 1000.0,
            ),
        None => Round::new().note("the client has no driver"),
    }
}

struct DriverVariant {
    name: &'static str,
    shape: FixtureShape,
    connections: u32,
    memory_budget: Option<usize>,
    unit: &'static str,
    extra_meta: Vec<(&'static str, String)>,
}

fn driver_variants() -> Vec<DriverVariant> {
    vec![
        DriverVariant {
            name: "driver/single_origin_split_download",
            shape: FixtureShape::new(SPLIT_BYTES).chunk_size(64 * 1024),
            connections: 4,
            memory_budget: None,
            unit: "one download",
            extra_meta: vec![("bytes", SPLIT_BYTES.to_string())],
        },
        DriverVariant {
            name: "driver/body_backpressure_256KiB_budget",
            shape: FixtureShape::new(SPLIT_BYTES).chunk_size(64 * 1024),
            connections: 4,
            memory_budget: Some(256 * 1024),
            unit: "one download",
            extra_meta: vec![
                ("bytes", SPLIT_BYTES.to_string()),
                ("memory_budget", (256 * 1024).to_string()),
            ],
        },
    ]
}

async fn run_driver(config: &Config, dir: &Path) -> Vec<Scenario> {
    const GROUP: &str = "driver";
    let mut scenarios = Vec::new();

    for variant in driver_variants() {
        let fixture = Fixture::spawn("driver", variant.shape);
        let expected = fixture.expected();
        let out_dir = dir.join(variant.name.replace('/', "_"));
        let _ = std::fs::create_dir_all(&out_dir);
        let connections = variant.connections;
        let memory_budget = variant.memory_budget;
        let scenario = collect(
            config,
            GROUP,
            variant.name,
            variant.unit,
            variant.extra_meta.clone(),
            move |round_index| {
                let fixture = fixture.clone();
                let expected = expected.clone();
                let out_dir = out_dir.clone();
                async move {
                    let Ok(downloader) = Downloader::builder().build() else {
                        return Round::new().note("client build failed");
                    };
                    let output = out_dir.join(format!("round-{round_index}.bin"));
                    let outcome = measure_download(
                        &downloader,
                        &fixture,
                        &output,
                        &expected,
                        move |spec| {
                            let mut spec = spec.max_connections(connections);
                            if let Some(budget) = memory_budget {
                                spec = spec.memory_budget(budget);
                            }
                            spec
                        },
                        true,
                    )
                    .await;
                    let stats = bench_driver_stats(&downloader);
                    let round = driver_round(stats).merge(download_round(&outcome, false));
                    let _ = std::fs::remove_file(&output);
                    drop(downloader);
                    round
                }
            },
        )
        .await;
        scenarios.push(scenario);
    }

    // Idle pools: no active descriptor must mean no busy loop. The window is
    // fixed, so the loop count inside it is comparable across runs.
    {
        let fixture = Fixture::spawn("driver_idle", FixtureShape::new(SPLIT_BYTES));
        let expected = fixture.expected();
        let out_dir = dir.join("driver_idle");
        let _ = std::fs::create_dir_all(&out_dir);
        let scenario = collect(
            config,
            GROUP,
            "driver/idle_pool_loop_rate",
            "one download plus a 300 ms idle window",
            vec![
                ("idle_window_ms", "300".into()),
                ("bytes", SPLIT_BYTES.to_string()),
            ],
            move |round_index| {
                let fixture = fixture.clone();
                let expected = expected.clone();
                let out_dir = out_dir.clone();
                async move {
                    let Ok(downloader) = Downloader::builder().build() else {
                        return Round::new().note("client build failed");
                    };
                    let output = out_dir.join(format!("round-{round_index}.bin"));
                    measure_download(
                        &downloader,
                        &fixture,
                        &output,
                        &expected,
                        |spec| spec.max_connections(4),
                        true,
                    )
                    .await;
                    let _ = std::fs::remove_file(&output);
                    let before = bench_driver_stats(&downloader).map_or(0, |stats| stats.loops);
                    let hold_started = Instant::now();
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    let held_millis = millis(hold_started.elapsed());
                    let stats = bench_driver_stats(&downloader);
                    let idle_loops = stats.map_or(0, |stats| stats.loops).saturating_sub(before);
                    let round = timed(held_millis)
                        .merge(driver_round(stats))
                        .metric("idle_loops", idle_loops as f64)
                        .metric(
                            "idle_loops_per_sec",
                            idle_loops as f64 * 1000.0 / held_millis,
                        );
                    drop(downloader);
                    round
                }
            },
        )
        .await;
        scenarios.push(scenario);
    }

    // Two origins: one busy pool and one idle pool must not cost loops.
    {
        let busy = Fixture::spawn("driver_busy", FixtureShape::new(SPLIT_BYTES));
        let idle = Fixture::spawn("driver_idle_origin", FixtureShape::new(SPLIT_BYTES));
        let busy_expected = busy.expected();
        let idle_expected = idle.expected();
        let out_dir = dir.join("driver_two_origins");
        let _ = std::fs::create_dir_all(&out_dir);
        let scenario = collect(
            config,
            GROUP,
            "driver/two_origins_one_idle_pool",
            "two downloads plus a 300 ms idle window",
            vec![("origins", "2".into()), ("idle_window_ms", "300".into())],
            move |round_index| {
                let busy = busy.clone();
                let idle = idle.clone();
                let busy_expected = busy_expected.clone();
                let idle_expected = idle_expected.clone();
                let out_dir = out_dir.clone();
                async move {
                    let Ok(downloader) = Downloader::builder().build() else {
                        return Round::new().note("client build failed");
                    };
                    // Warm both origins so both pools exist.
                    for (index, (fixture, expected)) in
                        [(&busy, &busy_expected), (&idle, &idle_expected)]
                            .into_iter()
                            .enumerate()
                    {
                        let output = out_dir.join(format!("round-{round_index}-warm-{index}.bin"));
                        measure_download(
                            &downloader,
                            fixture,
                            &output,
                            expected,
                            |spec| spec.max_connections(4),
                            true,
                        )
                        .await;
                        let _ = std::fs::remove_file(&output);
                    }
                    let before = bench_driver_stats(&downloader).map_or(0, |stats| stats.loops);
                    let hold_started = Instant::now();
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    let held_millis = millis(hold_started.elapsed());
                    let stats = bench_driver_stats(&downloader);
                    let idle_loops = stats.map_or(0, |stats| stats.loops).saturating_sub(before);
                    let round = timed(held_millis)
                        .merge(driver_round(stats))
                        .metric("idle_loops", idle_loops as f64)
                        .metric(
                            "idle_loops_per_sec",
                            idle_loops as f64 * 1000.0 / held_millis,
                        );
                    drop(downloader);
                    round
                }
            },
        )
        .await;
        scenarios.push(scenario);
    }

    // Cancel latency: how long a stop request takes to end a live transfer
    // whose body is stalled at the origin.
    {
        let fixture = Fixture::spawn("driver_cancel", FixtureShape::new(SPLIT_BYTES).gated());
        let out_dir = dir.join("driver_cancel");
        let _ = std::fs::create_dir_all(&out_dir);
        let scenario = collect(
            config,
            GROUP,
            "driver/cancel_latency",
            "one cancelled download",
            vec![("bytes", SPLIT_BYTES.to_string())],
            move |round_index| {
                let fixture = fixture.clone();
                let out_dir = out_dir.clone();
                async move {
                    let Ok(downloader) = Downloader::builder().build() else {
                        return Round::new().note("client build failed");
                    };
                    let output = out_dir.join(format!("round-{round_index}.bin"));
                    let spec = DownloadSpec::new(fixture.url.clone())
                        .output_path(&output)
                        .max_connections(4);
                    let before = fixture.counters();
                    let handle = downloader.download(spec);
                    if !wait_until(
                        || fixture.delta_since(before).requests > 0,
                        Duration::from_secs(10),
                    )
                    .await
                    {
                        return Round::new().note("the fixture never saw the request");
                    }
                    let cancel_started = Instant::now();
                    handle.cancel();
                    let result = handle.wait().await;
                    let cancel_ms = millis(cancel_started.elapsed());
                    // Let the stalled bodies finish, then close the gate again:
                    // the next round has to start stalled like this one did.
                    fixture.release();
                    fixture.arm_gate();
                    let _ = std::fs::remove_file(&output);
                    // The measured operation of this scenario is the cancel
                    // itself, so `total` is that latency.
                    let round = timed(cancel_ms)
                        .merge(driver_round(bench_driver_stats(&downloader)))
                        .metric("cancel_ms", cancel_ms)
                        .metric(
                            "cancelled",
                            f64::from(matches!(result, Err(bytehaul::DownloadError::Cancelled))),
                        )
                        .metric("file_bytes", 0.0);
                    drop(downloader);
                    round
                }
            },
        )
        .await;
        scenarios.push(scenario);
    }

    // A stalled active pool next to an idle pool: the shape the multi-pool wait
    // path is measured against. A driver that waits on the wrong pool of the
    // two returns immediately and spins, which shows up as a high `idle_loops`
    // count and a slow `cancel_ms` for the stalled transfer.
    {
        let stalled = Fixture::spawn("driver_stalled", FixtureShape::new(SPLIT_BYTES).gated());
        let idle = Fixture::spawn("driver_idle_peer", FixtureShape::new(SMALL_BYTES));
        let idle_expected = idle.expected();
        let out_dir = dir.join("driver_stalled_with_idle");
        let _ = std::fs::create_dir_all(&out_dir);
        let scenario = collect(
            config,
            GROUP,
            "driver/stalled_pool_with_idle_pool",
            "one stalled download plus a 300 ms hold",
            vec![
                ("hold_ms", "300".into()),
                ("pools", "2".into()),
                ("bytes", SPLIT_BYTES.to_string()),
            ],
            move |round_index| {
                let stalled = stalled.clone();
                let idle = idle.clone();
                let idle_expected = idle_expected.clone();
                let out_dir = out_dir.clone();
                async move {
                    let Ok(downloader) = Downloader::builder().build() else {
                        return Round::new().note("client build failed");
                    };
                    // Create the idle pool first: its origin is fully served and
                    // then left alone, so its connections sit in the driver.
                    let idle_output = out_dir.join(format!("round-{round_index}-idle.bin"));
                    measure_download(
                        &downloader,
                        &idle,
                        &idle_output,
                        &idle_expected,
                        |spec| spec.max_connections(1),
                        true,
                    )
                    .await;
                    let _ = std::fs::remove_file(&idle_output);

                    let output = out_dir.join(format!("round-{round_index}-stalled.bin"));
                    let spec = DownloadSpec::new(stalled.url.clone())
                        .output_path(&output)
                        .max_connections(4);
                    let before = stalled.counters();
                    let handle = downloader.download(spec);
                    if !wait_until(
                        || stalled.delta_since(before).requests > 0,
                        Duration::from_secs(10),
                    )
                    .await
                    {
                        return Round::new().note("the fixture never saw the request");
                    }

                    let loops_before =
                        bench_driver_stats(&downloader).map_or(0, |stats| stats.loops);
                    let hold_started = Instant::now();
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    let held_millis = millis(hold_started.elapsed());
                    let held_loops = bench_driver_stats(&downloader)
                        .map_or(0, |stats| stats.loops)
                        .saturating_sub(loops_before);

                    let cancel_started = Instant::now();
                    handle.cancel();
                    let result = handle.wait().await;
                    let cancel_ms = millis(cancel_started.elapsed());
                    // Let the stalled bodies finish, then close the gate again:
                    // the next round has to start stalled like this one did.
                    stalled.release();
                    stalled.arm_gate();
                    let _ = std::fs::remove_file(&output);
                    let round = timed(held_millis)
                        .merge(driver_round(bench_driver_stats(&downloader)))
                        .metric("held_loops", held_loops as f64)
                        .metric(
                            "held_loops_per_sec",
                            held_loops as f64 * 1000.0 / held_millis,
                        )
                        .metric("cancel_ms", cancel_ms)
                        .metric(
                            "cancelled",
                            f64::from(matches!(result, Err(bytehaul::DownloadError::Cancelled))),
                        );
                    drop(downloader);
                    round
                }
            },
        )
        .await;
        scenarios.push(scenario);
    }

    scenarios
}

// ──────────────────────────────────────────────────────────────
//  Group 5: end-to-end shapes
// ──────────────────────────────────────────────────────────────

struct EndToEndShape {
    name: &'static str,
    fixture: FixtureShape,
    expect_failure: bool,
    configure: fn(DownloadSpec) -> DownloadSpec,
    meta: Vec<(&'static str, String)>,
    verify_digest: bool,
}

fn end_to_end_shapes() -> Vec<EndToEndShape> {
    vec![
        EndToEndShape {
            name: "e2e/small_256KiB_single_connection",
            fixture: FixtureShape::new(SMALL_BYTES),
            expect_failure: false,
            configure: |spec| spec.max_connections(1),
            meta: vec![
                ("bytes", SMALL_BYTES.to_string()),
                ("connections", "1".into()),
            ],
            verify_digest: true,
        },
        EndToEndShape {
            name: "e2e/split_12MiB_4conns",
            fixture: FixtureShape::new(SPLIT_BYTES).chunk_size(64 * 1024),
            expect_failure: false,
            configure: |spec| spec.max_connections(4),
            meta: vec![
                ("bytes", SPLIT_BYTES.to_string()),
                ("connections", "4".into()),
            ],
            verify_digest: true,
        },
        EndToEndShape {
            name: "e2e/large_64MiB_4conns",
            fixture: FixtureShape::new(LARGE_BYTES).chunk_size(256 * 1024),
            expect_failure: false,
            configure: |spec| spec.max_connections(4),
            meta: vec![
                ("bytes", LARGE_BYTES.to_string()),
                ("connections", "4".into()),
            ],
            verify_digest: false,
        },
        EndToEndShape {
            name: "e2e/unknown_length_12MiB",
            fixture: FixtureShape::new(SPLIT_BYTES)
                .ranges(false)
                .chunk_size(64 * 1024),
            expect_failure: false,
            configure: |spec| spec.max_connections(4),
            meta: vec![
                ("bytes", SPLIT_BYTES.to_string()),
                ("content_length", "absent".into()),
            ],
            verify_digest: true,
        },
        EndToEndShape {
            name: "e2e/delayed_response_300ms",
            fixture: FixtureShape::new(SPLIT_BYTES)
                .chunk_size(64 * 1024)
                .head_delay(Duration::from_millis(300)),
            expect_failure: false,
            configure: |spec| spec.max_connections(4),
            meta: vec![
                ("bytes", SPLIT_BYTES.to_string()),
                ("head_delay_ms", "300".into()),
            ],
            verify_digest: true,
        },
        EndToEndShape {
            name: "e2e/cut_stream_12MiB",
            fixture: FixtureShape::new(SPLIT_BYTES)
                .chunk_size(64 * 1024)
                .cut(64 * 1024),
            expect_failure: true,
            configure: |spec| spec.max_connections(4).max_retries(1),
            meta: vec![
                ("bytes", SPLIT_BYTES.to_string()),
                ("retries", "1".into()),
                ("cut_after_bytes", (64 * 1024).to_string()),
            ],
            verify_digest: false,
        },
        EndToEndShape {
            name: "e2e/slow_tail_12MiB",
            fixture: FixtureShape::new(SPLIT_BYTES)
                .chunk_size(64 * 1024)
                .slow_tail(SPLIT_BYTES * 3 / 4, 256 * 1024, Duration::from_millis(20)),
            expect_failure: false,
            configure: |spec| spec.max_connections(4),
            meta: vec![
                ("bytes", SPLIT_BYTES.to_string()),
                ("tail", "last 25% at 256 KiB/20 ms".into()),
            ],
            verify_digest: true,
        },
    ]
}

async fn run_end_to_end(config: &Config, dir: &Path) -> Vec<Scenario> {
    const GROUP: &str = "e2e";
    let mut scenarios = Vec::new();

    for shape in end_to_end_shapes() {
        let fixture = Fixture::spawn("e2e", shape.fixture);
        let expected = fixture.expected();
        let out_dir = dir.join(shape.name.replace('/', "_"));
        let _ = std::fs::create_dir_all(&out_dir);
        let configure = shape.configure;
        let expect_failure = shape.expect_failure;
        let verify_digest = shape.verify_digest;
        let meta = shape.meta.clone();

        let scenario = collect(
            config,
            GROUP,
            shape.name,
            "one download",
            meta,
            move |round_index| {
                let fixture = fixture.clone();
                let expected = expected.clone();
                let out_dir = out_dir.clone();
                async move {
                    let Ok(downloader) = Downloader::builder().build() else {
                        return Round::new().note("client build failed");
                    };
                    let output = out_dir.join(format!("round-{round_index}.bin"));
                    let digest = if verify_digest {
                        use sha2::{Digest, Sha256};
                        Some(
                            Sha256::digest(expected.iter().as_slice())
                                .iter()
                                .map(|byte| format!("{byte:02x}"))
                                .collect::<String>(),
                        )
                    } else {
                        None
                    };
                    let (rss_before, _) = memory_bytes();
                    let outcome = measure_download(
                        &downloader,
                        &fixture,
                        &output,
                        &expected,
                        move |spec| {
                            let spec = configure(spec);
                            match &digest {
                                Some(digest) => spec.checksum(Checksum::Sha256(digest.clone())),
                                None => spec,
                            }
                        },
                        true,
                    )
                    .await;
                    let (rss_after, rss_peak) = memory_bytes();
                    let mut round = download_round(&outcome, expect_failure);
                    if let (Some(before), Some(after)) = (rss_before, rss_after) {
                        round = round.metric("rss_delta_bytes", after as f64 - before as f64);
                    }
                    round = round.optional("process_peak_rss_bytes", rss_peak.map(|v| v as f64));
                    let _ = std::fs::remove_file(&output);
                    drop(downloader);
                    round
                }
            },
        )
        .await;
        scenarios.push(scenario);
    }

    scenarios
}

// ──────────────────────────────────────────────────────────────
//  Report
// ──────────────────────────────────────────────────────────────

/// The metrics each group's summary table shows, in order. A name that is also
/// a scenario fact (`bytes`, `connections`) is printed from the scenario's
/// metadata instead of from the samples.
fn summary_columns(group: &str) -> &'static [&'static str] {
    match group {
        "scheduler" => &[
            "pieces",
            "total",
            "cpu_ms",
            "requests",
            "micros_per_request",
            "boundary_scans",
            "planned_ranges",
            "multi_lease_requests",
            "max_leases_per_request",
        ],
        "writer" => &[
            "bytes",
            "total",
            "cpu_ms",
            "writer_blocks",
            "avg_block_bytes",
            "cache_copied_bytes",
            "fsync_calls",
            "fsync_ms",
            "prealloc_ms",
        ],
        "client" => &[
            "total",
            "cpu_ms",
            "cache_entries",
            "driver_threads",
            "driver_threads_after_drop",
            "residual_released",
            "connections",
            "queue_waits",
            "queue_ms",
            "verified",
        ],
        "driver" => &[
            "bytes",
            "total",
            "cpu_ms",
            "cpu_percent",
            "loops",
            "idle_loops",
            "idle_loops_per_sec",
            "held_loops",
            "held_loops_per_sec",
            "submitted",
            "connections",
            "pauses",
            "resumes",
            "cancel_ms",
            "max_command_latency_ms",
        ],
        "e2e" => &[
            "bytes",
            "total",
            "cpu_ms",
            "to_first_report",
            "report_span",
            "finalize",
            "response_heads",
            "response_head_ms",
            "body_reads",
            "body_wait_ms",
            "requests",
            "served_bytes",
            "duplicate_bytes",
            "checksum_ms",
            "rss_delta_bytes",
            "verified",
            "failed",
        ],
        _ => &[],
    }
}

/// Runs a command and returns its first non-empty output line.
fn command_line(program: &str, args: &[&str]) -> Option<String> {
    let output = std::process::Command::new(program)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let line = text.lines().find(|line| !line.trim().is_empty())?;
    Some(line.trim().to_string())
}

/// The facts a report has to carry for its numbers to mean anything: the
/// revision and toolchain the run was built from.
fn environment_facts() -> Vec<(&'static str, String)> {
    let commit = command_line("git", &["rev-parse", "HEAD"]).unwrap_or_else(|| "unknown".into());
    let dirty = std::process::Command::new("git")
        .args(["status", "--porcelain"])
        .output()
        .ok()
        .map(|output| !output.stdout.is_empty())
        .unwrap_or(false);
    let toolchain = command_line("rustc", &["-vV"])
        .and_then(|line| line.strip_prefix("release: ").map(str::to_string))
        .or_else(|| command_line("rustc", &["-V"]))
        .unwrap_or_else(|| "unknown".into());
    let host = command_line("rustc", &["-vV"])
        .and_then(|_| {
            std::process::Command::new("rustc")
                .args(["-vV"])
                .output()
                .ok()
                .map(|output| String::from_utf8_lossy(&output.stdout).to_string())
        })
        .and_then(|text| {
            text.lines()
                .find_map(|line| line.strip_prefix("host: ").map(str::to_string))
        })
        .unwrap_or_else(|| format!("{} {}", std::env::consts::OS, std::env::consts::ARCH));

    vec![
        ("commit", commit),
        (
            "worktree",
            if dirty { "dirty" } else { "clean" }.to_string(),
        ),
        ("rustc", toolchain),
        ("target", host),
    ]
}

fn render_report(
    config: &Config,
    features: Option<&str>,
    scenarios: &[Scenario],
    csv_path: &Path,
) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "# Pipeline baseline (P1)");
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "Generated by `benches/pipeline_bench.rs`. Raw per-round samples: `{}`.",
        csv_path.display()
    );
    let _ = writeln!(out);
    let _ = writeln!(out, "## Environment");
    let _ = writeln!(out);
    let _ = writeln!(out, "| fact | value |");
    let _ = writeln!(out, "| --- | --- |");
    let _ = writeln!(out, "| crate | bytehaul {} |", env!("CARGO_PKG_VERSION"));
    for (name, value) in environment_facts() {
        let _ = writeln!(out, "| {name} | {value} |");
    }
    let _ = writeln!(
        out,
        "| platform | {} {} |",
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    let _ = writeln!(out, "| profile | bench (release-like) |");
    let _ = writeln!(out, "| rounds per scenario | {} |", config.rounds);
    let _ = writeln!(
        out,
        "| default config | 1 MiB pieces, `min_split_size` 10 MiB, `min_segment_size` 256 KiB |"
    );
    let _ = writeln!(
        out,
        "| libcurl / TLS | {} |",
        features.unwrap_or("not captured").trim()
    );
    if let Some(archive) = &config.archive {
        let _ = writeln!(
            out,
            "| archived copy | {}/report.md, {}/samples.csv |",
            archive.display(),
            archive.display()
        );
    }
    let _ = writeln!(out);
    let _ = writeln!(out, "## Timing boundaries");
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "* `millis` (per-round sample) — the whole round body, including the harness's own output verification."
    );
    let _ = writeln!(
        out,
        "* `total` — the measured operation: the download for a download round (until its task published a terminal state and `wait()` returned), the planning run for a scheduler round."
    );
    let _ = writeln!(
        out,
        "* `to_first_report` — submit until the first progress sample that reported bytes: queueing, output-file creation (and preallocation), the response head, and body bytes up to the first report."
    );
    let _ = writeln!(
        out,
        "* `report_span` — that first report until the last report of a running transfer."
    );
    let _ = writeln!(
        out,
        "* `finalize` — that last running report until the round ended: writer flush and sync, cleanup, and verification."
    );
    let _ = writeln!(
        out,
        "* `running_reports` — how many running progress samples the round observed. These boundaries come from the progress channel: the multi-connection monitor publishes on a 200 ms ticker (plus forced reports at phase boundaries), so a transfer that finishes inside one tick gives `running_reports = 1`, `report_span = 0`, and its whole body inside `to_first_report`. Only `total`, the counters and `prealloc_ms`/`fsync_ms`/`checksum_ms`/`response_head_ms`/`body_wait_ms` resolve finer than that."
    );
    let _ = writeln!(
        out,
        "* `response_head_ms`, `body_wait_ms` — recorded inside the library, summed over every HTTP request and body read of the download (redirects, probes and retried attempts each count), so a 4-connection download sums four waits. `response_head_ms` is request→response-head; `body_wait_ms` is the time `next_chunk` waited for bytes, which excludes the caller's writing. `response_heads` and `body_reads` are the matching counts. A delayed-response shape must show its delay here."
    );
    let _ = writeln!(
        out,
        "* `prealloc_ms`, `fsync_ms`, `checksum_ms` — also recorded inside the library, so they split `to_first_report` and `finalize` further. Together with `queue_ms` they are the six separately recorded phases: queueing, preallocation, response head, body, final sync and verification."
    );
    let _ = writeln!(
        out,
        "* `cpu_ms`, `cpu_percent` — process CPU time (user + kernel) over the same window as `millis`, so the fixture, the downloader's threads and the harness's own verification are all included and `cpu_percent` above 100 means several threads were busy. It is an upper bound on the download's own CPU, and comparable between rounds of the same scenario."
    );
    let _ = writeln!(
        out,
        "* Counters (`writer_blocks`, `cache_copied_bytes`, `preallocs`, `queue_waits`, …) are recorded by the production path only while the harness enables collection."
    );
    let _ = writeln!(
        out,
        "* `duplicate_bytes` = bytes the fixture sent for file positions this same round had already sent, measured by overlapping the ranges each response actually delivered. `unique_bytes` is the rest of what it served. A retried or re-requested range shows up here even when the total served stays below the file size."
    );
    let _ = writeln!(
        out,
        "* Summary cells are `median (p25-p75)` over {}. Ten samples do not support a stable P95, and the per-round appendix and CSV below are the primary record.",
        config.rounds
    );
    let _ = writeln!(out);

    for group in ["scheduler", "writer", "client", "driver", "e2e"] {
        let selected: Vec<&Scenario> = scenarios
            .iter()
            .filter(|scenario| scenario.group == group && !scenario.samples.is_empty())
            .collect();
        if selected.is_empty() {
            continue;
        }
        let _ = writeln!(out, "## {group}");
        let _ = writeln!(out);
        let columns = summary_columns(group);
        let _ = write!(out, "| scenario | measures | n | median (p25-p75) ms |");
        for column in columns {
            let _ = write!(out, " {column} |");
        }
        let _ = writeln!(out, " notes |");
        let _ = write!(out, "| --- | --- | --- | --- |");
        for _ in columns {
            let _ = write!(out, " --- |");
        }
        let _ = writeln!(out, " --- |");
        for scenario in &selected {
            let meta: BTreeMap<&str, String> = scenario.meta.iter().cloned().collect();
            let _ = write!(
                out,
                "| {} | {} | {} | {} |",
                scenario.name,
                scenario.unit,
                scenario.samples.len(),
                scenario.spread("millis").unwrap_or_else(|| "-".into())
            );
            for column in columns {
                // A scenario fact (`bytes`, `connections`) describes the shape,
                // so it is printed from the metadata; everything else is the
                // median of that metric.
                let value = meta
                    .get(column)
                    .cloned()
                    .or_else(|| scenario.summary_cell(column));
                let _ = write!(out, " {} |", value.unwrap_or_else(|| "-".into()));
            }
            let notes = scenario.notes();
            let note = if notes == 0 {
                "-".to_string()
            } else {
                format!("{notes}/{} rounds noted", scenario.samples.len())
            };
            let _ = writeln!(out, " {note} |");
        }
        let _ = writeln!(out);
        for scenario in &selected {
            if let Some(sample) = scenario.samples.iter().find(|sample| sample.note.is_some()) {
                let _ = writeln!(
                    out,
                    "* `{}` round {}: {}",
                    scenario.name,
                    sample.round,
                    sample.note.clone().unwrap_or_default()
                );
            }
        }
        let _ = writeln!(out);
    }

    // Per-round appendix: the primary metric of every scenario, round by round,
    // so a report reader can judge spread without the full CSV.
    let _ = writeln!(out, "## Per-round `total` (ms)");
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "| scenario | {} |",
        (0..config.rounds)
            .map(|round| format!("r{round}"))
            .collect::<Vec<_>>()
            .join(" | ")
    );
    let _ = writeln!(out, "| --- | {} |", vec!["---"; config.rounds].join(" | "));
    for scenario in scenarios.iter().filter(|s| !s.samples.is_empty()) {
        let values: Vec<String> = scenario
            .samples
            .iter()
            .map(|sample| {
                sample
                    .metrics
                    .get("total")
                    .map(|value| format!("{value:.2}"))
                    .unwrap_or_else(|| "-".into())
            })
            .collect();
        let _ = writeln!(out, "| {} | {} |", scenario.name, values.join(" | "));
    }
    let _ = writeln!(out);

    out
}

fn render_csv(scenarios: &[Scenario]) -> String {
    let mut out = String::from("group,scenario,round,round_millis,metric,value\n");
    for scenario in scenarios {
        for sample in &scenario.samples {
            for (metric, value) in &sample.metrics {
                let _ = writeln!(
                    out,
                    "{},{},{},{:.4},{},{}",
                    scenario.group, scenario.name, sample.round, sample.millis, metric, value
                );
            }
        }
    }
    out
}

// ──────────────────────────────────────────────────────────────
//  Entry point
// ──────────────────────────────────────────────────────────────

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() {
    let config = Config::from_args();
    if config.list {
        for name in scenario_names() {
            println!("{name}");
        }
        return;
    }
    // `scenario_names` is only used by `--list`, so nothing else would notice
    // it drifting from the scenarios that actually run.
    std::fs::create_dir_all(&config.out_dir).expect("the output directory must be creatable");
    let work_dir = config.out_dir.join("tmp");
    std::fs::create_dir_all(&work_dir).expect("the work directory must be creatable");

    println!(
        "pipeline_bench: {} rounds per scenario, {} scenarios listed, output in {}",
        config.rounds,
        scenario_names().len(),
        config.out_dir.display()
    );

    let baseline_threads = bench_driver_threads();
    let capture = LogCapture::install();
    let features = capture.probe(&work_dir).await;
    println!(
        "libcurl features: {}",
        features.as_deref().unwrap_or("not captured")
    );

    // Collection is on for the measured scenarios: the counters are the only
    // way to see the writer's and the cache's real work. `tests/pipeline_counters.rs`
    // is what proves the disabled path records nothing.
    bench_counters_reset();
    bench_counters_set_enabled(true);

    let mut scenarios = Vec::new();
    scenarios.extend(run_scheduler(&config).await);
    scenarios.extend(run_writer(&config, &work_dir).await);
    scenarios.extend(run_client(&config, &work_dir).await);
    scenarios.extend(run_driver(&config, &work_dir).await);
    scenarios.extend(run_end_to_end(&config, &work_dir).await);

    bench_counters_set_enabled(false);
    bench_counters_reset();
    if wait_for_driver_threads(baseline_threads, Duration::from_secs(20)).await {
        println!("every driver thread stopped after the run");
    } else {
        eprintln!(
            "warning: {} driver threads outlived the run",
            bench_driver_threads() - baseline_threads
        );
    }

    // `scenario_names` is only used by `--list`; nothing else would notice it
    // drifting from the scenarios that actually run. Every name it lists must
    // either be filtered out or have produced samples.
    {
        let listed = scenario_names();
        let mut unique = listed.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(
            listed.len(),
            unique.len(),
            "--list must not name the same scenario twice"
        );
        if config.filter.is_none() {
            let ran: std::collections::HashSet<&str> = scenarios
                .iter()
                .filter(|scenario| !scenario.samples.is_empty())
                .map(|scenario| scenario.name.as_str())
                .collect();
            let missing: Vec<&String> = listed
                .iter()
                .filter(|name| !ran.contains(name.as_str()))
                .collect();
            assert!(
                missing.is_empty(),
                "--list names scenarios the run does not produce: {missing:?}"
            );
        }
    }

    let csv_path = config.out_dir.join("samples.csv");
    std::fs::write(&csv_path, render_csv(&scenarios)).expect("samples.csv must be writable");
    let report = render_report(&config, features.as_deref(), &scenarios, &csv_path);
    let report_path = config.out_dir.join("report.md");
    std::fs::write(&report_path, &report).expect("report.md must be writable");

    // `target` is ignored, so a run that only ever writes there cannot be cited
    // by anything durable. `--archive` puts the same two files where a document
    // can point at them.
    if let Some(archive) = &config.archive {
        std::fs::create_dir_all(archive).expect("the archive directory must be creatable");
        let archived_report = archive.join("report.md");
        let archived_csv = archive.join("samples.csv");
        std::fs::copy(&report_path, &archived_report).expect("report.md must be archivable");
        std::fs::copy(&csv_path, &archived_csv).expect("samples.csv must be archivable");
        println!("archived: {}", archived_report.display());
        println!("archived: {}", archived_csv.display());
    }

    println!();
    println!("{report}");
    println!("report: {}", report_path.display());
    println!("samples: {}", csv_path.display());
}

/// Every scenario name, for `--list`. Kept in step with the builders above by
/// the `scenario_names_cover_every_scenario` harness self-check at the bottom of
/// this file.
fn scenario_names() -> Vec<String> {
    let mut names = Vec::new();
    for pieces in [64usize, 512, 4096] {
        for pattern in ["fresh", "holes", "resumed_suffix"] {
            for mode in ["dynamic", "fixed"] {
                names.push(format!("scheduler/{mode}/{pattern}/{pieces}"));
            }
        }
    }
    names.extend(
        writer_variants()
            .into_iter()
            .map(|variant| variant.name.to_string()),
    );
    names.extend(
        driver_variants()
            .into_iter()
            .map(|variant| variant.name.to_string()),
    );
    names.extend(
        [
            "client/distinct_connect_timeouts/2",
            "client/distinct_connect_timeouts/8",
            "client/distinct_connect_timeouts/32",
            "client/reuse_same_config",
            "client/queue_with_one_permit",
            "client/connection_reuse_two_downloads",
            "driver/idle_pool_loop_rate",
            "driver/two_origins_one_idle_pool",
            "driver/stalled_pool_with_idle_pool",
            "driver/cancel_latency",
        ]
        .map(String::from),
    );
    names.extend(
        end_to_end_shapes()
            .into_iter()
            .map(|shape| shape.name.to_string()),
    );
    names
}
