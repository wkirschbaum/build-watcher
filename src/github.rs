use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::status::RunStatus;

const GH_TIMEOUT: Duration = Duration::from_secs(30);
const GH_JSON_FIELDS: &str = "databaseId,status,conclusion,displayTitle,workflowName,headSha,event,headBranch,attempt,createdAt,updatedAt,url";
/// Default limit for `recent_runs` (per-branch).
const DEFAULT_BRANCH_LIMIT: u32 = 10;
/// Upper limit for `in_progress_runs_for_repo`.
const IN_PROGRESS_LIMIT: u32 = 100;
/// Default limit for `recent_runs_for_repo` (new-run detection).
pub const DEFAULT_REPO_LIMIT: u32 = 20;
/// Maximum open PRs to fetch per repo.
const MAX_OPEN_PRS: &str = "50";

/// Truncates a hex SHA to 7 characters. Returns the full string if shorter.
pub fn short_sha(sha: &str) -> &str {
    sha.get(..7).unwrap_or(sha)
}

/// Execute a `gh` CLI command with timeout. Returns raw stdout bytes on success.
async fn gh_exec(repo: &str, args: &[&str]) -> Result<Vec<u8>, GhError> {
    let output = tokio::time::timeout(
        GH_TIMEOUT,
        tokio::process::Command::new("gh").args(args).output(),
    )
    .await
    .map_err(|_| GhError::Timeout {
        repo: repo.to_string(),
        timeout_secs: GH_TIMEOUT.as_secs(),
    })?
    .map_err(|e| GhError::Spawn {
        repo: repo.to_string(),
        source: e,
    })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        return Err(GhError::CliError {
            repo: repo.to_string(),
            stderr,
        });
    }

    Ok(output.stdout)
}

#[derive(Debug, thiserror::Error)]
pub enum GhError {
    #[error("{repo}: gh timed out after {timeout_secs}s")]
    Timeout { repo: String, timeout_secs: u64 },
    #[error("{repo}: failed to run gh: {source}")]
    Spawn {
        repo: String,
        source: std::io::Error,
    },
    #[error("{repo}: gh error: {stderr}")]
    CliError { repo: String, stderr: String },
    #[error("{repo}: parse error: {source}")]
    Parse {
        repo: String,
        source: serde_json::Error,
    },
    #[error("{repo}: missing fields in response")]
    MissingFields { repo: String },
}

impl GhError {
    /// Returns `true` if the error indicates the repository does not exist or
    /// is inaccessible (e.g. deleted, renamed, or private without access).
    pub fn is_repo_not_found(&self) -> bool {
        if let GhError::CliError { stderr, .. } = self {
            stderr.contains("Could not resolve to a Repository") || stderr.contains("Not Found")
        } else {
            false
        }
    }
}

/// Summary of the last completed build, persisted across restarts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LastBuild {
    pub run_id: u64,
    pub conclusion: String,
    pub workflow: String,
    pub title: String,
    #[serde(default)]
    pub head_sha: String,
    #[serde(default)]
    pub event: String,
    /// Failing step names from the build, if available (e.g. "Build / Run tests").
    /// Populated when the run failed; `None` for successful builds or older persisted state.
    #[serde(default)]
    pub failing_steps: Option<String>,
    /// Database ID of the first failed job (for constructing job URLs).
    #[serde(default)]
    pub failing_job_id: Option<u64>,
    /// Unix timestamp (seconds) when this build completed. Persisted so age survives restarts.
    #[serde(default)]
    pub completed_at: Option<u64>,
    /// Duration in seconds from run start to completion. Only set for runs completed while the
    /// daemon was watching; `None` for already-completed runs detected on startup or mid-poll.
    #[serde(default)]
    pub duration_secs: Option<u64>,
    /// GitHub Actions attempt number. 1 for the original run, 2+ for re-runs.
    #[serde(default = "default_attempt")]
    pub attempt: u32,
    /// GitHub Actions run URL.
    #[serde(default)]
    pub url: String,
    /// GitHub login of the user who triggered this run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor: Option<String>,
    /// Name of the commit author (from head_commit.author.name).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit_author: Option<String>,
}

impl LastBuild {
    /// Human-friendly title: "PR: <title>" for `pull_request` events, else "<title> <sha>".
    pub fn display_title(&self) -> String {
        display_title(&self.event, &self.title)
    }
}

/// Raw JSON shape returned by `gh run list/view --json ...`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GhRunJson {
    database_id: Option<u64>,
    #[serde(default)]
    status: String,
    #[serde(default)]
    conclusion: String,
    #[serde(default)]
    display_title: String,
    #[serde(default)]
    workflow_name: String,
    #[serde(default)]
    head_sha: String,
    #[serde(default)]
    event: String,
    #[serde(default)]
    head_branch: String,
    #[serde(default = "default_attempt")]
    attempt: u32,
    #[serde(default)]
    created_at: String,
    #[serde(default)]
    updated_at: String,
    #[serde(default)]
    url: String,
}

/// Default GitHub Actions attempt number (1 = original run).
/// Used as a serde default across multiple structs.
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
}

