//! Per-address candidate policy for direct libcurl transfers.
//!
//! `docs/multi-ip-connection-plan.zh-CN.md` §5-§7 describes the design this
//! module implements: every direct request of one origin carries a *candidate
//! snapshot* (the addresses Hickory just returned, their absolute validity
//! deadline, and the request identity), the driver picks one address for it and
//! states that choice as a `CURLOPT_CONNECT_TO` entry, and the outcome of the
//! transfer feeds back into the ranking of that address.
//!
//! The policy itself is deliberately free of libcurl and of Tokio: it is a
//! state machine over `IpAddr` values that takes an explicit `now` so its
//! behaviour is testable with a controlled clock. `src/network/curl/driver`
//! owns the state, calls it from the driver thread only, and turns its
//! decisions into libcurl options.
//!
//! Two rules shape the whole module:
//!
//! * **Nothing here creates traffic.** Candidates are covered by the requests
//!   the session already wants to run; there is no probe request, no extra
//!   range, and no pre-warming (§1, §7).
//! * **Only unpolluted, completely consumed transfers rank.** A window that a
//!   local constraint (rate limit, memory wait, write backpressure) stretched
//!   says nothing about the address, and a transfer whose body the consumer
//!   abandoned is not a success sample at all (§7).

use std::collections::HashMap;
use std::net::IpAddr;
use std::time::{Duration, Instant};

use super::driver::PoolKey;

/// How many addresses the policy keeps as "preferred" once it has numbers.
///
/// §7: later requests should mainly use the first two IPs; a single candidate
/// degrades to one naturally.
const PREFERRED_SET: usize = 2;

/// Samples one address needs before its rate is allowed to reorder anything.
///
/// §7 rule 3: ranking starts only after several stable samples.
const MIN_SAMPLES: u32 = 3;

/// Shortest body window that may become a sample. Below this the transfer is
/// dominated by connection and response-head time.
const MIN_WINDOW: Duration = Duration::from_millis(200);

/// Fewest body bytes that may become a sample, so a tiny response cannot
/// produce a very high rate from rounding.
const MIN_SAMPLE_BYTES: u64 = 64 * 1024;

/// Largest share of a window that backpressure may occupy before the window is
/// discarded. §7: windows polluted by local constraints are dropped rather
/// than adjusted, and pause *counts* alone cannot tell pollution apart.
const MAX_BACKPRESSURE_RATIO: f64 = 0.10;

/// Weight of the newest sample in the per-address rate.
const EWMA_ALPHA: f64 = 0.3;

/// How long a rate stays meaningful. §7 rule 6: expired samples make an
/// address explorable again instead of trusted forever.
const SAMPLE_TTL: Duration = Duration::from_secs(60);

/// Half-life of the decay applied to a rate between updates.
const SAMPLE_HALF_LIFE: Duration = Duration::from_secs(30);

/// How much faster another address has to be before the preferred one changes.
/// §7 rule 5: a gap threshold suppresses flapping between near-equal addresses.
const SWITCH_GAP: f64 = 0.25;

/// How long the preferred address is kept before it may be replaced. Together
/// with [`SWITCH_GAP`] this is the hysteresis of §7 rule 5.
const MIN_DWELL: Duration = Duration::from_secs(5);

/// Every Nth selection probes an address outside the preferred set, when one
/// is worth probing. A counter instead of randomness keeps the driver
/// deterministic, which is what makes §7's convergence testable.
const EXPLORE_PERIOD: u64 = 16;

/// How long a connect failure parks an address. The address is not blamed for
/// a server-side 429/503 or a TLS verification failure - those are handled as
/// their own outcomes and never rotate to another address (§6).
const CONNECT_FAILURE_COOLDOWN: Duration = Duration::from_secs(3);

/// How long the history of an address that left the DNS answer is kept. §5:
/// TTL updates keep short-term history for addresses that still exist; a
/// removed address takes no new work.
const REMOVED_HISTORY: Duration = Duration::from_secs(120);

