use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::config::{Config, NotificationConfig, load_json, save_json, state_dir};
use crate::github::{GhError, LastBuild, RunInfo, gh_recent_runs, gh_run_status};
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

fn save_persisted(watches: &PersistedWatches) -> Result<(), crate::config::PersistError> {
    save_json(state_dir().join("watches.json"), watches)
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
    if let Err(e) = save_persisted(&persisted) {
        tracing::error!("Failed to save watches: {e}");
    }
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

/// Shared handle for managing poller lifecycle.
#[derive(Clone)]
pub struct WatcherHandle {
    pub tracker: TaskTracker,
    pub cancel: CancellationToken,
}

impl WatcherHandle {
    pub fn new(cancel: CancellationToken) -> Self {
        Self {
            tracker: TaskTracker::new(),
            cancel,
        }
    }

    /// Wait for all pollers to finish (call after cancellation).
    pub async fn shutdown(&self) {
        self.tracker.close();
        self.tracker.wait().await;
    }
}

// -- Watch logic --

#[tracing::instrument(skip(watches, config, handle), fields(%repo, %branch))]
pub async fn start_watch(
    watches: &Watches,
    config: &SharedConfig,
    handle: &WatcherHandle,
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

    let runs = gh_recent_runs(repo, branch)
        .await
        .map_err(|e| e.to_string())?;
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

    spawn_poller(watches.clone(), config.clone(), handle, key.to_string());

    Ok(msg)
}

fn spawn_poller(watches: Watches, config: SharedConfig, handle: &WatcherHandle, key: String) {
    let token = handle.cancel.child_token();
    handle.tracker.spawn(async move {
        poll_repo(watches, config, key, token).await;
    });
}

async fn notify_build_complete(
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
    )
    .await;
}

#[tracing::instrument(skip_all, fields(key))]
async fn poll_repo(watches: Watches, config: SharedConfig, key: String, token: CancellationToken) {
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

        // Cancellation-aware sleep: wake immediately on token cancel
        tokio::select! {
            () = tokio::time::sleep(Duration::from_secs(delay)) => {}
            () = token.cancelled() => {
                tracing::info!(key, "Shutting down poller");
                return;
            }
        }

        // Check if still watched
        {
            let w = watches.lock().await;
            if !w.contains_key(&key) {
                tracing::info!(key, "Watch cancelled");
                return;
            }
        }

        // Poll active runs every cycle
        if has_active {
            poll_active_runs(&watches, &key, &repo, &branch, &notif, &token).await;
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
    token: &CancellationToken,
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
        if token.is_cancelled() {
            return;
        }

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
                handle_poll_failure(watches, key, run_id, &e, &mut changed).await;
                continue;
            }
        };

        if run.is_completed() {
            // Check watch still exists before notifying (race with stop_watches)
            let still_watched = {
                let w = watches.lock().await;
                w.contains_key(key)
            };
            if still_watched {
                notify_build_complete(&run, repo, branch, key, notif).await;
            }

            tracing::info!(
                key,
                run_id,
                sha = run.short_sha(),
                conclusion = %run.conclusion,
                "Build completed"
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
                    run_id,
                    old = %old_status,
                    new = %run.status,
                    "Run status changed"
                );
                entry.active_runs.insert(run_id, run.status);
            }
        }
    }

    if changed {
        save_watches(watches).await;
    }
}

async fn handle_poll_failure(
    watches: &Watches,
    key: &str,
    run_id: u64,
    error: &GhError,
    changed: &mut bool,
) {
    let mut w = watches.lock().await;
    if let Some(entry) = w.get_mut(key) {
        let count = entry.failure_counts.entry(run_id).or_insert(0);
        *count += 1;
        if *count >= MAX_GH_FAILURES {
            tracing::warn!(
                key,
                run_id,
                count,
                "Removing run after consecutive failures"
            );
            entry.active_runs.remove(&run_id);
            entry.failure_counts.remove(&run_id);
            *changed = true;
        } else {
            tracing::error!(key, run_id, count, error = %error, "Poll failure");
        }
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
            tracing::error!(key, error = %e, "Failed to check for new runs");
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

    // Check watch still exists before sending any notifications
    {
        let w = watches.lock().await;
        if !w.contains_key(key) {
            return;
        }
    }

    for run in &new_runs {
        tracing::info!(
            key,
            run_id = run.id,
            sha = run.short_sha(),
            workflow = %run.workflow,
            title = %run.title,
            "New build detected"
        );
        let group = format!("{key}#{}", run.workflow);
        platform::send_notification(
            &format!("🔨 {} - started", run.workflow),
            &format!("[{branch}] {}", run.title),
            notif.build_started,
            Some(&run.url(repo)),
            Some(&group),
        )
        .await;

        // If it already completed between polls, also notify completion
        if run.is_completed() {
            notify_build_complete(run, repo, branch, key, notif).await;
            tracing::info!(
                key,
                run_id = run.id,
                sha = run.short_sha(),
                conclusion = %run.conclusion,
                "Build already completed"
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

pub async fn startup_watches(watches: &Watches, config: &SharedConfig, handle: &WatcherHandle) {
    // Resume existing watches — recover any in-progress builds that were active at shutdown
    let snapshot: Vec<String> = {
        let w = watches.lock().await;
        w.keys().cloned().collect()
    };

    // Recover in-progress runs concurrently
    let mut recover_futures = Vec::new();
    for key in &snapshot {
        let (repo, branch) = parse_watch_key(key);
        tracing::info!(key, "Resuming watch");
        let repo = repo.to_string();
        let branch = branch.to_string();
        let key = key.clone();
        recover_futures.push(async move { (key, gh_recent_runs(&repo, &branch).await) });
    }

    let results = futures::future::join_all(recover_futures).await;

    for (key, result) in results {
        match result {
            Ok(runs) => {
                let mut w = watches.lock().await;
                if let Some(entry) = w.get_mut(&key) {
                    for run in &runs {
                        if !run.is_completed() && !entry.active_runs.contains_key(&run.id) {
                            tracing::info!(key, run_id = run.id, "Recovering in-progress run");
                            entry.active_runs.insert(run.id, run.status.clone());
                        }
                    }
                }
            }
            Err(e) => tracing::warn!(key, error = %e, "Could not recover runs"),
        }

        spawn_poller(watches.clone(), config.clone(), handle, key);
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

    // Start new watches concurrently
    let mut new_futures = Vec::new();
    for (repo, branch, key) in &new_watches {
        tracing::info!(repo, branch, "Starting new watch from config");
        let watches = watches.clone();
        let config = config.clone();
        let handle = handle.clone();
        let repo = repo.clone();
        let branch = branch.clone();
        let key = key.clone();
        new_futures.push(async move {
            match start_watch(&watches, &config, &handle, &repo, &branch, &key).await {
                Ok(msg) | Err(msg) => tracing::info!("{msg}"),
            }
        });
    }

    futures::future::join_all(new_futures).await;
}
