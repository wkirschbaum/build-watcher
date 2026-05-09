use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{Mutex, Notify};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::config::AutoDiscoverRule;
use crate::events::EventBus;
use crate::github::{GitHubClient, RepoInfo};
use crate::history::SharedHistory;
use crate::persistence::{DiscoveredRepoSet, Persistence};

use super::repo_poller::RepoPoller;
use super::types::{PersistedWatches, WatchEntry, WatchKey};
use super::{DiscoveredRepos, RateLimitState, SharedConfig, Watches};

/// How often the centralized rate-limit refresh task runs.
const RATE_LIMIT_REFRESH_INTERVAL: Duration = Duration::from_secs(60);
/// How often the repo auto-discovery task re-fetches the repo list from GitHub.
const REPO_DISCOVER_INTERVAL: Duration = Duration::from_secs(60);

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
    pub discovered: super::DiscoveredBranches,
    /// Auto-discovered repos (managed by the repo-discovery task, separate from config).
    pub discovered_repos: DiscoveredRepos,
    /// Notified when config changes so pollers wake early and recompute intervals.
    pub config_changed: Arc<Notify>,
    /// Notified when auto-discover rules change to trigger an immediate discovery cycle.
    pub discover_trigger: Arc<Notify>,
    /// Tracks which repos have an active `RepoPoller` to avoid spawning duplicates.
    active_repo_pollers: Arc<Mutex<HashSet<String>>>,
}

impl WatcherHandle {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cancel: CancellationToken,
        events: EventBus,
        github: Arc<dyn GitHubClient>,
        persistence: Arc<dyn Persistence>,
        history: SharedHistory,
        discovered: super::DiscoveredBranches,
        discovered_repos: DiscoveredRepos,
        config_changed: Arc<Notify>,
    ) -> Self {
        Self {
            tracker: TaskTracker::new(),
            cancel,
            events,
            github,
            persistence,
            history,
            discovered,
            discovered_repos,
            config_changed,
            discover_trigger: Arc::new(Notify::new()),
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
        discovered: handle.discovered.clone(),
        config_changed: handle.config_changed.clone(),
        first_poll: true,
        pr_states: std::collections::HashMap::new(),
        not_found_count: 0,
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
    spawn_repo_discoverer(watches, config, handle, rate_limit);
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

/// Spawn a background task that periodically fetches all accessible repos from
/// GitHub and starts/stops watches based on `auto_discover_rules`.
///
/// Auto-discovered repos are tracked in `discovered_repos.json` — separate from
/// `config.repos` (manually-watched repos). This mirrors how branch discovery
/// works: rules exclusively manage their own set; manual watching is independent.
fn spawn_repo_discoverer(
    watches: &Watches,
    config: &SharedConfig,
    handle: &WatcherHandle,
    rate_limit: &RateLimitState,
) {
    let watches = watches.clone();
    let config = config.clone();
    let handle = handle.clone();
    let rate_limit = rate_limit.clone();
    let token = handle.cancel.child_token();
    let trigger = handle.discover_trigger.clone();

    handle.tracker.clone().spawn(async move {
        loop {
            run_discovery_cycle(&watches, &config, &handle, &rate_limit).await;
            tokio::select! {
                () = tokio::time::sleep(REPO_DISCOVER_INTERVAL) => {}
                () = trigger.notified() => {}
                () = token.cancelled() => return,
            }
        }
    });
}

async fn run_discovery_cycle(
    watches: &Watches,
    config: &SharedConfig,
    handle: &WatcherHandle,
    rate_limit: &RateLimitState,
) {
    // Read config once for everything needed this cycle.
    let (rules, manually_watched): (Vec<AutoDiscoverRule>, HashSet<String>) = {
        let cfg = config.read().await;
        (
            cfg.auto_discover_rules.clone(),
            cfg.repos.keys().cloned().collect(),
        )
    };
    if rules.is_empty() {
        return;
    }

    let repos = match handle.github.list_accessible_repos().await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "Repo auto-discovery: failed to list repos");
            return;
        }
    };

    let now = crate::config::unix_now();
    let matching: DiscoveredRepoSet = repos
        .iter()
        .filter(|r| rules.iter().any(|rule| repo_matches_rule(rule, r, now)))
        .map(|r| r.full_name.clone())
        .collect();

    let old_set: DiscoveredRepoSet = handle.discovered_repos.lock().await.clone();

    let to_add: Vec<String> = matching.difference(&old_set).cloned().collect();
    // Only auto-remove repos not also manually pinned in config.
    let to_remove: Vec<String> = old_set
        .difference(&matching)
        .filter(|r| !manually_watched.contains(*r))
        .cloned()
        .collect();

    if to_add.is_empty() && to_remove.is_empty() {
        return;
    }

    // new_set is built incrementally: repos are only added after watches start
    // successfully, so a transient branch-resolution failure leaves the repo
    // out of discovered_repos and the next cycle retries it.
    let mut new_set = old_set.clone();

    if !to_add.is_empty() {
        tracing::info!(count = to_add.len(), "Repo auto-discovery: adding repos");
        // Read configured branches for all new repos in one lock acquisition.
        let configured_per_repo: Vec<(String, Vec<String>)> = {
            let cfg = config.read().await;
            to_add
                .iter()
                .map(|repo| (repo.clone(), cfg.branches_for(repo)))
                .collect()
        };
        // Resolve default branches in parallel — one API call per repo.
        let mut join_set = tokio::task::JoinSet::new();
        for (repo, configured) in configured_per_repo {
            let github = handle.github.clone();
            join_set.spawn(async move {
                let branches = resolve_branches_for_repo(&*github, &repo, &configured).await;
                (repo, branches)
            });
        }
        while let Some(result) = join_set.join_next().await {
            let (repo, branches) = match result {
                Ok(pair) => pair,
                Err(e) => {
                    tracing::warn!(error = %e, "Branch resolution task panicked during repo discovery");
                    continue;
                }
            };
            if branches.is_empty() {
                tracing::warn!(repo = %repo, "Repo auto-discovery: no branches resolved, will retry next cycle");
                continue;
            }
            for branch in &branches {
                let _ = start_watch(watches, config, handle, rate_limit, &repo, branch).await;
            }
            new_set.insert(repo);
        }
    }

    if !to_remove.is_empty() {
        tracing::info!(
            count = to_remove.len(),
            "Repo auto-discovery: removing stale repos"
        );
        {
            let mut w = watches.lock().await;
            w.retain(|k, _| !to_remove.contains(&k.repo));
        }
        // Clean up branch discovery state so discovered.json doesn't accumulate orphans.
        let updated_disc = {
            let mut disc = handle.discovered.lock().await;
            for repo in &to_remove {
                disc.remove(repo);
                new_set.remove(repo);
            }
            disc.clone()
        };
        if let Err(e) = handle.persistence.save_discovered(&updated_disc).await {
            tracing::warn!(error = %e, "Failed to persist branch discovery cleanup");
        }
    }

    // Persist only when the auto-discovered set actually changed.
    if new_set != old_set {
        *handle.discovered_repos.lock().await = new_set.clone();
        if let Err(e) = handle.persistence.save_discovered_repos(&new_set).await {
            tracing::warn!(error = %e, "Failed to persist discovered repos");
        }
    }
}

