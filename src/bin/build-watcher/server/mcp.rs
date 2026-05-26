use std::time::Duration;

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Content, Implementation, ServerCapabilities, ServerInfo};
use rmcp::{ErrorData as McpError, ServerHandler, tool, tool_handler, tool_router};

use build_watcher::config::{PollAggression, unix_now};
use build_watcher::dirs::config_dir;
use build_watcher::format;
use build_watcher::github::{validate_branch, validate_repo};
use build_watcher::history::history_for;
use build_watcher::rate_limiter::{MIN_POLL_SECS, PollInput, compute_interval};
use build_watcher::watcher::{count_api_calls, is_paused};

use super::DaemonState;
use super::actions::{
    apply_levels, apply_pause, apply_quiet_hours, do_add_auto_discover_rule, do_configure_branches,
    do_remove_auto_discover_rule, do_rerun, do_stop_watches, do_watch_builds, format_outcomes,
    modify_ignore_list, validate_hhmm,
};
use super::build_watch_snapshot;
use super::schema::{
    AddAutoDiscoverRuleParams, BuildHistoryParams, ConfigureBranchesParams,
    ConfigureIgnoredEventsParams, ConfigureRepoParams, RemoveAutoDiscoverRuleParams, ReposParams,
    RerunBuildParams, SetPollAggressionParams, UpdateNotificationsParams, WatchFromGitRemoteParams,
};

#[derive(Clone)]
pub struct BuildWatcher {
    state: DaemonState,
}

#[tool_router]
impl BuildWatcher {
    pub(crate) fn new(state: DaemonState) -> Self {
        Self { state }
    }

    #[tool(description = "Watch GitHub Actions builds for one or more repos. \
                       Repos must be in 'owner/repo' format. \
                       Default: watches only the repo's GitHub default branch unless branches are pre-configured. \
                       To watch additional branches, call configure_branches after adding.")]
    async fn watch_builds(
        &self,
        Parameters(params): Parameters<ReposParams>,
    ) -> Result<CallToolResult, McpError> {
        for repo in &params.repos {
            if let Err(e) = validate_repo(repo) {
                return Ok(CallToolResult::error(vec![Content::text(e)]));
            }
        }

        let results = do_watch_builds(&self.state, &params.repos).await;

        Ok(CallToolResult::success(vec![Content::text(
            format_outcomes(&results, "\n\n"),
        )]))
    }

