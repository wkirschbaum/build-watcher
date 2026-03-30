use std::collections::{HashMap, HashSet};
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
use crate::status::{RunConclusion, RunStatus};

use super::types::WatchKey;
use super::{
    RateLimitState, SharedConfig, Watches, collect_persisted, filter_runs, runs_for_branch,
};

/// Maximum individual `run_status` fallback calls when the batch endpoint misses runs.
const MAX_FALLBACK_CALLS: usize = 10;
/// Maximum `failing_steps` backfill calls per poll cycle to avoid rate-limit blowout.
const MAX_BACKFILL_CALLS: usize = 5;

/// Reason a `cancellable_sleep` call returned.
enum WakeReason {
    Elapsed,
    /// Config changed (e.g. new watch added) — treated identically to `Elapsed`.
    ConfigChanged,
    Cancelled,
}

/// State change detected during a poll cycle.
/// Collected from both poll methods and deduplicated before emission.
#[derive(Debug)]
pub(super) enum RunChange {
    Started {
        run: RunSnapshot,
    },
    Completed {
        run: RunSnapshot,
        conclusion: RunConclusion,
        elapsed: Option<f64>,
        failing_steps: Option<String>,
        failing_job_id: Option<u64>,
    },
    StatusChanged {
        run: RunSnapshot,
        from: RunStatus,
        to: RunStatus,
    },
}

impl RunChange {
    pub(super) fn run_id(&self) -> u64 {
        match self {
            Self::Started { run }
            | Self::Completed { run, .. }
            | Self::StatusChanged { run, .. } => run.run_id,
        }
    }

