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
    wire: u64,
    forwarded: u64,
    samples: VecDeque<(Duration, u64)>,
    slow_since: Option<Duration>,
    rate: Option<f64>,
    window: Duration,
}
impl Observation {
    fn new(now: Instant, window: Duration) -> Self {
        Self {
            phase: Phase::Headers,
            phase_at: now,
            blocked_since: Some(now),
            reading: Duration::ZERO,
            wire: 0,
            forwarded: 0,
            samples: VecDeque::from([(Duration::ZERO, 0)]),
            slow_since: None,
            rate: None,
            window,
        }
    }
    fn advance(&mut self, now: Instant) {
        let elapsed = now.saturating_duration_since(self.phase_at);
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
        let remaining = len.saturating_sub(self.forwarded);
        // Explicit absolute thresholds can recover without historical peers,
        // but use a finite conservative rate floor for the benefit estimate.
        let healthy = baseline.or(spec.absolute.map(|v| v as f64));
        let Some(healthy) = healthy.filter(|v| *v > 0.) else {
            return false;
        };
        let keep = remaining as f64 / rate.max(1.);
        let replace = len as f64 / healthy + spec.window.as_secs_f64().min(1.);
        remaining > 0 && keep > replace * 2. && keep - replace > spec.window.as_secs_f64().min(2.)
    }
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
    actions: u64,
    hedge: bool,
    last_action: Option<Instant>,
    blocked_until: Option<Instant>,
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
    changed: Notify,
    state: Mutex<State>,
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
        if spec.slow_transfer_mode == SlowTransferMode::Disabled {
            return None;
        }
        let conflict = spec.headers.keys().any(|h| {
            h.eq_ignore_ascii_case("if-match")
                || h.eq_ignore_ascii_case("if-range")
                || h.eq_ignore_ascii_case("if-none-match")
                || h.eq_ignore_ascii_case("if-unmodified-since")
                || h.eq_ignore_ascii_case("if-modified-since")
        });
        let validator = if conflict {
            None
        } else {
            meta.etag
                .as_deref()
                .filter(|v| strong_etag(v))
                .map(str::to_owned)
        };
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
            changed: Notify::new(),
            state: Mutex::new(State {
                active: BTreeMap::new(),
                history: VecDeque::new(),
                recovered: Vec::new(),
                pending: VecDeque::new(),
                reserved: 0,
                actions: 0,
                hedge: false,
                last_action: None,
                blocked_until: None,
            }),
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
            extra_budget_bytes = self.extra_limit,
            recovery_actions = state.actions,
            "adaptive transfer diagnostics"
        );
    }
    fn tick(&self) -> Duration {
        (self.policy.window / 5)
            .min(Duration::from_secs(1))
            .max(Duration::from_nanos(1))
    }
    fn baseline(&self, target: LeaseKey, now: Instant) -> Option<f64> {
        let mut state = self.state.lock();
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
                if let Some(rate) = sample.lock().sample(now) {
                    current.push(rate);
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
            return None;
        }
        rates.sort_by(f64::total_cmp);
        let baseline = rates[rates.len() / 2];
        // Several live requests slowing together supersede historical peaks.
        if !current.is_empty() && current.iter().all(|rate| *rate < baseline * 0.5) {
            return None;
        }
        Some(baseline)
    }
    fn reserve(&self, len: u64, lineage: &SharedLineage, hedge: bool, now: Instant) -> bool {
        let mut state = self.state.lock();
        if state.hedge
            || state.blocked_until.is_some_and(|until| now < until)
            || state
                .last_action
                .is_some_and(|at| now.saturating_duration_since(at) < COOLDOWN)
            || len > self.extra_limit.saturating_sub(state.reserved)
        {
            return false;
        }
        let mut lineage = lineage.lock();
        if lineage.recoveries >= 2 {
            return false;
        }
        lineage.recoveries += 1;
        state.reserved += len;
        state.actions += 1;
        state.hedge = hedge;
        state.last_action = Some(now);
        true
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
            .any(|r| Arc::ptr_eq(&r.lineage, lineage))
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
            .find(|r| Arc::ptr_eq(&r.lineage, lineage))
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
    let validator = (recovery.policy.mode == SlowTransferMode::AdaptiveWithHedging)
        .then_some(recovery.validator.as_deref())
        .flatten();
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
                        scheduler.assign_to_with_split(
                            worker_id,
                            cfg.max_active_leases,
                            cfg.min_segment_size,
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
        loop {
            begin_lease_and_wait(&write_tx, segment.lease_key()).await?;
            let observation = Arc::new(Mutex::new(Observation::new(
                Instant::now(),
                recovery.policy.window,
            )));
            recovery
                .state
                .lock()
                .active
                .insert(segment.lease_key(), observation.clone());
            let response = super::take_matching_probe_response(&first_response, &segment).await;
            let context = AttemptContext {
                worker: &worker,
                cfg,
                recovery,
                scheduler: &scheduler,
                segment: &segment,
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
            let outcome = run_attempt(
                &context,
                response,
                &mut stop,
                &lineage,
            )
            .await;
            // run_attempt's futures have been dropped: no producer can enqueue
            // old generation data after this point.
            recovery.finish_observation(segment.lease_key(), matches!(outcome, Outcome::Complete));
            match outcome {
                Outcome::Complete => {
                    flush_lease_and_wait(&write_tx, segment.lease_key()).await?;
                    if !scheduler.lock().complete(segment.lease_key()) {
                        return Err(DownloadError::Internal("stale adaptive completion".into()));
                    }
                    recovery.completed(&segment, &lineage);
                    recovery.changed.notify_waiters();
                    break;
                }
                Outcome::Recover | Outcome::Staged(_) | Outcome::Failed(_) => {
                    let forwarded = observation.lock().forwarded;
                    if forwarded > 0 {
                        received.fetch_sub(forwarded, Ordering::Relaxed);
                    }
                    if let Err(error) = discard_lease_and_wait(&write_tx, segment.lease_key()).await
                    {
                        scheduler.lock().reclaim(segment.lease_key());
                        return Err(error);
                    }
                    match outcome {
                        Outcome::Recover => {
                            recovery
                                .duplicate
                                .fetch_add(observation.lock().wire, Ordering::Relaxed);
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
                                "slow attempt reclaimed after writer discard acknowledgement"
                            );
                            recovery.changed.notify_waiters();
                            break;
                        }
                        Outcome::Staged(mut staged) => {
                            recovery
                                .duplicate
                                .fetch_sub(segment.end - segment.start, Ordering::Relaxed);
                            recovery
                                .duplicate
                                .fetch_add(observation.lock().wire, Ordering::Relaxed);
                            segment = scheduler
                                .lock()
                                .renew(segment.lease_key(), worker_id)
                                .ok_or_else(|| {
                                    DownloadError::Internal(
                                        "cannot renew hedge winner lease".into(),
                                    )
                                })?;
                            begin_lease_and_wait(&write_tx, segment.lease_key()).await?;
                            staged
                                .copy_to(&segment, &write_tx, &received, &budget, &mut stop)
                                .await?;
                            flush_lease_and_wait(&write_tx, segment.lease_key()).await?;
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
                        Outcome::Failed(error) => {
                            let decision = lineage.lock().retries.decide(error);
                            match decision {
                                RetryDecision::Stop(error) => {
                                    scheduler.lock().reclaim(segment.lease_key());
                                    return Err(error);
                                }
                                RetryDecision::Retry { backoff, error, .. } => {
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
                                }
                            }
                        }
                        Outcome::Complete => unreachable!(),
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
enum Outcome {
    Complete,
    Recover,
    Staged(Staged),
    Failed(DownloadError),
}
async fn run_attempt(
    ctx: &AttemptContext<'_>,
    response: Option<(HttpResponse, ResponseMeta)>,
    stop: &mut watch::Receiver<StopSignal>,
    lineage: &SharedLineage,
) -> Outcome {
    let mut primary_stop = stop.clone();
    let primary = primary(ctx, response, &mut primary_stop);
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
                let now = Instant::now();
                let baseline = ctx.recovery.baseline(ctx.segment.lease_key(), now);
                if ctx.log_level >= LogLevel::Debug {
                    let sample = ctx.observation.lock();
                    log_debug!(ctx.log_level, download_id = ctx.download_id, worker_id = ctx.segment.owner_worker_id,
                        attempt = ctx.segment.attempt, phase = ?sample.phase, wire_bytes = sample.wire,
                        forwarded_bytes = sample.forwarded, reading_ms = sample.reading.as_millis() as u64,
                        baseline_bytes_sec = ?baseline, "adaptive request sample");
                }
                let eligible = ctx.observation.lock().should_recover(now, baseline, &ctx.recovery.policy, ctx.segment.end-ctx.segment.start);
                // A user cap deliberately couples all request rates. Keep
                // observing phases but conservatively suppress speculative
                // performance work while that cap is active.
                if matches!(ctx.speed, SpeedLimit::Limited(_)) {
                    log_debug!(ctx.log_level, download_id = ctx.download_id, "performance recovery suppressed by task speed cap");
                    continue;
                }
                if !eligible || challenger.is_some() { continue; }
                let len = ctx.segment.end-ctx.segment.start;
                let mut slot = None;
                if ctx.validator.is_some() && ctx.recovery.policy.mode == SlowTransferMode::AdaptiveWithHedging && len <= MAX_HEDGE
                    && !ctx.scheduler.lock().has_available() {
                    slot = ctx.recovery.slots.clone().try_acquire_owned().ok();
                }
                let hedge = slot.is_some();
                if !ctx.recovery.reserve(len, lineage, hedge, now) { continue; }
                log_info!(ctx.log_level, download_id = ctx.download_id, worker_id = ctx.segment.owner_worker_id,
                    attempt = ctx.segment.attempt, start = ctx.segment.start, end = ctx.segment.end, hedge,
                    "sustained low reading speed; bounded recovery selected");
                if let Some(permit) = slot {
                    hedge_guard = Some(HedgeGuard(ctx.recovery));
                    challenger = Some(Box::pin(async move {
                        let _slot = Slot { permit: Some(permit), owner: ctx.recovery };
                        stage(ctx).await
                    }));
                } else { return Outcome::Recover; }
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
) -> Result<HttpResponse, CandidateFailure> {
    let (response, meta) = match response {
        Some(response) => response,
        None => {
            ctx.worker
                .send_range(ctx.segment.start, ctx.segment.end - 1)
                .await?
        }
    };
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
    validate_segment_response(
        response.status().as_u16(),
        None,
        &meta,
        ctx.segment,
        ctx.total,
    )?;
    if ctx
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
async fn primary(
    ctx: &AttemptContext<'_>,
    response: Option<(HttpResponse, ResponseMeta)>,
    stop: &mut watch::Receiver<StopSignal>,
) -> Result<(), DownloadError> {
    let mut body = checked_response(ctx, response)
        .await
        .map_err(|failure| failure.error)?
        .into_body();
    let mut wire = 0u64;
    loop {
        ctx.observation.lock().phase(Phase::Reading);
        let chunk = next_data_chunk(&mut body, ctx.cfg.read_timeout).await?;
        let Some(mut data) = chunk else {
            break;
        };
        wire = wire.saturating_add(data.len() as u64);
        ctx.recovery
            .wire
            .fetch_add(data.len() as u64, Ordering::Relaxed);
        ctx.observation.lock().wire = wire;
        if wire > ctx.segment.end - ctx.segment.start {
            return Err(DownloadError::ResumeMismatch(
                "server overran adaptive range".into(),
            ));
        }
        while !data.is_empty() {
            let len = data.len().min(ctx.budget.max_chunk);
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
            let offset = ctx.segment.start + ctx.observation.lock().forwarded;
            slot.send(WriterCommand::Data {
                data: data.split_to(len),
                offset,
                lease_key: Some(ctx.segment.lease_key()),
            });
            permit.forget();
            ctx.observation.lock().forwarded += len as u64;
            ctx.received.fetch_add(len as u64, Ordering::Relaxed);
            if let Some(error) = stop_signal_error(*stop.borrow()) {
                return Err(error);
            }
        }
    }
    ctx.observation.lock().phase(Phase::WriterBarrier);
    if wire != ctx.segment.end - ctx.segment.start {
        return Err(DownloadError::Transport(crate::error::TransportError::new(
            crate::error::TransportErrorKind::Body,
            std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "adaptive range body ended early",
            ),
        )));
    }
    Ok(())
}

struct Staged {
    file: tokio::fs::File,
    _path: tempfile::TempPath,
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
async fn stage(ctx: &AttemptContext<'_>) -> Result<Staged, CandidateFailure> {
    let temporary = tempfile::Builder::new()
        .prefix(".bytehaul-hedge-")
        .tempfile_in(&ctx.recovery.output_dir)?;
    let (file, path) = temporary.into_parts();
    let mut staged = Staged {
        file: tokio::fs::File::from_std(file),
        _path: path,
    };
    let mut body = checked_response(ctx, None).await?.into_body();
    let mut wire = 0u64;
    while let Some(mut data) = next_data_chunk(&mut body, ctx.cfg.read_timeout).await? {
        wire = wire.saturating_add(data.len() as u64);
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
        assert_eq!(recovery.state.lock().reserved, 1000);
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
                .slow_transfer_mode(SlowTransferMode::Disabled),
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
