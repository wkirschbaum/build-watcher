use std::collections::HashSet;

use build_watcher::config::{self, NotificationLevel, NotificationOverrides};
use build_watcher::github::{DEFAULT_REPO_LIMIT, run_url};
use build_watcher::watcher::{
    SharedConfig, WatcherHandle, collect_persisted, last_failed_build, start_watch,
};

use super::DaemonState;

/// Result of a single action on a repo or branch.
pub(crate) struct ActionOutcome(Result<String, String>);

impl ActionOutcome {
    pub fn ok(msg: impl Into<String>) -> Self {
        Self(Ok(msg.into()))
    }

    pub fn err(msg: impl Into<String>) -> Self {
        Self(Err(msg.into()))
    }

    pub fn message(&self) -> &str {
        match &self.0 {
            Ok(msg) | Err(msg) => msg,
        }
    }
}

/// Join all outcome messages into a single string.
pub(crate) fn format_outcomes(outcomes: &[ActionOutcome], sep: &str) -> String {
    outcomes
        .iter()
        .map(|o| o.message())
        .collect::<Vec<_>>()
        .join(sep)
}

/// Shared logic for adding repos to watch — used by both the MCP tool and REST endpoint.
pub(crate) async fn do_watch_builds(state: &DaemonState, repos: &[String]) -> Vec<ActionOutcome> {
    let repo_branches: Vec<(String, Vec<String>)> = {
        let cfg = state.config.lock().await;
        let mut pairs = Vec::new();
        for repo in repos {
            let mut branches = Vec::new();

            // Always resolve the GitHub default branch as the primary.
            match state.handle.github.default_branch(repo).await {
                Ok(gh_default) => {
                    tracing::info!(repo = %repo, branch = %gh_default, "Resolved default branch");
                    branches.push(gh_default);
                }
                Err(e) => {
                    tracing::warn!(
                        repo = %repo, error = %e,
                        "Failed to resolve default branch, falling back to config"
                    );
                    // Fall back to configured defaults only when GitHub is unreachable.
                    branches.extend(cfg.branches_for(repo).iter().cloned());
                }
            }

            // Add any explicitly configured per-repo branches (they're intentional).
            if cfg.has_explicit_branches(repo) {
                for b in cfg.branches_for(repo) {
                    if !branches.contains(b) {
                        branches.push(b.clone());
                    }
                }
            }

            pairs.push((repo.clone(), branches));
        }
        pairs
    };

    let mut results = Vec::new();
    let mut started_repos: Vec<String> = Vec::new();
    for (repo, branches) in &repo_branches {
        let mut any_started = false;
        for branch in branches {
            match start_watch(
                &state.watches,
                &state.config,
                &state.handle,
                &state.rate_limit,
                repo,
                branch,
            )
            .await
            {
                Ok(msg) => {
                    any_started = true;
                    results.push(ActionOutcome::ok(msg));
                }
                Err(msg) => results.push(ActionOutcome::err(msg)),
            }
        }

        // Auto-discover additional branches from recent runs.
        for branch in discover_branches(&state.config, &state.handle, repo, branches).await {
            if let Ok(msg) = start_watch(
                &state.watches,
                &state.config,
                &state.handle,
                &state.rate_limit,
                repo,
                &branch,
            )
            .await
            {
                any_started = true;
                results.push(ActionOutcome::ok(msg));
            }
        }

        if any_started {
            started_repos.push(repo.clone());
        }
    }

    if !started_repos.is_empty() {
        let persisted = collect_persisted(&state.watches).await;
        let hist = state.handle.history.lock().await.clone();
        state.handle.persistence.save_state(&persisted, &hist).await;
        {
            let mut cfg = state.config.lock().await;
            cfg.add_repos(&started_repos);
        }
        if let Err(warning) = persist_config(&state.config).await {
            results.push(ActionOutcome::err(warning));
        }
    }

    results
}

