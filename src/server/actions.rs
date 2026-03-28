use build_watcher::config::{self, NotificationLevel, NotificationOverrides};
use build_watcher::watcher::{
    RateLimitState, SharedConfig, WatcherHandle, Watches, collect_persisted, start_watch,
};

/// Shared logic for adding repos to watch — used by both the MCP tool and REST endpoint.
pub(crate) async fn do_watch_builds(
    watches: &Watches,
    config: &SharedConfig,
    handle: &WatcherHandle,
    rate_limit: &RateLimitState,
    repos: &[String],
) -> Vec<String> {
    let repo_branches: Vec<(String, Vec<String>)> = {
        let cfg = config.lock().await;
        repos
            .iter()
            .map(|repo| (repo.clone(), cfg.branches_for(repo).to_vec()))
            .collect()
    };

    let mut results = Vec::new();
    let mut started_repos: Vec<String> = Vec::new();
    for (repo, branches) in &repo_branches {
        let mut any_started = false;
        for branch in branches {
            match start_watch(watches, config, handle, rate_limit, repo, branch).await {
                Ok(msg) => {
                    any_started = true;
                    results.push(msg);
                }
                Err(msg) => results.push(msg),
            }
        }
        if any_started {
            started_repos.push(repo.clone());
        }
    }

    if !started_repos.is_empty() {
        let persisted = collect_persisted(watches).await;
        let hist = handle.history.lock().await.clone();
        handle.persistence.save_state(&persisted, &hist).await;
        let snapshot = {
            let mut cfg = config.lock().await;
            cfg.add_repos(&started_repos);
            cfg.clone()
        };
        if let Err(warning) = persist_config(snapshot).await {
            results.push(warning);
        }
    }

    results
}

/// Shared logic for removing repos from watch — used by both the MCP tool and REST endpoint.
pub(crate) async fn do_stop_watches(
    watches: &Watches,
    config: &SharedConfig,
    handle: &WatcherHandle,
    repos: &[String],
) -> Vec<String> {
    let removed_counts: Vec<(String, usize)> = {
        let mut w = watches.lock().await;
        repos
            .iter()
            .map(|repo| {
                let keys: Vec<build_watcher::watcher::WatchKey> =
                    w.keys().filter(|k| k.matches_repo(repo)).cloned().collect();
                for key in &keys {
                    w.remove(key);
                }
                (repo.clone(), keys.len())
            })
            .collect()
    };
    if let Err(e) = handle
        .persistence
        .save_watches(&collect_persisted(watches).await)
        .await
    {
        tracing::error!(error = %e, "Failed to persist watches");
    }

    let (snapshot, mut results) = {
        let mut cfg = config.lock().await;
        let mut results = Vec::new();
        for (repo, branch_count) in removed_counts {
            let was_in_config = cfg.repos.contains_key(&repo);
            cfg.repos.remove(&repo);
            let msg = match (branch_count, was_in_config) {
                (n, _) if n > 0 => format!("Stopped watching {repo} ({n} branches)"),
                (_, true) => format!("{repo}: removed from config (was not actively polling)"),
                _ => format!("{repo}: not found"),
            };
            results.push(msg);
        }
        (cfg.clone(), results)
    };
    if let Err(warning) = persist_config(snapshot).await {
        results.push(warning);
    }

    results
}

/// Shared logic for updating which branches are watched for a repo.
///
/// Stops watches for branches no longer in the list, starts watches for new
/// branches, updates config, and persists both.
pub(crate) async fn do_configure_branches(
    watches: &Watches,
    config: &SharedConfig,
    handle: &WatcherHandle,
    rate_limit: &RateLimitState,
    repo: &str,
    new_branches: Vec<String>,
) -> Vec<String> {
    let mut results = Vec::new();

    // Current branches from live watches.
    let current_branches: Vec<String> = {
        let w = watches.lock().await;
        w.keys()
            .filter(|k| k.matches_repo(repo))
            .map(|k| k.branch.clone())
            .collect()
    };

    // Stop watches for removed branches.
    {
        let mut w = watches.lock().await;
        for branch in &current_branches {
            if !new_branches.contains(branch) {
                let key = build_watcher::watcher::WatchKey::new(repo, branch);
                if w.remove(&key).is_some() {
                    results.push(format!("Stopped watching {repo} [{branch}]"));
                }
            }
        }
    }

    // Start watches for new branches.
    for branch in &new_branches {
        if !current_branches.contains(branch) {
            match start_watch(watches, config, handle, rate_limit, repo, branch).await {
                Ok(msg) => results.push(msg),
                Err(msg) => results.push(msg),
            }
        }
    }

    // Update config and persist.
    {
        let mut cfg = config.lock().await;
        let rc = cfg.repos.entry(repo.to_string()).or_default();
        rc.branches = new_branches;
    }
    let persisted = collect_persisted(watches).await;
    if let Err(e) = handle.persistence.save_watches(&persisted).await {
        tracing::error!(error = %e, "Failed to persist watches");
    }
    let snapshot = config.lock().await.clone();
    if let Err(warning) = persist_config(snapshot).await {
        results.push(warning);
    }

    results
}

