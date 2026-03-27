use build_watcher::config::{NotificationLevel, NotificationOverrides};
use build_watcher::persistence::Persistence;
use build_watcher::watcher::{
    RateLimitState, SharedConfig, WatcherHandle, Watches, collect_persisted, start_watch,
};

use super::schema::UpdateNotificationsParams;

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
        if let Err(e) = handle
            .persistence
            .save_watches(&collect_persisted(watches).await)
            .await
        {
            tracing::error!(error = %e, "Failed to persist watches");
        }
        let hist = handle.history.lock().await.clone();
        if let Err(e) = handle.persistence.save_history(&hist).await {
            tracing::error!(error = %e, "Failed to persist history");
        }
        let snapshot = {
            let mut cfg = config.lock().await;
            cfg.add_repos(&started_repos);
            cfg.clone()
        };
        if let Some(warning) = persist_config(&*handle.persistence, snapshot).await {
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
    if let Some(warning) = persist_config(&*handle.persistence, snapshot).await {
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
    if let Err(e) = handle
        .persistence
        .save_watches(&collect_persisted(watches).await)
        .await
    {
        tracing::error!(error = %e, "Failed to persist watches");
    }
    let snapshot = config.lock().await.clone();
    if let Some(warning) = persist_config(&*handle.persistence, snapshot).await {
        results.push(warning);
    }

    results
}

pub(crate) async fn persist_config(
    persistence: &dyn Persistence,
    config: build_watcher::config::Config,
) -> Option<String> {
    match persistence.save_config(&config).await {
        Ok(()) => None,
        Err(e) => {
            tracing::error!("Failed to save config: {e}");
            Some(format!(
                "\n⚠️ Warning: config could not be saved to disk: {e}"
            ))
        }
    }
}

/// Apply notification level params to a global `NotificationConfig` (sets values directly).
pub(crate) fn apply_notification_levels(
    notif: &mut build_watcher::config::NotificationConfig,
    params: &UpdateNotificationsParams,
) {
    if let Some(l) = params.build_started {
        notif.build_started = l;
    }
    if let Some(l) = params.build_success {
        notif.build_success = l;
    }
    if let Some(l) = params.build_failure {
        notif.build_failure = l;
    }
}

/// Apply optional per-event levels to a `NotificationOverrides` struct.
///
/// Only fields present (`Some`) are updated; `None` fields are left unchanged.
pub(crate) fn apply_level_overrides(
    overrides: &mut NotificationOverrides,
    started: Option<NotificationLevel>,
    success: Option<NotificationLevel>,
    failure: Option<NotificationLevel>,
) {
    if let Some(l) = started {
        overrides.build_started = Some(l);
    }
    if let Some(l) = success {
        overrides.build_success = Some(l);
    }
    if let Some(l) = failure {
        overrides.build_failure = Some(l);
    }
}

/// Apply notification level params to an override struct (sets Option values).
pub(crate) fn apply_notification_overrides(
    overrides: &mut NotificationOverrides,
    params: &UpdateNotificationsParams,
) {
    apply_level_overrides(
        overrides,
        params.build_started,
        params.build_success,
        params.build_failure,
    );
}

/// Validate a time string in HH:MM (24-hour) format.
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

    fn notif_params(
        started: Option<NotificationLevel>,
        success: Option<NotificationLevel>,
        failure: Option<NotificationLevel>,
    ) -> UpdateNotificationsParams {
        UpdateNotificationsParams {
            repo: None,
            branch: None,
            build_started: started,
            build_success: success,
            build_failure: failure,
            quiet_start: None,
            quiet_end: None,
            quiet_clear: None,
            pause: None,
            pause_minutes: None,
        }
    }

    #[test]
    fn apply_notification_levels_selective() {
        let mut notif = build_watcher::config::NotificationConfig::default();
        let params = notif_params(
            Some(NotificationLevel::Off),
            None,
            Some(NotificationLevel::Low),
        );
        apply_notification_levels(&mut notif, &params);
        assert_eq!(notif.build_started, NotificationLevel::Off);
        assert_eq!(notif.build_success, NotificationLevel::Normal); // unchanged
        assert_eq!(notif.build_failure, NotificationLevel::Low);
    }

    #[test]
    fn apply_notification_overrides_selective() {
        let mut overrides = NotificationOverrides::default();
        let params = notif_params(None, Some(NotificationLevel::Critical), None);
        apply_notification_overrides(&mut overrides, &params);
        assert_eq!(overrides.build_started, None); // unchanged
        assert_eq!(overrides.build_success, Some(NotificationLevel::Critical));
        assert_eq!(overrides.build_failure, None); // unchanged
    }
}
