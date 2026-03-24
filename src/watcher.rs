use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::config::{Config, NotificationConfig, load_json, save_json, state_dir};
use crate::github::{GhError, LastBuild, RunInfo, gh_failing_steps, gh_recent_runs, gh_run_status};
use crate::platform;

pub type SharedConfig = Arc<Mutex<Config>>;
pub type Watches = Arc<Mutex<HashMap<String, WatchEntry>>>;
pub type PauseState = Arc<Mutex<Option<Instant>>>;

/// Runtime state for an in-progress run, including when we first saw it.
#[derive(Debug, Clone)]
pub struct ActiveRun {
    pub status: String,
    pub started_at: Instant,
}

// -- Watch key helpers --
//
// Keys use `#` as the delimiter (`owner/repo#branch`). This is safe because
// both `validate_repo` and `validate_branch` reject `#` in their inputs.

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
        .map(|(k, v)| (k, WatchEntry::from_persisted(v)))
        .collect()
}

pub async fn save_watches(watches: &Watches) {
    let persisted: PersistedWatches = {
        let w = watches.lock().await;
        w.iter()
            .map(|(k, v)| (k.clone(), v.to_persisted()))
            .collect()
    };
    if let Err(e) = save_json(state_dir().join("watches.json"), &persisted) {
        tracing::error!("Failed to save watches: {e}");
    }
}

// -- Watch entry --

const MAX_GH_FAILURES: u8 = 5;

/// Runtime state per repo/branch: high-water mark + in-progress runs.
#[derive(Debug, Clone)]
pub struct WatchEntry {
    last_seen_run_id: u64,
    pub active_runs: HashMap<u64, ActiveRun>,
    failure_counts: HashMap<u64, u8>,
    pub last_build: Option<LastBuild>,
}

impl WatchEntry {
    fn from_persisted(p: PersistedWatch) -> Self {
        Self {
            last_seen_run_id: p.last_seen_run_id,
            active_runs: HashMap::new(),
            failure_counts: HashMap::new(),
            last_build: p.last_build,
        }
    }

    fn to_persisted(&self) -> PersistedWatch {
        PersistedWatch {
            last_seen_run_id: self.last_seen_run_id,
            last_build: self.last_build.clone(),
        }
    }

    fn has_active_runs(&self) -> bool {
        !self.active_runs.is_empty()
    }

