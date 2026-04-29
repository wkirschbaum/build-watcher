use std::collections::HashMap;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use super::{
    DEFAULT_BRANCH_LIMIT, GH_TIMEOUT, GhAuthorJson, GhError, GhJob, GhPrJson, GhStep, GitHubClient,
    HistoryEntry, IN_PROGRESS_LIMIT, MAX_OPEN_PRS, PrInfo, RateLimit, RunAuthorInfo, RunInfo,
    RunStatus, default_attempt, extract_failing_steps, gh_auth_token,
};

const GITHUB_API_BASE: &str = "https://api.github.com";

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

/// GitHub client using direct HTTP via `reqwest`. Supports ETag-based
/// conditional requests (304 responses don't count against the rate limit)
/// and automatic token refresh on 401.
pub struct ReqwestClient {
    client: reqwest::Client,
    token: Mutex<String>,
    /// ETag cache: URL → (etag, response body).
    cache: Mutex<HashMap<String, CachedResponse>>,
    base_url: String,
}

impl ReqwestClient {
    pub fn new(token: String) -> Self {
        Self::with_base_url(token, GITHUB_API_BASE.to_string())
    }

    fn with_base_url(token: String, base_url: String) -> Self {
        let client = reqwest::Client::builder()
            .timeout(GH_TIMEOUT)
            .user_agent("build-watcher")
            .build()
            .expect("failed to build reqwest client");
        Self {
            client,
            token: Mutex::new(token),
            cache: Mutex::new(HashMap::new()),
            base_url,
        }
    }

