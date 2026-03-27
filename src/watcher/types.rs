use std::collections::HashMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::time::Instant;

use crate::config::{load_json, state_dir};
use crate::github::{GhError, LastBuild, RunInfo};

use super::Watches;

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
    pub(super) fn from_run(run: &RunInfo, now: Instant) -> Self {
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
    pub(super) fn parse(s: &str) -> Self {
        match s.rsplit_once('#') {
            Some((repo, branch)) => Self::new(repo, branch),
            None => {
                tracing::warn!(
                    key = s,
                    "Watch key missing #branch separator, falling back to '{FALLBACK_BRANCH}'"
                );
                Self::new(s, FALLBACK_BRANCH)
            }
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

pub(super) const MAX_GH_FAILURES: u8 = 5;

/// Runtime state per repo/branch: high-water mark + in-progress runs.
#[derive(Debug, Clone, Default)]
pub struct WatchEntry {
    pub(super) last_seen_run_id: u64,
    pub active_runs: HashMap<u64, ActiveRun>,
    pub(super) failure_counts: HashMap<u64, u8>,
    pub last_build: Option<LastBuild>,
}

impl WatchEntry {
    pub(crate) fn from_persisted(p: PersistedWatch) -> Self {
        Self {
            last_seen_run_id: p.last_seen_run_id,
            active_runs: HashMap::new(),
            failure_counts: HashMap::new(),
            last_build: p.last_build,
        }
    }

    pub(crate) fn to_persisted(&self) -> PersistedWatch {
        PersistedWatch {
            last_seen_run_id: self.last_seen_run_id,
            last_build: self.last_build.clone(),
        }
    }

    pub(super) fn has_active_runs(&self) -> bool {
        !self.active_runs.is_empty()
    }

    pub(super) fn record_completion(
        &mut self,
        run: &RunInfo,
        failing_steps: Option<String>,
        now_unix: u64,
    ) -> Option<Duration> {
        let elapsed = self
            .active_runs
            .remove(&run.id)
            .map(|a| a.started_at.elapsed());
        self.failure_counts.remove(&run.id);
        // Bump high-water mark so check_for_new_runs doesn't re-discover
        // this run as "new" after it completes.
        self.last_seen_run_id = self.last_seen_run_id.max(run.id);
        let mut last_build = run.to_last_build();
        last_build.failing_steps = failing_steps;
        last_build.completed_at = Some(now_unix);
        last_build.duration_secs = elapsed.map(|d| d.as_secs());
        self.last_build = Some(last_build);
        elapsed
    }

    pub(super) fn clear_failure_count(&mut self, run_id: u64) {
        self.failure_counts.remove(&run_id);
    }

    /// Record a poll failure. Returns `true` if the run was removed after too many failures.
    pub(super) fn record_failure(&mut self, run_id: u64, error: &GhError) -> bool {
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
    pub(super) fn update_status(&mut self, run_id: u64, new_status: &str) -> Option<String> {
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
    pub(super) fn incorporate_new_runs(
        &mut self,
        new_runs: &[&RunInfo],
        now: Instant,
        now_unix: u64,
    ) {
        if let Some(max_id) = new_runs.iter().map(|r| r.id).max() {
            self.last_seen_run_id = max_id;
        }
        for run in new_runs.iter().rev() {
            if run.is_completed() {
                let mut lb = run.to_last_build();
                lb.completed_at = Some(now_unix);
                self.last_build = Some(lb);
            } else {
                self.active_runs
                    .insert(run.id, ActiveRun::from_run(run, now));
            }
        }
    }
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
