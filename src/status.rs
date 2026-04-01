/// HTTP response types for `GET /status` and `GET /stats`.
///
/// Shared between the daemon (`server.rs`) and the TUI (`bin/bw.rs`).
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// GitHub Actions run conclusion values.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunConclusion {
    Success,
    Failure,
    Cancelled,
    #[serde(rename = "timed_out")]
    TimedOut,
    #[serde(rename = "startup_failure")]
    StartupFailure,
    #[default]
    #[serde(other)]
    Unknown,
}

/// GitHub Actions run status values.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    #[serde(rename = "in_progress")]
    InProgress,
    Queued,
    Waiting,
    Requested,
    Pending,
    Completed,
    #[default]
    #[serde(other)]
    Unknown,
}

impl FromStr for RunConclusion {
    type Err = ();
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "success" => Ok(Self::Success),
            "failure" => Ok(Self::Failure),
            "cancelled" => Ok(Self::Cancelled),
            "timed_out" => Ok(Self::TimedOut),
            "startup_failure" => Ok(Self::StartupFailure),
            _ => Ok(Self::Unknown),
        }
    }
}

impl FromStr for RunStatus {
    type Err = ();
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "in_progress" => Ok(Self::InProgress),
            "queued" => Ok(Self::Queued),
            "waiting" => Ok(Self::Waiting),
            "requested" => Ok(Self::Requested),
            "pending" => Ok(Self::Pending),
            "completed" => Ok(Self::Completed),
            _ => Ok(Self::Unknown),
        }
    }
}

impl RunConclusion {
    /// Return the raw string used for styling lookups (matches legacy string values).
    pub fn as_str(&self) -> &'static str {
        match self {
            RunConclusion::Success => "success",
            RunConclusion::Failure => "failure",
            RunConclusion::Cancelled => "cancelled",
            RunConclusion::TimedOut => "timed_out",
            RunConclusion::StartupFailure => "startup_failure",
            RunConclusion::Unknown => "",
        }
    }

    /// Severity ordering for display purposes: lower = worse.
    /// Failures (0) sort before cancellations (1), unknown (2), and success (3).
    pub fn severity(&self) -> u8 {
        match self {
            Self::Failure | Self::TimedOut | Self::StartupFailure => 0,
            Self::Cancelled => 1,
            Self::Success => 3,
            Self::Unknown => 2,
        }
    }
}

impl RunStatus {
    /// Return the raw string used for styling lookups (matches legacy string values).
    pub fn as_str(&self) -> &'static str {
        match self {
            RunStatus::InProgress => "in_progress",
            RunStatus::Queued => "queued",
            RunStatus::Waiting => "waiting",
            RunStatus::Requested => "requested",
            RunStatus::Pending => "pending",
            RunStatus::Completed => "completed",
            RunStatus::Unknown => "",
        }
    }
}

/// A single active run as returned by `GET /status`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ActiveRunView {
    pub run_id: u64,
    pub status: RunStatus,
    pub workflow: String,
    /// Human-readable title: plain commit title for pushes, "PR: …" for PRs.
    pub title: String,
    /// GitHub event type (e.g. `"push"`, `"pull_request"`).
    pub event: String,
    pub elapsed_secs: Option<f64>,
    /// GitHub Actions attempt number. 1 for the original run, 2+ for re-runs.
    #[serde(default = "crate::github::default_attempt")]
    pub attempt: u32,
    /// GitHub Actions run URL.
    #[serde(default)]
    pub url: String,
}

/// Summary of the last completed build as returned by `GET /status`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LastBuildView {
    pub run_id: u64,
    pub conclusion: RunConclusion,
    pub workflow: String,
    pub title: String,
    /// Comma-separated list of step names that failed, if available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failing_steps: Option<String>,
    /// Seconds since the build completed (not available after daemon restart).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub age_secs: Option<f64>,
    /// GitHub Actions attempt number. 1 for the original run, 2+ for re-runs.
    #[serde(default = "crate::github::default_attempt")]
    pub attempt: u32,
    /// Database ID of the first failed job (for constructing job URLs).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failing_job_id: Option<u64>,
    /// GitHub Actions run URL.
    #[serde(default)]
    pub url: String,
    /// Duration in seconds from run start to completion.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_secs: Option<u64>,
}

/// One watched repo/branch as returned by `GET /status`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WatchStatus {
    pub repo: String,
    pub branch: String,
    pub active_runs: Vec<ActiveRunView>,
    /// Last completed build per workflow.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub last_builds: Vec<LastBuildView>,
    /// Open PRs targeting this branch.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub prs: Vec<PrView>,
    /// Whether notifications are muted for this repo (all levels set to off).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub muted: bool,
    /// True until the first successful poll provides data.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub waiting: bool,
}

/// Compact PR view for the TUI status display.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrView {
    pub number: u64,
    pub title: String,
    pub url: String,
    pub author: String,
    pub merge_state: crate::github::MergeState,
    pub draft: bool,
}

