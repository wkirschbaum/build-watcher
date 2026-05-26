mod cli_client;
mod reqwest_client;

pub use cli_client::GhCliClient;
pub use reqwest_client::ReqwestClient;

use std::str::FromStr;
use std::time::Duration;

use serde::{Deserialize, Serialize};

pub(super) const GH_TIMEOUT: Duration = Duration::from_secs(30);
/// Default limit for `recent_runs` (per-branch).
pub(super) const DEFAULT_BRANCH_LIMIT: u32 = 10;
/// Upper limit for `in_progress_runs_for_repo`.
pub(super) const IN_PROGRESS_LIMIT: u32 = 100;
/// Default limit for `recent_runs_for_repo` (new-run detection).
pub const DEFAULT_REPO_LIMIT: u32 = 20;
/// Maximum open PRs to fetch per repo.
pub(super) const MAX_OPEN_PRS: &str = "50";

// ---------------------------------------------------------------------------
// RunStatus / RunConclusion — moved here from status.rs so github types are
// self-contained. status.rs re-exports these for backward compatibility.
// ---------------------------------------------------------------------------

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

impl std::fmt::Display for RunConclusion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl RunConclusion {
    pub fn as_str(&self) -> &'static str {
        match self {
            RunConclusion::Success => "success",
            RunConclusion::Failure => "failure",
            RunConclusion::Cancelled => "cancelled",
            RunConclusion::TimedOut => "timed_out",
            RunConclusion::StartupFailure => "startup_failure",
            RunConclusion::Unknown => "unknown",
        }
    }

    /// Severity ordering for display: lower = worse.
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

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum GhError {
    #[error("{repo}: request timed out after {timeout_secs}s")]
    Timeout { repo: String, timeout_secs: u64 },
    #[error("{repo}: failed to spawn gh CLI: {source}")]
    Spawn {
        repo: String,
        source: std::io::Error,
    },
    #[error("{repo}: repository not found or inaccessible")]
    NotFound { repo: String },
    #[error("{repo}: GitHub API error: {stderr}")]
    CliError { repo: String, stderr: String },
    #[error("{repo}: failed to parse API response: {source}")]
    Parse {
        repo: String,
        source: serde_json::Error,
    },
    #[error("{repo}: missing fields in API response")]
    MissingFields { repo: String },
}

impl GhError {
    /// Returns `true` if the error indicates the repository does not exist or
    /// is inaccessible (e.g. deleted, renamed, or private without access).
    pub fn is_repo_not_found(&self) -> bool {
        matches!(self, GhError::NotFound { .. })
    }
}

// ---------------------------------------------------------------------------
// Core data types
// ---------------------------------------------------------------------------

/// Summary of the last completed build, persisted across restarts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LastBuild {
    pub run_id: u64,
    pub conclusion: RunConclusion,
    pub workflow: String,
    pub title: String,
    #[serde(default)]
    pub head_sha: String,
    #[serde(default)]
    pub event: String,
    /// Failing step names from the build, if available.
    #[serde(default)]
    pub failing_steps: Option<String>,
    /// Database ID of the first failed job (for constructing job URLs).
    #[serde(default)]
    pub failing_job_id: Option<u64>,
    /// Unix timestamp (seconds) when this build completed.
    #[serde(default)]
    pub completed_at: Option<u64>,
    /// Duration in seconds from run start to completion.
    #[serde(default)]
    pub duration_secs: Option<u64>,
    /// GitHub Actions attempt number. 1 for the original run, 2+ for re-runs.
    #[serde(default = "default_attempt")]
    pub attempt: u32,
    /// GitHub Actions run URL.
    #[serde(default)]
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit_author: Option<String>,
    /// True when this build succeeded on a re-run after a prior failed attempt
    /// on the same commit. Computed by the tracker when flake detection is enabled.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub flaky: bool,
}

impl LastBuild {
    pub fn display_title(&self) -> String {
        display_title(&self.event, &self.title)
    }
}

