use std::collections::HashMap;

use crate::config::unix_now;
use crate::events::RunSnapshot;
use crate::github::{LastBuild, RunInfo};
use crate::history::push_build;
use crate::watcher::{ActiveRun, WatchKey, collect_persisted};

use super::{
    BACKFILL_WINDOW_SECS, MAX_BACKFILL_CALLS, MAX_FALLBACK_CALLS, NOT_FOUND_THRESHOLD, RepoPoller,
    RunChange,
};

impl RepoPoller {
    /// Batch-check all active runs for this repo using a single API call.
    /// Falls back to individual `run_status` for runs missing from the batch response,
    /// capped at `MAX_FALLBACK_CALLS` to avoid rate-limit exhaustion.
    pub(in crate::watcher) async fn poll_active_runs_batch(&self) -> Vec<RunChange> {
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
        // Runs dropped after MAX_GH_FAILURES get an "unknown" last_build record so
        // they don't silently vanish from the TUI without any trace.
        let mut abandoned: Vec<(WatchKey, LastBuild)> = Vec::new();
        {
            let mut w = self.watches.lock().await;
            for (run, key) in &found_runs[found_in_batch..] {
                if let Some(entry) = w.get_mut(key) {
                    entry.clear_failure_count(run.id);
                }
            }
            for (run_id, key, e) in &fallback_errors {
                if let Some(entry) = w.get_mut(key)
                    && let Some(active) = entry.record_failure(*run_id, e)
                {
                    let lb = active.to_abandoned_last_build(*run_id);
                    entry.last_builds.insert(lb.workflow.clone(), lb.clone());
                    abandoned.push((key.clone(), lb));
                }
            }
        }

        let (run_changes, changed) = self.process_resolved_runs(&found_runs).await;
        changes.extend(run_changes);

        if !abandoned.is_empty() {
            let mut hist = self.history.lock().await;
            for (key, lb) in abandoned.iter().rev() {
                push_build(&mut hist, key, lb.clone());
            }
        }

        if changed || !abandoned.is_empty() {
            let persisted = collect_persisted(&self.watches).await;
            let hist = self.history.lock().await.clone();
            self.persistence.save_state(&persisted, &hist).await;
        }

        changes
    }