/// Discover branches with recent GitHub Actions runs that aren't already being watched.
/// Returns an empty vec when auto-discover is disabled or no new branches are found.
async fn discover_branches(
    config: &SharedConfig,
    handle: &WatcherHandle,
    repo: &str,
    already_watching: &[String],
) -> Vec<String> {
    let (enabled, filter_re) = {
        let cfg = config.lock().await;
        (cfg.auto_discover_branches, cfg.branch_filter_regex())
    };
    if !enabled {
        return Vec::new();
    }

    let all_runs = match handle
        .github
        .recent_runs_for_repo(repo, DEFAULT_REPO_LIMIT)
        .await
    {
        Ok(runs) => runs,
        Err(e) => {
            tracing::warn!(repo = %repo, error = %e, "Auto-discover: failed to fetch runs");
            return Vec::new();
        }
    };

    // Fetch tags so we can exclude them from discovered "branches".
    let tags: HashSet<String> = handle
        .github
        .list_tags(repo)
        .await
        .unwrap_or_default()
        .into_iter()
        .collect();

    let watching: HashSet<&str> = already_watching.iter().map(|b| b.as_str()).collect();
    let discovered: Vec<String> = all_runs
        .iter()
        .map(|r| r.head_branch.as_str())
        .filter(|b| !watching.contains(b))
        .filter(|b| !tags.contains(*b))
        .filter(|b| filter_re.as_ref().is_none_or(|re| re.is_match(b)))
        .collect::<HashSet<_>>()
        .into_iter()
        .map(|b| b.to_string())
        .collect();

    if !discovered.is_empty() {
        tracing::info!(
            repo = %repo,
            branches = ?discovered,
            "Auto-discovered branches"
        );
    }
    discovered
}

