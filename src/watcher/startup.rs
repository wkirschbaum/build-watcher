use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{Mutex, Notify, Semaphore};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::config::unix_now;
use crate::events::EventBus;
use crate::github::GitHubClient;
use crate::history::{SharedHistory, push_build};
use crate::persistence::Persistence;

use super::repo_poller::RepoPoller;
use super::types::{ActiveRun, PersistedWatches, WatchEntry, WatchKey};
use super::{RateLimitState, SharedConfig, Watches, filter_runs};

/// How often the centralized rate-limit refresh task runs.
const RATE_LIMIT_REFRESH_INTERVAL: Duration = Duration::from_secs(60);

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
        config_changed: Arc<Notify>,
    ) -> Self {
        Self {
            tracker: TaskTracker::new(),
            cancel,
            events,
            github,
            persistence,
            history,
            config_changed,
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

    // No runs at all — register an idle watch so the poller will pick up future builds.
    if all_runs.is_empty() {
        let entry = WatchEntry {
            last_seen_run_id: 0,
            active_runs: HashMap::new(),
            failure_counts: HashMap::new(),
            last_builds: HashMap::new(),
            waiting: true,
        };
        {
            let mut w = watches.lock().await;
            if w.contains_key(&key) {
                return Ok(format!("{repo} [{branch}]: already being watched"));
            }
            w.insert(key.clone(), entry);
        }
        spawn_repo_poller(watches, config, handle, rate_limit, &key.repo).await;
        return Ok(format!(
            "{repo} [{branch}]: no workflow runs yet, watching for new builds"
        ));
    }

    let (workflow_filter, ignored_workflows): (Vec<String>, Vec<String>) = {
        let cfg = config.read().await;
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

    // Seed one last build per workflow from all completed runs (newest-first).
    let last_builds: HashMap<String, crate::github::LastBuild> = runs
        .iter()
        .filter(|r| r.is_completed())
        .fold(HashMap::new(), |mut map, r| {
            let mut lb = r.to_last_build();
            lb.completed_at = Some(unix_now());
            map.entry(lb.workflow.clone()).or_insert(lb);
            map
        });
    let entry = WatchEntry {
        last_seen_run_id: max_id,
        active_runs: active,
        failure_counts: HashMap::new(),
        last_builds: last_builds.clone(),
        waiting: false,
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
    for lb in last_builds.into_values() {
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

/// Start watches for all repos/branches defined in config.
///
/// Config is the single source of truth for what to watch. watches.json provides
/// runtime state (last_seen_run_id, last_builds) for repos that exist in config;
/// entries in watches.json that are not in config are ignored (stale state).
pub async fn startup_watches(
    watches: &Watches,
    config: &SharedConfig,
    handle: &WatcherHandle,
    rate_limit: &RateLimitState,
    persisted: PersistedWatches,
) {
    // Build the set of WatchKeys from config, resolving branches via GitHub.
    let config_keys = resolve_config_keys(config, handle).await;

    // Seed in-memory watches from persisted state for keys that exist in config.
    {
        let mut w = watches.lock().await;
        for key in &config_keys {
            if w.contains_key(key) {
                continue;
            }
            let entry = match persisted.get(key) {
                Some(p) => WatchEntry::from_persisted(p.clone()),
                None => WatchEntry::default(),
            };
            w.insert(key.clone(), entry);
        }
    }

    // Recover active runs from GitHub and spawn pollers.
    let snapshot: Vec<WatchKey> = {
        let w = watches.lock().await;
        w.keys().cloned().collect()
    };
    recover_watches(watches, config, handle, rate_limit, &snapshot).await;
    spawn_rate_limit_refresher(handle, rate_limit);
}

/// Spawn a single background task that refreshes the shared rate-limit state
/// every 60 seconds instead of each `RepoPoller` doing it independently.
fn spawn_rate_limit_refresher(handle: &WatcherHandle, rate_limit: &RateLimitState) {
    let gh = handle.github.clone();
    let rl = rate_limit.clone();
    let token = handle.cancel.child_token();
    handle.tracker.spawn(async move {
        loop {
            match gh.rate_limit().await {
                Ok(new_rl) => {
                    tracing::debug!(
                        remaining = new_rl.remaining,
                        limit = new_rl.limit,
                        "Rate limit refreshed"
                    );
                    *rl.lock().await = Some(new_rl);
                }
                Err(e) => {
                    tracing::warn!(error = %e, "Failed to fetch rate limit");
                }
            }
            tokio::select! {
                () = tokio::time::sleep(RATE_LIMIT_REFRESH_INTERVAL) => {}
                () = token.cancelled() => return,
            }
        }
    });
}

/// Resolve the complete set of WatchKeys from config, querying GitHub for
/// default branches where needed.
async fn resolve_config_keys(config: &SharedConfig, handle: &WatcherHandle) -> Vec<WatchKey> {
    let repos: Vec<(String, bool)> = {
        let cfg = config.read().await;
        cfg.watched_repos()
            .into_iter()
            .map(|repo| {
                let has_explicit = cfg.has_explicit_branches(repo);
                (repo.to_string(), has_explicit)
            })
            .collect()
    };

    let mut keys = Vec::new();
    for (repo, has_explicit) in &repos {
        let mut branches = Vec::new();

        match handle.github.default_branch(repo).await {
            Ok(gh_default) => {
                branches.push(gh_default);
            }
            Err(e) => {
                tracing::warn!(
                    repo = %repo, error = %e,
                    "Failed to resolve default branch on startup, falling back to config"
                );
                let cfg = config.read().await;
                branches.extend(cfg.branches_for(repo).iter().cloned());
            }
        }

        if *has_explicit {
            let cfg = config.read().await;
            for b in cfg.branches_for(repo) {
                if !branches.contains(b) {
                    branches.push(b.clone());
                }
            }
        }

        for branch in &branches {
            let key = WatchKey::new(repo, branch);
            if !keys.contains(&key) {
                keys.push(key);
            }
        }
    }

    keys
}

/// Maximum concurrent GitHub API requests during startup recovery.
const MAX_CONCURRENT_RECOVERY: usize = 10;

/// Recover active runs from GitHub for all watches and spawn pollers.
///
/// Makes one `recent_runs_for_repo` call per unique repo (instead of one
/// `recent_runs` per branch), then fans results to per-branch entries.
/// With 500 branches across 5 repos this saves ~495 API calls at startup.
pub(super) async fn recover_watches(
    watches: &Watches,
    config: &SharedConfig,
    handle: &WatcherHandle,
    rate_limit: &RateLimitState,
    snapshot: &[WatchKey],
) {
    // Group branches by repo.
    let mut repos: HashMap<String, Vec<&WatchKey>> = HashMap::new();
    for key in snapshot {
        repos.entry(key.repo.clone()).or_default().push(key);
    }

    let semaphore = Arc::new(Semaphore::new(MAX_CONCURRENT_RECOVERY));
    let mut set = tokio::task::JoinSet::new();
    for (repo, keys) in &repos {
        let branch_count = keys.len() as u32;
        tracing::info!(repo, branches = branch_count, "Resuming watches for repo");
        let repo = repo.clone();
        let gh = handle.github.clone();
        let sem = semaphore.clone();
        let limit = super::scaled_repo_limit(branch_count);
        set.spawn(async move {
            let _permit = sem.acquire().await;
            let result = gh.recent_runs_for_repo(&repo, limit).await;
            (repo, result)
        });
    }

    while let Some(join_result) = set.join_next().await {
        let Ok((repo, result)) = join_result else {
            tracing::error!("Recovery task panicked");
            continue;
        };
        let runs = match result {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(repo, error = %e, "Could not recover runs for repo");
                continue;
            }
        };

        let (workflow_filter, ignored_workflows) = {
            let cfg = config.read().await;
            (
                cfg.workflows_for(&repo).to_vec(),
                cfg.ignored_workflows.clone(),
            )
        };

        let now = Instant::now();
        let mut w = watches.lock().await;
        for key in repos.get(&repo).into_iter().flatten() {
            let Some(entry) = w.get_mut(key) else {
                continue;
            };
            let branch_runs = super::runs_for_branch(&runs, &key.branch);
            let filtered = filter_runs(&branch_runs, &workflow_filter, &ignored_workflows);

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
            // Bump high-water mark from all runs for this branch (not just
            // filtered) so check_for_new_runs doesn't re-notify for ignored workflows.
            if let Some(max_id) = branch_runs.iter().map(|r| r.id).max() {
                entry.last_seen_run_id = entry.last_seen_run_id.max(max_id);
            }
        }
    }

    // Spawn one RepoPoller per unique repo.
    for repo in repos.keys() {
        spawn_repo_poller(watches, config, handle, rate_limit, repo).await;
    }
}