/// Full response body for `GET /status`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusResponse {
    pub paused: bool,
    pub watches: Vec<WatchStatus>,
}

/// A single build history entry as returned by `GET /history`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryEntryView {
    pub id: u64,
    pub conclusion: String,
    pub workflow: String,
    pub title: String,
    /// Repo in `owner/name` format (populated by `/history/all`, empty for per-repo `/history`).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub repo: String,
    pub branch: String,
    pub event: String,
    /// Duration in seconds (`updated_at - created_at`), if timestamps are valid.
    pub duration_secs: Option<u64>,
    /// Seconds since `created_at`, computed at serialization time.
    pub age_secs: Option<u64>,
}

/// Global config defaults used by both `GET /defaults` (all fields populated)
/// and `POST /defaults` (only changed fields sent, `None` = no change).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DefaultsConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ignored_workflows: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ignored_events: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub poll_aggression: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_discover_branches: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch_filter: Option<String>,
}

/// Per-repo config view used by `GET /repo-config` and `POST /repo-config`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoConfigView {
    pub repo: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflows: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub watch_prs: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub poll_aggression: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_discover_branches: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch_filter: Option<String>,
}

/// Daemon stats returned by `GET /stats`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StatsResponse {
    pub uptime_secs: u64,
    pub active_poll_secs: u64,
    pub idle_poll_secs: u64,
    /// Current poll aggression level: "low", "medium", or "high".
    #[serde(default)]
    pub poll_aggression: String,
    pub rate_remaining: Option<u64>,
    pub rate_limit: Option<u64>,
    pub rate_reset_mins: Option<u64>,
    /// Events emitted when no subscribers were listening.
    #[serde(default)]
    pub dropped_events: u64,
}

impl StatusResponse {
    /// Apply a watch event to the local status snapshot.
    ///
    /// Updates only watches that already exist in the snapshot; new watches
    /// appear on the next `/status` resync.
    pub fn apply_event(&mut self, event: crate::events::WatchEvent) {
        use crate::events::WatchEvent;
        match event {
            WatchEvent::RunStarted(snap) => {
                let Some(watch) = find_watch_mut(&mut self.watches, &snap.repo, &snap.branch)
                else {
                    return;
                };
                if !watch.active_runs.iter().any(|r| r.run_id == snap.run_id) {
                    let title = snap.display_title();
                    watch.active_runs.push(ActiveRunView {
                        run_id: snap.run_id,
                        status: snap.status,
                        workflow: snap.workflow,
                        title,
                        event: snap.event,
                        elapsed_secs: Some(0.0),
                        attempt: snap.attempt,
                        url: snap.url,
                    });
                }
            }
            WatchEvent::RunCompleted {
                run,
                conclusion,
                failing_steps,
                failing_job_id,
                ..
            } => {
                let Some(watch) = find_watch_mut(&mut self.watches, &run.repo, &run.branch) else {
                    return;
                };
                watch.active_runs.retain(|r| r.run_id != run.run_id);
                let title = run.display_title();
                let new_build = LastBuildView {
                    run_id: run.run_id,
                    conclusion,
                    workflow: run.workflow.clone(),
                    title,
                    failing_steps,
                    age_secs: Some(0.0),
                    attempt: run.attempt,
                    failing_job_id,
                    url: run.url,
                    duration_secs: None,
                };
                // Replace existing entry for this workflow, or append.
                if let Some(existing) = watch
                    .last_builds
                    .iter_mut()
                    .find(|b| b.workflow == run.workflow)
                {
                    *existing = new_build;
                } else {
                    watch.last_builds.push(new_build);
                }
            }
            WatchEvent::StatusChanged { run, to, .. } => {
                let Some(watch) = find_watch_mut(&mut self.watches, &run.repo, &run.branch) else {
                    return;
                };
                if let Some(active) = watch
                    .active_runs
                    .iter_mut()
                    .find(|r| r.run_id == run.run_id)
                {
                    active.status = to;
                }
            }
            WatchEvent::PrStateChanged {
                repo,
                target_branch,
                number,
                title,
                url,
                to,
                ..
            } => {
                // Find the watch for the PR's target branch and upsert the PR.
                if let Some(watch) = find_watch_mut(&mut self.watches, &repo, &target_branch) {
                    if let Some(existing) = watch.prs.iter_mut().find(|p| p.number == number) {
                        existing.merge_state = to;
                    } else {
                        watch.prs.push(PrView {
                            number,
                            title,
                            url,
                            author: String::new(),
                            merge_state: to,
                            draft: false,
                        });
                    }
                }
            }
        }
    }
}