impl RunInfo {
    fn from_gh_json(raw: GhRunJson, repo: &str) -> Result<Self, GhError> {
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
        })
    }

    pub fn short_sha(&self) -> &str {
        short_sha(&self.head_sha)
    }

    /// Human-friendly title: "PR: <title>" for `pull_request` events, else "<title> <sha>".
    pub fn display_title(&self) -> String {
        display_title(&self.event, &self.title)
    }

    pub fn is_completed(&self) -> bool {
        self.status == RunStatus::Completed
    }

    pub fn succeeded(&self) -> bool {
        self.conclusion == "success"
    }

    /// Parse the conclusion string into a typed `RunConclusion`.
    pub fn run_conclusion(&self) -> crate::status::RunConclusion {
        self.conclusion
            .parse::<crate::status::RunConclusion>()
            .unwrap_or(crate::status::RunConclusion::Unknown)
    }

    /// Duration in seconds from `created_at` to `updated_at`.
    pub fn duration_secs(&self) -> Option<u64> {
        let start = parse_iso_epoch(&self.created_at)?;
        let end = parse_iso_epoch(&self.updated_at)?;
        Some(end.saturating_sub(start))
    }

    /// Seconds since `created_at`, given the current Unix epoch.
    pub fn elapsed_secs(&self, now_unix: u64) -> Option<f64> {
        let start = parse_iso_epoch(&self.created_at)?;
        Some(now_unix.saturating_sub(start) as f64)
    }

    pub fn to_last_build(&self) -> LastBuild {
        LastBuild {
            run_id: self.id,
            conclusion: self.conclusion.clone(),
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
            actor: None,
            commit_author: None,
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

/// Raw JSON shape for a PR from `gh pr list`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GhPrJson {
    number: u64,
    #[serde(default)]
    title: String,
    #[serde(default)]
    head_ref_name: String,
    #[serde(default)]
    base_ref_name: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    is_draft: bool,
    #[serde(default)]
    merge_state_status: MergeState,
    #[serde(default)]
    review_decision: String,
    author: Option<GhAuthorJson>,
}

#[derive(Debug, Deserialize)]
struct GhAuthorJson {
    #[serde(default)]
    login: String,
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
    pub review_decision: String,
}

const GH_PR_FIELDS: &str =
    "number,title,headRefName,baseRefName,url,isDraft,mergeStateStatus,reviewDecision,author";

/// Abstraction over the GitHub API. The real implementation (`GhCliClient`) calls
/// the `gh` CLI; tests can inject a mock.
#[async_trait::async_trait]
pub trait GitHubClient: Send + Sync + 'static {
    async fn recent_runs(&self, repo: &str, branch: &str) -> Result<Vec<RunInfo>, GhError>;
    /// Fetch recent runs across all branches for a repo (no `--branch` filter).
    async fn recent_runs_for_repo(&self, repo: &str, limit: u32) -> Result<Vec<RunInfo>, GhError>;
    /// Fetch all in-progress runs for a repo (no branch filter, `--status in_progress`).
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
    /// Fetch tag names for a repo (used to exclude tags from branch discovery).
    async fn list_tags(&self, repo: &str) -> Result<Vec<String>, GhError>;
    /// Fetch branch names for a repo (used to prune deleted branches from discovery).
    async fn list_branches(&self, repo: &str) -> Result<Vec<String>, GhError>;
    /// Fetch the default branch name for a repo (e.g. "main" or "master").
    async fn default_branch(&self, repo: &str) -> Result<String, GhError>;
    /// Fetch open PRs for a repo.
    async fn open_prs(&self, repo: &str) -> Result<Vec<PrInfo>, GhError>;
    /// Merge a PR by number.
    async fn pr_merge(&self, repo: &str, number: u64) -> Result<String, GhError>;
    /// Fetch the triggering actor and commit author for a run.
    /// Returns `None` on any error (graceful degradation).
    async fn run_author(&self, repo: &str, run_id: u64) -> Option<RunAuthorInfo>;
}

/// Author information fetched from the GitHub Actions run detail API.
#[derive(Debug, Clone)]
pub struct RunAuthorInfo {
    /// GitHub login of the user who triggered this run (pushed, re-ran, etc.).
    pub actor: String,
    /// Name of the commit author (from `head_commit.author.name`).
    pub commit_author: Option<String>,
}

/// Real GitHub client that shells out to the `gh` CLI.
pub struct GhCliClient;

/// Shared helper for `gh run list` with variable filters.
/// Parses the JSON response into `Vec<RunInfo>`, skipping entries with missing fields.
async fn gh_run_list(repo: &str, limit: u32, extra_args: &[&str]) -> Result<Vec<RunInfo>, GhError> {
    let limit_str = limit.to_string();
    let mut args = vec![
        "run",
        "list",
        "--repo",
        repo,
        "--limit",
        &limit_str,
        "--json",
        GH_JSON_FIELDS,
    ];
    args.extend_from_slice(extra_args);
    let stdout = gh_exec(repo, &args).await?;
    let raw: Vec<GhRunJson> = serde_json::from_slice(&stdout).map_err(|e| GhError::Parse {
        repo: repo.to_string(),
        source: e,
    })?;
    Ok(raw
        .into_iter()
        .filter_map(|r| RunInfo::from_gh_json(r, repo).ok())
        .collect())
}

#[async_trait::async_trait]
impl GitHubClient for GhCliClient {
    #[tracing::instrument(skip_all, fields(%repo, %branch))]
    async fn recent_runs(&self, repo: &str, branch: &str) -> Result<Vec<RunInfo>, GhError> {
        gh_run_list(repo, DEFAULT_BRANCH_LIMIT, &["--branch", branch]).await
    }

    #[tracing::instrument(skip_all, fields(%repo, %limit))]
    async fn recent_runs_for_repo(&self, repo: &str, limit: u32) -> Result<Vec<RunInfo>, GhError> {
        gh_run_list(repo, limit, &[]).await
    }

    #[tracing::instrument(skip_all, fields(%repo))]
    async fn in_progress_runs_for_repo(&self, repo: &str) -> Result<Vec<RunInfo>, GhError> {
        gh_run_list(repo, IN_PROGRESS_LIMIT, &["--status", "in_progress"]).await
    }

    #[tracing::instrument(skip_all, fields(%repo, %run_id))]
    async fn run_status(&self, repo: &str, run_id: u64) -> Result<RunInfo, GhError> {
        let id_str = run_id.to_string();
        let stdout = gh_exec(
            repo,
            &[
                "run",
                "view",
                &id_str,
                "--repo",
                repo,
                "--json",
                GH_JSON_FIELDS,
            ],
        )
        .await?;

        let raw: GhRunJson = serde_json::from_slice(&stdout).map_err(|e| GhError::Parse {
            repo: repo.to_string(),
            source: e,
        })?;

        RunInfo::from_gh_json(raw, repo)
    }

    async fn failing_steps(&self, repo: &str, run_id: u64) -> Option<FailureInfo> {
        let id_str = run_id.to_string();
        let stdout = match gh_exec(
            repo,
            &["run", "view", &id_str, "--repo", repo, "--json", "jobs"],
        )
        .await
        {
            Ok(s) => s,
            Err(e) => {
                tracing::debug!(%repo, %run_id, error = %e, "Failed to fetch failing steps");
                return None;
            }
        };

        match serde_json::from_slice::<GhJobsResponse>(&stdout) {
            Ok(resp) => extract_failing_steps(&resp.jobs),
            Err(e) => {
                tracing::debug!(%repo, %run_id, error = %e, "Failed to parse jobs response");
                None
            }
        }
    }

