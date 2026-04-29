use serde::Deserialize;

use super::{
    DEFAULT_BRANCH_LIMIT, GH_TIMEOUT, GhAuthorJson, GhError, GhJobsResponse, GhPrJson, GhRunJson,
    GitHubClient, HistoryEntry, IN_PROGRESS_LIMIT, MAX_OPEN_PRS, PrInfo, RunAuthorInfo, RunInfo,
    extract_failing_steps,
};

const GH_JSON_FIELDS: &str = "databaseId,status,conclusion,displayTitle,workflowName,headSha,event,headBranch,attempt,createdAt,updatedAt,url";
const GH_HISTORY_FIELDS: &str =
    "databaseId,conclusion,displayTitle,workflowName,headBranch,event,createdAt,updatedAt";
const GH_PR_FIELDS: &str =
    "number,title,headRefName,baseRefName,url,isDraft,mergeStateStatus,reviewDecision,author";

/// Raw JSON shape for history entries.
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
        if stderr.contains("Could not resolve to a Repository") || stderr.contains("Not Found") {
            return Err(GhError::NotFound {
                repo: repo.to_string(),
            });
        }
        return Err(GhError::CliError {
            repo: repo.to_string(),
            stderr,
        });
    }

    Ok(output.stdout)
}

/// Shared helper for `gh run list` with variable filters.
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

/// Real GitHub client that shells out to the `gh` CLI.
pub struct GhCliClient;

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

    async fn failing_steps(&self, repo: &str, run_id: u64) -> Option<super::FailureInfo> {
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

    async fn rate_limit(&self) -> Result<super::RateLimit, GhError> {
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
                author: pr.author.map(|a: GhAuthorJson| a.login).unwrap_or_default(),
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
