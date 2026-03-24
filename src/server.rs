use anyhow::Result;
use rmcp::handler::server::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Content, Implementation, ServerCapabilities, ServerInfo};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use rmcp::{ErrorData as McpError, ServerHandler, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use crate::config::{
    NotificationLevel, NotificationOverrides, PersistError, RepoConfig, config_dir, save_config,
    state_dir,
};
use crate::github::{gh_run_list_history, gh_run_rerun, validate_branch, validate_repo};
use crate::watcher::{
    PauseState, SharedConfig, WatcherHandle, Watches, last_failed_build, parse_watch_key,
    save_watches, start_watch, watch_key,
};

const DEFAULT_PORT: u16 = 8417;

/// Bind to the preferred port, trying up to 9 consecutive ports on conflict.
async fn bind_with_fallback(preferred: u16) -> Result<tokio::net::TcpListener> {
    for port in preferred..=preferred.saturating_add(9) {
        match tokio::net::TcpListener::bind(format!("127.0.0.1:{port}")).await {
            Ok(l) => return Ok(l),
            Err(_) if port < preferred.saturating_add(9) => continue,
            Err(e) => return Err(e.into()),
        }
    }
    unreachable!()
}

/// Build the axum router with the MCP StreamableHttpService.
fn build_router(
    watches: Watches,
    config: SharedConfig,
    handle: WatcherHandle,
    pause: PauseState,
    ct: &CancellationToken,
) -> axum::Router {
    let http_config = StreamableHttpServerConfig {
        stateful_mode: false,
        json_response: true,
        sse_keep_alive: None,
        cancellation_token: ct.child_token(),
        ..Default::default()
    };

    let service: StreamableHttpService<BuildWatcher, LocalSessionManager> =
        StreamableHttpService::new(
            move || {
                Ok(BuildWatcher::new(
                    watches.clone(),
                    config.clone(),
                    handle.clone(),
                    pause.clone(),
                ))
            },
            Default::default(),
            http_config,
        );

    axum::Router::new().nest_service("/mcp", service)
}

/// Run the MCP HTTP server with graceful shutdown.
///
/// Binds to the configured port, writes a port-discovery file, serves until
/// ctrl-c, then shuts down pollers and persists state.
pub async fn serve(
    watches: Watches,
    config: SharedConfig,
    handle: WatcherHandle,
    pause: PauseState,
    ct: CancellationToken,
) -> Result<()> {
    let port: u16 = std::env::var("BUILD_WATCHER_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(DEFAULT_PORT);

    let router = build_router(watches.clone(), config, handle.clone(), pause, &ct);
    let listener = bind_with_fallback(port).await?;
    let bound_port = listener.local_addr()?.port();

    let port_file = state_dir().join("port");
    if let Err(e) = std::fs::write(&port_file, bound_port.to_string()) {
        tracing::warn!("Failed to write port file {}: {e}", port_file.display());
    }

    if bound_port != port {
        tracing::warn!("Port {port} was occupied, using port {bound_port} instead");
        tracing::warn!("Re-run install.sh to update the MCP URL in ~/.claude.json");
    }
    tracing::info!("build-watcher listening on http://127.0.0.1:{bound_port}/mcp");

    axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            tokio::signal::ctrl_c().await.ok();
            tracing::info!("Shutting down...");
            ct.cancel();
        })
        .await?;

    handle.shutdown().await;
    save_watches(&watches).await;
    let _ = std::fs::remove_file(&port_file);
    tracing::info!("State saved, goodbye.");

    Ok(())
}

fn persist_warning(result: Result<(), PersistError>) -> Option<String> {
    match result {
        Ok(()) => None,
        Err(e) => {
            tracing::error!("Failed to save config: {e}");
            Some(format!(
                "\n⚠️ Warning: config could not be saved to disk: {e}"
            ))
        }
    }
}

/// Deserialize a `Vec<String>` that may arrive as either a proper JSON array
/// or as a JSON-encoded string (e.g. `"[\"a\",\"b\"]"`). Some MCP clients
/// double-encode array parameters; this handles both forms transparently.
fn deserialize_string_or_vec<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de;

    struct StringOrVec;

    impl<'de> de::Visitor<'de> for StringOrVec {
        type Value = Vec<String>;

        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("a string array or a JSON-encoded string array")
        }

        fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
            serde_json::from_str(v).map_err(de::Error::custom)
        }

        fn visit_seq<A: de::SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
            let mut vec = Vec::new();
            while let Some(item) = seq.next_element()? {
                vec.push(item);
            }
            Ok(vec)
        }
    }

    deserializer.deserialize_any(StringOrVec)
}