/// Result of one attempt against one address, as the driver saw it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AttemptOutcome {
    /// The transfer finished and libcurl returned the connection to the cache.
    Completed,
    /// The transfer failed after the connection was established.
    Failed,
    /// The connection could not be established at all, and no HTTP request can
    /// have reached the origin. Only this outcome parks the address, and only
    /// this one makes an internal fallback safe (§6).
    ConnectFailed,
    /// The caller aborted the transfer. Never a statement about the address.
    Cancelled,
}

/// One body window observed for a transfer, in the terms §7 requires.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TransferSample {
    /// Body bytes the write callback accepted.
    pub bytes: u64,
    /// Monotonic time from the first accepted byte to the last one, so waiting
    /// for remote data is included and only local stalls are visible as
    /// backpressure.
    pub window: Duration,
    /// Time inside the window the transfer spent paused because the consumer
    /// was behind.
    pub backpressure: Duration,
}

/// Why one address was chosen; §9 requires the reason in TRACE logs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SelectionReason {
    /// The origin has a single candidate, so there is nothing to choose.
    Single,
    /// The address had not been tried yet: §7 rule 2 covers every candidate
    /// before it starts preferring one.
    Coverage,
    /// A proven address from the preferred set.
    Preferred,
    /// A low-frequency probe of an address outside the preferred set.
    Exploration,
    /// Every candidate was parked or removed; this is a bounded fallback.
    Fallback,
}

impl SelectionReason {
    /// Stable label for logs and tests.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Single => "single",
            Self::Coverage => "coverage",
            Self::Preferred => "preferred",
            Self::Exploration => "exploration",
            Self::Fallback => "fallback",
        }
    }
}

/// One reserved address for one transfer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Selection {
    pub address: IpAddr,
    pub generation: u64,
    pub reason: SelectionReason,
}

/// Why a snapshot could not produce a selection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SelectionError {
    /// The snapshot's validity deadline had passed before the transfer
    /// started; §5 forbids starting a fresh selection from a stale snapshot.
    Expired,
    /// The answer no longer holds any address.
    NoCandidates,
}

/// The candidate addresses of one direct hop, as the transport resolved them.
///
/// Built on the Tokio side from one Hickory answer. `addresses` keeps the
/// resolver's order, which is what makes the first candidate of a fresh origin
/// deterministic.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CandidateSet {
    /// URL host the candidates belong to, for the `CONNECT_TO` entry.
    pub host: String,
    /// Port the URL asks for, also the default connect-to port.
    pub port: u16,
    /// Addresses of this answer, in resolver order.
    pub addresses: Vec<IpAddr>,
    /// Absolute instant this answer may no longer be used.
    pub valid_until: Instant,
}

/// What the policy knows about one address.
#[derive(Debug)]
struct Candidate {
    address: IpAddr,
    /// DNS generation this address was last part of. A completion event that
    /// arrives with an older generation only settles its own reservation (§5).
    generation: u64,
    /// Whether the address is in the current DNS answer. A removed address
    /// takes no new work.
    present: bool,
    /// When the address was last seen in an answer, for history expiry.
    last_seen: Instant,
    /// Transfers assigned to it that have not settled yet.
    in_flight: usize,
    /// Transfers ever assigned to it, for the coverage rule.
    attempts: u64,
    /// No new assignment before this instant.
    cooldown_until: Option<Instant>,
    /// Smoothed effective rate in bytes/second over unpolluted windows.
    rate: Option<f64>,
    /// Samples folded into `rate`.
    samples: u32,
    /// Windows dropped because a local constraint polluted them.
    polluted: u32,
    /// When `rate` was last updated, for decay and expiry.
    updated_at: Option<Instant>,
    successes: u64,
    failures: u64,
}

impl Candidate {
    fn new(address: IpAddr, generation: u64, now: Instant) -> Self {
        Self {
            address,
            generation,
            present: true,
            last_seen: now,
            in_flight: 0,
            attempts: 0,
            cooldown_until: None,
            rate: None,
            samples: 0,
            polluted: 0,
            updated_at: None,
            successes: 0,
            failures: 0,
        }
    }

    /// Whether a new transfer may be assigned to this address at `now`.
    fn is_available(&self, now: Instant) -> bool {
        self.present && self.cooldown_until.is_none_or(|until| now >= until)
    }