    async fn run_rerun(
        &self,
        repo: &str,
        run_id: u64,
        failed_only: bool,
    ) -> Result<String, GhError> {
        let id_str = run_id.to_string();
        let mut args = vec!["run", "rerun", &id_str, "--repo", repo];
        if failed_only {
            args.push("--failed");
        }
        let stdout = gh_exec(repo, &args).await?;
        Ok(String::from_utf8_lossy(&stdout).to_string())
    }

    async fn run_list_history(
        &self,
        repo: &str,
        branch: Option<&str>,
        limit: u32,
    ) -> Result<Vec<HistoryEntry>, GhError> {
        gh_run_list_history_impl(repo, branch, limit).await
    }

    async fn rate_limit(&self) -> Result<RateLimit, GhError> {
        let stdout = gh_exec(
            "rate_limit",
            &["api", "rate_limit", "--jq", ".resources.core"],
        )
        .await?;
        serde_json::from_slice(&stdout).map_err(|e| GhError::Parse {
            repo: "rate_limit".into(),
            source: e,
        })
    }

    async fn list_tags(&self, repo: &str) -> Result<Vec<String>, GhError> {
        let stdout = gh_exec(
            repo,
            &[
                "api",
                &format!("repos/{repo}/tags"),
                "--jq",
                ".[].name",
                "--paginate",
            ],
        )
        .await?;
        let text = String::from_utf8_lossy(&stdout);
        Ok(text
            .lines()
            .filter(|l| !l.is_empty())
            .map(|l| l.to_string())
            .collect())
    }

    #[tracing::instrument(skip_all, fields(%repo))]
    async fn list_branches(&self, repo: &str) -> Result<Vec<String>, GhError> {
        let stdout = gh_exec(
            repo,
            &[
                "api",
                &format!("repos/{repo}/branches"),
                "--jq",
                ".[].name",
                "--paginate",
            ],
        )
        .await?;
        let text = String::from_utf8_lossy(&stdout);
        Ok(text
            .lines()
            .filter(|l| !l.is_empty())
            .map(|l| l.to_string())
            .collect())
    }

    #[tracing::instrument(skip_all, fields(%repo))]
    async fn default_branch(&self, repo: &str) -> Result<String, GhError> {
        let stdout = gh_exec(
            repo,
            &[
                "repo",
                "view",
                repo,
                "--json",
                "defaultBranchRef",
                "--jq",
                ".defaultBranchRef.name",
            ],
        )
        .await?;
        let name = String::from_utf8_lossy(&stdout).trim().to_string();
        if name.is_empty() {
            Err(GhError::MissingFields {
                repo: repo.to_string(),
            })
        } else {
            Ok(name)
        }
    }

    #[tracing::instrument(skip_all, fields(%repo))]
    async fn open_prs(&self, repo: &str) -> Result<Vec<PrInfo>, GhError> {
        let stdout = gh_exec(
            repo,
            &[
                "pr",
                "list",
                "--repo",
                repo,
                "--state",
                "open",
                "--limit",
                MAX_OPEN_PRS,
                "--json",
                GH_PR_FIELDS,
            ],
        )
        .await?;
        let raw: Vec<GhPrJson> = serde_json::from_slice(&stdout).map_err(|e| GhError::Parse {
            repo: repo.to_string(),
            source: e,
        })?;
        Ok(raw
            .into_iter()
            .map(|pr| PrInfo {
                number: pr.number,
                title: pr.title,
                branch: pr.head_ref_name,
                target_branch: pr.base_ref_name,
                url: pr.url,
                author: pr.author.map(|a| a.login).unwrap_or_default(),
                draft: pr.is_draft,
                merge_state: pr.merge_state_status,
                review_decision: pr.review_decision,
            })
            .collect())
    }

    async fn pr_merge(&self, repo: &str, number: u64) -> Result<String, GhError> {
        let number_str = number.to_string();
        let stdout = gh_exec(
            repo,
            &["pr", "merge", &number_str, "--repo", repo, "--merge"],
        )
        .await?;
        Ok(String::from_utf8_lossy(&stdout).trim().to_string())
    }

    async fn run_author(&self, repo: &str, run_id: u64) -> Option<RunAuthorInfo> {
        let jq = ".triggering_actor.login as $actor | .head_commit.author.name as $author | {actor: $actor, commit_author: $author}";
        let endpoint = format!("repos/{repo}/actions/runs/{run_id}");
        let stdout = match gh_exec(repo, &["api", &endpoint, "--jq", jq]).await {
            Ok(s) => s,
            Err(e) => {
                tracing::debug!(%repo, %run_id, error = %e, "Failed to fetch run author");
                return None;
            }
        };

        #[derive(Deserialize)]
        struct AuthorResponse {
            actor: Option<String>,
            commit_author: Option<String>,
        }

        match serde_json::from_slice::<AuthorResponse>(&stdout) {
            Ok(resp) => {
                let actor = resp.actor.filter(|s| !s.is_empty())?;
                Some(RunAuthorInfo {
                    actor,
                    commit_author: resp.commit_author.filter(|s| !s.is_empty()),
                })
            }
            Err(e) => {
                tracing::debug!(%repo, %run_id, error = %e, "Failed to parse run author");
                None
            }
        }
    }
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

#[derive(Debug, Deserialize)]
struct GhStep {
    name: String,
    conclusion: String,
}

#[derive(Debug, Deserialize)]
struct GhJob {
    #[serde(default)]
    database_id: Option<u64>,
    name: String,
    conclusion: String,
    steps: Vec<GhStep>,
}

#[derive(Debug, Deserialize)]
struct GhJobsResponse {
    jobs: Vec<GhJob>,
}

/// Result of extracting failure info from a run's jobs.
#[derive(Debug)]
pub struct FailureInfo {
    /// Comma-separated list of "job / step" names that failed.
    pub steps: String,
    /// Database ID of the first failed job (for constructing job URLs).
    pub first_job_id: Option<u64>,
}

/// Pure extraction of failing job/step names from parsed GitHub API response.
fn extract_failing_steps(jobs: &[GhJob]) -> Option<FailureInfo> {
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

    /// Duration as `updated_at - created_at`, parsed from ISO 8601 timestamps.
    pub fn duration_secs(&self) -> Option<u64> {
        let start = parse_iso_epoch(&self.created_at)?;
        let end = parse_iso_epoch(&self.updated_at)?;
        Some(end.saturating_sub(start))
    }

    /// Seconds since `created_at`, given the current Unix epoch.
    pub fn age_secs(&self, now: u64) -> Option<u64> {
        let start = parse_iso_epoch(&self.created_at)?;
        Some(now.saturating_sub(start))
    }
}

/// Seconds elapsed since an ISO 8601 timestamp, given the current Unix epoch.
pub fn elapsed_since(iso: &str, now_unix: u64) -> Option<f64> {
    let start = parse_iso_epoch(iso)?;
    Some(now_unix.saturating_sub(start) as f64)
}

/// Parse an ISO 8601 / RFC 3339 timestamp (e.g. `"2026-03-24T10:30:00Z"`) to Unix epoch seconds.
fn parse_iso_epoch(s: &str) -> Option<u64> {
    u64::try_from(chrono::DateTime::parse_from_rfc3339(s).ok()?.timestamp()).ok()
}

const GH_HISTORY_FIELDS: &str =
    "databaseId,conclusion,displayTitle,workflowName,headBranch,event,createdAt,updatedAt";

/// Raw JSON shape for history entries (superset of `GhRunJson` with timestamps).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GhHistoryJson {
    database_id: Option<u64>,
    #[serde(default)]
    conclusion: String,
    #[serde(default)]
    display_title: String,
    #[serde(default)]
    workflow_name: String,
    #[serde(default)]
    head_branch: String,
    #[serde(default)]
    event: String,
    #[serde(default)]
    created_at: String,
    #[serde(default)]
    updated_at: String,
}