pub(crate) async fn persist_config(cfg: build_watcher::config::Config) -> Result<(), String> {
    config::save_config_async(&cfg).await.map_err(|e| {
        tracing::error!("Failed to save config: {e}");
        format!("\n⚠️ Warning: config could not be saved to disk: {e}")
    })
}

/// Types that can have notification levels applied to them field-by-field.
pub(crate) trait ApplyNotificationLevels {
    fn apply_started(&mut self, v: NotificationLevel);
    fn apply_success(&mut self, v: NotificationLevel);
    fn apply_failure(&mut self, v: NotificationLevel);
}

impl ApplyNotificationLevels for build_watcher::config::NotificationConfig {
    fn apply_started(&mut self, v: NotificationLevel) {
        self.build_started = v;
    }
    fn apply_success(&mut self, v: NotificationLevel) {
        self.build_success = v;
    }
    fn apply_failure(&mut self, v: NotificationLevel) {
        self.build_failure = v;
    }
}

impl ApplyNotificationLevels for NotificationOverrides {
    fn apply_started(&mut self, v: NotificationLevel) {
        self.build_started = Some(v);
    }
    fn apply_success(&mut self, v: NotificationLevel) {
        self.build_success = Some(v);
    }
    fn apply_failure(&mut self, v: NotificationLevel) {
        self.build_failure = Some(v);
    }
}

/// Apply optional per-event levels to any `ApplyNotificationLevels` target.
///
/// Only `Some` values are applied; `None` fields are left unchanged.
pub(crate) fn apply_levels<T: ApplyNotificationLevels>(
    target: &mut T,
    started: Option<NotificationLevel>,
    success: Option<NotificationLevel>,
    failure: Option<NotificationLevel>,
) {
    if let Some(l) = started {
        target.apply_started(l);
    }
    if let Some(l) = success {
        target.apply_success(l);
    }
    if let Some(l) = failure {
        target.apply_failure(l);
    }
}

/// Mute, unmute, or set per-event notification levels for a repo/branch.
///
/// Used by both the REST `POST /notifications` and the TUI daemon client.
/// The `action` string is one of `"mute"`, `"unmute"`, or `"set_levels"`.
pub(crate) fn do_notification_action(
    config: &mut build_watcher::config::Config,
    repo: &str,
    branch: Option<&str>,
    action: &str,
    started: Option<NotificationLevel>,
    success: Option<NotificationLevel>,
    failure: Option<NotificationLevel>,
) -> Result<String, String> {
    use build_watcher::config::BranchConfig;

    let Some(rc) = config.repos.get_mut(repo) else {
        return Err(format!("{repo}: not being watched"));
    };
    let all_off = NotificationOverrides {
        build_started: Some(NotificationLevel::Off),
        build_success: Some(NotificationLevel::Off),
        build_failure: Some(NotificationLevel::Off),
    };
    let target_label = match branch {
        Some(b) => format!("{repo}/{b}"),
        None => repo.to_string(),
    };
    match (action, branch) {
        ("mute", Some(branch)) => {
            rc.branch_notifications
                .entry(branch.to_string())
                .or_default()
                .notifications = all_off;
            Ok(format!("{target_label}: notifications muted"))
        }
        ("unmute", Some(branch)) => {
            if let Some(bc) = rc.branch_notifications.get_mut(branch) {
                bc.notifications = NotificationOverrides::default();
                if bc == &BranchConfig::default() {
                    rc.branch_notifications.remove(branch);
                }
            }
            Ok(format!(
                "{target_label}: notifications unmuted (using repo/global defaults)"
            ))
        }
        ("mute", None) => {
            rc.notifications = all_off;
            Ok(format!("{target_label}: notifications muted"))
        }
        ("unmute", None) => {
            rc.notifications = NotificationOverrides::default();
            Ok(format!(
                "{target_label}: notifications unmuted (using global defaults)"
            ))
        }
        ("set_levels", Some(branch)) => {
            let overrides = &mut rc
                .branch_notifications
                .entry(branch.to_string())
                .or_default()
                .notifications;
            apply_levels(overrides, started, success, failure);
            Ok(format!("{target_label}: notification levels updated"))
        }
        ("set_levels", None) => {
            apply_levels(&mut rc.notifications, started, success, failure);
            Ok(format!("{target_label}: notification levels updated"))
        }
        (other, _) => Err(format!("unknown action: {other:?}")),
    }
}