fn find_watch_mut<'a>(
    watches: &'a mut [WatchStatus],
    repo: &str,
    branch: &str,
) -> Option<&'a mut WatchStatus> {
    watches
        .iter_mut()
        .find(|w| w.repo == repo && w.branch == branch)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{RunSnapshot, WatchEvent};

    fn snap(repo: &str, branch: &str, run_id: u64) -> RunSnapshot {
        RunSnapshot {
            repo: repo.to_string(),
            branch: branch.to_string(),
            run_id,
            workflow: "CI".to_string(),
            title: "Fix bug".to_string(),
            event: "push".to_string(),
            status: RunStatus::Queued,
            attempt: 1,
            url: format!("https://github.com/{repo}/actions/runs/{run_id}"),
        }
    }

    fn watch(repo: &str, branch: &str) -> WatchStatus {
        WatchStatus {
            repo: repo.to_string(),
            branch: branch.to_string(),
            ..Default::default()
        }
    }

    fn status_with(watches: Vec<WatchStatus>) -> StatusResponse {
        StatusResponse {
            paused: false,
            watches,
        }
    }

    // -- RunConclusion / RunStatus serde round-trips --

    #[test]
    fn run_conclusion_serde_round_trip() {
        let cases = [
            (RunConclusion::Success, "\"success\""),
            (RunConclusion::Failure, "\"failure\""),
            (RunConclusion::Cancelled, "\"cancelled\""),
            (RunConclusion::TimedOut, "\"timed_out\""),
            (RunConclusion::StartupFailure, "\"startup_failure\""),
        ];
        for (variant, expected_json) in &cases {
            let json = serde_json::to_string(variant).unwrap();
            assert_eq!(&json, expected_json, "serializing {variant:?}");
            let decoded: RunConclusion = serde_json::from_str(&json).unwrap();
            assert_eq!(&decoded, variant, "round-trip for {variant:?}");
        }
    }

    #[test]
    fn run_conclusion_unknown_deserializes_to_unknown() {
        let decoded: RunConclusion = serde_json::from_str("\"action_required\"").unwrap();
        assert_eq!(decoded, RunConclusion::Unknown);
    }

    #[test]
    fn run_status_serde_round_trip() {
        let cases = [
            (RunStatus::InProgress, "\"in_progress\""),
            (RunStatus::Queued, "\"queued\""),
            (RunStatus::Waiting, "\"waiting\""),
            (RunStatus::Requested, "\"requested\""),
            (RunStatus::Pending, "\"pending\""),
            (RunStatus::Completed, "\"completed\""),
        ];
        for (variant, expected_json) in &cases {
            let json = serde_json::to_string(variant).unwrap();
            assert_eq!(&json, expected_json, "serializing {variant:?}");
            let decoded: RunStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(&decoded, variant, "round-trip for {variant:?}");
        }
    }

    #[test]
    fn run_status_unknown_deserializes_to_unknown() {
        let decoded: RunStatus = serde_json::from_str("\"some_future_status\"").unwrap();
        assert_eq!(decoded, RunStatus::Unknown);
    }

    // -- apply_event --

    #[test]
    fn run_started_inserts_active_run() {
        let mut status = status_with(vec![watch("alice/app", "main")]);
        status.apply_event(WatchEvent::RunStarted(snap("alice/app", "main", 1)));

        let runs = &status.watches[0].active_runs;
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].run_id, 1);
        assert_eq!(runs[0].status, RunStatus::Queued);
        assert_eq!(runs[0].workflow, "CI");
        assert_eq!(runs[0].elapsed_secs, Some(0.0));
    }

    #[test]
    fn run_completed_moves_to_last_build() {
        let mut status = status_with(vec![WatchStatus {
            repo: "alice/app".to_string(),
            branch: "main".to_string(),
            active_runs: vec![ActiveRunView {
                run_id: 7,
                status: RunStatus::InProgress,
                workflow: "CI".to_string(),
                title: "Fix bug".to_string(),
                event: "push".to_string(),
                elapsed_secs: Some(30.0),
                attempt: 1,
                url: String::new(),
            }],
            ..Default::default()
        }]);

        status.apply_event(WatchEvent::RunCompleted {
            run: snap("alice/app", "main", 7),
            conclusion: RunConclusion::Success,
            elapsed: Some(35.0),
            failing_steps: None,
            failing_job_id: None,
        });

        assert!(status.watches[0].active_runs.is_empty());
        assert_eq!(status.watches[0].last_builds.len(), 1);
        let lb = &status.watches[0].last_builds[0];
        assert_eq!(lb.run_id, 7);
        assert_eq!(lb.conclusion, RunConclusion::Success);
    }

    #[test]
    fn unknown_watch_is_ignored() {
        let mut status = status_with(vec![watch("alice/app", "main")]);
        status.apply_event(WatchEvent::RunStarted(snap("other/repo", "main", 1)));
        assert!(status.watches[0].active_runs.is_empty());
    }

    #[test]
    fn run_conclusion_severity_ordering() {
        // Failures should be most severe (lowest number)
        assert!(RunConclusion::Failure.severity() < RunConclusion::Cancelled.severity());
        assert!(RunConclusion::Cancelled.severity() < RunConclusion::Success.severity());
        // All failure types share the same severity
        assert_eq!(
            RunConclusion::Failure.severity(),
            RunConclusion::TimedOut.severity()
        );
        assert_eq!(
            RunConclusion::Failure.severity(),
            RunConclusion::StartupFailure.severity()
        );
    }
}