/// Fetch recent build history for a repo, optionally filtered by branch.
async fn gh_run_list_history_impl(
    repo: &str,
    branch: Option<&str>,
    limit: u32,
) -> Result<Vec<HistoryEntry>, GhError> {
    let limit_str = limit.to_string();
    let mut args = vec![
        "run",
        "list",
        "--repo",
        repo,
        "--limit",
        &limit_str,
        "--json",
        GH_HISTORY_FIELDS,
    ];
    if let Some(b) = branch {
        args.push("--branch");
        args.push(b);
    }

    let stdout = gh_exec(repo, &args).await?;
    let raw: Vec<GhHistoryJson> = serde_json::from_slice(&stdout).map_err(|e| GhError::Parse {
        repo: repo.to_string(),
        source: e,
    })?;

    Ok(raw
        .into_iter()
        .filter_map(|r| {
            Some(HistoryEntry {
                id: r.database_id?,
                conclusion: if r.conclusion.is_empty() {
                    "in_progress".to_string()
                } else {
                    r.conclusion
                },
                workflow: r.workflow_name,
                title: r.display_title,
                branch: r.head_branch,
                event: r.event,
                created_at: r.created_at,
                updated_at: r.updated_at,
            })
        })
        .collect())
}

/// GitHub API rate limit info for the `core` resource.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RateLimit {
    pub limit: u64,
    pub remaining: u64,
    pub reset: u64, // unix timestamp
    pub used: u64,
}

/// Validates that a branch name contains only safe characters.
/// Notably rejects `#` which is used as the key delimiter in watch keys (`repo#branch`).
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
/// Notably rejects `#` which is used as the key delimiter in watch keys (`repo#branch`).
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

// -- GitHub URLs --

/// Parse a GitHub `owner/repo` from a git remote URL (SSH or HTTPS).
///
/// Supports:
/// - `https://github.com/owner/repo.git`
/// - `https://github.com/owner/repo`
/// - `git@github.com:owner/repo.git`
/// - `ssh://git@github.com/owner/repo.git`
///
/// Returns an error for non-GitHub URLs.
pub fn parse_github_remote(url: &str) -> Result<String, String> {
    let url = url.trim();

    // SSH shorthand: git@github.com:owner/repo.git
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
    // Strip any trailing slash
    let path = path.strip_suffix('/').unwrap_or(path);

    validate_repo(path)
        .map_err(|_| format!("Could not extract owner/repo from remote URL: {url:?}"))?;

    Ok(path.to_string())
}

/// Detect the GitHub `owner/repo` from a local git repository's origin remote.
///
/// Runs `git -C <path> remote get-url origin` and parses the result.
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

/// URL for a specific workflow run.
pub fn run_url(repo: &str, run_id: u64) -> String {
    format!("https://github.com/{repo}/actions/runs/{run_id}")
}

/// URL for a specific job within a workflow run.
pub fn job_url(repo: &str, run_id: u64, job_id: u64) -> String {
    format!("https://github.com/{repo}/actions/runs/{run_id}/job/{job_id}")
}

/// URL for the Actions tab of a repository, optionally filtered by branch.
pub fn actions_url(repo: &str, branch: &str) -> String {
    format!("https://github.com/{repo}/actions?query=branch%3A{branch}",)
}

/// URL for a repository.
pub fn repo_url(repo: &str) -> String {
    format!("https://github.com/{repo}")
}

// ---------------------------------------------------------------------------
// ReqwestClient — direct HTTP GitHub client (no `gh` CLI process spawning)
// ---------------------------------------------------------------------------

const GITHUB_API_BASE: &str = "https://api.github.com";

/// Get a GitHub OAuth token from the `gh` CLI (one-time call at startup).
pub async fn gh_auth_token() -> Result<String, GhError> {
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
    })?
    .map_err(|e| GhError::Spawn {
        repo: "auth".into(),
        source: e,
    })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        return Err(GhError::CliError {
            repo: "auth".into(),
            stderr,
        });
    }

    let token = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if token.is_empty() {
        return Err(GhError::CliError {
            repo: "auth".into(),
            stderr: "gh auth token returned empty".into(),
        });
    }
    Ok(token)
}

/// Result of `cached_get_inner` — either the response body or a 401 signal.
enum CachedGetResult {
    Body(Vec<u8>),
    Unauthorized,
}

/// Cached HTTP response for ETag-based conditional requests.
struct CachedResponse {
    etag: String,
    body: Vec<u8>,
}

/// GitHub client using direct HTTP via `reqwest`. Avoids the process-spawn
/// overhead of the `gh` CLI, reuses connections via HTTP keep-alive, and
/// supports ETag-based conditional requests (304 responses don't count
/// against the GitHub rate limit).
///
/// On `401 Unauthorized`, the client automatically re-acquires the token
/// via `gh auth token` and retries the request once.
pub struct ReqwestClient {
    client: reqwest::Client,
    token: Mutex<String>,
    /// ETag cache: URL → (etag, response body). Protected by a std::sync::Mutex
    /// since it's only held briefly (never across await points).
    cache: Mutex<HashMap<String, CachedResponse>>,
}

