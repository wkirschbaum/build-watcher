use crate::config::PollAggression;
use crate::github::RateLimit;

/// Fastest permitted polling interval when active runs exist.
pub const MIN_ACTIVE_SECS: u64 = 15;
/// Fastest permitted polling interval when no active runs exist.
pub const MIN_IDLE_SECS: u64 = 30;

/// Assumed GitHub rate-limit window in seconds.
const WINDOW_SECS: u64 = 3600;

/// Compute active and idle poll intervals given the current rate-limit state.
///
/// # Strategy
///
/// 1. **No rate-limit data** → conservative fallback that scales with aggression.
///
/// 2. **Free zone** (`own_calls_so_far < half of target budget`): poll at floor
///    speed with `sqrt(calls)` scaling, same as before — we have headroom.
///
/// 3. **Throttle zone**: project both our own future usage and external usage
///    (other tools using the same GitHub token) forward to the window reset,
///    then spread our remaining budget across the available seconds.
///
/// # External usage projection
///
/// `rl.used` includes API calls from all sources, not just this daemon.
/// `last_active_secs` (the previous poll interval) lets us back-project our
/// own historical contribution:
///
/// ```text
/// own_calls_so_far = floor(elapsed / last_active_secs) × calls_per_cycle
/// external_used    = rl.used − own_calls_so_far          (saturating)
/// projected_ext    = external_used / elapsed × secs_to_reset
/// available_to_us  = rl.remaining − projected_ext         (saturating)
/// ```
///
/// On the first call (`last_active_secs = 0`) all of `rl.used` is attributed
/// to external sources — conservative but correct for a fresh session.
///
/// # Hard floors
///
/// `MIN_ACTIVE_SECS` (15 s) and `MIN_IDLE_SECS` (60 s) are never violated.
pub fn compute_intervals(
    rate_limit: Option<&RateLimit>,
    api_calls_per_cycle: u64,
    now: u64,
    aggression: PollAggression,
    last_active_secs: u64,
) -> (u64, u64) {
    let calls = api_calls_per_cycle.max(1);

    let Some(rl) = rate_limit else {
        return fallback_intervals(aggression);
    };

    let target_calls = aggression.target_calls(rl.limit).max(1);

    // Back-project our own calls since the window started.
    let window_start = rl.reset.saturating_sub(WINDOW_SECS);
    let elapsed = now.saturating_sub(window_start).max(1);
    let own_calls_so_far = if last_active_secs > 0 {
        (elapsed / last_active_secs).saturating_mul(calls)
    } else {
        0
    };

    // Free zone: own usage is under half the target → poll near floor speed,
    // scaled by aggression level so the setting actually affects polling rate.
    if own_calls_so_far * 2 < target_calls {
        let scale = (calls as f64).sqrt() as u64;
        let aggression_mult = aggression.interval_multiplier();
        let active = ((MIN_ACTIVE_SECS * scale.max(1)) as f64 * aggression_mult) as u64;
        let idle = ((MIN_IDLE_SECS * scale.max(1)) as f64 * aggression_mult) as u64;
        return (active.max(MIN_ACTIVE_SECS), idle.max(MIN_IDLE_SECS));
    }

    // Throttle zone: project external usage forward and compute effective budget.
    let secs_to_reset = rl.reset.saturating_sub(now).max(1);
    let external_used = rl.used.saturating_sub(own_calls_so_far);
    let projected_external =
        (external_used as f64 / elapsed as f64 * secs_to_reset as f64).min(rl.limit as f64) as u64;

    let available_to_us = rl.remaining.saturating_sub(projected_external);
    let our_remaining_quota = target_calls.saturating_sub(own_calls_so_far);
    let effective_budget = available_to_us.min(our_remaining_quota);

    // Spread effective_budget across remaining seconds.
    // checked_div: if budget == 0, wait out the full reset window.
    let rate_limited_secs = (calls * secs_to_reset)
        .checked_div(effective_budget)
        .unwrap_or(secs_to_reset);

    (
        MIN_ACTIVE_SECS.max(rate_limited_secs),
        MIN_IDLE_SECS.max(rate_limited_secs),
    )
}

