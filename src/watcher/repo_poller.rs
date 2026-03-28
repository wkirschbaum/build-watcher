use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Notify;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::config::unix_now;
use crate::events::{EventBus, RunSnapshot, WatchEvent};
use crate::github::{DEFAULT_REPO_LIMIT, GitHubClient, LastBuild, RunInfo};
use crate::history::push_build;
use crate::persistence::Persistence;
use crate::rate_limiter::compute_intervals;

use super::types::WatchKey;
use super::{
    RateLimitState, SharedConfig, Watches, collect_persisted, filter_runs, runs_for_branch,
};

/// How often each poller refreshes the shared rate limit state.
const RATE_LIMIT_REFRESH_INTERVAL: Duration = Duration::from_secs(60);
/// Maximum individual `run_status` fallback calls when the batch endpoint misses runs.
const MAX_FALLBACK_CALLS: usize = 10;

/// Reason a `cancellable_sleep` call returned.
enum WakeReason {
    Elapsed,
    /// Config changed (e.g. new watch added) — treated identically to `Elapsed`.
    ConfigChanged,
    Cancelled,
}

/// Snapshot of config values needed for a poll cycle, per branch.
struct BranchPollConfig {
    workflows: Vec<String>,
    ignored: Vec<String>,
}

/// Per-repo async polling task. Consolidates all branch watches for a single repo
/// into one poller, making repo-wide API calls and fanning results to per-branch state.
pub(super) struct RepoPoller {
    pub(super) repo: String,
    pub(super) watches: Watches,
    pub(super) config: SharedConfig,
    pub(super) rate_limit: RateLimitState,
    pub(super) token: CancellationToken,
    pub(super) events: EventBus,
    pub(super) github: Arc<dyn GitHubClient>,
    pub(super) persistence: Arc<dyn Persistence>,
    pub(super) history: crate::history::SharedHistory,
    pub(super) config_changed: Arc<Notify>,
    pub(super) last_active_secs: u64,
}

impl RepoPoller {
    /// Collect all watched branches for this repo.
    async fn watched_branches(&self) -> Vec<WatchKey> {
        let w = self.watches.lock().await;
        w.keys()
            .filter(|k| k.matches_repo(&self.repo))
            .cloned()
            .collect()
    }

    /// Returns `true` if ANY branch for this repo has active runs.
    async fn has_any_active(&self) -> bool {
        let w = self.watches.lock().await;
        w.iter()
            .any(|(k, e)| k.matches_repo(&self.repo) && e.has_active_runs())
    }

    /// Returns `true` if at least one branch is still being watched for this repo.
    async fn has_any_watches(&self) -> bool {
        let w = self.watches.lock().await;
        w.keys().any(|k| k.matches_repo(&self.repo))
    }

    async fn cancellable_sleep(&self, duration: Duration) -> WakeReason {
        tokio::select! {
            () = tokio::time::sleep(duration) => WakeReason::Elapsed,
            () = self.token.cancelled() => {
                tracing::info!(repo = %self.repo, "Shutting down repo poller");
                WakeReason::Cancelled
            }
            () = self.config_changed.notified() => WakeReason::ConfigChanged,
        }
    }

    /// Read config and compute poll intervals.
    async fn read_config(&self) -> (u64, u64) {
        let rate_limit = self.rate_limit.lock().await.clone();
        let api_calls = {
            let w = self.watches.lock().await;
            super::count_api_calls(&w)
        };
        let aggression = self.config.lock().await.poll_aggression;
        compute_intervals(
            rate_limit.as_ref(),
            api_calls,
            unix_now(),
            aggression,
            self.last_active_secs,
        )
    }

    /// Read per-branch workflow config for a given repo.
    async fn branch_poll_config(&self) -> BranchPollConfig {
        let cfg = self.config.lock().await;
        BranchPollConfig {
            workflows: cfg.workflows_for(&self.repo).to_vec(),
            ignored: cfg.ignored_workflows.clone(),
        }
    }