/// Raw JSON shape returned by `gh run list/view --json ...`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct GhRunJson {
    pub(super) database_id: Option<u64>,
    #[serde(default)]
    pub(super) status: String,
    #[serde(default)]
    pub(super) conclusion: String,
    #[serde(default)]
    pub(super) display_title: String,
    #[serde(default)]
    pub(super) workflow_name: String,
    #[serde(default)]
    pub(super) head_sha: String,
    #[serde(default)]
    pub(super) event: String,
    #[serde(default)]
    pub(super) head_branch: String,
    #[serde(default = "default_attempt")]
    pub(super) attempt: u32,
    #[serde(default)]
    pub(super) created_at: String,
    #[serde(default)]
    pub(super) updated_at: String,
    #[serde(default)]
    pub(super) url: String,
}

/// Default GitHub Actions attempt number (1 = original run).
pub fn default_attempt() -> u32 {
    1
}

/// A GitHub Actions run parsed for internal use.
#[derive(Debug, Clone)]
pub struct RunInfo {
    pub id: u64,
    pub status: RunStatus,
    pub conclusion: String,
    pub title: String,
    pub workflow: String,
    pub head_sha: String,
    pub event: String,
    pub head_branch: String,
    pub attempt: u32,
    pub created_at: String,
    pub updated_at: String,
    pub url: String,
    /// GitHub login of the actor who triggered the run (populated by the REST client only).
    pub actor: Option<String>,
    /// Name of the commit author from the head commit (populated by the REST client only).
    pub commit_author: Option<String>,
}

impl RunInfo {
    pub(super) fn from_gh_json(raw: GhRunJson, repo: &str) -> Result<Self, GhError> {
        let id = raw.database_id.ok_or_else(|| GhError::MissingFields {
            repo: repo.to_string(),
        })?;
        Ok(Self {
            id,
            status: if raw.status.is_empty() {
                return Err(GhError::MissingFields {
                    repo: repo.to_string(),
                });
            } else {
                raw.status
                    .parse::<RunStatus>()
                    .unwrap_or(RunStatus::Unknown)
            },
            conclusion: raw.conclusion,
            title: if raw.display_title.is_empty() {
                return Err(GhError::MissingFields {
                    repo: repo.to_string(),
                });
            } else {
                raw.display_title
            },
            workflow: if raw.workflow_name.is_empty() {
                return Err(GhError::MissingFields {
                    repo: repo.to_string(),
                });
            } else {
                raw.workflow_name
            },
            head_sha: raw.head_sha,
            event: raw.event,
            head_branch: raw.head_branch,
            attempt: raw.attempt,
            created_at: raw.created_at,
            updated_at: raw.updated_at,
            url: raw.url,
            actor: None,
            commit_author: None,
        })
    }

    pub fn short_sha(&self) -> &str {
        short_sha(&self.head_sha)
    }

    pub fn display_title(&self) -> String {
        display_title(&self.event, &self.title)
    }

    pub fn is_completed(&self) -> bool {
        self.status == RunStatus::Completed
    }

    pub fn succeeded(&self) -> bool {
        self.conclusion == "success"
    }

    pub fn run_conclusion(&self) -> RunConclusion {
        self.conclusion.parse().unwrap_or(RunConclusion::Unknown)
    }

    pub fn duration_secs(&self) -> Option<u64> {
        let start = parse_iso_epoch(&self.created_at)?;
        let end = parse_iso_epoch(&self.updated_at)?;
        Some(end.saturating_sub(start))
    }

    pub fn elapsed_secs(&self, now_unix: u64) -> Option<f64> {
        let start = parse_iso_epoch(&self.created_at)?;
        Some(now_unix.saturating_sub(start) as f64)
    }

    pub fn to_last_build(&self) -> LastBuild {
        LastBuild {
            run_id: self.id,
            conclusion: self.run_conclusion(),
            workflow: self.workflow.clone(),
            title: self.title.clone(),
            head_sha: self.head_sha.clone(),
            event: self.event.clone(),
            failing_steps: None,
            failing_job_id: None,
            completed_at: parse_iso_epoch(&self.updated_at),
            duration_secs: self.duration_secs(),
            attempt: self.attempt,
            url: self.url.clone(),
            actor: self.actor.clone(),
            commit_author: self.commit_author.clone(),
            flaky: false,
        }
    }
}

// -- Pull request types --

/// Merge-readiness state from GitHub's `mergeStateStatus` field.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum MergeState {
    Clean,
    Blocked,
    Unstable,
    Behind,
    Dirty,
    HasHooks,
    #[default]
    #[serde(other)]
    Unknown,
}