    /// The rate this address is worth at `now`, with the decay of §7 rule 5.
    ///
    /// A rate that decayed below the sample TTL is treated as absent, which
    /// makes the address explorable again instead of trusted forever.
    fn effective_rate(&self, now: Instant) -> Option<f64> {
        let rate = self.rate?;
        let updated_at = self.updated_at?;
        let age = now.saturating_duration_since(updated_at);
        if age >= SAMPLE_TTL {
            return None;
        }
        let halvings = age.as_secs_f64() / SAMPLE_HALF_LIFE.as_secs_f64();
        Some(rate * 0.5_f64.powf(halvings))
    }

    /// Whether this address has enough samples to be ranked.
    fn is_proven(&self) -> bool {
        self.samples >= MIN_SAMPLES
    }
}

/// The policy state of one origin.
#[derive(Debug)]
struct Origin {
    /// Bumped whenever the candidate set changes. Claims and completion events
    /// carry the generation they were made in.
    generation: u64,
    /// When the current answer stops being valid.
    valid_until: Instant,
    /// Candidates in the order the last answer listed them.
    candidates: Vec<Candidate>,
    /// Address the policy currently prefers, and since when. Together these
    /// are the hysteresis that suppresses switching (§7 rule 5).
    preferred: Option<IpAddr>,
    preferred_since: Option<Instant>,
}

impl Origin {
    fn new() -> Self {
        Self {
            generation: 0,
            valid_until: Instant::now(),
            candidates: Vec::new(),
            preferred: None,
            preferred_since: None,
        }
    }

    fn candidate(&self, address: IpAddr) -> Option<&Candidate> {
        self.candidates
            .iter()
            .find(|candidate| candidate.address == address)
    }

    fn candidate_mut(&mut self, address: IpAddr) -> Option<&mut Candidate> {
        self.candidates
            .iter_mut()
            .find(|candidate| candidate.address == address)
    }

    /// Folds one DNS answer in.
    ///
    /// Addresses that are still in the answer keep their history, including
    /// their rate and their in-flight reservations; §5: a TTL refresh must not
    /// throw away what is known about an address, and must not rebuild a
    /// connection just because the answer was refreshed.
    fn observe(&mut self, set: &CandidateSet, now: Instant) {
        let offered: Vec<IpAddr> = self
            .candidates
            .iter()
            .filter(|candidate| candidate.present)
            .map(|candidate| candidate.address)
            .collect();
        let same_set = offered.len() == set.addresses.len()
            && offered
                .iter()
                .all(|address| set.addresses.contains(address));
        if !same_set {
            self.generation += 1;
        }
        self.valid_until = set.valid_until;

        for candidate in &mut self.candidates {
            candidate.present = false;
        }
        let generation = self.generation;
        for address in &set.addresses {
            match self.candidate_mut(*address) {
                Some(candidate) => {
                    candidate.present = true;
                    candidate.generation = generation;
                    candidate.last_seen = now;
                }
                None => self
                    .candidates
                    .push(Candidate::new(*address, generation, now)),
            }
        }
        // The answer's order is the order candidates are covered in.
        self.candidates.sort_by_key(|candidate| {
            set.addresses
                .iter()
                .position(|address| *address == candidate.address)
                .unwrap_or(usize::MAX)
        });
        self.candidates.retain(|candidate| {
            candidate.present
                || candidate.in_flight > 0
                || now.saturating_duration_since(candidate.last_seen) < REMOVED_HISTORY
        });
        if self
            .preferred
            .is_some_and(|address| !set.addresses.contains(&address))
        {
            // A removed address stops taking work immediately, so the
            // preference cannot outlive it.
            self.preferred = None;
            self.preferred_since = None;
        }
    }
}

/// Which address one request should use, and why.
pub(crate) struct IpPolicy {
    origins: HashMap<PoolKey, Origin>,
    /// Counts selections so exploration does not need randomness.
    selections: u64,
    /// Windows this policy rejected; reported through the driver's counters.
    polluted_windows: u64,
}

impl Default for IpPolicy {
    fn default() -> Self {
        Self::new()
    }
}