    fn token(&self) -> String {
        self.token.lock().unwrap().clone()
    }

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
    /// On 401, refreshes the token and retries once.
    async fn cached_get(&self, url: &str, repo: &str) -> Result<Vec<u8>, GhError> {
        let resp = self.cached_get_inner(url, repo).await?;
        match resp {
            CachedGetResult::Body(body) => Ok(body),
            CachedGetResult::Unauthorized => {
                if self.refresh_token().await {
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

    async fn api_get<T: serde::de::DeserializeOwned>(
        &self,
        repo: &str,
        path: &str,
    ) -> Result<T, GhError> {
        let url = format!("{}{path}", self.base_url);
        let bytes = self.cached_get(&url, repo).await?;
        serde_json::from_slice(&bytes).map_err(|e| GhError::Parse {
            repo: repo.into(),
            source: e,
        })
    }

    async fn api_get_query<T: serde::de::DeserializeOwned>(
        &self,
        repo: &str,
        path: &str,
        query: &[(&str, &str)],
    ) -> Result<T, GhError> {
        let base = format!("{}{path}", self.base_url);
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

    async fn api_get_all_pages<T: serde::de::DeserializeOwned>(
        &self,
        repo: &str,
        path: &str,
    ) -> Result<Vec<T>, GhError> {
        let mut items = Vec::new();
        let mut url = Some(format!("{}{path}?per_page=100", self.base_url));

        while let Some(current_url) = url {
            let resp = self.send_with_retry(|s| s.get(&current_url), repo).await?;
            url = next_link_url(&resp);
            let page: Vec<T> = handle_response(resp, repo).await?;
            items.extend(page);
        }

        Ok(items)
    }

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

    async fn api_post_empty(&self, repo: &str, path: &str) -> Result<String, GhError> {
        let url = format!("{}{path}", self.base_url);
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

    async fn graphql_query(&self, repo: &str, query: &str) -> Result<serde_json::Value, GhError> {
        let url = format!("{}/graphql", self.base_url);
        let body = serde_json::json!({ "query": query });
        let resp = self
            .send_with_retry(|s| s.post_json(&url, &body), repo)
            .await?;
        handle_response(resp, repo).await
    }
}

/// Parse the `rel="next"` URL from a Link response header value.
fn parse_next_link(link: &str) -> Option<String> {
    for part in link.split(',') {
        let part = part.trim();
        if part.contains("rel=\"next\"") {
            let url = part.split('>').next()?.trim_start_matches('<');
            return Some(url.to_string());
        }
    }
    None
}

fn next_link_url(resp: &reqwest::Response) -> Option<String> {
    let link = resp.headers().get("link")?.to_str().ok()?;
    parse_next_link(link)
}

async fn response_error(resp: reqwest::Response, repo: &str) -> GhError {
    let status = resp.status();
    if status.as_u16() == 404 {
        return GhError::NotFound { repo: repo.into() };
    }
    let body = resp.text().await.unwrap_or_default();
    GhError::CliError {
        repo: repo.into(),
        stderr: format!("HTTP {status}: {body}"),
    }
}

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

#[derive(Debug, Deserialize)]
struct RestRunJson {
    id: Option<u64>,
    #[serde(default)]
    status: String,
    conclusion: Option<String>,
    #[serde(default)]
    display_title: String,
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

#[derive(Deserialize)]
struct NameJson {
    name: String,
}

#[derive(Deserialize)]
struct RestRepoJson {
    #[serde(default)]
    default_branch: String,
}

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

    async fn failing_steps(&self, repo: &str, run_id: u64) -> Option<super::FailureInfo> {
        let resp: RestJobsResponse = self
            .api_get(repo, &format!("/repos/{repo}/actions/runs/{run_id}/jobs"))
            .await
            .inspect_err(|e| {
                tracing::debug!(%repo, %run_id, error = %e, "Failed to fetch failing steps");
            })
            .ok()?;

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
            .api_get_query(
                repo,
                &format!("/repos/{repo}/branches"),
                &[("per_page", "100")],
            )
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
            .inspect_err(|e| {
                tracing::debug!(%repo, error = %e, "GraphQL PR parse error");
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
                author: pr.author.map(|a: GhAuthorJson| a.login).unwrap_or_default(),
                draft: pr.is_draft,
                merge_state: pr.merge_state_status,
                review_decision: pr.review_decision,
            })
            .collect())
    }

    async fn pr_merge(&self, repo: &str, number: u64) -> Result<String, GhError> {
        let url = format!("{}/repos/{repo}/pulls/{number}/merge", self.base_url);
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
            .inspect_err(|e| {
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
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn make_client(base_url: String) -> ReqwestClient {
        ReqwestClient::with_base_url("test-token".to_string(), base_url)
    }

    fn runs_response(runs: serde_json::Value) -> serde_json::Value {
        serde_json::json!({ "workflow_runs": runs })
    }

    fn sample_run() -> serde_json::Value {
        serde_json::json!({
            "id": 999,
            "status": "completed",
            "conclusion": "success",
            "display_title": "Fix bug",
            "name": "CI",
            "head_sha": "abc1234",
            "event": "push",
            "head_branch": "main",
            "run_attempt": 1,
            "created_at": "2026-01-01T10:00:00Z",
            "updated_at": "2026-01-01T10:05:00Z",
            "html_url": "https://github.com/alice/app/actions/runs/999"
        })
    }

    // -- next_link_url unit tests (no network) --

    #[test]
    fn parse_next_link_finds_rel_next() {
        let link = r#"<https://api.github.com/repos/a/b/actions/runs?page=2>; rel="next", <https://api.github.com/repos/a/b/actions/runs?page=5>; rel="last""#;
        assert_eq!(
            parse_next_link(link).unwrap(),
            "https://api.github.com/repos/a/b/actions/runs?page=2"
        );
    }

    #[test]
    fn parse_next_link_returns_none_when_absent() {
        let link = r#"<https://api.github.com/repos/a/b/actions/runs?page=5>; rel="last""#;
        assert!(parse_next_link(link).is_none());
    }

    #[test]
    fn parse_next_link_handles_only_next() {
        let link = r#"<https://example.com/page2>; rel="next""#;
        assert_eq!(parse_next_link(link).unwrap(), "https://example.com/page2");
    }

    // -- REST JSON parsing --

    #[tokio::test]
    async fn recent_runs_for_repo_parses_json() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/alice/app/actions/runs"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(runs_response(serde_json::json!([sample_run()]))),
            )
            .mount(&server)
            .await;

        let client = make_client(server.uri());
        let runs = client.recent_runs_for_repo("alice/app", 20).await.unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].id, 999);
        assert_eq!(runs[0].workflow, "CI");
        assert_eq!(runs[0].head_branch, "main");
        assert!(runs[0].is_completed());
        assert!(runs[0].succeeded());
    }

    #[tokio::test]
    async fn recent_runs_for_repo_returns_empty_on_no_runs() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/alice/app/actions/runs"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(runs_response(serde_json::json!([]))),
            )
            .mount(&server)
            .await;

        let client = make_client(server.uri());
        let runs = client.recent_runs_for_repo("alice/app", 20).await.unwrap();
        assert!(runs.is_empty());
    }

    // -- ETag caching --

    #[tokio::test]
    async fn etag_cache_returns_cached_body_on_304() {
        let server = MockServer::start().await;

        // First request only: 200 with ETag. up_to_n_times(1) exhausts this
        // mock after one match so the fallback 304 mock handles the second call.
        Mock::given(method("GET"))
            .and(path("/repos/alice/app/actions/runs"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("etag", "v1")
                    .set_body_json(runs_response(serde_json::json!([sample_run()]))),
            )
            .up_to_n_times(1)
            .mount(&server)
            .await;

        // Fallback: all subsequent requests → 304 Not Modified
        Mock::given(method("GET"))
            .and(path("/repos/alice/app/actions/runs"))
            .respond_with(ResponseTemplate::new(304))
            .mount(&server)
            .await;

        let client = make_client(server.uri());

        let runs1 = client.recent_runs_for_repo("alice/app", 20).await.unwrap();
        assert_eq!(runs1.len(), 1);
        assert_eq!(runs1[0].id, 999);

        // Second call: server returns 304, client returns cached body
        let runs2 = client.recent_runs_for_repo("alice/app", 20).await.unwrap();
        assert_eq!(runs2.len(), 1);
        assert_eq!(runs2[0].id, 999);

        // Verify the second request included the If-None-Match header
        let received = server.received_requests().await.unwrap();
        assert_eq!(received.len(), 2, "expected exactly 2 HTTP requests");
        assert!(
            received[1].headers.get("if-none-match").is_some(),
            "second request should carry If-None-Match"
        );
    }

    #[tokio::test]
    async fn api_error_propagates_as_gh_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/missing/repo/actions/runs"))
            .respond_with(ResponseTemplate::new(404).set_body_string("Not Found"))
            .mount(&server)
            .await;

        let client = make_client(server.uri());
        let err = client
            .recent_runs_for_repo("missing/repo", 20)
            .await
            .unwrap_err();
        assert!(
            err.is_repo_not_found(),
            "expected repo-not-found, got: {err}"
        );
    }
}
