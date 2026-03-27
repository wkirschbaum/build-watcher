mod poller;
mod startup;
mod types;

#[cfg(test)]
mod tests;

use std::collections::HashMap;
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
/// Each watch makes 1 `gh run list` call per idle cycle to check for new runs.
/// Each active run adds 1 `gh run view` call per active cycle.
/// This gives the rate limiter an accurate picture of actual API consumption.
pub fn count_api_calls(watches: &HashMap<WatchKey, WatchEntry>) -> u64 {
    let base_calls = watches.len() as u64; // 1 gh run list per watch
    let active_run_calls: u64 = watches.values().map(|e| e.active_runs.len() as u64).sum();
    base_calls + active_run_calls
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
