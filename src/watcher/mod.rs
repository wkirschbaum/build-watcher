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
    let unique_repos: HashSet<&str> = watches.keys().map(|k| k.repo.as_str()).collect();
    let base_calls = unique_repos.len() as u64;
    let repos_with_active = unique_repos
        .iter()
        .filter(|repo| {
            watches
                .iter()
                .any(|(k, e)| k.repo == **repo && e.has_active_runs())
        })
        .count() as u64;
    base_calls + repos_with_active
}

/// Filter runs by branch name. Used to fan out repo-wide results to per-branch watchers.
pub(super) fn runs_for_branch<'a>(runs: &'a [RunInfo], branch: &str) -> Vec<&'a RunInfo> {
    runs.iter().filter(|r| r.head_branch == branch).collect()
}

/// Filter runs by workflow allow-list and ignore-list. Case-insensitive matching.
fn filter_runs<'a>(
    runs: &'a [RunInfo],
    workflows: &[String],
    ignored: &[String],
) -> Vec<&'a RunInfo> {
    runs.iter()
        .filter(|r| !ignored.iter().any(|i| r.workflow.eq_ignore_ascii_case(i)))
        .filter(|r| {
            workflows.is_empty() || workflows.iter().any(|w| r.workflow.eq_ignore_ascii_case(w))
        })
        .collect()
}