impl ReqwestClient {
    pub fn new(token: String) -> Self {
        let client = reqwest::Client::builder()
            .timeout(GH_TIMEOUT)
            .user_agent("build-watcher")
            .build()
            .expect("failed to build reqwest client");
        Self {
            client,
            token: Mutex::new(token),
            cache: Mutex::new(HashMap::new()),
        }
    }

    fn token(&self) -> String {
        self.token.lock().unwrap().clone()
    }

    /// Try to refresh the token via `gh auth token`. Returns `true` if
    /// a new (different) token was obtained.
    async fn refresh_token(&self) -> bool {
        match gh_auth_token().await {
            Ok(new_token) => {
                let mut current = self.token.lock().unwrap();
                if *current != new_token {
                    tracing::info!("GitHub token refreshed");
                    *current = new_token;
                    true
                } else {
                    false
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "Failed to refresh GitHub token");
                false
            }
        }
    }

    fn get(&self, url: &str) -> reqwest::RequestBuilder {
        self.client
            .get(url)
            .bearer_auth(self.token())
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
    }

    fn post_json(&self, url: &str, body: &impl Serialize) -> reqwest::RequestBuilder {
        self.client
            .post(url)
            .bearer_auth(self.token())
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .json(body)
    }

    fn put_json(&self, url: &str, body: &impl Serialize) -> reqwest::RequestBuilder {
        self.client
            .put(url)
            .bearer_auth(self.token())
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .json(body)
    }

    /// Core GET with ETag-based conditional requests.
    /// Returns raw response bytes, using cached body on 304.
    /// On 401, refreshes the token and retries once.
    async fn cached_get(&self, url: &str, repo: &str) -> Result<Vec<u8>, GhError> {
        let resp = self.cached_get_inner(url, repo).await?;
        match resp {
            CachedGetResult::Body(body) => Ok(body),
            CachedGetResult::Unauthorized => {
                // Token may have expired — try to refresh and retry once.
                if self.refresh_token().await {
                    // Clear ETag cache — stale tokens may have produced cached 401s.
                    self.cache.lock().unwrap().clear();
                    match self.cached_get_inner(url, repo).await? {
                        CachedGetResult::Body(body) => Ok(body),
                        CachedGetResult::Unauthorized => Err(GhError::CliError {
                            repo: repo.into(),
                            stderr: "HTTP 401: Unauthorized (after token refresh)".into(),
                        }),
                    }
                } else {
                    Err(GhError::CliError {
                        repo: repo.into(),
                        stderr: "HTTP 401: Unauthorized (token refresh failed)".into(),
                    })
                }
            }
        }
    }

    async fn cached_get_inner(&self, url: &str, repo: &str) -> Result<CachedGetResult, GhError> {
        let mut builder = self.get(url);

        // Attach If-None-Match if we have a cached ETag for this URL.
        let has_cache = {
            let cache = self.cache.lock().unwrap();
            if let Some(cached) = cache.get(url) {
                builder = builder.header("If-None-Match", &cached.etag);
                true
            } else {
                false
            }
        };

        let resp = builder.send().await.map_err(|e| GhError::CliError {
            repo: repo.into(),
            stderr: e.to_string(),
        })?;

        if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            return Ok(CachedGetResult::Unauthorized);
        }

        if resp.status() == reqwest::StatusCode::NOT_MODIFIED {
            let cache = self.cache.lock().unwrap();
            if let Some(cached) = cache.get(url) {
                tracing::trace!(url, "ETag cache hit (304)");
                return Ok(CachedGetResult::Body(cached.body.clone()));
            }
            if has_cache {
                tracing::warn!(url, "304 but cached body missing");
            }
            return Err(GhError::CliError {
                repo: repo.into(),
                stderr: "304 Not Modified but no cached body".into(),
            });
        }

        if !resp.status().is_success() {
            return Err(response_error(resp, repo).await);
        }

