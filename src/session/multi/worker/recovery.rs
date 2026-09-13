//! Recovery recommendations consume observations but do not own leases, slots,
//! retries, writer barriers, or reservations. The worker applies each advice.
use super::*;

pub(super) enum Advice {
    Continue {
        reason: &'static str,
        baseline: Option<f64>,
    },
    Recover {
        baseline: Option<f64>,
        tail: bool,
        hedge_eligible: bool,
    },
}

pub(super) fn recommend(ctx: &AttemptContext<'_>, challenger_active: bool, now: Instant) -> Advice {
    let baseline = ctx.recovery.baseline(ctx.segment.lease_key(), now);
    let len = ctx.segment.end - ctx.segment.start;
    let rate_limited = matches!(ctx.speed, SpeedLimit::Limited(_));
    let tail_baseline = if !rate_limited
        && ctx
            .recovery
            .tail_eligible(len, ctx.scheduler.lock().has_available())
    {
        ctx.recovery
            .sample_baseline(ctx.segment.lease_key(), now, true)
    } else {
        None
    };
    let (ordinary, tail) = {
        let mut observation = ctx.observation.lock();
        (
            observation.should_recover(now, baseline, &ctx.recovery.policy, len),
            observation.should_recover_tail(now, tail_baseline, &ctx.recovery.policy, len),
        )
    };
    // A user cap couples request rates; local throttling cannot justify
    // speculative traffic. Observations still retain their phase accounting.
    if rate_limited {
        return Advice::Continue {
            reason: "rate_limit",
            baseline,
        };
    }
    if challenger_active {
        return Advice::Continue {
            reason: "challenger_active",
            baseline,
        };
    }
    if !(ordinary || tail) {
        let phase = ctx.observation.lock().phase;
        let reason = if phase != Phase::Reading {
            "not_reading"
        } else if baseline.is_none() && ctx.recovery.policy.absolute.is_none() {
            "no_healthy_baseline"
        } else {
            "slow_threshold_or_benefit"
        };
        return Advice::Continue { reason, baseline };
    }
    Advice::Recover {
        baseline,
        tail,
        hedge_eligible: ctx.request_end == ctx.segment.end
            && ctx.validator.is_some()
            && ctx.recovery.policy.mode == SlowTransferMode::AdaptiveWithHedging
            && len <= MAX_HEDGE
            && !ctx.scheduler.lock().has_available(),
    }
}
