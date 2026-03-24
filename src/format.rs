use std::time::Duration;

/// Format a Duration as "Xs", "Xm", or "Xm Ys".
pub fn duration(d: Duration) -> String {
    seconds(d.as_secs())
}

/// Format seconds as "Xs", "Xm", or "Xm Ys".
pub fn seconds(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else {
        let m = secs / 60;
        let s = secs % 60;
        if s == 0 {
            format!("{m}m")
        } else {
            format!("{m}m {s}s")
        }
    }
}

/// Format seconds as a human-readable "X ago" string.
pub fn age(secs: u64) -> String {
    if secs < 60 {
        "just now".to_string()
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86400)
    }
}

/// Truncate a string to `max` characters, appending "…" if truncated.
pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() > max {
        let truncated: String = s.chars().take(max - 1).collect();
        format!("{truncated}…")
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duration_seconds_only() {
        assert_eq!(duration(Duration::from_secs(42)), "42s");
    }

    #[test]
    fn duration_minutes_and_seconds() {
        assert_eq!(duration(Duration::from_secs(150)), "2m 30s");
    }

    #[test]
    fn duration_exact_minutes() {
        assert_eq!(duration(Duration::from_secs(120)), "2m");
    }

    #[test]
    fn duration_zero() {
        assert_eq!(duration(Duration::ZERO), "0s");
    }

    #[test]
    fn seconds_formats_correctly() {
        assert_eq!(seconds(0), "0s");
        assert_eq!(seconds(59), "59s");
        assert_eq!(seconds(60), "1m");
        assert_eq!(seconds(90), "1m 30s");
    }

    #[test]
    fn age_formats_correctly() {
        assert_eq!(age(30), "just now");
        assert_eq!(age(300), "5m ago");
        assert_eq!(age(7200), "2h ago");
        assert_eq!(age(172800), "2d ago");
    }

    #[test]
    fn truncate_short_string_unchanged() {
        assert_eq!(truncate("hello", 10), "hello");
    }

    #[test]
    fn truncate_long_string() {
        assert_eq!(truncate("hello world!", 8), "hello w…");
    }
}
