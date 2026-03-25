use std::fmt::Write as _;
use std::sync::Arc;

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
    NotificationLevel, NotificationOverrides, PersistError, QuietHours, RepoConfig, config_dir,
    save_config, state_dir,
};
use crate::format;
use crate::github::{gh_run_list_history, gh_run_rerun, validate_branch, validate_repo};
use crate::watcher::{
    MIN_ACTIVE_SECS, MIN_IDLE_SECS, PauseState, RateLimitState, SharedConfig, WatchKey,
    WatcherHandle, Watches, compute_intervals, last_failed_build, save_watches, start_watch,
};

const DEFAULT_PORT: u16 = 8417;

/// Bind to the preferred port, trying up to 9 consecutive ports on conflict.
async fn bind_with_fallback(preferred: u16) -> Result<tokio::net::TcpListener> {
    for port in preferred..=preferred.saturating_add(9) {
        match tokio::net::TcpListener::bind(format!("127.0.0.1:{port}")).await {
            Ok(l) => return Ok(l),
            Err(_) if port < preferred.saturating_add(9) => {}
            Err(e) => return Err(e.into()),
        }
    }
    unreachable!()
}

/// Build the axum router with the MCP `StreamableHttpService`.
fn build_router(
    watches: Watches,
    config: SharedConfig,
    handle: WatcherHandle,
    pause: PauseState,
    rate_limit: RateLimitState,
    started_at: std::time::Instant,
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
                    rate_limit.clone(),
                    started_at,
                ))
            },
            Arc::default(),
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
    rate_limit: RateLimitState,
    ct: CancellationToken,
) -> Result<()> {
    let started_at = std::time::Instant::now();
    let port: u16 = std::env::var("BUILD_WATCHER_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(DEFAULT_PORT);

    let router = build_router(
        watches.clone(),
        config,
        handle.clone(),
        pause,
        rate_limit,
        started_at,
        &ct,
    );
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
    /// Branches to watch for this repo (e.g. `["main", "develop"]`)
    #[serde(deserialize_with = "deserialize_string_or_vec")]
    branches: Vec<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SetDefaultBranchesParams {
    /// Default branches to watch when no per-repo config exists (e.g. `["main"]`)
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
struct SetAliasParams {
    /// GitHub repo in "owner/repo" format
    repo: String,
    /// Short display name shown in notification titles. Set to null or omit to clear the alias.
    alias: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ConfigureWorkflowsParams {
    /// GitHub repo in "owner/repo" format
    repo: String,
    /// Workflow names to watch (e.g. `["CI", "Deploy"]`). Empty list means all workflows.
    #[serde(deserialize_with = "deserialize_string_or_vec")]
    workflows: Vec<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct IgnoreWorkflowsParams {
    /// Workflow names to ignore globally (e.g. `["Semgrep", "Dependabot"]`)
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
struct ConfigureQuietHoursParams {
    /// Start of quiet period in HH:MM (24-hour) local time. Defaults to `"22:00"`.
    start: Option<String>,
    /// End of quiet period in HH:MM (24-hour) local time. Defaults to `"06:00"`.
    end: Option<String>,
    /// Set to true to disable quiet hours entirely.
    clear: Option<bool>,
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
    rate_limit: RateLimitState,
    started_at: std::time::Instant,
}

#[tool_router]
impl BuildWatcher {
    pub(crate) fn new(
        watches: Watches,
        config: SharedConfig,
        handle: WatcherHandle,
        pause: PauseState,
        rate_limit: RateLimitState,
        started_at: std::time::Instant,
    ) -> Self {
        Self {
            tool_router: Self::tool_router(),
            watches,
            config,
            handle,
            pause,
            rate_limit,
            started_at,
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
                match start_watch(
                    &self.watches,
                    &self.config,
                    &self.handle,
                    &self.rate_limit,
                    repo,
                    branch,
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
                    let keys: Vec<WatchKey> = watches
                        .keys()
                        .filter(|k| k.matches_repo(repo))
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
                let (repo, branch) = (&key.repo, &key.branch);
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
                        .values()
                        .map(|active| {
                            let time = format::duration(active.started_at.elapsed());
                            format!(
                                "{}: {} ({}, {time})",
                                active.workflow,
                                active.display_title(),
                                active.status,
                            )
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
                ..existing
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
        // Acquire rate_limit and watches before config to maintain consistent lock order
        // (pollers lock in the same order: rate_limit → watches → config).
        let (active_secs, idle_secs, rate_limit_line) = {
            let rl = self.rate_limit.lock().await;
            let num_watches = self.watches.lock().await.len();
            let (active, idle) = compute_intervals(rl.as_ref(), num_watches);
            let line = format_rate_limit_line(rl.as_ref(), active, idle);
            (active, idle, line)
        };

        let paused = {
            let p = self.pause.lock().await;
            p.is_some_and(|deadline| tokio::time::Instant::now() < deadline)
        };

        let config = self.config.lock().await;
        let mut lines = Vec::new();

        if paused {
            lines.push("⏸ Notifications: PAUSED".to_string());
        }
        lines.push(format!("Default branches: {:?}", config.default_branches));
        lines.push(format!("\nRate limit: {rate_limit_line}"));
        lines.push(format!("Polling: active={active_secs}s idle={idle_secs}s"));
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
                let label = config.short_repo(repo);
                if rc.branches.is_empty() {
                    lines.push(format!("  {repo}: (default branches)"));
                } else {
                    lines.push(format!("  {repo}: {:?}", rc.branches));
                }
                if let Some(alias) = &rc.alias {
                    lines.push(format!("    alias: \"{alias}\" (shown as \"{label}\")"));
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

        match &config.quiet_hours {
            Some(qh) => lines.push(format!(
                "\nQuiet hours: {}–{} (currently: {})",
                qh.start,
                qh.end,
                if config.is_in_quiet_hours() {
                    "active"
                } else {
                    "inactive"
                }
            )),
            None => lines.push("\nQuiet hours: not configured".to_string()),
        }

        lines.push(format!(
            "\nConfig file: {}",
            config_dir().join("config.json").display()
        ));

        Ok(CallToolResult::success(vec![Content::text(
            lines.join("\n"),
        )]))
    }

    #[tool(
        description = "Show a live stats snapshot: active builds, polling intervals, \
                       GitHub API rate limit, and notification state (paused / quiet hours)."
    )]
    async fn get_stats(&self) -> Result<CallToolResult, McpError> {
        // Lock order: rate_limit → watches → pause → config (matches poller order).
        let rl = self.rate_limit.lock().await;
        let watches_snap: Vec<(String, usize)> = {
            let w = self.watches.lock().await;
            w.iter()
                .map(|(k, e)| (k.to_string(), e.active_runs.len()))
                .collect()
        };
        let (active_secs, idle_secs) = compute_intervals(rl.as_ref(), watches_snap.len());
        let throttled = active_secs > MIN_ACTIVE_SECS || idle_secs > MIN_IDLE_SECS;

        let paused = {
            let p = self.pause.lock().await;
            p.is_some_and(|d| tokio::time::Instant::now() < d)
        };
        let (quiet_hours_label, quiet_active) = {
            let cfg = self.config.lock().await;
            let label = cfg.quiet_hours.as_ref().map_or_else(
                || "off".to_string(),
                |qh| format!("{}–{}", qh.start, qh.end),
            );
            let active = cfg.is_in_quiet_hours();
            (label, active)
        };

        let uptime = format::seconds(self.started_at.elapsed().as_secs());
        let mut lines = Vec::new();

        lines.push(format!("Uptime    : {uptime}"));

        // Watches
        let total_active_builds: usize = watches_snap.iter().map(|(_, n)| n).sum();
        lines.push(format!(
            "Watches   : {} repo/branch pairs, {} build(s) in progress",
            watches_snap.len(),
            total_active_builds,
        ));

        // Polling
        let throttle_note = if throttled { " [throttled]" } else { "" };
        lines.push(format!(
            "Polling   : {active_secs}s active / {idle_secs}s idle{throttle_note}",
        ));

        // Rate limit
        lines.push(String::new());
        lines.push("GitHub API rate limit".to_string());
        match rl.as_ref() {
            None => lines
                .push("  (no data yet — first refresh happens after the first poll)".to_string()),
            Some(rl) => {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let mins_left = rl.reset.saturating_sub(now) / 60;
                let pct = rl.remaining * 100 / rl.limit.max(1);
                lines.push(format!(
                    "  Remaining : {} / {} ({}%)",
                    rl.remaining, rl.limit, pct
                ));
                lines.push(format!("  Used      : {}", rl.used));
                lines.push(format!("  Resets in : {mins_left}m"));
            }
        }

        // Notification state
        lines.push(String::new());
        lines.push("Notifications".to_string());
        lines.push(format!(
            "  Paused      : {}",
            if paused { "yes" } else { "no" }
        ));
        lines.push(format!(
            "  Quiet hours : {} (currently: {})",
            quiet_hours_label,
            if quiet_active { "active" } else { "inactive" },
        ));

        Ok(CallToolResult::success(vec![Content::text(
            lines.join("\n"),
        )]))
    }

    #[tool(
        description = "Set or clear a daily quiet hours window during which desktop notifications \
                       are suppressed. Builds are still tracked; only the notification is skipped. \
                       Defaults to 22:00–06:00 local time if no times are given. \
                       Use clear=true to disable quiet hours."
    )]
    async fn configure_quiet_hours(
        &self,
        Parameters(params): Parameters<ConfigureQuietHoursParams>,
    ) -> Result<CallToolResult, McpError> {
        if params.clear == Some(true) {
            let mut config = self.config.lock().await;
            config.quiet_hours = None;
            let msg = persist_warning(save_config(&config))
                .unwrap_or_else(|| "Quiet hours cleared".to_string());
            return Ok(CallToolResult::success(vec![Content::text(msg)]));
        }

        let start = params.start.unwrap_or_else(|| "22:00".to_string());
        let end = params.end.unwrap_or_else(|| "06:00".to_string());

        if let Err(e) = validate_hhmm(&start) {
            return Ok(CallToolResult::error(vec![Content::text(e)]));
        }
        if let Err(e) = validate_hhmm(&end) {
            return Ok(CallToolResult::error(vec![Content::text(e)]));
        }

        let mut config = self.config.lock().await;
        config.quiet_hours = Some(QuietHours {
            start: start.clone(),
            end: end.clone(),
        });
        let msg = persist_warning(save_config(&config))
            .unwrap_or_else(|| format!("Quiet hours set: {start}–{end} (local time)"));
        Ok(CallToolResult::success(vec![Content::text(msg)]))
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
        description = "Set a display alias for a repo. The alias replaces the repo name in notification titles. Pass alias=null to clear and restore the default name."
    )]
    async fn set_alias(
        &self,
        Parameters(params): Parameters<SetAliasParams>,
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
        let mut msg = if let Some(alias) = &params.alias {
            rc.alias = Some(alias.clone());
            format!("{}: alias set to \"{}\"", params.repo, alias)
        } else {
            rc.alias = None;
            format!("{}: alias cleared", params.repo)
        };
        if let Some(warning) = persist_warning(save_config(&config)) {
            msg.push_str(&warning);
        }
        Ok(CallToolResult::success(vec![Content::text(msg)]))
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
        rc.workflows.clone_from(&params.workflows);
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
                const INDEFINITE: u64 = u32::MAX as u64; // ~136 years
                *p = Some(tokio::time::Instant::now() + std::time::Duration::from_secs(INDEFINITE));
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
                let _ = write!(
                    msg,
                    "Sound file: {}",
                    config
                        .sound_on_failure
                        .sound_file
                        .as_deref()
                        .unwrap_or("(system default)")
                );
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
                    tracing::info!(
                        repo = params.repo,
                        branch = key.branch,
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
                .map_or_else(|| "—".to_string(), format::seconds);
            let age = entry
                .age_secs()
                .map_or_else(|| "—".to_string(), format::age);
            let title = entry.display_title();

            if show_branch {
                lines.push(format!(
                    "{:<12} {:<15} {:<20} {:<30} {:<10} {}",
                    entry.conclusion,
                    format::truncate(&entry.branch, 13),
                    format::truncate(&entry.workflow, 18),
                    format::truncate(&title, 28),
                    duration,
                    age,
                ));
            } else {
                lines.push(format!(
                    "{:<12} {:<20} {:<35} {:<10} {}",
                    entry.conclusion,
                    format::truncate(&entry.workflow, 18),
                    format::truncate(&title, 33),
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
                apply_notification_levels(&mut config.notifications, &params);
                "global".to_string()
            }
            (Some(repo), None) => {
                let Some(rc) = config.repos.get_mut(repo) else {
                    return Ok(CallToolResult::error(vec![Content::text(format!(
                        "{repo} is not being watched — use watch_builds first"
                    ))]));
                };
                apply_notification_overrides(&mut rc.notifications, &params);
                repo.clone()
            }
            (Some(repo), Some(branch)) => {
                let Some(rc) = config.repos.get_mut(repo) else {
                    return Ok(CallToolResult::error(vec![Content::text(format!(
                        "{repo} is not being watched — use watch_builds first"
                    ))]));
                };
                let bc = rc.branch_notifications.entry(branch.clone()).or_default();
                apply_notification_overrides(&mut bc.notifications, &params);
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
                 Use set_alias to give a repo a short display name in notification titles. \
                 Use pause_notifications/resume_notifications to temporarily suppress notifications. \
                 Use configure_quiet_hours to suppress notifications between two HH:MM times (default 22:00–06:00). \
                 Use configure_sound to enable/disable audio alerts on failure. \
                 Use rerun_build to rerun a failed build (or the last failed build for a repo). \
                 Use build_history to see recent builds for a repo. \
                 Use get_stats for a live snapshot of polling, rate limit, and notification state. \
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

/// Apply notification level params to a global `NotificationConfig` (sets values directly).
fn apply_notification_levels(
    notif: &mut crate::config::NotificationConfig,
    params: &ConfigureNotificationsParams,
) {
    if let Some(l) = params.build_started {
        notif.build_started = l;
    }
    if let Some(l) = params.build_success {
        notif.build_success = l;
    }
    if let Some(l) = params.build_failure {
        notif.build_failure = l;
    }
}

/// Apply notification level params to an override struct (sets Option values).
fn apply_notification_overrides(
    overrides: &mut NotificationOverrides,
    params: &ConfigureNotificationsParams,
) {
    if let Some(l) = params.build_started {
        overrides.build_started = Some(l);
    }
    if let Some(l) = params.build_success {
        overrides.build_success = Some(l);
    }
    if let Some(l) = params.build_failure {
        overrides.build_failure = Some(l);
    }
}

/// Validate a time string in HH:MM (24-hour) format.
fn validate_hhmm(s: &str) -> Result<(), String> {
    let Some((h, m)) = s.split_once(':') else {
        return Err(format!("{s:?} is not HH:MM format (e.g. \"22:00\")"));
    };
    let h: u32 = h
        .parse()
        .map_err(|_| format!("{s:?}: hours must be a number"))?;
    let m: u32 = m
        .parse()
        .map_err(|_| format!("{s:?}: minutes must be a number"))?;
    if h > 23 || m > 59 {
        return Err(format!("{s:?}: hours must be 0–23, minutes 0–59"));
    }
    Ok(())
}

/// Format the rate-limit status line for `get_config`.
fn format_rate_limit_line(
    rl: Option<&crate::github::RateLimit>,
    active_secs: u64,
    idle_secs: u64,
) -> String {
    let Some(rl) = rl else {
        return "unknown (no watches active yet)".to_string();
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mins_left = rl.reset.saturating_sub(now) / 60;
    let throttled = active_secs > MIN_ACTIVE_SECS || idle_secs > MIN_IDLE_SECS;
    let throttle_note = if throttled { " [throttled]" } else { "" };
    format!(
        "{}/{} remaining (resets in {}m){}",
        rl.remaining, rl.limit, mins_left, throttle_note
    )
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
