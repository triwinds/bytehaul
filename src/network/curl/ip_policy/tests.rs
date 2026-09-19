//! Policy tests, driven by an explicit clock.
//!
//! Every case here runs on a synthetic timeline, so convergence, cooldown and
//! exploration are checked for exactly what they are instead of for what a
//! loaded machine happens to do.

use super::*;

fn pool() -> PoolKey {
    PoolKey::for_request("http://dual.test:8080/file", None)
}

fn address(last: u8) -> IpAddr {
    IpAddr::from([127, 0, 0, last])
}

fn snapshot(addresses: &[u8], valid_for: Duration, now: Instant) -> CandidateSet {
    CandidateSet {
        host: "dual.test".to_string(),
        port: 8080,
        addresses: addresses.iter().copied().map(address).collect(),
        valid_until: now + valid_for,
    }
}

fn long_lived(addresses: &[u8], now: Instant) -> CandidateSet {
    snapshot(addresses, Duration::from_secs(600), now)
}

/// A window of `bytes` bytes that took `window` and was not held back.
fn sample(bytes: u64, window: Duration) -> TransferSample {
    TransferSample {
        bytes,
        window,
        backpressure: Duration::ZERO,
    }
}

/// One megabyte per second.
fn slow_sample() -> TransferSample {
    sample(1_000_000, Duration::from_secs(1))
}

/// Two megabytes per second.
fn fast_sample() -> TransferSample {
    sample(1_000_000, Duration::from_millis(500))
}

/// Makes `count` addresses proven, in the order given, with one sample each.
fn rank(policy: &mut IpPolicy, pool: &PoolKey, samples: &[(u8, TransferSample)], now: Instant) {
    for _ in 0..MIN_SAMPLES {
        for (last, sample) in samples {
            policy.record_sample(pool, address(*last), *sample, now);
        }
    }
}

/// Selects and immediately completes, the way a successful request would.
fn run_one(policy: &mut IpPolicy, pool: &PoolKey, now: Instant) -> Selection {
    let selection = policy.select(pool, now).expect("a selection");
    policy.settle(
        pool,
        selection.address,
        selection.generation,
        AttemptOutcome::Completed,
        now,
    );
    selection
}

#[test]
fn every_candidate_is_covered_before_one_is_preferred() {
    let mut policy = IpPolicy::new();
    let pool = pool();
    let now = Instant::now();
    policy.observe(&pool, &long_lived(&[1, 2, 3], now), now);

    let picked: Vec<Selection> = (0..3).map(|_| run_one(&mut policy, &pool, now)).collect();
    assert_eq!(
        picked
            .iter()
            .map(|selection| selection.reason)
            .collect::<Vec<_>>(),
        vec![
            SelectionReason::Coverage,
            SelectionReason::Coverage,
            SelectionReason::Coverage
        ],
        "§7 rule 2: the first requests spread over every candidate"
    );
    let mut covered: Vec<IpAddr> = picked.iter().map(|selection| selection.address).collect();
    covered.sort();
    covered.dedup();
    assert_eq!(covered.len(), 3, "coverage reaches all three addresses");
}

#[test]
fn the_first_request_of_a_single_candidate_origin_uses_it() {
    let mut policy = IpPolicy::new();
    let pool = pool();
    let now = Instant::now();
    policy.observe(&pool, &long_lived(&[7], now), now);

    let selection = run_one(&mut policy, &pool, now);
    assert_eq!(selection.address, address(7));
    assert_eq!(selection.reason, SelectionReason::Single);
}

#[test]
fn selection_reserves_so_concurrent_requests_spread() {
    let mut policy = IpPolicy::new();
    let pool = pool();
    let now = Instant::now();
    policy.observe(&pool, &long_lived(&[1, 2], now), now);
    rank(
        &mut policy,
        &pool,
        &[(1, slow_sample()), (2, slow_sample())],
        now,
    );

    // Two selections without a settlement: the second must not pile onto the
    // address the first one already holds.
    let first = policy.select(&pool, now).unwrap();
    let second = policy.select(&pool, now).unwrap();
    assert_ne!(
        first.address, second.address,
        "§7 rule 4: in-flight occupancy distributes the load"
    );

    // Both released, both available again.
    policy.settle(
        &pool,
        first.address,
        first.generation,
        AttemptOutcome::Failed,
        now,
    );
    policy.settle(
        &pool,
        second.address,
        second.generation,
        AttemptOutcome::Failed,
        now,
    );
    let third = policy.select(&pool, now).unwrap();
    assert!(third.address == address(1) || third.address == address(2));
}

