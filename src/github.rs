use std::time::Duration;

use serde::{Deserialize, Serialize};

const GH_TIMEOUT: Duration = Duration::from_secs(30);
const GH_JSON_FIELDS: &str = "databaseId,status,conclusion,displayTitle,workflowName,headSha,event";

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
    /// Unix timestamp (seconds) when this build completed. Persisted so age survives restarts.
    #[serde(default)]
    pub completed_at: Option<u64>,
    /// Duration in seconds from run start to completion. Only set for runs completed while the
    /// daemon was watching; `None` for already-completed runs detected on startup or mid-poll.
    #[serde(default)]
    pub duration_secs: Option<u64>,
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
}

/// A GitHub Actions run parsed for internal use.
#[derive(Debug, Clone)]
pub struct RunInfo {
    pub id: u64,
    pub status: String,
    pub conclusion: String,
    pub title: String,
    pub workflow: String,
    pub head_sha: String,
    pub event: String,
}

impl RunInfo {
    fn from_gh_json(raw: GhRunJson) -> Option<Self> {
        Some(Self {
            id: raw.database_id?,
            status: if raw.status.is_empty() {
                "unknown".to_string()
            } else {
                raw.status
            },
            conclusion: raw.conclusion,
            title: if raw.display_title.is_empty() {
                "unknown".to_string()
            } else {
                raw.display_title
            },
            workflow: if raw.workflow_name.is_empty() {
                "unknown".to_string()
            } else {
                raw.workflow_name
            },
            head_sha: raw.head_sha,
            event: raw.event,
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
        self.status == "completed"
    }

    pub fn succeeded(&self) -> bool {
        self.conclusion == "success"
    }

    pub fn url(&self, repo: &str) -> String {
        run_url(repo, self.id)
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
            completed_at: None,
            duration_secs: None,
        }
    }
}

/// Abstraction over the GitHub API. The real implementation (`GhCliClient`) calls
/// the `gh` CLI; tests can inject a mock.
#[async_trait::async_trait]
pub trait GitHubClient: Send + Sync + 'static {
    async fn recent_runs(&self, repo: &str, branch: &str) -> Result<Vec<RunInfo>, GhError>;
    async fn run_status(&self, repo: &str, run_id: u64) -> Result<RunInfo, GhError>;
    async fn failing_steps(&self, repo: &str, run_id: u64) -> Option<String>;
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
}

/// Real GitHub client that shells out to the `gh` CLI.
pub struct GhCliClient;

#[async_trait::async_trait]
impl GitHubClient for GhCliClient {
    #[tracing::instrument(skip_all, fields(%repo, %branch))]
    async fn recent_runs(&self, repo: &str, branch: &str) -> Result<Vec<RunInfo>, GhError> {
        let stdout = gh_exec(
            repo,
            &[
                "run",
                "list",
                "--repo",
                repo,
                "--branch",
                branch,
                "--limit",
                "10",
                "--json",
                GH_JSON_FIELDS,
            ],
        )
        .await?;

        let raw: Vec<GhRunJson> = serde_json::from_slice(&stdout).map_err(|e| GhError::Parse {
            repo: repo.to_string(),
            source: e,
        })?;

        Ok(raw.into_iter().filter_map(RunInfo::from_gh_json).collect())
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

        RunInfo::from_gh_json(raw).ok_or_else(|| GhError::MissingFields {
            repo: repo.to_string(),
        })
    }

