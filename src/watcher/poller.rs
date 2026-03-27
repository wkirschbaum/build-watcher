use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Notify;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::config::unix_now;
use crate::events::{EventBus, RunSnapshot, WatchEvent};
use crate::github::{GitHubClient, LastBuild, RunInfo};
use crate::history::push_build;
use crate::persistence::Persistence;
use crate::rate_limiter::compute_intervals;

use super::types::WatchKey;
use super::{
    RateLimitState, SharedConfig, Watches, collect_persisted, count_api_calls, filter_runs,
};

/// How often each poller refreshes the shared rate limit state.
const RATE_LIMIT_REFRESH_INTERVAL: Duration = Duration::from_secs(60);

/// Reason a `cancellable_sleep` call returned.
pub(super) enum WakeReason {
    Elapsed,
    ConfigChanged,
    Cancelled,
}

/// Per-repo/branch async polling task.
pub(super) struct Poller {
    pub(super) key: WatchKey,
    pub(super) watches: Watches,
    pub(super) config: SharedConfig,
    pub(super) rate_limit: RateLimitState,
    pub(super) token: CancellationToken,
    pub(super) events: EventBus,
    pub(super) github: Arc<dyn GitHubClient>,
    pub(super) persistence: Arc<dyn Persistence>,
    pub(super) history: crate::history::SharedHistory,
    pub(super) config_changed: Arc<Notify>,
    /// Last computed active poll interval, used to back-project our own API call count.
    /// Zero on the first cycle (conservative: all of `rl.used` attributed to external).
    pub(super) last_active_secs: u64,
}

/// Snapshot of config values needed for a poll cycle.
pub(super) struct PollConfig {
    pub(super) active_secs: u64,
    pub(super) idle_secs: u64,
    pub(super) workflows: Vec<String>,
    pub(super) ignored: Vec<String>,
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

    /// Sleep for `duration`. Returns the reason the sleep ended.
    async fn cancellable_sleep(&self, duration: Duration) -> WakeReason {
        tokio::select! {
            () = tokio::time::sleep(duration) => WakeReason::Elapsed,
            () = self.token.cancelled() => {
                tracing::info!(key = %self.key, "Shutting down poller");
                WakeReason::Cancelled
            }
            () = self.config_changed.notified() => WakeReason::ConfigChanged,
        }
    }

    /// Main poller loop. Two polling modes:
    /// - Active runs exist: poll their status every `active_secs` (fast, ~10s)
    /// - No active runs: check for new runs every `idle_secs` (slow, ~60s)
    /// New-run checks always happen at least every `idle_secs`, even during active polling.
    #[tracing::instrument(skip_all, fields(key = %self.key))]
    pub(super) async fn run(mut self) {
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
            self.last_active_secs = pcfg.active_secs;
            let delay = if has_active {
                pcfg.active_secs
            } else {
                pcfg.idle_secs
            };

            match self.cancellable_sleep(Duration::from_secs(delay)).await {
                WakeReason::Cancelled => return,
                WakeReason::ConfigChanged => {
                    // Force an immediate poll so the new aggression takes effect now,
                    // not just on the next scheduled cycle.
                    last_new_run_check = None;
                }
                WakeReason::Elapsed => {}
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
        let cfg = self.config.lock().await;
        let (active_secs, idle_secs) = compute_intervals(
            rate_limit.as_ref(),
            api_calls,
            crate::config::unix_now(),
            cfg.poll_aggression,
            self.last_active_secs,
        );
        PollConfig {
            active_secs,
            idle_secs,
            workflows: cfg.workflows_for(repo).to_vec(),
            ignored: cfg.ignored_workflows.clone(),
        }
    }

    /// Remove this watch and its config entry when the repo no longer exists.
    async fn remove_dead_watch(&self, repo: &str) {
        let persisted = {
            let mut w = self.watches.lock().await;
            let keys: Vec<WatchKey> = w.keys().filter(|k| k.matches_repo(repo)).cloned().collect();
            for key in &keys {
                w.remove(key);
            }
            w.iter()
                .map(|(k, v)| (k.clone(), v.to_persisted()))
                .collect()
        };
        if let Err(e) = self.persistence.save_watches(&persisted).await {
            tracing::error!(error = %e, "Failed to save watches after removing dead repo");
        }

        let snapshot = {
            let mut cfg = self.config.lock().await;
            cfg.repos.remove(repo);
            cfg.clone()
        };
        if let Err(e) = self.persistence.save_config(&snapshot).await {
            tracing::error!(error = %e, "Failed to save config after removing dead repo");
        }
    }

    /// Poll all in-progress runs, emit events on completion/status change, handle failures.
    /// The watch lock is released during each GitHub API call (high latency)
    /// and re-acquired for each state update to avoid holding it across awaits.
    pub(super) async fn poll_active_runs(&self, repo: &str, branch: &str) {
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

                let new_build = {
                    let mut w = self.watches.lock().await;
                    if let Some(entry) = w.get_mut(&self.key) {
                        entry.record_completion(&run, failing_steps, unix_now());
                        entry.last_build.clone()
                    } else {
                        None
                    }
                };
                if let Some(lb) = new_build {
                    let mut hist = self.history.lock().await;
                    push_build(&mut hist, &self.key, lb);
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
            if let Err(e) = self.persistence.save_watches(&persisted).await {
                tracing::error!(key = %self.key, error = %e, "Failed to persist watches");
            }
            let hist = self.history.lock().await.clone();
            if let Err(e) = self.persistence.save_history(&hist).await {
                tracing::error!(key = %self.key, error = %e, "Failed to persist history");
            }
        }
    }

    /// Check for runs newer than our high-water mark. Emit events for new and completed runs.
    pub(super) async fn check_for_new_runs(&self, repo: &str, branch: &str, pcfg: &PollConfig) {
        let last_seen = {
            let w = self.watches.lock().await;
            match w.get(&self.key) {
                Some(entry) => entry.last_seen_run_id,
                None => return,
            }
        };

        let runs = match self.github.recent_runs(repo, branch).await {
            Ok(r) => r,
            Err(e) if e.is_repo_not_found() => {
                tracing::warn!(key = %self.key, error = %e, "Repo not found, removing watch");
                self.remove_dead_watch(repo).await;
                return;
            }
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
                entry.incorporate_new_runs(&new_runs, Instant::now(), unix_now());
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
        if let Err(e) = self.persistence.save_watches(&persisted).await {
            tracing::error!(key = %self.key, error = %e, "Failed to persist watches");
        }

        // Push completed new runs into history (oldest→newest so newest ends at index 0).
        let now_unix = unix_now();
        let completed: Vec<LastBuild> = new_runs
            .iter()
            .filter(|r| r.is_completed())
            .map(|r| {
                let mut lb = r.to_last_build();
                lb.completed_at = Some(now_unix);
                lb.failing_steps = failing_steps_by_id.get(&r.id).and_then(|s| s.clone());
                lb
            })
            .collect();
        if !completed.is_empty() {
            let mut hist = self.history.lock().await;
            // new_runs is newest-first; iterate in reverse to push oldest first so newest
            // ends up at index 0 after all inserts.
            for lb in completed.into_iter().rev() {
                push_build(&mut hist, &self.key, lb);
            }
            let hist_snapshot = hist.clone();
            drop(hist);
            if let Err(e) = self.persistence.save_history(&hist_snapshot).await {
                tracing::error!(key = %self.key, error = %e, "Failed to persist history");
            }
        }
    }
}
