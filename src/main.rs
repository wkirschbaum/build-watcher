mod config;
mod platform;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use config::{Config, NotificationConfig, NotificationLevel, RepoConfig, config_dir, load_config, load_json, save_config, save_json, state_dir};
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
const GH_TIMEOUT: Duration = Duration::from_secs(30);

type SharedConfig = Arc<Mutex<Config>>;

// -- Watch state persistence --

/// Watch key: "owner/repo#branch"
fn watch_key(repo: &str, branch: &str) -> String {
    format!("{repo}#{branch}")
}

fn parse_watch_key(key: &str) -> (&str, &str) {
    key.rsplit_once('#').unwrap_or((key, "main"))
}

/// Info about the last completed build.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LastBuild {
    run_id: u64,
    conclusion: String,
    workflow: String,
    title: String,
}

/// Persisted state per repo/branch.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedWatch {
    last_seen_run_id: u64,
    #[serde(default)]
    last_build: Option<LastBuild>,
}

type PersistedWatches = HashMap<String, PersistedWatch>;

fn load_watches() -> PersistedWatches {
    load_json(state_dir().join("watches.json")).unwrap_or_default()
}

fn save_persisted(watches: &PersistedWatches) {
    save_json(state_dir().join("watches.json"), watches);
}

const MAX_GH_FAILURES: u8 = 5;

/// Runtime state per repo/branch: high-water mark + in-progress runs.
#[derive(Debug, Clone)]
pub struct WatchEntry {
    last_seen_run_id: u64,
    active_runs: HashMap<u64, String>,       // run_id -> status
    failure_counts: HashMap<u64, u8>,         // run_id -> consecutive failure count
    last_build: Option<LastBuild>,
}

type Watches = Arc<Mutex<HashMap<String, WatchEntry>>>;

async fn save_watches(watches: &Watches) {
    let persisted: PersistedWatches = {
        let w = watches.lock().await;
        w.iter()
            .map(|(k, v)| (k.clone(), PersistedWatch { last_seen_run_id: v.last_seen_run_id, last_build: v.last_build.clone() }))
            .collect()
    };
    save_persisted(&persisted);
}

// -- GitHub CLI helpers --

struct RunInfo {
    id: u64,
    status: String,
    conclusion: String,
    title: String,
    workflow: String,
}

impl RunInfo {
    fn from_json(value: &serde_json::Value) -> Option<Self> {
        Some(Self {
            id: value["databaseId"].as_u64()?,
            status: value["status"].as_str().unwrap_or("unknown").to_string(),
            conclusion: value["conclusion"].as_str().unwrap_or("").to_string(),
            title: value["displayTitle"].as_str().unwrap_or("unknown").to_string(),
            workflow: value["workflowName"].as_str().unwrap_or("unknown").to_string(),
        })
    }

    fn is_completed(&self) -> bool {
        self.status == "completed"
    }

    fn succeeded(&self) -> bool {
        self.conclusion == "success"
    }

    fn url(&self, repo: &str) -> String {
        format!("https://github.com/{repo}/actions/runs/{}", self.id)
    }

    fn to_last_build(&self) -> LastBuild {
        LastBuild {
            run_id: self.id,
            conclusion: self.conclusion.clone(),
            workflow: self.workflow.clone(),
            title: self.title.clone(),
        }
    }
}