    async fn failing_steps(&self, repo: &str, run_id: u64) -> Option<String> {
        let id_str = run_id.to_string();
        let stdout = gh_exec(
            repo,
            &["run", "view", &id_str, "--repo", repo, "--json", "jobs"],
        )
        .await
        .ok()?;

        let resp: GhJobsResponse = serde_json::from_slice(&stdout).ok()?;
        extract_failing_steps(&resp.jobs)
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
}

/// Format a human-readable title. PR events get a "PR: " prefix;
/// all other events use the title as-is.
pub(crate) fn display_title(event: &str, title: &str) -> String {
    if event.starts_with("pull_request") {
        format!("PR: {title}")
    } else {
        title.to_string()
    }
}

#[derive(Debug, Deserialize)]
struct GhStep {
    name: String,
    conclusion: String,
}

#[derive(Debug, Deserialize)]
struct GhJob {
    name: String,
    conclusion: String,
    steps: Vec<GhStep>,
}

#[derive(Debug, Deserialize)]
struct GhJobsResponse {
    jobs: Vec<GhJob>,
}

/// Pure extraction of failing job/step names from parsed GitHub API response.
fn extract_failing_steps(jobs: &[GhJob]) -> Option<String> {
    let failures: Vec<String> = jobs
        .iter()
        .filter(|job| job.conclusion == "failure")
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

    if failures.is_empty() {
        None
    } else {
        Some(failures.join(", "))
    }
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

/// Parse an ISO 8601 / RFC 3339 timestamp (e.g. `"2026-03-24T10:30:00Z"`) to Unix epoch seconds.
fn parse_iso_epoch(s: &str) -> Option<u64> {
    u64::try_from(chrono::DateTime::parse_from_rfc3339(s).ok()?.timestamp()).ok()
}

const GH_HISTORY_FIELDS: &str =
    "databaseId,conclusion,displayTitle,workflowName,headBranch,event,createdAt,updatedAt";

/// Fetch recent build history for a repo, optionally filtered by branch.
async fn gh_run_list_history_impl(
    repo: &str,
    branch: Option<&str>,
    limit: u32,
) -> Result<Vec<HistoryEntry>, GhError> {
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

/// URL for a specific workflow run.
pub fn run_url(repo: &str, run_id: u64) -> String {
    format!("https://github.com/{repo}/actions/runs/{run_id}")
}

/// URL for a repository.
pub fn repo_url(repo: &str) -> String {
    format!("https://github.com/{repo}")
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
            "event": "push"
        })
    }

    fn run_from_value(v: &serde_json::Value) -> Option<RunInfo> {
        let raw: GhRunJson = serde_json::from_value(v.clone()).ok()?;
        RunInfo::from_gh_json(raw)
    }

    #[test]
    fn from_json_parses_all_fields() {
        let run = run_from_value(&sample_json()).unwrap();
        assert_eq!(run.id, 123456789);
        assert_eq!(run.status, "completed");
        assert_eq!(run.conclusion, "success");
        assert_eq!(run.title, "Fix login bug");
        assert_eq!(run.workflow, "Lint and Test");
        assert_eq!(run.head_sha, "abc1234def5678");
        assert_eq!(run.event, "push");
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
            run.url("alice/myapp"),
            "https://github.com/alice/myapp/actions/runs/123456789"
        );

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
    fn defaults_for_missing_optional_fields() {
        let v = json!({ "databaseId": 1 });
        let run = run_from_value(&v).unwrap();
        assert_eq!(run.status, "unknown");
        assert_eq!(run.title, "unknown");
        assert_eq!(run.workflow, "unknown");
        assert_eq!(run.conclusion, "");
        assert_eq!(run.head_sha, "");
        assert_eq!(run.event, "");
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

        let mut v = sample_json();
        v["event"] = json!("pull_request");
        assert_eq!(
            run_from_value(&v).unwrap().display_title(),
            "PR: Fix login bug"
        );
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
        assert_eq!(
            extract_failing_steps(&jobs),
            Some("Test / Run tests".to_string())
        );
    }

    #[test]
    fn extract_failing_steps_job_failed_no_step() {
        let jobs = vec![job("Deploy", "failure", vec![("Setup", "success")])];
        assert_eq!(extract_failing_steps(&jobs), Some("Deploy".to_string()));
    }

    #[test]
    fn extract_failing_steps_multiple_failures() {
        let jobs = vec![
            job("Lint", "failure", vec![("Check", "failure")]),
            job("Test", "failure", vec![("Run", "failure")]),
        ];
        assert_eq!(
            extract_failing_steps(&jobs),
            Some("Lint / Check, Test / Run".to_string())
        );
    }

    #[test]
    fn extract_failing_steps_none_when_all_pass() {
        let jobs = vec![job("Build", "success", vec![("Compile", "success")])];
        assert_eq!(extract_failing_steps(&jobs), None);
    }

    #[test]
    fn extract_failing_steps_empty_jobs() {
        assert_eq!(extract_failing_steps(&[]), None);
    }
}
