use rmcp::handler::server::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Content, Implementation, ServerCapabilities, ServerInfo};
use rmcp::{ErrorData as McpError, ServerHandler, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::config::{
    NotificationLevel, NotificationOverrides, RepoConfig, config_dir, save_config,
};
use crate::watcher::{
    SharedConfig, Watches, parse_watch_key, save_watches, start_watch, watch_key,
};

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

#[derive(Clone)]
pub struct BuildWatcher {
    tool_router: ToolRouter<Self>,
    watches: Watches,
    config: SharedConfig,
}

#[tool_router]
impl BuildWatcher {
    pub(crate) fn new(watches: Watches, config: SharedConfig) -> Self {
        Self {
            tool_router: Self::tool_router(),
            watches,
            config,
        }
    }

    #[tool(
        description = "Persistently watch GitHub Actions builds for one or more repos. Watches configured branches (default: main). Sends desktop notifications when builds start and complete. Repos should be in owner/repo format."
    )]
    async fn watch_builds(
        &self,
        Parameters(params): Parameters<WatchBuildsParams>,
    ) -> Result<CallToolResult, McpError> {
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
                match start_watch(&self.watches, &self.config, repo, branch, &key).await {
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
            save_config(&config);
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
                let last = entry
                    .last_build
                    .as_ref()
                    .map(|b| {
                        let sha = b.short_sha();
                        let event_str = if b.event.is_empty() {
                            String::new()
                        } else {
                            format!(", {}", b.event)
                        };
                        let sha_str = if sha.is_empty() {
                            String::new()
                        } else {
                            format!(" {sha}")
                        };
                        format!(
                            " (last: {}{} — {}: {}{})",
                            b.conclusion, event_str, b.workflow, b.title, sha_str
                        )
                    })
                    .unwrap_or_default();

                if entry.active_runs.is_empty() {
                    format!("- {repo} [{branch}] — idle{last}")
                } else {
                    let run_list: Vec<String> = entry
                        .active_runs
                        .iter()
                        .map(|(id, status)| format!("{id} ({status})"))
                        .collect();
                    format!(
                        "- {repo} [{branch}] — {} active: {}{last}",
                        entry.active_runs.len(),
                        run_list.join(", ")
                    )
                }
            })
            .collect();
        lines.sort();

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
                notifications: existing.notifications,
                branch_notifications: existing.branch_notifications,
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
        config.default_branches = params.branches;
        save_config(&config);

        Ok(CallToolResult::success(vec![Content::text(format!(
            "Default branches set to {:?}",
            config.default_branches,
        ))]))
    }

    #[tool(
        description = "Show the current configuration including watched repos, default branches, and per-repo overrides."
    )]
    async fn get_config(&self) -> Result<CallToolResult, McpError> {
        let config = self.config.lock().await;
        let mut lines = Vec::new();

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
        );
        Ok(CallToolResult::success(vec![Content::text(
            "Test notification sent. You should see it on your desktop.",
        )]))
    }

    #[tool(
        description = "Configure notification levels. Scope depends on which params are set: global (no repo/branch), per-repo (repo only), or per-branch (repo + branch). Only the events you specify are changed; others keep their current value. Levels: off, low, normal, critical. Examples: 'only notify me on failure for benefits' or 'on the release branch, only notify on success'."
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

        save_config(&config);

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

        Ok(CallToolResult::success(vec![Content::text(format!(
            "Updated notifications for {scope}:\n  build_started: {}\n  build_success: {}\n  build_failure: {}",
            effective.build_started, effective.build_success, effective.build_failure,
        ))]))
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
                 Use configure_notifications to control which events trigger notifications — \
                 set scope with repo and branch params (global if omitted, per-repo, or per-branch). \
                 Levels: off, low, normal, critical. Use get_config to see current settings.",
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