impl MergeState {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Clean => "ready",
            Self::Blocked => "blocked",
            Self::Unstable => "unstable",
            Self::Behind => "behind",
            Self::Dirty => "conflict",
            Self::HasHooks => "hooks",
            Self::Unknown => "unknown",
        }
    }

    pub fn icon(&self) -> &'static str {
        match self {
            Self::Clean => "✓",
            Self::Blocked => "⊘",
            Self::Unstable => "!",
            Self::Behind => "↓",
            Self::Dirty => "✗",
            _ => "?",
        }
    }
}

impl std::fmt::Display for MergeState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

/// Raw JSON shape for a PR, shared by both CLI and REST clients.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct GhPrJson {
    pub(super) number: u64,
    #[serde(default)]
    pub(super) title: String,
    #[serde(default)]
    pub(super) head_ref_name: String,
    #[serde(default)]
    pub(super) base_ref_name: String,
    #[serde(default)]
    pub(super) url: String,
    #[serde(default)]
    pub(super) is_draft: bool,
    #[serde(default)]
    pub(super) merge_state_status: MergeState,
    #[serde(default)]
    pub(super) review_decision: Option<String>,
    pub(super) author: Option<GhAuthorJson>,
}

#[derive(Debug, Deserialize)]
pub(super) struct GhAuthorJson {
    #[serde(default)]
    pub(super) login: String,
}

/// A GitHub pull request parsed for internal use.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrInfo {
    pub number: u64,
    pub title: String,
    pub branch: String,
    pub target_branch: String,
    pub url: String,
    pub author: String,
    pub draft: bool,
    pub merge_state: MergeState,
    pub review_decision: Option<String>,
}

/// Summary of a GitHub repository, used for auto-discovery.
#[derive(Debug, Clone)]
pub struct RepoInfo {
    /// Full slug: `"owner/repo"`.
    pub full_name: String,
    /// Owner login (user or org).
    pub owner: String,
    /// Repository name without the owner prefix.
    pub name: String,
    /// ISO 8601 timestamp of the last push, if available.
    pub pushed_at: Option<String>,
}

/// Abstraction over the GitHub API.
#[async_trait::async_trait]
pub trait GitHubClient: Send + Sync + 'static {
    async fn recent_runs(&self, repo: &str, branch: &str) -> Result<Vec<RunInfo>, GhError>;
    async fn recent_runs_for_repo(&self, repo: &str, limit: u32) -> Result<Vec<RunInfo>, GhError>;
    async fn in_progress_runs_for_repo(&self, repo: &str) -> Result<Vec<RunInfo>, GhError>;
    async fn run_status(&self, repo: &str, run_id: u64) -> Result<RunInfo, GhError>;
    async fn failing_steps(&self, repo: &str, run_id: u64) -> Option<FailureInfo>;
    async fn run_rerun(
        &self,
        repo: &str,
        run_id: u64,
        failed_only: bool,
    ) -> Result<String, GhError>;
    async fn run_list_history(
        &self,
        repo: &str,
        branch: Option<&str>,
        limit: u32,
    ) -> Result<Vec<HistoryEntry>, GhError>;
    async fn rate_limit(&self) -> Result<RateLimit, GhError>;
    async fn list_tags(&self, repo: &str) -> Result<Vec<String>, GhError>;
    async fn list_branches(&self, repo: &str) -> Result<Vec<String>, GhError>;
    async fn default_branch(&self, repo: &str) -> Result<String, GhError>;
    async fn open_prs(&self, repo: &str) -> Result<Vec<PrInfo>, GhError>;
    async fn pr_merge(&self, repo: &str, number: u64) -> Result<String, GhError>;
    /// Fetch all repos accessible to the authenticated user (owned, org member, collaborator).
    /// Returns an empty vec when not supported (e.g. CLI fallback client).
    async fn list_accessible_repos(&self) -> Result<Vec<RepoInfo>, GhError>;
}

/// Author information fetched from the GitHub Actions run detail API.
#[derive(Debug, Clone)]
pub struct RunAuthorInfo {
    pub actor: String,
    pub commit_author: Option<String>,
}

// -- Shared job/step types (used by both CLI and REST clients) --

#[derive(Debug, Deserialize)]
pub(super) struct GhStep {
    pub(super) name: String,
    pub(super) conclusion: String,
}