    #[tool(
        description = "Detect the GitHub repo from a local git repository's origin remote and start watching it. \
                       Pass the absolute path to the repo directory. \
                       Equivalent to calling watch_builds with the detected 'owner/repo' slug."
    )]
    async fn watch_from_git_remote(
        &self,
        Parameters(params): Parameters<WatchFromGitRemoteParams>,
    ) -> Result<CallToolResult, McpError> {
        let repo = match build_watcher::github::repo_from_git_remote(&params.path).await {
            Ok(r) => r,
            Err(e) => {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Could not detect GitHub repo from {}: {e}",
                    params.path
                ))]));
            }
        };

        let results = do_watch_builds(&self.state, std::slice::from_ref(&repo)).await;
        let mut msg = format!("Detected repo: {repo}\n\n");
        msg.push_str(&format_outcomes(&results, "\n\n"));
        Ok(CallToolResult::success(vec![Content::text(msg)]))
    }

    #[tool(
        description = "Stop watching builds for one or more repos and remove them from config. \
                       Repos must be in 'owner/repo' format. \
                       Stops all branches for each repo; per-repo config (alias, notifications) is also cleared."
    )]
    async fn stop_watches(
        &self,
        Parameters(params): Parameters<ReposParams>,
    ) -> Result<CallToolResult, McpError> {
        for repo in &params.repos {
            if let Err(e) = validate_repo(repo) {
                return Ok(CallToolResult::error(vec![Content::text(e)]));
            }
        }
        let results = do_stop_watches(&self.state, &params.repos).await;
        Ok(CallToolResult::success(vec![Content::text(
            format_outcomes(&results, "\n"),
        )]))
    }

    #[tool(
        description = "List all currently watched repos and branches with their latest build status. \
                          Shows active runs (workflow, title, status, elapsed) and last completed build per branch."
    )]
    async fn list_watches(&self) -> Result<CallToolResult, McpError> {
        let paused = is_paused(&self.state.pause).await;
        let watches = self.state.watches.lock().await;
        if watches.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "No active watches",
            )]));
        }
        let snapshot = build_watch_snapshot(&watches, None, None, paused);

        let mut lines: Vec<String> = Vec::new();
        if snapshot.paused {
            lines.push("⏸ Notifications paused\n".to_string());
        }

        let watch_lines: Vec<String> = snapshot
            .watches
            .iter()
            .map(|w| {
                let last = if w.last_builds.is_empty() {
                    String::new()
                } else {
                    let parts: Vec<String> = w
                        .last_builds
                        .iter()
                        .map(|b| format!("{}: {} — {}", b.workflow, b.conclusion.as_str(), b.title))
                        .collect();
                    format!(" (last: {})", parts.join("; "))
                };

                if w.active_runs.is_empty() {
                    format!("- {} [{}] — idle{last}", w.repo, w.branch)
                } else {
                    let run_list: Vec<String> = w
                        .active_runs
                        .iter()
                        .map(|r| {
                            let time = r
                                .elapsed_secs
                                .map(|s| format::duration(Duration::from_secs_f64(s)))
                                .unwrap_or_default();
                            format!(
                                "{}: {} ({}, {time})",
                                r.workflow,
                                r.title,
                                r.status.as_str()
                            )
                        })
                        .collect();
                    format!(
                        "- {} [{}] — {} active: {}{last}",
                        w.repo,
                        w.branch,
                        w.active_runs.len(),
                        run_list.join(", ")
                    )
                }
            })
            .collect();
        lines.extend(watch_lines);

        Ok(CallToolResult::success(vec![Content::text(
            lines.join("\n"),
        )]))
    }

    #[tool(description = "Set the branch list for a specific repo. \
                          Replaces any previously configured branches. \
                          Gotcha: omitting a branch here stops watching it; use watch_builds first if the repo is new.")]
    async fn configure_branches(
        &self,
        Parameters(params): Parameters<ConfigureBranchesParams>,
    ) -> Result<CallToolResult, McpError> {
        if params.branches.is_empty() {
            return Ok(CallToolResult::error(vec![Content::text(
                "branches must not be empty",
            )]));
        }
        for branch in &params.branches {
            if let Err(e) = validate_branch(branch) {
                return Ok(CallToolResult::error(vec![Content::text(e)]));
            }
        }
        if let Err(e) = validate_repo(&params.repo) {
            return Ok(CallToolResult::error(vec![Content::text(e)]));
        }
        let results = do_configure_branches(&self.state, &params.repo, params.branches).await;
        Ok(CallToolResult::success(vec![Content::text(
            format_outcomes(&results, "\n"),
        )]))
    }

    #[tool(
        description = "Show a live stats snapshot: uptime, active builds, poll interval, \
                       GitHub API rate limit (remaining/limit/reset), notification state (paused/quiet hours/levels), \
                       and config file path."
    )]
    async fn get_stats(&self) -> Result<CallToolResult, McpError> {
        // Lock order: rate_limit → watches → pause → config (matches poller order).
        let now = unix_now();
        let rl = self.state.rate_limit.lock().await;
        let (watches_snap, api_calls) = {
            let w = self.state.watches.lock().await;
            let snap: Vec<(String, usize)> = w
                .iter()
                .map(|(k, e)| (k.to_string(), e.active_runs.len()))
                .collect();
            let calls = count_api_calls(&w);
            (snap, calls)
        };
        let aggression = self.state.config.read().await.poll_aggression;
        let poll_secs = compute_interval(&PollInput {
            rate_limit: rl.clone(),
            calls_per_cycle: api_calls,
            now,
            aggression,
        });
        let throttled = poll_secs > MIN_POLL_SECS;

        let paused = is_paused(&self.state.pause).await;
        let (
            quiet_hours_label,
            quiet_active,
            notif_levels,
            ignored_workflows,
            ignored_events,
            repo_count,
        ) = {
            let cfg = self.state.config.read().await;
            let label = cfg.quiet_hours.as_ref().map_or_else(
                || "off".to_string(),
                |qh| format!("{}–{}", qh.start, qh.end),
            );
            let active = cfg.is_in_quiet_hours();
            let levels = cfg.notifications.clone();
            let ignored_wf = cfg.ignored_workflows.clone();
            let ignored_ev = cfg.ignored_events.clone();
            let repos = cfg.repos.len();
            (label, active, levels, ignored_wf, ignored_ev, repos)
        };

        let uptime = format::seconds(self.state.started_at.elapsed().as_secs());
        let mut lines = Vec::new();

        lines.push(format!("Uptime    : {uptime}"));

        let total_active_builds: usize = watches_snap.iter().map(|(_, n)| n).sum();
        lines.push(format!(
            "Watches   : {} repo/branch pairs, {} build(s) in progress",
            watches_snap.len(),
            total_active_builds,
        ));

        let throttle_note = if throttled { " [throttled]" } else { "" };
        lines.push(format!("Polling   : every {poll_secs}s{throttle_note}",));

        lines.push(String::new());
        lines.push("GitHub API rate limit".to_string());
        match rl.as_ref() {
            None => lines
                .push("  (no data yet — first refresh happens after the first poll)".to_string()),
            Some(rl) => {
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

        lines.push(String::new());
        lines.push("Notifications".to_string());
        lines.push(format!(
            "  Paused      : {}",
            if paused { "yes" } else { "no" }
        ));
        lines.push(format!(
            "  Quiet hours : {} (currently: {})",
            quiet_hours_label,
            if quiet_active { "quiet" } else { "allowing" },
        ));
        lines.push(format!(
            "  Levels      : started={} success={} failure={}",
            notif_levels.build_started, notif_levels.build_success, notif_levels.build_failure
        ));

        lines.push(String::new());
        lines.push("Settings".to_string());
        lines.push(format!("  Poll aggression  : {aggression}"));
        lines.push(format!("  Watched repos    : {repo_count}"));
        if ignored_workflows.is_empty() {
            lines.push("  Ignored workflows: (none)".to_string());
        } else {
            lines.push(format!(
                "  Ignored workflows: {}",
                ignored_workflows.join(", ")
            ));
        }
        if ignored_events.is_empty() {
            lines.push("  Ignored events   : (none)".to_string());
        } else {
            lines.push(format!(
                "  Ignored events   : {}",
                ignored_events.join(", ")
            ));
        }

        let dropped = self.state.handle.events.dropped_count();
        if dropped > 0 {
            lines.push(format!("  Dropped events   : {dropped}"));
        }

        lines.push(String::new());
        lines.push(format!(
            "Config file : {}",
            config_dir().join("config.json").display()
        ));

        Ok(CallToolResult::success(vec![Content::text(
            lines.join("\n"),
        )]))
    }

    #[tool(description = "Configure per-repo settings. \
                       workflows: allow-list of workflow names to track (empty = all; omit = no change); matching is case-insensitive. \
                       alias: display name shown in TUI and notifications (omit = no change; use clear_alias=true to remove). \
                       watch_prs: whether to poll open PRs for merge-readiness (true/false). \
                       poll_aggression: per-repo rate-limit aggression (low/medium/high); use clear_poll_aggression=true to revert to global. \
                       auto_discover_branches: override global branch auto-discovery for this repo. \
                       branch_filter: regex to filter auto-discovered branches; empty string clears. \
                       At least one field must be provided.")]
    async fn configure_repo(
        &self,
        Parameters(params): Parameters<ConfigureRepoParams>,
    ) -> Result<CallToolResult, McpError> {
        if let Err(e) = validate_repo(&params.repo) {
            return Ok(CallToolResult::error(vec![Content::text(e)]));
        }
        if params.workflows.is_none()
            && params.alias.is_none()
            && params.clear_alias != Some(true)
            && params.watch_prs.is_none()
            && params.poll_aggression.is_none()
            && params.clear_poll_aggression != Some(true)
            && params.auto_discover_branches.is_none()
            && params.branch_filter.is_none()
        {
            return Ok(CallToolResult::error(vec![Content::text(
                "at least one field must be set",
            )]));
        }

        let repo = params.repo;
        let result = self
            .state
            .config
            .modify(|config| {
                let rc = config.repos.entry(repo.clone()).or_default();
                let mut msgs = Vec::new();
                if let Some(workflows) = &params.workflows {
                    rc.workflows.clone_from(workflows);
                    if workflows.is_empty() {
                        msgs.push(format!("{repo}: watching all workflows"));
                    } else {
                        msgs.push(format!("{repo}: watching workflows {workflows:?}"));
                    }
                }
                if params.clear_alias == Some(true) {
                    rc.alias = None;
                    msgs.push(format!("{repo}: alias cleared"));
                } else if let Some(alias) = &params.alias {
                    rc.alias = Some(alias.clone());
                    msgs.push(format!("{repo}: alias set to \"{alias}\""));
                }
                if let Some(watch_prs) = params.watch_prs {
                    rc.watch_prs = watch_prs;
                    msgs.push(format!(
                        "{repo}: PR watching {}",
                        if watch_prs { "enabled" } else { "disabled" }
                    ));
                }
                if params.clear_poll_aggression == Some(true) {
                    rc.poll_aggression = None;
                    msgs.push(format!("{repo}: poll aggression reset to global default"));
                } else if let Some(aggression) = params.poll_aggression {
                    rc.poll_aggression = Some(aggression);
                    msgs.push(format!("{repo}: poll aggression set to {aggression}"));
                }
                if let Some(enabled) = params.auto_discover_branches {
                    rc.auto_discover_branches = Some(enabled);
                    msgs.push(format!(
                        "{repo}: auto-discover branches {}",
                        if enabled { "enabled" } else { "disabled" }
                    ));
                }
                if let Some(filter) = &params.branch_filter {
                    if filter.is_empty() {
                        rc.branch_filter = None;
                        msgs.push(format!("{repo}: branch filter cleared"));
                    } else {
                        rc.branch_filter = Some(filter.clone());
                        msgs.push(format!("{repo}: branch filter set to {filter:?}"));
                    }
                }
                msgs
            })
            .await;
        let msgs = match result {
            Ok(m) => m,
            Err(e) => vec![format!(
                "\u{26a0}\u{fe0f} Warning: config could not be saved to disk: {e}"
            )],
        };
        Ok(CallToolResult::success(vec![Content::text(
            msgs.join("\n"),
        )]))
    }

    #[tool(description = "Add to or remove from an ignore list. \
                       kind='event' (default): ignore by GitHub event type (push, pull_request, schedule, workflow_dispatch, …); \
                         global scope by default, pass repo to scope to a single repo. \
                       kind='workflow': ignore by workflow name (Semgrep, Dependabot, CodeQL, …); always global scope. \
                       Pass add and/or remove — at least one must be non-empty. Matching is case-insensitive.")]
    async fn configure_ignored_events(
        &self,
        Parameters(params): Parameters<ConfigureIgnoredEventsParams>,
    ) -> Result<CallToolResult, McpError> {
        if params.add.is_empty() && params.remove.is_empty() {
            return Ok(CallToolResult::error(vec![Content::text(
                "at least one of add or remove must be non-empty",
            )]));
        }

        let kind = params.kind.as_deref().unwrap_or("event");
        if kind != "event" && kind != "workflow" {
            return Ok(CallToolResult::error(vec![Content::text(
                "kind must be 'event' or 'workflow'",
            )]));
        }

        if kind == "event"
            && let Some(repo) = &params.repo
            && let Err(e) = validate_repo(repo)
        {
            return Ok(CallToolResult::error(vec![Content::text(e)]));
        }

        let add = params.add;
        let remove = params.remove;
        let repo = params.repo;
        let result = self
            .state
            .config
            .modify(|config| {
                if kind == "workflow" {
                    modify_ignore_list(&mut config.ignored_workflows, &add, &remove, "workflow")
                } else {
                    let list = if let Some(ref repo) = repo {
                        &mut config.repos.entry(repo.clone()).or_default().ignored_events
                    } else {
                        &mut config.ignored_events
                    };
                    let scope = repo.as_deref().unwrap_or("global");
                    let mut msgs = modify_ignore_list(list, &add, &remove, "event");
                    msgs.push(format!("Scope: {scope}"));
                    msgs
                }
            })
            .await;
        let msgs = match result {
            Ok(m) => m,
            Err(e) => vec![format!(
                "\u{26a0}\u{fe0f} Warning: config could not be saved to disk: {e}"
            )],
        };

        Ok(CallToolResult::success(vec![Content::text(
            msgs.join("\n"),
        )]))
    }

    #[tool(description = "Rerun a GitHub Actions build. \
                       Default: reruns the last failed build for the repo when run_id is omitted. \
                       Set failed_only=true to rerun only the failed jobs (faster); omit or false to rerun all jobs.")]
    async fn rerun_build(
        &self,
        Parameters(params): Parameters<RerunBuildParams>,
    ) -> Result<CallToolResult, McpError> {
        if let Err(e) = validate_repo(&params.repo) {
            return Ok(CallToolResult::error(vec![Content::text(e)]));
        }

        match do_rerun(&self.state, &params.repo, params.run_id, params.failed_only).await {
            Ok(msg) => Ok(CallToolResult::success(vec![Content::text(msg)])),
            Err(e) => Ok(CallToolResult::error(vec![Content::text(e)])),
        }
    }

    #[tool(description = "Show recent build history for a repo. \
                       Displays conclusion, workflow, title, duration, and age per run. \
                       Default: last 10 runs across all branches; pass branch to filter, limit to change count (max 50).")]
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

        let limit = params.limit.unwrap_or(10).min(50) as usize;
        let entries = {
            let hist = self.state.handle.history.lock().await;
            history_for(&hist, &params.repo, params.branch.as_deref(), limit)
        };

        if entries.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "No builds found",
            )]));
        }

        let distinct_branches = entries
            .iter()
            .map(|(br, _)| br)
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

        let now = unix_now();
        for (branch, lb) in &entries {
            let duration = lb
                .duration_secs
                .map_or_else(|| "—".to_string(), format::seconds);
            let age = lb
                .completed_at
                .map(|t| now.saturating_sub(t))
                .map_or_else(|| "—".to_string(), format::age);
            let title = lb.display_title();

            if show_branch {
                lines.push(format!(
                    "{:<12} {:<15} {:<20} {:<30} {:<10} {}",
                    lb.conclusion,
                    format::truncate(branch, 13),
                    format::truncate(&lb.workflow, 18),
                    format::truncate(&title, 28),
                    duration,
                    age,
                ));
            } else {
                lines.push(format!(
                    "{:<12} {:<20} {:<35} {:<10} {}",
                    lb.conclusion,
                    format::truncate(&lb.workflow, 18),
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
        description = "Update notification settings — any combination of params in one call. \
                       Levels: build_started/build_success/build_failure accept off/low/normal/critical; \
                         default scope is global, pass repo and/or branch to narrow. \
                       Quiet hours: quiet_start + quiet_end in HH:MM (24h local time); use quiet_clear=true to disable. \
                       Pause: pause=true to silence all notifications (add pause_minutes for a timed pause), pause=false to resume."
    )]
    async fn update_notifications(
        &self,
        Parameters(params): Parameters<UpdateNotificationsParams>,
    ) -> Result<CallToolResult, McpError> {
        if params.branch.is_some() && params.repo.is_none() {
            return Ok(CallToolResult::error(vec![Content::text(
                "branch requires repo to be set",
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
        if let Some(s) = &params.quiet_start
            && let Err(e) = validate_hhmm(s)
        {
            return Ok(CallToolResult::error(vec![Content::text(e)]));
        }
        if let Some(s) = &params.quiet_end
            && let Err(e) = validate_hhmm(s)
        {
            return Ok(CallToolResult::error(vec![Content::text(e)]));
        }
        if params.quiet_start.is_some() != params.quiet_end.is_some() {
            return Ok(CallToolResult::error(vec![Content::text(
                "quiet_start and quiet_end must both be provided",
            )]));
        }

        let has_levels = params.build_started.is_some()
            || params.build_success.is_some()
            || params.build_failure.is_some();
        let has_quiet = params.quiet_start.is_some()
            || params.quiet_end.is_some()
            || params.quiet_clear == Some(true);

        if !has_levels && !has_quiet && params.pause.is_none() {
            return Ok(CallToolResult::error(vec![Content::text(
                "at least one parameter must be set",
            )]));
        }

        let mut msgs = Vec::new();

        // Pause / resume
        if let Some(pause) = params.pause {
            msgs.push(apply_pause(&self.state.pause, pause, params.pause_minutes).await);
        }

        // Quiet hours + notification levels (both touch config)
        if has_levels || has_quiet {
            let quiet_start = params.quiet_start;
            let quiet_end = params.quiet_end;
            let quiet_clear = params.quiet_clear == Some(true);
            let build_started = params.build_started;
            let build_success = params.build_success;
            let build_failure = params.build_failure;
            let repo = params.repo;
            let branch = params.branch;

            let result = self
                .state
                .config
                .modify(|config| {
                    let mut inner_msgs = Vec::new();

                    // Quiet hours (pre-validated above, so Err shouldn't occur)
                    match apply_quiet_hours(
                        config,
                        quiet_start.as_deref(),
                        quiet_end.as_deref(),
                        quiet_clear,
                    ) {
                        Ok(qh_msgs) => inner_msgs.extend(qh_msgs),
                        Err(e) => inner_msgs.push(format!("Error: {e}")),
                    }

                    // Notification levels
                    if has_levels {
                        let levels = (build_started, build_success, build_failure);
                        let scope = match (&repo, &branch) {
                            (None, _) => {
                                apply_levels(
                                    &mut config.notifications,
                                    levels.0,
                                    levels.1,
                                    levels.2,
                                );
                                "global".to_string()
                            }
                            (Some(repo), None) => {
                                let rc = config.repos.entry(repo.clone()).or_default();
                                apply_levels(&mut rc.notifications, levels.0, levels.1, levels.2);
                                repo.clone()
                            }
                            (Some(repo), Some(branch)) => {
                                let rc = config.repos.entry(repo.clone()).or_default();
                                let bc =
                                    rc.branch_notifications.entry(branch.clone()).or_default();
                                apply_levels(&mut bc.notifications, levels.0, levels.1, levels.2);
                                format!("{repo} [{branch}]")
                            }
                        };
                        let effective = match (&repo, &branch) {
                            (Some(repo), Some(branch)) => config.notifications_for(repo, branch),
                            (Some(repo), None) => config.notifications_for(
                                repo,
                                config
                                    .branches_for(repo)
                                    .first()
                                    .map_or("main", |s| s.as_str()),
                            ),
                            _ => config.notifications.clone(),
                        };
                        inner_msgs.push(format!(
                            "Updated notifications for {scope}:\n  build_started: {}\n  build_success: {}\n  build_failure: {}",
                            effective.build_started, effective.build_success, effective.build_failure,
                        ));
                    }

                    inner_msgs
                })
                .await;
            match result {
                Ok(inner_msgs) => msgs.extend(inner_msgs),
                Err(e) => msgs.push(format!(
                    "\u{26a0}\u{fe0f} Warning: config could not be saved to disk: {e}"
                )),
            }
        }

        Ok(CallToolResult::success(vec![Content::text(
            msgs.join("\n"),
        )]))
    }

    #[tool(
        description = "Set how aggressively the daemon polls GitHub relative to the rate-limit budget. \
                       low=≤15%, medium=≤40% (default), high=≤80% of the 5000 req/hr budget. \
                       Lower aggression when watching many repos or when rate limit is frequently exhausted."
    )]
    async fn set_poll_aggression(
        &self,
        Parameters(params): Parameters<SetPollAggressionParams>,
    ) -> Result<CallToolResult, McpError> {
        let level: PollAggression = match params.level.parse() {
            Ok(l) => l,
            Err(e) => {
                return Ok(CallToolResult::error(vec![Content::text(e)]));
            }
        };
        if let Err(e) = self
            .state
            .config
            .modify(|cfg| {
                cfg.poll_aggression = level;
            })
            .await
        {
            return Ok(CallToolResult::success(vec![Content::text(format!(
                "\u{26a0}\u{fe0f} Warning: config could not be saved to disk: {e}"
            ))]));
        }
        Ok(CallToolResult::success(vec![Content::text(format!(
            "Poll aggression set to {level}."
        ))]))
    }

    #[tool(description = "Add or replace an auto-discover rule. \
                       The daemon will periodically (every ~60 s) fetch all GitHub repos \
                       accessible to the authenticated user and automatically start watching \
                       those matching the rule. \
                       Auto-discovered repos are tracked separately from manually-added repos: \
                       use remove_auto_discover_rule (not stop_watches) to stop them. \
                       repo_pattern is a regex matched against the full \"owner/name\" path \
                       (e.g. `^myorg/foo-.*$`). org_pattern is a legacy regex matched only \
                       against the owner; new rules should set repo_pattern instead. \
                       recently_updated filters by last-push age: 'any' (no filter), \
                       'week', 'month', or 'year'.")]
    async fn add_auto_discover_rule(
        &self,
        Parameters(params): Parameters<AddAutoDiscoverRuleParams>,
    ) -> Result<CallToolResult, McpError> {
        let recently_updated = match params
            .recently_updated
            .as_deref()
            .unwrap_or("any")
            .parse::<build_watcher::config::RecentlyUpdated>()
        {
            Ok(r) => r,
            Err(e) => return Ok(CallToolResult::error(vec![Content::text(e)])),
        };
        match do_add_auto_discover_rule(
            &self.state,
            params.id,
            params.org_pattern,
            params.repo_pattern,
            recently_updated,
        )
        .await
        {
            Ok(msg) => Ok(CallToolResult::success(vec![Content::text(msg)])),
            Err(e) => Ok(CallToolResult::error(vec![Content::text(e)])),
        }
    }

    #[tool(description = "Remove an auto-discover rule by ID. \
                       Repos that were matched exclusively by this rule will stop being watched \
                       on the next discovery cycle (~60 s). \
                       Repos that are also manually watched (via watch_builds) are unaffected.")]
    async fn remove_auto_discover_rule(
        &self,
        Parameters(params): Parameters<RemoveAutoDiscoverRuleParams>,
    ) -> Result<CallToolResult, McpError> {
        match do_remove_auto_discover_rule(&self.state, &params.id).await {
            Ok(msg) => Ok(CallToolResult::success(vec![Content::text(msg)])),
            Err(e) => Ok(CallToolResult::error(vec![Content::text(e)])),
        }
    }

    #[tool(description = "List all auto-discover rules and the repos currently matched by them.")]
    async fn list_auto_discover_rules(&self) -> Result<CallToolResult, McpError> {
        let (rules, discovered) = {
            let cfg = self.state.config.read().await;
            let dr = self.state.handle.discovered_repos.lock().await;
            (cfg.auto_discover_rules.clone(), dr.clone())
        };

        if rules.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "No auto-discover rules configured.",
            )]));
        }

        let mut lines = Vec::new();
        for rule in &rules {
            let mut parts = Vec::new();
            if let Some(p) = &rule.org_pattern {
                parts.push(format!("org: {p:?}"));
            }
            if let Some(p) = &rule.repo_pattern {
                parts.push(format!("repo: {p:?}"));
            }
            parts.push(format!("updated: {}", rule.recently_updated));
            lines.push(format!("• {} ({})", rule.id, parts.join(", ")));
        }

        if !discovered.is_empty() {
            lines.push(String::new());
            lines.push(format!(
                "Currently discovered ({} repos):",
                discovered.len()
            ));
            let mut sorted: Vec<&String> = discovered.iter().collect();
            sorted.sort();
            for repo in sorted {
                lines.push(format!("  {repo}"));
            }
        } else {
            lines.push("\nNo repos discovered yet (next cycle in ≤60 s).".to_string());
        }

        Ok(CallToolResult::success(vec![Content::text(
            lines.join("\n"),
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
                 Use watch_from_git_remote with a local repo path to auto-detect and watch the GitHub repo from its origin remote. \
                 Use configure_branches to set which branches to watch for a specific repo. \
                 Use configure_repo to set per-repo workflow allow-list, alias, watch_prs, poll_aggression, auto_discover_branches, or branch_filter. \
                 Use configure_ignored_workflows(add/remove) to manage the global workflow ignore list (e.g. Semgrep, Dependabot). \
                 Use configure_ignored_events(add/remove) to ignore runs by GitHub event type (e.g. schedule, workflow_dispatch) globally or per-repo. \
                 Use update_notifications to set notification levels (off/low/normal/critical, per event and scope), \
                 configure quiet hours (quiet_start/quiet_end in HH:MM, or quiet_clear=true), \
                 or pause/resume (pause=true/false, with optional pause_minutes). \
                 Use rerun_build to rerun a failed build (or the last failed build for a repo). \
                 Use build_history to see recent builds for a repo. \
                 Use get_stats for a live snapshot of polling, rate limit, notification state, and config file path. \
                 Use set_poll_aggression to control how much of the GitHub rate-limit budget the daemon uses per hour (low/medium/high).",
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