fn repo_matches_rule(rule: &AutoDiscoverRule, repo: &RepoInfo, now_unix: u64) -> bool {
    // `org_pattern` (when set) matches against the owner only — kept for
    // backwards compatibility with rules created before the unified filter.
    if rule
        .compiled_org_pattern
        .as_ref()
        .is_some_and(|re| !re.is_match(&repo.owner))
    {
        return false;
    }
    // `repo_pattern` matches against the full "owner/name" path so a single
    // regex like `^myorg/foo-.*$` can filter without a separate org field.
    if rule
        .compiled_repo_pattern
        .as_ref()
        .is_some_and(|re| !re.is_match(&repo.full_name))
    {
        return false;
    }
    if let Some(max_age) = rule.recently_updated.max_age_secs() {
        let Some(pushed_at) = &repo.pushed_at else {
            return false;
        };
        let Some(pushed_epoch) = crate::github::parse_iso_epoch(pushed_at) else {
            return false;
        };
        if now_unix.saturating_sub(pushed_epoch) > max_age {
            return false;
        }
    }
    true
}

/// Resolve the branch list for a single repo: GitHub default branch first,
/// then any configured/discovered branches not already in the list.
pub async fn resolve_branches_for_repo(
    github: &dyn GitHubClient,
    repo: &str,
    configured: &[String],
) -> Vec<String> {
    let mut branches = Vec::new();
    match github.default_branch(repo).await {
        Ok(gh_default) => {
            tracing::info!(repo = %repo, branch = %gh_default, "Resolved default branch");
            branches.push(gh_default);
        }
        Err(e) => tracing::warn!(repo = %repo, error = %e, "Failed to resolve default branch"),
    }
    for b in configured {
        if !branches.contains(b) {
            branches.push(b.clone());
        }
    }
    branches
}