    /// Process all resolved runs: detect completions and status changes.
    /// Returns `(changes, any_state_changed)`.
    async fn process_resolved_runs(
        &self,
        found_runs: &[(RunInfo, WatchKey)],
    ) -> (Vec<RunChange>, bool) {
        let mut changes = Vec::new();
        let mut changed = false;

        // Phase 1: fetch failure info via API (no lock held).
        struct CompletedInfo {
            run_idx: usize,
            failing_steps: Option<String>,
            failing_job_id: Option<u64>,
        }
        let mut completions: Vec<CompletedInfo> = Vec::new();

        for (i, (run, _key)) in found_runs.iter().enumerate() {
            if self.token.is_cancelled() {
                return (changes, changed);
            }
            if run.is_completed() {
                let failure_info = if run.succeeded() {
                    None
                } else {
                    self.github.failing_steps(&self.repo, run.id).await
                };
                completions.push(CompletedInfo {
                    run_idx: i,
                    failing_steps: failure_info.as_ref().map(|f| f.steps.clone()),
                    failing_job_id: failure_info.as_ref().and_then(|f| f.first_job_id),
                });
            }
        }

        let detect_flakes = self.config.read().await.detect_flakes;

        // Resolve flake status for each completion (Success with prior failure on same SHA).
        let mut flaky_by_idx: HashMap<usize, bool> = HashMap::new();
        if detect_flakes {
            let hist = self.history.lock().await;
            for c in &completions {
                let (run, key) = &found_runs[c.run_idx];
                let is_recovered = run.succeeded()
                    && crate::history::is_flake(&hist, key, &run.workflow, &run.head_sha);
                flaky_by_idx.insert(c.run_idx, is_recovered);
            }
        }
        let flake_for = |idx: usize| *flaky_by_idx.get(&idx).unwrap_or(&false);

        // Emit events for completions, copying author from the ActiveRun
        // (which holds it from initial detection) before it gets removed.
        for c in &completions {
            let (run, key) = &found_runs[c.run_idx];
            let mut snapshot = RunSnapshot::from_run_info(run, &self.repo, &key.branch);
            {
                let w = self.watches.lock().await;
                if let Some(active) = w.get(key).and_then(|e| e.active_runs.get(&run.id)) {
                    snapshot.actor.clone_from(&active.actor);
                    snapshot.commit_author.clone_from(&active.commit_author);
                }
            }
            let flaky = flake_for(c.run_idx);
            changes.push(RunChange::Completed {
                run: snapshot,
                conclusion: run.run_conclusion(),
                elapsed: run.duration_secs().map(|s| s as f64),
                failing_steps: c.failing_steps.clone(),
                failing_job_id: c.failing_job_id,
                flaky,
            });
            tracing::info!(
                key = %key, run_id = run.id,
                sha = run.short_sha(), conclusion = %run.conclusion, flaky,
                "Build completed"
            );
        }

        // Phase 2: single lock to apply all completions and status updates.
        let mut new_builds: Vec<(WatchKey, crate::github::LastBuild)> = Vec::new();
        {
            let mut w = self.watches.lock().await;
            for c in &completions {
                let (run, key) = &found_runs[c.run_idx];
                if let Some(entry) = w.get_mut(key) {
                    entry.record_completion(
                        run,
                        c.failing_steps.clone(),
                        c.failing_job_id,
                        flake_for(c.run_idx),
                    );
                    if let Some(lb) = entry.last_builds.get(&run.workflow) {
                        new_builds.push((key.clone(), lb.clone()));
                    }
                }
            }
            // Status updates for non-completed runs.
            for (run, key) in found_runs {
                if !run.is_completed()
                    && let Some(entry) = w.get_mut(key)
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

        // Push completed builds to history.
        if !new_builds.is_empty() {
            changed = true;
            let mut hist = self.history.lock().await;
            for (key, lb) in new_builds {
                push_build(&mut hist, &key, lb);
            }
        }

        (changes, changed)
    }

    /// Retry fetching failing_steps for failed builds that are missing them.
    /// Gives up after 10 minutes to avoid hammering the API indefinitely.
    /// Returns `true` if any state was updated.
    async fn backfill_failing_steps(&self) -> bool {
        let now = unix_now();
        let missing: Vec<(WatchKey, u64, String)> = {
            let w = self.watches.lock().await;
            w.iter()
                .filter(|(k, _)| k.repo == self.repo)
                .flat_map(|(k, entry)| {
                    entry.last_builds.values().filter_map(move |lb| {
                        if lb.conclusion != crate::github::RunConclusion::Success
                            && lb.failing_steps.is_none()
                            && lb
                                .completed_at
                                .is_some_and(|t| now.saturating_sub(t) < BACKFILL_WINDOW_SECS)
                        {
                            Some((k.clone(), lb.run_id, lb.workflow.clone()))
                        } else {
                            None
                        }
                    })
                })
                .collect()
        };

        // Fetch all failing steps outside the lock.
        let mut results: Vec<(WatchKey, u64, String, crate::github::FailureInfo)> = Vec::new();
        for (key, run_id, workflow) in missing.into_iter().take(MAX_BACKFILL_CALLS) {
            if self.token.is_cancelled() {
                break;
            }
            if let Some(info) = self.github.failing_steps(&self.repo, run_id).await {
                results.push((key, run_id, workflow, info));
            }
        }

        if results.is_empty() {
            return false;
        }

        // Apply all results in a single lock.
        let mut changed = false;
        let mut w = self.watches.lock().await;
        for (key, run_id, workflow, info) in results {
            if let Some(entry) = w.get_mut(&key)
                && let Some(lb) = entry.last_builds.get_mut(&workflow)
                && lb.run_id == run_id
            {
                lb.failing_steps = Some(info.steps);
                lb.failing_job_id = info.first_job_id;
                changed = true;
            }
        }
        changed
    }

    /// Check for new runs across all watched branches using a single repo-wide API call.
    pub(in crate::watcher) async fn check_for_new_runs_repo_wide(
        &mut self,
        cached_prs: Option<&[crate::github::PrInfo]>,
    ) -> Vec<RunChange> {
        let mut changes = Vec::new();

        let branches = self.watched_branches().await;
        if branches.is_empty() {
            return changes;
        }

        let limit = super::super::scaled_repo_limit(branches.len() as u32);
        let all_runs = match self.github.recent_runs_for_repo(&self.repo, limit).await {
            Ok(r) => {
                self.not_found_count = 0;
                r
            }
            Err(e) if e.is_repo_not_found() => {
                self.not_found_count += 1;
                if self.not_found_count >= NOT_FOUND_THRESHOLD {
                    tracing::warn!(
                        repo = %self.repo, count = self.not_found_count,
                        "Repo not found on {} consecutive polls, removing watches",
                        NOT_FOUND_THRESHOLD
                    );
                    self.remove_dead_repo().await;
                } else {
                    tracing::warn!(
                        repo = %self.repo, count = self.not_found_count,
                        error = %e,
                        "Repo not found (attempt {}/{}), will retry",
                        self.not_found_count, NOT_FOUND_THRESHOLD
                    );
                }
                return changes;
            }
            Err(e) => {
                tracing::error!(repo = %self.repo, error = %e, "Failed to check for new runs");
                return changes;
            }
        };

        // Sync discovered branches: add new, remove stale.
        let branches = self.sync_branches(&all_runs, branches, cached_prs).await;

        let run_filters = self.run_filters().await;
        let (show_author, detect_flakes) = {
            let cfg = self.config.read().await;
            (cfg.show_author, cfg.detect_flakes)
        };
        let mut any_changed = false;

        for key in &branches {
            let branch_runs = super::super::runs_for_branch(&all_runs, &key.branch);

            let (last_seen, active_ids, prev_last_builds, is_initial) = {
                let w = self.watches.lock().await;
                match w.get(key) {
                    Some(entry) => {
                        let ids: Vec<u64> = entry.active_runs.keys().copied().collect();
                        (
                            entry.last_seen_run_id,
                            ids,
                            entry.last_builds.clone(),
                            entry.waiting,
                        )
                    }
                    None => continue,
                }
            };

            // unseen = runs newer than high-water mark AND not already tracked as active.
            // On the initial seed after restart, use >= so that in-progress runs whose
            // ID equals `last_seen_run_id` are recaptured (they were active when the
            // daemon last stopped and their ID *is* the high-water mark).
            let unseen: Vec<&RunInfo> = branch_runs
                .iter()
                .filter(|r| {
                    let dominated = active_ids.contains(&r.id);
                    let newer = if is_initial {
                        r.id >= last_seen
                    } else {
                        r.id > last_seen
                    };
                    newer && !dominated
                })
                .copied()
                .collect();
            let new_runs = run_filters.filter(&unseen);
            // Identify re-runs: last_build run_ids that reappear in the API with
            // a changed state. Skip runs already tracked as active (those are
            // handled by poll_active_runs_batch).
            let reruns: Vec<(&RunInfo, &LastBuild)> = prev_last_builds
                .values()
                .filter_map(|lb| {
                    let r = branch_runs.iter().find(|r| r.id == lb.run_id)?;
                    let dominated = active_ids.contains(&r.id);
                    let changed = !r.is_completed() || r.run_conclusion() != lb.conclusion;
                    (!dominated && changed).then_some((*r, lb))
                })
                .collect();

            if unseen.is_empty() && reruns.is_empty() {
                continue;
            }

            // On initial seed (waiting entry), skip notifications and extra API
            // calls — just update state below.
            let mut failure_by_id: HashMap<u64, (Option<String>, Option<u64>)> = HashMap::new();
            let mut author_by_id: HashMap<u64, crate::github::RunAuthorInfo> = HashMap::new();
            // Flake status by run_id. Populated only when detect_flakes is on AND
            // this is not the initial seed (history is empty during seeding).
            let mut flaky_runs: HashMap<u64, bool> = HashMap::new();

            if !is_initial {
                // -- Pre-fetch failure info outside the lock (async API calls). --

                for run in &new_runs {
                    if run.is_completed()
                        && !run.succeeded()
                        && let Some(info) = self.github.failing_steps(&self.repo, run.id).await
                    {
                        failure_by_id.insert(run.id, (Some(info.steps), info.first_job_id));
                    }
                }
                for (rerun, _lb) in &reruns {
                    if rerun.is_completed()
                        && !rerun.succeeded()
                        && let Some(info) = self.github.failing_steps(&self.repo, rerun.id).await
                    {
                        failure_by_id.insert(rerun.id, (Some(info.steps), info.first_job_id));
                    }
                }

                // -- Collect author info from the run data (no extra API call needed). --

                if show_author {
                    for run in &new_runs {
                        if let Some(actor) = run.actor.clone() {
                            author_by_id.insert(
                                run.id,
                                crate::github::RunAuthorInfo {
                                    actor,
                                    commit_author: run.commit_author.clone(),
                                },
                            );
                        }
                    }
                    for (rerun, _) in &reruns {
                        if let std::collections::hash_map::Entry::Vacant(e) =
                            author_by_id.entry(rerun.id)
                            && let Some(actor) = rerun.actor.clone()
                        {
                            e.insert(crate::github::RunAuthorInfo {
                                actor,
                                commit_author: rerun.commit_author.clone(),
                            });
                        }
                    }
                }

                // Resolve flake status for completed Success runs in this batch
                // (one history lock per branch, not per run).
                if detect_flakes {
                    let hist = self.history.lock().await;
                    for run in &new_runs {
                        if run.is_completed()
                            && run.succeeded()
                            && crate::history::is_flake(&hist, key, &run.workflow, &run.head_sha)
                        {
                            flaky_runs.insert(run.id, true);
                        }
                    }
                    for (rerun, _lb) in &reruns {
                        if rerun.is_completed()
                            && rerun.succeeded()
                            && crate::history::is_flake(
                                &hist,
                                key,
                                &rerun.workflow,
                                &rerun.head_sha,
                            )
                        {
                            flaky_runs.insert(rerun.id, true);
                        }
                    }
                }
                let is_flaky = |run_id: u64| flaky_runs.get(&run_id).copied().unwrap_or(false);

                // -- Emit events for new runs. --

                for run in &new_runs {
                    let mut snapshot = RunSnapshot::from_run_info(run, &self.repo, &key.branch);
                    if let Some(info) = author_by_id.get(&run.id) {
                        snapshot.set_author(info);
                    }
                    if run.is_completed() {
                        let (failing_steps, failing_job_id) =
                            failure_by_id.get(&run.id).cloned().unwrap_or((None, None));
                        tracing::info!(
                            key = %key, run_id = run.id,
                            sha = run.short_sha(), conclusion = %run.conclusion,
                            "Build already completed"
                        );
                        changes.push(RunChange::Completed {
                            run: snapshot,
                            conclusion: run.run_conclusion(),
                            elapsed: None,
                            failing_steps,
                            failing_job_id,
                            flaky: is_flaky(run.id),
                        });
                    } else {
                        tracing::info!(
                            key = %key, run_id = run.id,
                            sha = run.short_sha(), workflow = %run.workflow, title = %run.title,
                            "New build detected"
                        );
                        changes.push(RunChange::Started { run: snapshot });
                    }
                }

                // -- Emit events for reruns. --

                for (rerun, lb) in &reruns {
                    let mut snapshot = RunSnapshot::from_run_info(rerun, &self.repo, &key.branch);
                    if let Some(info) = author_by_id.get(&rerun.id) {
                        snapshot.actor = Some(info.actor.clone());
                        snapshot.commit_author = info.commit_author.clone();
                    }
                    if !rerun.is_completed() {
                        tracing::info!(
                            key = %key, run_id = rerun.id,
                            "Re-run detected (now in progress)"
                        );
                        changes.push(RunChange::Started { run: snapshot });
                    } else {
                        let (failing_steps, failing_job_id) = failure_by_id
                            .get(&rerun.id)
                            .cloned()
                            .unwrap_or((None, None));
                        let flaky = is_flaky(rerun.id);
                        tracing::info!(
                            key = %key, run_id = rerun.id,
                            old_conclusion = %lb.conclusion, new_conclusion = %rerun.conclusion, flaky,
                            "Re-run completed with different conclusion"
                        );
                        changes.push(RunChange::Completed {
                            run: snapshot,
                            conclusion: rerun.run_conclusion(),
                            elapsed: None,
                            failing_steps,
                            failing_job_id,
                            flaky,
                        });
                    }
                }
            } // end if !is_initial

            // -- Single lock: apply all state changes. --

            {
                let mut w = self.watches.lock().await;
                if let Some(entry) = w.get_mut(key) {
                    entry.incorporate_new_runs(&new_runs);

                    for (rerun, _lb) in &reruns {
                        if !rerun.is_completed() {
                            entry
                                .active_runs
                                .insert(rerun.id, ActiveRun::from_run(rerun));
                        } else {
                            let mut new_lb = rerun.to_last_build();
                            if let Some((steps, job_id)) = failure_by_id.get(&rerun.id) {
                                new_lb.failing_steps = steps.clone();
                                new_lb.failing_job_id = *job_id;
                            }
                            new_lb.flaky = flaky_runs.get(&rerun.id).copied().unwrap_or(false);
                            entry.last_builds.insert(new_lb.workflow.clone(), new_lb);
                        }
                    }

                    entry.apply_author_info(&author_by_id);

                    // Apply failure info and flake status to last builds.
                    for lb in entry.last_builds.values_mut() {
                        if let Some((steps, job_id)) = failure_by_id.get(&lb.run_id) {
                            lb.failing_steps = steps.clone();
                            lb.failing_job_id = *job_id;
                        }
                        if let Some(&f) = flaky_runs.get(&lb.run_id) {
                            lb.flaky = f;
                        }
                    }
                    // Bump the high-water mark for ALL unseen runs (including filtered-out
                    // ones) so ignored workflows don't re-trigger on the next poll.
                    if let Some(max_id) = unseen.iter().map(|r| r.id).max() {
                        entry.last_seen_run_id = entry.last_seen_run_id.max(max_id);
                    }
                    if !new_runs.is_empty() || !unseen.is_empty() || !reruns.is_empty() {
                        any_changed = true;
                    }
                }
            }

            // Push completed new runs and rerun completions into history.
            let mut completed: Vec<LastBuild> = new_runs
                .iter()
                .filter(|r| r.is_completed())
                .map(|r| {
                    let mut lb = r.to_last_build();
                    if let Some((steps, job_id)) = failure_by_id.get(&r.id) {
                        lb.failing_steps = steps.clone();
                        lb.failing_job_id = *job_id;
                    }
                    lb.flaky = flaky_runs.get(&r.id).copied().unwrap_or(false);
                    lb
                })
                .collect();
            for (rerun, _lb) in &reruns {
                if rerun.is_completed() {
                    let mut new_lb = rerun.to_last_build();
                    if let Some((steps, job_id)) = failure_by_id.get(&rerun.id) {
                        new_lb.failing_steps = steps.clone();
                        new_lb.failing_job_id = *job_id;
                    }
                    new_lb.flaky = flaky_runs.get(&rerun.id).copied().unwrap_or(false);
                    completed.push(new_lb);
                }
            }
            if !completed.is_empty() {
                let mut hist = self.history.lock().await;
                for lb in completed.into_iter().rev() {
                    push_build(&mut hist, key, lb);
                }
            }
        }

        // Clear waiting flag for all branches — the poll succeeded.
        {
            let mut w = self.watches.lock().await;
            for (key, entry) in w.iter_mut() {
                if key.matches_repo(&self.repo) && entry.waiting {
                    entry.waiting = false;
                    any_changed = true;
                }
            }
        }

        let backfill_changed = self.backfill_failing_steps().await;

        if any_changed || backfill_changed {
            let persisted = collect_persisted(&self.watches).await;
            let hist = self.history.lock().await.clone();
            self.persistence.save_state(&persisted, &hist).await;
        }

        changes
    }
}
