use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use crate::config::unix_now;
use crate::events::{EventBus, RunSnapshot, WatchEvent};
use crate::github::GitHubClient;
use crate::persistence::Persistence;
use crate::rate_limiter::{PollInput, compute_interval};
use crate::status::{RunConclusion, RunStatus};

use super::types::WatchKey;
use super::{RateLimitState, SharedConfig, Watches};

mod branch_tracker;
mod pr_tracker;
mod run_tracker;

/// A single ignore-list filter dimension: extracts a string field from RunInfo
/// and checks it against a list of ignored values (case-insensitive).
pub(super) struct IgnoreFilter<'a> {
    pub field: fn(&crate::github::RunInfo) -> &str,
    pub ignored: &'a [String],
}

/// Snapshot of all run-filtering config for a repo: workflow allow-list + ignore lists.
/// Read once from config, then reused for all `filter_runs` calls in a poll cycle.
pub(super) struct RunFilters {
    pub workflows: Vec<String>,
    pub ignored_workflows: Vec<String>,
    pub ignored_events: Vec<String>,
}

impl RunFilters {
    pub(super) async fn from_config(
        config: &crate::config::SharedConfigManager,
        repo: &str,
    ) -> Self {
        let cfg = config.read().await;
        Self {
            workflows: cfg.workflows_for(repo).to_vec(),
            ignored_workflows: cfg.ignored_workflows.clone(),
            ignored_events: cfg.ignored_events_for(repo),
        }
    }

    pub(super) fn ignore_filters(&self) -> [IgnoreFilter<'_>; 2] {
        [
            IgnoreFilter {
                field: |r| &r.workflow,
                ignored: &self.ignored_workflows,
            },
            IgnoreFilter {
                field: |r| &r.event,
                ignored: &self.ignored_events,
            },
        ]
    }

    pub(super) fn filter<'a, R: std::borrow::Borrow<crate::github::RunInfo> + 'a>(
        &self,
        runs: &'a [R],
    ) -> Vec<&'a crate::github::RunInfo> {
        let filters = self.ignore_filters();
        filter_runs(runs, &self.workflows, &filters)
    }
}

pub(super) fn filter_runs<'a, R: std::borrow::Borrow<crate::github::RunInfo> + 'a>(
    runs: &'a [R],
    workflows: &[String],
    ignore_filters: &[IgnoreFilter<'_>],
) -> Vec<&'a crate::github::RunInfo> {
    runs.iter()
        .map(|r| r.borrow())
        .filter(|r| {
            !ignore_filters.iter().any(|f| {
                let val = (f.field)(r);
                f.ignored.iter().any(|i| val.eq_ignore_ascii_case(i))
            })
        })
        .filter(|r| {
            workflows.is_empty() || workflows.iter().any(|w| r.workflow.eq_ignore_ascii_case(w))
        })
        .collect()
}