    fn record_completion(&mut self, run: &RunInfo) -> Option<Duration> {
        let elapsed = self
            .active_runs
            .remove(&run.id)
            .map(|a| a.started_at.elapsed());
        self.failure_counts.remove(&run.id);
        self.last_build = Some(run.to_last_build());
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

    fn update_status(&mut self, run_id: u64, new_status: String) {
        if let Some(active) = self.active_runs.get_mut(&run_id)
            && active.status != new_status
        {
            tracing::debug!(run_id, old = %active.status, new = %new_status, "Run status changed");
            active.status = new_status;
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
            } else {
                self.active_runs.insert(
                    run.id,
                    ActiveRun {
                        status: run.status.clone(),
                        started_at: Instant::now(),
                    },
                );
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
}

impl WatcherHandle {
    pub fn new(cancel: CancellationToken) -> Self {
        Self {
            tracker: TaskTracker::new(),
            cancel,
        }
    }

    pub async fn shutdown(&self) {
        self.tracker.close();
        self.tracker.wait().await;
    }
}

// -- Starting watches --

#[tracing::instrument(skip(watches, config, handle, pause), fields(%repo, %branch))]
pub async fn start_watch(
    watches: &Watches,
    config: &SharedConfig,
    handle: &WatcherHandle,
    pause: &PauseState,
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

    let all_runs = gh_recent_runs(repo, branch)
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
    let runs: Vec<&RunInfo> = all_runs
        .iter()
        .filter(|r| {
            !ignored_workflows
                .iter()
                .any(|i| r.workflow.eq_ignore_ascii_case(i))
        })
        .filter(|r| {
            workflow_filter.is_empty()
                || workflow_filter
                    .iter()
                    .any(|w| r.workflow.eq_ignore_ascii_case(w))
        })
        .collect();
    if runs.is_empty() {
        return Err(format!(
            "{repo} [{branch}]: no runs match workflow filter {workflow_filter:?}"
        ));
    }

    let max_id = runs.iter().map(|r| r.id).max().expect("runs is non-empty");
    let now = Instant::now();
    let active: HashMap<u64, ActiveRun> = runs
        .iter()
        .filter(|r| !r.is_completed())
        .map(|r| {
            (
                r.id,
                ActiveRun {
                    status: r.status.clone(),
                    started_at: now,
                },
            )
        })
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
    };

    {
        let mut w = watches.lock().await;
        // Re-check: a concurrent call may have inserted while we queried GitHub.
        if w.contains_key(key) {
            return Ok(format!("{repo} [{branch}]: already being watched"));
        }
        w.insert(key.to_string(), entry);
    }
    save_watches(watches).await;

    spawn_poller(watches, config, handle, pause, key.to_string());
    Ok(msg)
}

fn spawn_poller(
    watches: &Watches,
    config: &SharedConfig,
    handle: &WatcherHandle,
    pause: &PauseState,
    key: String,
) {
    let poller = Poller {
        key,
        watches: watches.clone(),
        config: config.clone(),
        pause: pause.clone(),
        token: handle.cancel.child_token(),
    };
    handle.tracker.spawn(poller.run());
}

// -- Poller --

/// Per-repo/branch async polling task.
struct Poller {
    key: String,
    watches: Watches,
    config: SharedConfig,
    pause: PauseState,
    token: CancellationToken,
}

/// Snapshot of config values needed for a poll cycle.
struct PollConfig {
    active_secs: u64,
    idle_secs: u64,
    notif: NotificationConfig,
    workflows: Vec<String>,
    ignored: Vec<String>,
    sound_on_failure: bool,
    sound_file: Option<String>,
}

fn format_duration(d: Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else {
        let mins = secs / 60;
        let rem = secs % 60;
        if rem == 0 {
            format!("{mins}m")
        } else {
            format!("{mins}m {rem}s")
        }
    }
}

impl Poller {
    async fn is_paused(&self) -> bool {
        let p = self.pause.lock().await;
        p.is_some_and(|deadline| Instant::now() < deadline)
    }

    fn repo_and_branch(&self) -> (String, String) {
        let (repo, branch) = parse_watch_key(&self.key);
        (repo.to_string(), branch.to_string())
    }

    /// Returns `true` if this watch is still active. Logs and returns `false` if removed.
    async fn is_active(&self) -> bool {
        let w = self.watches.lock().await;
        if w.contains_key(&self.key) {
            true
        } else {
            tracing::info!(key = self.key, "Watch cancelled");
            false
        }
    }

    /// Sleep for `duration`, returning `false` if cancelled during sleep.
    async fn cancellable_sleep(&self, duration: Duration) -> bool {
        tokio::select! {
            () = tokio::time::sleep(duration) => true,
            () = self.token.cancelled() => {
                tracing::info!(key = self.key, "Shutting down poller");
                false
            }
        }
    }

    #[tracing::instrument(skip_all, fields(key = self.key))]
    async fn run(self) {
        let (repo, branch) = self.repo_and_branch();
        let mut last_new_run_check: Option<Instant> = None;

        loop {
            let has_active = match self.read_active_state().await {
                Some(active) => active,
                None => return,
            };

            let pcfg = self.read_config(&repo, &branch).await;
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
                self.poll_active_runs(&repo, &branch, &pcfg).await;
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
        match w.get(&self.key) {
            Some(entry) => Some(entry.has_active_runs()),
            None => {
                tracing::info!(key = self.key, "Watch cancelled");
                None
            }
        }
    }

    async fn read_config(&self, repo: &str, branch: &str) -> PollConfig {
        let cfg = self.config.lock().await;
        PollConfig {
            active_secs: cfg.active_poll_seconds,
            idle_secs: cfg.idle_poll_seconds,
            notif: cfg.notifications_for(repo, branch),
            workflows: cfg.workflows_for(repo).to_vec(),
            ignored: cfg.ignored_workflows.clone(),
            sound_on_failure: cfg.sound_on_failure_for(repo),
            sound_file: cfg.sound_on_failure.sound_file.clone(),
        }
    }

    /// Poll all in-progress runs, notify on completion, handle failures.
    async fn poll_active_runs(&self, repo: &str, branch: &str, pcfg: &PollConfig) {
        let run_ids: Vec<u64> = {
            let w = self.watches.lock().await;
            match w.get(&self.key) {
                Some(entry) => entry.active_runs.keys().cloned().collect(),
                None => return,
            }
        };

        let mut changed = false;

        for run_id in run_ids {
            if self.token.is_cancelled() {
                return;
            }

            let run = match gh_run_status(repo, run_id).await {
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
                // Extract duration before removing from active_runs
                let elapsed = {
                    let w = self.watches.lock().await;
                    w.get(&self.key)
                        .and_then(|e| e.active_runs.get(&run_id))
                        .map(|a| a.started_at.elapsed())
                };

                if self.is_active().await {
                    self.notify_completion(&run, repo, branch, pcfg, elapsed)
                        .await;
                }

                tracing::info!(
                    key = self.key, run_id,
                    sha = run.short_sha(), conclusion = %run.conclusion,
                    "Build completed"
                );

                let mut w = self.watches.lock().await;
                if let Some(entry) = w.get_mut(&self.key) {
                    entry.record_completion(&run);
                }
                changed = true;
            } else {
                let mut w = self.watches.lock().await;
                if let Some(entry) = w.get_mut(&self.key) {
                    entry.update_status(run_id, run.status);
                }
            }
        }

        if changed {
            save_watches(&self.watches).await;
        }
    }

    /// Check for runs newer than our high-water mark. Notify starts (and immediate completions).
    async fn check_for_new_runs(&self, repo: &str, branch: &str, pcfg: &PollConfig) {
        let last_seen = {
            let w = self.watches.lock().await;
            match w.get(&self.key) {
                Some(entry) => entry.last_seen_run_id,
                None => return,
            }
        };

        let runs = match gh_recent_runs(repo, branch).await {
            Ok(r) => r,
            Err(e) => {
                tracing::error!(key = self.key, error = %e, "Failed to check for new runs");
                return;
            }
        };

        let new_runs: Vec<&RunInfo> = runs
            .iter()
            .filter(|r| r.id > last_seen)
            .filter(|r| {
                !pcfg
                    .ignored
                    .iter()
                    .any(|i| r.workflow.eq_ignore_ascii_case(i))
            })
            .filter(|r| {
                pcfg.workflows.is_empty()
                    || pcfg
                        .workflows
                        .iter()
                        .any(|w| r.workflow.eq_ignore_ascii_case(w))
            })
            .collect();
        // Still bump the high-water mark from all runs (not just filtered) to avoid
        // re-checking filtered-out runs on every poll cycle.
        let all_new: Vec<&RunInfo> = runs.iter().filter(|r| r.id > last_seen).collect();
        if new_runs.is_empty() && all_new.is_empty() {
            return;
        }

        if !self.is_active().await {
            return;
        }

        for run in &new_runs {
            tracing::info!(
                key = self.key, run_id = run.id,
                sha = run.short_sha(), workflow = %run.workflow, title = %run.title,
                "New build detected"
            );
            self.notify_started(run, repo, branch, &pcfg.notif).await;

            if run.is_completed() {
                self.notify_completion(run, repo, branch, pcfg, None).await;
                tracing::info!(
                    key = self.key, run_id = run.id,
                    sha = run.short_sha(), conclusion = %run.conclusion,
                    "Build already completed"
                );
            }
        }

        {
            let mut w = self.watches.lock().await;
            if let Some(entry) = w.get_mut(&self.key) {
                // Incorporate only filtered runs for active tracking, but bump
                // high-water mark from all new runs.
                entry.incorporate_new_runs(&new_runs);
                if let Some(max_id) = all_new.iter().map(|r| r.id).max() {
                    entry.last_seen_run_id = entry.last_seen_run_id.max(max_id);
                }
            }
        }
        save_watches(&self.watches).await;
    }

    // -- Notifications --

    /// Notification group key: stacks notifications per workflow within a watch.
    fn notification_group(&self, run: &RunInfo) -> String {
        format!("{}#{}", self.key, run.workflow)
    }

    async fn notify_started(
        &self,
        run: &RunInfo,
        repo: &str,
        branch: &str,
        notif: &NotificationConfig,
    ) {
        if self.is_paused().await {
            return;
        }
        let group = self.notification_group(run);
        platform::send_notification(
            &format!("🔨 {} - started", run.workflow),
            &format!("[{branch}] {}", run.display_title()),
            notif.build_started,
            Some(&run.url(repo)),
            Some(&group),
        )
        .await;
    }

    async fn notify_completion(
        &self,
        run: &RunInfo,
        repo: &str,
        branch: &str,
        pcfg: &PollConfig,
        elapsed: Option<Duration>,
    ) {
        if self.is_paused().await {
            return;
        }
        let (emoji, level) = if run.succeeded() {
            ("✅", pcfg.notif.build_success)
        } else {
            ("❌", pcfg.notif.build_failure)
        };

        let duration_str = elapsed.map(|d| format!(" in {}", format_duration(d)));

        let mut body = format!("[{branch}] {}", run.display_title());
        if let Some(ds) = &duration_str {
            body.push_str(ds);
        }

        // Fetch failing step context for failed builds (best-effort)
        if !run.succeeded()
            && let Some(steps) = gh_failing_steps(repo, run.id).await
        {
            body.push_str(&format!("\nFailed: {steps}"));
        }

        let group = self.notification_group(run);
        platform::send_notification(
            &format!("{emoji} {} - {}", run.workflow, run.conclusion),
            &body,
            level,
            Some(&run.url(repo)),
            Some(&group),
        )
        .await;

        // Play sound on failure if enabled
        if !run.succeeded() && pcfg.sound_on_failure {
            platform::play_sound(pcfg.sound_file.as_deref()).await;
        }
    }
}

// -- Startup --

pub async fn startup_watches(
    watches: &Watches,
    config: &SharedConfig,
    handle: &WatcherHandle,
    pause: &PauseState,
) {
    let snapshot: Vec<String> = {
        let w = watches.lock().await;
        w.keys().cloned().collect()
    };

    recover_existing_watches(watches, config, handle, pause, &snapshot).await;
    start_new_config_watches(watches, config, handle, pause, &snapshot).await;
}

/// Resume persisted watches and recover any in-progress runs from GitHub.
async fn recover_existing_watches(
    watches: &Watches,
    config: &SharedConfig,
    handle: &WatcherHandle,
    pause: &PauseState,
    snapshot: &[String],
) {
    let futures: Vec<_> = snapshot
        .iter()
        .map(|key| {
            let (repo, branch) = parse_watch_key(key);
            tracing::info!(key, "Resuming watch");
            let repo = repo.to_string();
            let branch = branch.to_string();
            let key = key.clone();
            async move { (key, gh_recent_runs(&repo, &branch).await) }
        })
        .collect();

    for (key, result) in futures::future::join_all(futures).await {
        if let Ok(runs) = result {
            let mut w = watches.lock().await;
            if let Some(entry) = w.get_mut(&key) {
                for run in &runs {
                    if !run.is_completed() && !entry.active_runs.contains_key(&run.id) {
                        tracing::info!(key, run_id = run.id, "Recovering in-progress run");
                        entry.active_runs.insert(
                            run.id,
                            ActiveRun {
                                status: run.status.clone(),
                                started_at: Instant::now(),
                            },
                        );
                    }
                }
                // Bump high-water mark so check_for_new_runs doesn't re-notify.
                if let Some(max_id) = runs.iter().map(|r| r.id).max() {
                    entry.last_seen_run_id = entry.last_seen_run_id.max(max_id);
                }
            }
        } else if let Err(e) = &result {
            tracing::warn!(key, error = %e, "Could not recover runs");
        }

        spawn_poller(watches, config, handle, pause, key);
    }
}

/// Start watches for config repos that don't have persisted state yet.
async fn start_new_config_watches(
    watches: &Watches,
    config: &SharedConfig,
    handle: &WatcherHandle,
    pause: &PauseState,
    snapshot: &[String],
) {
    let new_watches: Vec<(String, String, String)> = {
        let cfg = config.lock().await;
        cfg.watched_repos()
            .into_iter()
            .flat_map(|repo| {
                cfg.branches_for(repo)
                    .iter()
                    .filter_map(|branch| {
                        let key = watch_key(repo, branch);
                        (!snapshot.contains(&key)).then(|| (repo.clone(), branch.clone(), key))
                    })
                    .collect::<Vec<_>>()
            })
            .collect()
    };

    let futures: Vec<_> = new_watches
        .into_iter()
        .map(|(repo, branch, key)| {
            tracing::info!(repo, branch, "Starting new watch from config");
            let watches = watches.clone();
            let config = config.clone();
            let handle = handle.clone();
            let pause = pause.clone();
            async move {
                match start_watch(&watches, &config, &handle, &pause, &repo, &branch, &key).await {
                    Ok(msg) | Err(msg) => tracing::info!("{msg}"),
                }
            }
        })
        .collect();

    futures::future::join_all(futures).await;
}

/// Find the most recent failed build across all branches of a repo.
pub fn last_failed_build<'a>(
    watches: &'a HashMap<String, WatchEntry>,
    repo: &str,
) -> Option<(String, &'a LastBuild)> {
    let prefix = format!("{repo}#");
    watches
        .iter()
        .filter(|(k, _)| k.starts_with(&prefix))
        .filter_map(|(k, entry)| {
            entry
                .last_build
                .as_ref()
                .filter(|b| b.conclusion != "success")
                .map(|b| (k.clone(), b))
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
        }
    }

    // -- Watch key tests --

    #[test]
    fn watch_key_format() {
        assert_eq!(watch_key("alice/myapp", "main"), "alice/myapp#main");
    }

    #[test]
    fn parse_watch_key_splits_correctly() {
        assert_eq!(parse_watch_key("alice/myapp#main"), ("alice/myapp", "main"));
    }

    #[test]
    fn parse_watch_key_falls_back_to_main() {
        assert_eq!(parse_watch_key("alice/myapp"), ("alice/myapp", "main"));
    }

    // -- WatchEntry state machine tests --

    #[test]
    fn record_completion_removes_run_and_sets_last_build() {
        let mut entry = make_entry();
        let run = make_run(101, "completed", "success");

        entry.record_completion(&run);

        assert!(!entry.active_runs.contains_key(&101));
        assert!(entry.active_runs.contains_key(&102));
        let lb = entry.last_build.unwrap();
        assert_eq!(lb.run_id, 101);
        assert_eq!(lb.conclusion, "success");
    }

    #[test]
    fn record_completion_clears_failure_count() {
        let mut entry = make_entry();
        entry.failure_counts.insert(101, 3);
        let run = make_run(101, "completed", "failure");

        entry.record_completion(&run);

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

        entry.update_status(101, "queued".to_string());

        assert_eq!(entry.active_runs[&101].status, "queued");
    }

    #[test]
    fn update_status_noop_when_same() {
        let mut entry = make_entry();

        entry.update_status(101, "in_progress".to_string());

        assert_eq!(entry.active_runs[&101].status, "in_progress");
    }

    #[test]
    fn update_status_noop_for_unknown_run() {
        let mut entry = make_entry();

        entry.update_status(999, "completed".to_string());

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
        entry.record_completion(&run);

        let persisted = entry.to_persisted();
        let restored = WatchEntry::from_persisted(persisted);

        assert_eq!(restored.last_seen_run_id, entry.last_seen_run_id);
        assert!(restored.active_runs.is_empty());
        assert!(restored.failure_counts.is_empty());
        assert_eq!(restored.last_build.unwrap().run_id, 101);
    }
}
