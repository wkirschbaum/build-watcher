use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::config::{Config, load_json, state_dir};
use crate::events::{EventBus, RunSnapshot, WatchEvent};
use crate::github::{GhError, GitHubClient, LastBuild, RateLimit, RunInfo};
use crate::persistence::Persistence;

pub type SharedConfig = Arc<Mutex<Config>>;
pub type Watches = Arc<Mutex<HashMap<WatchKey, WatchEntry>>>;
pub type PauseState = Arc<Mutex<Option<Instant>>>;
pub type RateLimitState = Arc<Mutex<Option<RateLimit>>>;

/// Returns `true` if notifications are currently paused (deadline is in the future).
pub async fn is_paused(pause: &PauseState) -> bool {
    let p = pause.lock().await;
    p.is_some_and(|deadline| Instant::now() < deadline)
}

/// Fastest permitted polling interval when active runs exist.
pub const MIN_ACTIVE_SECS: u64 = 15;
/// Fastest permitted polling interval when no active runs exist.
pub const MIN_IDLE_SECS: u64 = 60;
/// Fallback intervals used before the first rate-limit fetch succeeds.
pub const FALLBACK_ACTIVE_SECS: u64 = 30;
pub const FALLBACK_IDLE_SECS: u64 = 120;
/// How often each poller refreshes the shared rate limit state.
const RATE_LIMIT_REFRESH_INTERVAL: Duration = Duration::from_secs(60);

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

/// Compute dynamic polling intervals based on the current GitHub rate limit.
///
/// `api_calls_per_cycle` is the total expected GitHub API calls per poll cycle
/// across all watches (from `count_api_calls`). This accounts for both the
/// base `gh run list` call per watch and the `gh run view` call per active run.
///
/// Strategy:
/// - No data yet → conservative fallback.
/// - More than 50% of the limit remains → floor speed scaled by call count.
/// - Below 50% → throttle: spread the remaining budget evenly across the
///   seconds until the window resets, then floor at MIN values.
///
/// The 50% threshold gives a comfortable safety margin: it lets polling run
/// at full speed for the first half of the window, then gradually backs off
/// so the remaining budget lasts exactly to the reset.
///
/// Above 50%, floor intervals scale as `MIN_*_SECS × (isqrt(calls))`
/// — square-root growth keeps latency low for typical setups (≤10 watches)
/// while gently slowing things down as the call count grows.
pub fn compute_intervals(
    rate_limit: Option<&RateLimit>,
    api_calls_per_cycle: u64,
    now: u64,
) -> (u64, u64) {
    let calls = api_calls_per_cycle.max(1);
    let Some(rl) = rate_limit else {
        return (FALLBACK_ACTIVE_SECS, FALLBACK_IDLE_SECS);
    };

    // Above 50%: scale floor intervals by sqrt(calls).
    // 1 call → ×1, 2–3 → ×1, 4–8 → ×2, 9–15 → ×3, 16–24 → ×4, …
    if rl.remaining * 2 > rl.limit {
        let scale = (calls as f64).sqrt() as u64;
        let scale = scale.max(1);
        return (MIN_ACTIVE_SECS * scale, MIN_IDLE_SECS * scale);
    }

    let seconds_until_reset = rl.reset.saturating_sub(now).max(1);

    // Spread remaining budget evenly: min_interval = calls * secs / remaining.
    // checked_div handles remaining == 0 by waiting out the full reset window.
    let rate_limited_secs = (calls * seconds_until_reset)
        .checked_div(rl.remaining)
        .unwrap_or(seconds_until_reset);

    (
        MIN_ACTIVE_SECS.max(rate_limited_secs),
        MIN_IDLE_SECS.max(rate_limited_secs),
    )
}

/// Runtime state for an in-progress run, including when we first saw it.
#[derive(Debug, Clone)]
pub struct ActiveRun {
    pub status: String,
    pub started_at: Instant,
    pub workflow: String,
    pub title: String,
    pub event: String,
}

impl ActiveRun {
    fn from_run(run: &RunInfo, now: Instant) -> Self {
        Self {
            status: run.status.clone(),
            started_at: now,
            workflow: run.workflow.clone(),
            title: run.title.clone(),
            event: run.event.clone(),
        }
    }

    pub fn display_title(&self) -> String {
        crate::github::display_title(&self.event, &self.title)
    }
}

// -- Watch key --

/// Fallback branch for legacy persisted keys that lack `#branch`.
/// All current keys include it; this only guards against hand-edited JSON.
const FALLBACK_BRANCH: &str = "main";

/// Type-safe watch key combining repo and branch.
/// Serializes as `"owner/repo#branch"` for persistence compatibility.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct WatchKey {
    pub repo: String,
    pub branch: String,
}

impl WatchKey {
    pub fn new(repo: &str, branch: &str) -> Self {
        Self {
            repo: repo.to_string(),
            branch: branch.to_string(),
        }
    }

    /// Parse from the persisted `"owner/repo#branch"` format.
    fn parse(s: &str) -> Self {
        match s.rsplit_once('#') {
            Some((repo, branch)) => Self::new(repo, branch),
            None => Self::new(s, FALLBACK_BRANCH),
        }
    }

    pub fn matches_repo(&self, repo: &str) -> bool {
        self.repo == repo
    }
}

impl std::fmt::Display for WatchKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}#{}", self.repo, self.branch)
    }
}

impl Serialize for WatchKey {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for WatchKey {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Ok(Self::parse(&s))
    }
}

// -- Watch state persistence --

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedWatch {
    pub(crate) last_seen_run_id: u64,
    #[serde(default)]
    pub(crate) last_build: Option<LastBuild>,
}

pub(crate) type PersistedWatches = HashMap<WatchKey, PersistedWatch>;

pub fn load_watches() -> HashMap<WatchKey, WatchEntry> {
    let persisted: PersistedWatches =
        load_json(&state_dir().join("watches.json")).unwrap_or_default();
    persisted
        .into_iter()
        .map(|(k, v)| (k, WatchEntry::from_persisted(v)))
        .collect()
}

/// Collect the persisted representation of all watches (acquires the lock).
pub async fn collect_persisted(watches: &Watches) -> PersistedWatches {
    let w = watches.lock().await;
    w.iter()
        .map(|(k, v)| (k.clone(), v.to_persisted()))
        .collect()
}

// -- Watch entry --

const MAX_GH_FAILURES: u8 = 5;

/// Runtime state per repo/branch: high-water mark + in-progress runs.
#[derive(Debug, Clone, Default)]
pub struct WatchEntry {
    last_seen_run_id: u64,
    pub active_runs: HashMap<u64, ActiveRun>,
    failure_counts: HashMap<u64, u8>,
    pub last_build: Option<LastBuild>,
    /// When the last build completed (not persisted — `None` after daemon restart).
    pub completed_at: Option<Instant>,
}

