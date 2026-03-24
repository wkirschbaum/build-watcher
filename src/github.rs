use std::time::Duration;

use serde::{Deserialize, Serialize};

const GH_TIMEOUT: Duration = Duration::from_secs(30);
const GH_JSON_FIELDS: &str =
    "databaseId,status,conclusion,displayTitle,workflowName,headSha,headBranch,event";

/// Truncates a hex SHA to 7 characters. Returns the full string if shorter.
pub fn short_sha(sha: &str) -> &str {
    sha.get(..7).unwrap_or(sha)
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
}

impl LastBuild {
    /// Human-friendly title: "PR: <title>" for pull_request events, else "<title> <sha>".
    pub fn display_title(&self) -> String {
        display_title(&self.event, &self.title, &self.head_sha)
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
#[derive(Debug)]
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

    /// Human-friendly title: "PR: <title>" for pull_request events, else "<title> <sha>".
    pub fn display_title(&self) -> String {
        display_title(&self.event, &self.title, &self.head_sha)
    }

    pub fn is_completed(&self) -> bool {
        self.status == "completed"
    }

    pub fn succeeded(&self) -> bool {
        self.conclusion == "success"
    }

    pub fn url(&self, repo: &str) -> String {
        format!("https://github.com/{repo}/actions/runs/{}", self.id)
    }

    pub fn to_last_build(&self) -> LastBuild {
        LastBuild {
            run_id: self.id,
            conclusion: self.conclusion.clone(),
            workflow: self.workflow.clone(),
            title: self.title.clone(),
            head_sha: self.head_sha.clone(),
            event: self.event.clone(),
        }
    }
}

#[tracing::instrument(skip_all, fields(%repo, %branch))]
pub async fn gh_recent_runs(repo: &str, branch: &str) -> Result<Vec<RunInfo>, GhError> {
    let output = tokio::time::timeout(
        GH_TIMEOUT,
        tokio::process::Command::new("gh")
            .args([
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
            ])
            .output(),
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

    let raw: Vec<GhRunJson> =
        serde_json::from_slice(&output.stdout).map_err(|e| GhError::Parse {
            repo: repo.to_string(),
            source: e,
        })?;

    Ok(raw.into_iter().filter_map(RunInfo::from_gh_json).collect())
}

#[tracing::instrument(skip_all, fields(%repo, %run_id))]
pub async fn gh_run_status(repo: &str, run_id: u64) -> Result<RunInfo, GhError> {
    let output = tokio::time::timeout(
        GH_TIMEOUT,
        tokio::process::Command::new("gh")
            .args([
                "run",
                "view",
                &run_id.to_string(),
                "--repo",
                repo,
                "--json",
                GH_JSON_FIELDS,
            ])
            .output(),
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

    let raw: GhRunJson = serde_json::from_slice(&output.stdout).map_err(|e| GhError::Parse {
        repo: repo.to_string(),
        source: e,
    })?;

    RunInfo::from_gh_json(raw).ok_or_else(|| GhError::MissingFields {
        repo: repo.to_string(),
    })
}

fn display_title(event: &str, title: &str, head_sha: &str) -> String {
    if event.starts_with("pull_request") {
        format!("PR: {title}")
    } else {
        let sha = short_sha(head_sha);
        if sha.is_empty() {
            title.to_string()
        } else {
            format!("{title} ({sha})")
        }
    }
}

/// Fetch the failing job and step names for a completed run.
/// Returns a human-readable string like "Job: Build / Step: Run tests", or None on error.
pub async fn gh_failing_steps(repo: &str, run_id: u64) -> Option<String> {
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

    let output = tokio::time::timeout(
        GH_TIMEOUT,
        tokio::process::Command::new("gh")
            .args([
                "run",
                "view",
                &run_id.to_string(),
                "--repo",
                repo,
                "--json",
                "jobs",
            ])
            .output(),
    )
    .await
    .ok()?
    .ok()?;

    if !output.status.success() {
        return None;
    }

    let resp: GhJobsResponse = serde_json::from_slice(&output.stdout).ok()?;
    let mut failures: Vec<String> = Vec::new();

    for job in &resp.jobs {
        if job.conclusion == "failure" {
            let step = job
                .steps
                .iter()
                .find(|s| s.conclusion == "failure")
                .map(|s| format!("{} / {}", job.name, s.name))
                .unwrap_or_else(|| job.name.clone());
            failures.push(step);
        }
    }

    if failures.is_empty() {
        None
    } else {
        Some(failures.join(", "))
    }
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
    fn short_sha_truncates_to_seven() {
        let run = run_from_value(&sample_json()).unwrap();
        assert_eq!(run.short_sha(), "abc1234");
    }

    #[test]
    fn short_sha_returns_full_sha_when_short() {
        let mut v = sample_json();
        v["headSha"] = json!("abc");
        let run = run_from_value(&v).unwrap();
        assert_eq!(run.short_sha(), "abc");
    }

    #[test]
    fn shared_short_sha_uses_get() {
        assert_eq!(short_sha("abc1234def5678"), "abc1234");
        assert_eq!(short_sha("abc"), "abc");
        assert_eq!(short_sha(""), "");
    }

    #[test]
    fn is_completed_true_when_status_completed() {
        let run = run_from_value(&sample_json()).unwrap();
        assert!(run.is_completed());
    }

    #[test]
    fn is_completed_false_when_in_progress() {
        let mut v = sample_json();
        v["status"] = json!("in_progress");
        let run = run_from_value(&v).unwrap();
        assert!(!run.is_completed());
    }

    #[test]
    fn succeeded_true_for_success_conclusion() {
        let run = run_from_value(&sample_json()).unwrap();
        assert!(run.succeeded());
    }

    #[test]
    fn succeeded_false_for_failure_conclusion() {
        let mut v = sample_json();
        v["conclusion"] = json!("failure");
        let run = run_from_value(&v).unwrap();
        assert!(!run.succeeded());
    }

    #[test]
    fn url_format() {
        let run = run_from_value(&sample_json()).unwrap();
        assert_eq!(
            run.url("alice/myapp"),
            "https://github.com/alice/myapp/actions/runs/123456789"
        );
    }

    #[test]
    fn to_last_build_copies_fields() {
        let run = run_from_value(&sample_json()).unwrap();
        let lb = run.to_last_build();
        assert_eq!(lb.run_id, 123456789);
        assert_eq!(lb.conclusion, "success");
        assert_eq!(lb.workflow, "Lint and Test");
        assert_eq!(lb.title, "Fix login bug");
        assert_eq!(lb.head_sha, "abc1234def5678");
        assert_eq!(lb.event, "push");
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
    fn validate_repo_accepts_valid() {
        assert!(validate_repo("alice/myapp").is_ok());
        assert!(validate_repo("my-org/my_repo.rs").is_ok());
    }

    #[test]
    fn validate_repo_rejects_invalid() {
        assert!(validate_repo("noslash").is_err());
        assert!(validate_repo("a/b/c").is_err());
        assert!(validate_repo("/repo").is_err());
        assert!(validate_repo("owner/").is_err());
        assert!(validate_repo("owner/repo name").is_err());
    }

    #[test]
    fn validate_branch_accepts_valid() {
        assert!(validate_branch("main").is_ok());
        assert!(validate_branch("feature/my-branch").is_ok());
        assert!(validate_branch("release-1.0").is_ok());
    }

    #[test]
    fn validate_branch_rejects_invalid() {
        assert!(validate_branch("").is_err());
        assert!(validate_branch("branch name").is_err());
    }
}