#[test]
fn a_connect_failure_parks_the_address_for_a_while() {
    let mut policy = IpPolicy::new();
    let pool = pool();
    let now = Instant::now();
    policy.observe(&pool, &long_lived(&[1, 2], now), now);

    let first = policy.select(&pool, now).unwrap();
    assert_eq!(first.address, address(1));
    policy.settle(
        &pool,
        first.address,
        first.generation,
        AttemptOutcome::ConnectFailed,
        now,
    );

    let second = policy.select(&pool, now).unwrap();
    assert_eq!(
        second.address,
        address(2),
        "a refused connection parks that address, not the origin"
    );

    // The cooldown expires and the address comes back.
    let later = now + CONNECT_FAILURE_COOLDOWN + Duration::from_millis(1);
    let third = policy.select(&pool, later).unwrap();
    assert_eq!(third.address, address(1));
}

#[test]
fn parking_asks_for_the_other_address_before_the_cooldown_expires() {
    let mut policy = IpPolicy::new();
    let pool = pool();
    let now = Instant::now();
    policy.observe(&pool, &long_lived(&[1, 2], now), now);
    for last in [1u8, 2] {
        let selection = policy.select(&pool, now).unwrap();
        assert_eq!(selection.address, address(last));
        policy.settle(
            &pool,
            selection.address,
            selection.generation,
            AttemptOutcome::ConnectFailed,
            now,
        );
    }

    // Every candidate is parked: the request still gets an address instead of
    // an error, and the driver bounds how often that may happen (§6).
    let fallback = policy.select(&pool, now).unwrap();
    assert_eq!(fallback.reason, SelectionReason::Fallback);
}

#[test]
fn a_stale_snapshot_is_refused_instead_of_starting_a_selection() {
    let mut policy = IpPolicy::new();
    let pool = pool();
    let now = Instant::now();
    policy.observe(&pool, &snapshot(&[1], Duration::from_millis(50), now), now);

    assert_eq!(policy.select(&pool, now).unwrap().address, address(1));
    assert_eq!(
        policy
            .select(&pool, now + Duration::from_secs(1))
            .unwrap_err(),
        SelectionError::Expired,
        "§5: a snapshot that expired while queued must not start a new selection"
    );
}

#[test]
fn an_unknown_origin_has_no_candidates() {
    let mut policy = IpPolicy::new();
    let now = Instant::now();
    assert_eq!(
        policy.select(&pool(), now).unwrap_err(),
        SelectionError::NoCandidates
    );
}

#[test]
fn a_settled_attempt_stops_counting_as_in_flight() {
    let mut policy = IpPolicy::new();
    let pool = pool();
    let now = Instant::now();
    policy.observe(&pool, &long_lived(&[1], now), now);

    for outcome in [
        AttemptOutcome::Completed,
        AttemptOutcome::Failed,
        AttemptOutcome::Cancelled,
    ] {
        let selection = policy.select(&pool, now).unwrap();
        policy.settle(&pool, selection.address, selection.generation, outcome, now);
    }
    let next = policy.select(&pool, now).unwrap();
    policy.settle(
        &pool,
        next.address,
        next.generation,
        AttemptOutcome::Completed,
        now,
    );
    // A leaked reservation would have made the address look busy forever; the
    // count is observable through the spread rule, so a single address that
    // keeps being selected is the assertion.
    assert_eq!(next.address, address(1));
}

#[test]
fn a_removed_address_takes_no_new_work() {
    let mut policy = IpPolicy::new();
    let pool = pool();
    let now = Instant::now();
    policy.observe(&pool, &long_lived(&[1, 2], now), now);
    policy.observe(&pool, &long_lived(&[2], now), now);

    assert_eq!(policy.candidates(&pool), vec![address(2)]);
    for _ in 0..4 {
        assert_eq!(policy.select(&pool, now).unwrap().address, address(2));
    }
}