impl WatchEntry {
    pub(crate) fn from_persisted(p: PersistedWatch) -> Self {
        Self {
            last_seen_run_id: p.last_seen_run_id,
            active_runs: HashMap::new(),
            failure_counts: HashMap::new(),
            last_build: p.last_build,
            completed_at: None,
        }
    }

    pub(crate) fn to_persisted(&self) -> PersistedWatch {
        PersistedWatch {
            last_seen_run_id: self.last_seen_run_id,
            last_build: self.last_build.clone(),
        }
    }

    fn has_active_runs(&self) -> bool {
        !self.active_runs.is_empty()
    }

    fn record_completion(
        &mut self,
        run: &RunInfo,
        failing_steps: Option<String>,
    ) -> Option<Duration> {
        let elapsed = self
            .active_runs
            .remove(&run.id)
            .map(|a| a.started_at.elapsed());
        self.failure_counts.remove(&run.id);
        let mut last_build = run.to_last_build();
        last_build.failing_steps = failing_steps;
        self.last_build = Some(last_build);
        self.completed_at = Some(Instant::now());
        elapsed
    }

    fn clear_failure_count(&mut self, run_id: u64) {
        self.failure_counts.remove(&run_id);
    }

    /// Record a poll failure. Returns `true` if the run was removed after too many failures.
    fn record_failure(&mut self, run_id: u64, error: &GhError) -> bool {
        let count = self.failure_counts.entry(run_id).or_insert(0);
        *count += 1;
        if *count >= MAX_GH_FAILURES {
            tracing::warn!(run_id, count, "Removing run after consecutive failures");
            self.active_runs.remove(&run_id);
            self.failure_counts.remove(&run_id);
            true
        } else {
            tracing::error!(run_id, count, error = %error, "Poll failure");
            false
        }
    }

    /// Update a run's status. Returns the old status if it changed.
    fn update_status(&mut self, run_id: u64, new_status: &str) -> Option<String> {
        if let Some(active) = self.active_runs.get_mut(&run_id)
            && active.status != new_status
        {
            let old = std::mem::replace(&mut active.status, new_status.to_string());
            tracing::debug!(run_id, old = %old, new = %new_status, "Run status changed");
            Some(old)
        } else {
            None
        }
    }

    /// Incorporate newly discovered runs. Iterate oldest-first so the newest completed
    /// run ends up as `last_build`.
    fn incorporate_new_runs(&mut self, new_runs: &[&RunInfo]) {
        if let Some(max_id) = new_runs.iter().map(|r| r.id).max() {
            self.last_seen_run_id = max_id;
        }
        for run in new_runs.iter().rev() {
            if run.is_completed() {
                self.last_build = Some(run.to_last_build());
                self.completed_at = Some(Instant::now());
            } else {
                self.active_runs
                    .insert(run.id, ActiveRun::from_run(run, Instant::now()));
            }
        }
    }
}

// -- Watcher handle --

/// Shared handle for managing poller lifecycle.
#[derive(Clone)]
pub struct WatcherHandle {
    pub tracker: TaskTracker,
    pub cancel: CancellationToken,
    pub events: EventBus,
    pub github: Arc<dyn GitHubClient>,
    pub persistence: Arc<dyn Persistence>,
}

