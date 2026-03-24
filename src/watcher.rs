use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::config::{Config, NotificationConfig, load_json, save_json, state_dir};
use crate::github::{LastBuild, RunInfo, gh_recent_runs, gh_run_status};
use crate::platform;

pub type SharedConfig = Arc<Mutex<Config>>;

// -- Watch key helpers --

pub fn watch_key(repo: &str, branch: &str) -> String {
    format!("{repo}#{branch}")
}

pub fn parse_watch_key(key: &str) -> (&str, &str) {
    key.rsplit_once('#').unwrap_or((key, "main"))
}

// -- Watch state persistence --

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedWatch {
    last_seen_run_id: u64,
    #[serde(default)]
    last_build: Option<LastBuild>,
}

type PersistedWatches = HashMap<String, PersistedWatch>;

pub fn load_watches() -> HashMap<String, WatchEntry> {
    let persisted: PersistedWatches =
        load_json(state_dir().join("watches.json")).unwrap_or_default();
    persisted
        .into_iter()
        .map(|(k, v)| {
            (
                k,
                WatchEntry {
                    last_seen_run_id: v.last_seen_run_id,
                    active_runs: HashMap::new(),
                    failure_counts: HashMap::new(),
                    last_build: v.last_build,
                },
            )
        })
        .collect()
}

fn save_persisted(watches: &PersistedWatches) {
    save_json(state_dir().join("watches.json"), watches);
}

pub async fn save_watches(watches: &Watches) {
    let persisted: PersistedWatches = {
        let w = watches.lock().await;
        w.iter()
            .map(|(k, v)| {
                (
                    k.clone(),
                    PersistedWatch {
                        last_seen_run_id: v.last_seen_run_id,
                        last_build: v.last_build.clone(),
                    },
                )
            })
            .collect()
    };
    save_persisted(&persisted);
}

const MAX_GH_FAILURES: u8 = 5;

/// Runtime state per repo/branch: high-water mark + in-progress runs.
#[derive(Debug, Clone)]
pub struct WatchEntry {
    last_seen_run_id: u64,
    pub active_runs: HashMap<u64, String>, // run_id -> status
    failure_counts: HashMap<u64, u8>,      // run_id -> consecutive failure count
    pub last_build: Option<LastBuild>,
}

pub type Watches = Arc<Mutex<HashMap<String, WatchEntry>>>;

// -- Watch logic --

pub async fn start_watch(
    watches: &Watches,
    config: &SharedConfig,
    repo: &str,
    branch: &str,
    key: &str,
) -> std::result::Result<String, String> {
    {
        let w = watches.lock().await;
        if w.contains_key(key) {
            return Ok(format!("{repo} [{branch}]: already being watched"));
        }
    }

    let runs = gh_recent_runs(repo, branch).await?;
    if runs.is_empty() {
        return Err(format!("{repo} [{branch}]: no workflow runs found"));
    }

    let max_id = runs.iter().map(|r| r.id).max().expect("runs is non-empty");
    let active: HashMap<u64, String> = runs
        .iter()
        .filter(|r| !r.is_completed())
        .map(|r| (r.id, r.status.clone()))
        .collect();

    let last_completed = runs.iter().find(|r| r.is_completed());

    let msg = if active.is_empty() {
        let latest = &runs[0]; // gh returns newest first
        format!(
            "{repo} [{branch}]: latest build already completed ({}), watching for new builds\n  {}: {} {}\n  {}",
            latest.conclusion,
            latest.workflow,
            latest.title,
            latest.short_sha(),
            latest.url(repo)
        )
    } else {
        format!(
            "{repo} [{branch}]: watching {} active build(s)",
            active.len()
        )
    };

    let entry = WatchEntry {
        last_seen_run_id: max_id,
        active_runs: active,
        failure_counts: HashMap::new(),
        last_build: last_completed.map(|r| r.to_last_build()),
    };

    {
        let mut w = watches.lock().await;
        // Re-check inside the lock: a concurrent call may have inserted while we
        // were making the gh network call above.
        if w.contains_key(key) {
            return Ok(format!("{repo} [{branch}]: already being watched"));
        }
        w.insert(key.to_string(), entry);
    }
    save_watches(watches).await;

    spawn_poller(watches.clone(), config.clone(), key.to_string());

    Ok(msg)
}