        let etag = resp
            .headers()
            .get("etag")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());

        let body = resp
            .bytes()
            .await
            .map_err(|e| GhError::CliError {
                repo: repo.into(),
                stderr: e.to_string(),
            })?
            .to_vec();

        if let Some(etag) = etag {
            let mut cache = self.cache.lock().unwrap();
            cache.insert(
                url.to_string(),
                CachedResponse {
                    etag,
                    body: body.clone(),
                },
            );
        }

        Ok(CachedGetResult::Body(body))
    }

    /// GET a JSON endpoint with ETag caching.
    async fn api_get<T: serde::de::DeserializeOwned>(
        &self,
        repo: &str,
        path: &str,
    ) -> Result<T, GhError> {
        let url = format!("{GITHUB_API_BASE}{path}");
        let bytes = self.cached_get(&url, repo).await?;
        serde_json::from_slice(&bytes).map_err(|e| GhError::Parse {
            repo: repo.into(),
            source: e,
        })
    }

    /// GET with query parameters and ETag caching.
    async fn api_get_query<T: serde::de::DeserializeOwned>(
        &self,
        repo: &str,
        path: &str,
        query: &[(&str, &str)],
    ) -> Result<T, GhError> {
        let base = format!("{GITHUB_API_BASE}{path}");
        let url = reqwest::Url::parse_with_params(&base, query).map_err(|e| GhError::CliError {
            repo: repo.into(),
            stderr: e.to_string(),
        })?;
        let bytes = self.cached_get(url.as_str(), repo).await?;
        serde_json::from_slice(&bytes).map_err(|e| GhError::Parse {
            repo: repo.into(),
            source: e,
        })
    }

    /// Paginated GET that collects all pages of `Vec<T>`.
    /// Pagination does not use ETag caching (each page has its own ETag).
    async fn api_get_all_pages<T: serde::de::DeserializeOwned>(
        &self,
        repo: &str,
        path: &str,
    ) -> Result<Vec<T>, GhError> {
        let mut items = Vec::new();
        let mut url = Some(format!("{GITHUB_API_BASE}{path}?per_page=100"));

        while let Some(current_url) = url {
            let resp = self.send_with_retry(|s| s.get(&current_url), repo).await?;
            url = next_link_url(&resp);
            let page: Vec<T> = handle_response(resp, repo).await?;
            items.extend(page);
        }

        Ok(items)
    }

    /// Fetch workflow runs with optional filters, returning parsed `RunInfo`s.
    async fn list_runs(
        &self,
        repo: &str,
        limit: u32,
        extra: &[(&str, &str)],
    ) -> Result<Vec<RunInfo>, GhError> {
        let limit_str = limit.to_string();
        let mut query: Vec<(&str, &str)> = vec![("per_page", &limit_str)];
        query.extend_from_slice(extra);
        let resp: RestRunsResponse = self
            .api_get_query(repo, &format!("/repos/{repo}/actions/runs"), &query)
            .await?;
        Ok(resp
            .workflow_runs
            .into_iter()
            .filter_map(|r| r.into_run_info(repo).ok())
            .collect())
    }

    /// Send a request, retrying once on 401 after refreshing the token.
    async fn send_with_retry(
        &self,
        build_request: impl Fn(&Self) -> reqwest::RequestBuilder,
        repo: &str,
    ) -> Result<reqwest::Response, GhError> {
        let resp = build_request(self)
            .send()
            .await
            .map_err(|e| GhError::CliError {
                repo: repo.into(),
                stderr: e.to_string(),
            })?;
        if resp.status() == reqwest::StatusCode::UNAUTHORIZED && self.refresh_token().await {
            build_request(self)
                .send()
                .await
                .map_err(|e| GhError::CliError {
                    repo: repo.into(),
                    stderr: e.to_string(),
                })
        } else {
            Ok(resp)
        }
    }

    /// POST (empty body) to an endpoint and return the response text.
    async fn api_post_empty(&self, repo: &str, path: &str) -> Result<String, GhError> {
        let url = format!("{GITHUB_API_BASE}{path}");
        let resp = self
            .send_with_retry(
                |s| {
                    s.client
                        .post(&url)
                        .bearer_auth(s.token())
                        .header("Accept", "application/vnd.github+json")
                        .header("X-GitHub-Api-Version", "2022-11-28")
                },
                repo,
            )
            .await?;
        if !resp.status().is_success() {
            return Err(response_error(resp, repo).await);
        }
        resp.text().await.map_err(|e| GhError::CliError {
            repo: repo.into(),
            stderr: e.to_string(),
        })
    }

    /// Execute a GraphQL query and return raw JSON bytes.
    async fn graphql_query(&self, repo: &str, query: &str) -> Result<serde_json::Value, GhError> {
        let url = format!("{GITHUB_API_BASE}/graphql");
        let body = serde_json::json!({ "query": query });
        let resp = self
            .send_with_retry(|s| s.post_json(&url, &body), repo)
            .await?;
        handle_response(resp, repo).await
    }
}

/// Parse the Link header for the `rel="next"` URL.
fn next_link_url(resp: &reqwest::Response) -> Option<String> {
    let link = resp.headers().get("link")?.to_str().ok()?;
    for part in link.split(',') {
        let part = part.trim();
        if part.contains("rel=\"next\"") {
            let url = part.split('>').next()?.trim_start_matches('<');
            return Some(url.to_string());
        }
    }
    None
}

/// Convert a non-success response into a `GhError`.
async fn response_error(resp: reqwest::Response, repo: &str) -> GhError {
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    GhError::CliError {
        repo: repo.into(),
        stderr: if status.as_u16() == 404 {
            format!("HTTP 404: Not Found - {body}")
        } else {
            format!("HTTP {status}: {body}")
        },
    }
}

/// Deserialize a successful response or return a `GhError`.
async fn handle_response<T: serde::de::DeserializeOwned>(
    resp: reqwest::Response,
    repo: &str,
) -> Result<T, GhError> {
    if !resp.status().is_success() {
        return Err(response_error(resp, repo).await);
    }
    let bytes = resp.bytes().await.map_err(|e| GhError::CliError {
        repo: repo.into(),
        stderr: e.to_string(),
    })?;
    serde_json::from_slice(&bytes).map_err(|e| GhError::Parse {
        repo: repo.into(),
        source: e,
    })
}

// -- REST API response types --

#[derive(Debug, Deserialize)]
struct RestRunsResponse {
    #[serde(default)]
    workflow_runs: Vec<RestRunJson>,
}

/// Workflow run from the GitHub REST API.
#[derive(Debug, Deserialize)]
struct RestRunJson {
    id: Option<u64>,
    #[serde(default)]
    status: String,
    conclusion: Option<String>,
    #[serde(default)]
    display_title: String,
    /// Workflow name (called `name` in REST, `workflowName` in GraphQL).
    #[serde(default)]
    name: String,
    #[serde(default)]
    head_sha: String,
    #[serde(default)]
    event: String,
    #[serde(default)]
    head_branch: String,
    #[serde(default = "default_attempt")]
    run_attempt: u32,
    #[serde(default)]
    created_at: String,
    #[serde(default)]
    updated_at: String,
    #[serde(default)]
    html_url: String,
    // Author fields — present on single-run detail, absent from list.
    triggering_actor: Option<RestActorJson>,
    head_commit: Option<RestHeadCommitJson>,
}

#[derive(Debug, Deserialize)]
struct RestActorJson {
    #[serde(default)]
    login: String,
}

#[derive(Debug, Deserialize)]
struct RestHeadCommitJson {
    author: Option<RestCommitAuthorJson>,
}

#[derive(Debug, Deserialize)]
struct RestCommitAuthorJson {
    #[serde(default)]
    name: String,
}

impl RestRunJson {
    fn into_run_info(self, repo: &str) -> Result<RunInfo, GhError> {
        let id = self
            .id
            .ok_or_else(|| GhError::MissingFields { repo: repo.into() })?;
        if self.status.is_empty() || self.display_title.is_empty() || self.name.is_empty() {
            return Err(GhError::MissingFields { repo: repo.into() });
        }
        Ok(RunInfo {
            id,
            status: self
                .status
                .parse::<RunStatus>()
                .unwrap_or(RunStatus::Unknown),
            conclusion: self.conclusion.unwrap_or_default(),
            title: self.display_title,
            workflow: self.name,
            head_sha: self.head_sha,
            event: self.event,
            head_branch: self.head_branch,
            attempt: self.run_attempt,
            created_at: self.created_at,
            updated_at: self.updated_at,
            url: self.html_url,
        })
    }

    fn into_history_entry(self) -> Option<HistoryEntry> {
        Some(HistoryEntry {
            id: self.id?,
            conclusion: if self.conclusion.as_deref().unwrap_or("").is_empty() {
                "in_progress".to_string()
            } else {
                self.conclusion.unwrap_or_default()
            },
            workflow: self.name,
            title: self.display_title,
            branch: self.head_branch,
            event: self.event,
            created_at: self.created_at,
            updated_at: self.updated_at,
        })
    }
}

