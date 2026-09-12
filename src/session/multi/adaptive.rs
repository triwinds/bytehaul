//! Adaptive attempts share the existing scheduler and authoritative writer. An
//! attempt owns its primary future and optional challenger, so dropping a loser
//! stops its producer before the FIFO discard barrier or lease renewal.
use super::super::flow::wait_for_stop;
use super::*;
use crate::config::SlowTransferMode;
use crate::storage::segment::LeaseKey;
use parking_lot::Mutex;
use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore};

const COOLDOWN: Duration = Duration::from_secs(10);
const MAX_EXTRA: u64 = 16 * 1024 * 1024;
const MAX_HEDGE: u64 = 1024 * 1024;
const TAIL_WINDOW: Duration = Duration::from_secs(1);
const TAIL_GRACE: Duration = Duration::from_secs(1);
const TAIL_DURATION: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Headers,
    Reading,
    RateLimited,
    MemoryBlocked,
    ChannelBlocked,
    WriterBarrier,
}

struct Observation {
    phase: Phase,
    phase_at: Instant,
    blocked_since: Option<Instant>,
    reading: Duration,
    headers: Duration,
    backpressure: Duration,
    writer_barrier: Duration,
    request_id: Option<u64>,
    last_reason: Option<(&'static str, &'static str)>,
    wire: u64,
    enqueued: u64,
    forwarded: u64,
    samples: VecDeque<(Duration, u64)>,
    slow_since: Option<Duration>,
    rate: Option<f64>,
    window: Duration,
    tail_samples: VecDeque<(Duration, u64)>,
    tail_slow_since: Option<Duration>,
}
impl Observation {
    fn new(now: Instant, window: Duration) -> Self {
        Self {
            phase: Phase::Headers,
            phase_at: now,
            blocked_since: Some(now),
            reading: Duration::ZERO,
            headers: Duration::ZERO,
            backpressure: Duration::ZERO,
            writer_barrier: Duration::ZERO,
            request_id: None,
            last_reason: None,
            wire: 0,
            enqueued: 0,
            forwarded: 0,
            samples: VecDeque::from([(Duration::ZERO, 0)]),
            slow_since: None,
            rate: None,
            window,
            tail_samples: VecDeque::from([(Duration::ZERO, 0)]),
            tail_slow_since: None,
        }
    }
    fn advance(&mut self, now: Instant) {
        let elapsed = now.saturating_duration_since(self.phase_at);
        match self.phase {
            Phase::Headers => self.headers += elapsed,
            Phase::WriterBarrier => self.writer_barrier += elapsed,
            Phase::RateLimited | Phase::MemoryBlocked | Phase::ChannelBlocked => {
                self.backpressure += elapsed
            }
            Phase::Reading => {}
        }
        if self.phase == Phase::Reading {
            self.reading += elapsed;
        } else if self
            .blocked_since
            .is_some_and(|at| now.saturating_duration_since(at) >= self.window)
        {
            self.samples.clear();
            self.samples.push_back((self.reading, self.wire));
            self.slow_since = None;
            self.rate = None;
        }
        if self.phase != Phase::Reading
            && self
                .blocked_since
                .is_some_and(|at| now.saturating_duration_since(at) >= self.window.min(TAIL_WINDOW))
        {
            self.tail_samples.clear();
            self.tail_samples.push_back((self.reading, self.wire));
            self.tail_slow_since = None;
        }
        self.phase_at = now;
    }
    fn phase(&mut self, phase: Phase) {
        let now = Instant::now();
        self.advance(now);
        if phase == Phase::Reading {
            self.blocked_since = None;
        } else if self.phase == Phase::Reading {
            self.blocked_since = Some(now);
        }
        self.phase = phase;
    }
    fn sample(&mut self, now: Instant) -> Option<f64> {
        self.advance(now);
        if self.phase != Phase::Reading {
            return None;
        }
        self.samples.push_back((self.reading, self.wire));
        while self.samples.len() > 2
            && self.reading.saturating_sub(self.samples[1].0) >= self.window
        {
            self.samples.pop_front();
        }
        let (at, bytes) = *self.samples.front()?;
        let span = self.reading.saturating_sub(at);
        self.rate = (span >= self.window).then(|| (self.wire - bytes) as f64 / span.as_secs_f64());
        self.rate
    }
    fn tail_sample(&mut self, now: Instant) -> Option<f64> {
        self.advance(now);
        if self.phase != Phase::Reading {
            return None;
        }
        let window = self.window.min(TAIL_WINDOW);
        self.tail_samples.push_back((self.reading, self.wire));
        while self.tail_samples.len() > 2
            && self.reading.saturating_sub(self.tail_samples[1].0) >= window
        {
            self.tail_samples.pop_front();
        }
        let (at, bytes) = *self.tail_samples.front()?;
        let span = self.reading.saturating_sub(at);
        (span >= window).then(|| (self.wire - bytes) as f64 / span.as_secs_f64())
    }
    fn should_recover_tail(
        &mut self,
        now: Instant,
        baseline: Option<f64>,
        spec: &Policy,
        len: u64,
    ) -> bool {
        let rate = self.tail_sample(now);
        let Some((rate, healthy)) = rate.zip(baseline.filter(|rate| *rate > 0.)) else {
            self.tail_slow_since = None;
            return false;
        };
        if self.reading < spec.grace.min(TAIL_GRACE) || rate >= healthy * 0.25 {
            self.tail_slow_since = None;
            return false;
        }
        let since = *self.tail_slow_since.get_or_insert(self.reading);
        self.reading.saturating_sub(since) >= spec.duration.min(TAIL_DURATION)
            && recovery_benefits(rate, healthy, len, self.forwarded, spec.window)
    }
    fn should_recover(
        &mut self,
        now: Instant,
        baseline: Option<f64>,
        spec: &Policy,
        len: u64,
    ) -> bool {
        let Some(rate) = self.sample(now) else {
            return false;
        };
        if self.reading < spec.grace {
            self.slow_since = None;
            return false;
        }
        let low = spec.absolute.is_some_and(|limit| rate < limit as f64)
            || baseline.is_some_and(|reference| rate < reference * 0.25);
        if !low {
            self.slow_since = None;
            return false;
        }
        let since = *self.slow_since.get_or_insert(self.reading);
        if self.reading.saturating_sub(since) < spec.duration {
            return false;
        }
        // Explicit absolute thresholds can recover without historical peers,
        // but use a finite conservative rate floor for the benefit estimate.
        let healthy = baseline.or(spec.absolute.map(|v| v as f64));
        let Some(healthy) = healthy.filter(|v| *v > 0.) else {
            return false;
        };
        recovery_benefits(rate, healthy, len, self.forwarded, spec.window)
    }
}

/// All application-observed bytes not yet enqueued belong to read-ahead,
/// including the current frame while local forwarding is blocked. Hyper/socket
/// buffers never handed to this task are outside this body-layer accounting.
fn cancellation_cost_bound(sample: &Observation, len: u64, protected: bool) -> u64 {
    let read_ahead = sample.wire.saturating_sub(sample.enqueued);
    if protected {
        read_ahead
    } else {
        len.saturating_add(read_ahead)
    }
}

fn recovery_benefits(rate: f64, healthy: f64, len: u64, forwarded: u64, window: Duration) -> bool {
    let remaining = len.saturating_sub(forwarded);
    let keep = remaining as f64 / rate.max(1.);
    let replace = len as f64 / healthy + window.as_secs_f64().min(1.);
    remaining > 0 && keep > replace * 2. && keep - replace > window.as_secs_f64().min(2.)
}

struct Policy {
    mode: SlowTransferMode,
    absolute: Option<u64>,
    duration: Duration,
    grace: Duration,
    window: Duration,
}
struct Lineage {
    retries: RetryState,
    recoveries: u8,
}
type SharedLineage = Arc<Mutex<Lineage>>;
type PendingRange = (usize, u64, u64, usize);
struct Recovered {
    piece: usize,
    start: u64,
    end: u64,
    remaining: u64,
    lineage: SharedLineage,
}
struct State {
    active: BTreeMap<LeaseKey, Arc<Mutex<Observation>>>,
    history: VecDeque<(Instant, f64)>,
    recovered: Vec<Recovered>,
    pending: VecDeque<PendingRange>,
    reserved: u64,
    consumed_extra: u64,
    decisions: BTreeMap<&'static str, u64>,
    retries: u64,
    actions: u64,
    hedge: bool,
    last_action: Option<Instant>,
    blocked_until: Option<Instant>,
}
/// Owns exactly one in-flight cost bound. Dropping an interrupted action settles
/// its observed cost once, including producer cancellation and writer failure.
struct Reservation {
    state: Arc<Mutex<State>>,
    bound: u64,
    cost: u64,
}
impl Drop for Reservation {
    fn drop(&mut self) {
        let mut state = self.state.lock();
        state.reserved -= self.bound;
        state.consumed_extra = state.consumed_extra.saturating_add(self.cost);
    }
}