#[derive(Debug, Deserialize, JsonSchema)]
struct WatchBuildsParams {
    /// List of GitHub repos in "owner/repo" format
    #[serde(deserialize_with = "deserialize_string_or_vec")]
    repos: Vec<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct StopWatchesParams {
    /// List of GitHub repos in "owner/repo" format
    #[serde(deserialize_with = "deserialize_string_or_vec")]
    repos: Vec<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ConfigureBranchesParams {
    /// GitHub repo in "owner/repo" format
    repo: String,
    /// Branches to watch for this repo (e.g. ["main", "develop"])
    #[serde(deserialize_with = "deserialize_string_or_vec")]
    branches: Vec<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SetDefaultBranchesParams {
    /// Default branches to watch when no per-repo config exists (e.g. ["main"])
    #[serde(deserialize_with = "deserialize_string_or_vec")]
    branches: Vec<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ConfigureNotificationsParams {
    /// Optional: GitHub repo in "owner/repo" format. If omitted, sets global defaults.
    repo: Option<String>,
    /// Optional: branch name. Requires repo. If omitted with repo, sets repo-level defaults.
    branch: Option<String>,
    /// Notification level for build started events (off, low, normal, critical)
    build_started: Option<NotificationLevel>,
    /// Notification level for build success events (off, low, normal, critical)
    build_success: Option<NotificationLevel>,
    /// Notification level for build failure events (off, low, normal, critical)
    build_failure: Option<NotificationLevel>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ConfigureWorkflowsParams {
    /// GitHub repo in "owner/repo" format
    repo: String,
    /// Workflow names to watch (e.g. ["CI", "Deploy"]). Empty list means all workflows.
    #[serde(deserialize_with = "deserialize_string_or_vec")]
    workflows: Vec<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct IgnoreWorkflowsParams {
    /// Workflow names to ignore globally (e.g. ["Semgrep", "Dependabot"])
    #[serde(deserialize_with = "deserialize_string_or_vec")]
    workflows: Vec<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct UnignoreWorkflowsParams {
    /// Workflow names to stop ignoring
    #[serde(deserialize_with = "deserialize_string_or_vec")]
    workflows: Vec<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct PauseNotificationsParams {
    /// Minutes to pause notifications. Omit or 0 to pause until restart.
    minutes: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ConfigureSoundParams {
    /// Enable or disable failure sound globally
    enabled: Option<bool>,
    /// Custom sound file path (absolute). Omit to use system default.
    sound_file: Option<String>,
    /// Optional repo to override sound setting per-repo
    repo: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct RerunBuildParams {
    /// GitHub repo in "owner/repo" format
    repo: String,
    /// Run ID to rerun. Omit to rerun the last failed build.
    run_id: Option<u64>,
    /// If true, only rerun failed jobs within the run (default: false)
    #[serde(default)]
    failed_only: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct BuildHistoryParams {
    /// GitHub repo in "owner/repo" format
    repo: String,
    /// Optional branch filter. If omitted, shows all branches.
    branch: Option<String>,
    /// Number of builds to show (default: 10, max: 50)
    limit: Option<u32>,
}

#[derive(Clone)]
pub struct BuildWatcher {
    tool_router: ToolRouter<Self>,
    watches: Watches,
    config: SharedConfig,
    handle: WatcherHandle,
    pause: PauseState,
}

#[tool_router]
impl BuildWatcher {
    pub(crate) fn new(
        watches: Watches,
        config: SharedConfig,
        handle: WatcherHandle,
        pause: PauseState,
    ) -> Self {
        Self {
            tool_router: Self::tool_router(),
            watches,
            config,
            handle,
            pause,
        }
    }

    #[tool(
        description = "Persistently watch GitHub Actions builds for one or more repos. Watches configured branches (default: main). Sends desktop notifications when builds start and complete. Repos should be in owner/repo format."
    )]
    async fn watch_builds(
        &self,
        Parameters(params): Parameters<WatchBuildsParams>,
    ) -> Result<CallToolResult, McpError> {
        // Validate all repo names upfront
        for repo in &params.repos {
            if let Err(e) = validate_repo(repo) {
                return Ok(CallToolResult::error(vec![Content::text(e)]));
            }
        }

        // Read branch config without modifying it yet. We only add a repo to the
        // persisted config after at least one branch successfully starts — this way a
        // typo'd repo name (or a repo with no workflow runs) doesn't end up permanently
        // in config, which would cause failed retries on every daemon restart.
        let repo_branches: Vec<(String, Vec<String>)> = {
            let config = self.config.lock().await;
            params
                .repos
                .iter()
                .map(|repo| (repo.clone(), config.branches_for(repo).to_vec()))
                .collect()
        };

        let mut results = Vec::new();
        let mut started_repos: Vec<String> = Vec::new();
        for (repo, branches) in &repo_branches {
            let mut any_started = false;
            for branch in branches {
                let key = watch_key(repo, branch);
                match start_watch(
                    &self.watches,
                    &self.config,
                    &self.handle,
                    &self.pause,
                    repo,
                    branch,
                    &key,
                )
                .await
                {
                    Ok(msg) => {
                        any_started = true;
                        results.push(msg);
                    }
                    Err(msg) => results.push(msg),
                }
            }
            if any_started {
                started_repos.push(repo.clone());
            }
        }

        // Persist only the repos that actually got a poller running. Repos whose
        // every branch failed (e.g. no runs, bad name) are not saved.
        if !started_repos.is_empty() {
            let mut config = self.config.lock().await;
            config.add_repos(&started_repos);
            if let Some(warning) = persist_warning(save_config(&config)) {
                results.push(warning);
            }
        }

        Ok(CallToolResult::success(vec![Content::text(
            results.join("\n\n"),
        )]))
    }

    #[tool(
        description = "Stop watching builds for one or more repos. Stops all branches and removes from config. Repos should be in owner/repo format."
    )]
    async fn stop_watches(
        &self,
        Parameters(params): Parameters<StopWatchesParams>,
    ) -> Result<CallToolResult, McpError> {
        // Phase 1: remove from the runtime watch map. The polling tasks detect the
        // missing key on their next iteration and exit cleanly — no explicit
        // cancellation is needed.
        let removed_counts: Vec<(String, usize)> = {
            let mut watches = self.watches.lock().await;
            params
                .repos
                .iter()
                .map(|repo| {
                    let prefix = format!("{repo}#");
                    let keys: Vec<String> = watches
                        .keys()
                        .filter(|k| k.starts_with(&prefix))
                        .cloned()
                        .collect();
                    for key in &keys {
                        watches.remove(key);
                    }
                    (repo.clone(), keys.len())
                })
                .collect()
        };
        save_watches(&self.watches).await;

        // Phase 2: remove from config. We check both sources of truth here so the
        // response message is accurate — a repo can be in config but have no active
        // poller if a previous watch_builds call failed partway through.
        let mut config = self.config.lock().await;
        let mut results = Vec::new();
        for (repo, branch_count) in removed_counts {
            let was_in_config = config.repos.contains_key(&repo);
            config.repos.remove(&repo);
            let msg = match (branch_count, was_in_config) {
                (n, _) if n > 0 => format!("Stopped watching {repo} ({n} branches)"),
                (_, true) => format!("{repo}: removed from config (was not actively polling)"),
                _ => format!("{repo}: not found"),
            };
            results.push(msg);
        }
        if let Some(warning) = persist_warning(save_config(&config)) {
            results.push(warning);
        }

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

        let paused = {
            let p = self.pause.lock().await;
            p.is_some_and(|deadline| tokio::time::Instant::now() < deadline)
        };

        let mut lines: Vec<String> = Vec::new();
        if paused {
            lines.push("⏸ Notifications paused\n".to_string());
        }

        let mut watch_lines: Vec<String> = watches
            .iter()
            .map(|(key, entry)| {
                let (repo, branch) = parse_watch_key(key);
                let last = entry
                    .last_build
                    .as_ref()
                    .map(|b| {
                        format!(
                            " (last: {} — {}: {})",
                            b.conclusion,
                            b.workflow,
                            b.display_title()
                        )
                    })
                    .unwrap_or_default();

                if entry.active_runs.is_empty() {
                    format!("- {repo} [{branch}] — idle{last}")
                } else {
                    let run_list: Vec<String> = entry
                        .active_runs
                        .iter()
                        .map(|(id, active)| {
                            let elapsed = active.started_at.elapsed();
                            let secs = elapsed.as_secs();
                            let time = if secs < 60 {
                                format!("{secs}s")
                            } else {
                                format!("{}m {}s", secs / 60, secs % 60)
                            };
                            format!("{id} ({}, {time})", active.status)
                        })
                        .collect();
                    format!(
                        "- {repo} [{branch}] — {} active: {}{last}",
                        entry.active_runs.len(),
                        run_list.join(", ")
                    )
                }
            })
            .collect();
        watch_lines.sort();
        lines.extend(watch_lines);

        Ok(CallToolResult::success(vec![Content::text(
            lines.join("\n"),
        )]))
    }

    #[tool(
        description = "Configure which branches to watch for a specific repo. Overrides the default branches for this repo."
    )]
    async fn configure_branches(
        &self,
        Parameters(params): Parameters<ConfigureBranchesParams>,
    ) -> Result<CallToolResult, McpError> {
        if let Err(e) = validate_repo(&params.repo) {
            return Ok(CallToolResult::error(vec![Content::text(e)]));
        }
        for branch in &params.branches {
            if let Err(e) = validate_branch(branch) {
                return Ok(CallToolResult::error(vec![Content::text(e)]));
            }
        }

        let mut config = self.config.lock().await;
        let Some(existing) = config.repos.get(&params.repo).cloned() else {
            return Ok(CallToolResult::error(vec![Content::text(format!(
                "{} is not being watched — use watch_builds first",
                params.repo
            ))]));
        };
        config.repos.insert(
            params.repo.clone(),
            RepoConfig {
                branches: params.branches.clone(),
                workflows: existing.workflows,
                sound_on_failure: existing.sound_on_failure,
                notifications: existing.notifications,
                branch_notifications: existing.branch_notifications,
            },
        );
        let mut msg = format!(
            "Set {}: watching branches {:?}\nRestart watches with watch_builds to apply.",
            params.repo, params.branches,
        );
        if let Some(warning) = persist_warning(save_config(&config)) {
            msg.push_str(&warning);
        }

        Ok(CallToolResult::success(vec![Content::text(msg)]))
    }

    #[tool(description = "Set the default branches to watch for repos without per-repo config.")]
    async fn set_default_branches(
        &self,
        Parameters(params): Parameters<SetDefaultBranchesParams>,
    ) -> Result<CallToolResult, McpError> {
        for branch in &params.branches {
            if let Err(e) = validate_branch(branch) {
                return Ok(CallToolResult::error(vec![Content::text(e)]));
            }
        }

        let mut config = self.config.lock().await;
        config.default_branches = params.branches;
        let mut msg = format!("Default branches set to {:?}", config.default_branches);
        if let Some(warning) = persist_warning(save_config(&config)) {
            msg.push_str(&warning);
        }

        Ok(CallToolResult::success(vec![Content::text(msg)]))
    }

    #[tool(
        description = "Show the current configuration including watched repos, default branches, and per-repo overrides."
    )]
    async fn get_config(&self) -> Result<CallToolResult, McpError> {
        let config = self.config.lock().await;
        let mut lines = Vec::new();

        // Pause status
        let paused = {
            let p = self.pause.lock().await;
            p.is_some_and(|deadline| tokio::time::Instant::now() < deadline)
        };
        if paused {
            lines.push("⏸ Notifications: PAUSED".to_string());
        }

        lines.push(format!("Default branches: {:?}", config.default_branches));
        lines.push(format!(
            "\nPolling:\n  active builds: every {}s\n  idle repos: every {}s",
            config.active_poll_seconds, config.idle_poll_seconds,
        ));
        lines.push(format!(
            "\nNotifications:\n  build_started: {}\n  build_success: {}\n  build_failure: {}",
            config.notifications.build_started,
            config.notifications.build_success,
            config.notifications.build_failure,
        ));

        lines.push(format!(
            "\nSound on failure: {}{}",
            if config.sound_on_failure.enabled {
                "enabled"
            } else {
                "disabled"
            },
            config
                .sound_on_failure
                .sound_file
                .as_ref()
                .map(|p| format!(" ({p})"))
                .unwrap_or_default()
        ));

        if !config.ignored_workflows.is_empty() {
            lines.push(format!(
                "\nIgnored workflows: {:?}",
                config.ignored_workflows
            ));
        }

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
                if !rc.workflows.is_empty() {
                    lines.push(format!("    workflows: {:?}", rc.workflows));
                }
                if !rc.notifications.is_empty() {
                    lines.push(format!(
                        "    notifications: {}",
                        format_notification_overrides(&rc.notifications)
                    ));
                }
                for (branch, bc) in &rc.branch_notifications {
                    if !bc.notifications.is_empty() {
                        lines.push(format!(
                            "    [{branch}] notifications: {}",
                            format_notification_overrides(&bc.notifications)
                        ));
                    }
                }
            }
        }

        lines.push(format!(
            "\nConfig file: {}",
            config_dir().join("config.json").display()
        ));

        Ok(CallToolResult::success(vec![Content::text(
            lines.join("\n"),
        )]))
    }

    #[tool(description = "Send a test desktop notification to verify notifications are working")]
    async fn test_notification(&self) -> Result<CallToolResult, McpError> {
        crate::platform::send_notification(
            "🔔 Build Watcher Test",
            "If you see this, notifications are working!",
            NotificationLevel::Normal,
            None,
            None,
        )
        .await;
        Ok(CallToolResult::success(vec![Content::text(
            "Test notification sent. You should see it on your desktop.",
        )]))
    }

    #[tool(
        description = "Configure which workflows to watch for a repo. Only matching workflow names will trigger notifications. Empty list means all workflows (default). Matching is case-insensitive."
    )]
    async fn configure_workflows(
        &self,
        Parameters(params): Parameters<ConfigureWorkflowsParams>,
    ) -> Result<CallToolResult, McpError> {
        if let Err(e) = validate_repo(&params.repo) {
            return Ok(CallToolResult::error(vec![Content::text(e)]));
        }

        let mut config = self.config.lock().await;
        let Some(rc) = config.repos.get_mut(&params.repo) else {
            return Ok(CallToolResult::error(vec![Content::text(format!(
                "{} is not being watched — use watch_builds first",
                params.repo
            ))]));
        };
        rc.workflows = params.workflows.clone();
        let mut msg = if params.workflows.is_empty() {
            format!("{}: watching all workflows", params.repo)
        } else {
            format!(
                "{}: watching workflows {:?}\nApplies to new builds immediately.",
                params.repo, params.workflows
            )
        };
        if let Some(warning) = persist_warning(save_config(&config)) {
            msg.push_str(&warning);
        }

        Ok(CallToolResult::success(vec![Content::text(msg)]))
    }

    #[tool(
        description = "Globally ignore workflows by name across all repos. Ignored workflows are never tracked or notified. Case-insensitive. Example: ignore Semgrep, Dependabot."
    )]
    async fn ignore_workflows(
        &self,
        Parameters(params): Parameters<IgnoreWorkflowsParams>,
    ) -> Result<CallToolResult, McpError> {
        if params.workflows.is_empty() {
            return Ok(CallToolResult::error(vec![Content::text(
                "At least one workflow name is required",
            )]));
        }

        let mut config = self.config.lock().await;
        let mut added = Vec::new();
        for w in &params.workflows {
            if !config
                .ignored_workflows
                .iter()
                .any(|existing| existing.eq_ignore_ascii_case(w))
            {
                config.ignored_workflows.push(w.clone());
                added.push(w.as_str());
            }
        }
        let mut msg = if added.is_empty() {
            "All specified workflows are already ignored".to_string()
        } else {
            format!(
                "Now ignoring: {}\nIgnored workflows: {:?}",
                added.join(", "),
                config.ignored_workflows
            )
        };
        if let Some(warning) = persist_warning(save_config(&config)) {
            msg.push_str(&warning);
        }

        Ok(CallToolResult::success(vec![Content::text(msg)]))
    }

    #[tool(
        description = "Stop ignoring workflows. Removes them from the global ignore list so they are tracked and notified again."
    )]
    async fn unignore_workflows(
        &self,
        Parameters(params): Parameters<UnignoreWorkflowsParams>,
    ) -> Result<CallToolResult, McpError> {
        if params.workflows.is_empty() {
            return Ok(CallToolResult::error(vec![Content::text(
                "At least one workflow name is required",
            )]));
        }

        let mut config = self.config.lock().await;
        let before = config.ignored_workflows.len();
        config.ignored_workflows.retain(|existing| {
            !params
                .workflows
                .iter()
                .any(|w| w.eq_ignore_ascii_case(existing))
        });
        let removed = before - config.ignored_workflows.len();
        let mut msg = if removed == 0 {
            "None of the specified workflows were in the ignore list".to_string()
        } else if config.ignored_workflows.is_empty() {
            format!("Removed {removed} workflow(s) from ignore list. No workflows are ignored now.")
        } else {
            format!(
                "Removed {removed} workflow(s). Still ignoring: {:?}",
                config.ignored_workflows
            )
        };
        if let Some(warning) = persist_warning(save_config(&config)) {
            msg.push_str(&warning);
        }

        Ok(CallToolResult::success(vec![Content::text(msg)]))
    }

    #[tool(
        description = "Temporarily pause all desktop notifications. Specify minutes to auto-resume, or omit for indefinite (until resume_notifications or restart). Builds are still tracked — only notifications are suppressed."
    )]
    async fn pause_notifications(
        &self,
        Parameters(params): Parameters<PauseNotificationsParams>,
    ) -> Result<CallToolResult, McpError> {
        let mut p = self.pause.lock().await;
        let msg = match params.minutes {
            Some(mins) if mins > 0 => {
                *p = Some(tokio::time::Instant::now() + std::time::Duration::from_secs(mins * 60));
                format!("Notifications paused for {mins} minutes")
            }
            _ => {
                // Effectively forever (136 years)
                *p = Some(
                    tokio::time::Instant::now() + std::time::Duration::from_secs(u32::MAX as u64),
                );
                "Notifications paused until resume or restart".to_string()
            }
        };

        Ok(CallToolResult::success(vec![Content::text(msg)]))
    }

    #[tool(description = "Resume desktop notifications after a pause")]
    async fn resume_notifications(&self) -> Result<CallToolResult, McpError> {
        let mut p = self.pause.lock().await;
        let was_paused = p.is_some_and(|deadline| tokio::time::Instant::now() < deadline);
        *p = None;
        let msg = if was_paused {
            "Notifications resumed"
        } else {
            "Notifications were not paused"
        };
        Ok(CallToolResult::success(vec![Content::text(msg)]))
    }

    #[tool(
        description = "Configure the sound-on-failure feature. Disabled by default. When enabled, plays an audio alert when builds fail. Set globally or per-repo."
    )]
    async fn configure_sound(
        &self,
        Parameters(params): Parameters<ConfigureSoundParams>,
    ) -> Result<CallToolResult, McpError> {
        if let Some(repo) = &params.repo
            && let Err(e) = validate_repo(repo)
        {
            return Ok(CallToolResult::error(vec![Content::text(e)]));
        }

        if params.repo.is_some() && params.sound_file.is_some() {
            return Ok(CallToolResult::error(vec![Content::text(
                "sound_file can only be set globally, not per-repo",
            )]));
        }

        let mut config = self.config.lock().await;
        let mut msg = String::new();

        if let Some(repo) = &params.repo {
            let Some(rc) = config.repos.get_mut(repo) else {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "{repo} is not being watched — use watch_builds first"
                ))]));
            };
            if let Some(enabled) = params.enabled {
                rc.sound_on_failure = Some(enabled);
                msg = format!(
                    "{repo}: sound on failure {}",
                    if enabled { "enabled" } else { "disabled" }
                );
            }
        } else {
            if let Some(enabled) = params.enabled {
                config.sound_on_failure.enabled = enabled;
                msg = format!(
                    "Sound on failure globally {}",
                    if enabled { "enabled" } else { "disabled" }
                );
            }
            if let Some(path) = &params.sound_file {
                config.sound_on_failure.sound_file = if path.is_empty() {
                    None
                } else {
                    Some(path.clone())
                };
                if !msg.is_empty() {
                    msg.push('\n');
                }
                msg.push_str(&format!(
                    "Sound file: {}",
                    config
                        .sound_on_failure
                        .sound_file
                        .as_deref()
                        .unwrap_or("(system default)")
                ));
            }
        }

        if msg.is_empty() {
            msg = format!(
                "Sound on failure: {}\nSound file: {}",
                if config.sound_on_failure.enabled {
                    "enabled"
                } else {
                    "disabled"
                },
                config
                    .sound_on_failure
                    .sound_file
                    .as_deref()
                    .unwrap_or("(system default)")
            );
        }
        if let Some(warning) = persist_warning(save_config(&config)) {
            msg.push_str(&warning);
        }

        Ok(CallToolResult::success(vec![Content::text(msg)]))
    }

    #[tool(
        description = "Rerun a GitHub Actions build. Specify a run_id, or omit to rerun the last failed build for the repo. Set failed_only to only rerun failed jobs."
    )]
    async fn rerun_build(
        &self,
        Parameters(params): Parameters<RerunBuildParams>,
    ) -> Result<CallToolResult, McpError> {
        if let Err(e) = validate_repo(&params.repo) {
            return Ok(CallToolResult::error(vec![Content::text(e)]));
        }

        let run_id = if let Some(id) = params.run_id {
            id
        } else {
            let watches = self.watches.lock().await;
            match last_failed_build(&watches, &params.repo) {
                Some((key, build)) => {
                    let (_, branch) = parse_watch_key(&key);
                    tracing::info!(
                        repo = params.repo,
                        branch,
                        run_id = build.run_id,
                        "Rerunning last failed build"
                    );
                    build.run_id
                }
                None => {
                    return Ok(CallToolResult::error(vec![Content::text(format!(
                        "No recent failed build found for {}",
                        params.repo
                    ))]));
                }
            }
        };

        match gh_run_rerun(&params.repo, run_id, params.failed_only).await {
            Ok(_) => {
                let url = format!("https://github.com/{}/actions/runs/{run_id}", params.repo);
                let kind = if params.failed_only {
                    "failed jobs"
                } else {
                    "all jobs"
                };
                Ok(CallToolResult::success(vec![Content::text(format!(
                    "Rerunning {kind} for run {run_id}\n{url}"
                ))]))
            }
            Err(e) => Ok(CallToolResult::error(vec![Content::text(e.to_string())])),
        }
    }

    #[tool(
        description = "Show recent build history for a repo. Displays conclusion, workflow, title, duration, and age. Optionally filter by branch."
    )]
    async fn build_history(
        &self,
        Parameters(params): Parameters<BuildHistoryParams>,
    ) -> Result<CallToolResult, McpError> {
        if let Err(e) = validate_repo(&params.repo) {
            return Ok(CallToolResult::error(vec![Content::text(e)]));
        }
        if let Some(branch) = &params.branch
            && let Err(e) = validate_branch(branch)
        {
            return Ok(CallToolResult::error(vec![Content::text(e)]));
        }

        let limit = params.limit.unwrap_or(10).min(50);
        let entries = match gh_run_list_history(&params.repo, params.branch.as_deref(), limit).await
        {
            Ok(e) => e,
            Err(e) => return Ok(CallToolResult::error(vec![Content::text(e.to_string())])),
        };

        if entries.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "No builds found",
            )]));
        }

        let distinct_branches = entries
            .iter()
            .map(|e| &e.branch)
            .collect::<std::collections::HashSet<_>>()
            .len();
        let show_branch = params.branch.is_none() && distinct_branches > 1;
        let mut lines = Vec::new();

        if show_branch {
            lines.push(format!(
                "{:<12} {:<15} {:<20} {:<30} {:<10} {}",
                "Conclusion", "Branch", "Workflow", "Title", "Duration", "When"
            ));
            lines.push(format!(
                "{:<12} {:<15} {:<20} {:<30} {:<10} {}",
                "───────────",
                "───────────────",
                "────────────────────",
                "──────────────────────────────",
                "──────────",
                "─────"
            ));
        } else {
            lines.push(format!(
                "{:<12} {:<20} {:<35} {:<10} {}",
                "Conclusion", "Workflow", "Title", "Duration", "When"
            ));
            lines.push(format!(
                "{:<12} {:<20} {:<35} {:<10} {}",
                "───────────",
                "────────────────────",
                "───────────────────────────────────",
                "──────────",
                "─────"
            ));
        }

        for entry in &entries {
            let duration = entry
                .duration_secs()
                .map(format_secs)
                .unwrap_or_else(|| "—".to_string());
            let age = entry
                .age_secs()
                .map(format_age)
                .unwrap_or_else(|| "—".to_string());
            let title = entry.display_title();

            if show_branch {
                lines.push(format!(
                    "{:<12} {:<15} {:<20} {:<30} {:<10} {}",
                    entry.conclusion,
                    truncate(&entry.branch, 13),
                    truncate(&entry.workflow, 18),
                    truncate(&title, 28),
                    duration,
                    age,
                ));
            } else {
                lines.push(format!(
                    "{:<12} {:<20} {:<35} {:<10} {}",
                    entry.conclusion,
                    truncate(&entry.workflow, 18),
                    truncate(&title, 33),
                    duration,
                    age,
                ));
            }
        }

        Ok(CallToolResult::success(vec![Content::text(
            lines.join("\n"),
        )]))
    }

    #[tool(
        description = "Configure notification levels. Scope depends on which params are set: global (no repo/branch), per-repo (repo only), or per-branch (repo + branch). Only the events you specify are changed; others keep their current value. Levels: off, low, normal, critical. Examples: 'only notify me on failure for build-watcher' or 'on the release branch, only notify on success'."
    )]
    async fn configure_notifications(
        &self,
        Parameters(params): Parameters<ConfigureNotificationsParams>,
    ) -> Result<CallToolResult, McpError> {
        if params.branch.is_some() && params.repo.is_none() {
            return Ok(CallToolResult::error(vec![Content::text(
                "branch requires repo to be set",
            )]));
        }

        if params.build_started.is_none()
            && params.build_success.is_none()
            && params.build_failure.is_none()
        {
            return Ok(CallToolResult::error(vec![Content::text(
                "at least one of build_started, build_success, or build_failure must be set",
            )]));
        }

        if let Some(repo) = &params.repo
            && let Err(e) = validate_repo(repo)
        {
            return Ok(CallToolResult::error(vec![Content::text(e)]));
        }
        if let Some(branch) = &params.branch
            && let Err(e) = validate_branch(branch)
        {
            return Ok(CallToolResult::error(vec![Content::text(e)]));
        }

        let mut config = self.config.lock().await;

        let scope = match (&params.repo, &params.branch) {
            (None, _) => {
                // Global
                if let Some(l) = params.build_started {
                    config.notifications.build_started = l;
                }
                if let Some(l) = params.build_success {
                    config.notifications.build_success = l;
                }
                if let Some(l) = params.build_failure {
                    config.notifications.build_failure = l;
                }
                "global".to_string()
            }
            (Some(repo), None) => {
                // Per-repo
                let Some(rc) = config.repos.get_mut(repo) else {
                    return Ok(CallToolResult::error(vec![Content::text(format!(
                        "{repo} is not being watched — use watch_builds first"
                    ))]));
                };
                if let Some(l) = params.build_started {
                    rc.notifications.build_started = Some(l);
                }
                if let Some(l) = params.build_success {
                    rc.notifications.build_success = Some(l);
                }
                if let Some(l) = params.build_failure {
                    rc.notifications.build_failure = Some(l);
                }
                repo.clone()
            }
            (Some(repo), Some(branch)) => {
                // Per-branch
                let Some(rc) = config.repos.get_mut(repo) else {
                    return Ok(CallToolResult::error(vec![Content::text(format!(
                        "{repo} is not being watched — use watch_builds first"
                    ))]));
                };
                let bc = rc.branch_notifications.entry(branch.clone()).or_default();
                if let Some(l) = params.build_started {
                    bc.notifications.build_started = Some(l);
                }
                if let Some(l) = params.build_success {
                    bc.notifications.build_success = Some(l);
                }
                if let Some(l) = params.build_failure {
                    bc.notifications.build_failure = Some(l);
                }
                format!("{repo} [{branch}]")
            }
        };

        let save_warning = persist_warning(save_config(&config));

        // Show effective config for the scope
        let effective = match (&params.repo, &params.branch) {
            (Some(repo), Some(branch)) => config.notifications_for(repo, branch),
            (Some(repo), None) => config.notifications_for(
                repo,
                config
                    .default_branches
                    .first()
                    .map_or("main", |s| s.as_str()),
            ),
            _ => config.notifications.clone(),
        };

        let mut msg = format!(
            "Updated notifications for {scope}:\n  build_started: {}\n  build_success: {}\n  build_failure: {}",
            effective.build_started, effective.build_success, effective.build_failure,
        );
        if let Some(warning) = save_warning {
            msg.push_str(&warning);
        }

        Ok(CallToolResult::success(vec![Content::text(msg)]))
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
                 set_default_branches to change the default (main). \
                 Use configure_workflows to filter which workflows to watch per repo. \
                 Use ignore_workflows/unignore_workflows to globally ignore workflows like Semgrep or Dependabot. \
                 Use configure_notifications to control which events trigger notifications — \
                 set scope with repo and branch params (global if omitted, per-repo, or per-branch). \
                 Levels: off, low, normal, critical. \
                 Use pause_notifications/resume_notifications to temporarily suppress notifications. \
                 Use configure_sound to enable/disable audio alerts on failure. \
                 Use rerun_build to rerun a failed build (or the last failed build for a repo). \
                 Use build_history to see recent builds for a repo. \
                 Use get_config to see current settings.",
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

fn format_secs(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else {
        let m = secs / 60;
        let s = secs % 60;
        if s == 0 {
            format!("{m}m")
        } else {
            format!("{m}m {s}s")
        }
    }
}

fn format_age(secs: u64) -> String {
    if secs < 60 {
        "just now".to_string()
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86400)
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() > max {
        format!("{}…", &s[..max - 1])
    } else {
        s.to_string()
    }
}

fn format_notification_overrides(overrides: &NotificationOverrides) -> String {
    [
        overrides.build_started.map(|l| format!("started: {l}")),
        overrides.build_success.map(|l| format!("success: {l}")),
        overrides.build_failure.map(|l| format!("failure: {l}")),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(", ")
}

#[cfg(test)]
mod tests {
    use super::deserialize_string_or_vec;

    #[test]
    fn deserialize_proper_array() {
        let json = r#"["alice/app","bob/lib"]"#;
        let mut de = serde_json::Deserializer::from_str(json);
        let result = deserialize_string_or_vec(&mut de).unwrap();
        assert_eq!(result, vec!["alice/app", "bob/lib"]);
    }

    #[test]
    fn deserialize_stringified_array() {
        let json = r#""[\"alice/app\",\"bob/lib\"]""#;
        let mut de = serde_json::Deserializer::from_str(json);
        let result = deserialize_string_or_vec(&mut de).unwrap();
        assert_eq!(result, vec!["alice/app", "bob/lib"]);
    }

    #[test]
    fn deserialize_empty_array() {
        let json = r#"[]"#;
        let mut de = serde_json::Deserializer::from_str(json);
        let result = deserialize_string_or_vec(&mut de).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn deserialize_invalid_string_errors() {
        let json = r#""not json""#;
        let mut de = serde_json::Deserializer::from_str(json);
        assert!(deserialize_string_or_vec(&mut de).is_err());
    }
}