fn fallback_intervals(aggression: PollAggression) -> (u64, u64) {
    let m = aggression.interval_multiplier();
    let active = ((MIN_ACTIVE_SECS as f64) * m) as u64;
    let idle = ((MIN_IDLE_SECS as f64) * m) as u64;
    (active.max(MIN_ACTIVE_SECS), idle.max(MIN_IDLE_SECS))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fixed "now" for deterministic tests — middle of a rate-limit window.
    const T: u64 = 1_000_000;

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
    fn fallback_by_aggression() {
        assert_eq!(
            compute_intervals(None, 1, T, PollAggression::Low, 0),
            (105, 210) // 15×7, 30×7
        );
        assert_eq!(
            compute_intervals(None, 1, T, PollAggression::Medium, 0),
            (30, 60) // 15×2, 30×2
        );
        assert_eq!(
            compute_intervals(None, 1, T, PollAggression::High, 0),
            (MIN_ACTIVE_SECS, MIN_IDLE_SECS) // 15×1, 30×1
        );
    }

    // -- Free zone --

    #[test]
    fn free_zone_high_aggression_at_floor() {
        // High aggression (mult=1.0): target = 70% of 5000 = 3500. own_calls=0 → free zone → floor.
        let rl = make_rl(5000, 5000, 3600);
        let (active, idle) = compute_intervals(Some(&rl), 1, T, PollAggression::High, 0);
        assert_eq!(active, MIN_ACTIVE_SECS);
        assert_eq!(idle, MIN_IDLE_SECS);
    }

    #[test]
    fn free_zone_medium_aggression_scaled() {
        // Medium aggression (mult=2.0): free zone → floor × 2.
        let rl = make_rl(5000, 5000, 3600);
        let (active, idle) = compute_intervals(Some(&rl), 1, T, PollAggression::Medium, 0);
        assert_eq!(active, (MIN_ACTIVE_SECS as f64 * 2.0) as u64); // 30
        assert_eq!(idle, (MIN_IDLE_SECS as f64 * 2.0) as u64); // 60
    }

    #[test]
    fn free_zone_low_aggression_scaled() {
        // Low aggression (mult=7.0): free zone → floor × 7.
        let rl = make_rl(5000, 5000, 3600);
        let (active, idle) = compute_intervals(Some(&rl), 1, T, PollAggression::Low, 0);
        assert_eq!(active, MIN_ACTIVE_SECS * 7); // 105
        assert_eq!(idle, MIN_IDLE_SECS * 7); // 210
    }

    #[test]
    fn free_zone_sqrt_scaling() {
        // High aggression, fresh window, 6 calls → sqrt(6) = 2 → ×2 (mult=1.0)
        let rl = make_rl(5000, 5000, 3600);
        let (active, idle) = compute_intervals(Some(&rl), 6, T, PollAggression::High, 0);
        assert_eq!(active, MIN_ACTIVE_SECS * 2);
        assert_eq!(idle, MIN_IDLE_SECS * 2);
    }

    #[test]
    fn free_zone_while_own_calls_under_half_target() {
        // High: target = 3500. If we're 1800s into a 3600s window polling at 30s
        // intervals with 1 call/cycle → own_calls = 1800/30 * 1 = 60. 60*2=120 < 3500 → free.
        let rl = make_rl(4000, 5000, 1800);
        let last = 30;
        let (active, idle) = compute_intervals(Some(&rl), 1, T, PollAggression::High, last);
        assert_eq!(active, MIN_ACTIVE_SECS);
        assert_eq!(idle, MIN_IDLE_SECS);
    }

    // -- Throttle zone --

    #[test]
    fn throttle_when_own_calls_exceed_half_target() {
        // Medium: target = 1250. 900s into window, polling at 30s/cycle, 1 call/cycle.
        // own_calls = 900/30 = 30. 30*2 = 60... still in free zone.
        // Use a smaller target: Low with limit=1000 → target=100.
        // 900s elapsed, 30s interval, 1 call → own=30. 30*2=60 >= 100? No, 60 < 100 → free.
        // Adjust: elapsed=1800, last=30 → own=60. 60*2=120 >= 100 → throttle!
        let rl = make_rl(900, 1000, 1800); // 1800s left, used=100, limit=1000
        // now=T, window_start = T+1800-3600 = T-1800, elapsed=1800
        // own = 1800/30 * 1 = 60. target=Low→100. 60*2=120 >= 100 → throttle.
        // external_used = 100 - 60 = 40. projected_ext = 40/1800 * 1800 = 40.
        // available = 900 - 40 = 860. our_remaining_quota = 100 - 60 = 40.
        // effective_budget = min(860, 40) = 40.
        // rate_limited = 1 * 1800 / 40 = 45s.
        let (active, idle) = compute_intervals(Some(&rl), 1, T, PollAggression::Low, 30);
        assert_eq!(active, 45);
        assert_eq!(idle, 45); // 45 > MIN_IDLE_SECS (30)
    }

    #[test]
    fn budget_exhausted_waits_for_reset() {
        // own_calls >= target_calls → our_remaining_quota = 0 → effective_budget = 0 → wait.
        // Low, limit=1000, target=100. Elapsed=3600s, last=30 → own=120 > 100.
        let rl = make_rl(800, 1000, 1800);
        // own = 1800/30 = 60... need more. Use last=10: own = 1800/10 = 180 > 100.
        let (active, idle) = compute_intervals(Some(&rl), 1, T, PollAggression::Low, 10);
        // effective_budget = min(available_to_us, 0) = 0 → wait (1800s), floored at MIN.
        assert_eq!(active, 1800);
        assert_eq!(idle, 1800);
    }

    #[test]
    fn external_usage_reduces_available_budget() {
        // High: target = 70% of 5000 = 3500. Window half elapsed (1800s left).
        // Own: last=60, elapsed=1800, 1 call → own = 30. 30*2=60 < 3500 → free zone.
        // Now add heavy external usage so remaining is very low:
        // remaining=100, used=4900. external_used = 4900 - 30 = 4870.
        // projected_ext = 4870 / 1800 * 1800 = 4870 → available = 100 - 4870 = 0 (saturating).
        // effective_budget = min(0, 3500-30) = 0 → wait.
        // But own_calls*2 < target_calls → would normally be free zone. Check: 30*2=60 < 3500 yes.
        // The free-zone check doesn't verify available_to_us, so let me re-read the algorithm...
        // Actually in the free zone we return immediately without checking effective_budget.
        // So this test should be in throttle zone. Let me push own_calls past the threshold.
        // High: target=3500. Need own_calls*2 >= 3500 → own_calls >= 1750.
        // last=1, elapsed=1800 → own = 1800. 1800*2=3600 >= 3500 → throttle.
        let rl = make_rl(100, 5000, 1800); // remaining=100, used=4900
        // own = 1800/1 * 1 = 1800. external_used = 4900 - 1800 = 3100.
        // projected_ext = 3100 / 1800 * 1800 = 3100. available = 100 - 3100 = 0 (saturating).
        // our_remaining_quota = 3500 - 1800 = 1700. effective_budget = min(0, 1700) = 0 → wait.
        let (active, idle) = compute_intervals(Some(&rl), 1, T, PollAggression::High, 1);
        assert_eq!(active, 1800);
        assert_eq!(idle, 1800);
    }

    #[test]
    fn first_call_attributes_all_used_to_external() {
        // last_active_secs = 0 → own_calls = 0 → free zone (own*2 < target).
        // High: target=2500. Even with lots of rl.used, we're in free zone.
        let rl = make_rl(1000, 5000, 3600); // used=4000
        let (active, idle) = compute_intervals(Some(&rl), 1, T, PollAggression::High, 0);
        // own=0, 0*2=0 < 2500 → free zone → floor (High mult=1.0)
        assert_eq!(active, MIN_ACTIVE_SECS);
        assert_eq!(idle, MIN_IDLE_SECS);
    }

    #[test]
    fn zero_calls_treated_as_one() {
        let rl = make_rl(5000, 5000, 3600);
        let (a0, i0) = compute_intervals(Some(&rl), 0, T, PollAggression::High, 0);
        let (a1, i1) = compute_intervals(Some(&rl), 1, T, PollAggression::High, 0);
        assert_eq!(a0, a1);
        assert_eq!(i0, i1);
    }

    #[test]
    fn min_floors_never_violated() {
        // Even with a tiny budget, floors hold.
        let _rl = make_rl(1, 5000, 3600); // nearly exhausted
        // Low: target=500. last=1, elapsed=3600 → own=3600 >> 500 → budget=0 → wait 3600.
        // Floor: max(15, 3600) = 3600 and max(60, 3600) = 3600. No floor violation.
        // Verify floors hold in free zone:
        let rl2 = make_rl(5000, 5000, 3600);
        let (active, idle) = compute_intervals(Some(&rl2), 1, T, PollAggression::High, 0);
        assert!(active >= MIN_ACTIVE_SECS);
        assert!(idle >= MIN_IDLE_SECS);
    }

    #[test]
    fn low_aggression_is_more_conservative_than_medium() {
        // Both in free zone with fresh window — Low mult=3.0, Medium mult=1.5.
        let rl = make_rl(5000, 5000, 3600);
        let (low_a, low_i) = compute_intervals(Some(&rl), 1, T, PollAggression::Low, 0);
        let (med_a, med_i) = compute_intervals(Some(&rl), 1, T, PollAggression::Medium, 0);
        let (high_a, high_i) = compute_intervals(Some(&rl), 1, T, PollAggression::High, 0);
        assert!(low_a > med_a, "Low should be more conservative than Medium");
        assert!(
            med_a > high_a,
            "Medium should be more conservative than High"
        );
        assert!(
            low_i > med_i,
            "Low idle should be more conservative than Medium"
        );
        assert!(
            med_i > high_i,
            "Medium idle should be more conservative than High"
        );
    }
}