impl State {
    fn pending_backoff(&self, now: Instant) -> Option<Duration> {
        if self.pending.is_empty() {
            return None;
        }
        self.blocked_until
            .and_then(|until| until.checked_duration_since(now))
            .filter(|delay| !delay.is_zero())
    }
}
pub(super) struct Coordinator {
    policy: Policy,
    validator: Option<String>,
    output_dir: PathBuf,
    slots: Arc<Semaphore>,
    slot_capacity: usize,
    changed: Notify,
    state: Arc<Mutex<State>>,
    extra_limit: u64,
    history_limit: usize,
    // Wire counters are diagnostic and never feed effective progress.
    wire: AtomicU64,
    duplicate: AtomicU64,
}
impl Coordinator {
    pub(super) fn new(
        spec: &DownloadSpec,
        meta: &ResponseMeta,
        output: &Path,
        total: u64,
    ) -> Option<Arc<Self>> {
        if spec.slow_transfer_mode == SlowTransferMode::Disabled && spec.request_batch_size == 0 {
            return None;
        }
        let validator = usable_validator(spec, meta);
        Some(Arc::new(Self {
            policy: Policy {
                mode: spec.slow_transfer_mode,
                absolute: spec.low_speed_limit,
                duration: spec.low_speed_duration,
                grace: spec.slow_start_grace,
                window: spec.slow_sample_window,
            },
            validator,
            output_dir: output
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(Path::new("."))
                .to_path_buf(),
            slots: Arc::new(Semaphore::new(spec.max_connections as usize)),
            slot_capacity: spec.max_connections as usize,
            changed: Notify::new(),
            state: Arc::new(Mutex::new(State {
                active: BTreeMap::new(),
                history: VecDeque::new(),
                recovered: Vec::new(),
                pending: VecDeque::new(),
                reserved: 0,
                consumed_extra: 0,
                decisions: BTreeMap::new(),
                retries: 0,
                actions: 0,
                hedge: false,
                last_action: None,
                blocked_until: None,
            })),
            extra_limit: (total / 100).min(MAX_EXTRA),
            history_limit: (spec.max_connections as usize).saturating_mul(2).max(4),
            wire: AtomicU64::new(0),
            duplicate: AtomicU64::new(0),
        }))
    }
    pub(super) fn report(&self, log_level: LogLevel, download_id: u64) {
        let state = self.state.lock();
        log_info!(
            log_level,
            download_id,
            wire_bytes = self.wire.load(Ordering::Relaxed),
            duplicate_bytes = self.duplicate.load(Ordering::Relaxed),
            reserved_extra_bytes = state.reserved,
            consumed_extra_bytes = state.consumed_extra,
            reservation_semantics = "in_flight_application_body_cost",
            body_counter_scope = "adaptive_primary_and_challenger_data_frames",
            discarded_or_staged_body_bytes = self.duplicate.load(Ordering::Relaxed),
            ordinary_retries = state.retries,
            recovery_decision_ticks = ?state.decisions,
            extra_budget_bytes = self.extra_limit,
            recovery_actions = state.actions,
            "adaptive transfer diagnostics"
        );
    }
    fn tick(&self) -> Duration {
        (self.policy.window / 5)
            .min(TAIL_WINDOW / 5)
            .max(Duration::from_nanos(1))
    }
    fn baseline(&self, target: LeaseKey, now: Instant) -> Option<f64> {
        self.sample_baseline(target, now, false)
    }
    fn tail_eligible(&self, len: u64, available: bool) -> bool {
        len <= MAX_HEDGE
            && !available
            && self.slots.available_permits() > 0
            && self.state.lock().pending.is_empty()
    }
    fn sample_baseline(&self, target: LeaseKey, now: Instant, tail: bool) -> Option<f64> {
        let mut state = self.state.lock();
        // A slot also covers writer setup, discard and retry backoff, where
        // no observation is registered. Those requests provide no healthy
        // network evidence and must suppress the accelerated path.
        if tail
            && (!state.active.contains_key(&target)
                || self.slot_capacity - self.slots.available_permits() != state.active.len())
        {
            return None;
        }
        let ttl = self
            .policy
            .window
            .saturating_mul(12)
            .max(Duration::from_secs(30));
        state
            .history
            .retain(|(at, _)| now.saturating_duration_since(*at) <= ttl);
        let mut current = Vec::new();
        for (&key, sample) in &state.active {
            if key != target {
                let mut sample = sample.lock();
                let rate = if tail {
                    sample.tail_sample(now)
                } else {
                    sample.sample(now)
                };
                if let Some(rate) = rate {
                    current.push(rate);
                } else if tail {
                    // Missing live evidence cannot endorse a historical peak.
                    return None;
                }
            }
        }
        let mut rates: Vec<_> = state
            .history
            .iter()
            .map(|(_, rate)| *rate)
            .chain(current.iter().copied())
            .filter(|r| *r > 0.)
            .collect();
        if rates.len() < 2 {
            *state
                .decisions
                .entry("baseline_insufficient_samples")
                .or_default() += 1;
            return None;
        }
        rates.sort_by(f64::total_cmp);
        let baseline = rates[rates.len() / 2];
        // Several live requests slowing together supersede historical peaks.
        if !current.is_empty() && current.iter().all(|rate| *rate < baseline * 0.5) {
            *state.decisions.entry("baseline_all_live_slow").or_default() += 1;
            return None;
        }
        Some(baseline)
    }
    #[cfg(test)]
    fn reserve(&self, len: u64, lineage: &SharedLineage, hedge: bool, now: Instant) -> bool {
        self.reserve_result(len, lineage, hedge, now).is_ok()
    }
    fn reserve_result(
        &self,
        len: u64,
        lineage: &SharedLineage,
        hedge: bool,
        now: Instant,
    ) -> Result<Reservation, &'static str> {
        let mut state = self.state.lock();
        let reason = if state.hedge {
            Some("challenger_active")
        } else if state.blocked_until.is_some_and(|until| now < until) {
            Some("global_backoff")
        } else if state
            .last_action
            .is_some_and(|at| now.saturating_duration_since(at) < COOLDOWN)
        {
            Some("cooldown")
        } else if len
            > self
                .extra_limit
                .saturating_sub(state.reserved.saturating_add(state.consumed_extra))
        {
            Some("budget")
        } else {
            None
        };
        if let Some(reason) = reason {
            return Err(reason);
        }
        let mut lineage = lineage.lock();
        if lineage.recoveries >= 2 {
            return Err("lineage_limit");
        }
        lineage.recoveries += 1;
        state.reserved += len;
        state.actions += 1;
        state.hedge = hedge;
        state.last_action = Some(now);
        Ok(Reservation {
            state: self.state.clone(),
            bound: len,
            cost: len,
        })
    }
    fn finish_observation(&self, key: LeaseKey, completed: bool) {
        let mut state = self.state.lock();
        if let Some(observation) = state.active.remove(&key) {
            let mut observation = observation.lock();
            observation.advance(Instant::now());
            if completed && !observation.reading.is_zero() && observation.wire > 0 {
                state.history.push_back((
                    Instant::now(),
                    observation.wire as f64 / observation.reading.as_secs_f64(),
                ));
                while state.history.len() > self.history_limit {
                    state.history.pop_front();
                }
            }
        }
    }
    fn lineage(&self, segment: &Segment, cfg: &WorkerConfig) -> SharedLineage {
        let state = self.state.lock();
        if let Some(record) = state.recovered.iter().find(|r| {
            r.piece == segment.piece_id && r.start <= segment.start && r.end >= segment.end
        }) {
            return record.lineage.clone();
        }
        Arc::new(Mutex::new(Lineage {
            retries: RetryState::new(
                cfg.max_retries,
                cfg.retry_base_delay,
                cfg.retry_max_delay,
                cfg.max_retry_elapsed,
            ),
            recoveries: 0,
        }))
    }
    fn recovered(&self, segment: &Segment, lineage: &SharedLineage, max_active_leases: usize) {
        let mut state = self.state.lock();
        state.pending.push_back((
            segment.piece_id,
            segment.start,
            segment.end,
            max_active_leases.max(1),
        ));
        if !state
            .recovered
            .iter()
            .any(|r| r.piece == segment.piece_id && Arc::ptr_eq(&r.lineage, lineage))
        {
            state.recovered.push(Recovered {
                piece: segment.piece_id,
                start: segment.start,
                end: segment.end,
                remaining: segment.end - segment.start,
                lineage: lineage.clone(),
            });
        }
    }
    fn completed(&self, segment: &Segment, lineage: &SharedLineage) {
        let mut state = self.state.lock();
        if let Some(record) = state
            .recovered
            .iter_mut()
            .find(|r| r.piece == segment.piece_id && Arc::ptr_eq(&r.lineage, lineage))
        {
            record.remaining = record.remaining.saturating_sub(segment.end - segment.start);
        }
        state.recovered.retain(|r| r.remaining > 0);
    }
    fn backoff(&self, delay: Duration) {
        let until = Instant::now().checked_add(delay);
        let mut state = self.state.lock();
        state.blocked_until = state.blocked_until.max(until);
    }
}
pub(super) fn usable_validator(spec: &DownloadSpec, meta: &ResponseMeta) -> Option<String> {
    let conflict = spec.headers.keys().any(|h| {
        [
            "if-match",
            "if-range",
            "if-none-match",
            "if-unmodified-since",
            "if-modified-since",
        ]
        .iter()
        .any(|condition| h.eq_ignore_ascii_case(condition))
    });
    (!conflict)
        .then(|| {
            meta.etag
                .as_deref()
                .filter(|v| strong_etag(v))
                .map(str::to_owned)
        })
        .flatten()
}

fn strong_etag(value: &str) -> bool {
    value.starts_with('"')
        && value.ends_with('"')
        && value.len() >= 2
        && value[1..value.len() - 1]
            .bytes()
            .all(|b| b == 0x21 || (0x23..=0x7e).contains(&b) || b >= 0x80)
}

struct Slot<'a> {
    permit: Option<OwnedSemaphorePermit>,
    owner: &'a Coordinator,
}
impl Drop for Slot<'_> {
    fn drop(&mut self) {
        drop(self.permit.take());
        self.owner.changed.notify_waiters();
    }
}
struct HedgeGuard<'a>(&'a Coordinator);
impl Drop for HedgeGuard<'_> {
    fn drop(&mut self) {
        self.0.state.lock().hedge = false;
    }
}

