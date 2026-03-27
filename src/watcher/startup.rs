use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use tokio::sync::{Mutex, Notify};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::config::unix_now;
use crate::events::EventBus;
use crate::github::GitHubClient;
use crate::history::{SharedHistory, push_build};
use crate::persistence::Persistence;

use super::repo_poller::RepoPoller;
use super::types::{ActiveRun, WatchEntry, WatchKey};
use super::{RateLimitState, SharedConfig, Watches, filter_runs};

// -- Watcher handle --

/// Shared handle for managing poller lifecycle.
#[derive(Clone)]
pub struct WatcherHandle {
    pub tracker: TaskTracker,
    pub cancel: CancellationToken,
    pub events: EventBus,
    pub github: Arc<dyn GitHubClient>,
    pub persistence: Arc<dyn Persistence>,
    pub history: SharedHistory,
    /// Notified when config changes so pollers wake early and recompute intervals.
    pub config_changed: Arc<Notify>,
    /// Tracks which repos have an active `RepoPoller` to avoid spawning duplicates.
    active_repo_pollers: Arc<Mutex<HashSet<String>>>,
}

impl WatcherHandle {
    pub fn new(
        cancel: CancellationToken,
        events: EventBus,
        github: Arc<dyn GitHubClient>,
        persistence: Arc<dyn Persistence>,
        history: SharedHistory,
    ) -> Self {
        Self {
            tracker: TaskTracker::new(),
            cancel,
            events,
            github,
            persistence,
            history,
            config_changed: Arc::new(Notify::new()),
            active_repo_pollers: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    pub async fn shutdown(&self) {
        self.tracker.close();
        self.tracker.wait().await;
    }
}

// -- Starting watches --

#[tracing::instrument(skip(watches, config, handle, rate_limit), fields(%repo, %branch))]
pub async fn start_watch(
    watches: &Watches,
    config: &SharedConfig,
    handle: &WatcherHandle,
    rate_limit: &RateLimitState,
    repo: &str,
    branch: &str,
) -> std::result::Result<String, String> {
    let key = WatchKey::new(repo, branch);
    {
        let w = watches.lock().await;
        if w.contains_key(&key) {
            return Ok(format!("{repo} [{branch}]: already being watched"));
        }
    }

    let all_runs = handle
        .github
        .recent_runs(repo, branch)
        .await
        .map_err(|e| e.to_string())?;
    if all_runs.is_empty() {
        return Err(format!("{repo} [{branch}]: no workflow runs found"));
    }

    let (workflow_filter, ignored_workflows): (Vec<String>, Vec<String>) = {
        let cfg = config.lock().await;
        (
            cfg.workflows_for(repo).to_vec(),
            cfg.ignored_workflows.clone(),
        )
    };
    let runs = filter_runs(&all_runs, &workflow_filter, &ignored_workflows);
    if runs.is_empty() {
        return Err(format!(
            "{repo} [{branch}]: no runs match workflow filter {workflow_filter:?}"
        ));
    }

    let Some(max_id) = runs.iter().map(|r| r.id).max() else {
        return Err(format!(
            "{repo} [{branch}]: filtered runs unexpectedly empty"
        ));
    };
    let now = Instant::now();
    let active: HashMap<u64, ActiveRun> = runs
        .iter()
        .filter(|r| !r.is_completed())
        .map(|r| (r.id, ActiveRun::from_run(r, now)))
        .collect();
    let last_completed = runs.iter().find(|r| r.is_completed());

    let msg = if active.is_empty() {
        let latest = runs[0];
        format!(
            "{repo} [{branch}]: latest build already completed ({}), watching for new builds\n  {}: {}\n  {}",
            latest.conclusion,
            latest.workflow,
            latest.display_title(),
            latest.url(repo),
        )
    } else {
        format!(
            "{repo} [{branch}]: watching {} active build(s)",
            active.len()
        )
    };

    let last_build = last_completed.map(|r| {
        let mut lb = (*r).to_last_build();
        lb.completed_at = Some(unix_now());
        lb
    });
    let entry = WatchEntry {
        last_seen_run_id: max_id,
        active_runs: active,
        failure_counts: HashMap::new(),
        last_build: last_build.clone(),
    };

    {
        let mut w = watches.lock().await;
        // Re-check: a concurrent call may have inserted while we queried GitHub.
        if w.contains_key(&key) {
            return Ok(format!("{repo} [{branch}]: already being watched"));
        }
        w.insert(key.clone(), entry);
    }

    // Seed history with the initial completed build (in-memory; caller persists).
    if let Some(lb) = last_build {
        let mut hist = handle.history.lock().await;
        push_build(&mut hist, &key, lb);
    }

    // Persistence is the caller's responsibility — start_watch only updates
    // in-memory state and spawns the poller.

    spawn_repo_poller(watches, config, handle, rate_limit, &key.repo).await;
    Ok(msg)
}

/// Spawn a `RepoPoller` for `repo` if one isn't already running.
/// If a poller already exists, notifies it via `config_changed` so it picks up
/// the new branch on its next cycle.
pub(super) async fn spawn_repo_poller(
    watches: &Watches,
    config: &SharedConfig,
    handle: &WatcherHandle,
    rate_limit: &RateLimitState,
    repo: &str,
) {
    let mut active = handle.active_repo_pollers.lock().await;
    if active.contains(repo) {
        // Poller already running — wake it so it picks up the new branch.
        handle.config_changed.notify_waiters();
        return;
    }
    active.insert(repo.to_string());
    drop(active);

    let pollers = handle.active_repo_pollers.clone();
    let repo_owned = repo.to_string();
    let poller = RepoPoller {
        repo: repo.to_string(),
        watches: watches.clone(),
        config: config.clone(),
        rate_limit: rate_limit.clone(),
        token: handle.cancel.child_token(),
        events: handle.events.clone(),
        github: handle.github.clone(),
        persistence: handle.persistence.clone(),
        history: handle.history.clone(),
        config_changed: handle.config_changed.clone(),
        last_active_secs: 0,
    };
    handle.tracker.spawn(async move {
        poller.run().await;
        // Clean up when the poller exits.
        pollers.lock().await.remove(&repo_owned);
    });
}

// -- Startup --

pub async fn startup_watches(
    watches: &Watches,
    config: &SharedConfig,
    handle: &WatcherHandle,
    rate_limit: &RateLimitState,
) {
    let snapshot: Vec<WatchKey> = {
        let w = watches.lock().await;
        w.keys().cloned().collect()
    };

    recover_existing_watches(watches, config, handle, rate_limit, &snapshot).await;
    start_new_config_watches(watches, config, handle, rate_limit, &snapshot).await;
}

/// Resume persisted watches and recover any in-progress runs from GitHub.
pub(super) async fn recover_existing_watches(
    watches: &Watches,
    config: &SharedConfig,
    handle: &WatcherHandle,
    rate_limit: &RateLimitState,
    snapshot: &[WatchKey],
) {
    let mut set = tokio::task::JoinSet::new();
    for key in snapshot {
        tracing::info!(key = %key, "Resuming watch");
        let key = key.clone();
        let gh = handle.github.clone();
        set.spawn(async move {
            let result = gh.recent_runs(&key.repo, &key.branch).await;
            (key, result)
        });
    }

    while let Some(join_result) = set.join_next().await {
        let Ok((key, result)) = join_result else {
            tracing::error!("Recovery task panicked");
            continue;
        };
        if let Ok(runs) = result {
            let (workflow_filter, ignored_workflows) = {
                let cfg = config.lock().await;
                (
                    cfg.workflows_for(&key.repo).to_vec(),
                    cfg.ignored_workflows.clone(),
                )
            };
            let filtered = filter_runs(&runs, &workflow_filter, &ignored_workflows);

            let now = Instant::now();
            let mut w = watches.lock().await;
            if let Some(entry) = w.get_mut(&key) {
                for run in &filtered {
                    if !run.is_completed() && !entry.active_runs.contains_key(&run.id) {
                        tracing::info!(key = %key, run_id = run.id, "Recovering in-progress run");
                        // Note: started_at is approximate — the actual GitHub start
                        // time is lost across restarts, so elapsed time in the
                        // completion notification may be inaccurate for recovered runs.
                        entry
                            .active_runs
                            .insert(run.id, ActiveRun::from_run(run, now));
                    }
                }
                // Bump high-water mark from all runs (not just filtered) so
                // check_for_new_runs doesn't re-notify for ignored workflows.
                if let Some(max_id) = runs.iter().map(|r| r.id).max() {
                    entry.last_seen_run_id = entry.last_seen_run_id.max(max_id);
                }
            }
        } else if let Err(e) = &result {
            tracing::warn!(key = %key, error = %e, "Could not recover runs");
        }
    }

    // Spawn one RepoPoller per unique repo.
    let unique_repos: HashSet<String> = snapshot.iter().map(|k| k.repo.clone()).collect();
    for repo in unique_repos {
        spawn_repo_poller(watches, config, handle, rate_limit, &repo).await;
    }
}

/// Start watches for config repos that don't have persisted state yet.
async fn start_new_config_watches(
    watches: &Watches,
    config: &SharedConfig,
    handle: &WatcherHandle,
    rate_limit: &RateLimitState,
    snapshot: &[WatchKey],
) {
    let new_keys: Vec<WatchKey> = {
        let cfg = config.lock().await;
        cfg.watched_repos()
            .into_iter()
            .flat_map(|repo| {
                cfg.branches_for(repo)
                    .iter()
                    .filter_map(|branch| {
                        let key = WatchKey::new(repo, branch);
                        (!snapshot.contains(&key)).then_some(key)
                    })
                    .collect::<Vec<_>>()
            })
            .collect()
    };

    let mut set = tokio::task::JoinSet::new();
    for key in new_keys {
        tracing::info!(
            repo = key.repo,
            branch = key.branch,
            "Starting new watch from config"
        );
        let watches = watches.clone();
        let config = config.clone();
        let handle = handle.clone();
        let rate_limit = rate_limit.clone();
        set.spawn(async move {
            match start_watch(
                &watches,
                &config,
                &handle,
                &rate_limit,
                &key.repo,
                &key.branch,
            )
            .await
            {
                Ok(msg) | Err(msg) => tracing::info!("{msg}"),
            }
        });
    }

    while set.join_next().await.is_some() {}
}