/// Validate a time string in HH:MM (24-hour) format.
/// Apply pause/resume to the pause state. Returns a human-readable message.
pub(crate) async fn apply_pause(
    pause: &build_watcher::watcher::PauseState,
    do_pause: bool,
    minutes: Option<u64>,
) -> String {
    let mut p = pause.lock().await;
    if do_pause {
        match minutes {
            Some(mins) if mins > 0 => {
                *p = Some(tokio::time::Instant::now() + std::time::Duration::from_secs(mins * 60));
                format!("Notifications paused for {mins} minutes")
            }
            _ => {
                const INDEFINITE: u64 = u32::MAX as u64;
                *p = Some(tokio::time::Instant::now() + std::time::Duration::from_secs(INDEFINITE));
                "Notifications paused indefinitely".to_string()
            }
        }
    } else {
        let was_paused = p.is_some_and(|d| tokio::time::Instant::now() < d);
        *p = None;
        if was_paused {
            "Notifications resumed".to_string()
        } else {
            "Notifications were not paused".to_string()
        }
    }
}

/// Apply quiet hours changes to config. Returns messages describing what changed.
pub(crate) fn apply_quiet_hours(
    config: &mut build_watcher::config::Config,
    quiet_start: Option<&str>,
    quiet_end: Option<&str>,
    quiet_clear: bool,
) -> Vec<String> {
    let mut msgs = Vec::new();
    if quiet_clear {
        config.quiet_hours = None;
        msgs.push("Quiet hours cleared".to_string());
    } else if quiet_start.is_some() || quiet_end.is_some() {
        let start = quiet_start.unwrap_or("22:00").to_string();
        let end = quiet_end.unwrap_or("06:00").to_string();
        config.quiet_hours = Some(build_watcher::config::QuietHours {
            start: start.clone(),
            end: end.clone(),
        });
        msgs.push(format!("Quiet hours set: {start}–{end} (local time)"));
    }
    msgs
}

pub(crate) fn validate_hhmm(s: &str) -> Result<(), String> {
    let Some((h, m)) = s.split_once(':') else {
        return Err(format!("{s:?} is not HH:MM format (e.g. \"22:00\")"));
    };
    let h: u32 = h
        .parse()
        .map_err(|_| format!("{s:?}: hours must be a number"))?;
    let m: u32 = m
        .parse()
        .map_err(|_| format!("{s:?}: minutes must be a number"))?;
    if h > 23 || m > 59 {
        return Err(format!("{s:?}: hours must be 0–23, minutes 0–59"));
    }
    Ok(())
}

#[cfg(test)]
fn format_notification_overrides(overrides: &NotificationOverrides) -> String {
    [
        overrides.build_started.map(|l| format!("started: {l}")),
        overrides.build_success.map(|l| format!("success: {l}")),
        overrides.build_failure.map(|l| format!("failure: {l}")),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use build_watcher::config::{NotificationLevel, NotificationOverrides};

    #[test]
    fn hhmm_validation() {
        assert!(validate_hhmm("00:00").is_ok());
        assert!(validate_hhmm("23:59").is_ok());
        assert!(validate_hhmm("24:00").is_err());
        assert!(validate_hhmm("12:60").is_err());
        assert!(validate_hhmm("noon").is_err());
        assert!(validate_hhmm("12").is_err());
    }

    #[test]
    fn notification_overrides_formatting() {
        assert_eq!(
            format_notification_overrides(&NotificationOverrides::default()),
            ""
        );
        assert_eq!(
            format_notification_overrides(&NotificationOverrides {
                build_started: Some(NotificationLevel::Off),
                build_success: Some(NotificationLevel::Normal),
                build_failure: Some(NotificationLevel::Critical),
            }),
            "started: off, success: normal, failure: critical"
        );
        assert_eq!(
            format_notification_overrides(&NotificationOverrides {
                build_failure: Some(NotificationLevel::Low),
                ..Default::default()
            }),
            "failure: low"
        );
    }

    #[test]
    fn apply_levels_to_notification_config() {
        let mut notif = build_watcher::config::NotificationConfig::default();
        apply_levels(
            &mut notif,
            Some(NotificationLevel::Off),
            None,
            Some(NotificationLevel::Low),
        );
        assert_eq!(notif.build_started, NotificationLevel::Off);
        assert_eq!(notif.build_success, NotificationLevel::Normal); // unchanged
        assert_eq!(notif.build_failure, NotificationLevel::Low);
    }

    #[test]
    fn apply_levels_to_overrides() {
        let mut overrides = NotificationOverrides::default();
        apply_levels(
            &mut overrides,
            None,
            Some(NotificationLevel::Critical),
            None,
        );
        assert_eq!(overrides.build_started, None); // unchanged
        assert_eq!(overrides.build_success, Some(NotificationLevel::Critical));
        assert_eq!(overrides.build_failure, None); // unchanged
    }
}
