pub(crate) mod repo_poller;
mod startup;
mod types;

#[cfg(test)]
mod tests;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use tokio::sync::Mutex;
use tokio::time::Instant;

use crate::config::Config;
use crate::github::{RateLimit, RunInfo};

pub type SharedConfig = Arc<Mutex<Config>>;
pub type Watches = Arc<Mutex<HashMap<WatchKey, WatchEntry>>>;
pub type PauseState = Arc<Mutex<Option<Instant>>>;
pub type RateLimitState = Arc<Mutex<Option<RateLimit>>>;

pub use startup::{WatcherHandle, start_watch, startup_watches};
pub use types::{
    ActiveRun, PersistedWatch, WatchEntry, WatchKey, collect_persisted, last_failed_build,
    load_watches,
};

/// Returns `true` if notifications are currently paused (deadline is in the future).
pub async fn is_paused(pause: &PauseState) -> bool {
    let p = pause.lock().await;
    p.is_some_and(|deadline| Instant::now() < deadline)
}

/// Count the expected API calls per poll cycle across all watches.
///
/// With per-repo polling:
/// - 1 `recent_runs_for_repo` call per unique repo (new-run detection)
/// - 1 `in_progress_runs_for_repo` call per repo that has any active runs (batch status check)
/// - Occasional fallback `run_status` calls are unpredictable and not budgeted.
pub fn count_api_calls(watches: &HashMap<WatchKey, WatchEntry>) -> u64 {
    let mut all_repos = HashSet::new();
    let mut repos_with_active = HashSet::new();
    for (k, e) in watches {
        all_repos.insert(k.repo.as_str());
        if e.has_active_runs() {
            repos_with_active.insert(k.repo.as_str());
        }
    }
    all_repos.len() as u64 + repos_with_active.len() as u64
}

/// Maximum limit for `recent_runs_for_repo` calls, even with many branches.
const MAX_REPO_LIMIT: u32 = 200;

/// Compute a dynamic `--limit` for `recent_runs_for_repo` based on how many
/// branches are watched. With few branches the default (20) suffices; with
/// many branches we scale up so that runs on quiet branches aren't missed.
pub(super) fn scaled_repo_limit(branch_count: u32) -> u32 {
    use crate::github::DEFAULT_REPO_LIMIT;
    (branch_count * 3).clamp(DEFAULT_REPO_LIMIT, MAX_REPO_LIMIT)
}

/// Filter runs by branch name. Used to fan out repo-wide results to per-branch watchers.
pub(super) fn runs_for_branch<'a>(runs: &'a [RunInfo], branch: &str) -> Vec<&'a RunInfo> {
    runs.iter().filter(|r| r.head_branch == branch).collect()
}

/// Filter runs by workflow allow-list and ignore-list. Case-insensitive matching.
fn filter_runs<'a, R: std::borrow::Borrow<RunInfo> + 'a>(
    runs: &'a [R],
    workflows: &[String],
    ignored: &[String],
) -> Vec<&'a RunInfo> {
    runs.iter()
        .map(|r| r.borrow())
        .filter(|r| !ignored.iter().any(|i| r.workflow.eq_ignore_ascii_case(i)))
        .filter(|r| {
            workflows.is_empty() || workflows.iter().any(|w| r.workflow.eq_ignore_ascii_case(w))
        })
        .collect()
}