/// REST API jobs response.
#[derive(Debug, Deserialize)]
struct RestJobsResponse {
    #[serde(default)]
    jobs: Vec<RestJobJson>,
}

#[derive(Debug, Deserialize)]
struct RestJobJson {
    id: Option<u64>,
    #[serde(default)]
    name: String,
    conclusion: Option<String>,
    #[serde(default)]
    steps: Vec<RestStepJson>,
}

#[derive(Debug, Deserialize)]
struct RestStepJson {
    #[serde(default)]
    name: String,
    conclusion: Option<String>,
}

/// Simple name-bearing JSON object (used for tags / branches pagination).
#[derive(Deserialize)]
struct NameJson {
    name: String,
}

/// Minimal repo info from `GET /repos/{owner}/{repo}`.
#[derive(Deserialize)]
struct RestRepoJson {
    #[serde(default)]
    default_branch: String,
}

/// GraphQL PR query response wrappers.
#[derive(Deserialize)]
struct GraphqlPrResponse {
    data: Option<GraphqlPrData>,
    errors: Option<Vec<GraphqlErrorJson>>,
}

#[derive(Deserialize)]
struct GraphqlPrData {
    repository: Option<GraphqlPrRepo>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphqlPrRepo {
    pull_requests: GraphqlPrConnection,
}

#[derive(Deserialize)]
struct GraphqlPrConnection {
    nodes: Vec<GhPrJson>,
}

#[derive(Deserialize)]
struct GraphqlErrorJson {
    #[serde(default)]
    message: String,
}

#[async_trait::async_trait]
impl GitHubClient for ReqwestClient {
    #[tracing::instrument(skip_all, fields(%repo, %branch))]
    async fn recent_runs(&self, repo: &str, branch: &str) -> Result<Vec<RunInfo>, GhError> {
        self.list_runs(repo, DEFAULT_BRANCH_LIMIT, &[("branch", branch)])
            .await
    }

    #[tracing::instrument(skip_all, fields(%repo, %limit))]
    async fn recent_runs_for_repo(&self, repo: &str, limit: u32) -> Result<Vec<RunInfo>, GhError> {
        self.list_runs(repo, limit, &[]).await
    }

    #[tracing::instrument(skip_all, fields(%repo))]
    async fn in_progress_runs_for_repo(&self, repo: &str) -> Result<Vec<RunInfo>, GhError> {
        self.list_runs(repo, IN_PROGRESS_LIMIT, &[("status", "in_progress")])
            .await
    }

    #[tracing::instrument(skip_all, fields(%repo, %run_id))]
    async fn run_status(&self, repo: &str, run_id: u64) -> Result<RunInfo, GhError> {
        let raw: RestRunJson = self
            .api_get(repo, &format!("/repos/{repo}/actions/runs/{run_id}"))
            .await?;
        raw.into_run_info(repo)
    }

    async fn failing_steps(&self, repo: &str, run_id: u64) -> Option<FailureInfo> {
        let resp: RestJobsResponse = self
            .api_get(repo, &format!("/repos/{repo}/actions/runs/{run_id}/jobs"))
            .await
            .map_err(|e| {
                tracing::debug!(%repo, %run_id, error = %e, "Failed to fetch failing steps");
            })
            .ok()?;

        // Convert REST job format to the internal GhJob format for extract_failing_steps.
        let jobs: Vec<GhJob> = resp
            .jobs
            .into_iter()
            .map(|j| GhJob {
                database_id: j.id,
                name: j.name,
                conclusion: j.conclusion.unwrap_or_default(),
                steps: j
                    .steps
                    .into_iter()
                    .map(|s| GhStep {
                        name: s.name,
                        conclusion: s.conclusion.unwrap_or_default(),
                    })
                    .collect(),
            })
            .collect();
        extract_failing_steps(&jobs)
    }

    async fn run_rerun(
        &self,
        repo: &str,
        run_id: u64,
        failed_only: bool,
    ) -> Result<String, GhError> {
        let path = if failed_only {
            format!("/repos/{repo}/actions/runs/{run_id}/rerun-failed-jobs")
        } else {
            format!("/repos/{repo}/actions/runs/{run_id}/rerun")
        };
        self.api_post_empty(repo, &path).await
    }

    async fn run_list_history(
        &self,
        repo: &str,
        branch: Option<&str>,
        limit: u32,
    ) -> Result<Vec<HistoryEntry>, GhError> {
        let limit_str = limit.to_string();
        let mut query: Vec<(&str, &str)> = vec![("per_page", &limit_str)];
        if let Some(b) = branch {
            query.push(("branch", b));
        }
        let resp: RestRunsResponse = self
            .api_get_query(repo, &format!("/repos/{repo}/actions/runs"), &query)
            .await?;
        Ok(resp
            .workflow_runs
            .into_iter()
            .filter_map(|r| r.into_history_entry())
            .collect())
    }

    async fn rate_limit(&self) -> Result<RateLimit, GhError> {
        #[derive(Deserialize)]
        struct RateLimitResponse {
            resources: RateLimitResources,
        }
        #[derive(Deserialize)]
        struct RateLimitResources {
            core: RateLimit,
        }
        let resp: RateLimitResponse = self.api_get("rate_limit", "/rate_limit").await?;
        Ok(resp.resources.core)
    }

    async fn list_tags(&self, repo: &str) -> Result<Vec<String>, GhError> {
        let items: Vec<NameJson> = self
            .api_get_all_pages(repo, &format!("/repos/{repo}/tags"))
            .await?;
        Ok(items.into_iter().map(|n| n.name).collect())
    }

    #[tracing::instrument(skip_all, fields(%repo))]
    async fn list_branches(&self, repo: &str) -> Result<Vec<String>, GhError> {
        let items: Vec<NameJson> = self
            .api_get_all_pages(repo, &format!("/repos/{repo}/branches"))
            .await?;
        Ok(items.into_iter().map(|n| n.name).collect())
    }

    #[tracing::instrument(skip_all, fields(%repo))]
    async fn default_branch(&self, repo: &str) -> Result<String, GhError> {
        let info: RestRepoJson = self.api_get(repo, &format!("/repos/{repo}")).await?;
        if info.default_branch.is_empty() {
            Err(GhError::MissingFields { repo: repo.into() })
        } else {
            Ok(info.default_branch)
        }
    }