impl WatcherHandle {
    pub fn new(
        cancel: CancellationToken,
        events: EventBus,
        github: Arc<dyn GitHubClient>,
        persistence: Arc<dyn Persistence>,
    ) -> Self {
        Self {
            tracker: TaskTracker::new(),
            cancel,
            events,
            github,
            persistence,
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

    let entry = WatchEntry {
        last_seen_run_id: max_id,
        active_runs: active,
        failure_counts: HashMap::new(),
        last_build: last_completed.map(|r| (*r).to_last_build()),
        completed_at: last_completed.map(|_| Instant::now()),
    };

    {
        let mut w = watches.lock().await;
        // Re-check: a concurrent call may have inserted while we queried GitHub.
        if w.contains_key(&key) {
            return Ok(format!("{repo} [{branch}]: already being watched"));
        }
        w.insert(key.clone(), entry);
    }
    // Persistence is the caller's responsibility — start_watch only updates
    // in-memory state and spawns the poller.

    spawn_poller(watches, config, handle, rate_limit, key);
    Ok(msg)
}

fn spawn_poller(
    watches: &Watches,
    config: &SharedConfig,
    handle: &WatcherHandle,
    rate_limit: &RateLimitState,
    key: WatchKey,
) {
    let poller = Poller {
        key,
        watches: watches.clone(),
        config: config.clone(),
        rate_limit: rate_limit.clone(),
        token: handle.cancel.child_token(),
        events: handle.events.clone(),
        github: handle.github.clone(),
        persistence: handle.persistence.clone(),
    };
    handle.tracker.spawn(poller.run());
}

// -- Workflow filtering --

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

// -- Poller --

/// Per-repo/branch async polling task.
struct Poller {
    key: WatchKey,
    watches: Watches,
    config: SharedConfig,
    rate_limit: RateLimitState,
    token: CancellationToken,
    events: EventBus,
    github: Arc<dyn GitHubClient>,
    persistence: Arc<dyn Persistence>,
}

/// Snapshot of config values needed for a poll cycle.
struct PollConfig {
    active_secs: u64,
    idle_secs: u64,
    workflows: Vec<String>,
    ignored: Vec<String>,
}

impl Poller {
    /// Returns `true` if this watch is still active. Logs and returns `false` if removed.
    async fn is_active(&self) -> bool {
        let w = self.watches.lock().await;
        if w.contains_key(&self.key) {
            true
        } else {
            tracing::info!(key = %self.key, "Watch cancelled");
            false
        }
    }

    /// Sleep for `duration`, returning `false` if cancelled during sleep.
    async fn cancellable_sleep(&self, duration: Duration) -> bool {
        tokio::select! {
            () = tokio::time::sleep(duration) => true,
            () = self.token.cancelled() => {
                tracing::info!(key = %self.key, "Shutting down poller");
                false
            }
        }
    }

    /// Main poller loop. Two polling modes:
    /// - Active runs exist: poll their status every `active_secs` (fast, ~10s)
    /// - No active runs: check for new runs every `idle_secs` (slow, ~60s)
    /// New-run checks always happen at least every `idle_secs`, even during active polling.
    #[tracing::instrument(skip_all, fields(key = %self.key))]
    async fn run(self) {
        let repo = self.key.repo.clone();
        let branch = self.key.branch.clone();
        let mut last_new_run_check: Option<Instant> = None;
        let mut last_rate_limit_refresh: Option<Instant> = None;

        loop {
            // Refresh rate limit state every minute. The `gh api rate_limit`
            // call is free and doesn't count against the budget.
            if last_rate_limit_refresh.is_none_or(|t| t.elapsed() >= RATE_LIMIT_REFRESH_INTERVAL) {
                match self.github.rate_limit().await {
                    Ok(rl) => {
                        tracing::debug!(
                            remaining = rl.remaining,
                            limit = rl.limit,
                            "Rate limit refreshed"
                        );
                        *self.rate_limit.lock().await = Some(rl);
                    }
                    Err(e) => {
                        tracing::warn!(key = %self.key, error = %e, "Failed to fetch rate limit");
                    }
                }
                last_rate_limit_refresh = Some(Instant::now());
            }

            let Some(has_active) = self.read_active_state().await else {
                return;
            };

            let pcfg = self.read_config(&repo).await;
            let delay = if has_active {
                pcfg.active_secs
            } else {
                pcfg.idle_secs
            };

            if !self.cancellable_sleep(Duration::from_secs(delay)).await {
                return;
            }
            if !self.is_active().await {
                return;
            }

            if has_active {
                self.poll_active_runs(&repo, &branch).await;
            }

            let due = last_new_run_check
                .is_none_or(|t| t.elapsed() >= Duration::from_secs(pcfg.idle_secs));
            if due {
                self.check_for_new_runs(&repo, &branch, &pcfg).await;
                last_new_run_check = Some(Instant::now());
            }
        }
    }

    async fn read_active_state(&self) -> Option<bool> {
        let w = self.watches.lock().await;
        if let Some(entry) = w.get(&self.key) {
            Some(entry.has_active_runs())
        } else {
            tracing::info!(key = %self.key, "Watch cancelled");
            None
        }
    }

    async fn read_config(&self, repo: &str) -> PollConfig {
        let rate_limit = self.rate_limit.lock().await.clone();
        let api_calls = {
            let w = self.watches.lock().await;
            count_api_calls(&w)
        };
        let (active_secs, idle_secs) =
            compute_intervals(rate_limit.as_ref(), api_calls, crate::config::unix_now());
        let cfg = self.config.lock().await;
        PollConfig {
            active_secs,
            idle_secs,
            workflows: cfg.workflows_for(repo).to_vec(),
            ignored: cfg.ignored_workflows.clone(),
        }
    }

    /// Poll all in-progress runs, emit events on completion/status change, handle failures.
    /// The watch lock is released during each GitHub API call (high latency)
    /// and re-acquired for each state update to avoid holding it across awaits.
    async fn poll_active_runs(&self, repo: &str, branch: &str) {
        let run_ids: Vec<u64> = {
            let w = self.watches.lock().await;
            match w.get(&self.key) {
                Some(entry) => entry.active_runs.keys().copied().collect(),
                None => return,
            }
        };

        let mut changed = false;

        for run_id in run_ids {
            if self.token.is_cancelled() {
                return;
            }

            let run = match self.github.run_status(repo, run_id).await {
                Ok(run) => {
                    let mut w = self.watches.lock().await;
                    if let Some(entry) = w.get_mut(&self.key) {
                        entry.clear_failure_count(run_id);
                    }
                    run
                }
                Err(e) => {
                    let mut w = self.watches.lock().await;
                    if let Some(entry) = w.get_mut(&self.key) {
                        changed |= entry.record_failure(run_id, &e);
                    }
                    continue;
                }
            };

            if run.is_completed() {
                let elapsed = {
                    let w = self.watches.lock().await;
                    w.get(&self.key)
                        .and_then(|e| e.active_runs.get(&run_id))
                        .map(|a| a.started_at.elapsed().as_secs_f64())
                };

                let failing_steps = if run.succeeded() {
                    None
                } else {
                    self.github.failing_steps(repo, run.id).await
                };

                if self.is_active().await {
                    self.events.emit(WatchEvent::RunCompleted {
                        run: RunSnapshot::from_run_info(&run, repo, branch),
                        conclusion: run.conclusion.clone(),
                        elapsed,
                        failing_steps: failing_steps.clone(),
                    });
                }

                tracing::info!(
                    key = %self.key, run_id,
                    sha = run.short_sha(), conclusion = %run.conclusion,
                    "Build completed"
                );

                let mut w = self.watches.lock().await;
                if let Some(entry) = w.get_mut(&self.key) {
                    entry.record_completion(&run, failing_steps);
                }
                changed = true;
            } else {
                let mut w = self.watches.lock().await;
                if let Some(entry) = w.get_mut(&self.key)
                    && let Some(old_status) = entry.update_status(run_id, &run.status)
                {
                    self.events.emit(WatchEvent::StatusChanged {
                        run: RunSnapshot::from_run_info(&run, repo, branch),
                        from: old_status,
                        to: run.status.clone(),
                    });
                }
            }
        }

        if changed {
            let persisted = collect_persisted(&self.watches).await;
            self.persistence.save_watches(&persisted).await;
        }
    }

    /// Check for runs newer than our high-water mark. Emit events for new and completed runs.
    async fn check_for_new_runs(&self, repo: &str, branch: &str, pcfg: &PollConfig) {
        let last_seen = {
            let w = self.watches.lock().await;
            match w.get(&self.key) {
                Some(entry) => entry.last_seen_run_id,
                None => return,
            }
        };

        let runs = match self.github.recent_runs(repo, branch).await {
            Ok(r) => r,
            Err(e) => {
                tracing::error!(key = %self.key, error = %e, "Failed to check for new runs");
                return;
            }
        };

        // unseen = all runs newer than our high-water mark (owned)
        // new_runs = unseen runs that pass workflow filters (borrowed from unseen)
        // We track both because the high-water mark must advance past filtered-out runs too.
        let unseen: Vec<RunInfo> = runs.into_iter().filter(|r| r.id > last_seen).collect();
        let new_runs = filter_runs(&unseen, &pcfg.workflows, &pcfg.ignored);
        if new_runs.is_empty() && unseen.is_empty() {
            return;
        }

        if !self.is_active().await {
            return;
        }

        // Collect failing_steps for any already-completed runs so we can
        // backfill last_build after incorporate_new_runs sets it.
        let mut failing_steps_by_id: HashMap<u64, Option<String>> = HashMap::new();

        for run in &new_runs {
            tracing::info!(
                key = %self.key, run_id = run.id,
                sha = run.short_sha(), workflow = %run.workflow, title = %run.title,
                "New build detected"
            );
            let snapshot = RunSnapshot::from_run_info(run, repo, branch);
            self.events.emit(WatchEvent::RunStarted(snapshot.clone()));

            if run.is_completed() {
                let failing_steps = if run.succeeded() {
                    None
                } else {
                    self.github.failing_steps(repo, run.id).await
                };
                self.events.emit(WatchEvent::RunCompleted {
                    run: snapshot,
                    conclusion: run.conclusion.clone(),
                    elapsed: None,
                    failing_steps: failing_steps.clone(),
                });
                failing_steps_by_id.insert(run.id, failing_steps);
                tracing::info!(
                    key = %self.key, run_id = run.id,
                    sha = run.short_sha(), conclusion = %run.conclusion,
                    "Build already completed"
                );
            }
        }

        {
            let mut w = self.watches.lock().await;
            if let Some(entry) = w.get_mut(&self.key) {
                // Incorporate only filtered runs for active tracking, but bump
                // high-water mark from all unseen runs to avoid re-checking.
                entry.incorporate_new_runs(&new_runs);
                // Backfill failing_steps for whichever run became last_build.
                if let Some(ref mut lb) = entry.last_build
                    && let Some(steps) = failing_steps_by_id.get(&lb.run_id)
                {
                    lb.failing_steps = steps.clone();
                }
                if let Some(max_id) = unseen.iter().map(|r| r.id).max() {
                    entry.last_seen_run_id = entry.last_seen_run_id.max(max_id);
                }
            }
        }
        let persisted = collect_persisted(&self.watches).await;
        self.persistence.save_watches(&persisted).await;
    }
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
async fn recover_existing_watches(
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

        spawn_poller(watches, config, handle, rate_limit, key);
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

/// Find the most recent failed build across all branches of a repo.
pub fn last_failed_build<'a>(
    watches: &'a HashMap<WatchKey, WatchEntry>,
    repo: &str,
) -> Option<(&'a WatchKey, &'a LastBuild)> {
    watches
        .iter()
        .filter(|(k, _)| k.matches_repo(repo))
        .filter_map(|(k, entry)| {
            entry
                .last_build
                .as_ref()
                .filter(|b| b.conclusion != "success")
                .map(|b| (k, b))
        })
        .max_by_key(|(_, b)| b.run_id)
}

// -- Tests --

#[cfg(test)]
mod tests {
    use super::*;

    fn make_run(id: u64, status: &str, conclusion: &str) -> RunInfo {
        RunInfo {
            id,
            status: status.to_string(),
            conclusion: conclusion.to_string(),
            title: "Test PR".to_string(),
            workflow: "CI".to_string(),
            head_sha: "abc1234".to_string(),
            event: "push".to_string(),
        }
    }

    fn make_active(status: &str) -> ActiveRun {
        ActiveRun {
            status: status.to_string(),
            started_at: Instant::now(),
            workflow: "CI".to_string(),
            title: "Test PR".to_string(),
            event: "push".to_string(),
        }
    }

    fn make_entry() -> WatchEntry {
        WatchEntry {
            last_seen_run_id: 100,
            active_runs: HashMap::from([
                (101, make_active("in_progress")),
                (102, make_active("queued")),
            ]),
            failure_counts: HashMap::new(),
            last_build: None,
            completed_at: None,
        }
    }

    // -- WatchKey tests --

    #[test]
    fn watch_key_roundtrip_and_matching() {
        let k = WatchKey::new("alice/myapp", "main");
        assert_eq!(k.to_string(), "alice/myapp#main");
        assert!(k.matches_repo("alice/myapp"));
        assert!(!k.matches_repo("bob/other"));

        let parsed = WatchKey::parse("alice/myapp#main");
        assert_eq!(parsed.repo, "alice/myapp");
        assert_eq!(parsed.branch, "main");

        // Falls back to "main" when no branch in persisted key
        let legacy = WatchKey::parse("alice/myapp");
        assert_eq!(legacy.branch, "main");
    }

    #[test]
    fn watch_key_serde_roundtrip() {
        let key = WatchKey::new("alice/app", "feature/xyz");
        let json = serde_json::to_string(&key).unwrap();
        assert_eq!(json, "\"alice/app#feature/xyz\"");
        let parsed: WatchKey = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, key);
    }

    #[test]
    fn active_run_display_title() {
        let push = make_active("in_progress");
        assert_eq!(push.display_title(), "Test PR");

        let mut pr = make_active("queued");
        pr.event = "pull_request".to_string();
        assert_eq!(pr.display_title(), "PR: Test PR");
    }

    // -- WatchEntry state machine tests --

    #[test]
    fn record_completion_returns_elapsed() {
        let mut entry = make_entry();
        let run = make_run(101, "completed", "success");

        let elapsed = entry.record_completion(&run, None);

        // Active run was present, so elapsed should be Some
        assert!(elapsed.is_some());
        // Should be very small since we just created it
        assert!(elapsed.unwrap() < std::time::Duration::from_secs(1));
    }

    #[test]
    fn record_completion_returns_none_for_unknown_run() {
        let mut entry = make_entry();
        let run = make_run(999, "completed", "success");

        let elapsed = entry.record_completion(&run, None);

        assert!(elapsed.is_none());
    }

    #[test]
    fn record_completion_removes_run_and_sets_last_build() {
        let mut entry = make_entry();
        let run = make_run(101, "completed", "success");

        entry.record_completion(&run, None);

        assert!(!entry.active_runs.contains_key(&101));
        assert!(entry.active_runs.contains_key(&102));
        let lb = entry.last_build.unwrap();
        assert_eq!(lb.run_id, 101);
        assert_eq!(lb.conclusion, "success");
    }

    #[test]
    fn record_completion_stores_failing_steps() {
        let mut entry = make_entry();
        let run = make_run(101, "completed", "failure");

        entry.record_completion(&run, Some("Build / Run tests".to_string()));

        let lb = entry.last_build.unwrap();
        assert_eq!(lb.failing_steps.as_deref(), Some("Build / Run tests"));
    }

    #[test]
    fn record_completion_clears_failure_count() {
        let mut entry = make_entry();
        entry.failure_counts.insert(101, 3);
        let run = make_run(101, "completed", "failure");

        entry.record_completion(&run, None);

        assert!(!entry.failure_counts.contains_key(&101));
    }

    #[test]
    fn record_failure_increments_count() {
        let mut entry = make_entry();
        let error = GhError::Timeout {
            repo: "test".to_string(),
            timeout_secs: 30,
        };

        let removed = entry.record_failure(101, &error);

        assert!(!removed);
        assert_eq!(entry.failure_counts[&101], 1);
        assert!(entry.active_runs.contains_key(&101));
    }

    #[test]
    fn record_failure_removes_run_at_max_failures() {
        let mut entry = make_entry();
        entry.failure_counts.insert(101, MAX_GH_FAILURES - 1);
        let error = GhError::Timeout {
            repo: "test".to_string(),
            timeout_secs: 30,
        };

        let removed = entry.record_failure(101, &error);

        assert!(removed);
        assert!(!entry.active_runs.contains_key(&101));
        assert!(!entry.failure_counts.contains_key(&101));
    }

    #[test]
    fn clear_failure_count_resets_on_success() {
        let mut entry = make_entry();
        entry.failure_counts.insert(101, 3);

        entry.clear_failure_count(101);

        assert!(!entry.failure_counts.contains_key(&101));
    }

    #[test]
    fn update_status_changes_when_different() {
        let mut entry = make_entry();

        let old = entry.update_status(101, "queued");

        assert_eq!(old, Some("in_progress".to_string()));
        assert_eq!(entry.active_runs[&101].status, "queued");
    }

    #[test]
    fn update_status_noop_when_same() {
        let mut entry = make_entry();

        let old = entry.update_status(101, "in_progress");

        assert!(old.is_none());
        assert_eq!(entry.active_runs[&101].status, "in_progress");
    }

    #[test]
    fn update_status_noop_for_unknown_run() {
        let mut entry = make_entry();

        entry.update_status(999, "completed");

        assert!(!entry.active_runs.contains_key(&999));
    }

    #[test]
    fn incorporate_new_runs_tracks_in_progress() {
        let mut entry = make_entry();
        let run = make_run(200, "in_progress", "");
        let new_runs: Vec<&RunInfo> = vec![&run];

        entry.incorporate_new_runs(&new_runs);

        assert_eq!(entry.last_seen_run_id, 200);
        assert_eq!(entry.active_runs[&200].status, "in_progress");
        assert!(entry.last_build.is_none());
    }

    #[test]
    fn incorporate_new_runs_records_completed() {
        let mut entry = make_entry();
        let run = make_run(200, "completed", "success");
        let new_runs: Vec<&RunInfo> = vec![&run];

        entry.incorporate_new_runs(&new_runs);

        assert_eq!(entry.last_seen_run_id, 200);
        assert!(!entry.active_runs.contains_key(&200));
        assert_eq!(entry.last_build.unwrap().run_id, 200);
    }

    #[test]
    fn incorporate_new_runs_newest_completed_wins_last_build() {
        let mut entry = make_entry();
        let old = make_run(200, "completed", "failure");
        let new = make_run(201, "completed", "success");
        let new_runs: Vec<&RunInfo> = vec![&new, &old];

        entry.incorporate_new_runs(&new_runs);

        assert_eq!(entry.last_seen_run_id, 201);
        let lb = entry.last_build.unwrap();
        assert_eq!(lb.run_id, 201);
        assert_eq!(lb.conclusion, "success");
    }

    #[test]
    fn incorporate_new_runs_mixed_statuses() {
        let mut entry = make_entry();
        let completed = make_run(200, "completed", "success");
        let active = make_run(201, "in_progress", "");
        let new_runs: Vec<&RunInfo> = vec![&active, &completed];

        entry.incorporate_new_runs(&new_runs);

        assert_eq!(entry.last_seen_run_id, 201);
        assert_eq!(entry.active_runs[&201].status, "in_progress");
        assert!(!entry.active_runs.contains_key(&200));
        assert_eq!(entry.last_build.unwrap().run_id, 200);
    }

    #[test]
    fn has_active_runs_reflects_state() {
        let mut entry = make_entry();
        assert!(entry.has_active_runs());

        entry.active_runs.clear();
        assert!(!entry.has_active_runs());
    }

    #[test]
    fn persisted_roundtrip_preserves_fields() {
        let mut entry = make_entry();
        let run = make_run(101, "completed", "success");
        entry.record_completion(&run, None);

        let persisted = entry.to_persisted();
        let restored = WatchEntry::from_persisted(persisted);

        assert_eq!(restored.last_seen_run_id, entry.last_seen_run_id);
        assert!(restored.active_runs.is_empty());
        assert!(restored.failure_counts.is_empty());
        assert_eq!(restored.last_build.unwrap().run_id, 101);
    }

    // -- filter_runs tests --

    #[test]
    fn filter_runs_no_filters() {
        let runs = vec![
            make_run(1, "completed", "success"),
            make_run(2, "in_progress", ""),
        ];
        assert_eq!(filter_runs(&runs, &[], &[]).len(), 2);
    }

    #[test]
    fn filter_runs_workflow_allowlist() {
        let mut r1 = make_run(1, "completed", "success");
        r1.workflow = "CI".to_string();
        let mut r2 = make_run(2, "completed", "success");
        r2.workflow = "Deploy".to_string();
        let runs = vec![r1, r2];

        let filtered = filter_runs(&runs, &["ci".to_string()], &[]);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].workflow, "CI");
    }