async fn wait_backoff_deadline(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await,
        None => std::future::pending().await,
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn worker_loop(
    worker_id: usize,
    cfg: &WorkerConfig,
    recovery: &Arc<Coordinator>,
    scheduler: Scheduler,
    write_tx: mpsc::Sender<WriterCommand>,
    received: Arc<AtomicU64>,
    mut stop: watch::Receiver<StopSignal>,
    budget: Arc<MemoryBudget>,
    speed: SpeedLimit,
    first_response: SharedProbeResponse,
    total: u64,
    log_level: LogLevel,
    download_id: u64,
) -> Result<(), DownloadError> {
    let validator = recovery.validator.as_deref();
    let worker = match validator {
        Some(etag) => cfg.worker.clone().with_validator(etag),
        None => cfg.worker.clone(),
    };
    if recovery.policy.mode == SlowTransferMode::AdaptiveWithHedging && validator.is_none() {
        log_debug!(
            log_level,
            download_id,
            worker_id,
            "hedging disabled: no usable strong validator or conflicting condition header"
        );
    }
    loop {
        if let Some(error) = stop_signal_error(*stop.borrow()) {
            return Err(error);
        }
        // Register before inspecting state. Notify::notify_waiters plus enable
        // prevents a completion/reclaim between checking and waiting being lost.
        let notified = recovery.changed.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        let mut backoff_deadline = None;
        let assignment = {
            let mut scheduler = scheduler.lock();
            if scheduler.all_done() {
                return Ok(());
            }
            recovery
                .slots
                .clone()
                .try_acquire_owned()
                .ok()
                .and_then(|permit| {
                    let mut state = recovery.state.lock();
                    let segment = if state.pending_backoff(Instant::now()).is_some() {
                        // Capture while deciding not to assign. Rechecking
                        // after unlocking could lose a deadline that expires
                        // between the decision and registering the timer.
                        backoff_deadline = state.blocked_until;
                        None
                    } else if let Some((piece, start, end, slots)) = state.pending.pop_front() {
                        let len = end - start;
                        let split = len.div_ceil(slots as u64).max(cfg.min_segment_size);
                        let split_end =
                            if slots > 1 && len.saturating_sub(split) >= cfg.min_segment_size {
                                start + split
                            } else {
                                end
                            };
                        if split_end < end {
                            state.pending.push_front((piece, split_end, end, slots - 1));
                        }
                        scheduler.assign_subrange(piece, start, split_end, worker_id)
                    } else {
                        scheduler.assign_to_with_request_split(
                            worker_id,
                            cfg.max_active_leases,
                            cfg.min_segment_size,
                            recovery
                                .slot_capacity
                                .saturating_sub(recovery.slots.available_permits())
                                .saturating_sub(1),
                        )
                    };
                    segment.map(|segment| (segment, permit))
                })
        };
        let Some((mut segment, permit)) = assignment else {
            tokio::select! {
                _ = &mut notified => {},
                error = wait_for_stop(&mut stop) => return Err(error),
                _ = wait_backoff_deadline(backoff_deadline) => {},
            }
            continue;
        };
        let slot = Slot {
            permit: Some(permit),
            owner: recovery,
        };
        let lineage = recovery.lineage(&segment, cfg);
        // Consume an exact probe before batching, so it neither becomes an
        // unused live response nor forces a second request for the same bytes.
        let mut initial_response =
            super::take_matching_probe_response(&first_response, &segment).await;
        // Recovered ranges keep their existing per-piece lineage and splitting.
        let mut queued: VecDeque<Segment> = if segment.attempt == 1 && initial_response.is_none() {
            scheduler
                .lock()
                .extend_batch(
                    &segment,
                    worker_id,
                    cfg.max_active_leases,
                    cfg.request_batch_size,
                )
                .into()
        } else {
            VecDeque::new()
        };
        let mut request_end = queued.back().map_or(segment.end, |last| last.end);
        let mut stream = None;
        let mut observation = Arc::new(Mutex::new(Observation::new(
            Instant::now(),
            recovery.policy.window,
        )));
        loop {
            observation.lock().phase(Phase::WriterBarrier);
            begin_lease_and_wait(&write_tx, segment.lease_key()).await?;
            if stream.is_none() {
                observation.lock().phase(Phase::Headers);
            }
            observation.lock().forwarded = 0;
            recovery
                .state
                .lock()
                .active
                .insert(segment.lease_key(), observation.clone());
            let response = initial_response.take();
            let context = AttemptContext {
                worker: &worker,
                cfg,
                recovery,
                scheduler: &scheduler,
                segment: &segment,
                request_end,
                write_tx: &write_tx,
                received: &received,
                budget: &budget,
                speed: &speed,
                total,
                validator,
                observation: &observation,
                log_level,
                download_id,
            };
            let outcome = run_attempt(&context, response, &mut stop, &lineage, &mut stream).await;
            // run_attempt's futures have been dropped: no producer can enqueue
            // old generation data after this point.
            if matches!(outcome, Outcome::Complete) && !queued.is_empty() {
                // Move one request observation across piece identities without
                // fabricating several independent healthy history samples.
                recovery.state.lock().active.remove(&segment.lease_key());
            } else {
                recovery
                    .finish_observation(segment.lease_key(), matches!(outcome, Outcome::Complete));
            }
            match outcome {
                Outcome::Complete => {
                    flush_lease_and_wait(&write_tx, segment.lease_key()).await?;
                    {
                        let mut sample = observation.lock();
                        sample.advance(Instant::now());
                        log_debug!(log_level, download_id, worker_id,
                            request_id = ?sample.request_id, lease = ?segment.lease_key(),
                            start = segment.start, end = segment.end, batch_end = request_end,
                            reading_ms = sample.reading.as_millis() as u64,
                            backpressure_ms = sample.backpressure.as_millis() as u64,
                            writer_barrier_ms = sample.writer_barrier.as_millis() as u64,
                            "adaptive piece writer acknowledged");
                    }
                    if !scheduler.lock().complete(segment.lease_key()) {
                        return Err(DownloadError::Internal("stale adaptive completion".into()));
                    }
                    recovery.completed(&segment, &lineage);
                    recovery.changed.notify_waiters();
                    if let Some(next) = queued.pop_front() {
                        segment = next;
                        continue;
                    }
                    break;
                }
                Outcome::Recover(_) | Outcome::Staged(_) | Outcome::Failed(_) => {
                    // Cancelled attempt futures no longer own the retained body.
                    // No unstarted reserved piece has contributed progress.
                    let read_ahead = stream
                        .as_ref()
                        .map_or(0, |body| body.wire.saturating_sub(body.consumed));
                    drop(stream.take());
                    request_end = segment.end;
                    let forwarded = observation.lock().forwarded;
                    let (outcome, mut retry_decision) = match outcome {
                        Outcome::Failed(error) => {
                            (None, Some(lineage.lock().retries.decide(error)))
                        }
                        outcome => (Some(outcome), None),
                    };
                    let retain = validator.is_some()
                        && forwarded > 0
                        && forwarded < segment.end - segment.start
                        && (matches!(outcome, Some(Outcome::Recover(_)))
                            || matches!(&retry_decision, Some(RetryDecision::Retry { .. })));
                    let mut prefix = segment.clone();
                    prefix.end = prefix.start + forwarded;
                    observation.lock().phase(Phase::WriterBarrier);
                    if let Err(error) = super::settle_prefix(
                        &write_tx,
                        &scheduler,
                        &mut segment,
                        &received,
                        forwarded,
                        retain,
                    )
                    .await
                    {
                        scheduler.lock().reclaim(segment.lease_key());
                        return Err(error);
                    }
                    {
                        let mut sample = observation.lock();
                        sample.advance(Instant::now());
                        log_debug!(log_level, download_id, worker_id,
                            request_id = ?sample.request_id, lease = ?prefix.lease_key(),
                            retained_prefix = retain, forwarded_bytes = forwarded,
                            read_ahead_body_bytes = read_ahead,
                            reading_ms = sample.reading.as_millis() as u64,
                            backpressure_ms = sample.backpressure.as_millis() as u64,
                            writer_barrier_ms = sample.writer_barrier.as_millis() as u64,
                            "adaptive interrupted request writer acknowledged");
                    }
                    // The producer is gone and FIFO acknowledgement completed.
                    // Released batch leases inherit action/retry history, rather
                    // than becoming fresh work with a reset lineage.
                    if let Some(RetryDecision::Retry { backoff, .. }) = &retry_decision {
                        recovery.backoff(*backoff);
                    }
                    for unused in queued.drain(..) {
                        let mut scheduler = scheduler.lock();
                        if !matches!(&retry_decision, Some(RetryDecision::Stop(_))) {
                            recovery.recovered(&unused, &lineage, cfg.max_active_leases);
                            scheduler.reclaim(unused.lease_key());
                        }
                    }
                    if retain {
                        recovery.completed(&prefix, &lineage);
                    }
                    recovery.duplicate.fetch_add(
                        read_ahead + if retain { 0 } else { forwarded },
                        Ordering::Relaxed,
                    );
                    match outcome {
                        Some(Outcome::Recover(mut reservation)) => {
                            reservation.cost = read_ahead + if retain { 0 } else { forwarded };
                            {
                                let mut scheduler = scheduler.lock();
                                recovery.recovered(&segment, &lineage, cfg.max_active_leases);
                                scheduler.reclaim(segment.lease_key());
                            }
                            log_info!(
                                log_level,
                                download_id,
                                worker_id,
                                start = segment.start,
                                end = segment.end,
                                attempt = segment.attempt,
                                "slow attempt reclaimed after writer acknowledgement"
                            );
                            recovery.changed.notify_waiters();
                            break;
                        }
                        Some(Outcome::Staged(mut staged)) => {
                            segment = scheduler
                                .lock()
                                .renew(segment.lease_key(), worker_id)
                                .ok_or_else(|| {
                                    DownloadError::Internal(
                                        "cannot renew hedge winner lease".into(),
                                    )
                                })?;
                            let writer_setup = Instant::now();
                            begin_lease_and_wait(&write_tx, segment.lease_key()).await?;
                            let writer_setup_ms = writer_setup.elapsed().as_millis() as u64;
                            let copy_started = Instant::now();
                            staged
                                .copy_to(&segment, &write_tx, &received, &budget, &mut stop)
                                .await?;
                            let staged_copy_ms = copy_started.elapsed().as_millis() as u64;
                            let barrier_started = Instant::now();
                            flush_lease_and_wait(&write_tx, segment.lease_key()).await?;
                            log_debug!(log_level, download_id, worker_id,
                                request_id = ?staged.request_id, lease = ?segment.lease_key(),
                                writer_setup_ms, staged_copy_ms,
                                writer_barrier_ms = barrier_started.elapsed().as_millis() as u64,
                                "hedge winner writer acknowledged");
                            // Only after the winner's writer acknowledgement can
                            // its staged bytes become useful rather than extra.
                            staged.reservation.cost = read_ahead + forwarded;
                            recovery
                                .duplicate
                                .fetch_sub(segment.end - segment.start, Ordering::Relaxed);
                            if !scheduler.lock().complete(segment.lease_key()) {
                                return Err(DownloadError::Internal(
                                    "stale hedge winner completion".into(),
                                ));
                            }
                            recovery.completed(&segment, &lineage);
                            recovery.changed.notify_waiters();
                            log_info!(
                                log_level,
                                download_id,
                                worker_id,
                                start = segment.start,
                                end = segment.end,
                                "hedge won and committed through authoritative writer"
                            );
                            break;
                        }
                        None => {
                            let decision = retry_decision
                                .take()
                                .expect("failed attempt has retry decision");
                            match decision {
                                RetryDecision::Stop(error) => {
                                    scheduler.lock().reclaim(segment.lease_key());
                                    return Err(error);
                                }
                                RetryDecision::Retry { backoff, error, .. } => {
                                    recovery.state.lock().retries += 1;
                                    recovery.backoff(backoff);
                                    log_warn!(log_level, download_id, worker_id, attempt = segment.attempt, error = %error, backoff_ms = backoff.as_millis() as u64, "adaptive segment failed, retrying");
                                    segment = scheduler
                                        .lock()
                                        .renew(segment.lease_key(), worker_id)
                                        .ok_or_else(|| {
                                            DownloadError::Internal(
                                                "cannot renew adaptive retry lease".into(),
                                            )
                                        })?;
                                    sleep_with_backoff(backoff, &mut stop).await?;
                                    observation = Arc::new(Mutex::new(Observation::new(
                                        Instant::now(),
                                        recovery.policy.window,
                                    )));
                                }
                            }
                        }
                        Some(Outcome::Complete | Outcome::Failed(_)) => unreachable!(),
                    }
                }
            }
        }
        drop(slot);
        // Give an already idle worker an opportunity to take a reclaimed range.
        tokio::task::yield_now().await;
    }
}

struct AttemptContext<'a> {
    worker: &'a HttpWorker,
    cfg: &'a WorkerConfig,
    recovery: &'a Coordinator,
    scheduler: &'a Scheduler,
    segment: &'a Segment,
    request_end: u64,
    write_tx: &'a mpsc::Sender<WriterCommand>,
    received: &'a Arc<AtomicU64>,
    budget: &'a Arc<MemoryBudget>,
    speed: &'a SpeedLimit,
    total: u64,
    validator: Option<&'a str>,
    observation: &'a Arc<Mutex<Observation>>,
    log_level: LogLevel,
    download_id: u64,
}
impl AttemptContext<'_> {
    fn decision(&self, reason: &'static str, baseline: Option<f64>) {
        // Fixed reason vocabulary bounds memory; emit only state changes.
        *self
            .recovery
            .state
            .lock()
            .decisions
            .entry(reason)
            .or_default() += 1;
        let has_available = self.scheduler.lock().has_available();
        let tail_block = if self.segment.end - self.segment.start > MAX_HEDGE {
            "range_too_large"
        } else if has_available {
            "unclaimed_work"
        } else if self.recovery.slots.available_permits() == 0 {
            "no_idle_slot"
        } else if !self.recovery.state.lock().pending.is_empty() {
            "pending_recovery"
        } else if baseline.is_none() {
            "no_healthy_baseline"
        } else {
            "none"
        };
        let mut sample = self.observation.lock();
        if sample.last_reason == Some((reason, tail_block)) {
            return;
        }
        sample.last_reason = Some((reason, tail_block));
        sample.advance(Instant::now());
        log_debug!(self.log_level, download_id = self.download_id,
            worker_id = self.segment.owner_worker_id, request_id = ?sample.request_id,
            lease = ?self.segment.lease_key(), start = self.segment.start, end = self.segment.end,
            batch_end = self.request_end, reserved_batch_bytes = self.request_end - self.segment.end,
            remaining_piece_bytes = (self.segment.end - self.segment.start).saturating_sub(sample.forwarded),
            remaining_required_bytes = self.total.saturating_sub(self.received.load(Ordering::Relaxed)),
            idle_slots = self.recovery.slots.available_permits(),
            active_slots = self.recovery.slot_capacity - self.recovery.slots.available_permits(),
            fast_tail_block = tail_block,
            batch_final_piece = self.request_end == self.segment.end,
            hedge_validator_available = self.validator.is_some(),
            reason, phase = ?sample.phase, body_bytes = sample.wire,
            reading_ms = sample.reading.as_millis() as u64,
            headers_phase_ms = sample.headers.as_millis() as u64,
            backpressure_ms = sample.backpressure.as_millis() as u64,
            writer_barrier_ms = sample.writer_barrier.as_millis() as u64,
            baseline_bytes_sec = ?baseline, "adaptive recovery decision");
    }
}
enum Outcome {
    Complete,
    Recover(Reservation),
    Staged(Staged),
    Failed(DownloadError),
}
async fn run_attempt(
    ctx: &AttemptContext<'_>,
    response: Option<(HttpResponse, ResponseMeta)>,
    stop: &mut watch::Receiver<StopSignal>,
    lineage: &SharedLineage,
    stream: &mut Option<RequestStream>,
) -> Outcome {
    let mut primary_stop = stop.clone();
    let primary = primary(ctx, response, &mut primary_stop, stream);
    tokio::pin!(primary);
    let mut ticker = tokio::time::interval(ctx.recovery.tick());
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut challenger: Option<futures::future::BoxFuture<'_, Result<Staged, CandidateFailure>>> =
        None;
    let mut hedge_guard = None;
    loop {
        tokio::select! {
            biased;
            error = wait_for_stop(stop) => return Outcome::Failed(error),
            _ = ctx.write_tx.closed() => return Outcome::Failed(DownloadError::ChannelClosed),
            result = &mut primary => return match result { Ok(()) => Outcome::Complete, Err(error) => Outcome::Failed(error) },
            result = async { match challenger.as_mut() { Some(future) => future.await, None => std::future::pending().await } } => {
                challenger = None;
                drop(hedge_guard.take());
                match result {
                    Ok(staged) => return Outcome::Staged(staged),
                    Err(failure) => {
                        // A changed object is fatal even if another attempt is
                        // still progressing. Ordinary challenger failure isn't.
                        if failure.identity_changed { return Outcome::Failed(failure.error); }
                        if let Some(seconds) = failure.error.retry_after_secs() { ctx.recovery.backoff(Duration::from_secs(seconds)); }
                        log_debug!(ctx.log_level, download_id = ctx.download_id, "hedge failed; primary remains authoritative");
                    }
                }
            },
            _ = ticker.tick() => {
                if ctx.recovery.policy.mode == SlowTransferMode::Disabled { ctx.decision("disabled", None); continue; }
                let now = Instant::now();
                let baseline = ctx.recovery.baseline(ctx.segment.lease_key(), now);
                let len = ctx.segment.end-ctx.segment.start;
                let tail_baseline = if !matches!(ctx.speed, SpeedLimit::Limited(_))
                    && ctx.recovery.tail_eligible(len, ctx.scheduler.lock().has_available()) {
                    ctx.recovery.sample_baseline(ctx.segment.lease_key(), now, true)
                } else { None };
                let (ordinary, tail) = {
                    let mut observation = ctx.observation.lock();
                    (observation.should_recover(now, baseline, &ctx.recovery.policy, len),
                     observation.should_recover_tail(now, tail_baseline, &ctx.recovery.policy, len))
                };
                let eligible = ordinary || tail;
                // A user cap deliberately couples all request rates. Keep
                // observing phases but conservatively suppress speculative
                // performance work while that cap is active.
                if matches!(ctx.speed, SpeedLimit::Limited(_)) {
                    ctx.decision("rate_limit", baseline);
                    continue;
                }
                if challenger.is_some() { ctx.decision("challenger_active", baseline); continue; }
                if !eligible {
                    let phase = ctx.observation.lock().phase;
                    let reason = if phase != Phase::Reading { "not_reading" }
                        else if baseline.is_none() && ctx.recovery.policy.absolute.is_none() { "no_healthy_baseline" }
                        else { "slow_threshold_or_benefit" };
                    ctx.decision(reason, baseline);
                    continue;
                }
                let len = ctx.segment.end-ctx.segment.start;
                let mut slot = None;
                if ctx.request_end == ctx.segment.end && ctx.validator.is_some() && ctx.recovery.policy.mode == SlowTransferMode::AdaptiveWithHedging && len <= MAX_HEDGE
                    && !ctx.scheduler.lock().has_available() {
                    slot = ctx.recovery.slots.clone().try_acquire_owned().ok();
                }
                let mut hedge = slot.is_some();
                // This task owns the primary future. Once selected it is dropped
                // before another body poll; no producer can race this snapshot.
                let mut cost_bound = if hedge { len } else {
                    cancellation_cost_bound(&ctx.observation.lock(), len, ctx.validator.is_some())
                };
                let mut reservation = match ctx.recovery.reserve_result(cost_bound, lineage, hedge, now) {
                    Ok(reservation) => reservation,
                    Err("budget") if hedge => {
                        // A refused hedge has not consumed an action or lineage
                        // allowance. Release its spare permit before considering
                        // the existing cancel-before-resume path, rechecking all
                        // reservation guards against that path's own cost bound.
                        drop(slot.take());
                        *ctx.recovery.state.lock().decisions.entry("hedge_budget").or_default() += 1;
                        hedge = false;
                        cost_bound = cancellation_cost_bound(&ctx.observation.lock(), len, ctx.validator.is_some());
                        match ctx.recovery.reserve_result(cost_bound, lineage, false, now) {
                            Ok(reservation) => reservation,
                            Err(reason) => { ctx.decision(reason, baseline); continue; }
                        }
                    }
                    Err(reason) => { ctx.decision(reason, baseline); continue; }
                };
                if hedge { reservation.cost = 0; }
                ctx.decision("selected", baseline);
                log_info!(ctx.log_level, download_id = ctx.download_id, worker_id = ctx.segment.owner_worker_id,
                    attempt = ctx.segment.attempt, start = ctx.segment.start, end = ctx.segment.end,
                    hedge, fast_tail = tail, cost_bound_bytes = cost_bound,
                    "sustained low reading speed; bounded recovery selected");
                if let Some(permit) = slot {
                    hedge_guard = Some(HedgeGuard(ctx.recovery));
                    challenger = Some(Box::pin(async move {
                        let _slot = Slot { permit: Some(permit), owner: ctx.recovery };
                        stage(ctx, reservation).await
                    }));
                } else { return Outcome::Recover(reservation); }
            }
        }
    }
}

