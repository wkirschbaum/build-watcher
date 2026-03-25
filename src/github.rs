use std::time::Duration;

use serde::{Deserialize, Serialize};

const GH_TIMEOUT: Duration = Duration::from_secs(30);
const GH_JSON_FIELDS: &str =
    "databaseId,status,conclusion,displayTitle,workflowName,headSha,headBranch,event";

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
    /// Human-friendly title: "PR: <title>" for `pull_request` events, else "<title> <sha>".
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

    /// Human-friendly title: "PR: <title>" for `pull_request` events, else "<title> <sha>".
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
pub async fn gh_run_status(repo: &str, run_id: u64) -> Result<RunInfo, GhError> {
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

/// Format a human-readable title. PR events (`pull_request`, `pull_request_target`)
/// show "PR: <title>", push events show "<title> (<sha>)".
pub(crate) fn display_title(event: &str, title: &str, head_sha: &str) -> String {
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
/// Returns a human-readable string like "Build / Run tests", or None on error.
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

    let id_str = run_id.to_string();
    let stdout = gh_exec(
        repo,
        &["run", "view", &id_str, "--repo", repo, "--json", "jobs"],
    )
    .await
    .ok()?;

    let resp: GhJobsResponse = serde_json::from_slice(&stdout).ok()?;
    let mut failures: Vec<String> = Vec::new();

    for job in &resp.jobs {
        if job.conclusion == "failure" {
            let step = job
                .steps
                .iter()
                .find(|s| s.conclusion == "failure")
                .map_or_else(
                    || job.name.clone(),
                    |s| format!("{} / {}", job.name, s.name),
                );
            failures.push(step);
        }
    }

    if failures.is_empty() {
        None
    } else {
        Some(failures.join(", "))
    }
}

/// Rerun a GitHub Actions run. If `failed_only` is true, only reruns failed jobs.
pub async fn gh_run_rerun(repo: &str, run_id: u64, failed_only: bool) -> Result<String, GhError> {
    let id_str = run_id.to_string();
    let mut args = vec!["run", "rerun", &id_str, "--repo", repo];
    if failed_only {
        args.push("--failed");
    }
    let stdout = gh_exec(repo, &args).await?;
    Ok(String::from_utf8_lossy(&stdout).to_string())
}

/// A build history entry with timestamps for duration/age calculation.
#[derive(Debug)]
#[allow(dead_code)]
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
        display_title(&self.event, &self.title, "")
    }

    /// Duration as `updated_at - created_at`, parsed from ISO 8601 timestamps.
    pub fn duration_secs(&self) -> Option<u64> {
        let start = parse_iso_epoch(&self.created_at)?;
        let end = parse_iso_epoch(&self.updated_at)?;
        Some(end.saturating_sub(start))
    }

    /// Seconds since `created_at`.
    pub fn age_secs(&self) -> Option<u64> {
        let start = parse_iso_epoch(&self.created_at)?;
        let now = crate::config::unix_now();
        Some(now.saturating_sub(start))
    }
}

/// Minimal ISO 8601 parser -> Unix epoch seconds. Handles "2026-03-24T10:30:00Z" format.
fn parse_iso_epoch(s: &str) -> Option<u64> {
    // Format: YYYY-MM-DDTHH:MM:SSZ (GitHub always returns UTC)
    let s = s.trim().trim_end_matches('Z');
    let (date, time) = s.split_once('T')?;
    let parts: Vec<&str> = date.split('-').collect();
    if parts.len() != 3 {
        return None;
    }
    let year: u64 = parts[0].parse().ok()?;
    let month: u64 = parts[1].parse().ok()?;
    let day: u64 = parts[2].parse().ok()?;

    let time_parts: Vec<&str> = time.split(':').collect();
    if time_parts.len() < 2 {
        return None;
    }
    let hour: u64 = time_parts[0].parse().ok()?;
    let min: u64 = time_parts[1].parse().ok()?;
    // Handle fractional seconds (e.g. "30.123")
    let sec: u64 = time_parts
        .get(2)
        .and_then(|s| s.split('.').next()?.parse().ok())
        .unwrap_or(0);

    if !(1..=12).contains(&month) || day == 0 || hour > 23 || min > 59 || sec > 59 {
        return None;
    }

    let is_leap = year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400));
    let month_days: [u64; 12] = [
        31,
        if is_leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    #[allow(clippy::cast_possible_truncation)]
    // month is validated 1-12, value fits usize on all targets
    if day > month_days[(month - 1) as usize] {
        return None;
    }

    // Days from epoch (constant-time calculation)
    let leap_days = |y: u64| -> u64 { y / 4 - y / 100 + y / 400 };
    let y0 = year - 1;
    let days_to_year = year * 365 + leap_days(y0) - (1970 * 365 + leap_days(1969));
    let mut days: u64 = days_to_year;
    for md in &month_days[..((month - 1) as usize)] {
        days += md;
    }
    days += day - 1;

    Some(days * 86400 + hour * 3600 + min * 60 + sec)
}

