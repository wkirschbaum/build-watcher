use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;

use anyhow::Result;
use rmcp::handler::server::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResult, Content, Implementation, ServerCapabilities, ServerInfo,
};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService,
    session::local::LocalSessionManager,
};
use rmcp::{ErrorData as McpError, ServerHandler, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;

const DEFAULT_PORT: u16 = 8417;

// -- State file persistence --

fn state_path() -> PathBuf {
    let dir = dirs();
    dir.join("watches.json")
}

fn dirs() -> PathBuf {
    let dir = PathBuf::from(
        std::env::var("STATE_DIRECTORY")
            .unwrap_or_else(|_| {
                let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
                format!("{home}/.local/state/build-watcher")
            }),
    );
    std::fs::create_dir_all(&dir).ok();
    dir
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WatchEntry {
    run_id: Option<u64>,
    status: String,
}

type Watches = Arc<Mutex<HashMap<String, WatchEntry>>>;

fn load_watches() -> HashMap<String, WatchEntry> {
    let path = state_path();
    match std::fs::read_to_string(&path) {
        Ok(data) => serde_json::from_str(&data).unwrap_or_default(),
        Err(_) => HashMap::new(),
    }
}

async fn save_watches(watches: &Watches) {
    let w = watches.lock().await;
    let path = state_path();
    if let Ok(data) = serde_json::to_string_pretty(&*w) {
        let _ = std::fs::write(path, data);
    }
}

// -- Notifications --

fn send_notification(title: &str, body: &str, success: bool, url: Option<&str>) {
    if cfg!(target_os = "macos") {
        // macOS: use osascript for native notifications
        let sound = if success { "Glass" } else { "Basso" };
        let script = if let Some(url) = url {
            format!(
                r#"display notification "{body}" with title "{title}" sound name "{sound}"
do shell script "open {url}""#
            )
        } else {
            format!(
                r#"display notification "{body}" with title "{title}" sound name "{sound}""#
            )
        };
        let _ = Command::new("osascript").args(["-e", &script]).spawn();
    } else {
        // Linux: use notify-send
        let icon = if success { "dialog-information" } else { "dialog-error" };
        let urgency = if success { "normal" } else { "critical" };
        let notification_body = match url {
            Some(u) => format!("{body}\n{u}"),
            None => body.to_string(),
        };
        let _ = Command::new("notify-send")
            .args(["--urgency", urgency, "--icon", icon, title, &notification_body])
            .spawn();
    }
}

// -- GitHub CLI helpers --

fn gh_run_list(repo: &str) -> std::io::Result<std::process::Output> {
    Command::new("gh")
        .args([
            "run", "list", "--repo", repo, "--branch", "main", "--limit", "1", "--json",
            "databaseId,status,conclusion,displayTitle,workflowName",
        ])
        .output()
}

fn gh_run_view(repo: &str, run_id: u64) -> std::io::Result<std::process::Output> {
    Command::new("gh")
        .args([
            "run", "view", &run_id.to_string(), "--repo", repo,
            "--json", "status,conclusion,displayTitle,workflowName",
        ])
        .output()
}

// -- MCP Server --

#[derive(Debug, Deserialize, JsonSchema)]
struct WatchBuildsParams {
    /// List of GitHub repos in "owner/repo" format
    repos: Vec<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct StopWatchesParams {
    /// List of GitHub repos in "owner/repo" format
    repos: Vec<String>,
}

#[derive(Clone)]
pub struct BuildWatcher {
    tool_router: ToolRouter<Self>,
    watches: Watches,
}

#[tool_router]
impl BuildWatcher {
    pub fn new(watches: Watches) -> Self {
        Self {
            tool_router: Self::tool_router(),
            watches,
        }
    }

    #[tool(description = "Persistently watch GitHub Actions builds on main for one or more repos. Sends desktop notifications when builds start and complete. Keeps watching for new builds even after one finishes. Repos should be in owner/repo format.")]
    async fn watch_builds(
        &self,
        Parameters(params): Parameters<WatchBuildsParams>,
    ) -> Result<CallToolResult, McpError> {
        let mut results = Vec::new();

        for repo in &params.repos {
            match start_watch(&self.watches, repo).await {
                Ok(msg) => results.push(msg),
                Err(msg) => results.push(msg),
            }
        }

        Ok(CallToolResult::success(vec![Content::text(
            results.join("\n\n"),
        )]))
    }

    #[tool(description = "Stop watching builds for one or more repos. Repos should be in owner/repo format.")]
    async fn stop_watches(
        &self,
        Parameters(params): Parameters<StopWatchesParams>,
    ) -> Result<CallToolResult, McpError> {
        let mut watches = self.watches.lock().await;
        let mut results = Vec::new();

        for repo in &params.repos {
            if watches.remove(repo).is_some() {
                results.push(format!("Stopped watching {repo}"));
            } else {
                results.push(format!("No active watch for {repo}"));
            }
        }
        drop(watches);
        save_watches(&self.watches).await;

        Ok(CallToolResult::success(vec![Content::text(
            results.join("\n"),
        )]))
    }

    #[tool(description = "List all currently watched builds and their status")]
    async fn list_watches(&self) -> Result<CallToolResult, McpError> {
        let watches = self.watches.lock().await;
        if watches.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "No active watches",
            )]));
        }

        let mut lines = Vec::new();
        for (repo, entry) in watches.iter() {
            match entry.run_id {
                Some(run_id) => lines.push(format!(
                    "- {repo} (run {run_id}) — status: {}",
                    entry.status
                )),
                None => lines.push(format!(
                    "- {repo} — watching for new builds"
                )),
            }
        }
        Ok(CallToolResult::success(vec![Content::text(
            lines.join("\n"),
        )]))
    }

    #[tool(description = "Send a test desktop notification to verify notifications are working")]
    async fn test_notification(&self) -> Result<CallToolResult, McpError> {
        send_notification(
            "Build Watcher Test",
            "If you see this, notifications are working!",
            true,
            None,
        );
        Ok(CallToolResult::success(vec![Content::text(
            "Test notification sent. You should see it on your desktop.",
        )]))
    }
}