struct CandidateFailure {
    error: DownloadError,
    identity_changed: bool,
}
impl From<DownloadError> for CandidateFailure {
    fn from(error: DownloadError) -> Self {
        let identity_changed = matches!(error, DownloadError::HttpStatus { status: 412, .. });
        Self {
            error,
            identity_changed,
        }
    }
}
impl From<std::io::Error> for CandidateFailure {
    fn from(error: std::io::Error) -> Self {
        DownloadError::Io(error).into()
    }
}

async fn checked_response(
    ctx: &AttemptContext<'_>,
    response: Option<(HttpResponse, ResponseMeta)>,
    request_end: u64,
) -> Result<HttpResponse, CandidateFailure> {
    let (response, meta) = match response {
        Some(response) => response,
        None => {
            ctx.worker
                .clone()
                .with_attempt(
                    ctx.segment.owner_worker_id,
                    ctx.segment.piece_id,
                    ctx.segment.lease_key().lease_id,
                )
                .send_range(ctx.segment.start, request_end - 1)
                .await?
        }
    };
    let diagnostics = response
        .extensions()
        .get::<crate::http::worker::RequestDiagnostics>();
    log_debug!(ctx.log_level, download_id = ctx.download_id,
        worker_id = ctx.segment.owner_worker_id,
        request_id = ?diagnostics.map(|d| d.id), lease = ?ctx.segment.lease_key(),
        start = ctx.segment.start, end = request_end,
        headers_ms = ?diagnostics.map(|d| d.headers_elapsed.as_millis() as u64),
        "adaptive response associated with lease");
    // A positively different validator is an identity failure even when the
    // same response also has an invalid range/total. Missing metadata on an
    // already malformed response remains an optional-candidate failure.
    if ctx.validator.is_some_and(|expected| {
        meta.etag
            .as_deref()
            .is_some_and(|actual| actual != expected)
    }) {
        return Err(CandidateFailure {
            error: DownloadError::ResumeMismatch(
                "object validator changed during adaptive download".into(),
            ),
            identity_changed: true,
        });
    }
    let retry_after = response
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok());
    validate_range_response(
        response.status().as_u16(),
        retry_after,
        &meta,
        RangeValidationMode::Segment,
        ExpectedRange {
            start: ctx.segment.start,
            end_inclusive: request_end - 1,
            total_size: Some(ctx.total),
        },
    )?;
    if ctx.recovery.policy.mode == SlowTransferMode::AdaptiveWithHedging
        && ctx
            .validator
            .is_some_and(|etag| meta.etag.as_deref() != Some(etag))
    {
        return Err(CandidateFailure {
            error: DownloadError::ResumeMismatch(
                "object validator changed during adaptive download".into(),
            ),
            identity_changed: true,
        });
    }
    Ok(response)
}
struct RequestStream {
    body: hyper::body::Incoming,
    buffered: bytes::Bytes,
    wire: u64,
    expected: u64,
    consumed: u64,
}