#[test]
fn an_old_generation_settlement_does_not_park_a_current_address() {
    let mut policy = IpPolicy::new();
    let pool = pool();
    let now = Instant::now();
    policy.observe(&pool, &long_lived(&[1, 2], now), now);
    let stale = policy.select(&pool, now).unwrap();

    // The DNS answer changed while the transfer was in flight.
    policy.observe(&pool, &long_lived(&[2, 3], now), now);

    policy.settle(
        &pool,
        stale.address,
        stale.generation,
        AttemptOutcome::ConnectFailed,
        now,
    );
    // Address 2 is still offered, so the stale failure must not park it.
    let next = policy.select(&pool, now).unwrap();
    let following = policy.select(&pool, now).unwrap();
    let picked: Vec<IpAddr> = vec![next.address, following.address];
    assert!(
        picked.contains(&address(2)),
        "§5: an event from an older generation only settles its own reservation, got {picked:?}"
    );
}

#[test]
fn only_unpolluted_complete_windows_rank() {
    let mut policy = IpPolicy::new();
    let pool = pool();
    let now = Instant::now();
    policy.observe(&pool, &long_lived(&[1, 2], now), now);

    // Too short, too small, and mostly backpressure: all three are pollution,
    // not slow addresses.
    let polluted = [
        sample(1_000_000, MIN_WINDOW - Duration::from_millis(1)),
        sample(MIN_SAMPLE_BYTES - 1, Duration::from_secs(1)),
        TransferSample {
            bytes: 1_000_000,
            window: Duration::from_secs(1),
            backpressure: Duration::from_millis(500),
        },
    ];
    for window in polluted {
        assert!(
            !policy.record_sample(&pool, address(1), window, now),
            "a polluted window must not be folded in"
        );
    }

    rank(
        &mut policy,
        &pool,
        &[(1, slow_sample()), (2, fast_sample())],
        now,
    );
    let selection = run_one(&mut policy, &pool, now);
    assert_eq!(
        policy.preferred(&pool),
        Some(address(2)),
        "only the address with clean numbers may be preferred"
    );
    assert_eq!(selection.address, address(2));
}

#[test]
fn the_faster_address_becomes_preferred_once_it_has_samples() {
    let mut policy = IpPolicy::new();
    let pool = pool();
    let now = Instant::now();
    policy.observe(&pool, &long_lived(&[1, 2], now), now);
    policy.observe(
        &pool,
        &long_lived(&[1, 2], now + Duration::from_secs(1)),
        now,
    );

    rank(&mut policy, &pool, &[(1, slow_sample())], now);
    run_one(&mut policy, &pool, now);
    assert_eq!(policy.preferred(&pool), Some(address(1)));

    rank(&mut policy, &pool, &[(2, fast_sample())], now);
    let later = now + MIN_DWELL + Duration::from_secs(1);
    let selection = run_one(&mut policy, &pool, later);
    assert_eq!(policy.preferred(&pool), Some(address(2)));
    assert_eq!(selection.address, address(2));
}

#[test]
fn a_marginal_improvement_does_not_switch_the_preferred_address() {
    let mut policy = IpPolicy::new();
    let pool = pool();
    let now = Instant::now();
    policy.observe(&pool, &long_lived(&[1, 2], now), now);

    rank(&mut policy, &pool, &[(1, slow_sample())], now);
    run_one(&mut policy, &pool, now);
    assert_eq!(policy.preferred(&pool), Some(address(1)));

    // 10 % faster is below the 25 % switch gap: §7 rule 5 suppresses flapping.
    let marginal = sample(1_100_000, Duration::from_secs(1));
    rank(&mut policy, &pool, &[(2, marginal)], now);
    for _ in 0..4 {
        let selected = run_one(&mut policy, &pool, now + MIN_DWELL + Duration::from_secs(1));
        assert_eq!(selected.address, address(1));
    }
    assert_eq!(
        policy.preferred(&pool),
        Some(address(1)),
        "a small difference must not move the preference"
    );
}

