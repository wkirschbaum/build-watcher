use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::config::{Config, NotificationConfig, load_json, save_json, state_dir};
use crate::github::{GhError, LastBuild, RunInfo, gh_recent_runs, gh_run_status};
use crate::platform;

pub type SharedConfig = Arc<Mutex<Config>>;
pub type Watches = Arc<Mutex<HashMap<String, WatchEntry>>>;

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
    pub active_runs: HashMap<u64, String>,
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

    fn record_completion(&mut self, run: &RunInfo) {
        self.active_runs.remove(&run.id);
        self.failure_counts.remove(&run.id);
        self.last_build = Some(run.to_last_build());
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
        if let Some(old) = self.active_runs.get(&run_id)
            && *old != new_status
        {
            tracing::debug!(run_id, old = %old, new = %new_status, "Run status changed");
            self.active_runs.insert(run_id, new_status);
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
                self.active_runs.insert(run.id, run.status.clone());
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
        let latest = &runs[0];
        format!(
            "{repo} [{branch}]: latest build already completed ({}), watching for new builds\n  {}: {} {}\n  {}",
            latest.conclusion,
            latest.workflow,
            latest.title,
            latest.short_sha(),
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
        last_build: last_completed.map(|r| r.to_last_build()),
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

    spawn_poller(watches, config, handle, key.to_string());
    Ok(msg)
}

fn spawn_poller(watches: &Watches, config: &SharedConfig, handle: &WatcherHandle, key: String) {
    let poller = Poller {
        key,
        watches: watches.clone(),
        config: config.clone(),
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
    token: CancellationToken,
}

impl Poller {
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

            let (active_secs, idle_secs, notif) = self.read_config(&repo, &branch).await;
            let delay = if has_active { active_secs } else { idle_secs };

            if !self.cancellable_sleep(Duration::from_secs(delay)).await {
                return;
            }
            if !self.is_active().await {
                return;
            }

            if has_active {
                self.poll_active_runs(&repo, &branch, &notif).await;
            }

            let due =
                last_new_run_check.is_none_or(|t| t.elapsed() >= Duration::from_secs(idle_secs));
            if due {
                self.check_for_new_runs(&repo, &branch, &notif).await;
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

    async fn read_config(&self, repo: &str, branch: &str) -> (u64, u64, NotificationConfig) {
        let cfg = self.config.lock().await;
        (
            cfg.active_poll_seconds,
            cfg.idle_poll_seconds,
            cfg.notifications_for(repo, branch),
        )
    }

    /// Poll all in-progress runs, notify on completion, handle failures.
    async fn poll_active_runs(&self, repo: &str, branch: &str, notif: &NotificationConfig) {
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
                if self.is_active().await {
                    self.notify_completion(&run, repo, branch, notif).await;
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
    async fn check_for_new_runs(&self, repo: &str, branch: &str, notif: &NotificationConfig) {
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

        let new_runs: Vec<&RunInfo> = runs.iter().filter(|r| r.id > last_seen).collect();
        if new_runs.is_empty() {
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
            self.notify_started(run, repo, branch, notif).await;

            if run.is_completed() {
                self.notify_completion(run, repo, branch, notif).await;
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
                entry.incorporate_new_runs(&new_runs);
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
        let group = self.notification_group(run);
        platform::send_notification(
            &format!("🔨 {} - started", run.workflow),
            &format!("[{branch}] {}", run.title),
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
        notif: &NotificationConfig,
    ) {
        let (emoji, level) = if run.succeeded() {
            ("✅", notif.build_success)
        } else {
            ("❌", notif.build_failure)
        };
        let group = self.notification_group(run);
        platform::send_notification(
            &format!("{emoji} {} - {}", run.workflow, run.conclusion),
            &format!("[{branch}] {}", run.title),
            level,
            Some(&run.url(repo)),
            Some(&group),
        )
        .await;
    }
}

// -- Startup --

pub async fn startup_watches(watches: &Watches, config: &SharedConfig, handle: &WatcherHandle) {
    let snapshot: Vec<String> = {
        let w = watches.lock().await;
        w.keys().cloned().collect()
    };

    recover_existing_watches(watches, config, handle, &snapshot).await;
    start_new_config_watches(watches, config, handle, &snapshot).await;
}

/// Resume persisted watches and recover any in-progress runs from GitHub.
async fn recover_existing_watches(
    watches: &Watches,
    config: &SharedConfig,
    handle: &WatcherHandle,
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
                        entry.active_runs.insert(run.id, run.status.clone());
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

        spawn_poller(watches, config, handle, key);
    }
}

/// Start watches for config repos that don't have persisted state yet.
async fn start_new_config_watches(
    watches: &Watches,
    config: &SharedConfig,
    handle: &WatcherHandle,
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
            async move {
                match start_watch(&watches, &config, &handle, &repo, &branch, &key).await {
                    Ok(msg) | Err(msg) => tracing::info!("{msg}"),
                }
            }
        })
        .collect();

    futures::future::join_all(futures).await;
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

    fn make_entry() -> WatchEntry {
        WatchEntry {
            last_seen_run_id: 100,
            active_runs: HashMap::from([
                (101, "in_progress".to_string()),
                (102, "queued".to_string()),
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

        assert_eq!(entry.active_runs[&101], "queued");
    }

    #[test]
    fn update_status_noop_when_same() {
        let mut entry = make_entry();

        entry.update_status(101, "in_progress".to_string());

        assert_eq!(entry.active_runs[&101], "in_progress");
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
        assert_eq!(entry.active_runs[&200], "in_progress");
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
        assert_eq!(entry.active_runs[&201], "in_progress");
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