async fn primary(
    ctx: &AttemptContext<'_>,
    response: Option<(HttpResponse, ResponseMeta)>,
    stop: &mut watch::Receiver<StopSignal>,
    stream: &mut Option<RequestStream>,
) -> Result<(), DownloadError> {
    if stream.is_none() {
        let response = checked_response(ctx, response, ctx.request_end)
            .await
            .map_err(|failure| failure.error)?;
        ctx.observation.lock().request_id = response
            .extensions()
            .get::<crate::http::worker::RequestDiagnostics>()
            .map(|d| d.id);
        let body = response.into_body();
        *stream = Some(RequestStream {
            body,
            buffered: bytes::Bytes::new(),
            wire: 0,
            consumed: 0,
            expected: ctx.request_end - ctx.segment.start,
        });
    }
    let stream = stream.as_mut().expect("initialized request body");
    loop {
        let forwarded = ctx.observation.lock().forwarded;
        if forwarded == ctx.segment.end - ctx.segment.start && ctx.segment.end < ctx.request_end {
            // A frame may contain the next piece. Keep its suffix across the
            // flush/complete barrier instead of issuing another HTTP request.
            ctx.observation.lock().phase(Phase::WriterBarrier);
            return Ok(());
        }
        if stream.buffered.is_empty() {
            ctx.observation.lock().phase(Phase::Reading);
            let chunk = next_data_chunk(&mut stream.body, ctx.cfg.read_timeout).await?;
            let Some(data) = chunk else {
                if stream.wire != stream.expected {
                    return Err(DownloadError::Transport(crate::error::TransportError::new(
                        crate::error::TransportErrorKind::Body,
                        std::io::Error::new(
                            std::io::ErrorKind::UnexpectedEof,
                            "range body ended early",
                        ),
                    )));
                }
                ctx.observation.lock().phase(Phase::WriterBarrier);
                return Ok(());
            };
            stream.wire = stream.wire.saturating_add(data.len() as u64);
            ctx.recovery
                .wire
                .fetch_add(data.len() as u64, Ordering::Relaxed);
            ctx.observation.lock().wire += data.len() as u64;
            if stream.wire > stream.expected {
                return Err(DownloadError::ResumeMismatch(
                    "server overran requested range".into(),
                ));
            }
            stream.buffered = data;
            if stream.buffered.is_empty() {
                continue;
            }
        }
        let remaining = ctx.segment.end - ctx.segment.start - forwarded;
        let len = stream
            .buffered
            .len()
            .min(ctx.budget.max_chunk)
            .min(usize::try_from(remaining).unwrap_or(usize::MAX));
        if len == 0 {
            return Err(DownloadError::ResumeMismatch(
                "server overran final piece".into(),
            ));
        }
        ctx.observation.lock().phase(Phase::RateLimited);
        ctx.speed.acquire(len).await;
        ctx.observation.lock().phase(Phase::MemoryBlocked);
        let permit = ctx
            .budget
            .semaphore
            .acquire_many(len as u32)
            .await
            .map_err(|_| DownloadError::ChannelClosed)?;
        ctx.observation.lock().phase(Phase::ChannelBlocked);
        let slot = ctx
            .write_tx
            .reserve()
            .await
            .map_err(|_| DownloadError::ChannelClosed)?;
        slot.send(WriterCommand::Data {
            data: stream.buffered.split_to(len),
            offset: ctx.segment.start + forwarded,
            lease_key: Some(ctx.segment.lease_key()),
        });
        permit.forget();
        stream.consumed += len as u64;
        {
            let mut sample = ctx.observation.lock();
            sample.forwarded += len as u64;
            sample.enqueued += len as u64;
        }
        ctx.received.fetch_add(len as u64, Ordering::Relaxed);
        if let Some(error) = stop_signal_error(*stop.borrow()) {
            return Err(error);
        }
    }
}