#[tool_handler]
impl ServerHandler for BuildWatcher {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::from_build_env())
            .with_instructions(
                "Monitors GitHub Actions builds and sends desktop notifications on completion. \
                 Use watch_builds with one or more repos in 'owner/repo' format to start watching.",
            )
    }

    async fn initialize(
        &self,
        _request: rmcp::model::InitializeRequestParams,
        _context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<ServerInfo, McpError> {
        Ok(self.get_info())
    }
}

// -- Watch logic --

async fn start_watch(watches: &Watches, repo: &str) -> std::result::Result<String, String> {
    // Check if already watching
    {
        let w = watches.lock().await;
        if w.contains_key(repo) {
            return Ok(format!("{repo}: already being watched"));
        }
    }

    let output = gh_run_list(repo).map_err(|e| format!("{repo}: failed to run gh: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("{repo}: gh error: {stderr}"));
    }

    let runs: Vec<serde_json::Value> =
        serde_json::from_slice(&output.stdout).map_err(|e| format!("{repo}: parse error: {e}"))?;

    let run = runs
        .first()
        .ok_or_else(|| format!("{repo}: no workflow runs found on main"))?;

    let run_id = run["databaseId"].as_u64().unwrap_or(0);
    let status = run["status"].as_str().unwrap_or("unknown").to_string();
    let conclusion = run["conclusion"].as_str().unwrap_or("");
    let title = run["displayTitle"].as_str().unwrap_or("unknown");
    let workflow = run["workflowName"].as_str().unwrap_or("unknown");

    let msg = if status == "completed" {
        let url = format!("https://github.com/{repo}/actions/runs/{run_id}");
        format!(
            "{repo}: latest build already completed ({conclusion}), watching for new builds\n  {workflow}: {title}\n  {url}"
        )
    } else {
        format!(
            "{repo}: watching run {run_id} ({status})\n  {workflow}: {title}"
        )
    };

    let entry = WatchEntry {
        run_id: if status == "completed" { None } else { Some(run_id) },
        status: if status == "completed" { "idle".to_string() } else { status.clone() },
    };

    {
        let mut w = watches.lock().await;
        w.insert(repo.to_string(), entry);
    }
    save_watches(watches).await;

    let watches = watches.clone();
    let repo_owned = repo.to_string();
    let current_run_id = if status == "completed" { None } else { Some(run_id) };
    tokio::spawn(async move {
        poll_repo(watches, repo_owned, current_run_id).await;
    });

    Ok(msg)
}

const ACTIVE_POLL_SECS: u64 = 10;
const IDLE_POLL_SECS: u64 = 600; // 10 minutes

/// Persistently watches a repo. If `current_run_id` is Some, polls that run
/// until completion. Then keeps polling `gh run list` for new runs.
async fn poll_repo(watches: Watches, repo: String, mut current_run_id: Option<u64>) {
    loop {
        let delay = if current_run_id.is_some() {
            ACTIVE_POLL_SECS
        } else {
            IDLE_POLL_SECS
        };
        tokio::time::sleep(std::time::Duration::from_secs(delay)).await;

        // Check if the watch was cancelled
        {
            let w = watches.lock().await;
            if !w.contains_key(&repo) {
                tracing::info!("Watch cancelled for {repo}");
                return;
            }
        }

        if let Some(run_id) = current_run_id {
            // Poll a specific in-progress run
            match poll_run(&watches, &repo, run_id).await {
                PollResult::StillRunning => {}
                PollResult::Completed => {
                    // Build finished — go back to idle, watch for new runs
                    current_run_id = None;
                    let mut w = watches.lock().await;
                    if let Some(entry) = w.get_mut(&repo) {
                        entry.run_id = None;
                        entry.status = "idle".to_string();
                    }
                    drop(w);
                    save_watches(&watches).await;
                }
                PollResult::Error => {}
            }
        } else {
            // Idle — check for new runs
            match check_for_new_run(&watches, &repo).await {
                Some(new_run_id) => {
                    current_run_id = Some(new_run_id);
                }
                None => {}
            }
        }
    }
}

enum PollResult {
    StillRunning,
    Completed,
    Error,
}

async fn poll_run(watches: &Watches, repo: &str, run_id: u64) -> PollResult {
    let output = match gh_run_view(repo, run_id) {
        Ok(o) => o,
        Err(e) => {
            tracing::error!("Failed to poll {repo} run {run_id}: {e}");
            return PollResult::Error;
        }
    };

    if !output.status.success() {
        tracing::error!(
            "gh error polling {repo}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        return PollResult::Error;
    }

    let run: serde_json::Value = match serde_json::from_slice(&output.stdout) {
        Ok(v) => v,
        Err(e) => {
            tracing::error!("Parse error for {repo}: {e}");
            return PollResult::Error;
        }
    };

    let status = run["status"].as_str().unwrap_or("unknown");
    let conclusion = run["conclusion"].as_str().unwrap_or("");
    let title = run["displayTitle"].as_str().unwrap_or("unknown");
    let workflow = run["workflowName"].as_str().unwrap_or("unknown");

    {
        let mut w = watches.lock().await;
        if let Some(entry) = w.get_mut(repo) {
            entry.status = status.to_string();
        }
    }
    save_watches(watches).await;

    if status == "completed" {
        let url = format!("https://github.com/{repo}/actions/runs/{run_id}");
        send_notification(
            &format!("Build {conclusion}: {repo}"),
            &format!("{workflow}: {title}"),
            conclusion == "success",
            Some(&url),
        );
        tracing::info!("Build completed for {repo}: {conclusion}");
        PollResult::Completed
    } else {
        tracing::debug!("Polling {repo} run {run_id}: {status}");
        PollResult::StillRunning
    }
}

/// Check `gh run list` for a new in-progress run. If found, update the watch entry.
async fn check_for_new_run(watches: &Watches, repo: &str) -> Option<u64> {
    let output = match gh_run_list(repo) {
        Ok(o) => o,
        Err(e) => {
            tracing::error!("Failed to check {repo} for new runs: {e}");
            return None;
        }
    };

    if !output.status.success() {
        return None;
    }

    let runs: Vec<serde_json::Value> = match serde_json::from_slice(&output.stdout) {
        Ok(v) => v,
        Err(_) => return None,
    };

    let run = runs.first()?;
    let run_id = run["databaseId"].as_u64()?;
    let status = run["status"].as_str().unwrap_or("unknown");

    if status == "completed" {
        return None;
    }

    let title = run["displayTitle"].as_str().unwrap_or("unknown");
    let workflow = run["workflowName"].as_str().unwrap_or("unknown");

    tracing::info!("New build detected for {repo}: run {run_id} ({workflow}: {title})");
    send_notification(
        &format!("Build started: {repo}"),
        &format!("{workflow}: {title}"),
        true,
        Some(&format!("https://github.com/{repo}/actions/runs/{run_id}")),
    );

    {
        let mut w = watches.lock().await;
        if let Some(entry) = w.get_mut(repo) {
            entry.run_id = Some(run_id);
            entry.status = status.to_string();
        }
    }
    save_watches(watches).await;

    Some(run_id)
}

// Resume pollers for watches loaded from disk
async fn resume_watches(watches: &Watches) {
    let snapshot = {
        let w = watches.lock().await;
        w.clone()
    };

    for (repo, entry) in snapshot {
        tracing::info!("Resuming watch for {} (run {:?}, status {})", repo, entry.run_id, entry.status);
        let watches = watches.clone();
        let current_run_id = entry.run_id;
        tokio::spawn(async move {
            poll_repo(watches, repo, current_run_id).await;
        });
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::from_default_env()
                .add_directive("build_watcher=info".parse()?)
        )
        .init();

    let port: u16 = std::env::var("BUILD_WATCHER_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(DEFAULT_PORT);

    // Load persisted watches
    let watches: Watches = Arc::new(Mutex::new(load_watches()));
    resume_watches(&watches).await;

    let ct = CancellationToken::new();
    let config = StreamableHttpServerConfig {
        stateful_mode: false,
        json_response: true,
        sse_keep_alive: None,
        cancellation_token: ct.child_token(),
        ..Default::default()
    };

    let watches_for_factory = watches.clone();
    let service: StreamableHttpService<BuildWatcher, LocalSessionManager> =
        StreamableHttpService::new(
            move || Ok(BuildWatcher::new(watches_for_factory.clone())),
            Default::default(),
            config,
        );

    let router = axum::Router::new().nest_service("/mcp", service);
    let listener = tokio::net::TcpListener::bind(format!("127.0.0.1:{port}")).await?;

    tracing::info!("build-watcher listening on http://127.0.0.1:{port}/mcp");

    axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            tokio::signal::ctrl_c().await.ok();
            tracing::info!("Shutting down...");
            ct.cancel();
        })
        .await?;

    // Save state on shutdown
    save_watches(&watches).await;
    tracing::info!("State saved, goodbye.");

    Ok(())
}