async fn gh_recent_runs(repo: &str, branch: &str) -> Result<Vec<RunInfo>, String> {
    let output = tokio::time::timeout(
        GH_TIMEOUT,
        tokio::process::Command::new("gh")
            .args([
                "run", "list", "--repo", repo, "--branch", branch, "--limit", "10", "--json",
                "databaseId,status,conclusion,displayTitle,workflowName",
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

async fn gh_run_status(repo: &str, run_id: u64) -> Result<RunInfo, String> {
    let output = tokio::time::timeout(
        GH_TIMEOUT,
        tokio::process::Command::new("gh")
            .args([
                "run", "view", &run_id.to_string(), "--repo", repo,
                "--json", "databaseId,status,conclusion,displayTitle,workflowName",
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

#[derive(Debug, Deserialize, JsonSchema)]
struct ConfigureBranchesParams {
    /// GitHub repo in "owner/repo" format
    repo: String,
    /// Branches to watch for this repo (e.g. ["main", "develop"])
    branches: Vec<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SetDefaultBranchesParams {
    /// Default branches to watch when no per-repo config exists (e.g. ["main"])
    branches: Vec<String>,
}

#[derive(Clone)]
pub struct BuildWatcher {
    tool_router: ToolRouter<Self>,
    watches: Watches,
    config: SharedConfig,
}

#[tool_router]
impl BuildWatcher {
    pub fn new(watches: Watches, config: SharedConfig) -> Self {
        Self {
            tool_router: Self::tool_router(),
            watches,
            config,
        }
    }

    #[tool(description = "Persistently watch GitHub Actions builds for one or more repos. Watches configured branches (default: main). Sends desktop notifications when builds start and complete. Repos should be in owner/repo format.")]
    async fn watch_builds(
        &self,
        Parameters(params): Parameters<WatchBuildsParams>,
    ) -> Result<CallToolResult, McpError> {
        // Collect branch info and release the config lock before making gh calls
        let repo_branches: Vec<(String, Vec<String>)> = {
            let mut config = self.config.lock().await;
            config.add_repos(&params.repos);
            save_config(&config);
            params.repos.iter().map(|repo| {
                (repo.clone(), config.branches_for(repo).to_vec())
            }).collect()
        };

        let mut results = Vec::new();
        for (repo, branches) in &repo_branches {
            for branch in branches {
                let key = watch_key(repo, branch);
                let msg = match start_watch(&self.watches, &self.config, repo, branch, &key).await {
                    Ok(msg) | Err(msg) => msg,
                };
                results.push(msg);
            }
        }

        Ok(CallToolResult::success(vec![Content::text(
            results.join("\n\n"),
        )]))
    }

    #[tool(description = "Stop watching builds for one or more repos. Stops all branches and removes from config. Repos should be in owner/repo format.")]
    async fn stop_watches(
        &self,
        Parameters(params): Parameters<StopWatchesParams>,
    ) -> Result<CallToolResult, McpError> {
        let mut watches = self.watches.lock().await;
        let mut results = Vec::new();

        for repo in &params.repos {
            let prefix = format!("{repo}#");
            let keys_to_remove: Vec<String> = watches
                .keys()
                .filter(|k| k.starts_with(&prefix))
                .cloned()
                .collect();

            if keys_to_remove.is_empty() {
                results.push(format!("No active watch for {repo}"));
            } else {
                for key in &keys_to_remove {
                    watches.remove(key);
                }
                results.push(format!("Stopped watching {repo} ({} branches)", keys_to_remove.len()));
            }
        }
        drop(watches);
        save_watches(&self.watches).await;

        let mut config = self.config.lock().await;
        config.remove_repos(&params.repos);
        save_config(&config);

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

        let mut lines: Vec<String> = watches
            .iter()
            .map(|(key, entry)| {
                let (repo, branch) = parse_watch_key(key);
                let last = entry.last_build.as_ref().map(|b| {
                    format!(" (last: {} — {}: {})", b.conclusion, b.workflow, b.title)
                }).unwrap_or_default();

                if entry.active_runs.is_empty() {
                    format!("- {repo} [{branch}] — idle{last}")
                } else {
                    let run_list: Vec<String> = entry.active_runs
                        .iter()
                        .map(|(id, status)| format!("{id} ({status})"))
                        .collect();
                    format!("- {repo} [{branch}] — {} active: {}{last}", entry.active_runs.len(), run_list.join(", "))
                }
            })
            .collect();
        lines.sort();

        Ok(CallToolResult::success(vec![Content::text(
            lines.join("\n"),
        )]))
    }

    #[tool(description = "Configure which branches to watch for a specific repo. Overrides the default branches for this repo.")]
    async fn configure_branches(
        &self,
        Parameters(params): Parameters<ConfigureBranchesParams>,
    ) -> Result<CallToolResult, McpError> {
        let mut config = self.config.lock().await;
        config.repos.insert(
            params.repo.clone(),
            RepoConfig {
                branches: params.branches.clone(),
            },
        );
        save_config(&config);

        Ok(CallToolResult::success(vec![Content::text(format!(
            "Set {}: watching branches {:?}\nRestart watches with watch_builds to apply.",
            params.repo, params.branches,
        ))]))
    }

    #[tool(description = "Set the default branches to watch for repos without per-repo config.")]
    async fn set_default_branches(
        &self,
        Parameters(params): Parameters<SetDefaultBranchesParams>,
    ) -> Result<CallToolResult, McpError> {
        let mut config = self.config.lock().await;
        config.default_branches = params.branches.clone();
        save_config(&config);

        Ok(CallToolResult::success(vec![Content::text(format!(
            "Default branches set to {:?}",
            params.branches,
        ))]))
    }

    #[tool(description = "Show the current configuration including watched repos, default branches, and per-repo overrides.")]
    async fn get_config(&self) -> Result<CallToolResult, McpError> {
        let config = self.config.lock().await;
        let mut lines = Vec::new();

        lines.push(format!("Default branches: {:?}", config.default_branches));
        lines.push(format!(
            "\nPolling:\n  active builds: every {}s\n  idle repos: every {}s",
            config.active_poll_seconds,
            config.idle_poll_seconds,
        ));
        lines.push(format!(
            "\nNotifications:\n  build_started: {}\n  build_success: {}\n  build_failure: {}",
            config.notifications.build_started,
            config.notifications.build_success,
            config.notifications.build_failure,
        ));

        let watched = config.watched_repos();
        if watched.is_empty() {
            lines.push("\nNo watched repos.".to_string());
        } else {
            lines.push("\nRepos:".to_string());
            for repo in watched {
                let rc = &config.repos[repo];
                if rc.branches.is_empty() {
                    lines.push(format!("  {repo}: (default branches)"));
                } else {
                    lines.push(format!("  {repo}: {:?}", rc.branches));
                }
            }
        }

        lines.push(format!("\nConfig file: {}", config_dir().join("config.json").display()));

        Ok(CallToolResult::success(vec![Content::text(
            lines.join("\n"),
        )]))
    }

    #[tool(description = "Send a test desktop notification to verify notifications are working")]
    async fn test_notification(&self) -> Result<CallToolResult, McpError> {
        platform::send_notification(
            "Build Watcher Test",
            "If you see this, notifications are working!",
            NotificationLevel::Normal,
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
                 Use watch_builds with one or more repos in 'owner/repo' format to start watching. \
                 Use configure_branches to set which branches to watch per repo, or \
                 set_default_branches to change the default (main). Use get_config to see current settings.",
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

async fn start_watch(
    watches: &Watches,
    config: &SharedConfig,
    repo: &str,
    branch: &str,
    key: &str,
) -> std::result::Result<String, String> {
    {
        let w = watches.lock().await;
        if w.contains_key(key) {
            return Ok(format!("{repo} [{branch}]: already being watched"));
        }
    }

    let runs = gh_recent_runs(repo, branch).await?;
    if runs.is_empty() {
        return Err(format!("{repo} [{branch}]: no workflow runs found"));
    }

    let max_id = runs.iter().map(|r| r.id).max().unwrap();
    let active: HashMap<u64, String> = runs
        .iter()
        .filter(|r| !r.is_completed())
        .map(|r| (r.id, r.status.clone()))
        .collect();

    let last_completed = runs.iter().find(|r| r.is_completed());

    let msg = if active.is_empty() {
        let latest = &runs[0]; // gh returns newest first
        format!(
            "{repo} [{branch}]: latest build already completed ({}), watching for new builds\n  {}: {}\n  {}",
            latest.conclusion, latest.workflow, latest.title, latest.url(repo)
        )
    } else {
        format!(
            "{repo} [{branch}]: watching {} active build(s)",
            active.len()
        )
    };

    let entry = WatchEntry {
        last_seen_run_id: max_id,
        active_runs: active,
        failure_counts: HashMap::new(),
        last_build: last_completed.map(|r| r.to_last_build()),
    };

    {
        let mut w = watches.lock().await;
        w.insert(key.to_string(), entry);
    }
    save_watches(watches).await;

    spawn_poller(watches.clone(), config.clone(), key.to_string());

    Ok(msg)
}

fn spawn_poller(watches: Watches, config: SharedConfig, key: String) {
    tokio::spawn(async move {
        poll_repo(watches, config, key).await;
    });
}

async fn poll_repo(watches: Watches, config: SharedConfig, key: String) {
    let (repo, branch) = parse_watch_key(&key);
    let repo = repo.to_string();
    let branch = branch.to_string();

    let mut last_new_run_check = tokio::time::Instant::now();

    loop {
        let has_active = {
            let w = watches.lock().await;
            match w.get(&key) {
                Some(entry) => !entry.active_runs.is_empty(),
                None => {
                    tracing::info!("Watch cancelled for {key}");
                    return;
                }
            }
        };

        let (active_poll_secs, idle_poll_secs, notif) = {
            let cfg = config.lock().await;
            (cfg.active_poll_seconds, cfg.idle_poll_seconds, cfg.notifications.clone())
        };

        let delay = if has_active { active_poll_secs } else { idle_poll_secs };
        tokio::time::sleep(Duration::from_secs(delay)).await;

        // Check if still watched
        {
            let w = watches.lock().await;
            if !w.contains_key(&key) {
                tracing::info!("Watch cancelled for {key}");
                return;
            }
        }

        // Poll active runs every cycle
        if has_active {
            poll_active_runs(&watches, &key, &repo, &branch, &notif).await;
        }

        // Check for new runs at the idle interval regardless of active state
        if last_new_run_check.elapsed() >= Duration::from_secs(idle_poll_secs) {
            check_for_new_runs(&watches, &key, &repo, &branch, &notif).await;
            last_new_run_check = tokio::time::Instant::now();
        }
    }
}

/// Poll all active runs for a watch. Notifies on completion and removes finished runs.
async fn poll_active_runs(
    watches: &Watches,
    key: &str,
    repo: &str,
    branch: &str,
    notif: &NotificationConfig,
) {
    let run_ids: Vec<u64> = {
        let w = watches.lock().await;
        match w.get(key) {
            Some(entry) => entry.active_runs.keys().cloned().collect(),
            None => return,
        }
    };

    let mut changed = false;

    for run_id in run_ids {
        let run = match gh_run_status(repo, run_id).await {
            Ok(r) => {
                // Reset failure count on success
                let mut w = watches.lock().await;
                if let Some(entry) = w.get_mut(key) {
                    entry.failure_counts.remove(&run_id);
                }
                r
            }
            Err(e) => {
                let mut w = watches.lock().await;
                if let Some(entry) = w.get_mut(key) {
                    let count = entry.failure_counts.entry(run_id).or_insert(0);
                    *count += 1;
                    if *count >= MAX_GH_FAILURES {
                        tracing::warn!("Removing run {run_id} from {key} after {count} consecutive failures");
                        entry.active_runs.remove(&run_id);
                        entry.failure_counts.remove(&run_id);
                        changed = true;
                    } else {
                        tracing::error!("{e} (failure {count}/{MAX_GH_FAILURES})");
                    }
                }
                continue;
            }
        };

        if run.is_completed() {
            let level = if run.succeeded() { notif.build_success } else { notif.build_failure };
            platform::send_notification(
                &format!("Build {}: {repo} [{branch}]", run.conclusion),
                &format!("{}: {}", run.workflow, run.title),
                level,
                Some(&run.url(repo)),
            );
            tracing::info!("Build completed for {key} run {run_id}: {}", run.conclusion);

            let mut w = watches.lock().await;
            if let Some(entry) = w.get_mut(key) {
                entry.active_runs.remove(&run_id);
                entry.last_build = Some(run.to_last_build());
            }
            changed = true;
        } else {
            // Update status if changed
            let mut w = watches.lock().await;
            if let Some(entry) = w.get_mut(key) {
                if let Some(old_status) = entry.active_runs.get(&run_id) {
                    if *old_status != run.status {
                        tracing::debug!("Run {run_id} status changed: {} -> {}", old_status, run.status);
                        entry.active_runs.insert(run_id, run.status);
                    }
                }
            }
        }
    }

    if changed {
        save_watches(watches).await;
    }
}

/// Check for new runs we haven't seen yet. Notify on new starts, track in-progress ones.
async fn check_for_new_runs(
    watches: &Watches,
    key: &str,
    repo: &str,
    branch: &str,
    notif: &NotificationConfig,
) {
    let last_seen = {
        let w = watches.lock().await;
        match w.get(key) {
            Some(entry) => entry.last_seen_run_id,
            None => return,
        }
    };

    let runs = match gh_recent_runs(repo, branch).await {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("{e}");
            return;
        }
    };

    let new_runs: Vec<&RunInfo> = runs.iter().filter(|r| r.id > last_seen).collect();
    if new_runs.is_empty() {
        return;
    }

    let new_max = new_runs.iter().map(|r| r.id).max().unwrap();

    for run in &new_runs {
        tracing::info!("New build detected for {key}: run {} ({}: {})", run.id, run.workflow, run.title);
        platform::send_notification(
            &format!("Build started: {repo} [{branch}]"),
            &format!("{}: {}", run.workflow, run.title),
            notif.build_started,
            Some(&run.url(repo)),
        );

        // If it already completed between polls, also notify completion
        if run.is_completed() {
            let level = if run.succeeded() { notif.build_success } else { notif.build_failure };
            platform::send_notification(
                &format!("Build {}: {repo} [{branch}]", run.conclusion),
                &format!("{}: {}", run.workflow, run.title),
                level,
                Some(&run.url(repo)),
            );
            tracing::info!("Build already completed for {key} run {}: {}", run.id, run.conclusion);
        }
    }

    // Update state
    let mut w = watches.lock().await;
    if let Some(entry) = w.get_mut(key) {
        entry.last_seen_run_id = new_max;
        // Track new in-progress runs, record completed ones
        for run in &new_runs {
            if run.is_completed() {
                entry.last_build = Some(run.to_last_build());
            } else {
                entry.active_runs.insert(run.id, run.status.clone());
            }
        }
    }
    drop(w);
    save_watches(watches).await;
}

async fn startup_watches(watches: &Watches, config: &SharedConfig) {
    // Resume existing watches — recover any in-progress builds that were active at shutdown
    let snapshot: Vec<String> = {
        let w = watches.lock().await;
        w.keys().cloned().collect()
    };
    for key in &snapshot {
        let (repo, branch) = parse_watch_key(key);
        tracing::info!("Resuming watch for {key}");

        // Scan for in-progress runs we may have missed during downtime
        match gh_recent_runs(repo, branch).await {
            Ok(runs) => {
                let mut w = watches.lock().await;
                if let Some(entry) = w.get_mut(key) {
                    for run in &runs {
                        if !run.is_completed() && !entry.active_runs.contains_key(&run.id) {
                            tracing::info!("Recovering in-progress run {} for {key}", run.id);
                            entry.active_runs.insert(run.id, run.status.clone());
                        }
                    }
                }
            }
            Err(e) => tracing::warn!("Could not recover runs for {key}: {e}"),
        }

        spawn_poller(watches.clone(), config.clone(), key.clone());
    }

    // Start watches for any config repos not already in state
    let new_watches: Vec<(String, String, String)> = {
        let cfg = config.lock().await;
        let mut result = Vec::new();
        for repo in cfg.watched_repos() {
            for branch in cfg.branches_for(repo) {
                let key = watch_key(repo, branch);
                if !snapshot.contains(&key) {
                    result.push((repo.clone(), branch.clone(), key));
                }
            }
        }
        result
    };

    for (repo, branch, key) in &new_watches {
        tracing::info!("Starting new watch from config: {repo} [{branch}]");
        match start_watch(watches, config, repo, branch, key).await {
            Ok(msg) | Err(msg) => tracing::info!("{msg}"),
        }
    }
}

// -- Main --

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::from_default_env()
                .add_directive("build_watcher=info".parse()?),
        )
        .init();

    let port: u16 = std::env::var("BUILD_WATCHER_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(DEFAULT_PORT);

    let mut cfg = load_config();
    let persisted = load_watches();

    // Migrate repos from watches.json into config on first run
    let watch_keys: Vec<String> = persisted.keys().cloned().collect();
    cfg.migrate_from_watches(&watch_keys);

    // Re-save config on startup to normalize schema (adds missing fields with defaults)
    save_config(&cfg);

    // Convert persisted state to runtime state (active_runs start empty, rediscovered by poller)
    let watch_state: HashMap<String, WatchEntry> = persisted
        .into_iter()
        .map(|(k, v)| (k, WatchEntry {
            last_seen_run_id: v.last_seen_run_id,
            active_runs: HashMap::new(),
            failure_counts: HashMap::new(),
            last_build: v.last_build,
        }))
        .collect();

    let config: SharedConfig = Arc::new(Mutex::new(cfg));
    let watches: Watches = Arc::new(Mutex::new(watch_state));

    // Auto-watch all repos from config (resumes existing, starts new)
    startup_watches(&watches, &config).await;

    let ct = CancellationToken::new();
    let http_config = StreamableHttpServerConfig {
        stateful_mode: false,
        json_response: true,
        sse_keep_alive: None,
        cancellation_token: ct.child_token(),
        ..Default::default()
    };

    let watches_for_factory = watches.clone();
    let config_for_factory = config.clone();
    let service: StreamableHttpService<BuildWatcher, LocalSessionManager> =
        StreamableHttpService::new(
            move || Ok(BuildWatcher::new(watches_for_factory.clone(), config_for_factory.clone())),
            Default::default(),
            http_config,
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

    save_watches(&watches).await;
    tracing::info!("State saved, goodbye.");

    Ok(())
}