struct Staged {
    file: tokio::fs::File,
    _path: tempfile::TempPath,
    reservation: Reservation,
    request_id: Option<u64>,
}
impl Staged {
    async fn copy_to(
        &mut self,
        segment: &Segment,
        tx: &mpsc::Sender<WriterCommand>,
        received: &Arc<AtomicU64>,
        budget: &MemoryBudget,
        stop: &mut watch::Receiver<StopSignal>,
    ) -> Result<(), DownloadError> {
        self.file.rewind().await?;
        let mut offset = segment.start;
        while offset < segment.end {
            let len = (segment.end - offset).min(budget.max_chunk.min(64 * 1024) as u64) as usize;
            let copy = async {
                let permit = budget
                    .semaphore
                    .acquire_many(len as u32)
                    .await
                    .map_err(|_| DownloadError::ChannelClosed)?;
                let mut data = vec![0u8; len];
                self.file.read_exact(&mut data).await?;
                let slot = tx
                    .reserve()
                    .await
                    .map_err(|_| DownloadError::ChannelClosed)?;
                slot.send(WriterCommand::Data {
                    offset,
                    data: bytes::Bytes::from(data),
                    lease_key: Some(segment.lease_key()),
                });
                permit.forget();
                received.fetch_add(len as u64, Ordering::Relaxed);
                Ok::<_, DownloadError>(())
            };
            tokio::select! { biased;
                error = wait_for_stop(stop) => return Err(error),
                _ = tx.closed() => return Err(DownloadError::ChannelClosed),
                result = copy => { result?; offset += len as u64; }
            }
        }
        Ok(())
    }
}
async fn stage(
    ctx: &AttemptContext<'_>,
    mut reservation: Reservation,
) -> Result<Staged, CandidateFailure> {
    reservation.cost = 0;
    let temporary = tempfile::Builder::new()
        .prefix(".bytehaul-hedge-")
        .tempfile_in(&ctx.recovery.output_dir)?;
    let (file, path) = temporary.into_parts();
    let mut staged = Staged {
        file: tokio::fs::File::from_std(file),
        _path: path,
        reservation,
        request_id: None,
    };
    let response = checked_response(ctx, None, ctx.segment.end).await?;
    staged.request_id = response
        .extensions()
        .get::<crate::http::worker::RequestDiagnostics>()
        .map(|d| d.id);
    // A speculative body's worst-case extra cost must be known before reading
    // it. Content-Length lets Hyper cap admitted body data at this exact range;
    // an optional chunked challenger is abandoned without touching the primary.
    let framed_length = response
        .headers()
        .get(hyper::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok());
    if framed_length != Some(ctx.segment.end - ctx.segment.start)
        || response
            .headers()
            .contains_key(hyper::header::TRANSFER_ENCODING)
    {
        return Err(DownloadError::ResumeMismatch(
            "challenger requires an exact Content-Length for its extra-byte bound".into(),
        )
        .into());
    }
    let mut body = response.into_body();
    let mut wire = 0u64;
    while let Some(mut data) = next_data_chunk(&mut body, ctx.cfg.read_timeout).await? {
        wire = wire.saturating_add(data.len() as u64);
        staged.reservation.cost = wire;
        ctx.recovery
            .wire
            .fetch_add(data.len() as u64, Ordering::Relaxed);
        // Conservative diagnostic until ownership is decided; a winner's
        // useful bytes are accounted only when copied to the main writer.
        ctx.recovery
            .duplicate
            .fetch_add(data.len() as u64, Ordering::Relaxed);
        if wire > ctx.segment.end - ctx.segment.start {
            return Err(
                DownloadError::ResumeMismatch("challenger overran its range".into()).into(),
            );
        }
        while !data.is_empty() {
            let len = data.len().min(ctx.budget.max_chunk);
            ctx.speed.acquire(len).await;
            let _permit = ctx
                .budget
                .semaphore
                .acquire_many(len as u32)
                .await
                .map_err(|_| DownloadError::ChannelClosed)?;
            staged.file.write_all(&data.split_to(len)).await?;
        }
    }
    if wire != ctx.segment.end - ctx.segment.start {
        return Err(DownloadError::Transport(crate::error::TransportError::new(
            crate::error::TransportErrorKind::Body,
            std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "challenger body ended early",
            ),
        ))
        .into());
    }
    staged.file.flush().await?;
    Ok(staged)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn policy() -> Policy {
        Policy {
            mode: SlowTransferMode::Adaptive,
            absolute: Some(1000),
            duration: Duration::from_secs(2),
            grace: Duration::from_secs(2),
            window: Duration::from_secs(1),
        }
    }
    fn reading(now: Instant) -> Observation {
        let mut sample = Observation::new(now, Duration::from_secs(1));
        sample.phase = Phase::Reading;
        sample.blocked_since = None;
        sample
    }
    fn meta(etag: Option<&str>) -> ResponseMeta {
        ResponseMeta {
            content_length: None,
            content_range_start: None,
            content_range_end: None,
            content_range_total: None,
            accept_ranges: true,
            etag: etag.map(str::to_owned),
            last_modified: None,
            content_disposition: None,
            content_encoding: None,
        }
    }
    fn coordinator(total: u64) -> Arc<Coordinator> {
        Coordinator::new(
            &DownloadSpec::new("http://example.invalid"),
            &meta(Some("\"v1\"")),
            Path::new("file"),
            total,
        )
        .unwrap()
    }
    fn lineage() -> SharedLineage {
        Arc::new(Mutex::new(Lineage {
            retries: RetryState::new(2, Duration::ZERO, Duration::ZERO, None),
            recoveries: 0,
        }))
    }
    #[test]
    fn prefix_recovery_reserves_only_unforwarded_body_and_settles_once() {
        let recovery = coordinator(64_469_455);
        let lineage = lineage();
        let now = Instant::now();
        let mut sample = reading(now);
        // One earlier piece plus a partial current piece are already enqueued;
        // a frame suffix is still local, including bytes awaiting a permit.
        sample.wire = 1024 * 1024 + 80_000;
        sample.enqueued = 1024 * 1024 + 40_000;
        sample.forwarded = 40_000;
        let bound = cancellation_cost_bound(&sample, 1024 * 1024, true);
        assert_eq!(bound, 40_000);
        assert!(cancellation_cost_bound(&sample, 1024 * 1024, false) > recovery.extra_limit);
        let mut reservation = recovery
            .reserve_result(bound, &lineage, false, now)
            .unwrap();
        assert_eq!(recovery.state.lock().reserved, 40_000);
        assert_eq!(recovery.state.lock().consumed_extra, 0);
        // A conservative bound can be refunded when less discarded traffic is
        // proved; unused bound must not remain consumed across the next lease.
        reservation.cost = 30_000;
        drop(reservation);
        assert_eq!(recovery.state.lock().reserved, 0);
        assert_eq!(recovery.state.lock().consumed_extra, 30_000);
        assert_eq!(
            recovery
                .reserve_result(1024 * 1024, &lineage, true, now + COOLDOWN)
                .err(),
            Some("budget")
        );
        assert_eq!(lineage.lock().recoveries, 1);
    }

    #[test]
    fn refused_hedge_and_over_budget_cancellation_do_not_charge_an_action() {
        let recovery = coordinator(64_469_455);
        let lineage = lineage();
        let now = Instant::now();
        let mut sample = reading(now);
        sample.wire = recovery.extra_limit + 1;
        let spare = recovery.slots.try_acquire().unwrap();
        assert_eq!(
            recovery
                .reserve_result(1024 * 1024, &lineage, true, now)
                .err(),
            Some("budget")
        );
        drop(spare);
        let cancellation = cancellation_cost_bound(&sample, 1024 * 1024, true);
        assert_eq!(
            recovery
                .reserve_result(cancellation, &lineage, false, now)
                .err(),
            Some("budget")
        );
        let state = recovery.state.lock();
        assert_eq!(state.actions, 0);
        assert_eq!(state.reserved, 0);
        assert_eq!(state.consumed_extra, 0);
        assert_eq!(state.last_action, None);
        assert!(!state.hedge);
        assert_eq!(lineage.lock().recoveries, 0);
        assert_eq!(recovery.slots.available_permits(), recovery.slot_capacity);
    }

    #[test]
    fn zero_cost_and_interrupted_reservations_remain_bounded() {
        let recovery = coordinator(64_469_455);
        let lineage = lineage();
        let now = Instant::now();
        let reservation = recovery.reserve_result(123, &lineage, false, now).unwrap();
        // Dropping a future during writer acknowledgement charges its bound
        // exactly once even when the success settlement code never runs.
        drop(reservation);
        assert_eq!(recovery.state.lock().reserved, 0);
        assert_eq!(recovery.state.lock().consumed_extra, 123);
        drop(
            recovery
                .reserve_result(0, &lineage, false, now + COOLDOWN)
                .unwrap(),
        );
        assert_eq!(
            recovery
                .reserve_result(0, &lineage, false, now + COOLDOWN * 2)
                .err(),
            Some("lineage_limit")
        );
        assert_eq!(recovery.state.lock().consumed_extra, 123);
        assert_eq!(recovery.state.lock().actions, 2);
    }

    #[tokio::test]
    async fn writer_barrier_success_failure_and_abort_settle_reservation_once() {
        for outcome in ["confirm", "writer_failure", "cancel"] {
            let recovery = coordinator(64_469_455);
            let mut reservation = recovery
                .reserve_result(321, &lineage(), false, Instant::now())
                .unwrap();
            let scheduler: Scheduler =
                Arc::new(Mutex::new(SchedulerState::new(PieceMap::new(32, 32))));
            let mut segment = scheduler.lock().assign_to(0).unwrap();
            let received = Arc::new(AtomicU64::new(12));
            let (write_tx, mut write_rx) = mpsc::channel(1);
            let task = tokio::spawn(async move {
                let result = super::super::settle_prefix(
                    &write_tx,
                    &scheduler,
                    &mut segment,
                    &received,
                    12,
                    true,
                )
                .await;
                if result.is_ok() {
                    reservation.cost = 123;
                }
                drop(reservation);
                result
            });
            let Some(WriterCommand::FlushLease { ack, .. }) = write_rx.recv().await else {
                panic!("prefix must wait on the authoritative writer");
            };
            assert_eq!(recovery.state.lock().reserved, 321);
            assert_eq!(recovery.state.lock().consumed_extra, 0);
            assert!(!task.is_finished());
            match outcome {
                "confirm" => {
                    ack.send(()).unwrap();
                    task.await.unwrap().unwrap();
                }
                "writer_failure" => {
                    drop(ack);
                    assert!(matches!(
                        task.await.unwrap(),
                        Err(DownloadError::ChannelClosed)
                    ));
                }
                _ => {
                    task.abort();
                    assert!(task.await.unwrap_err().is_cancelled());
                    assert!(ack.send(()).is_err());
                }
            }
            assert_eq!(recovery.state.lock().reserved, 0);
            assert_eq!(
                recovery.state.lock().consumed_extra,
                if outcome == "confirm" { 123 } else { 321 }
            );
        }
    }

    #[tokio::test]
    async fn real_memory_backpressure_suppresses_recovery_and_cancels() {
        use warp::Filter;
        let calls = Arc::new(AtomicU64::new(0));
        let request_calls = calls.clone();
        let route = warp::any().map(move || {
            request_calls.fetch_add(1, Ordering::Relaxed);
            warp::http::Response::builder()
                .status(206)
                .header("content-length", "32")
                .header("content-range", "bytes 0-31/32")
                .header("etag", "\"v1\"")
                .body(vec![7u8; 32])
                .unwrap()
        });
        let (addr, server) = warp::serve(route).bind_ephemeral(([127, 0, 0, 1], 0));
        let server = tokio::spawn(server);
        let spec = DownloadSpec::new(format!("http://{addr}/file"))
            .low_speed_limit(1024)
            .slow_start_grace(Duration::from_millis(20))
            .slow_sample_window(Duration::from_millis(30))
            .low_speed_duration(Duration::from_millis(60));
        let client = crate::network::ClientNetworkConfig::default()
            .build_client()
            .unwrap();
        let worker = HttpWorker::new(client, &spec);
        let recovery = Coordinator::new(
            &spec,
            &meta(Some("\"v1\"")),
            Path::new("unused"),
            64_469_455,
        )
        .unwrap();
        let scheduler: Scheduler = Arc::new(Mutex::new(SchedulerState::new(PieceMap::new(32, 32))));
        let segment = scheduler.lock().assign_to(0).unwrap();
        let cfg = WorkerConfig {
            worker: worker.clone(),
            read_timeout: Duration::from_secs(2),
            max_retries: 0,
            retry_base_delay: Duration::ZERO,
            retry_max_delay: Duration::ZERO,
            max_retry_elapsed: None,
            max_active_leases: 4,
            min_segment_size: 32,
            request_batch_size: 0,
            validator: Some("\"v1\"".into()),
            recovery: None,
        };
        let budget = Arc::new(MemoryBudget::new(32));
        let _all_memory = budget.semaphore.acquire_many(32).await.unwrap();
        let (write_tx, _write_rx) = mpsc::channel(1);
        let received = Arc::new(AtomicU64::new(0));
        let observation = Arc::new(Mutex::new(Observation::new(
            Instant::now(),
            spec.slow_sample_window,
        )));
        recovery
            .state
            .lock()
            .active
            .insert(segment.lease_key(), observation.clone());
        let _slot = recovery.slots.try_acquire().unwrap();
        let speed = SpeedLimit::Unlimited;
        let ctx = AttemptContext {
            worker: &worker,
            cfg: &cfg,
            recovery: &recovery,
            scheduler: &scheduler,
            segment: &segment,
            request_end: 32,
            write_tx: &write_tx,
            received: &received,
            budget: &budget,
            speed: &speed,
            total: 32,
            validator: Some("\"v1\""),
            observation: &observation,
            log_level: LogLevel::Off,
            download_id: 0,
        };
        let (stop_tx, mut stop) = watch::channel(StopSignal::Running);
        let mut stream = None;
        let lineage = lineage();
        let attempt = run_attempt(&ctx, None, &mut stop, &lineage, &mut stream);
        tokio::pin!(attempt);
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                tokio::select! {
                    _ = &mut attempt => panic!("request must remain blocked on memory"),
                    _ = tokio::time::sleep(Duration::from_millis(1)) => {
                        if observation.lock().phase == Phase::MemoryBlocked { break; }
                    }
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(observation.lock().wire, 32);
        assert_eq!(cancellation_cost_bound(&observation.lock(), 32, true), 32);
        tokio::select! {
            _ = &mut attempt => panic!("local pressure must not trigger recovery"),
            _ = tokio::time::sleep(Duration::from_millis(200)) => {}
        }
        assert_eq!(recovery.state.lock().actions, 0);
        assert_eq!(received.load(Ordering::Relaxed), 0);
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        stop_tx.send(StopSignal::Cancel).unwrap();
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(1), &mut attempt)
                .await
                .unwrap(),
            Outcome::Failed(DownloadError::Cancelled)
        ));
        server.abort();
    }

    #[test]
    fn released_batch_pieces_share_lineage_without_merging_completion_accounting() {
        let recovery = coordinator(64_469_455);
        let lineage = lineage();
        let first = Segment {
            piece_id: 1,
            lease_id: 1,
            start: 100,
            end: 200,
            owner_worker_id: 0,
            attempt: 1,
        };
        let second = Segment {
            piece_id: 2,
            lease_id: 2,
            start: 200,
            end: 300,
            owner_worker_id: 0,
            attempt: 1,
        };
        recovery.recovered(&first, &lineage, 4);
        recovery.recovered(&second, &lineage, 4);
        assert_eq!(recovery.state.lock().recovered.len(), 2);
        recovery.completed(&first, &lineage);
        let state = recovery.state.lock();
        assert_eq!(state.recovered.len(), 1);
        assert_eq!(state.recovered[0].piece, 2);
        assert_eq!(state.recovered[0].remaining, 100);
        assert!(Arc::ptr_eq(&state.recovered[0].lineage, &lineage));
    }

    #[test]
    fn diagnostic_timing_separates_headers_reading_and_local_waits() {
        let start = Instant::now();
        let mut observation = Observation::new(start, Duration::from_secs(30));
        observation.advance(start + Duration::from_secs(2));
        observation.phase = Phase::Reading;
        observation.advance(start + Duration::from_secs(5));
        observation.phase = Phase::MemoryBlocked;
        observation.advance(start + Duration::from_secs(9));
        observation.phase = Phase::ChannelBlocked;
        observation.advance(start + Duration::from_secs(14));
        observation.phase = Phase::RateLimited;
        observation.advance(start + Duration::from_secs(20));
        observation.phase = Phase::WriterBarrier;
        observation.advance(start + Duration::from_secs(27));
        assert_eq!(observation.headers, Duration::from_secs(2));
        assert_eq!(observation.reading, Duration::from_secs(3));
        assert_eq!(observation.backpressure, Duration::from_secs(15));
        assert_eq!(observation.writer_barrier, Duration::from_secs(7));
        // Repeated snapshots do not add elapsed time a second time.
        observation.advance(start + Duration::from_secs(27));
        assert_eq!(observation.writer_barrier, Duration::from_secs(7));
    }

    #[test]
    fn reservation_diagnostics_preserve_budget_cooldown_and_lineage_guards() {
        let recovery = coordinator(10_000);
        let lineage = lineage();
        let now = Instant::now();
        assert_eq!(
            recovery
                .reserve_result(101, &lineage, false, now)
                .map(|_| ()),
            Err("budget")
        );
        assert_eq!(recovery.state.lock().actions, 0);
        assert_eq!(lineage.lock().recoveries, 0);
        recovery.backoff(Duration::from_secs(1));
        assert_eq!(
            recovery
                .reserve_result(10, &lineage, false, now)
                .map(|_| ()),
            Err("global_backoff")
        );
        let later = now + Duration::from_secs(2);
        assert_eq!(
            recovery
                .reserve_result(10, &lineage, false, later)
                .map(|_| ()),
            Ok(())
        );
        assert_eq!(
            recovery
                .reserve_result(10, &lineage, false, later)
                .map(|_| ()),
            Err("cooldown")
        );
        assert_eq!(
            recovery
                .reserve_result(10, &lineage, false, later + COOLDOWN)
                .map(|_| ()),
            Ok(())
        );
        assert_eq!(
            recovery
                .reserve_result(10, &lineage, false, later + COOLDOWN * 2)
                .map(|_| ()),
            Err("lineage_limit")
        );
        assert_eq!(recovery.state.lock().consumed_extra, 20);
    }

    #[test]
    fn default_tail_detection_is_short_sustained_and_resets_without_evidence() {
        let recovery = coordinator(1_000_000);
        let policy = &recovery.policy;
        let start = Instant::now();
        let mut observation = Observation::new(start, policy.window);
        observation.phase = Phase::Reading;
        observation.blocked_since = None;
        for second in 0..=3 {
            observation.wire = second * 10;
            let now = start + Duration::from_secs(second);
            assert!(!observation.should_recover(now, Some(2000.), policy, 10000));
            assert_eq!(
                observation.should_recover_tail(now, Some(2000.), policy, 10000),
                second == 3
            );
        }
        let now = start + Duration::from_secs(3);
        observation.slow_since = Some(Duration::from_secs(2));
        let ordinary_samples = observation.samples.clone();
        observation.forwarded = 9999;
        assert!(!observation.should_recover_tail(now, Some(2000.), policy, 10000));
        observation.forwarded = 0;
        assert!(!observation.should_recover_tail(now, None, policy, 10000));
        assert_eq!(observation.tail_slow_since, None);
        assert!(!observation.should_recover_tail(now, Some(2000.), policy, 10000));
        assert_eq!(observation.slow_since, Some(Duration::from_secs(2)));
        assert_eq!(observation.samples, ordinary_samples);
        observation.wire += 10000;
        assert!(!observation.should_recover_tail(
            start + Duration::from_secs(4),
            Some(2000.),
            policy,
            10000
        ));
        assert_eq!(observation.tail_slow_since, None);
    }
    #[test]
    fn tail_blocks_clear_only_short_history_and_exclude_local_time() {
        for phase in [
            Phase::Headers,
            Phase::RateLimited,
            Phase::MemoryBlocked,
            Phase::ChannelBlocked,
            Phase::WriterBarrier,
        ] {
            let start = Instant::now();
            let mut observation = Observation::new(start, Duration::from_secs(5));
            observation.phase = phase;
            observation.tail_slow_since = Some(Duration::ZERO);
            observation.slow_since = Some(Duration::ZERO);
            assert_eq!(
                observation.tail_sample(start + Duration::from_secs(1)),
                None
            );
            assert_eq!(observation.tail_slow_since, None);
            assert_eq!(observation.slow_since, Some(Duration::ZERO));
            assert_eq!(observation.reading, Duration::ZERO);
            observation.phase = Phase::Reading;
            observation.blocked_since = None;
            assert_eq!(
                observation.tail_sample(start + Duration::from_millis(1500)),
                None
            );
        }
    }
    #[test]
    fn tail_requires_idle_slot_exhausted_work_and_mature_healthy_peers() {
        let recovery = coordinator(1_000_000);
        let now = Instant::now();
        let key = LeaseKey {
            piece_id: 0,
            lease_id: 1,
        };
        assert!(recovery.tail_eligible(MAX_HEDGE, false));
        assert!(!recovery.tail_eligible(MAX_HEDGE + 1, false));
        assert!(!recovery.tail_eligible(MAX_HEDGE, true));
        let permits = recovery.slots.try_acquire_many(4).unwrap();
        assert!(!recovery.tail_eligible(MAX_HEDGE, false));
        drop(permits);
        recovery.state.lock().pending.push_back((0, 0, 1000, 1));
        assert!(!recovery.tail_eligible(MAX_HEDGE, false));
        recovery.state.lock().pending.clear();
        let _target_permit = recovery.slots.try_acquire().unwrap();
        recovery
            .state
            .lock()
            .active
            .insert(key, Arc::new(Mutex::new(reading(now))));
        assert_eq!(recovery.sample_baseline(key, now, true), None);
        recovery
            .state
            .lock()
            .history
            .extend([(now, 2000.), (now, 2000.)]);
        assert_eq!(recovery.sample_baseline(key, now, true), Some(2000.));
        let peer_key = LeaseKey {
            piece_id: 1,
            lease_id: 2,
        };
        let peer_permit = recovery.slots.try_acquire().unwrap();
        assert_eq!(
            recovery.sample_baseline(key, now, true),
            None,
            "an occupied slot without an observation cannot endorse history"
        );
        for (age, phase, wire, expected) in [
            (0, Phase::Reading, 2000, None),
            (2, Phase::MemoryBlocked, 4000, None),
            (2, Phase::Reading, 10, None),
            (2, Phase::Reading, 4000, Some(2000.)),
        ] {
            let mut peer = reading(now - Duration::from_secs(age));
            peer.phase = phase;
            peer.wire = wire;
            recovery
                .state
                .lock()
                .active
                .insert(peer_key, Arc::new(Mutex::new(peer)));
            assert_eq!(recovery.sample_baseline(key, now, true), expected);
        }
        recovery.state.lock().active.remove(&peer_key);
        assert_eq!(
            recovery.sample_baseline(key, now, true),
            None,
            "a peer leaving observation for discard or backoff still occupies a slot"
        );
        drop(peer_permit);
        assert_eq!(recovery.sample_baseline(key, now, true), Some(2000.));
        assert_eq!(
            recovery.sample_baseline(key, now + Duration::from_secs(61), true),
            None
        );
    }
    #[test]
    fn sustained_detection_requires_grace_window_duration_and_benefit() {
        let start = Instant::now();
        let mut observation = reading(start);
        let config = policy();
        for seconds in 0..4 {
            observation.wire = seconds * 10;
            assert!(!observation.should_recover(
                start + Duration::from_secs(seconds),
                Some(2000.),
                &config,
                10000
            ));
        }
        observation.wire = 40;
        assert!(observation.should_recover(
            start + Duration::from_secs(4),
            Some(2000.),
            &config,
            10000
        ));
        observation.forwarded = 9999;
        assert!(!observation.should_recover(
            start + Duration::from_secs(5),
            Some(2000.),
            &config,
            10000
        ));
        observation.forwarded = 0;
        observation.wire = 4000;
        assert!(!observation.should_recover(
            start + Duration::from_secs(6),
            Some(2000.),
            &config,
            10000
        ));
        assert_eq!(observation.slow_since, None);
        observation.wire = 4010;
        assert!(!observation.should_recover(
            start + Duration::from_secs(7),
            Some(2000.),
            &config,
            10000
        ));
    }
    #[test]
    fn long_local_block_clears_stale_window_even_with_regular_monitor_ticks() {
        for phase in [
            Phase::RateLimited,
            Phase::MemoryBlocked,
            Phase::ChannelBlocked,
            Phase::WriterBarrier,
        ] {
            let start = Instant::now();
            let mut observation = reading(start);
            observation.slow_since = Some(Duration::ZERO);
            observation.phase = phase;
            observation.blocked_since = Some(start);
            for tick in 1..=20 {
                assert_eq!(
                    observation.sample(start + Duration::from_millis(tick * 100)),
                    None
                );
            }
            assert_eq!(observation.reading, Duration::ZERO);
            assert_eq!(observation.slow_since, None);
            observation.phase = Phase::Reading;
            observation.blocked_since = None;
            assert_eq!(
                observation.sample(start + Duration::from_millis(2100)),
                None
            );
            assert!(!observation.should_recover(
                start + Duration::from_millis(2200),
                Some(2000.),
                &policy(),
                10000
            ));
        }
    }
    #[test]
    fn relative_samples_expire_require_two_and_suppress_collective_slowdown() {
        let recovery = coordinator(1_000_000);
        let now = Instant::now();
        let key = LeaseKey {
            piece_id: 0,
            lease_id: 1,
        };
        assert_eq!(recovery.baseline(key, now), None);
        recovery.state.lock().history.push_back((now, 1000.));
        assert_eq!(recovery.baseline(key, now), None);
        recovery.state.lock().history.push_back((now, 2000.));
        assert_eq!(recovery.baseline(key, now), Some(2000.));
        let mut peer = reading(now - Duration::from_secs(10));
        peer.window = Duration::from_secs(1);
        peer.wire = 10;
        recovery.state.lock().active.insert(
            LeaseKey {
                piece_id: 1,
                lease_id: 2,
            },
            Arc::new(Mutex::new(peer)),
        );
        assert_eq!(
            recovery.baseline(key, now),
            None,
            "historical peaks cannot restart all slow live requests"
        );
        recovery.state.lock().active.clear();
        assert_eq!(recovery.baseline(key, now + Duration::from_secs(61)), None);
    }
    #[test]
    fn performance_budget_cooldown_backoff_and_lineage_caps_are_shared() {
        let recovery = coordinator(100_000);
        let lineage = lineage();
        let now = Instant::now();
        assert!(!recovery.reserve(1001, &lineage, false, now));
        assert!(recovery.reserve(500, &lineage, false, now));
        assert!(!recovery.reserve(1, &lineage, false, now + Duration::from_secs(9)));
        assert!(recovery.reserve(500, &lineage, true, now + Duration::from_secs(10)));
        assert!(!recovery.reserve(1, &lineage, false, now + Duration::from_secs(20)));
        assert_eq!(recovery.state.lock().consumed_extra, 1000);
        drop(HedgeGuard(&recovery));
        let large = coordinator(u64::MAX);
        assert_eq!(large.extra_limit, MAX_EXTRA);
        let lineage = lineage.clone();
        assert!(!large.reserve(1, &lineage, false, now));
        large.backoff(Duration::from_secs(30));
        assert!(!large.reserve(
            1,
            &super::tests::lineage(),
            false,
            now + Duration::from_secs(10)
        ));
        assert_eq!(coordinator(99).extra_limit, 0);
    }
    #[test]
    fn reclaimed_queue_respects_backoff_that_arrived_during_discard() {
        let recovery = coordinator(100_000);
        let now = Instant::now();
        let mut state = recovery.state.lock();
        state.blocked_until = Some(now + Duration::from_secs(30));
        assert_eq!(
            state.pending_backoff(now),
            None,
            "ordinary work retains its existing policy"
        );
        state.pending.push_back((0, 0, 1000, 1));
        assert_eq!(state.pending_backoff(now), Some(Duration::from_secs(30)));
        assert_eq!(state.pending_backoff(now + Duration::from_secs(31)), None);
        assert_eq!(
            state.pending.len(),
            1,
            "blocked work remains available after its deadline"
        );
    }
    #[tokio::test(start_paused = true)]
    async fn captured_backoff_expiring_before_wait_still_wakes_without_notification() {
        let recovery = coordinator(100_000);
        let now = Instant::now();
        let captured = {
            let mut state = recovery.state.lock();
            state.pending.push_back((0, 0, 1000, 1));
            state.blocked_until = Some(now + Duration::from_secs(1));
            assert!(state.pending_backoff(now).is_some());
            state.blocked_until
        };
        // Model the scheduling gap after the locked assignment decision. A
        // second time-dependent state lookup would now suppress the timer.
        tokio::time::advance(Duration::from_secs(2)).await;
        assert_eq!(
            recovery
                .state
                .lock()
                .pending_backoff(now + Duration::from_secs(2)),
            None
        );
        tokio::time::timeout(Duration::from_millis(1), wait_backoff_deadline(captured))
            .await
            .expect("an already elapsed captured deadline must still wake the idle worker");
        assert!(
            tokio::time::timeout(Duration::from_millis(1), wait_backoff_deadline(None))
                .await
                .is_err()
        );
    }
    #[tokio::test]
    async fn released_slot_is_available_when_idle_worker_is_notified() {
        let recovery = coordinator(1000);
        let permit = recovery.slots.clone().acquire_many_owned(4).await.unwrap();
        let notified = recovery.changed.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        drop(Slot {
            permit: Some(permit),
            owner: &recovery,
        });
        notified.await;
        assert_eq!(recovery.slots.available_permits(), 4);
    }
    #[test]
    fn strong_validator_and_condition_conflict_gate_hedging() {
        for invalid in ["W/\"v1\"", "v1", "\"bad\"quote\"", "\"bad\nvalue\""] {
            assert!(!strong_etag(invalid));
        }
        for valid in ["\"\"", "\"v1\""] {
            assert!(strong_etag(valid));
        }
        let config = DownloadSpec::new("http://example.invalid");
        assert!(Coordinator::new(
            &config
                .clone()
                .slow_transfer_mode(SlowTransferMode::Disabled)
                .request_batch_size(0),
            &meta(None),
            Path::new("file"),
            100
        )
        .is_none());
        assert!(
            Coordinator::new(&config, &meta(Some("W/\"v1\"")), Path::new("file"), 100)
                .unwrap()
                .validator
                .is_none()
        );
        for condition in [
            "IF-MATCH",
            "If-Range",
            "if-none-match",
            "if-modified-since",
            "if-unmodified-since",
        ] {
            let config = config
                .clone()
                .headers([(condition.into(), "custom".into())].into());
            assert!(
                Coordinator::new(&config, &meta(Some("\"v1\"")), Path::new("file"), 100)
                    .unwrap()
                    .validator
                    .is_none()
            );
        }
    }
}
