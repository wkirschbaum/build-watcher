use std::time::Duration;

const SECS_PER_MINUTE: u64 = 60;
const SECS_PER_HOUR: u64 = 3600;
const SECS_PER_DAY: u64 = 86400;

/// Format a Duration as "Xs", "Xm", or "Xm Ys".
pub fn duration(d: Duration) -> String {
    seconds(d.as_secs())
}

/// Format seconds as "Xs", "Xm", or "Xm Ys".
pub fn seconds(secs: u64) -> String {
    if secs < SECS_PER_MINUTE {
        format!("{secs}s")
    } else {
        let m = secs / SECS_PER_MINUTE;
        let s = secs % SECS_PER_MINUTE;
        if s == 0 {
            format!("{m}m")
        } else {
            format!("{m}m {s}s")
        }
    }
}

/// Format seconds as a human-readable "X ago" string.
pub fn age(secs: u64) -> String {
    if secs < SECS_PER_MINUTE {
        "just now".to_string()
    } else if secs < SECS_PER_HOUR {
        format!("{}m ago", secs / SECS_PER_MINUTE)
    } else if secs < SECS_PER_DAY {
        format!("{}h ago", secs / SECS_PER_HOUR)
    } else {
        format!("{}d ago", secs / SECS_PER_DAY)
    }
}

/// Format a GitHub Actions status or conclusion for display.
///
/// Converts snake_case API values (e.g. `"in_progress"`) to readable labels
/// (e.g. `"in progress"`). Values that are already readable pass through unchanged.
pub fn status(s: &str) -> &str {
    match s {
        "in_progress" => "in progress",
        "timed_out" => "timed out",
        "startup_failure" => "startup fail",
        other => other,
    }
}

/// Truncate a string to `max` characters, appending "…" if truncated.
pub fn truncate(s: &str, max: usize) -> String {
    // Collect char boundary indices in a single pass.
    let mut indices = s.char_indices();
    match indices.nth(max.saturating_sub(1)) {
        // Fewer than `max` chars — nothing to truncate.
        None => s.to_string(),
        // Exactly `max` chars and nothing after — fits exactly.
        Some((_, _)) if indices.next().is_none() => s.to_string(),
        // More than `max` chars — truncate at the boundary before `max`.
        Some((end, _)) => format!("{}…", &s[..end]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duration_formatting() {
        assert_eq!(duration(Duration::ZERO), "0s");
        assert_eq!(duration(Duration::from_secs(42)), "42s");
        assert_eq!(duration(Duration::from_secs(120)), "2m");
        assert_eq!(duration(Duration::from_secs(150)), "2m 30s");
    }

    #[test]
    fn seconds_formatting() {
        assert_eq!(seconds(0), "0s");
        assert_eq!(seconds(59), "59s");
        assert_eq!(seconds(60), "1m");
        assert_eq!(seconds(90), "1m 30s");
    }

    #[test]
    fn age_formatting() {
        assert_eq!(age(30), "just now");
        assert_eq!(age(300), "5m ago");
        assert_eq!(age(7200), "2h ago");
        assert_eq!(age(172800), "2d ago");
    }

    #[test]
    fn status_formatting() {
        assert_eq!(status("in_progress"), "in progress");
        assert_eq!(status("timed_out"), "timed out");
        assert_eq!(status("startup_failure"), "startup fail");
        assert_eq!(status("success"), "success");
        assert_eq!(status("failure"), "failure");
        assert_eq!(status("queued"), "queued");
    }

    #[test]
    fn truncate_behavior() {
        assert_eq!(truncate("hello", 10), "hello"); // shorter than max
        assert_eq!(truncate("hello", 5), "hello"); // exactly max — no ellipsis
        assert_eq!(truncate("hello!", 5), "hell…"); // one over max
        assert_eq!(truncate("hello world!", 8), "hello w…");
        assert_eq!(truncate("", 5), ""); // empty string
        assert_eq!(truncate("héllo", 3), "hé…"); // multibyte chars
    }
}