    #[test]
    fn filter_runs_ignored_workflows() {
        let mut r1 = make_run(1, "completed", "success");
        r1.workflow = "CI".to_string();
        let mut r2 = make_run(2, "completed", "success");
        r2.workflow = "Semgrep".to_string();
        let runs = vec![r1, r2];

        let filtered = filter_runs(&runs, &[], &["semgrep".to_string()]);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].workflow, "CI");
    }

    #[test]
    fn filter_runs_both_filters() {
        let mut r1 = make_run(1, "completed", "success");
        r1.workflow = "CI".to_string();
        let mut r2 = make_run(2, "completed", "success");
        r2.workflow = "Deploy".to_string();
        let mut r3 = make_run(3, "completed", "success");
        r3.workflow = "Semgrep".to_string();
        let runs = vec![r1, r2, r3];

        // Allow CI and Deploy, ignore Semgrep
        let filtered = filter_runs(
            &runs,
            &["CI".to_string(), "Deploy".to_string()],
            &["Semgrep".to_string()],
        );
        assert_eq!(filtered.len(), 2);
    }

    // -- last_failed_build tests --

    #[test]
    fn last_failed_build_finds_failure() {
        let mut watches = HashMap::new();
        let mut entry = make_entry();
        let run = make_run(200, "completed", "failure");
        entry.record_completion(&run, None);
        watches.insert(WatchKey::new("alice/app", "main"), entry);

        let result = last_failed_build(&watches, "alice/app");
        assert!(result.is_some());
        let (key, build) = result.unwrap();
        assert_eq!(key.repo, "alice/app");
        assert_eq!(key.branch, "main");
        assert_eq!(build.run_id, 200);
    }

    #[test]
    fn last_failed_build_ignores_success() {
        let mut watches = HashMap::new();
        let mut entry = make_entry();
        let run = make_run(200, "completed", "success");
        entry.record_completion(&run, None);
        watches.insert(WatchKey::new("alice/app", "main"), entry);

        assert!(last_failed_build(&watches, "alice/app").is_none());
    }

    #[test]
    fn last_failed_build_picks_most_recent() {
        let mut watches = HashMap::new();

        let mut entry1 = make_entry();
        let run1 = make_run(100, "completed", "failure");
        entry1.record_completion(&run1, None);
        watches.insert(WatchKey::new("alice/app", "main"), entry1);

        let mut entry2 = make_entry();
        let run2 = make_run(200, "completed", "failure");
        entry2.record_completion(&run2, None);
        watches.insert(WatchKey::new("alice/app", "develop"), entry2);

        let (_, build) = last_failed_build(&watches, "alice/app").unwrap();
        assert_eq!(build.run_id, 200);
    }

    #[test]
    fn last_failed_build_ignores_other_repos() {
        let mut watches = HashMap::new();
        let mut entry = make_entry();
        let run = make_run(200, "completed", "failure");
        entry.record_completion(&run, None);
        watches.insert(WatchKey::new("bob/other", "main"), entry);

        assert!(last_failed_build(&watches, "alice/app").is_none());
    }

    // -- compute_intervals --

    const T: u64 = 1_000_000; // fixed "now" for deterministic tests

    fn make_rate_limit(remaining: u64, limit: u64, secs_until_reset: u64) -> RateLimit {
        RateLimit {
            limit,
            remaining,
            reset: T + secs_until_reset,
            used: limit.saturating_sub(remaining),
        }
    }

    #[test]
    fn compute_intervals_no_data_returns_fallback() {
        let (active, idle) = compute_intervals(None, 1, T);
        assert_eq!(active, FALLBACK_ACTIVE_SECS);
        assert_eq!(idle, FALLBACK_IDLE_SECS);
    }

    #[test]
    fn compute_intervals_above_threshold_scales_with_call_count() {
        let rl = make_rate_limit(3000, 5000, 3600); // 60% remaining — above 50%

        // 1 api call: sqrt(1) = 1 → bare floor
        let (active, idle) = compute_intervals(Some(&rl), 1, T);
        assert_eq!(active, MIN_ACTIVE_SECS);
        assert_eq!(idle, MIN_IDLE_SECS);

        // 3 api calls: sqrt(3) = 1 → ×1
        let (active, idle) = compute_intervals(Some(&rl), 3, T);
        assert_eq!(active, MIN_ACTIVE_SECS);
        assert_eq!(idle, MIN_IDLE_SECS);

        // 6 api calls: sqrt(6) = 2 → ×2
        let (active, idle) = compute_intervals(Some(&rl), 6, T);
        assert_eq!(active, MIN_ACTIVE_SECS * 2);
        assert_eq!(idle, MIN_IDLE_SECS * 2);

        // 10 api calls: sqrt(10) = 3 → ×3
        let (active, idle) = compute_intervals(Some(&rl), 10, T);
        assert_eq!(active, MIN_ACTIVE_SECS * 3);
        assert_eq!(idle, MIN_IDLE_SECS * 3);
    }

    #[test]
    fn compute_intervals_below_threshold_throttles() {
        // At 50%: floors dominate for 1 call
        let rl50 = make_rate_limit(2500, 5000, 3600);
        assert_eq!(compute_intervals(Some(&rl50), 1, T), (15, 60));

        // At 10%: still floors for 1 call (rate_limited = 3600/500 = 7s < floors)
        let rl10 = make_rate_limit(500, 5000, 3600);
        assert_eq!(compute_intervals(Some(&rl10), 1, T), (15, 60));
    }

    #[test]
    fn compute_intervals_scales_with_call_count() {
        // 500/5000 remaining, 10 api calls (e.g. 5 watches + 5 active runs), 3600s
        // rate_limited = (10 * 3600) / 500 = 72s
        let rl = make_rate_limit(500, 5000, 3600);
        let (active, idle) = compute_intervals(Some(&rl), 10, T);
        assert_eq!(active, 72);
        assert_eq!(idle, 72);
    }

    #[test]
    fn compute_intervals_zero_remaining_waits_for_reset() {
        let rl = make_rate_limit(0, 5000, 3600);
        let (active, idle) = compute_intervals(Some(&rl), 1, T);
        assert_eq!(active, 3600);
        assert_eq!(idle, 3600);
    }

    #[test]
    fn compute_intervals_zero_calls_treated_as_one() {
        let rl = make_rate_limit(500, 5000, 3600);
        let (a0, i0) = compute_intervals(Some(&rl), 0, T);
        let (a1, i1) = compute_intervals(Some(&rl), 1, T);
        assert_eq!(a0, a1);
        assert_eq!(i0, i1);
    }

    #[test]
    fn count_api_calls_reflects_active_runs() {
        let mut watches = HashMap::new();
        // 2 watches, one with 3 active runs, one idle
        let mut active_runs = HashMap::new();
        for id in 1..=3 {
            active_runs.insert(id, make_active("in_progress"));
        }
        let entry1 = WatchEntry {
            last_seen_run_id: 100,
            active_runs,
            failure_counts: HashMap::new(),
            last_build: None,
            completed_at: None,
        };
        let entry2 = WatchEntry {
            last_seen_run_id: 100,
            active_runs: HashMap::new(),
            failure_counts: HashMap::new(),
            last_build: None,
            completed_at: None,
        };
        watches.insert(WatchKey::new("owner/repo1", "main"), entry1);
        watches.insert(WatchKey::new("owner/repo2", "main"), entry2);

        // 2 base calls (one per watch) + 3 active run calls = 5
        assert_eq!(count_api_calls(&watches), 5);
    }

    #[test]
    fn count_api_calls_empty_watches() {
        let watches = HashMap::new();
        assert_eq!(count_api_calls(&watches), 0);
    }

    // -- Mock GitHub client for integration tests --

    // -- Mock GitHub client --

    struct MockGitHub {
        runs: Vec<RunInfo>,
        failure_msg: Option<String>,
    }

    impl MockGitHub {
        fn with_runs(runs: Vec<RunInfo>) -> Arc<dyn crate::github::GitHubClient> {
            Arc::new(Self {
                runs,
                failure_msg: None,
            })
        }

        fn with_runs_and_failures(
            runs: Vec<RunInfo>,
            failure_msg: &str,
        ) -> Arc<dyn crate::github::GitHubClient> {
            Arc::new(Self {
                runs,
                failure_msg: Some(failure_msg.to_string()),
            })
        }
    }

    #[async_trait::async_trait]
    impl crate::github::GitHubClient for MockGitHub {
        async fn recent_runs(&self, _: &str, _: &str) -> Result<Vec<RunInfo>, GhError> {
            Ok(self.runs.clone())
        }
        async fn run_status(&self, _: &str, run_id: u64) -> Result<RunInfo, GhError> {
            self.runs
                .iter()
                .find(|r| r.id == run_id)
                .cloned()
                .ok_or(GhError::MissingFields {
                    repo: "mock".into(),
                })
        }
        async fn failing_steps(&self, _: &str, _: u64) -> Option<String> {
            self.failure_msg.clone()
        }
        async fn run_rerun(&self, _: &str, _: u64, _: bool) -> Result<String, GhError> {
            Ok(String::new())
        }
        async fn run_list_history(
            &self,
            _: &str,
            _: Option<&str>,
            _: u32,
        ) -> Result<Vec<crate::github::HistoryEntry>, GhError> {
            Ok(vec![])
        }
        async fn rate_limit(&self) -> Result<RateLimit, GhError> {
            Ok(RateLimit {
                limit: 5000,
                remaining: 5000,
                reset: crate::config::unix_now() + 3600,
                used: 0,
            })
        }
    }

    fn mock_handle(github: Arc<dyn crate::github::GitHubClient>) -> WatcherHandle {
        WatcherHandle::new(
            CancellationToken::new(),
            crate::events::EventBus::new(),
            github,
            Arc::new(crate::persistence::NullPersistence),
        )
    }

    fn make_poller(
        key: &WatchKey,
        watches: &Watches,
        config: &SharedConfig,
        rate_limit: &RateLimitState,
        handle: &WatcherHandle,
    ) -> Poller {
        Poller {
            key: key.clone(),
            watches: watches.clone(),
            config: config.clone(),
            rate_limit: rate_limit.clone(),
            token: handle.cancel.child_token(),
            events: handle.events.clone(),
            github: handle.github.clone(),
            persistence: handle.persistence.clone(),
        }
    }

    #[tokio::test]
    async fn start_watch_with_mock_github() {
        let runs = vec![
            make_run(100, "completed", "success"),
            make_run(101, "in_progress", ""),
        ];
        let gh = MockGitHub::with_runs(runs);
        let watches: Watches = Arc::new(Mutex::new(HashMap::new()));
        let config: SharedConfig = Arc::new(Mutex::new(Config::default()));
        let rate_limit: RateLimitState = Arc::new(Mutex::new(None));
        let handle = mock_handle(gh);

        let result =
            start_watch(&watches, &config, &handle, &rate_limit, "alice/app", "main").await;
        assert!(result.is_ok());

        let w = watches.lock().await;
        let key = WatchKey::new("alice/app", "main");
        assert!(w.contains_key(&key));
        let entry = &w[&key];
        assert_eq!(entry.last_seen_run_id, 101);
        assert!(entry.active_runs.contains_key(&101));
        assert!(!entry.active_runs.contains_key(&100)); // completed, not tracked

        handle.cancel.cancel();
    }

    #[tokio::test]
    async fn start_watch_rejects_empty_runs() {
        let gh = MockGitHub::with_runs(vec![]);
        let watches: Watches = Arc::new(Mutex::new(HashMap::new()));
        let config: SharedConfig = Arc::new(Mutex::new(Config::default()));
        let rate_limit: RateLimitState = Arc::new(Mutex::new(None));
        let handle = mock_handle(gh);

        let result =
            start_watch(&watches, &config, &handle, &rate_limit, "alice/app", "main").await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("no workflow runs found"));
    }

    #[tokio::test]
    async fn start_watch_deduplicates() {
        let runs = vec![make_run(100, "completed", "success")];
        let gh = MockGitHub::with_runs(runs);
        let watches: Watches = Arc::new(Mutex::new(HashMap::new()));
        let config: SharedConfig = Arc::new(Mutex::new(Config::default()));
        let rate_limit: RateLimitState = Arc::new(Mutex::new(None));
        let handle = mock_handle(gh);

        let r1 = start_watch(&watches, &config, &handle, &rate_limit, "alice/app", "main").await;
        assert!(r1.is_ok());

        let r2 = start_watch(&watches, &config, &handle, &rate_limit, "alice/app", "main").await;
        assert!(r2.unwrap().contains("already being watched"));

        handle.cancel.cancel();
    }

    // -- Poller: check_for_new_runs --

    #[tokio::test]
    async fn check_for_new_runs_detects_new_builds() {
        let key = WatchKey::new("alice/app", "main");
        // Mock returns runs 99-102; watch has seen up to 100
        let runs = vec![
            make_run(99, "completed", "success"),
            make_run(100, "completed", "success"),
            make_run(101, "in_progress", ""),
            make_run(102, "completed", "failure"),
        ];
        let gh = MockGitHub::with_runs_and_failures(runs, "Build / Run tests");
        let watches: Watches = Arc::new(Mutex::new(HashMap::new()));
        let config: SharedConfig = Arc::new(Mutex::new(Config::default()));
        let rate_limit: RateLimitState = Arc::new(Mutex::new(None));
        let handle = mock_handle(gh);

        // Seed a watch entry with last_seen=100
        {
            let mut w = watches.lock().await;
            w.insert(
                key.clone(),
                WatchEntry {
                    last_seen_run_id: 100,
                    active_runs: HashMap::new(),
                    failure_counts: HashMap::new(),
                    last_build: None,
                    completed_at: None,
                },
            );
        }

        let mut rx = handle.events.subscribe();
        let poller = make_poller(&key, &watches, &config, &rate_limit, &handle);
        let pcfg = PollConfig {
            active_secs: 15,
            idle_secs: 60,
            workflows: vec![],
            ignored: vec![],
        };

        poller.check_for_new_runs("alice/app", "main", &pcfg).await;

        // Verify high-water mark advanced
        let w = watches.lock().await;
        let entry = &w[&key];
        assert_eq!(entry.last_seen_run_id, 102);
        // 101 is in_progress → tracked as active
        assert!(entry.active_runs.contains_key(&101));
        // 102 is completed → last_build
        assert_eq!(entry.last_build.as_ref().unwrap().run_id, 102);
        drop(w);

        // Verify events: RunStarted for 101 and 102, RunCompleted for 102
        let mut events = vec![];
        while let Ok(e) = rx.try_recv() {
            events.push(e);
        }
        assert!(
            events.len() >= 3,
            "expected ≥3 events, got {}",
            events.len()
        );

        handle.cancel.cancel();
    }

    #[tokio::test]
    async fn check_for_new_runs_applies_workflow_filter() {
        let key = WatchKey::new("alice/app", "main");
        let mut ci = make_run(101, "in_progress", "");
        ci.workflow = "CI".to_string();
        let mut semgrep = make_run(102, "in_progress", "");
        semgrep.workflow = "Semgrep".to_string();

        let gh = MockGitHub::with_runs(vec![ci, semgrep]);
        let watches: Watches = Arc::new(Mutex::new(HashMap::new()));
        let config: SharedConfig = Arc::new(Mutex::new(Config::default()));
        let rate_limit: RateLimitState = Arc::new(Mutex::new(None));
        let handle = mock_handle(gh);

        {
            let mut w = watches.lock().await;
            w.insert(
                key.clone(),
                WatchEntry {
                    last_seen_run_id: 100,
                    active_runs: HashMap::new(),
                    failure_counts: HashMap::new(),
                    last_build: None,
                    completed_at: None,
                },
            );
        }

        let poller = make_poller(&key, &watches, &config, &rate_limit, &handle);
        let pcfg = PollConfig {
            active_secs: 15,
            idle_secs: 60,
            workflows: vec![],
            ignored: vec!["Semgrep".to_string()],
        };

        poller.check_for_new_runs("alice/app", "main", &pcfg).await;

        let w = watches.lock().await;
        let entry = &w[&key];
        // CI tracked, Semgrep filtered out
        assert!(entry.active_runs.contains_key(&101));
        assert!(!entry.active_runs.contains_key(&102));
        // High-water mark includes filtered runs
        assert_eq!(entry.last_seen_run_id, 102);

        handle.cancel.cancel();
    }

    // -- Poller: poll_active_runs --

    #[tokio::test]
    async fn poll_active_runs_detects_completion() {
        let key = WatchKey::new("alice/app", "main");
        // Mock: run 101 now completed
        let runs = vec![make_run(101, "completed", "success")];
        let gh = MockGitHub::with_runs(runs);
        let watches: Watches = Arc::new(Mutex::new(HashMap::new()));
        let config: SharedConfig = Arc::new(Mutex::new(Config::default()));
        let rate_limit: RateLimitState = Arc::new(Mutex::new(None));
        let handle = mock_handle(gh);

        // Seed watch with run 101 as active
        {
            let mut w = watches.lock().await;
            let mut entry = WatchEntry {
                last_seen_run_id: 101,
                active_runs: HashMap::new(),
                failure_counts: HashMap::new(),
                last_build: None,
                completed_at: None,
            };
            entry.active_runs.insert(101, make_active("in_progress"));
            w.insert(key.clone(), entry);
        }

        let mut rx = handle.events.subscribe();
        let poller = make_poller(&key, &watches, &config, &rate_limit, &handle);

        poller.poll_active_runs("alice/app", "main").await;

        let w = watches.lock().await;
        let entry = &w[&key];
        // Run removed from active, recorded as last_build
        assert!(!entry.active_runs.contains_key(&101));
        assert_eq!(entry.last_build.as_ref().unwrap().run_id, 101);
        assert_eq!(entry.last_build.as_ref().unwrap().conclusion, "success");
        drop(w);

        // RunCompleted event emitted
        match rx.try_recv() {
            Ok(WatchEvent::RunCompleted { conclusion, .. }) => {
                assert_eq!(conclusion, "success");
            }
            other => panic!("expected RunCompleted, got {other:?}"),
        }

        handle.cancel.cancel();
    }

    #[tokio::test]
    async fn poll_active_runs_emits_status_change() {
        let key = WatchKey::new("alice/app", "main");
        // Mock: run 101 changed from queued to in_progress
        let runs = vec![make_run(101, "in_progress", "")];
        let gh = MockGitHub::with_runs(runs);
        let watches: Watches = Arc::new(Mutex::new(HashMap::new()));
        let config: SharedConfig = Arc::new(Mutex::new(Config::default()));
        let rate_limit: RateLimitState = Arc::new(Mutex::new(None));
        let handle = mock_handle(gh);

        {
            let mut w = watches.lock().await;
            let mut entry = WatchEntry {
                last_seen_run_id: 101,
                active_runs: HashMap::new(),
                failure_counts: HashMap::new(),
                last_build: None,
                completed_at: None,
            };
            entry.active_runs.insert(101, make_active("queued"));
            w.insert(key.clone(), entry);
        }

        let mut rx = handle.events.subscribe();
        let poller = make_poller(&key, &watches, &config, &rate_limit, &handle);

        poller.poll_active_runs("alice/app", "main").await;

        // Still active, status updated
        let w = watches.lock().await;
        assert_eq!(w[&key].active_runs[&101].status, "in_progress");
        drop(w);

        // StatusChanged event
        match rx.try_recv() {
            Ok(WatchEvent::StatusChanged { from, to, .. }) => {
                assert_eq!(from, "queued");
                assert_eq!(to, "in_progress");
            }
            other => panic!("expected StatusChanged, got {other:?}"),
        }

        handle.cancel.cancel();
    }

    #[tokio::test]
    async fn poll_active_runs_fetches_failing_steps() {
        let key = WatchKey::new("alice/app", "main");
        let runs = vec![make_run(101, "completed", "failure")];
        let gh = MockGitHub::with_runs_and_failures(runs, "Build / Run tests");
        let watches: Watches = Arc::new(Mutex::new(HashMap::new()));
        let config: SharedConfig = Arc::new(Mutex::new(Config::default()));
        let rate_limit: RateLimitState = Arc::new(Mutex::new(None));
        let handle = mock_handle(gh);

        {
            let mut w = watches.lock().await;
            let mut entry = WatchEntry {
                last_seen_run_id: 101,
                active_runs: HashMap::new(),
                failure_counts: HashMap::new(),
                last_build: None,
                completed_at: None,
            };
            entry.active_runs.insert(101, make_active("in_progress"));
            w.insert(key.clone(), entry);
        }

        let mut rx = handle.events.subscribe();
        let poller = make_poller(&key, &watches, &config, &rate_limit, &handle);

        poller.poll_active_runs("alice/app", "main").await;

        match rx.try_recv() {
            Ok(WatchEvent::RunCompleted {
                failing_steps,
                conclusion,
                ..
            }) => {
                assert_eq!(conclusion, "failure");
                assert_eq!(failing_steps.as_deref(), Some("Build / Run tests"));
            }
            other => panic!("expected RunCompleted, got {other:?}"),
        }

        handle.cancel.cancel();
    }

    // -- Startup recovery --

    #[tokio::test]
    async fn recover_existing_watches_recovers_active_runs() {
        let key = WatchKey::new("alice/app", "main");
        // Mock returns a mix: 100 completed, 101 in_progress
        let runs = vec![
            make_run(100, "completed", "success"),
            make_run(101, "in_progress", ""),
        ];
        let gh = MockGitHub::with_runs(runs);
        let watches: Watches = Arc::new(Mutex::new(HashMap::new()));
        let config: SharedConfig = Arc::new(Mutex::new(Config::default()));
        let rate_limit: RateLimitState = Arc::new(Mutex::new(None));
        let handle = mock_handle(gh);

        // Seed: persisted watch with last_seen=99, no active runs
        {
            let mut w = watches.lock().await;
            w.insert(
                key.clone(),
                WatchEntry {
                    last_seen_run_id: 99,
                    active_runs: HashMap::new(),
                    failure_counts: HashMap::new(),
                    last_build: None,
                    completed_at: None,
                },
            );
        }

        let snapshot = vec![key.clone()];
        recover_existing_watches(&watches, &config, &handle, &rate_limit, &snapshot).await;

        let w = watches.lock().await;
        let entry = &w[&key];
        // In-progress run recovered
        assert!(entry.active_runs.contains_key(&101));
        // High-water mark bumped
        assert_eq!(entry.last_seen_run_id, 101);

        handle.cancel.cancel();
    }
}