    /// Main poller loop.
    #[tracing::instrument(skip_all, fields(repo = %self.repo))]
    pub(super) async fn run(mut self) {
        let mut last_rate_limit_refresh: Option<Instant> = None;

        loop {
            if !self.has_any_watches().await {
                tracing::info!(repo = %self.repo, "No more watches for repo, exiting poller");
                return;
            }

            // Refresh rate limit every minute (free call).
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
                        tracing::warn!(repo = %self.repo, error = %e, "Failed to fetch rate limit");
                    }
                }
                last_rate_limit_refresh = Some(Instant::now());
            }

            let has_active = self.has_any_active().await;
            let (active_secs, idle_secs) = self.read_config().await;
            self.last_active_secs = active_secs;
            let delay = if has_active { active_secs } else { idle_secs };

            match self.cancellable_sleep(Duration::from_secs(delay)).await {
                WakeReason::Cancelled => return,
                WakeReason::ConfigChanged | WakeReason::Elapsed => {}
            }

            if !self.has_any_watches().await {
                tracing::info!(repo = %self.repo, "No more watches for repo, exiting poller");
                return;
            }

            // Always run both: poll_active_runs_batch returns early when there are no
            // tracked active runs, so calling it unconditionally is cheap.
            // Running check_for_new_runs_repo_wide every cycle ensures new builds are
            // detected within one sleep interval regardless of active state.
            self.poll_active_runs_batch().await;
            self.check_for_new_runs_repo_wide().await;
        }
    }

    /// Remove this watch and its config entry when the repo no longer exists.
    async fn remove_dead_repo(&self) {
        let persisted = {
            let mut w = self.watches.lock().await;
            let keys: Vec<WatchKey> = w
                .keys()
                .filter(|k| k.matches_repo(&self.repo))
                .cloned()
                .collect();
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
            cfg.repos.remove(&self.repo);
            cfg.clone()
        };
        if let Err(e) = crate::config::save_config_async(&snapshot).await {
            tracing::error!(error = %e, "Failed to save config after removing dead repo");
        }
    }

    /// Batch-check all active runs for this repo using a single API call.
    /// Falls back to individual `run_status` for runs missing from the batch response,
    /// capped at `MAX_FALLBACK_CALLS` to avoid rate-limit exhaustion.
    pub(super) async fn poll_active_runs_batch(&self) {
        // Collect all (run_id, WatchKey) pairs for active runs in this repo.
        let active_run_keys: Vec<(u64, WatchKey)> = {
            let w = self.watches.lock().await;
            w.iter()
                .filter(|(k, _)| k.matches_repo(&self.repo))
                .flat_map(|(k, e)| e.active_runs.keys().map(move |&run_id| (run_id, k.clone())))
                .collect()
        };

        if active_run_keys.is_empty() {
            return;
        }

        // One API call to get all in-progress runs for the repo.
        let batch_runs = match self.github.in_progress_runs_for_repo(&self.repo).await {
            Ok(runs) => runs,
            Err(e) => {
                tracing::error!(repo = %self.repo, error = %e, "Failed to batch-check active runs");
                return;
            }
        };
        let batch_by_id: HashMap<u64, &RunInfo> = batch_runs.iter().map(|r| (r.id, r)).collect();

        // Separate runs found in batch vs missing (need fallback).
        let mut found_runs: Vec<(RunInfo, WatchKey)> = Vec::new();
        let mut missing_runs: Vec<(u64, WatchKey)> = Vec::new();

        for (run_id, key) in &active_run_keys {
            if let Some(&run) = batch_by_id.get(run_id) {
                found_runs.push((run.clone(), key.clone()));
            } else {
                missing_runs.push((*run_id, key.clone()));
            }
        }

        // Clear failure counts for found runs in a single lock acquisition.
        {
            let mut w = self.watches.lock().await;
            for (run, key) in &found_runs {
                if let Some(entry) = w.get_mut(key) {
                    entry.clear_failure_count(run.id);
                }
            }
        }

        let found_in_batch = found_runs.len();

        // Fallback: individually check missing runs, capped to avoid rate-limit exhaustion.
        if missing_runs.len() > MAX_FALLBACK_CALLS {
            tracing::warn!(
                repo = %self.repo,
                missing = missing_runs.len(),
                cap = MAX_FALLBACK_CALLS,
                "Too many runs missing from batch, capping fallback calls"
            );
        }
        let mut fallback_errors: Vec<(u64, WatchKey, crate::github::GhError)> = Vec::new();
        for (run_id, key) in missing_runs.iter().take(MAX_FALLBACK_CALLS) {
            if self.token.is_cancelled() {
                return;
            }
            match self.github.run_status(&self.repo, *run_id).await {
                Ok(run) => found_runs.push((run, key.clone())),
                Err(e) => fallback_errors.push((*run_id, key.clone(), e)),
            }
        }
        // Apply all fallback results in a single lock acquisition.
        {
            let mut w = self.watches.lock().await;
            for (run, key) in &found_runs[found_in_batch..] {
                if let Some(entry) = w.get_mut(key) {
                    entry.clear_failure_count(run.id);
                }
            }
            for (run_id, key, e) in &fallback_errors {
                if let Some(entry) = w.get_mut(key) {
                    entry.record_failure(*run_id, e);
                }
            }
        }

        // Process all resolved runs.
        let mut changed = false;
        for (run, key) in &found_runs {
            if self.token.is_cancelled() {
                return;
            }

            if run.is_completed() {
                let elapsed = {
                    let w = self.watches.lock().await;
                    w.get(key)
                        .and_then(|e| e.active_runs.get(&run.id))
                        .map(|a| a.started_at.elapsed().as_secs_f64())
                };

                let failing_steps = if run.succeeded() {
                    None
                } else {
                    self.github.failing_steps(&self.repo, run.id).await
                };

                self.events.emit(WatchEvent::RunCompleted {
                    run: RunSnapshot::from_run_info(run, &self.repo, &key.branch),
                    conclusion: run.run_conclusion(),
                    elapsed,
                    failing_steps: failing_steps.clone(),
                });

                tracing::info!(
                    key = %key, run_id = run.id,
                    sha = run.short_sha(), conclusion = %run.conclusion,
                    "Build completed"
                );

                let new_build = {
                    let mut w = self.watches.lock().await;
                    if let Some(entry) = w.get_mut(key) {
                        entry.record_completion(run, failing_steps, unix_now());
                        entry.last_build.clone()
                    } else {
                        None
                    }
                };
                if let Some(lb) = new_build {
                    let mut hist = self.history.lock().await;
                    push_build(&mut hist, key, lb);
                }
                changed = true;
            } else {
                let mut w = self.watches.lock().await;
                if let Some(entry) = w.get_mut(key)
                    && let Some(old_status) = entry.update_status(run.id, &run.status)
                {
                    self.events.emit(WatchEvent::StatusChanged {
                        run: RunSnapshot::from_run_info(run, &self.repo, &key.branch),
                        from: old_status,
                        to: run.status.clone(),
                    });
                }
            }
        }

        if changed {
            let persisted = collect_persisted(&self.watches).await;
            let hist = self.history.lock().await.clone();
            self.persistence.save_state(&persisted, &hist).await;
        }
    }

    /// Check for new runs across all watched branches using a single repo-wide API call.
    pub(super) async fn check_for_new_runs_repo_wide(&self) {
        let branches = self.watched_branches().await;
        if branches.is_empty() {
            return;
        }

        let all_runs = match self
            .github
            .recent_runs_for_repo(&self.repo, DEFAULT_REPO_LIMIT)
            .await
        {
            Ok(r) => r,
            Err(e) if e.is_repo_not_found() => {
                tracing::warn!(repo = %self.repo, error = %e, "Repo not found, removing watches");
                self.remove_dead_repo().await;
                return;
            }
            Err(e) => {
                tracing::error!(repo = %self.repo, error = %e, "Failed to check for new runs");
                return;
            }
        };

        let bpcfg = self.branch_poll_config().await;
        let mut any_changed = false;

        for key in &branches {
            let branch_runs = runs_for_branch(&all_runs, &key.branch);

            let (last_seen, active_ids) = {
                let w = self.watches.lock().await;
                match w.get(key) {
                    Some(entry) => {
                        let ids: Vec<u64> = entry.active_runs.keys().copied().collect();
                        (entry.last_seen_run_id, ids)
                    }
                    None => continue,
                }
            };

            // unseen = runs newer than high-water mark AND not already tracked as active
            let unseen: Vec<&RunInfo> = branch_runs
                .iter()
                .filter(|r| r.id > last_seen && !active_ids.contains(&r.id))
                .copied()
                .collect();
            let unseen_as_owned: Vec<RunInfo> = unseen.iter().map(|r| (*r).clone()).collect();
            let new_runs = filter_runs(&unseen_as_owned, &bpcfg.workflows, &bpcfg.ignored);
            if new_runs.is_empty() && unseen.is_empty() {
                continue;
            }

            // Collect failing_steps for already-completed runs.
            let mut failing_steps_by_id: HashMap<u64, Option<String>> = HashMap::new();

            for run in &new_runs {
                let snapshot = RunSnapshot::from_run_info(run, &self.repo, &key.branch);

                if run.is_completed() {
                    // Run completed before we saw it — emit only RunCompleted,
                    // not a spurious RunStarted that would cause a double notification.
                    let failing_steps = if run.succeeded() {
                        None
                    } else {
                        self.github.failing_steps(&self.repo, run.id).await
                    };
                    tracing::info!(
                        key = %key, run_id = run.id,
                        sha = run.short_sha(), conclusion = %run.conclusion,
                        "Build already completed"
                    );
                    self.events.emit(WatchEvent::RunCompleted {
                        run: snapshot,
                        conclusion: run.run_conclusion(),
                        elapsed: None,
                        failing_steps: failing_steps.clone(),
                    });
                    failing_steps_by_id.insert(run.id, failing_steps);
                } else {
                    tracing::info!(
                        key = %key, run_id = run.id,
                        sha = run.short_sha(), workflow = %run.workflow, title = %run.title,
                        "New build detected"
                    );
                    self.events.emit(WatchEvent::RunStarted(snapshot));
                }
            }

            {
                let mut w = self.watches.lock().await;
                if let Some(entry) = w.get_mut(key) {
                    entry.incorporate_new_runs(&new_runs, Instant::now(), unix_now());
                    if let Some(ref mut lb) = entry.last_build
                        && let Some(steps) = failing_steps_by_id.get(&lb.run_id)
                    {
                        lb.failing_steps = steps.clone();
                    }
                    // Bump the high-water mark for ALL unseen runs (including filtered-out
                    // ones) so ignored workflows don't re-trigger on the next poll.
                    if let Some(max_id) = unseen.iter().map(|r| r.id).max() {
                        entry.last_seen_run_id = entry.last_seen_run_id.max(max_id);
                    }
                    any_changed = true;
                }
            }

            // Push completed new runs into history.
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
                for lb in completed.into_iter().rev() {
                    push_build(&mut hist, key, lb);
                }
            }
        }

        if any_changed {
            let persisted = collect_persisted(&self.watches).await;
            let hist = self.history.lock().await.clone();
            self.persistence.save_state(&persisted, &hist).await;
        }
    }
}