    #[tracing::instrument(skip_all, fields(%repo))]
    async fn open_prs(&self, repo: &str) -> Result<Vec<PrInfo>, GhError> {
        let (owner, name) = repo
            .split_once('/')
            .ok_or_else(|| GhError::MissingFields { repo: repo.into() })?;
        let query = format!(
            r#"{{ repository(owner: "{owner}", name: "{name}") {{ pullRequests(states: OPEN, first: {MAX_OPEN_PRS}) {{ nodes {{ number title headRefName baseRefName url isDraft mergeStateStatus reviewDecision author {{ login }} }} }} }} }}"#,
        );
        let resp: GraphqlPrResponse = self
            .graphql_query(repo, &query)
            .await
            .map(|v| {
                serde_json::from_value(v).map_err(|e| GhError::Parse {
                    repo: repo.into(),
                    source: e,
                })
            })?
            .map_err(|e| {
                tracing::debug!(%repo, error = %e, "GraphQL PR parse error");
                e
            })?;

        if let Some(errors) = &resp.errors
            && !errors.is_empty()
        {
            let msgs: Vec<&str> = errors.iter().map(|e| e.message.as_str()).collect();
            return Err(GhError::CliError {
                repo: repo.into(),
                stderr: msgs.join("; "),
            });
        }

        let nodes = resp
            .data
            .and_then(|d| d.repository)
            .map(|r| r.pull_requests.nodes)
            .unwrap_or_default();

        Ok(nodes
            .into_iter()
            .map(|pr| PrInfo {
                number: pr.number,
                title: pr.title,
                branch: pr.head_ref_name,
                target_branch: pr.base_ref_name,
                url: pr.url,
                author: pr.author.map(|a| a.login).unwrap_or_default(),
                draft: pr.is_draft,
                merge_state: pr.merge_state_status,
                review_decision: pr.review_decision,
            })
            .collect())
    }

    async fn pr_merge(&self, repo: &str, number: u64) -> Result<String, GhError> {
        let url = format!("{GITHUB_API_BASE}/repos/{repo}/pulls/{number}/merge");
        let body = serde_json::json!({ "merge_method": "merge" });
        let resp = self
            .send_with_retry(|s| s.put_json(&url, &body), repo)
            .await?;
        if !resp.status().is_success() {
            return Err(response_error(resp, repo).await);
        }
        resp.text().await.map_err(|e| GhError::CliError {
            repo: repo.into(),
            stderr: e.to_string(),
        })
    }

    async fn run_author(&self, repo: &str, run_id: u64) -> Option<RunAuthorInfo> {
        let raw: RestRunJson = self
            .api_get(repo, &format!("/repos/{repo}/actions/runs/{run_id}"))
            .await
            .map_err(|e| {
                tracing::debug!(%repo, %run_id, error = %e, "Failed to fetch run author");
            })
            .ok()?;
        let actor = raw.triggering_actor?.login;
        if actor.is_empty() {
            return None;
        }
        Some(RunAuthorInfo {
            actor,
            commit_author: raw
                .head_commit
                .and_then(|hc| hc.author)
                .map(|a| a.name)
                .filter(|s| !s.is_empty()),
        })
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
        assert_eq!(run.duration_secs(), Some(330)); // 5m30s

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
        assert_eq!(lb.conclusion, "success");
        assert_eq!(lb.workflow, "Lint and Test");
        assert_eq!(lb.title, "Fix login bug");
    }

    #[test]
    fn missing_required_fields_returns_none() {
        // Missing status, title, workflow → from_gh_json returns Err
        let v = json!({ "databaseId": 1 });
        assert!(run_from_value(&v).is_none());

        // Missing just title
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
        let not_found = GhError::CliError {
            repo: "alice/gone".to_string(),
            stderr: "GraphQL: Could not resolve to a Repository with the name 'alice/gone'."
                .to_string(),
        };
        assert!(not_found.is_repo_not_found());

        let http_404 = GhError::CliError {
            repo: "alice/gone".to_string(),
            stderr: "HTTP 404: Not Found".to_string(),
        };
        assert!(http_404.is_repo_not_found());

        let transient = GhError::CliError {
            repo: "alice/app".to_string(),
            stderr: "HTTP 502: Bad Gateway".to_string(),
        };
        assert!(!transient.is_repo_not_found());

        let timeout = GhError::Timeout {
            repo: "alice/app".to_string(),
            timeout_secs: 30,
        };
        assert!(!timeout.is_repo_not_found());
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
        // Fractional seconds are ignored
        assert_eq!(
            parse_iso_epoch("2024-01-01T12:30:45Z"),
            parse_iso_epoch("2024-01-01T12:30:45.123Z")
        );
        // Duration between two timestamps
        let start = parse_iso_epoch("2024-01-01T10:00:00Z").unwrap();
        let end = parse_iso_epoch("2024-01-01T10:05:30Z").unwrap();
        assert_eq!(end - start, 330);
    }

    #[test]
    fn parse_iso_epoch_rejects_invalid() {
        // Malformed
        assert!(parse_iso_epoch("").is_none());
        assert!(parse_iso_epoch("not-a-date").is_none());
        assert!(parse_iso_epoch("2024-01-01").is_none());
        // Invalid day
        assert!(parse_iso_epoch("2024-02-30T00:00:00Z").is_none());
        assert!(parse_iso_epoch("2023-02-29T00:00:00Z").is_none()); // non-leap
        assert!(parse_iso_epoch("2024-02-29T00:00:00Z").is_some()); // leap
        // Invalid time
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
        assert_eq!(pr.duration_secs(), None); // invalid timestamps

        let bad = make_history("push", "invalid", "2024-01-01T10:05:30Z");
        assert_eq!(bad.duration_secs(), None);
    }

    #[test]
    fn history_entry_age_secs() {
        let entry = make_history("push", "2024-01-01T10:00:00Z", "2024-01-01T10:05:30Z");
        let created_epoch = parse_iso_epoch("2024-01-01T10:00:00Z").unwrap();
        // 5 minutes after created_at
        assert_eq!(entry.age_secs(created_epoch + 300), Some(300));
        // now before created_at saturates to 0
        assert_eq!(entry.age_secs(created_epoch - 100), Some(0));
        // invalid timestamp returns None
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

    // -- PR parsing tests --

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
}