#[derive(Debug, Deserialize)]
pub(super) struct GhJob {
    #[serde(default)]
    pub(super) database_id: Option<u64>,
    pub(super) name: String,
    pub(super) conclusion: String,
    pub(super) steps: Vec<GhStep>,
}

#[derive(Debug, Deserialize)]
pub(super) struct GhJobsResponse {
    pub(super) jobs: Vec<GhJob>,
}

/// Result of extracting failure info from a run's jobs.
#[derive(Debug)]
pub struct FailureInfo {
    pub steps: String,
    pub first_job_id: Option<u64>,
}

pub(super) fn extract_failing_steps(jobs: &[GhJob]) -> Option<FailureInfo> {
    let failed_jobs: Vec<&GhJob> = jobs
        .iter()
        .filter(|job| job.conclusion == "failure")
        .collect();

    if failed_jobs.is_empty() {
        return None;
    }

    let first_job_id = failed_jobs.first().and_then(|j| j.database_id);
    let steps: Vec<String> = failed_jobs
        .iter()
        .map(|job| {
            job.steps
                .iter()
                .find(|s| s.conclusion == "failure")
                .map_or_else(
                    || job.name.clone(),
                    |s| format!("{} / {}", job.name, s.name),
                )
        })
        .collect();

    Some(FailureInfo {
        steps: steps.join(", "),
        first_job_id,
    })
}

/// A build history entry with timestamps for duration/age calculation.
#[derive(Debug)]
pub struct HistoryEntry {
    pub id: u64,
    pub conclusion: String,
    pub workflow: String,
    pub title: String,
    pub branch: String,
    pub event: String,
    pub created_at: String,
    pub updated_at: String,
}

impl HistoryEntry {
    pub fn display_title(&self) -> String {
        display_title(&self.event, &self.title)
    }

    pub fn duration_secs(&self) -> Option<u64> {
        let start = parse_iso_epoch(&self.created_at)?;
        let end = parse_iso_epoch(&self.updated_at)?;
        Some(end.saturating_sub(start))
    }

    pub fn age_secs(&self, now: u64) -> Option<u64> {
        let start = parse_iso_epoch(&self.created_at)?;
        Some(now.saturating_sub(start))
    }
}

/// GitHub API rate limit info for the `core` resource.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RateLimit {
    pub limit: u64,
    pub remaining: u64,
    pub reset: u64,
    pub used: u64,
}

// ---------------------------------------------------------------------------
// Shared pure helpers
// ---------------------------------------------------------------------------

/// Truncates a hex SHA to 7 characters. Returns the full string if shorter.
pub fn short_sha(sha: &str) -> &str {
    sha.get(..7).unwrap_or(sha)
}

/// Format a human-readable title with a compact event prefix.
pub(crate) fn display_title(event: &str, title: &str) -> String {
    let prefix = match event {
        e if e.starts_with("pull_request") => "PR: ",
        "schedule" => "cron: ",
        "workflow_dispatch" => "manual: ",
        _ => "",
    };
    format!("{prefix}{title}")
}

/// Parse an ISO 8601 / RFC 3339 timestamp to Unix epoch seconds.
pub fn parse_iso_epoch(s: &str) -> Option<u64> {
    u64::try_from(chrono::DateTime::parse_from_rfc3339(s).ok()?.timestamp()).ok()
}

