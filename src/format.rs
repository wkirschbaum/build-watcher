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
    fn truncate_behavior() {
        assert_eq!(truncate("hello", 10), "hello"); // shorter than max
        assert_eq!(truncate("hello", 5), "hello"); // exactly max — no ellipsis
        assert_eq!(truncate("hello!", 5), "hell…"); // one over max
        assert_eq!(truncate("hello world!", 8), "hello w…");
        assert_eq!(truncate("", 5), ""); // empty string
        assert_eq!(truncate("héllo", 3), "hé…"); // multibyte chars
    }
}