/// Resolve the complete set of WatchKeys from config + discovered state,
/// querying GitHub for the repo default branch where needed.
async fn resolve_config_keys(config: &SharedConfig, handle: &WatcherHandle) -> Vec<WatchKey> {
    let repos: Vec<(String, Vec<String>)> = {
        let discovered_branches = handle.discovered.lock().await;
        let discovered_repos = handle.discovered_repos.lock().await;
        let cfg = config.read().await;
        // Union of manually-configured repos and auto-discovered repos.
        let mut all_repos: Vec<String> = cfg.watched_repos().into_iter().cloned().collect();
        for repo in discovered_repos.iter() {
            if !all_repos.contains(repo) {
                all_repos.push(repo.clone());
            }
        }
        all_repos
            .into_iter()
            .map(|repo| {
                let mut branches = cfg.branches_for(&repo);
                if let Some(disc) = discovered_branches.get(&repo) {
                    for b in disc {
                        if !branches.contains(b) {
                            branches.push(b.clone());
                        }
                    }
                }
                (repo, branches)
            })
            .collect()
    };

    // Resolve default branches in parallel — one API call per repo.
    let mut join_set = tokio::task::JoinSet::new();
    for (repo, configured_branches) in repos {
        let github = handle.github.clone();
        join_set.spawn(async move {
            let branches = resolve_branches_for_repo(&*github, &repo, &configured_branches).await;
            (repo, branches)
        });
    }
    let mut keys = Vec::new();
    while let Some(result) = join_set.join_next().await {
        let (repo, branches) = match result {
            Ok(pair) => pair,
            Err(e) => {
                tracing::warn!(error = %e, "Branch resolution task panicked during startup");
                continue;
            }
        };
        for branch in branches {
            let key = WatchKey::new(&repo, &branch);
            if !keys.contains(&key) {
                keys.push(key);
            }
        }
    }
    keys
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RecentlyUpdated;

    fn make_rule(
        org: Option<&str>,
        repo: Option<&str>,
        recency: RecentlyUpdated,
    ) -> AutoDiscoverRule {
        let mut r = AutoDiscoverRule {
            id: "test".to_string(),
            org_pattern: org.map(String::from),
            repo_pattern: repo.map(String::from),
            recently_updated: recency,
            compiled_org_pattern: None,
            compiled_repo_pattern: None,
        };
        if let Some(p) = &r.org_pattern {
            r.compiled_org_pattern = Some(Arc::new(regex::Regex::new(p).unwrap()));
        }
        if let Some(p) = &r.repo_pattern {
            r.compiled_repo_pattern = Some(Arc::new(regex::Regex::new(p).unwrap()));
        }
        r
    }

    fn make_repo(owner: &str, name: &str) -> RepoInfo {
        RepoInfo {
            full_name: format!("{owner}/{name}"),
            owner: owner.to_string(),
            name: name.to_string(),
            pushed_at: None,
        }
    }

    fn make_repo_with_push(owner: &str, name: &str, pushed_at: &str) -> RepoInfo {
        RepoInfo {
            full_name: format!("{owner}/{name}"),
            owner: owner.to_string(),
            name: name.to_string(),
            pushed_at: Some(pushed_at.to_string()),
        }
    }

    #[test]
    fn repo_pattern_matches_full_owner_name_path() {
        let rule = make_rule(None, Some(r"^myorg/foo-.*$"), RecentlyUpdated::Any);
        assert!(repo_matches_rule(&rule, &make_repo("myorg", "foo-bar"), 0));
        assert!(!repo_matches_rule(&rule, &make_repo("myorg", "bar"), 0));
        assert!(!repo_matches_rule(
            &rule,
            &make_repo("otherorg", "foo-bar"),
            0
        ));
    }

    #[test]
    fn repo_pattern_anchored_to_name_only_no_longer_matches() {
        // Regression guard: a regex anchored only to the name (legacy semantics)
        // should NOT match against the new full-path string.
        let rule = make_rule(None, Some(r"^foo-.*$"), RecentlyUpdated::Any);
        assert!(!repo_matches_rule(&rule, &make_repo("myorg", "foo-bar"), 0));
    }

    #[test]
    fn legacy_org_pattern_still_filters_owner() {
        let rule = make_rule(Some(r"^myorg$"), None, RecentlyUpdated::Any);
        assert!(repo_matches_rule(&rule, &make_repo("myorg", "anything"), 0));
        assert!(!repo_matches_rule(
            &rule,
            &make_repo("otherorg", "anything"),
            0
        ));
    }

    #[test]
    fn org_and_repo_pattern_both_required() {
        let rule = make_rule(Some(r"^myorg$"), Some(r"^myorg/foo$"), RecentlyUpdated::Any);
        assert!(repo_matches_rule(&rule, &make_repo("myorg", "foo"), 0));
        // Owner mismatch: fails on org_pattern.
        assert!(!repo_matches_rule(&rule, &make_repo("other", "foo"), 0));
        // Path mismatch: fails on repo_pattern (the full-path one).
        assert!(!repo_matches_rule(&rule, &make_repo("myorg", "bar"), 0));
    }

    #[test]
    fn unset_patterns_match_anything() {
        let rule = make_rule(None, None, RecentlyUpdated::Any);
        assert!(repo_matches_rule(&rule, &make_repo("any", "thing"), 0));
    }

    #[test]
    fn recency_filter_rejects_stale_repos() {
        let rule = make_rule(None, None, RecentlyUpdated::Week);
        // Repo with no pushed_at fails when recency is set.
        assert!(!repo_matches_rule(
            &rule,
            &make_repo("o", "r"),
            1_700_000_000
        ));
        // Push from 2020 > one week before 2023 → rejected.
        let stale = "2020-01-01T00:00:00Z";
        assert!(!repo_matches_rule(
            &rule,
            &make_repo_with_push("o", "r", stale),
            1_700_000_000
        ));
    }
}