/// Seconds elapsed since an ISO 8601 timestamp, given the current Unix epoch.
pub fn elapsed_since(iso: &str, now_unix: u64) -> Option<f64> {
    let start = parse_iso_epoch(iso)?;
    Some(now_unix.saturating_sub(start) as f64)
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

/// Validates that a branch name contains only safe characters.
pub fn validate_branch(branch: &str) -> Result<(), String> {
    if branch.is_empty()
        || !branch
            .chars()
            .all(|c| c.is_alphanumeric() || "-_./".contains(c))
    {
        return Err(format!(
            "Invalid branch name: {branch:?} — expected alphanumeric, hyphen, underscore, dot, or slash characters"
        ));
    }
    Ok(())
}

/// Validates that a repo name contains only safe characters.
pub fn validate_repo(repo: &str) -> Result<(), String> {
    let parts: Vec<&str> = repo.split('/').collect();
    if parts.len() != 2
        || parts
            .iter()
            .any(|p| p.is_empty() || !p.chars().all(|c| c.is_alphanumeric() || "-_.".contains(c)))
    {
        return Err(format!(
            "Invalid repo format: {repo:?} — expected \"owner/repo\" with alphanumeric, hyphen, underscore, or dot characters"
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// GitHub URLs
// ---------------------------------------------------------------------------

/// Parse a GitHub `owner/repo` from a git remote URL (SSH or HTTPS).
pub fn parse_github_remote(url: &str) -> Result<String, String> {
    let url = url.trim();

    let path = if let Some(rest) = url.strip_prefix("git@github.com:") {
        rest
    } else if let Some(rest) = url
        .strip_prefix("https://github.com/")
        .or_else(|| url.strip_prefix("ssh://git@github.com/"))
    {
        rest
    } else {
        return Err(format!(
            "Not a GitHub remote URL: {url:?} — only github.com remotes are supported"
        ));
    };

    let path = path.strip_suffix(".git").unwrap_or(path);
    let path = path.strip_suffix('/').unwrap_or(path);

    validate_repo(path)
        .map_err(|_| format!("Could not extract owner/repo from remote URL: {url:?}"))?;

    Ok(path.to_string())
}

/// Detect the GitHub `owner/repo` from a local git repository's origin remote.
pub async fn repo_from_git_remote(path: &str) -> Result<String, GhError> {
    let output = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::process::Command::new("git")
            .args(["-C", path, "remote", "get-url", "origin"])
            .output(),
    )
    .await
    .map_err(|_| GhError::Timeout {
        repo: path.to_string(),
        timeout_secs: 5,
    })?
    .map_err(|e| GhError::Spawn {
        repo: path.to_string(),
        source: e,
    })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        return Err(GhError::CliError {
            repo: path.to_string(),
            stderr,
        });
    }

    let url = String::from_utf8_lossy(&output.stdout);
    parse_github_remote(&url).map_err(|msg| GhError::CliError {
        repo: path.to_string(),
        stderr: msg,
    })
}

pub fn run_url(repo: &str, run_id: u64) -> String {
    format!("https://github.com/{repo}/actions/runs/{run_id}")
}

pub fn job_url(repo: &str, run_id: u64, job_id: u64) -> String {
    format!("https://github.com/{repo}/actions/runs/{run_id}/job/{job_id}")
}

pub fn actions_url(repo: &str, branch: &str) -> String {
    format!("https://github.com/{repo}/actions?query=branch%3A{branch}")
}

pub fn repo_url(repo: &str) -> String {
    format!("https://github.com/{repo}")
}

// ---------------------------------------------------------------------------
// Token acquisition
// ---------------------------------------------------------------------------

/// Resolve a GitHub API token. Checks `GITHUB_TOKEN` env var first, then
/// falls back to `gh auth token`.
pub async fn gh_auth_token() -> Result<String, GhError> {
    if let Ok(token) = std::env::var("GITHUB_TOKEN") {
        let token = token.trim().to_string();
        if !token.is_empty() {
            tracing::info!("Using GITHUB_TOKEN environment variable");
            return Ok(token);
        }
    }

    let output = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::process::Command::new("gh")
            .args(["auth", "token"])
            .output(),
    )
    .await
    .map_err(|_| GhError::Timeout {
        repo: "auth".into(),
        timeout_secs: 5,
    })?;

    match output {
        Ok(output) if output.status.success() => {
            let token = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if token.is_empty() {
                Err(GhError::CliError {
                    repo: "auth".into(),
                    stderr: "gh auth token returned empty — run `gh auth login` or set GITHUB_TOKEN"
                        .into(),
                })
            } else {
                Ok(token)
            }
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
            Err(GhError::CliError {
                repo: "auth".into(),
                stderr: format!(
                    "{stderr}\nHint: run `gh auth login` or set the GITHUB_TOKEN environment variable"
                ),
            })
        }
        Err(_) => Err(GhError::CliError {
            repo: "auth".into(),
            stderr: "GitHub CLI (gh) not found. Install it from https://cli.github.com/ or set the GITHUB_TOKEN environment variable".into(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample_json() -> serde_json::Value {
        json!({
            "databaseId": 123456789,
            "status": "completed",
            "conclusion": "success",
            "displayTitle": "Fix login bug",
            "workflowName": "Lint and Test",
            "headSha": "abc1234def5678",
            "event": "push",
            "headBranch": "main",
            "createdAt": "2026-01-01T10:00:00Z",
            "updatedAt": "2026-01-01T10:05:30Z",
            "url": "https://github.com/test/repo/actions/runs/123456789"
        })
    }

    fn run_from_value(v: &serde_json::Value) -> Option<RunInfo> {
        let raw: GhRunJson = serde_json::from_value(v.clone()).ok()?;
        RunInfo::from_gh_json(raw, "test/repo").ok()
    }

    #[test]
    fn from_json_parses_all_fields() {
        let run = run_from_value(&sample_json()).unwrap();
        assert_eq!(run.id, 123456789);
        assert_eq!(run.status, RunStatus::Completed);
        assert_eq!(run.conclusion, "success");
        assert_eq!(run.title, "Fix login bug");
        assert_eq!(run.workflow, "Lint and Test");
        assert_eq!(run.head_sha, "abc1234def5678");
        assert_eq!(run.event, "push");
        assert_eq!(run.head_branch, "main");
    }

    #[test]
    fn from_json_returns_none_on_missing_id() {
        let v = json!({ "status": "completed" });
        assert!(run_from_value(&v).is_none());
    }

    #[test]
    fn short_sha_truncation() {
        assert_eq!(short_sha("abc1234def5678"), "abc1234");
        assert_eq!(short_sha("abc"), "abc");
        assert_eq!(short_sha(""), "");
    }

    #[test]
    fn run_info_status_helpers() {
        let run = run_from_value(&sample_json()).unwrap();
        assert!(run.is_completed());
        assert!(run.succeeded());
        assert_eq!(run.short_sha(), "abc1234");
        assert_eq!(
            run.url,
            "https://github.com/test/repo/actions/runs/123456789"
        );
        assert_eq!(run.duration_secs(), Some(330));

        let mut v = sample_json();
        v["status"] = json!("in_progress");
        v["conclusion"] = json!("failure");
        let run = run_from_value(&v).unwrap();
        assert!(!run.is_completed());
        assert!(!run.succeeded());
    }

    #[test]
    fn to_last_build_copies_fields() {
        let lb = run_from_value(&sample_json()).unwrap().to_last_build();
        assert_eq!(lb.run_id, 123456789);
        assert_eq!(lb.conclusion, RunConclusion::Success);
        assert_eq!(lb.workflow, "Lint and Test");
        assert_eq!(lb.title, "Fix login bug");
    }

    #[test]
    fn missing_required_fields_returns_none() {
        let v = json!({ "databaseId": 1 });
        assert!(run_from_value(&v).is_none());

        let v = json!({ "databaseId": 1, "status": "completed", "workflowName": "CI" });
        assert!(run_from_value(&v).is_none());
    }

    #[test]
    fn repo_validation() {
        assert!(validate_repo("alice/myapp").is_ok());
        assert!(validate_repo("my-org/my_repo.rs").is_ok());
        assert!(validate_repo("noslash").is_err());
        assert!(validate_repo("a/b/c").is_err());
        assert!(validate_repo("/repo").is_err());
        assert!(validate_repo("owner/").is_err());
        assert!(validate_repo("owner/repo name").is_err());
    }

    #[test]
    fn is_repo_not_found_detects_gh_errors() {
        assert!(
            GhError::NotFound {
                repo: "alice/gone".to_string(),
            }
            .is_repo_not_found()
        );

        assert!(
            !GhError::CliError {
                repo: "alice/app".to_string(),
                stderr: "HTTP 502: Bad Gateway".to_string(),
            }
            .is_repo_not_found()
        );

        assert!(
            !GhError::Timeout {
                repo: "alice/app".to_string(),
                timeout_secs: 30,
            }
            .is_repo_not_found()
        );
    }

    #[test]
    fn parse_github_remote_https() {
        assert_eq!(
            parse_github_remote("https://github.com/owner/repo.git").unwrap(),
            "owner/repo"
        );
        assert_eq!(
            parse_github_remote("https://github.com/owner/repo").unwrap(),
            "owner/repo"
        );
        assert_eq!(
            parse_github_remote("https://github.com/owner/repo/").unwrap(),
            "owner/repo"
        );
    }

    #[test]
    fn parse_github_remote_ssh() {
        assert_eq!(
            parse_github_remote("git@github.com:owner/repo.git").unwrap(),
            "owner/repo"
        );
        assert_eq!(
            parse_github_remote("git@github.com:owner/repo").unwrap(),
            "owner/repo"
        );
        assert_eq!(
            parse_github_remote("ssh://git@github.com/owner/repo.git").unwrap(),
            "owner/repo"
        );
    }

    #[test]
    fn parse_github_remote_with_trailing_newline() {
        assert_eq!(
            parse_github_remote("git@github.com:owner/repo.git\n").unwrap(),
            "owner/repo"
        );
    }

    #[test]
    fn parse_github_remote_non_github() {
        assert!(parse_github_remote("https://gitlab.com/owner/repo.git").is_err());
        assert!(parse_github_remote("git@bitbucket.org:owner/repo.git").is_err());
    }

    #[test]
    fn branch_validation() {
        assert!(validate_branch("main").is_ok());
        assert!(validate_branch("feature/my-branch").is_ok());
        assert!(validate_branch("release-1.0").is_ok());
        assert!(validate_branch("").is_err());
        assert!(validate_branch("branch name").is_err());
    }

    #[test]
    fn display_title_formatting() {
        let run = run_from_value(&sample_json()).unwrap();
        assert_eq!(run.display_title(), "Fix login bug");
        assert_eq!(run.to_last_build().display_title(), "Fix login bug");

        let cases = [
            ("pull_request", "PR: Fix login bug"),
            ("pull_request_target", "PR: Fix login bug"),
            ("schedule", "cron: Fix login bug"),
            ("workflow_dispatch", "manual: Fix login bug"),
            ("push", "Fix login bug"),
        ];
        for (event, expected) in cases {
            let mut v = sample_json();
            v["event"] = json!(event);
            assert_eq!(run_from_value(&v).unwrap().display_title(), expected);
        }
    }

    #[test]
    fn parse_iso_epoch_valid() {
        assert_eq!(
            parse_iso_epoch("2024-01-01T00:00:00Z").unwrap(),
            19723 * 86400
        );
        assert_eq!(
            parse_iso_epoch("2024-01-01T12:30:45Z"),
            parse_iso_epoch("2024-01-01T12:30:45.123Z")
        );
        let start = parse_iso_epoch("2024-01-01T10:00:00Z").unwrap();
        let end = parse_iso_epoch("2024-01-01T10:05:30Z").unwrap();
        assert_eq!(end - start, 330);
    }

    #[test]
    fn parse_iso_epoch_rejects_invalid() {
        assert!(parse_iso_epoch("").is_none());
        assert!(parse_iso_epoch("not-a-date").is_none());
        assert!(parse_iso_epoch("2024-01-01").is_none());
        assert!(parse_iso_epoch("2024-02-30T00:00:00Z").is_none());
        assert!(parse_iso_epoch("2023-02-29T00:00:00Z").is_none());
        assert!(parse_iso_epoch("2024-02-29T00:00:00Z").is_some());
        assert!(parse_iso_epoch("2024-01-01T24:00:00Z").is_none());
        assert!(parse_iso_epoch("2024-01-01T12:60:00Z").is_none());
    }

    fn make_history(event: &str, created: &str, updated: &str) -> HistoryEntry {
        HistoryEntry {
            id: 1,
            conclusion: "success".to_string(),
            workflow: "CI".to_string(),
            title: "Test".to_string(),
            branch: "main".to_string(),
            event: event.to_string(),
            created_at: created.to_string(),
            updated_at: updated.to_string(),
        }
    }

    #[test]
    fn history_entry_methods() {
        let entry = make_history("push", "2024-01-01T10:00:00Z", "2024-01-01T10:05:30Z");
        assert_eq!(entry.display_title(), "Test");
        assert_eq!(entry.duration_secs(), Some(330));

        let pr = make_history("pull_request", "", "");
        assert_eq!(pr.display_title(), "PR: Test");
        assert_eq!(pr.duration_secs(), None);

        let bad = make_history("push", "invalid", "2024-01-01T10:05:30Z");
        assert_eq!(bad.duration_secs(), None);
    }

    #[test]
    fn history_entry_age_secs() {
        let entry = make_history("push", "2024-01-01T10:00:00Z", "2024-01-01T10:05:30Z");
        let created_epoch = parse_iso_epoch("2024-01-01T10:00:00Z").unwrap();
        assert_eq!(entry.age_secs(created_epoch + 300), Some(300));
        assert_eq!(entry.age_secs(created_epoch - 100), Some(0));
        let bad = make_history("push", "invalid", "");
        assert_eq!(bad.age_secs(created_epoch), None);
    }

    fn job(name: &str, conclusion: &str, steps: Vec<(&str, &str)>) -> GhJob {
        GhJob {
            database_id: None,
            name: name.to_string(),
            conclusion: conclusion.to_string(),
            steps: steps
                .into_iter()
                .map(|(n, c)| GhStep {
                    name: n.to_string(),
                    conclusion: c.to_string(),
                })
                .collect(),
        }
    }

    #[test]
    fn extract_failing_steps_finds_failed_job_and_step() {
        let jobs = vec![
            job(
                "Build",
                "success",
                vec![("Checkout", "success"), ("Compile", "success")],
            ),
            job(
                "Test",
                "failure",
                vec![("Checkout", "success"), ("Run tests", "failure")],
            ),
        ];
        let info = extract_failing_steps(&jobs).unwrap();
        assert_eq!(info.steps, "Test / Run tests");
    }

    #[test]
    fn extract_failing_steps_job_failed_no_step() {
        let jobs = vec![job("Deploy", "failure", vec![("Setup", "success")])];
        let info = extract_failing_steps(&jobs).unwrap();
        assert_eq!(info.steps, "Deploy");
    }

    #[test]
    fn extract_failing_steps_multiple_failures() {
        let jobs = vec![
            job("Lint", "failure", vec![("Check", "failure")]),
            job("Test", "failure", vec![("Run", "failure")]),
        ];
        let info = extract_failing_steps(&jobs).unwrap();
        assert_eq!(info.steps, "Lint / Check, Test / Run");
    }

    #[test]
    fn extract_failing_steps_none_when_all_pass() {
        let jobs = vec![job("Build", "success", vec![("Compile", "success")])];
        assert!(extract_failing_steps(&jobs).is_none());
    }

    #[test]
    fn extract_failing_steps_empty_jobs() {
        assert!(extract_failing_steps(&[]).is_none());
    }

    #[test]
    fn pr_json_parses_all_fields() {
        let json = json!([{
            "number": 42,
            "title": "Fix login bug",
            "headRefName": "feat/login",
            "baseRefName": "main",
            "url": "https://github.com/alice/app/pull/42",
            "isDraft": false,
            "mergeStateStatus": "CLEAN",
            "reviewDecision": "APPROVED",
            "author": { "login": "alice" }
        }]);
        let raw: Vec<GhPrJson> = serde_json::from_value(json).unwrap();
        assert_eq!(raw.len(), 1);
        let pr = &raw[0];
        assert_eq!(pr.number, 42);
        assert_eq!(pr.merge_state_status, MergeState::Clean);
        assert_eq!(pr.head_ref_name, "feat/login");
        assert_eq!(pr.base_ref_name, "main");
        assert!(!pr.is_draft);
    }

    #[test]
    fn merge_state_deserializes_all_variants() {
        for (s, expected) in [
            ("CLEAN", MergeState::Clean),
            ("BLOCKED", MergeState::Blocked),
            ("UNSTABLE", MergeState::Unstable),
            ("BEHIND", MergeState::Behind),
            ("DIRTY", MergeState::Dirty),
            ("HAS_HOOKS", MergeState::HasHooks),
            ("SOMETHING_NEW", MergeState::Unknown),
        ] {
            let v: MergeState = serde_json::from_value(json!(s)).unwrap();
            assert_eq!(v, expected, "failed for {s}");
        }
    }

    #[test]
    fn merge_state_labels() {
        assert_eq!(MergeState::Clean.label(), "ready");
        assert_eq!(MergeState::Dirty.label(), "conflict");
        assert_eq!(MergeState::Unknown.label(), "unknown");
    }

    // -- RunConclusion / RunStatus --

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

    #[test]
    fn run_conclusion_severity_ordering() {
        assert!(RunConclusion::Failure.severity() < RunConclusion::Cancelled.severity());
        assert!(RunConclusion::Cancelled.severity() < RunConclusion::Success.severity());
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