fn spawn_poller(watches: Watches, config: SharedConfig, key: String) {
    tokio::spawn(async move {
        poll_repo(watches, config, key).await;
    });
}

fn notify_build_complete(
    run: &RunInfo,
    repo: &str,
    branch: &str,
    key: &str,
    notif: &NotificationConfig,
) {
    let (emoji, level) = if run.succeeded() {
        ("✅", notif.build_success)
    } else {
        ("❌", notif.build_failure)
    };
    let group = format!("{key}#{}", run.workflow);
    platform::send_notification(
        &format!("{emoji} {} - {}", run.workflow, run.conclusion),
        &format!("[{branch}] {}", run.title),
        level,
        Some(&run.url(repo)),
        Some(&group),
    );
}

async fn poll_repo(watches: Watches, config: SharedConfig, key: String) {
    let (repo, branch) = parse_watch_key(&key);
    let repo = repo.to_string();
    let branch = branch.to_string();

    let mut last_new_run_check = tokio::time::Instant::now();

    loop {
        let has_active = {
            let w = watches.lock().await;
            match w.get(&key) {
                Some(entry) => !entry.active_runs.is_empty(),
                None => {
                    tracing::info!("Watch cancelled for {key}");
                    return;
                }
            }
        };

        let (active_poll_secs, idle_poll_secs, notif) = {
            let cfg = config.lock().await;
            (
                cfg.active_poll_seconds,
                cfg.idle_poll_seconds,
                cfg.notifications_for(&repo, &branch),
            )
        };

        let delay = if has_active {
            active_poll_secs
        } else {
            idle_poll_secs
        };
        tokio::time::sleep(Duration::from_secs(delay)).await;

        // Check if still watched
        {
            let w = watches.lock().await;
            if !w.contains_key(&key) {
                tracing::info!("Watch cancelled for {key}");
                return;
            }
        }

        // Poll active runs every cycle
        if has_active {
            poll_active_runs(&watches, &key, &repo, &branch, &notif).await;
        }

        // Check for new runs at the idle interval regardless of active state
        if last_new_run_check.elapsed() >= Duration::from_secs(idle_poll_secs) {
            check_for_new_runs(&watches, &key, &repo, &branch, &notif).await;
            last_new_run_check = tokio::time::Instant::now();
        }
    }
}

/// Poll all active runs for a watch. Notifies on completion and removes finished runs.
async fn poll_active_runs(
    watches: &Watches,
    key: &str,
    repo: &str,
    branch: &str,
    notif: &NotificationConfig,
) {
    let run_ids: Vec<u64> = {
        let w = watches.lock().await;
        match w.get(key) {
            Some(entry) => entry.active_runs.keys().cloned().collect(),
            None => return,
        }
    };

    let mut changed = false;

    for run_id in run_ids {
        let run = match gh_run_status(repo, run_id).await {
            Ok(r) => {
                // Reset failure count on success
                let mut w = watches.lock().await;
                if let Some(entry) = w.get_mut(key) {
                    entry.failure_counts.remove(&run_id);
                }
                r
            }
            Err(e) => {
                let mut w = watches.lock().await;
                if let Some(entry) = w.get_mut(key) {
                    let count = entry.failure_counts.entry(run_id).or_insert(0);
                    *count += 1;
                    if *count >= MAX_GH_FAILURES {
                        tracing::warn!(
                            "Removing run {run_id} from {key} after {count} consecutive failures"
                        );
                        entry.active_runs.remove(&run_id);
                        entry.failure_counts.remove(&run_id);
                        changed = true;
                    } else {
                        tracing::error!("{e} (failure {count}/{MAX_GH_FAILURES})");
                    }
                }
                continue;
            }
        };

        if run.is_completed() {
            notify_build_complete(&run, repo, branch, key, notif);
            tracing::info!(
                "Build completed for {key} run {run_id} {}: {}",
                run.short_sha(),
                run.conclusion
            );

            let mut w = watches.lock().await;
            if let Some(entry) = w.get_mut(key) {
                entry.active_runs.remove(&run_id);
                entry.last_build = Some(run.to_last_build());
            }
            changed = true;
        } else {
            // Update status if changed
            let mut w = watches.lock().await;
            if let Some(entry) = w.get_mut(key)
                && let Some(old_status) = entry.active_runs.get(&run_id)
                && *old_status != run.status
            {
                tracing::debug!(
                    "Run {run_id} status changed: {} -> {}",
                    old_status,
                    run.status
                );
                entry.active_runs.insert(run_id, run.status);
            }
        }
    }

    if changed {
        save_watches(watches).await;
    }
}