    pub(super) fn into_event(self) -> WatchEvent {
        match self {
            Self::Started { run } => WatchEvent::RunStarted(run),
            Self::Completed {
                run,
                conclusion,
                elapsed,
                failing_steps,
                failing_job_id,
            } => WatchEvent::RunCompleted {
                run,
                conclusion,
                elapsed,
                failing_steps,
                failing_job_id,
            },
            Self::StatusChanged { run, from, to } => WatchEvent::StatusChanged { run, from, to },
        }
    }
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
        loop {
            if !self.has_any_watches().await {
                tracing::info!(repo = %self.repo, "No more watches for repo, exiting poller");
                return;
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

            // Collect changes from both poll methods, then deduplicate by run_id
            // before emitting. This prevents double notifications when a run completes
            // between the two API calls within a single cycle.
            let mut changes = self.poll_active_runs_batch().await;
            changes.extend(self.check_for_new_runs_repo_wide().await);

            let mut seen = HashSet::new();
            for change in changes {
                if seen.insert(change.run_id()) {
                    self.events.emit(change.into_event());
                } else {
                    tracing::debug!(run_id = change.run_id(), "Suppressed duplicate event");
                }
            }
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

        {
            let mut cfg = self.config.lock().await;
            cfg.repos.remove(&self.repo);
        }
        // Re-read under the save lock so concurrent modifications aren't lost.
        let snapshot = self.config.lock().await.clone();
        if let Err(e) = crate::config::save_config_async(&snapshot).await {
            tracing::error!(error = %e, "Failed to save config after removing dead repo");
        }
    }

    /// Batch-check all active runs for this repo using a single API call.
    /// Falls back to individual `run_status` for runs missing from the batch response,
    /// capped at `MAX_FALLBACK_CALLS` to avoid rate-limit exhaustion.
    pub(super) async fn poll_active_runs_batch(&self) -> Vec<RunChange> {
        let mut changes = Vec::new();

        // Collect all (run_id, WatchKey) pairs for active runs in this repo.
        let active_run_keys: Vec<(u64, WatchKey)> = {
            let w = self.watches.lock().await;
            w.iter()
                .filter(|(k, _)| k.matches_repo(&self.repo))
                .flat_map(|(k, e)| e.active_runs.keys().map(move |&run_id| (run_id, k.clone())))
                .collect()
        };

        if active_run_keys.is_empty() {
            return changes;
        }

        // One API call to get all in-progress runs for the repo.
        let batch_runs = match self.github.in_progress_runs_for_repo(&self.repo).await {
            Ok(runs) => runs,
            Err(e) => {
                tracing::error!(repo = %self.repo, error = %e, "Failed to batch-check active runs");
                return changes;
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
                return changes;
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
                return changes;
            }

            if run.is_completed() {
                let elapsed = {
                    let w = self.watches.lock().await;
                    w.get(key)
                        .and_then(|e| e.active_runs.get(&run.id))
                        .map(|a| a.started_at.elapsed().as_secs_f64())
                };

                let failure_info = if run.succeeded() {
                    None
                } else {
                    self.github.failing_steps(&self.repo, run.id).await
                };
                let failing_steps = failure_info.as_ref().map(|f| f.steps.clone());
                let failing_job_id = failure_info.as_ref().and_then(|f| f.first_job_id);

                changes.push(RunChange::Completed {
                    run: RunSnapshot::from_run_info(run, &self.repo, &key.branch),
                    conclusion: run.run_conclusion(),
                    elapsed,
                    failing_steps: failing_steps.clone(),
                    failing_job_id,
                });

                tracing::info!(
                    key = %key, run_id = run.id,
                    sha = run.short_sha(), conclusion = %run.conclusion,
                    "Build completed"
                );

                let new_build = {
                    let mut w = self.watches.lock().await;
                    if let Some(entry) = w.get_mut(key) {
                        entry.record_completion(run, failing_steps, failing_job_id, unix_now());
                        entry.last_builds.get(&run.workflow).cloned()
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
                    changes.push(RunChange::StatusChanged {
                        run: RunSnapshot::from_run_info(run, &self.repo, &key.branch),
                        from: old_status,
                        to: run.status.clone(),
                    });
                }
            }
        }

        // Retry fetching failing_steps for failed builds that are missing them.
        // Give up after 10 minutes to avoid hammering the API indefinitely.
        {
            let now = unix_now();
            let missing: Vec<(WatchKey, u64, String)> = {
                let w = self.watches.lock().await;
                w.iter()
                    .filter(|(k, _)| k.repo == self.repo)
                    .flat_map(|(k, entry)| {
                        entry.last_builds.values().filter_map(move |lb| {
                            if lb.conclusion != "success"
                                && lb.failing_steps.is_none()
                                && lb.completed_at.is_some_and(|t| now.saturating_sub(t) < 600)
                            {
                                Some((k.clone(), lb.run_id, lb.workflow.clone()))
                            } else {
                                None
                            }
                        })
                    })
                    .collect()
            };
            for (key, run_id, workflow) in missing.into_iter().take(MAX_BACKFILL_CALLS) {
                if self.token.is_cancelled() {
                    break;
                }
                if let Some(info) = self.github.failing_steps(&self.repo, run_id).await {
                    let mut w = self.watches.lock().await;
                    if let Some(entry) = w.get_mut(&key)
                        && let Some(lb) = entry.last_builds.get_mut(&workflow)
                        && lb.run_id == run_id
                    {
                        lb.failing_steps = Some(info.steps);
                        lb.failing_job_id = info.first_job_id;
                        changed = true;
                    }
                }
            }
        }

        if changed {
            let persisted = collect_persisted(&self.watches).await;
            let hist = self.history.lock().await.clone();
            self.persistence.save_state(&persisted, &hist).await;
        }

        changes
    }

    /// Check for new runs across all watched branches using a single repo-wide API call.
    pub(super) async fn check_for_new_runs_repo_wide(&self) -> Vec<RunChange> {
        let mut changes = Vec::new();

        let branches = self.watched_branches().await;
        if branches.is_empty() {
            return changes;
        }

        let limit = super::scaled_repo_limit(branches.len() as u32);
        let all_runs = match self.github.recent_runs_for_repo(&self.repo, limit).await {
            Ok(r) => r,
            Err(e) if e.is_repo_not_found() => {
                tracing::warn!(repo = %self.repo, error = %e, "Repo not found, removing watches");
                self.remove_dead_repo().await;
                return changes;
            }
            Err(e) => {
                tracing::error!(repo = %self.repo, error = %e, "Failed to check for new runs");
                return changes;
            }
        };

        let bpcfg = self.branch_poll_config().await;
        let mut any_changed = false;
        let mut backfill_calls = 0usize;

        for key in &branches {
            let branch_runs = runs_for_branch(&all_runs, &key.branch);

            let (last_seen, active_ids, prev_last_builds) = {
                let w = self.watches.lock().await;
                match w.get(key) {
                    Some(entry) => {
                        let ids: Vec<u64> = entry.active_runs.keys().copied().collect();
                        (entry.last_seen_run_id, ids, entry.last_builds.clone())
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
            let new_runs = filter_runs(&unseen, &bpcfg.workflows, &bpcfg.ignored);
            // Check for re-runs: any last_build's run_id appears in the API response
            // but with a different conclusion or back to in-progress.
            let may_have_rerun = prev_last_builds.values().any(|lb| {
                branch_runs.iter().any(|r| {
                    r.id == lb.run_id && (!r.is_completed() || r.conclusion != lb.conclusion)
                })
            });
            if new_runs.is_empty() && unseen.is_empty() && !may_have_rerun {
                continue;
            }

            // Collect failure info for already-completed runs.
            // Maps run_id → (failing_steps, failing_job_id).
            let mut failure_by_id: HashMap<u64, (Option<String>, Option<u64>)> = HashMap::new();

            for run in &new_runs {
                let snapshot = RunSnapshot::from_run_info(run, &self.repo, &key.branch);

                if run.is_completed() {
                    // Run completed before we saw it — emit only RunCompleted,
                    // not a spurious RunStarted that would cause a double notification.
                    let failure_info = if run.succeeded() {
                        None
                    } else {
                        self.github.failing_steps(&self.repo, run.id).await
                    };
                    let failing_steps = failure_info.as_ref().map(|f| f.steps.clone());
                    let failing_job_id = failure_info.as_ref().and_then(|f| f.first_job_id);
                    tracing::info!(
                        key = %key, run_id = run.id,
                        sha = run.short_sha(), conclusion = %run.conclusion,
                        "Build already completed"
                    );
                    changes.push(RunChange::Completed {
                        run: snapshot,
                        conclusion: run.run_conclusion(),
                        elapsed: None,
                        failing_steps: failing_steps.clone(),
                        failing_job_id,
                    });
                    failure_by_id.insert(run.id, (failing_steps, failing_job_id));
                } else {
                    tracing::info!(
                        key = %key, run_id = run.id,
                        sha = run.short_sha(), workflow = %run.workflow, title = %run.title,
                        "New build detected"
                    );
                    changes.push(RunChange::Started { run: snapshot });
                }
            }

            // Detect re-runs: if any last_build's run_id appears in the API response
            // with a different status (back to in_progress) or different conclusion,
            // the run was re-run on GitHub and we need to pick up the change.
            let mut rerun_detected = false;
            for lb in prev_last_builds.values() {
                let Some(rerun) = branch_runs.iter().find(|r| r.id == lb.run_id) else {
                    continue;
                };
                if !rerun.is_completed() {
                    // Re-run is in progress — track it as active again.
                    tracing::info!(
                        key = %key, run_id = rerun.id,
                        "Re-run detected (now in progress)"
                    );
                    let snapshot = RunSnapshot::from_run_info(rerun, &self.repo, &key.branch);
                    changes.push(RunChange::Started { run: snapshot });
                    rerun_detected = true;
                } else if rerun.conclusion != lb.conclusion {
                    // Re-run completed with a different conclusion.
                    let failure_info = if rerun.succeeded() {
                        None
                    } else {
                        self.github.failing_steps(&self.repo, rerun.id).await
                    };
                    let failing_steps = failure_info.as_ref().map(|f| f.steps.clone());
                    let failing_job_id = failure_info.as_ref().and_then(|f| f.first_job_id);
                    tracing::info!(
                        key = %key, run_id = rerun.id,
                        old_conclusion = %lb.conclusion, new_conclusion = %rerun.conclusion,
                        "Re-run completed with different conclusion"
                    );
                    let snapshot = RunSnapshot::from_run_info(rerun, &self.repo, &key.branch);
                    changes.push(RunChange::Completed {
                        run: snapshot,
                        conclusion: rerun.run_conclusion(),
                        elapsed: None,
                        failing_steps: failing_steps.clone(),
                        failing_job_id,
                    });
                    failure_by_id.insert(rerun.id, (failing_steps, failing_job_id));
                    rerun_detected = true;
                }
            }

            {
                let mut w = self.watches.lock().await;
                if let Some(entry) = w.get_mut(key) {
                    entry.incorporate_new_runs(&new_runs, Instant::now(), unix_now());

                    // Apply re-run state changes.
                    if rerun_detected {
                        for lb in prev_last_builds.values() {
                            let Some(rerun) = branch_runs.iter().find(|r| r.id == lb.run_id) else {
                                continue;
                            };
                            if !rerun.is_completed() {
                                // Add back to active runs for poll_active_runs_batch to track.
                                entry.active_runs.insert(
                                    rerun.id,
                                    super::types::ActiveRun::from_run(rerun, Instant::now()),
                                );
                            } else {
                                // Update last_builds with new conclusion.
                                let mut new_lb = rerun.to_last_build();
                                new_lb.completed_at = Some(unix_now());
                                if let Some((steps, job_id)) = failure_by_id.get(&rerun.id) {
                                    new_lb.failing_steps = steps.clone();
                                    new_lb.failing_job_id = *job_id;
                                }
                                entry.last_builds.insert(new_lb.workflow.clone(), new_lb);
                            }
                        }
                    }

                    // Apply failure info to any matching last_builds.
                    for lb in entry.last_builds.values_mut() {
                        if let Some((steps, job_id)) = failure_by_id.get(&lb.run_id) {
                            lb.failing_steps = steps.clone();
                            lb.failing_job_id = *job_id;
                        }
                    }
                    // Bump the high-water mark for ALL unseen runs (including filtered-out
                    // ones) so ignored workflows don't re-trigger on the next poll.
                    if let Some(max_id) = unseen.iter().map(|r| r.id).max() {
                        entry.last_seen_run_id = entry.last_seen_run_id.max(max_id);
                    }
                    any_changed = any_changed || rerun_detected;
                    if !new_runs.is_empty() || !unseen.is_empty() {
                        any_changed = true;
                    }
                }
            }

            // Push completed new runs (and re-run completions) into history.
            let now_unix = unix_now();
            let mut completed: Vec<LastBuild> = new_runs
                .iter()
                .filter(|r| r.is_completed())
                .map(|r| {
                    let mut lb = r.to_last_build();
                    lb.completed_at = Some(now_unix);
                    if let Some((steps, job_id)) = failure_by_id.get(&r.id) {
                        lb.failing_steps = steps.clone();
                        lb.failing_job_id = *job_id;
                    }
                    lb
                })
                .collect();
            // Include re-run completions in history.
            if rerun_detected {
                for lb in prev_last_builds.values() {
                    if let Some(rerun) = branch_runs.iter().find(|r| r.id == lb.run_id)
                        && rerun.is_completed()
                        && rerun.conclusion != lb.conclusion
                    {
                        let mut new_lb = rerun.to_last_build();
                        new_lb.completed_at = Some(now_unix);
                        if let Some((steps, job_id)) = failure_by_id.get(&rerun.id) {
                            new_lb.failing_steps = steps.clone();
                            new_lb.failing_job_id = *job_id;
                        }
                        completed.push(new_lb);
                    }
                }
            }
            if !completed.is_empty() {
                let mut hist = self.history.lock().await;
                for lb in completed.into_iter().rev() {
                    push_build(&mut hist, key, lb);
                }
            }

            // Backfill: if any last_build is a failure with no failing_steps, fetch them now.
            // Capped across all branches to avoid rate-limit blowout with many failures.
            if backfill_calls < MAX_BACKFILL_CALLS {
                let needs_backfill: Vec<(String, u64)> = {
                    let w = self.watches.lock().await;
                    w.get(key)
                        .map(|e| {
                            e.last_builds
                                .iter()
                                .filter(|(_, lb)| {
                                    lb.conclusion != "success" && lb.failing_steps.is_none()
                                })
                                .map(|(wf, lb)| (wf.clone(), lb.run_id))
                                .collect()
                        })
                        .unwrap_or_default()
                };
                for (wf_name, run_id) in needs_backfill {
                    if backfill_calls >= MAX_BACKFILL_CALLS {
                        break;
                    }
                    backfill_calls += 1;
                    let info = self.github.failing_steps(&self.repo, run_id).await;
                    if let Some(info) = info {
                        let mut w = self.watches.lock().await;
                        if let Some(entry) = w.get_mut(key)
                            && let Some(lb) = entry.last_builds.get_mut(&wf_name)
                        {
                            tracing::info!(key = %key, run_id, "Backfilled failing steps");
                            lb.failing_steps = Some(info.steps);
                            lb.failing_job_id = info.first_job_id;
                            any_changed = true;
                        }
                    }
                }
            }
        }

        if any_changed {
            let persisted = collect_persisted(&self.watches).await;
            let hist = self.history.lock().await.clone();
            self.persistence.save_state(&persisted, &hist).await;
        }

        changes
    }
}