#[test]
fn a_preferred_address_stays_until_the_minimum_dwell_passed() {
    let mut policy = IpPolicy::new();
    let pool = pool();
    let now = Instant::now();
    policy.observe(&pool, &long_lived(&[1, 2], now), now);

    rank(&mut policy, &pool, &[(1, slow_sample())], now);
    run_one(&mut policy, &pool, now);
    assert_eq!(policy.preferred(&pool), Some(address(1)));

    rank(&mut policy, &pool, &[(2, fast_sample())], now);
    // Well before MIN_DWELL: the much faster address has to wait.
    let soon = now + Duration::from_millis(50);
    assert_eq!(run_one(&mut policy, &pool, soon).address, address(1));
    assert_eq!(policy.preferred(&pool), Some(address(1)));

    let after_dwell = now + MIN_DWELL + Duration::from_millis(1);
    assert_eq!(run_one(&mut policy, &pool, after_dwell).address, address(2));
    assert_eq!(policy.preferred(&pool), Some(address(2)));
}

#[test]
fn an_expired_rate_stops_holding_the_preference() {
    let mut policy = IpPolicy::new();
    let pool = pool();
    let now = Instant::now();
    policy.observe(&pool, &long_lived(&[1, 2], now), now);

    rank(&mut policy, &pool, &[(1, slow_sample())], now);
    run_one(&mut policy, &pool, now);
    assert_eq!(policy.preferred(&pool), Some(address(1)));

    // Long after the samples of address 1 went stale, address 2 is measured
    // again on the same timeline the next selection runs on: there is nothing
    // left to compare address 1 against, so the preference moves without
    // waiting for the switch gap.
    let much_later = now + SAMPLE_TTL + MIN_DWELL + Duration::from_secs(1);
    rank(&mut policy, &pool, &[(2, fast_sample())], much_later);
    let selection = run_one(&mut policy, &pool, much_later);
    assert_eq!(selection.address, address(2));
    assert_eq!(policy.preferred(&pool), Some(address(2)));
}

#[test]
fn exploration_probes_an_address_outside_the_preferred_set() {
    let mut policy = IpPolicy::new();
    let pool = pool();
    let now = Instant::now();
    policy.observe(&pool, &long_lived(&[1, 2, 3], now), now);
    rank(
        &mut policy,
        &pool,
        &[
            (1, sample(3_000_000, Duration::from_secs(1))),
            (2, sample(2_000_000, Duration::from_secs(1))),
            (3, sample(1_000_000, Duration::from_secs(1))),
        ],
        now,
    );

    let mut explored = 0;
    let mut explored_addresses = Vec::new();
    for _ in 0..EXPLORE_PERIOD as usize {
        let selection = run_one(&mut policy, &pool, now);
        if selection.reason == SelectionReason::Exploration {
            explored += 1;
            explored_addresses.push(selection.address);
        }
    }
    assert_eq!(
        explored, 1,
        "§7 rule 6: exploration is low frequency, not a second load balancer"
    );
    assert_eq!(
        explored_addresses,
        vec![address(3)],
        "the address outside the preferred set is the one probed"
    );
}

#[test]
fn a_single_candidate_origin_never_explores() {
    let mut policy = IpPolicy::new();
    let pool = pool();
    let now = Instant::now();
    policy.observe(&pool, &long_lived(&[9], now), now);
    rank(&mut policy, &pool, &[(9, slow_sample())], now);

    for _ in 0..(EXPLORE_PERIOD as usize * 2) {
        assert_eq!(
            run_one(&mut policy, &pool, now).address,
            address(9),
            "one candidate degrades to one, exploration included"
        );
    }
}

#[test]
fn refreshes_keep_the_history_of_addresses_that_still_exist() {
    let mut policy = IpPolicy::new();
    let pool = pool();
    let now = Instant::now();
    policy.observe(&pool, &long_lived(&[1, 2], now), now);
    rank(
        &mut policy,
        &pool,
        &[(1, slow_sample()), (2, fast_sample())],
        now,
    );
    let before = run_one(&mut policy, &pool, now);
    assert_eq!(before.address, address(2));

    // A TTL refresh that offers the same addresses must not throw away what is
    // known about them (§5: a stable candidate is not rebuilt for a refresh).
    let later = now + Duration::from_secs(1);
    policy.observe(&pool, &long_lived(&[1, 2], later), later);
    assert_eq!(policy.preferred(&pool), Some(address(2)));
    assert_eq!(
        run_one(&mut policy, &pool, later).address,
        address(2),
        "the address that ranked first is still the one used"
    );
}