/// Consecutive "repo not found" responses required before the repo is removed.
/// Guards against transient 404s triggering permanent repo deletion.
pub(super) const NOT_FOUND_THRESHOLD: u32 = 3;
/// Maximum individual `run_status` fallback calls when the batch endpoint misses runs.
const MAX_FALLBACK_CALLS: usize = 10;
/// Maximum `failing_steps` backfill calls per poll cycle to avoid rate-limit blowout.
const MAX_BACKFILL_CALLS: usize = 5;
/// Window (seconds) within which completed builds are eligible for failing-steps backfill.
const BACKFILL_WINDOW_SECS: u64 = 600;

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
    pub(super) discovered: super::DiscoveredBranches,
    pub(super) config_changed: Arc<Notify>,
    /// True until the first poll cycle completes — triggers a 1 s initial delay.
    pub(super) first_poll: bool,
    /// Last known merge state per PR number — used to detect transitions.
    pub(super) pr_states: HashMap<u64, crate::github::MergeState>,
    /// Consecutive "repo not found" responses from `recent_runs_for_repo`.
    /// Repo is only removed after `NOT_FOUND_THRESHOLD` consecutive 404s to
    /// avoid permanent deletion on a transient API error.
    pub(super) not_found_count: u32,
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

    /// Read config and compute the poll interval.
    async fn read_config(&self) -> u64 {
        let rate_limit = self.rate_limit.lock().await.clone();
        let api_calls = {
            let w = self.watches.lock().await;
            super::count_api_calls(&w)
        };
        let cfg = self.config.read().await;
        let aggression = cfg
            .repos
            .get(&self.repo)
            .and_then(|rc| rc.poll_aggression)
            .unwrap_or(cfg.poll_aggression);
        drop(cfg);
        compute_interval(&PollInput {
            rate_limit,
            calls_per_cycle: api_calls,
            now: unix_now(),
            aggression,
        })
    }

    /// Read run-filtering config for this repo.
    async fn run_filters(&self) -> RunFilters {
        RunFilters::from_config(&self.config, &self.repo).await
    }

    /// Main poller loop.
    #[tracing::instrument(skip_all, fields(repo = %self.repo))]
    pub(super) async fn run(mut self) {
        loop {
            if !self.has_any_watches().await {
                tracing::info!(repo = %self.repo, "No more watches for repo, exiting poller");
                return;
            }

            let delay = if self.first_poll {
                self.first_poll = false;
                1
            } else {
                self.read_config().await
            };

            let wall_before = unix_now();
            match self.cancellable_sleep(Duration::from_secs(delay)).await {
                WakeReason::Cancelled => return,
                WakeReason::ConfigChanged | WakeReason::Elapsed => {}
            }
            // If wall time advanced much more than the expected sleep duration, the
            // system was suspended. Reset watches so the next check_for_new_runs call
            // silently seeds state rather than notifying for builds that completed
            // during sleep. (active_runs are preserved and still processed normally.)
            if unix_now().saturating_sub(wall_before) > delay + 30 {
                tracing::info!(repo = %self.repo, "System wake detected; suppressing first post-wake poll");
                let mut w = self.watches.lock().await;
                for (key, entry) in w.iter_mut() {
                    if key.matches_repo(&self.repo) {
                        entry.waiting = true;
                    }
                }
            }

            if !self.has_any_watches().await {
                tracing::info!(repo = %self.repo, "No more watches for repo, exiting poller");
                return;
            }

            // Fetch open PRs once if watch_prs or auto_discover needs them.
            // Shared by both sync_branches (source-branch discovery) and poll_prs
            // (display/events) so we never call open_prs twice in one cycle.
            let open_prs = self.maybe_fetch_open_prs().await;

            // Collect changes from both poll methods, then deduplicate by run_id
            // before emitting. This prevents double notifications when a run completes
            // between the two API calls within a single cycle.
            let mut changes = self.poll_active_runs_batch().await;
            changes.extend(self.check_for_new_runs_repo_wide(open_prs.as_deref()).await);

            let mut seen = HashSet::new();
            for change in changes {
                if seen.insert(change.run_id()) {
                    self.events.emit(change.into_event());
                } else {
                    tracing::debug!(run_id = change.run_id(), "Suppressed duplicate event");
                }
            }

            // PR display update — every cycle regardless of whether builds are active.
            self.poll_prs_with(open_prs).await;
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

        // Clean up branch discovery state so discovered.json doesn't accumulate orphans.
        let updated_disc = {
            let mut disc = self.discovered.lock().await;
            disc.remove(&self.repo);
            disc.clone()
        };
        if let Err(e) = self.persistence.save_discovered(&updated_disc).await {
            tracing::error!(error = %e, "Failed to save branch discovery state after removing dead repo");
        }

        let repo = self.repo.clone();
        if let Err(e) = self
            .config
            .modify(|cfg| {
                cfg.repos.remove(&repo);
            })
            .await
        {
            tracing::error!(error = %e, "Failed to save config after removing dead repo");
        }
    }

    /// Fetch open PRs if `watch_prs` or `auto_discover_branches` is enabled for this repo.
    /// Returns `None` when neither feature is active (skips the API call entirely).
    async fn maybe_fetch_open_prs(&self) -> Option<Vec<crate::github::PrInfo>> {
        let needs_prs = {
            let cfg = self.config.read().await;
            cfg.auto_discover_for(&self.repo)
                || cfg.repos.get(&self.repo).is_some_and(|rc| rc.watch_prs)
        };
        if !needs_prs {
            return None;
        }
        match self.github.open_prs(&self.repo).await {
            Ok(prs) => Some(prs),
            Err(e) => {
                tracing::debug!(repo = %self.repo, error = %e, "Failed to fetch open PRs");
                None
            }
        }
    }
}