impl IpPolicy {
    pub(crate) fn new() -> Self {
        Self {
            origins: HashMap::new(),
            selections: 0,
            polluted_windows: 0,
        }
    }

    /// Folds one DNS answer into the origin's candidate table.
    pub(crate) fn observe(&mut self, pool: &PoolKey, set: &CandidateSet, now: Instant) {
        self.origins
            .entry(pool.clone())
            .or_insert_with(Origin::new)
            .observe(set, now);
    }

    /// Reserves one address for a transfer of `pool`.
    ///
    /// A successful reservation is what keeps the address counted as busy, so
    /// §5's atomic "select and reserve" step is this single call: two requests
    /// can never both take the last free candidate by accident.
    pub(crate) fn select(
        &mut self,
        pool: &PoolKey,
        now: Instant,
    ) -> Result<Selection, SelectionError> {
        let origin = self
            .origins
            .get_mut(pool)
            .ok_or(SelectionError::NoCandidates)?;
        if !origin.candidates.iter().any(|candidate| candidate.present) {
            return Err(SelectionError::NoCandidates);
        }
        if now >= origin.valid_until {
            return Err(SelectionError::Expired);
        }
        self.selections += 1;
        let reason = {
            let exploration_due = self.selections.is_multiple_of(EXPLORE_PERIOD);
            let origin = self.origins.get_mut(pool).expect("origin checked above");
            origin.update_preferred(now);
            origin
                .pick(now, exploration_due)
                .ok_or(SelectionError::NoCandidates)?
        };
        let origin = self.origins.get_mut(pool).expect("origin checked above");
        let candidate = origin
            .candidate_mut(reason.address)
            .expect("the chosen candidate is in the table");
        candidate.in_flight += 1;
        candidate.attempts += 1;
        Ok(Selection {
            address: reason.address,
            generation: candidate.generation,
            reason: reason.reason,
        })
    }

    /// Reports how one reserved attempt ended.
    ///
    /// `generation` is the one the reservation was made in: an event from an
    /// older generation only releases its own reservation and never revives a
    /// candidate the current answer dropped (§5).
    pub(crate) fn settle(
        &mut self,
        pool: &PoolKey,
        address: IpAddr,
        generation: u64,
        outcome: AttemptOutcome,
        now: Instant,
    ) {
        let Some(origin) = self.origins.get_mut(pool) else {
            return;
        };
        // Read before the candidate is borrowed mutably: the gate compares the
        // event's generation with the origin's current one.
        let current_generation = origin.generation;
        let Some(candidate) = origin.candidate_mut(address) else {
            return;
        };
        candidate.in_flight = candidate.in_flight.saturating_sub(1);
        match outcome {
            AttemptOutcome::ConnectFailed => {
                if generation == current_generation {
                    candidate.cooldown_until = Some(now + CONNECT_FAILURE_COOLDOWN);
                }
                candidate.failures += 1;
            }
            AttemptOutcome::Failed => candidate.failures += 1,
            AttemptOutcome::Completed => candidate.successes += 1,
            AttemptOutcome::Cancelled => {}
        }
    }

    /// Folds one completely consumed, unpolluted body window into an address.
    ///
    /// Windows that fail §7's minimums are counted as pollution instead of
    /// being adjusted: a rate that is only right after subtracting local stall
    /// time is a made-up number. Returns whether the window was usable.
    pub(crate) fn record_sample(
        &mut self,
        pool: &PoolKey,
        address: IpAddr,
        sample: TransferSample,
        now: Instant,
    ) -> bool {
        let Some(origin) = self.origins.get_mut(pool) else {
            return false;
        };
        let Some(candidate) = origin.candidate_mut(address) else {
            return false;
        };
        if !sample_is_usable(&sample) {
            candidate.polluted += 1;
            self.polluted_windows += 1;
            return false;
        }
        let rate = sample.bytes as f64 / sample.window.as_secs_f64();
        candidate.rate = Some(match candidate.effective_rate(now) {
            Some(previous) => EWMA_ALPHA * rate + (1.0 - EWMA_ALPHA) * previous,
            None => rate,
        });
        candidate.samples += 1;
        candidate.updated_at = Some(now);
        true
    }