const GH_HISTORY_FIELDS: &str =
    "databaseId,conclusion,displayTitle,workflowName,headBranch,event,createdAt,updatedAt";

/// Fetch recent build history for a repo, optionally filtered by branch.
pub async fn gh_run_list_history(
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

/// Fetch current rate limit for the `core` resource. This call is free and
/// does not count against the rate limit itself.
pub async fn gh_rate_limit() -> Result<RateLimit, GhError> {
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

    #[test]
    fn display_title_for_push_event() {
        let run = run_from_value(&sample_json()).unwrap();
        assert_eq!(run.display_title(), "Fix login bug (abc1234)");
    }

    #[test]
    fn display_title_for_pr_event() {
        let mut v = sample_json();
        v["event"] = json!("pull_request");
        let run = run_from_value(&v).unwrap();
        assert_eq!(run.display_title(), "PR: Fix login bug");
    }

    #[test]
    fn display_title_for_empty_sha() {
        let mut v = sample_json();
        v["headSha"] = json!("");
        let run = run_from_value(&v).unwrap();
        assert_eq!(run.display_title(), "Fix login bug");
    }

    #[test]
    fn last_build_display_title() {
        let run = run_from_value(&sample_json()).unwrap();
        let lb = run.to_last_build();
        assert_eq!(lb.display_title(), "Fix login bug (abc1234)");
    }

    #[test]
    fn parse_iso_epoch_basic() {
        // 2024-01-01T00:00:00Z = known epoch value
        let epoch = parse_iso_epoch("2024-01-01T00:00:00Z");
        assert!(epoch.is_some());
        // 2024-01-01 is 19723 days after 1970-01-01
        assert_eq!(epoch.unwrap(), 19723 * 86400);
    }

    #[test]
    fn parse_iso_epoch_with_fractional_seconds() {
        let a = parse_iso_epoch("2024-01-01T12:30:45Z");
        let b = parse_iso_epoch("2024-01-01T12:30:45.123Z");
        assert_eq!(a, b); // fractional seconds are ignored
    }

    #[test]
    fn parse_iso_epoch_returns_none_for_invalid() {
        assert!(parse_iso_epoch("").is_none());
        assert!(parse_iso_epoch("not-a-date").is_none());
        assert!(parse_iso_epoch("2024-01-01").is_none()); // no time component
    }

    #[test]
    fn parse_iso_epoch_duration_calculation() {
        let start = parse_iso_epoch("2024-01-01T10:00:00Z").unwrap();
        let end = parse_iso_epoch("2024-01-01T10:05:30Z").unwrap();
        assert_eq!(end - start, 330); // 5m 30s = 330 seconds
    }

    #[test]
    fn parse_iso_epoch_rejects_invalid_day() {
        assert!(parse_iso_epoch("2024-02-30T00:00:00Z").is_none()); // Feb 30
        assert!(parse_iso_epoch("2024-04-31T00:00:00Z").is_none()); // Apr 31
        assert!(parse_iso_epoch("2023-02-29T00:00:00Z").is_none()); // non-leap Feb 29
        assert!(parse_iso_epoch("2024-02-29T00:00:00Z").is_some()); // leap Feb 29
    }

    #[test]
    fn parse_iso_epoch_rejects_invalid_time() {
        assert!(parse_iso_epoch("2024-01-01T24:00:00Z").is_none());
        assert!(parse_iso_epoch("2024-01-01T12:60:00Z").is_none());
    }
}