/// Shared logic for removing repos from watch — used by both the MCP tool and REST endpoint.
pub(crate) async fn do_stop_watches(state: &DaemonState, repos: &[String]) -> Vec<ActionOutcome> {
    let removed_counts: Vec<(String, usize)> = {
        let mut w = state.watches.lock().await;
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
    if let Err(e) = state
        .handle
        .persistence
        .save_watches(&collect_persisted(&state.watches).await)
        .await
    {
        tracing::error!(error = %e, "Failed to persist watches");
    }

    let mut results = {
        let mut cfg = state.config.lock().await;
        let mut results: Vec<ActionOutcome> = Vec::new();
        for (repo, branch_count) in removed_counts {
            let was_in_config = cfg.repos.contains_key(&repo);
            cfg.repos.remove(&repo);
            match (branch_count, was_in_config) {
                (n, _) if n > 0 => results.push(ActionOutcome::ok(format!(
                    "Stopped watching {repo} ({n} branches)"
                ))),
                (_, true) => results.push(ActionOutcome::ok(format!(
                    "{repo}: removed from config (was not actively polling)"
                ))),
                _ => results.push(ActionOutcome::err(format!("{repo}: not found"))),
            }
        }
        results
    };
    if let Err(warning) = persist_config(&state.config).await {
        results.push(ActionOutcome::err(warning));
    }

    results
}

/// Shared logic for updating which branches are watched for a repo.
///
/// Stops watches for branches no longer in the list, starts watches for new
/// branches, updates config, and persists both.
pub(crate) async fn do_configure_branches(
    state: &DaemonState,
    repo: &str,
    new_branches: Vec<String>,
) -> Vec<ActionOutcome> {
    let mut results = Vec::new();

    // Current branches from live watches.
    let current_branches: Vec<String> = {
        let w = state.watches.lock().await;
        w.keys()
            .filter(|k| k.matches_repo(repo))
            .map(|k| k.branch.clone())
            .collect()
    };

    // Stop watches for removed branches.
    {
        let mut w = state.watches.lock().await;
        for branch in &current_branches {
            if !new_branches.contains(branch) {
                let key = build_watcher::watcher::WatchKey::new(repo, branch);
                if w.remove(&key).is_some() {
                    results.push(ActionOutcome::ok(format!(
                        "Stopped watching {repo} [{branch}]"
                    )));
                }
            }
        }
    }

    // Start watches for new branches.
    for branch in &new_branches {
        if !current_branches.contains(branch) {
            match start_watch(
                &state.watches,
                &state.config,
                &state.handle,
                &state.rate_limit,
                repo,
                branch,
            )
            .await
            {
                Ok(msg) => results.push(ActionOutcome::ok(msg)),
                Err(msg) => results.push(ActionOutcome::err(msg)),
            }
        }
    }

    // Update config and persist.
    {
        let mut cfg = state.config.lock().await;
        let rc = cfg.repos.entry(repo.to_string()).or_default();
        rc.branches = new_branches;
    }
    let persisted = collect_persisted(&state.watches).await;
    if let Err(e) = state.handle.persistence.save_watches(&persisted).await {
        tracing::error!(error = %e, "Failed to persist watches");
    }
    if let Err(warning) = persist_config(&state.config).await {
        results.push(ActionOutcome::err(warning));
    }

    results
}

/// Rerun a GitHub Actions build. If `run_id` is `None`, finds the last failed build
/// from in-memory watches or GitHub history.
///
/// Returns a human-readable success message including the run URL, or an error string.
pub(crate) async fn do_rerun(
    state: &DaemonState,
    repo: &str,
    run_id: Option<u64>,
    failed_only: bool,
) -> Result<String, String> {
    let run_id = match run_id {
        Some(id) => id,
        None => {
            // Try in-memory watches first.
            let in_memory = {
                let watches = state.watches.lock().await;
                last_failed_build(&watches, repo).map(|(key, build)| {
                    tracing::info!(
                        repo = repo,
                        branch = key.branch,
                        run_id = build.run_id,
                        "Rerunning last failed build (from memory)"
                    );
                    build.run_id
                })
            };

            if let Some(id) = in_memory {
                id
            } else {
                // Fall back to GitHub history.
                tracing::debug!(
                    repo = repo,
                    "No in-memory failed build; querying GitHub history"
                );
                let entries = state
                    .handle
                    .github
                    .run_list_history(repo, None, 20)
                    .await
                    .map_err(|e| {
                        format!("No in-memory failed build and GitHub history lookup failed: {e}")
                    })?;
                let entry = entries
                    .into_iter()
                    .find(|e| e.conclusion == "failure")
                    .ok_or_else(|| format!("No recent failed build found for {repo}"))?;
                tracing::info!(
                    repo = repo,
                    run_id = entry.id,
                    "Rerunning last failed build (from GitHub history)"
                );
                entry.id
            }
        }
    };

    state
        .handle
        .github
        .run_rerun(repo, run_id, failed_only)
        .await
        .map_err(|e| e.to_string())?;

    // Burst-poll at 1s, 5s, 10s so the poller picks up the rerun quickly.
    let notify = state.handle.config_changed.clone();
    tokio::spawn(async move {
        for delay in [1, 5, 10] {
            tokio::time::sleep(std::time::Duration::from_secs(delay)).await;
            notify.notify_waiters();
        }
    });

    let url = run_url(repo, run_id);
    let kind = if failed_only {
        "failed jobs"
    } else {
        "all jobs"
    };
    Ok(format!("Rerunning {kind} for run {run_id}\n{url}"))
}

/// Persist the current in-memory config to disk.
///
/// Re-reads the shared config under the save lock so concurrent modifications
/// (e.g. poll_aggression changed while watch_builds is in flight) are never
/// lost due to a stale snapshot winning the I/O race.
pub(crate) async fn persist_config(config: &SharedConfig) -> Result<(), String> {
    let snapshot = config.lock().await.clone();
    config::save_config_async(&snapshot).await.map_err(|e| {
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

    let rc = config.repos.entry(repo.to_string()).or_default();
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

    #[test]
    fn mute_auto_creates_config_entry() {
        // Repo is not in config.repos — muting should create the entry, not error.
        let mut config = build_watcher::config::Config::default();
        assert!(!config.repos.contains_key("alice/app"));

        let result =
            do_notification_action(&mut config, "alice/app", None, "mute", None, None, None);
        assert!(result.is_ok(), "expected Ok, got {result:?}");
        assert!(config.repos.contains_key("alice/app"));
        let rc = &config.repos["alice/app"];
        assert_eq!(rc.notifications.build_started, Some(NotificationLevel::Off));
        assert_eq!(rc.notifications.build_success, Some(NotificationLevel::Off));
        assert_eq!(rc.notifications.build_failure, Some(NotificationLevel::Off));
    }

    #[test]
    fn unmute_auto_creates_config_entry() {
        let mut config = build_watcher::config::Config::default();
        let result =
            do_notification_action(&mut config, "alice/app", None, "unmute", None, None, None);
        assert!(result.is_ok());
        // Entry is created but with default (empty) overrides — no harm.
        assert!(config.repos.contains_key("alice/app"));
    }

    #[test]
    fn mute_branch_auto_creates_config_entry() {
        let mut config = build_watcher::config::Config::default();
        let result = do_notification_action(
            &mut config,
            "alice/app",
            Some("main"),
            "mute",
            None,
            None,
            None,
        );
        assert!(result.is_ok());
        let rc = &config.repos["alice/app"];
        let bc = &rc.branch_notifications["main"];
        assert_eq!(bc.notifications.build_started, Some(NotificationLevel::Off));
    }

    #[test]
    fn set_levels_auto_creates_config_entry() {
        let mut config = build_watcher::config::Config::default();
        let result = do_notification_action(
            &mut config,
            "alice/app",
            None,
            "set_levels",
            Some(NotificationLevel::Critical),
            None,
            None,
        );
        assert!(result.is_ok());
        let rc = &config.repos["alice/app"];
        assert_eq!(
            rc.notifications.build_started,
            Some(NotificationLevel::Critical)
        );
    }
}
