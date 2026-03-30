use chrono::Timelike;

use super::types::{
    Config, NotificationConfig, NotificationLevel, NotificationOverrides, QuietHours,
};

impl Config {
    /// Resolve effective notification levels for a repo/branch.
    /// Priority: branch overrides > repo overrides > global defaults.
    pub fn notifications_for(&self, repo: &str, branch: &str) -> NotificationConfig {
        let global = &self.notifications;
        let repo_cfg = self.repos.get(repo);
        let repo_notif = repo_cfg.map(|r| &r.notifications);
        let branch_notif = repo_cfg
            .and_then(|r| r.branch_notifications.get(branch))
            .map(|b| &b.notifications);

        let resolve = |get_field: fn(&NotificationOverrides) -> Option<NotificationLevel>,
                       global_val: NotificationLevel|
         -> NotificationLevel {
            branch_notif
                .and_then(get_field)
                .or_else(|| repo_notif.and_then(get_field))
                .unwrap_or(global_val)
        };

        NotificationConfig {
            build_started: resolve(|o| o.build_started, global.build_started),
            build_success: resolve(|o| o.build_success, global.build_success),
            build_failure: resolve(|o| o.build_failure, global.build_failure),
        }
    }

    /// Workflow filter for a repo. Empty slice means all workflows.
    pub fn workflows_for(&self, repo: &str) -> &[String] {
        self.repos
            .get(repo)
            .filter(|r| !r.workflows.is_empty())
            .map_or(&[], |r| r.workflows.as_slice())
    }

    pub fn branches_for(&self, repo: &str) -> &[String] {
        self.repos
            .get(repo)
            .filter(|r| !r.branches.is_empty())
            .map_or(&self.default_branches, |r| r.branches.as_slice())
    }

    /// Returns `true` if the repo has explicit branch configuration (not using global defaults).
    pub fn has_explicit_branches(&self, repo: &str) -> bool {
        self.repos.get(repo).is_some_and(|r| !r.branches.is_empty())
    }

    /// Returns `true` if the current local time falls within the configured quiet hours.
    pub fn is_in_quiet_hours(&self) -> bool {
        let Some(qh) = &self.quiet_hours else {
            return false;
        };
        let cur_mins = local_time_minutes();
        is_in_quiet_hours_at(qh, cur_mins)
    }

    /// Compile the `branch_filter` regex, if set and valid.
    pub fn branch_filter_regex(&self) -> Option<regex::Regex> {
        self.branch_filter
            .as_ref()
            .filter(|p| !p.is_empty())
            .and_then(|p| regex::Regex::new(p).ok())
    }
}

/// Returns the current local time as minutes since midnight.
fn local_time_minutes() -> u32 {
    let now = chrono::Local::now();
    now.hour() * 60 + now.minute()
}