/// Check for new runs we haven't seen yet. Notify on new starts, track in-progress ones.
async fn check_for_new_runs(
    watches: &Watches,
    key: &str,
    repo: &str,
    branch: &str,
    notif: &NotificationConfig,
) {
    let last_seen = {
        let w = watches.lock().await;
        match w.get(key) {
            Some(entry) => entry.last_seen_run_id,
            None => return,
        }
    };

    let runs = match gh_recent_runs(repo, branch).await {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("{e}");
            return;
        }
    };

    let new_runs: Vec<&RunInfo> = runs.iter().filter(|r| r.id > last_seen).collect();
    if new_runs.is_empty() {
        return;
    }

    let new_max = new_runs
        .iter()
        .map(|r| r.id)
        .max()
        .expect("new_runs is non-empty");

    for run in &new_runs {
        tracing::info!(
            "New build detected for {key}: run {} {} ({}: {})",
            run.id,
            run.short_sha(),
            run.workflow,
            run.title
        );
        let group = format!("{key}#{}", run.workflow);
        platform::send_notification(
            &format!("🔨 {} - started", run.workflow),
            &format!("[{branch}] {}", run.title),
            notif.build_started,
            Some(&run.url(repo)),
            Some(&group),
        );

        // If it already completed between polls, also notify completion
        if run.is_completed() {
            notify_build_complete(run, repo, branch, key, notif);
            tracing::info!(
                "Build already completed for {key} run {} {}: {}",
                run.id,
                run.short_sha(),
                run.conclusion
            );
        }
    }

    // Update state
    let mut w = watches.lock().await;
    if let Some(entry) = w.get_mut(key) {
        entry.last_seen_run_id = new_max;
        // Track new in-progress runs, record completed ones (iterate oldest→newest so
        // the highest-id completed run ends up as last_build)
        for run in new_runs.iter().rev() {
            if run.is_completed() {
                entry.last_build = Some(run.to_last_build());
            } else {
                entry.active_runs.insert(run.id, run.status.clone());
            }
        }
    }
    drop(w);
    save_watches(watches).await;
}

pub async fn startup_watches(watches: &Watches, config: &SharedConfig) {
    // Resume existing watches — recover any in-progress builds that were active at shutdown
    let snapshot: Vec<String> = {
        let w = watches.lock().await;
        w.keys().cloned().collect()
    };
    for key in &snapshot {
        let (repo, branch) = parse_watch_key(key);
        tracing::info!("Resuming watch for {key}");

        // Scan for in-progress runs we may have missed during downtime
        match gh_recent_runs(repo, branch).await {
            Ok(runs) => {
                let mut w = watches.lock().await;
                if let Some(entry) = w.get_mut(key) {
                    for run in &runs {
                        if !run.is_completed() && !entry.active_runs.contains_key(&run.id) {
                            tracing::info!("Recovering in-progress run {} for {key}", run.id);
                            entry.active_runs.insert(run.id, run.status.clone());
                        }
                    }
                }
            }
            Err(e) => tracing::warn!("Could not recover runs for {key}: {e}"),
        }

        spawn_poller(watches.clone(), config.clone(), key.clone());
    }

    // Start watches for any config repos not already in state
    let new_watches: Vec<(String, String, String)> = {
        let cfg = config.lock().await;
        let mut result = Vec::new();
        for repo in cfg.watched_repos() {
            for branch in cfg.branches_for(repo) {
                let key = watch_key(repo, branch);
                if !snapshot.contains(&key) {
                    result.push((repo.clone(), branch.clone(), key));
                }
            }
        }
        result
    };

    for (repo, branch, key) in &new_watches {
        tracing::info!("Starting new watch from config: {repo} [{branch}]");
        match start_watch(watches, config, repo, branch, key).await {
            Ok(msg) | Err(msg) => tracing::info!("{msg}"),
        }
    }
}
