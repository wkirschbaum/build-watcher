use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{Mutex, Notify};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::events::EventBus;
use crate::github::GitHubClient;
use crate::history::SharedHistory;
use crate::persistence::Persistence;

use super::repo_poller::RepoPoller;
use super::types::{PersistedWatches, WatchEntry, WatchKey};
use super::{RateLimitState, SharedConfig, Watches};

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

/// Register a watch entry and ensure a poller is running. The entry starts in
/// `waiting` state — the poller's first cycle (after ~1 s) fetches initial data
/// from GitHub and clears the flag.
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
        let mut w = watches.lock().await;
        if w.contains_key(&key) {
            return Ok(format!("{repo} [{branch}]: already being watched"));
        }
        w.insert(
            key.clone(),
            WatchEntry {
                waiting: true,
                ..Default::default()
            },
        );
    }

    spawn_repo_poller(watches, config, handle, rate_limit, &key.repo).await;
    Ok(format!("{repo} [{branch}]: watching"))
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
        first_poll: true,
        pr_states: std::collections::HashMap::new(),
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
///
/// Entries are inserted in `waiting` state so they appear in the TUI immediately.
/// The poller's first cycle (after ~1 s) fetches current data from GitHub and
/// clears the waiting flag.
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
    // Entries without persisted data start as `waiting`.
    let mut repos_to_poll: HashSet<String> = HashSet::new();
    {
        let mut w = watches.lock().await;
        for key in &config_keys {
            if w.contains_key(key) {
                continue;
            }
            let entry = match persisted.get(key) {
                Some(p) => WatchEntry::from_persisted(p.clone()),
                None => WatchEntry {
                    waiting: true,
                    ..Default::default()
                },
            };
            w.insert(key.clone(), entry);
            repos_to_poll.insert(key.repo.clone());
        }
    }

    // Spawn one poller per unique repo — first cycle runs after ~1 s.
    for repo in &repos_to_poll {
        spawn_repo_poller(watches, config, handle, rate_limit, repo).await;
    }
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
    let repos: Vec<(String, Vec<String>)> = {
        let cfg = config.read().await;
        cfg.watched_repos()
            .into_iter()
            .map(|repo| {
                let branches = cfg.branches_for(repo);
                (repo.to_string(), branches)
            })
            .collect()
    };

    let mut keys = Vec::new();
    for (repo, configured_branches) in &repos {
        let mut branches = Vec::new();

        // Always resolve the GitHub default branch.
        match handle.github.default_branch(repo).await {
            Ok(gh_default) => branches.push(gh_default),
            Err(e) => {
                tracing::warn!(
                    repo = %repo, error = %e,
                    "Failed to resolve default branch on startup"
                );
            }
        }

        // Add any explicitly configured branches.
        for b in configured_branches {
            if !branches.contains(b) {
                branches.push(b.clone());
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