    /// Candidate addresses of one origin, in coverage order. Diagnostics only.
    #[cfg(test)]
    pub(crate) fn candidates(&self, pool: &PoolKey) -> Vec<IpAddr> {
        self.origins
            .get(pool)
            .map(|origin| {
                origin
                    .candidates
                    .iter()
                    .filter(|candidate| candidate.present)
                    .map(|candidate| candidate.address)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The address the policy currently prefers, if any. Diagnostics only.
    #[cfg(test)]
    pub(crate) fn preferred(&self, pool: &PoolKey) -> Option<IpAddr> {
        self.origins.get(pool).and_then(|origin| origin.preferred)
    }
}

/// A window is usable when it is long enough, big enough, and was not mostly
/// spent waiting on the local consumer.
fn sample_is_usable(sample: &TransferSample) -> bool {
    if sample.window < MIN_WINDOW || sample.bytes < MIN_SAMPLE_BYTES {
        return false;
    }
    let polluting = sample.backpressure.as_secs_f64();
    polluting <= sample.window.as_secs_f64() * MAX_BACKPRESSURE_RATIO
}

/// One pick, before the reservation is written back.
struct Pick {
    address: IpAddr,
    reason: SelectionReason,
}

impl Origin {
    /// Keeps the preferred address in step with the current rates.
    ///
    /// §7 rules 3-5: only proven addresses rank, the preferred one only moves
    /// after [`MIN_DWELL`] and only for a [`SWITCH_GAP`] improvement, and a
    /// preferred address that is no longer offered is dropped at once.
    fn update_preferred(&mut self, now: Instant) {
        let mut ranked: Vec<(IpAddr, f64)> = self
            .candidates
            .iter()
            .filter(|candidate| candidate.present && candidate.is_proven())
            .filter_map(|candidate| {
                candidate
                    .effective_rate(now)
                    .map(|rate| (candidate.address, rate))
            })
            .collect();
        ranked.sort_by(|left, right| right.1.total_cmp(&left.1));
        let Some((best, best_rate)) = ranked.first().copied() else {
            // Nothing proven: the coverage rule decides, and there is nothing
            // to prefer yet.
            self.preferred = None;
            self.preferred_since = None;
            return;
        };
        match self.preferred {
            None => {
                self.preferred = Some(best);
                self.preferred_since = Some(now);
            }
            Some(current) if current == best => {}
            Some(current) => {
                let current_rate = self
                    .candidate(current)
                    .and_then(|candidate| candidate.effective_rate(now));
                let settled = self
                    .preferred_since
                    .is_none_or(|since| now.saturating_duration_since(since) >= MIN_DWELL);
                // A preferred address whose rate expired may be replaced
                // without waiting for the gap: there is nothing to compare.
                let clearly_better =
                    current_rate.is_none_or(|rate| best_rate > rate * (1.0 + SWITCH_GAP));
                if settled && clearly_better {
                    self.preferred = Some(best);
                    self.preferred_since = Some(now);
                }
            }
        }
    }

    /// Chooses one candidate, applying §7 rule 2 (coverage), rule 4 (the
    /// preferred set, spread by in-flight occupancy, never waiting for it),
    /// rule 6 (low-frequency exploration) and the bounded fallback.
    fn pick(&self, now: Instant, exploration_due: bool) -> Option<Pick> {
        let present = self
            .candidates
            .iter()
            .filter(|candidate| candidate.present)
            .count();
        if present == 0 {
            return None;
        }

        if present == 1 {
            let address = self
                .candidates
                .iter()
                .find(|candidate| candidate.present)
                .map(|candidate| candidate.address)?;
            return Some(Pick {
                address,
                reason: SelectionReason::Single,
            });
        }
        // "Untried" is about knowing nothing, so an address that only ever
        // produced samples counts as tried too: there is nothing left to learn
        // by covering it again.
        let untried = self
            .candidates
            .iter()
            .find(|candidate| {
                candidate.present && candidate.attempts == 0 && candidate.samples == 0
            })
            .map(|candidate| candidate.address);
        if let Some(address) = untried {
            return Some(Pick {
                address,
                reason: SelectionReason::Coverage,
            });
        }

        // Before anything is proven there is no set to prefer, so every
        // available candidate is still eligible; the spread below is what
        // keeps those requests from piling onto one address.
        let mut eligible: Vec<IpAddr> = self
            .preferred_set(now)
            .into_iter()
            .filter(|address| {
                self.candidate(*address)
                    .is_some_and(|candidate| candidate.is_available(now))
            })
            .collect();
        if eligible.is_empty() {
            eligible = self
                .candidates
                .iter()
                .filter(|candidate| candidate.is_available(now))
                .map(|candidate| candidate.address)
                .collect();
        }
        if !eligible.is_empty() {
            if exploration_due {
                if let Some(address) = self.explore_candidate(now, &eligible) {
                    return Some(Pick {
                        address,
                        reason: SelectionReason::Exploration,
                    });
                }
            }
            // Spread over the eligible set by occupancy instead of queueing
            // behind one address (§7 rule 4: never starve other requests
            // waiting for the preferred one to become free).
            let address = eligible
                .into_iter()
                .min_by_key(|address| {
                    self.candidate(*address)
                        .map(|candidate| candidate.in_flight)
                        .unwrap_or(usize::MAX)
                })
                .expect("the eligible set is not empty");
            return Some(Pick {
                address,
                reason: SelectionReason::Preferred,
            });
        }

        // Every candidate is parked or unproven and parked addresses are left:
        // a bounded fallback keeps the request alive instead of failing it,
        // and the driver counts these attempts (§6).
        let fallback = self
            .candidates
            .iter()
            .filter(|candidate| candidate.present)
            .min_by_key(|candidate| {
                // The candidate that becomes usable soonest is the one worth a
                // bounded probe.
                candidate
                    .cooldown_until
                    .map(|until| until.saturating_duration_since(now))
                    .unwrap_or_default()
            })
            .map(|candidate| candidate.address)?;
        Some(Pick {
            address: fallback,
            reason: SelectionReason::Fallback,
        })
    }

    /// The addresses §7 rule 4 prefers: the fastest ranked ones, with the
    /// current preferred address pinned in even while hysteresis holds it back.
    fn preferred_set(&self, now: Instant) -> Vec<IpAddr> {
        let mut ranked: Vec<(IpAddr, f64)> = self
            .candidates
            .iter()
            .filter(|candidate| candidate.present && candidate.is_proven())
            .filter_map(|candidate| {
                candidate
                    .effective_rate(now)
                    .map(|rate| (candidate.address, rate))
            })
            .collect();
        ranked.sort_by(|left, right| right.1.total_cmp(&left.1));
        let mut set: Vec<IpAddr> = ranked
            .into_iter()
            .map(|(address, _)| address)
            .take(PREFERRED_SET)
            .collect();
        if let Some(preferred) = self.preferred {
            // Occupancy ties are resolved in list order. Keep the incumbent
            // first even when it is already ranked second, otherwise a small
            // rate change bypasses update_preferred's gap and dwell gates.
            set.retain(|address| *address != preferred);
            set.insert(0, preferred);
            set.truncate(PREFERRED_SET);
        }
        set
    }

    /// One address outside the preferred set that is worth a probe.
    ///
    /// §7 rule 6: exploration reuses a normal request rather than adding one,
    /// and an unproven address is always worth probing until it has samples.
    fn explore_candidate(&self, now: Instant, preferred: &[IpAddr]) -> Option<IpAddr> {
        self.candidates
            .iter()
            .filter(|candidate| {
                candidate.present
                    && !preferred.contains(&candidate.address)
                    && candidate.is_available(now)
            })
            .max_by_key(|candidate| {
                // Probe what is least known first: no samples at all beats a
                // stale rate, which is already handled by the expiry rule.
                match (candidate.samples, candidate.effective_rate(now)) {
                    (0, _) => 2,
                    (_, None) => 1,
                    (_, Some(_)) => 0,
                }
            })
            .map(|candidate| candidate.address)
    }
}

#[cfg(test)]
mod tests;
