use std::time::Duration;

use serde::{Deserialize, Serialize};

const GH_TIMEOUT: Duration = Duration::from_secs(30);
const GH_JSON_FIELDS: &str =
    "databaseId,status,conclusion,displayTitle,workflowName,headSha,headBranch,event";

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
    pub fn short_sha(&self) -> &str {
        if self.head_sha.len() >= 7 {
            &self.head_sha[..7]
        } else {
            &self.head_sha
        }
    }
}

/// A GitHub Actions run as returned by the `gh` CLI.
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
    pub fn from_json(value: &serde_json::Value) -> Option<Self> {
        Some(Self {
            id: value["databaseId"].as_u64()?,
            status: value["status"].as_str().unwrap_or("unknown").to_string(),
            conclusion: value["conclusion"].as_str().unwrap_or("").to_string(),
            title: value["displayTitle"]
                .as_str()
                .unwrap_or("unknown")
                .to_string(),
            workflow: value["workflowName"]
                .as_str()
                .unwrap_or("unknown")
                .to_string(),
            head_sha: value["headSha"].as_str().unwrap_or("").to_string(),
            event: value["event"].as_str().unwrap_or("").to_string(),
        })
    }

    pub fn short_sha(&self) -> &str {
        if self.head_sha.len() >= 7 {
            &self.head_sha[..7]
        } else {
            &self.head_sha
        }
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

pub async fn gh_recent_runs(repo: &str, branch: &str) -> Result<Vec<RunInfo>, String> {
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
    .map_err(|_| format!("{repo}: gh timed out after {}s", GH_TIMEOUT.as_secs()))?
    .map_err(|e| format!("{repo}: failed to run gh: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("{repo}: gh error: {stderr}"));
    }

    let values: Vec<serde_json::Value> =
        serde_json::from_slice(&output.stdout).map_err(|e| format!("{repo}: parse error: {e}"))?;

    Ok(values.iter().filter_map(RunInfo::from_json).collect())
}

pub async fn gh_run_status(repo: &str, run_id: u64) -> Result<RunInfo, String> {
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
    .map_err(|_| format!("{repo}: gh timed out after {}s", GH_TIMEOUT.as_secs()))?
    .map_err(|e| format!("{repo}: failed to run gh: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("{repo}: gh error: {stderr}"));
    }

    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).map_err(|e| format!("{repo}: parse error: {e}"))?;

    RunInfo::from_json(&value).ok_or_else(|| format!("{repo}: missing fields in response"))
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

    #[test]
    fn from_json_parses_all_fields() {
        let run = RunInfo::from_json(&sample_json()).unwrap();
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
        assert!(RunInfo::from_json(&v).is_none());
    }

    #[test]
    fn short_sha_truncates_to_seven() {
        let run = RunInfo::from_json(&sample_json()).unwrap();
        assert_eq!(run.short_sha(), "abc1234");
    }

    #[test]
    fn short_sha_returns_full_sha_when_short() {
        let mut v = sample_json();
        v["headSha"] = json!("abc");
        let run = RunInfo::from_json(&v).unwrap();
        assert_eq!(run.short_sha(), "abc");
    }

    #[test]
    fn is_completed_true_when_status_completed() {
        let run = RunInfo::from_json(&sample_json()).unwrap();
        assert!(run.is_completed());
    }

    #[test]
    fn is_completed_false_when_in_progress() {
        let mut v = sample_json();
        v["status"] = json!("in_progress");
        let run = RunInfo::from_json(&v).unwrap();
        assert!(!run.is_completed());
    }

    #[test]
    fn succeeded_true_for_success_conclusion() {
        let run = RunInfo::from_json(&sample_json()).unwrap();
        assert!(run.succeeded());
    }

    #[test]
    fn succeeded_false_for_failure_conclusion() {
        let mut v = sample_json();
        v["conclusion"] = json!("failure");
        let run = RunInfo::from_json(&v).unwrap();
        assert!(!run.succeeded());
    }

    #[test]
    fn url_format() {
        let run = RunInfo::from_json(&sample_json()).unwrap();
        assert_eq!(
            run.url("alice/myapp"),
            "https://github.com/alice/myapp/actions/runs/123456789"
        );
    }

    #[test]
    fn to_last_build_copies_fields() {
        let run = RunInfo::from_json(&sample_json()).unwrap();
        let lb = run.to_last_build();
        assert_eq!(lb.run_id, 123456789);
        assert_eq!(lb.conclusion, "success");
        assert_eq!(lb.workflow, "Lint and Test");
        assert_eq!(lb.title, "Fix login bug");
        assert_eq!(lb.head_sha, "abc1234def5678");
        assert_eq!(lb.event, "push");
    }
}