/// Pure helper for testability — takes the current time as `cur_mins` (minutes since midnight).
fn is_in_quiet_hours_at(qh: &QuietHours, cur_mins: u32) -> bool {
    let parse = |s: &str| -> Option<u32> {
        let (h, m) = s.split_once(':')?;
        let h: u32 = h.parse().ok()?;
        let m: u32 = m.parse().ok()?;
        if h > 23 || m > 59 {
            return None;
        }
        Some(h * 60 + m)
    };
    let (Some(start), Some(end)) = (parse(&qh.start), parse(&qh.end)) else {
        return false; // invalid config — never suppress
    };
    if start <= end {
        // Same-day range e.g. 09:00–17:00
        cur_mins >= start && cur_mins < end
    } else {
        // Overnight range e.g. 22:00–08:00
        cur_mins >= start || cur_mins < end
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::config::{
        BranchConfig, Config, NotificationLevel, NotificationOverrides, RepoConfig,
    };

    fn qh(start: &str, end: &str) -> QuietHours {
        QuietHours {
            start: start.to_string(),
            end: end.to_string(),
        }
    }

    #[test]
    fn quiet_hours_same_day_inside() {
        assert!(is_in_quiet_hours_at(&qh("09:00", "17:00"), 9 * 60));
        assert!(is_in_quiet_hours_at(&qh("09:00", "17:00"), 12 * 60));
        assert!(is_in_quiet_hours_at(&qh("09:00", "17:00"), 17 * 60 - 1));
    }

    #[test]
    fn quiet_hours_same_day_outside() {
        assert!(!is_in_quiet_hours_at(&qh("09:00", "17:00"), 8 * 60 + 59));
        assert!(!is_in_quiet_hours_at(&qh("09:00", "17:00"), 17 * 60));
        assert!(!is_in_quiet_hours_at(&qh("09:00", "17:00"), 23 * 60));
    }

    #[test]
    fn quiet_hours_overnight_inside() {
        assert!(is_in_quiet_hours_at(&qh("22:00", "08:00"), 22 * 60));
        assert!(is_in_quiet_hours_at(&qh("22:00", "08:00"), 23 * 60 + 59));
        assert!(is_in_quiet_hours_at(&qh("22:00", "08:00"), 0));
        assert!(is_in_quiet_hours_at(&qh("22:00", "08:00"), 7 * 60 + 59));
    }

    #[test]
    fn quiet_hours_overnight_outside() {
        assert!(!is_in_quiet_hours_at(&qh("22:00", "08:00"), 8 * 60));
        assert!(!is_in_quiet_hours_at(&qh("22:00", "08:00"), 21 * 60 + 59));
        assert!(!is_in_quiet_hours_at(&qh("22:00", "08:00"), 12 * 60));
    }

    #[test]
    fn quiet_hours_invalid_config_never_suppresses() {
        assert!(!is_in_quiet_hours_at(&qh("bad", "08:00"), 12 * 60));
        assert!(!is_in_quiet_hours_at(&qh("22:00", "99:00"), 23 * 60));
    }

    #[test]
    fn notifications_for_global_defaults() {
        let config = Config::default();
        let n = config.notifications_for("any/repo", "main");
        assert_eq!(n.build_started, NotificationLevel::Normal);
        assert_eq!(n.build_success, NotificationLevel::Normal);
        assert_eq!(n.build_failure, NotificationLevel::Critical);
    }

    #[test]
    fn notifications_for_repo_override() {
        let mut config = Config::default();
        config.repos.insert(
            "alice/app".to_string(),
            RepoConfig {
                notifications: NotificationOverrides {
                    build_started: Some(NotificationLevel::Off),
                    build_success: None,
                    build_failure: Some(NotificationLevel::Low),
                },
                ..Default::default()
            },
        );
        let n = config.notifications_for("alice/app", "main");
        assert_eq!(n.build_started, NotificationLevel::Off);
        assert_eq!(n.build_success, NotificationLevel::Normal); // inherited from global
        assert_eq!(n.build_failure, NotificationLevel::Low);
    }

    #[test]
    fn notifications_for_branch_override() {
        let mut config = Config::default();
        let mut branch_notifications = HashMap::new();
        branch_notifications.insert(
            "release".to_string(),
            BranchConfig {
                notifications: NotificationOverrides {
                    build_started: Some(NotificationLevel::Off),
                    build_success: Some(NotificationLevel::Critical),
                    build_failure: None,
                },
            },
        );
        config.repos.insert(
            "alice/app".to_string(),
            RepoConfig {
                notifications: NotificationOverrides {
                    build_failure: Some(NotificationLevel::Low),
                    ..Default::default()
                },
                branch_notifications,
                ..Default::default()
            },
        );
        let n = config.notifications_for("alice/app", "release");
        assert_eq!(n.build_started, NotificationLevel::Off); // from branch
        assert_eq!(n.build_success, NotificationLevel::Critical); // from branch
        assert_eq!(n.build_failure, NotificationLevel::Low); // from repo (branch is None)
    }
}
