use crate::config::PollAggression;
use crate::github::RateLimit;

/// Fastest permitted polling interval when active runs exist.
pub const MIN_ACTIVE_SECS: u64 = 5;
/// Fastest permitted polling interval when no active runs exist.
pub const MIN_IDLE_SECS: u64 = 15;

/// All inputs needed to compute poll intervals.
pub struct PollInput {
    pub rate_limit: Option<RateLimit>,
    pub calls_per_cycle: u64,
    pub now: u64,
    pub aggression: PollAggression,
}

/// Compute active and idle poll intervals.
///
/// Uses `time_left` (seconds until reset), `limit` (total calls per window),
/// `used` (calls consumed so far), and `aggression` (target fraction).
///
/// ```text
/// remaining_budget = (target_fraction × limit) − used
/// interval = time_left × calls_per_cycle / remaining_budget
/// ```
///
/// Target fractions: High = 80%, Medium = 40%, Low = 15%.
pub fn compute_intervals(input: &PollInput) -> (u64, u64) {
    let calls = input.calls_per_cycle.max(1);

    let Some(ref rl) = input.rate_limit else {
        // No data yet — assume 5000 limit, full window remaining.
        let budget = input.aggression.target_calls(5000).max(1);
        let interval = calls * 3600 / budget;
        return (interval.max(MIN_ACTIVE_SECS), interval.max(MIN_IDLE_SECS));
    };

    let time_left = rl.reset.saturating_sub(input.now).max(1);
    let target_budget = input.aggression.target_calls(rl.limit);
    let remaining_budget = target_budget.saturating_sub(rl.used);

    let interval = if remaining_budget == 0 {
        time_left
    } else {
        calls * time_left / remaining_budget
    };

    (interval.max(MIN_ACTIVE_SECS), interval.max(MIN_IDLE_SECS))
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: u64 = 1_000_000;

    fn input(rl: Option<RateLimit>, calls: u64, aggression: PollAggression) -> PollInput {
        PollInput {
            rate_limit: rl,
            calls_per_cycle: calls,
            now: T,
            aggression,
        }
    }

    fn make_rl(remaining: u64, limit: u64, secs_until_reset: u64) -> RateLimit {
        RateLimit {
            limit,
            remaining,
            reset: T + secs_until_reset,
            used: limit.saturating_sub(remaining),
        }
    }

    // -- Fallback (no rate-limit data) --

    #[test]
    fn fallback_polls_at_floor_for_single_call() {
        for agg in [
            PollAggression::Low,
            PollAggression::Medium,
            PollAggression::High,
        ] {
            let (active, idle) = compute_intervals(&input(None, 1, agg));
            assert_eq!(active, MIN_ACTIVE_SECS, "{agg:?}");
            assert_eq!(idle, MIN_IDLE_SECS, "{agg:?}");
        }
    }

    #[test]
    fn fallback_scales_with_calls_per_cycle() {
        // Low: 15% of 5000 = 750 budget. 10 calls/cycle → 10×3600/750 = 48s.
        let (active, idle) = compute_intervals(&input(None, 10, PollAggression::Low));
        assert_eq!(active, 48);
        assert_eq!(idle, 48);
    }

    // -- Fresh window --

    #[test]
    fn fresh_window_polls_at_floor() {
        let rl = make_rl(5000, 5000, 3600);
        let (active, idle) = compute_intervals(&input(Some(rl), 1, PollAggression::High));
        assert_eq!(active, MIN_ACTIVE_SECS);
        assert_eq!(idle, MIN_IDLE_SECS);
    }

    // -- Budget tracks actual usage --

    #[test]
    fn half_used_budget_doubles_interval() {
        // High: target = 4000. used = 2000 → remaining = 2000.
        // 1 call/cycle, 1800s left → 1800/2000 < 1 → floor.
        let rl = make_rl(3000, 5000, 1800);
        let (active, _) = compute_intervals(&input(Some(rl), 1, PollAggression::High));
        assert_eq!(active, MIN_ACTIVE_SECS);
    }

    #[test]
    fn tight_budget_slows_down() {
        // Low: target = 750. used = 700 → remaining = 50.
        // 1 call/cycle, 1800s left → 1800/50 = 36.
        let rl = make_rl(4300, 5000, 1800);
        let (active, idle) = compute_intervals(&input(Some(rl), 1, PollAggression::Low));
        assert_eq!(active, 36);
        assert_eq!(idle, 36);
    }

    #[test]
    fn budget_exhausted_waits_for_reset() {
        // Low: target = 750. used = 800 → remaining = 0. Wait out window.
        let rl = make_rl(4200, 5000, 1800);
        let (active, idle) = compute_intervals(&input(Some(rl), 1, PollAggression::Low));
        assert_eq!(active, 1800);
        assert_eq!(idle, 1800);
    }

    // -- Aggression ordering --

    #[test]
    fn aggression_ordering() {
        let rl = make_rl(3000, 5000, 1800); // used = 2000
        let (low_a, _) = compute_intervals(&input(Some(rl.clone()), 3, PollAggression::Low));
        let (med_a, _) = compute_intervals(&input(Some(rl.clone()), 3, PollAggression::Medium));
        let (high_a, _) = compute_intervals(&input(Some(rl), 3, PollAggression::High));
        assert!(
            low_a >= med_a,
            "Low ({low_a}) should be >= Medium ({med_a})"
        );
        assert!(
            med_a >= high_a,
            "Medium ({med_a}) should be >= High ({high_a})"
        );
    }

    // -- Edge cases --

    #[test]
    fn zero_calls_treated_as_one() {
        let rl = make_rl(5000, 5000, 3600);
        let (a0, i0) = compute_intervals(&input(Some(rl.clone()), 0, PollAggression::High));
        let (a1, i1) = compute_intervals(&input(Some(rl), 1, PollAggression::High));
        assert_eq!(a0, a1);
        assert_eq!(i0, i1);
    }

    #[test]
    fn min_floors_never_violated() {
        for agg in [
            PollAggression::Low,
            PollAggression::Medium,
            PollAggression::High,
        ] {
            let rl = make_rl(5000, 5000, 3600);
            let (active, idle) = compute_intervals(&input(Some(rl), 1, agg));
            assert!(active >= MIN_ACTIVE_SECS, "{agg:?}: active={active}");
            assert!(idle >= MIN_IDLE_SECS, "{agg:?}: idle={idle}");
        }
    }

    // -- Realistic scenarios --

    #[test]
    fn realistic_high_aggression_4_repos() {
        // 4 repos = ~4 calls/cycle. High: target = 4000. Fresh window.
        // interval = 4×3600/4000 = 3.6 → floor.
        let rl = make_rl(5000, 5000, 3600);
        let (active, idle) = compute_intervals(&input(Some(rl), 4, PollAggression::High));
        assert_eq!(active, MIN_ACTIVE_SECS);
        assert_eq!(idle, MIN_IDLE_SECS);
    }

    #[test]
    fn realistic_low_aggression_many_repos() {
        // 20 repos = ~20 calls/cycle. Low: target = 750.
        // interval = 20×3600/750 = 96s.
        let rl = make_rl(5000, 5000, 3600);
        let (active, idle) = compute_intervals(&input(Some(rl), 20, PollAggression::Low));
        assert_eq!(active, 96);
        assert_eq!(idle, 96);
    }

    #[test]
    fn external_usage_eats_into_our_budget() {
        // High: target = 4000. External tools used 3900 → remaining = 100.
        // 1 call/cycle, 1800s left → 1800/100 = 18s.
        let rl = make_rl(1100, 5000, 1800); // used = 3900
        let (active, idle) = compute_intervals(&input(Some(rl), 1, PollAggression::High));
        assert_eq!(active, 18);
        assert_eq!(idle, 18);
    }

    #[test]
    fn external_usage_exceeds_our_target() {
        // Low: target = 750. External tools used 800 → remaining = 0. Back off.
        let rl = make_rl(4200, 5000, 1800); // used = 800
        let (active, idle) = compute_intervals(&input(Some(rl), 1, PollAggression::Low));
        assert_eq!(active, 1800);
        assert_eq!(idle, 1800);
    }

    #[test]
    fn stale_rate_limit_past_reset_polls_at_floor() {
        // now is past rl.reset → time_left = max(0,1) = 1.
        let mut rl = make_rl(5000, 5000, 0);
        rl.reset = T.saturating_sub(100); // reset was 100s ago
        let (active, idle) = compute_intervals(&input(Some(rl), 1, PollAggression::High));
        assert_eq!(active, MIN_ACTIVE_SECS);
        assert_eq!(idle, MIN_IDLE_SECS);
    }
}
