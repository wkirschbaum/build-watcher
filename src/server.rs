use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

type AnyResult<T> = std::result::Result<T, Box<dyn std::error::Error>>;
use axum::extract::{Query, State};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::routing::get;
use rmcp::handler::server::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Content, Implementation, ServerCapabilities, ServerInfo};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use rmcp::{ErrorData as McpError, ServerHandler, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio_stream::StreamExt as _;
use tokio_stream::wrappers::BroadcastStream;
use tokio_util::sync::CancellationToken;

use build_watcher::config::{
    BranchConfig, NotificationConfig, NotificationLevel, NotificationOverrides, QuietHours,
    config_dir, state_dir, unix_now,
};
use build_watcher::events::{EventBus, WatchEvent};
use build_watcher::format;
use build_watcher::github::{validate_branch, validate_repo};
use build_watcher::persistence::Persistence;
use build_watcher::status::{
    ActiveRunView, LastBuildView, StatsResponse, StatusResponse, WatchStatus,
};
use build_watcher::watcher::{
    MIN_ACTIVE_SECS, MIN_IDLE_SECS, PauseState, RateLimitState, SharedConfig, WatchEntry, WatchKey,
    WatcherHandle, Watches, collect_persisted, compute_intervals, count_api_calls, is_paused,
    last_failed_build, start_watch,
};

pub const DEFAULT_PORT: u16 = 8417;

// -- SSE / status endpoints --

/// Shared state for the HTTP routes.
#[derive(Clone)]
struct AppState {
    watches: Watches,
    config: SharedConfig,
    handle: WatcherHandle,
    pause: PauseState,
    events: EventBus,
    github: Arc<dyn build_watcher::github::GitHubClient>,
    rate_limit: RateLimitState,
    started_at: std::time::Instant,
}

/// Build a snapshot of all current watches from already-locked state.
///
/// Pure function (no async, no locks) — callers acquire the locks and pass
/// the data in. Both the `GET /status` HTTP handler and the `list_watches`
/// MCP tool call this so the watch-enumeration logic lives in one place.
fn build_watch_snapshot(
    watches: &HashMap<WatchKey, WatchEntry>,
    config: Option<&build_watcher::config::Config>,
    paused: bool,
    now: tokio::time::Instant,
) -> StatusResponse {
    let mut watch_list: Vec<WatchStatus> = watches
        .iter()
        .map(|(key, entry)| {
            let mut active_runs: Vec<ActiveRunView> = entry
                .active_runs
                .iter()
                .map(|(run_id, run)| {
                    let elapsed_secs = now
                        .checked_duration_since(run.started_at)
                        .map(|d| d.as_secs_f64());
                    ActiveRunView {
                        run_id: *run_id,
                        status: run.status.clone(),
                        workflow: run.workflow.clone(),
                        title: run.display_title(),
                        event: run.event.clone(),
                        elapsed_secs,
                    }
                })
                .collect();
            active_runs.sort_by_key(|r| r.run_id);

            let last_build = entry.last_build.as_ref().map(|lb| {
                let age_secs = entry
                    .completed_at
                    .and_then(|t| now.checked_duration_since(t))
                    .map(|d| d.as_secs_f64());
                LastBuildView {
                    run_id: lb.run_id,
                    conclusion: lb.conclusion.clone(),
                    workflow: lb.workflow.clone(),
                    title: lb.display_title(),
                    failing_steps: lb.failing_steps.clone(),
                    age_secs,
                }
            });

            let muted = config.is_some_and(|cfg| {
                let n = cfg.notifications_for(&key.repo, &key.branch);
                n.build_started == NotificationLevel::Off
                    && n.build_success == NotificationLevel::Off
                    && n.build_failure == NotificationLevel::Off
            });

            WatchStatus {
                repo: key.repo.clone(),
                branch: key.branch.clone(),
                active_runs,
                last_build,
                muted,
            }
        })
        .collect();
    watch_list.sort_by(|a, b| a.repo.cmp(&b.repo).then(a.branch.cmp(&b.branch)));

    StatusResponse {
        paused,
        watches: watch_list,
    }
}

/// `GET /status` — JSON snapshot of all current watches and their build state.
async fn status_handler(State(state): State<AppState>) -> axum::Json<StatusResponse> {
    let now = tokio::time::Instant::now();
    let paused = is_paused(&state.pause).await;
    let watches = state.watches.lock().await;
    let cfg = state.config.lock().await;
    axum::Json(build_watch_snapshot(&watches, Some(&cfg), paused, now))
}

/// `GET /events` — SSE stream of `WatchEvent`s as they occur.
///
/// Each frame has an event type matching the variant name and a JSON data payload.
/// A keepalive comment is sent every 30 seconds to detect dropped connections.
async fn events_handler(State(state): State<AppState>) -> impl axum::response::IntoResponse {
    let stream = BroadcastStream::new(state.events.subscribe())
        .filter_map(|result| result.ok())
        .map(|event| {
            let event_type = match &event {
                WatchEvent::RunStarted(_) => "RunStarted",
                WatchEvent::RunCompleted { .. } => "RunCompleted",
                WatchEvent::StatusChanged { .. } => "StatusChanged",
            };
            let data = serde_json::to_string(&event).unwrap_or_default();
            Ok::<_, Infallible>(Event::default().event(event_type).data(data))
        });

    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(30)))
}

/// `GET /stats` — Daemon stats: uptime, polling intervals, rate limit.
async fn stats_handler(State(state): State<AppState>) -> axum::Json<StatsResponse> {
    let uptime_secs = state.started_at.elapsed().as_secs();
    let api_calls = count_api_calls(&*state.watches.lock().await);
    let rl = state.rate_limit.lock().await;
    let (active_poll_secs, idle_poll_secs) = compute_intervals(rl.as_ref(), api_calls, unix_now());

    let (rate_remaining, rate_limit, rate_reset_mins) = match rl.as_ref() {
        Some(r) => {
            let reset_mins = r.reset.saturating_sub(unix_now()) / 60;
            (Some(r.remaining), Some(r.limit), Some(reset_mins))
        }
        None => (None, None, None),
    };

    axum::Json(StatsResponse {
        uptime_secs,
        active_poll_secs,
        idle_poll_secs,
        rate_remaining,
        rate_limit,
        rate_reset_mins,
    })
}

// -- Pause / rerun endpoints --

#[derive(Deserialize)]
struct PauseRequest {
    pause: bool,
}

/// `POST /pause` — Toggle notification pause.
async fn pause_handler(
    State(state): State<AppState>,
    axum::Json(body): axum::Json<PauseRequest>,
) -> axum::Json<serde_json::Value> {
    let mut p = state.pause.lock().await;
    if body.pause {
        const INDEFINITE: u64 = u32::MAX as u64;
        *p = Some(tokio::time::Instant::now() + Duration::from_secs(INDEFINITE));
    } else {
        *p = None;
    }
    let paused = p.is_some_and(|d| tokio::time::Instant::now() < d);
    axum::Json(serde_json::json!({ "paused": paused }))
}

#[derive(Deserialize)]
struct RerunRequest {
    repo: String,
    run_id: u64,
}

/// `POST /rerun` — Rerun a GitHub Actions build by run ID.
async fn rerun_handler(
    State(state): State<AppState>,
    axum::Json(body): axum::Json<RerunRequest>,
) -> axum::response::Response {
    use axum::http::StatusCode;
    use axum::response::IntoResponse;

    match state.github.run_rerun(&body.repo, body.run_id, false).await {
        Ok(_) => axum::Json(serde_json::json!({ "ok": true })).into_response(),
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            axum::Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

// -- Watch / unwatch / notifications REST endpoints --

/// Shared logic for adding repos to watch — used by both the MCP tool and REST endpoint.
async fn do_watch_builds(
    watches: &Watches,
    config: &SharedConfig,
    handle: &WatcherHandle,
    rate_limit: &RateLimitState,
    repos: &[String],
) -> Vec<String> {
    let repo_branches: Vec<(String, Vec<String>)> = {
        let cfg = config.lock().await;
        repos
            .iter()
            .map(|repo| (repo.clone(), cfg.branches_for(repo).to_vec()))
            .collect()
    };

    let mut results = Vec::new();
    let mut started_repos: Vec<String> = Vec::new();
    for (repo, branches) in &repo_branches {
        let mut any_started = false;
        for branch in branches {
            match start_watch(watches, config, handle, rate_limit, repo, branch).await {
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

    if !started_repos.is_empty() {
        handle
            .persistence
            .save_watches(&collect_persisted(watches).await)
            .await;
        let snapshot = {
            let mut cfg = config.lock().await;
            cfg.add_repos(&started_repos);
            cfg.clone()
        };
        if let Some(warning) = persist_config(&*handle.persistence, snapshot).await {
            results.push(warning);
        }
    }

    results
}

/// Shared logic for removing repos from watch — used by both the MCP tool and REST endpoint.
async fn do_stop_watches(
    watches: &Watches,
    config: &SharedConfig,
    handle: &WatcherHandle,
    repos: &[String],
) -> Vec<String> {
    let removed_counts: Vec<(String, usize)> = {
        let mut w = watches.lock().await;
        repos
            .iter()
            .map(|repo| {
                let keys: Vec<WatchKey> =
                    w.keys().filter(|k| k.matches_repo(repo)).cloned().collect();
                for key in &keys {
                    w.remove(key);
                }
                (repo.clone(), keys.len())
            })
            .collect()
    };
    handle
        .persistence
        .save_watches(&collect_persisted(watches).await)
        .await;

    let (snapshot, mut results) = {
        let mut cfg = config.lock().await;
        let mut results = Vec::new();
        for (repo, branch_count) in removed_counts {
            let was_in_config = cfg.repos.contains_key(&repo);
            cfg.repos.remove(&repo);
            let msg = match (branch_count, was_in_config) {
                (n, _) if n > 0 => format!("Stopped watching {repo} ({n} branches)"),
                (_, true) => format!("{repo}: removed from config (was not actively polling)"),
                _ => format!("{repo}: not found"),
            };
            results.push(msg);
        }
        (cfg.clone(), results)
    };
    if let Some(warning) = persist_config(&*handle.persistence, snapshot).await {
        results.push(warning);
    }

    results
}

/// Shared logic for updating which branches are watched for a repo.
///
/// Stops watches for branches no longer in the list, starts watches for new
/// branches, updates config, and persists both.
async fn do_configure_branches(
    watches: &Watches,
    config: &SharedConfig,
    handle: &WatcherHandle,
    rate_limit: &RateLimitState,
    repo: &str,
    new_branches: Vec<String>,
) -> Vec<String> {
    let mut results = Vec::new();

    // Current branches from live watches.
    let current_branches: Vec<String> = {
        let w = watches.lock().await;
        w.keys()
            .filter(|k| k.matches_repo(repo))
            .map(|k| k.branch.clone())
            .collect()
    };

    // Stop watches for removed branches.
    {
        let mut w = watches.lock().await;
        for branch in &current_branches {
            if !new_branches.contains(branch) {
                let key = WatchKey::new(repo, branch);
                if w.remove(&key).is_some() {
                    results.push(format!("Stopped watching {repo} [{branch}]"));
                }
            }
        }
    }

    // Start watches for new branches.
    for branch in &new_branches {
        if !current_branches.contains(branch) {
            match start_watch(watches, config, handle, rate_limit, repo, branch).await {
                Ok(msg) => results.push(msg),
                Err(msg) => results.push(msg),
            }
        }
    }

    // Update config and persist.
    {
        let mut cfg = config.lock().await;
        let rc = cfg.repos.entry(repo.to_string()).or_default();
        rc.branches = new_branches;
    }
    handle
        .persistence
        .save_watches(&collect_persisted(watches).await)
        .await;
    let snapshot = config.lock().await.clone();
    if let Some(warning) = persist_config(&*handle.persistence, snapshot).await {
        results.push(warning);
    }

    results
}

#[derive(Deserialize)]
struct WatchRequest {
    repos: Vec<String>,
}

/// `POST /watch` — Start watching one or more repos.
async fn watch_handler(
    State(state): State<AppState>,
    axum::Json(body): axum::Json<WatchRequest>,
) -> axum::response::Response {
    use axum::response::IntoResponse;

    for repo in &body.repos {
        if let Err(e) = validate_repo(repo) {
            return axum::Json(serde_json::json!({ "error": e })).into_response();
        }
    }

    let results = do_watch_builds(
        &state.watches,
        &state.config,
        &state.handle,
        &state.rate_limit,
        &body.repos,
    )
    .await;
    axum::Json(serde_json::json!({ "ok": true, "messages": results })).into_response()
}

/// `POST /unwatch` — Stop watching one or more repos.
async fn unwatch_handler(
    State(state): State<AppState>,
    axum::Json(body): axum::Json<WatchRequest>,
) -> axum::Json<serde_json::Value> {
    let results = do_stop_watches(&state.watches, &state.config, &state.handle, &body.repos).await;
    axum::Json(serde_json::json!({ "ok": true, "messages": results }))
}

#[derive(Deserialize)]
struct BranchesRequest {
    repo: String,
    branches: Vec<String>,
}

/// `POST /branches` — Set which branches to watch for a repo.
///
/// Stops watches for branches no longer in the list, starts watches for new
/// branches, and updates the config.
async fn branches_handler(
    State(state): State<AppState>,
    axum::Json(body): axum::Json<BranchesRequest>,
) -> axum::response::Response {
    use axum::response::IntoResponse;

    if let Err(e) = validate_repo(&body.repo) {
        return axum::Json(serde_json::json!({ "error": e })).into_response();
    }
    for b in &body.branches {
        if let Err(e) = validate_branch(b) {
            return axum::Json(serde_json::json!({ "error": e })).into_response();
        }
    }
    if body.branches.is_empty() {
        return axum::Json(serde_json::json!({ "error": "branches must not be empty" }))
            .into_response();
    }

    let results = do_configure_branches(
        &state.watches,
        &state.config,
        &state.handle,
        &state.rate_limit,
        &body.repo,
        body.branches,
    )
    .await;
    axum::Json(serde_json::json!({ "ok": true, "messages": results })).into_response()
}

#[derive(Deserialize)]
struct NotificationsQuery {
    repo: String,
    branch: String,
}

/// `GET /notifications` — Resolved notification config for a specific repo/branch.
async fn get_notifications_handler(
    State(state): State<AppState>,
    Query(q): Query<NotificationsQuery>,
) -> axum::Json<NotificationConfig> {
    let cfg = state.config.lock().await;
    axum::Json(cfg.notifications_for(&q.repo, &q.branch))
}

#[derive(Deserialize)]
struct NotificationsRequest {
    repo: String,
    /// Optional branch — when set, mute/unmute applies to that branch only.
    #[serde(default)]
    branch: Option<String>,
    /// "mute" sets all levels to off; "unmute" clears overrides; "set_levels" sets per-event levels.
    action: String,
    #[serde(default)]
    build_started: Option<NotificationLevel>,
    #[serde(default)]
    build_success: Option<NotificationLevel>,
    #[serde(default)]
    build_failure: Option<NotificationLevel>,
}

/// `POST /notifications` — Mute, unmute, or set per-event levels for repo/branch notifications.
async fn notifications_handler(
    State(state): State<AppState>,
    axum::Json(body): axum::Json<NotificationsRequest>,
) -> axum::response::Response {
    use axum::response::IntoResponse;

    let (snapshot, msg) = {
        let mut cfg = state.config.lock().await;
        let Some(rc) = cfg.repos.get_mut(&body.repo) else {
            return axum::Json(
                serde_json::json!({ "error": format!("{}: not being watched", body.repo) }),
            )
            .into_response();
        };
        let all_off = NotificationOverrides {
            build_started: Some(NotificationLevel::Off),
            build_success: Some(NotificationLevel::Off),
            build_failure: Some(NotificationLevel::Off),
        };
        let target_label = if let Some(b) = &body.branch {
            format!("{}/{}", body.repo, b)
        } else {
            body.repo.clone()
        };
        let msg = match (body.action.as_str(), &body.branch) {
            ("mute", Some(branch)) => {
                rc.branch_notifications
                    .entry(branch.clone())
                    .or_default()
                    .notifications = all_off;
                format!("{target_label}: notifications muted")
            }
            ("unmute", Some(branch)) => {
                if let Some(bc) = rc.branch_notifications.get_mut(branch) {
                    bc.notifications = NotificationOverrides::default();
                    if bc == &BranchConfig::default() {
                        rc.branch_notifications.remove(branch);
                    }
                }
                format!("{target_label}: notifications unmuted (using repo/global defaults)")
            }
            ("mute", None) => {
                rc.notifications = all_off;
                format!("{target_label}: notifications muted")
            }
            ("unmute", None) => {
                rc.notifications = NotificationOverrides::default();
                format!("{target_label}: notifications unmuted (using global defaults)")
            }
            ("set_levels", Some(branch)) => {
                let overrides = &mut rc
                    .branch_notifications
                    .entry(branch.clone())
                    .or_default()
                    .notifications;
                apply_level_overrides(
                    overrides,
                    body.build_started,
                    body.build_success,
                    body.build_failure,
                );
                format!("{target_label}: notification levels updated")
            }
            ("set_levels", None) => {
                apply_level_overrides(
                    &mut rc.notifications,
                    body.build_started,
                    body.build_success,
                    body.build_failure,
                );
                format!("{target_label}: notification levels updated")
            }
            (other, _) => {
                return axum::Json(
                    serde_json::json!({ "error": format!("unknown action: {other:?}") }),
                )
                .into_response();
            }
        };
        (cfg.clone(), msg)
    };
    if let Some(warning) = persist_config(&*state.handle.persistence, snapshot).await {
        return axum::Json(serde_json::json!({ "ok": true, "message": msg, "warning": warning }))
            .into_response();
    }
    axum::Json(serde_json::json!({ "ok": true, "message": msg })).into_response()
}

#[derive(Serialize)]
struct DefaultsResponse {
    default_branches: Vec<String>,
    ignored_workflows: Vec<String>,
}

/// `GET /defaults` — Read global default config (branches, ignored workflows).
async fn get_defaults_handler(State(state): State<AppState>) -> axum::Json<DefaultsResponse> {
    let cfg = state.config.lock().await;
    axum::Json(DefaultsResponse {
        default_branches: cfg.default_branches.clone(),
        ignored_workflows: cfg.ignored_workflows.clone(),
    })
}

#[derive(Deserialize)]
struct SetDefaultsRequest {
    #[serde(default)]
    default_branches: Option<Vec<String>>,
    #[serde(default)]
    ignored_workflows: Option<Vec<String>>,
}

/// `POST /defaults` — Update global default config fields.
async fn set_defaults_handler(
    State(state): State<AppState>,
    axum::Json(body): axum::Json<SetDefaultsRequest>,
) -> axum::response::Response {
    use axum::response::IntoResponse;

    let (snapshot, messages) = {
        let mut cfg = state.config.lock().await;
        let mut messages = Vec::new();
        if let Some(branches) = body.default_branches {
            for b in &branches {
                if let Err(e) = validate_branch(b) {
                    return axum::Json(serde_json::json!({ "error": e })).into_response();
                }
            }
            if branches.is_empty() {
                return axum::Json(
                    serde_json::json!({ "error": "default_branches must not be empty" }),
                )
                .into_response();
            }
            cfg.default_branches = branches.clone();
            messages.push(format!("default branches: {}", branches.join(", ")));
        }
        if let Some(workflows) = body.ignored_workflows {
            cfg.ignored_workflows = workflows.clone();
            if workflows.is_empty() {
                messages.push("ignored workflows cleared".to_string());
            } else {
                messages.push(format!("ignored workflows: {}", workflows.join(", ")));
            }
        }
        (cfg.clone(), messages)
    };
    if let Some(warning) = persist_config(&*state.handle.persistence, snapshot).await {
        return axum::Json(
            serde_json::json!({ "ok": true, "messages": messages, "warning": warning }),
        )
        .into_response();
    }
    axum::Json(serde_json::json!({ "ok": true, "messages": messages })).into_response()
}

/// `POST /shutdown` — Initiate graceful daemon shutdown.
async fn shutdown_handler(State(state): State<AppState>) -> axum::Json<serde_json::Value> {
    tracing::info!("Shutdown requested via REST API");
    state.handle.cancel.cancel();
    axum::Json(serde_json::json!({ "ok": true, "message": "shutting down" }))
}

/// Bind to the preferred port, trying up to 9 consecutive ports on conflict.
async fn bind_with_fallback(preferred: u16) -> AnyResult<tokio::net::TcpListener> {
    let last = preferred.saturating_add(9);
    for port in preferred..=last {
        match tokio::net::TcpListener::bind(format!("127.0.0.1:{port}")).await {
            Ok(l) => return Ok(l),
            Err(e) if port == last => return Err(e.into()),
            Err(_) => {}
        }
    }
    unreachable!("preferred..=last is never empty")
}

/// Build the axum router with the MCP `StreamableHttpService` and SSE/status routes.
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

    let app_state = AppState {
        watches: watches.clone(),
        config: config.clone(),
        handle: handle.clone(),
        pause: pause.clone(),
        events: handle.events.clone(),
        github: handle.github.clone(),
        rate_limit: rate_limit.clone(),
        started_at,
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

    axum::Router::new()
        .route("/status", get(status_handler))
        .route("/stats", get(stats_handler))
        .route("/events", get(events_handler))
        .route("/pause", axum::routing::post(pause_handler))
        .route("/rerun", axum::routing::post(rerun_handler))
        .route("/watch", axum::routing::post(watch_handler))
        .route("/unwatch", axum::routing::post(unwatch_handler))
        .route(
            "/notifications",
            axum::routing::get(get_notifications_handler).post(notifications_handler),
        )
        .route("/branches", axum::routing::post(branches_handler))
        .route(
            "/defaults",
            axum::routing::get(get_defaults_handler).post(set_defaults_handler),
        )
        .route("/shutdown", axum::routing::post(shutdown_handler))
        .with_state(app_state)
        .nest_service("/mcp", service)
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
) -> AnyResult<()> {
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
    std::fs::write(&port_file, bound_port.to_string())
        .map_err(|e| format!("Failed to write port file {}: {e}", port_file.display()))?;

    if bound_port != port {
        tracing::warn!("Port {port} was occupied, using port {bound_port} instead");
        tracing::warn!("Re-run install.sh to update the MCP URL in ~/.claude.json");
    }
    tracing::info!("build-watcher listening on http://127.0.0.1:{bound_port}/mcp");

    axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {
                    tracing::info!("Ctrl-C received, shutting down...");
                }
                _ = ct.cancelled() => {
                    tracing::info!("Shutdown requested, shutting down...");
                }
            }
            ct.cancel();
        })
        .await?;

    handle.shutdown().await;
    let persisted = collect_persisted(&watches).await;
    handle.persistence.save_watches(&persisted).await;
    let _ = std::fs::remove_file(&port_file);
    tracing::info!("State saved, goodbye.");

    Ok(())
}

async fn persist_config(
    persistence: &dyn Persistence,
    config: build_watcher::config::Config,
) -> Option<String> {
    match persistence.save_config(&config).await {
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

/// Like `deserialize_string_or_vec` but wraps the result in `Some`, and returns `None` for null
/// or absent fields (use with `#[serde(default)]`).
fn deserialize_opt_string_or_vec<'de, D>(deserializer: D) -> Result<Option<Vec<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de;

    struct OptStringOrVec;

    impl<'de> de::Visitor<'de> for OptStringOrVec {
        type Value = Option<Vec<String>>;

        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("a string array, a JSON-encoded string array, or null")
        }

        fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
            serde_json::from_str(v).map(Some).map_err(de::Error::custom)
        }

        fn visit_seq<A: de::SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
            let mut vec = Vec::new();
            while let Some(item) = seq.next_element()? {
                vec.push(item);
            }
            Ok(Some(vec))
        }
    }

    deserializer.deserialize_any(OptStringOrVec)
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
    /// GitHub repo in "owner/repo" format. Omit to set the global default branches.
    repo: Option<String>,
    /// Branches to watch (e.g. `["main", "develop"]`)
    #[serde(deserialize_with = "deserialize_string_or_vec")]
    branches: Vec<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct UpdateNotificationsParams {
    // --- Notification levels ---
    /// Scope: GitHub repo in "owner/repo" format. Omit for global defaults.
    repo: Option<String>,
    /// Scope: branch name. Requires repo.
    branch: Option<String>,
    /// Level for build started events (off, low, normal, critical)
    build_started: Option<NotificationLevel>,
    /// Level for build success events (off, low, normal, critical)
    build_success: Option<NotificationLevel>,
    /// Level for build failure events (off, low, normal, critical)
    build_failure: Option<NotificationLevel>,

    // --- Quiet hours ---
    /// Start of quiet window in HH:MM (24h) local time. Defaults to "22:00".
    quiet_start: Option<String>,
    /// End of quiet window in HH:MM (24h) local time. Defaults to "06:00".
    quiet_end: Option<String>,
    /// Set true to disable quiet hours entirely.
    quiet_clear: Option<bool>,

    // --- Pause control ---
    /// true = pause, false = resume. Combine with pause_minutes for a timed pause.
    pause: Option<bool>,
    /// Minutes to pause (only used when pause=true). Omit for indefinite.
    pause_minutes: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ConfigureRepoParams {
    /// GitHub repo in "owner/repo" format
    repo: String,
    /// Workflow allow-list. Empty = all workflows. Omit to leave unchanged.
    #[serde(default, deserialize_with = "deserialize_opt_string_or_vec")]
    workflows: Option<Vec<String>>,
    /// Display alias for notification titles. Omit to leave unchanged.
    alias: Option<String>,
    /// Set true to clear the alias entirely.
    clear_alias: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ConfigureIgnoredWorkflowsParams {
    /// Workflow names to add to the global ignore list (case-insensitive)
    #[serde(default, deserialize_with = "deserialize_string_or_vec")]
    add: Vec<String>,
    /// Workflow names to remove from the global ignore list (case-insensitive)
    #[serde(default, deserialize_with = "deserialize_string_or_vec")]
    remove: Vec<String>,
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
        for repo in &params.repos {
            if let Err(e) = validate_repo(repo) {
                return Ok(CallToolResult::error(vec![Content::text(e)]));
            }
        }

        let results = do_watch_builds(
            &self.watches,
            &self.config,
            &self.handle,
            &self.rate_limit,
            &params.repos,
        )
        .await;

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
        let results =
            do_stop_watches(&self.watches, &self.config, &self.handle, &params.repos).await;
        Ok(CallToolResult::success(vec![Content::text(
            results.join("\n"),
        )]))
    }

    #[tool(description = "List all currently watched builds and their status")]
    async fn list_watches(&self) -> Result<CallToolResult, McpError> {
        let paused = is_paused(&self.pause).await;
        let watches = self.watches.lock().await;
        if watches.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "No active watches",
            )]));
        }
        let snapshot = build_watch_snapshot(&watches, None, paused, tokio::time::Instant::now());

        let mut lines: Vec<String> = Vec::new();
        if snapshot.paused {
            lines.push("⏸ Notifications paused\n".to_string());
        }

        let watch_lines: Vec<String> = snapshot
            .watches
            .iter()
            .map(|w| {
                let last = w
                    .last_build
                    .as_ref()
                    .map(|b| format!(" (last: {} — {}: {})", b.conclusion, b.workflow, b.title))
                    .unwrap_or_default();

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
                            format!("{}: {} ({}, {time})", r.workflow, r.title, r.status)
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

    #[tool(
        description = "Configure which branches to watch. If repo is given, overrides branches for that repo only. If repo is omitted, sets the global default branches used for repos without per-repo config."
    )]
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

        match params.repo {
            None => {
                let (snapshot, mut msg) = {
                    let mut config = self.config.lock().await;
                    config.default_branches = params.branches;
                    let msg = format!("Default branches set to {:?}", config.default_branches);
                    (config.clone(), msg)
                };
                if let Some(warning) = persist_config(&*self.handle.persistence, snapshot).await {
                    msg.push_str(&warning);
                }
                Ok(CallToolResult::success(vec![Content::text(msg)]))
            }
            Some(repo) => {
                if let Err(e) = validate_repo(&repo) {
                    return Ok(CallToolResult::error(vec![Content::text(e)]));
                }
                let results = do_configure_branches(
                    &self.watches,
                    &self.config,
                    &self.handle,
                    &self.rate_limit,
                    &repo,
                    params.branches,
                )
                .await;
                Ok(CallToolResult::success(vec![Content::text(
                    results.join("\n"),
                )]))
            }
        }
    }

    #[tool(
        description = "Show a live stats snapshot: active builds, polling intervals, \
                       GitHub API rate limit, and notification state (paused / quiet hours)."
    )]
    async fn get_stats(&self) -> Result<CallToolResult, McpError> {
        // Lock order: rate_limit → watches → pause → config (matches poller order).
        let now = unix_now();
        let rl = self.rate_limit.lock().await;
        let (watches_snap, api_calls) = {
            let w = self.watches.lock().await;
            let snap: Vec<(String, usize)> = w
                .iter()
                .map(|(k, e)| (k.to_string(), e.active_runs.len()))
                .collect();
            let calls = count_api_calls(&w);
            (snap, calls)
        };
        let (active_secs, idle_secs) = compute_intervals(rl.as_ref(), api_calls, now);
        let throttled = active_secs > MIN_ACTIVE_SECS || idle_secs > MIN_IDLE_SECS;

        let paused = is_paused(&self.pause).await;
        let (quiet_hours_label, quiet_active, notif_levels, ignored_workflows, repo_count) = {
            let cfg = self.config.lock().await;
            let label = cfg.quiet_hours.as_ref().map_or_else(
                || "off".to_string(),
                |qh| format!("{}–{}", qh.start, qh.end),
            );
            let active = cfg.is_in_quiet_hours();
            let levels = cfg.notifications.clone();
            let ignored = cfg.ignored_workflows.clone();
            let repos = cfg.repos.len();
            (label, active, levels, ignored, repos)
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
            if quiet_active { "quiet" } else { "allowing" },
        ));
        lines.push(format!(
            "  Levels      : started={} success={} failure={}",
            notif_levels.build_started, notif_levels.build_success, notif_levels.build_failure
        ));

        // Settings
        lines.push(String::new());
        lines.push("Settings".to_string());
        lines.push(format!("  Watched repos  : {repo_count}"));
        if ignored_workflows.is_empty() {
            lines.push("  Ignored workflows: (none)".to_string());
        } else {
            lines.push(format!(
                "  Ignored workflows: {}",
                ignored_workflows.join(", ")
            ));
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

    #[tool(
        description = "Configure per-repo settings: workflow allow-list and display alias. \
                       workflows: names to watch (empty = all; omit = no change). \
                       alias: display name in notifications (omit = no change; use clear_alias=true to remove). \
                       Workflow matching is case-insensitive."
    )]
    async fn configure_repo(
        &self,
        Parameters(params): Parameters<ConfigureRepoParams>,
    ) -> Result<CallToolResult, McpError> {
        if let Err(e) = validate_repo(&params.repo) {
            return Ok(CallToolResult::error(vec![Content::text(e)]));
        }
        if params.workflows.is_none() && params.alias.is_none() && params.clear_alias != Some(true)
        {
            return Ok(CallToolResult::error(vec![Content::text(
                "at least one of workflows, alias, or clear_alias must be set",
            )]));
        }

        let (snapshot, mut msgs) = {
            let mut config = self.config.lock().await;
            let Some(rc) = config.repos.get_mut(&params.repo) else {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "{} is not being watched — use watch_builds first",
                    params.repo
                ))]));
            };
            let mut msgs = Vec::new();
            if let Some(workflows) = &params.workflows {
                rc.workflows.clone_from(workflows);
                if workflows.is_empty() {
                    msgs.push(format!("{}: watching all workflows", params.repo));
                } else {
                    msgs.push(format!(
                        "{}: watching workflows {:?}",
                        params.repo, workflows
                    ));
                }
            }
            if params.clear_alias == Some(true) {
                rc.alias = None;
                msgs.push(format!("{}: alias cleared", params.repo));
            } else if let Some(alias) = &params.alias {
                rc.alias = Some(alias.clone());
                msgs.push(format!("{}: alias set to \"{alias}\"", params.repo));
            }
            (config.clone(), msgs)
        };
        if let Some(warning) = persist_config(&*self.handle.persistence, snapshot).await {
            msgs.push(warning);
        }
        Ok(CallToolResult::success(vec![Content::text(
            msgs.join("\n"),
        )]))
    }

    #[tool(
        description = "Add to or remove from the global workflow ignore list. Ignored workflows are \
                       never tracked or notified across all repos. Case-insensitive. \
                       Pass add and/or remove — at least one must be non-empty."
    )]
    async fn configure_ignored_workflows(
        &self,
        Parameters(params): Parameters<ConfigureIgnoredWorkflowsParams>,
    ) -> Result<CallToolResult, McpError> {
        if params.add.is_empty() && params.remove.is_empty() {
            return Ok(CallToolResult::error(vec![Content::text(
                "at least one of add or remove must be non-empty",
            )]));
        }

        let (snapshot, mut msgs) = {
            let mut config = self.config.lock().await;

            let mut added = Vec::new();
            for w in &params.add {
                if !config
                    .ignored_workflows
                    .iter()
                    .any(|existing| existing.eq_ignore_ascii_case(w))
                {
                    config.ignored_workflows.push(w.clone());
                    added.push(w.as_str());
                }
            }

            let before = config.ignored_workflows.len();
            config.ignored_workflows.retain(|existing| {
                !params
                    .remove
                    .iter()
                    .any(|w| w.eq_ignore_ascii_case(existing))
            });
            let removed = before - config.ignored_workflows.len();

            let mut msgs = Vec::new();
            if !added.is_empty() {
                msgs.push(format!("Added to ignore list: {}", added.join(", ")));
            } else if !params.add.is_empty() {
                msgs.push("All specified workflows were already ignored".to_string());
            }
            if removed > 0 {
                msgs.push(format!("Removed from ignore list: {removed} workflow(s)"));
            } else if !params.remove.is_empty() {
                msgs.push("None of the specified workflows were in the ignore list".to_string());
            }
            if config.ignored_workflows.is_empty() {
                msgs.push("No workflows are globally ignored now.".to_string());
            } else {
                msgs.push(format!("Ignored: {:?}", config.ignored_workflows));
            }

            (config.clone(), msgs)
        };
        if let Some(warning) = persist_config(&*self.handle.persistence, snapshot).await {
            msgs.push(warning);
        }

        Ok(CallToolResult::success(vec![Content::text(
            msgs.join("\n"),
        )]))
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
            let in_memory = {
                let watches = self.watches.lock().await;
                last_failed_build(&watches, &params.repo).map(|(key, build)| {
                    tracing::info!(
                        repo = params.repo,
                        branch = key.branch,
                        run_id = build.run_id,
                        "Rerunning last failed build (from memory)"
                    );
                    build.run_id
                })
            };

            if let Some(id) = in_memory {
                id
            } else {
                // Fall back to GitHub API history
                tracing::debug!(
                    repo = params.repo,
                    "No in-memory failed build; querying GitHub history"
                );
                match self
                    .handle
                    .github
                    .run_list_history(&params.repo, None, 20)
                    .await
                {
                    Ok(entries) => match entries.into_iter().find(|e| e.conclusion == "failure") {
                        Some(entry) => {
                            tracing::info!(
                                repo = params.repo,
                                run_id = entry.id,
                                "Rerunning last failed build (from GitHub history)"
                            );
                            entry.id
                        }
                        None => {
                            return Ok(CallToolResult::error(vec![Content::text(format!(
                                "No recent failed build found for {}",
                                params.repo
                            ))]));
                        }
                    },
                    Err(e) => {
                        return Ok(CallToolResult::error(vec![Content::text(format!(
                            "No in-memory failed build and GitHub history lookup failed: {e}"
                        ))]));
                    }
                }
            }
        };

        match self
            .handle
            .github
            .run_rerun(&params.repo, run_id, params.failed_only)
            .await
        {
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
        let entries = match self
            .handle
            .github
            .run_list_history(&params.repo, params.branch.as_deref(), limit)
            .await
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

        let now = unix_now();
        for entry in &entries {
            let duration = entry
                .duration_secs()
                .map_or_else(|| "—".to_string(), format::seconds);
            let age = entry
                .age_secs(now)
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
        description = "Update notification settings in one call — any combination of params. \
                       Levels: set build_started/success/failure with optional repo/branch scope (global if omitted). \
                       Quiet hours: quiet_start + quiet_end in HH:MM local time (defaults 22:00–06:00), or quiet_clear=true to disable. \
                       Pause: pause=true to pause (add pause_minutes for timed), pause=false to resume. \
                       Levels: off, low, normal, critical."
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
            let mut p = self.pause.lock().await;
            if pause {
                let msg = match params.pause_minutes {
                    Some(mins) if mins > 0 => {
                        *p = Some(
                            tokio::time::Instant::now() + std::time::Duration::from_secs(mins * 60),
                        );
                        format!("Notifications paused for {mins} minutes")
                    }
                    _ => {
                        const INDEFINITE: u64 = u32::MAX as u64; // ~136 years
                        *p = Some(
                            tokio::time::Instant::now()
                                + std::time::Duration::from_secs(INDEFINITE),
                        );
                        "Notifications paused indefinitely".to_string()
                    }
                };
                msgs.push(msg);
            } else {
                let was_paused = p.is_some_and(|d| tokio::time::Instant::now() < d);
                *p = None;
                msgs.push(if was_paused {
                    "Notifications resumed".to_string()
                } else {
                    "Notifications were not paused".to_string()
                });
            }
        }

        // Quiet hours + notification levels (both touch config)
        if has_levels || has_quiet {
            let (snapshot, scope, effective) = {
                let mut config = self.config.lock().await;

                // Quiet hours
                if params.quiet_clear == Some(true) {
                    config.quiet_hours = None;
                    msgs.push("Quiet hours cleared".to_string());
                } else if has_quiet {
                    let start = params.quiet_start.as_deref().unwrap_or("22:00").to_string();
                    let end = params.quiet_end.as_deref().unwrap_or("06:00").to_string();
                    config.quiet_hours = Some(QuietHours {
                        start: start.clone(),
                        end: end.clone(),
                    });
                    msgs.push(format!("Quiet hours set: {start}–{end} (local time)"));
                }

                // Notification levels
                let (scope, effective) = if has_levels {
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
                    (scope, Some(effective))
                } else {
                    (String::new(), None)
                };

                (config.clone(), scope, effective)
            };

            if let Some(eff) = effective {
                msgs.push(format!(
                    "Updated notifications for {scope}:\n  build_started: {}\n  build_success: {}\n  build_failure: {}",
                    eff.build_started, eff.build_success, eff.build_failure,
                ));
            }

            if let Some(warning) = persist_config(&*self.handle.persistence, snapshot).await {
                msgs.push(warning);
            }
        }

        Ok(CallToolResult::success(vec![Content::text(
            msgs.join("\n"),
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
                 Use configure_branches to set which branches to watch — omit repo to set global defaults, or pass repo to override for a specific repo. \
                 Use configure_repo to set per-repo workflow allow-list and/or display alias. \
                 Use configure_ignored_workflows(add/remove) to manage the global workflow ignore list (e.g. Semgrep, Dependabot). \
                 Use update_notifications to set notification levels (off/low/normal/critical, per event and scope), \
                 configure quiet hours (quiet_start/quiet_end in HH:MM, or quiet_clear=true), \
                 or pause/resume (pause=true/false, with optional pause_minutes). \
                 Use rerun_build to rerun a failed build (or the last failed build for a repo). \
                 Use build_history to see recent builds for a repo. \
                 Use get_stats for a live snapshot of polling, rate limit, notification state, and config file path.",
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
    notif: &mut build_watcher::config::NotificationConfig,
    params: &UpdateNotificationsParams,
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

/// Apply optional per-event levels to a `NotificationOverrides` struct.
///
/// Only fields present (`Some`) are updated; `None` fields are left unchanged.
fn apply_level_overrides(
    overrides: &mut NotificationOverrides,
    started: Option<NotificationLevel>,
    success: Option<NotificationLevel>,
    failure: Option<NotificationLevel>,
) {
    if let Some(l) = started {
        overrides.build_started = Some(l);
    }
    if let Some(l) = success {
        overrides.build_success = Some(l);
    }
    if let Some(l) = failure {
        overrides.build_failure = Some(l);
    }
}

/// Apply notification level params to an override struct (sets Option values).
fn apply_notification_overrides(
    overrides: &mut NotificationOverrides,
    params: &UpdateNotificationsParams,
) {
    apply_level_overrides(
        overrides,
        params.build_started,
        params.build_success,
        params.build_failure,
    );
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

#[cfg(test)]
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
    use build_watcher::config::{NotificationLevel, NotificationOverrides};

    fn deser(json: &str) -> Result<Vec<String>, serde_json::Error> {
        let mut de = serde_json::Deserializer::from_str(json);
        deserialize_string_or_vec(&mut de)
    }

    #[test]
    fn deserialize_string_or_vec_variants() {
        assert_eq!(deser(r#"["a","b"]"#).unwrap(), ["a", "b"]);
        assert_eq!(deser(r#""[\"a\",\"b\"]""#).unwrap(), ["a", "b"]);
        assert!(deser(r#"[]"#).unwrap().is_empty());
        assert!(deser(r#""not json""#).is_err());
    }

    fn deser_opt(json: &str) -> Result<Option<Vec<String>>, serde_json::Error> {
        let mut de = serde_json::Deserializer::from_str(json);
        super::deserialize_opt_string_or_vec(&mut de)
    }

    #[test]
    fn deserialize_opt_string_or_vec_variants() {
        assert_eq!(
            deser_opt(r#"["a","b"]"#).unwrap(),
            Some(vec!["a".to_string(), "b".to_string()])
        );
        assert_eq!(
            deser_opt(r#""[\"x\"]""#).unwrap(),
            Some(vec!["x".to_string()])
        );
        assert_eq!(deser_opt("null").unwrap(), None);
        assert!(deser_opt(r#""not json""#).is_err());
    }

    #[test]
    fn hhmm_validation() {
        assert!(super::validate_hhmm("00:00").is_ok());
        assert!(super::validate_hhmm("23:59").is_ok());
        assert!(super::validate_hhmm("24:00").is_err());
        assert!(super::validate_hhmm("12:60").is_err());
        assert!(super::validate_hhmm("noon").is_err());
        assert!(super::validate_hhmm("12").is_err());
    }

    #[test]
    fn notification_overrides_formatting() {
        assert_eq!(
            super::format_notification_overrides(&NotificationOverrides::default()),
            ""
        );
        assert_eq!(
            super::format_notification_overrides(&NotificationOverrides {
                build_started: Some(NotificationLevel::Off),
                build_success: Some(NotificationLevel::Normal),
                build_failure: Some(NotificationLevel::Critical),
            }),
            "started: off, success: normal, failure: critical"
        );
        assert_eq!(
            super::format_notification_overrides(&NotificationOverrides {
                build_failure: Some(NotificationLevel::Low),
                ..Default::default()
            }),
            "failure: low"
        );
    }

    fn notif_params(
        started: Option<NotificationLevel>,
        success: Option<NotificationLevel>,
        failure: Option<NotificationLevel>,
    ) -> super::UpdateNotificationsParams {
        super::UpdateNotificationsParams {
            repo: None,
            branch: None,
            build_started: started,
            build_success: success,
            build_failure: failure,
            quiet_start: None,
            quiet_end: None,
            quiet_clear: None,
            pause: None,
            pause_minutes: None,
        }
    }

    #[test]
    fn apply_notification_levels_selective() {
        let mut notif = build_watcher::config::NotificationConfig::default();
        let params = notif_params(
            Some(NotificationLevel::Off),
            None,
            Some(NotificationLevel::Low),
        );
        super::apply_notification_levels(&mut notif, &params);
        assert_eq!(notif.build_started, NotificationLevel::Off);
        assert_eq!(notif.build_success, NotificationLevel::Normal); // unchanged
        assert_eq!(notif.build_failure, NotificationLevel::Low);
    }

    #[test]
    fn apply_notification_overrides_selective() {
        let mut overrides = NotificationOverrides::default();
        let params = notif_params(None, Some(NotificationLevel::Critical), None);
        super::apply_notification_overrides(&mut overrides, &params);
        assert_eq!(overrides.build_started, None); // unchanged
        assert_eq!(overrides.build_success, Some(NotificationLevel::Critical));
        assert_eq!(overrides.build_failure, None); // unchanged
    }

    // -- SSE / status endpoint tests --

    use build_watcher::events::{EventBus, RunSnapshot, WatchEvent};
    use build_watcher::watcher::{PauseState, WatchEntry, WatchKey, Watches};
    use std::collections::HashMap;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    fn empty_state() -> (Watches, PauseState, EventBus) {
        let watches = Arc::new(Mutex::new(HashMap::new()));
        let pause: PauseState = Arc::new(Mutex::new(None));
        let events = EventBus::new();
        (watches, pause, events)
    }

    struct StubGitHub;

    #[async_trait::async_trait]
    impl build_watcher::github::GitHubClient for StubGitHub {
        async fn recent_runs(
            &self,
            _: &str,
            _: &str,
        ) -> Result<Vec<build_watcher::github::RunInfo>, build_watcher::github::GhError> {
            Ok(vec![])
        }
        async fn run_status(
            &self,
            _: &str,
            _: u64,
        ) -> Result<build_watcher::github::RunInfo, build_watcher::github::GhError> {
            Err(build_watcher::github::GhError::MissingFields {
                repo: "stub".to_string(),
            })
        }
        async fn run_rerun(
            &self,
            _: &str,
            _: u64,
            _: bool,
        ) -> Result<String, build_watcher::github::GhError> {
            Ok(String::new())
        }
        async fn run_list_history(
            &self,
            _: &str,
            _: Option<&str>,
            _: u32,
        ) -> Result<Vec<build_watcher::github::HistoryEntry>, build_watcher::github::GhError>
        {
            Ok(vec![])
        }
        async fn rate_limit(
            &self,
        ) -> Result<build_watcher::github::RateLimit, build_watcher::github::GhError> {
            Err(build_watcher::github::GhError::MissingFields {
                repo: "stub".to_string(),
            })
        }
        async fn failing_steps(&self, _: &str, _: u64) -> Option<String> {
            None
        }
    }

    fn stub_handle() -> build_watcher::watcher::WatcherHandle {
        build_watcher::watcher::WatcherHandle::new(
            tokio_util::sync::CancellationToken::new(),
            EventBus::new(),
            Arc::new(StubGitHub),
            Arc::new(build_watcher::persistence::NullPersistence),
        )
    }

    fn test_router(watches: Watches, pause: PauseState, events: EventBus) -> axum::Router {
        let handle = stub_handle();
        let app_state = super::AppState {
            watches,
            config: Arc::new(Mutex::new(build_watcher::config::Config::default())),
            handle,
            pause,
            events,
            github: Arc::new(StubGitHub),
            rate_limit: Arc::new(Mutex::new(None)),
            started_at: std::time::Instant::now(),
        };
        axum::Router::new()
            .route("/status", axum::routing::get(super::status_handler))
            .route("/events", axum::routing::get(super::events_handler))
            .with_state(app_state)
    }

    fn notifications_test_router(
        config: Arc<Mutex<build_watcher::config::Config>>,
    ) -> axum::Router {
        let (watches, pause, events) = empty_state();
        let handle = stub_handle();
        let app_state = super::AppState {
            watches,
            config,
            handle,
            pause,
            events,
            github: Arc::new(StubGitHub),
            rate_limit: Arc::new(Mutex::new(None)),
            started_at: std::time::Instant::now(),
        };
        axum::Router::new()
            .route(
                "/notifications",
                axum::routing::get(super::get_notifications_handler)
                    .post(super::notifications_handler),
            )
            .with_state(app_state)
    }

    async fn get_status_json(router: axum::Router) -> serde_json::Value {
        use http_body_util::BodyExt;
        use tower::ServiceExt;

        let req = http::Request::get("/status")
            .body(axum::body::Body::empty())
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 200);
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap()
    }

    fn snap() -> RunSnapshot {
        RunSnapshot {
            repo: "alice/app".to_string(),
            branch: "main".to_string(),
            run_id: 42,
            workflow: "CI".to_string(),
            title: "Fix bug".to_string(),
            event: "push".to_string(),
            status: "in_progress".to_string(),
        }
    }

    #[tokio::test]
    async fn status_empty_watches() {
        let (watches, pause, events) = empty_state();
        let json = get_status_json(test_router(watches, pause, events)).await;
        assert_eq!(json["paused"], false);
        assert_eq!(json["watches"], serde_json::json!([]));
    }

    #[tokio::test]
    async fn status_paused_flag() {
        let (watches, pause, events) = empty_state();
        // Set pause deadline far in the future.
        *pause.lock().await =
            Some(tokio::time::Instant::now() + std::time::Duration::from_secs(300));
        let json = get_status_json(test_router(watches, pause, events)).await;
        assert_eq!(json["paused"], true);
    }

    #[tokio::test]
    async fn status_with_last_build() {
        use build_watcher::github::LastBuild;

        let (watches, pause, events) = empty_state();
        let key = WatchKey::new("alice/app", "main");
        let mut entry = WatchEntry::default();
        entry.last_build = Some(LastBuild {
            run_id: 99,
            conclusion: "failure".to_string(),
            workflow: "CI".to_string(),
            title: "Initial commit".to_string(),
            head_sha: "abc1234".to_string(),
            event: "push".to_string(),
            failing_steps: Some("Build / Run tests".to_string()),
        });
        watches.lock().await.insert(key, entry);

        let json = get_status_json(test_router(watches, pause, events)).await;
        let watches_arr = &json["watches"];
        assert_eq!(watches_arr.as_array().unwrap().len(), 1);
        let w = &watches_arr[0];
        assert_eq!(w["repo"], "alice/app");
        assert_eq!(w["branch"], "main");
        assert_eq!(w["active_runs"], serde_json::json!([]));
        assert_eq!(w["last_build"]["run_id"], 99);
        assert_eq!(w["last_build"]["conclusion"], "failure");
        assert_eq!(w["last_build"]["title"], "Initial commit");
        assert_eq!(w["last_build"]["failing_steps"], "Build / Run tests");
    }

    #[tokio::test]
    async fn status_watches_sorted() {
        let (watches, pause, events) = empty_state();
        {
            let mut w = watches.lock().await;
            w.insert(WatchKey::new("zoo/bar", "main"), WatchEntry::default());
            w.insert(WatchKey::new("alice/app", "main"), WatchEntry::default());
            w.insert(WatchKey::new("alice/app", "develop"), WatchEntry::default());
        }
        let json = get_status_json(test_router(watches, pause, events)).await;
        let repos: Vec<&str> = json["watches"]
            .as_array()
            .unwrap()
            .iter()
            .map(|w| w["repo"].as_str().unwrap())
            .collect();
        // alice/app entries come before zoo/bar
        assert_eq!(repos[0], "alice/app");
        assert_eq!(repos[1], "alice/app");
        assert_eq!(repos[2], "zoo/bar");
        // develop before main for alice/app
        assert_eq!(json["watches"][0]["branch"], "develop");
        assert_eq!(json["watches"][1]["branch"], "main");
    }

    fn test_router_full(watches: Watches, pause: PauseState, events: EventBus) -> axum::Router {
        let handle = stub_handle();
        let app_state = super::AppState {
            watches,
            config: Arc::new(Mutex::new(build_watcher::config::Config::default())),
            handle,
            pause,
            events,
            github: Arc::new(StubGitHub),
            rate_limit: Arc::new(Mutex::new(None)),
            started_at: std::time::Instant::now(),
        };
        axum::Router::new()
            .route("/status", axum::routing::get(super::status_handler))
            .route("/stats", axum::routing::get(super::stats_handler))
            .route("/pause", axum::routing::post(super::pause_handler))
            .route("/events", axum::routing::get(super::events_handler))
            .with_state(app_state)
    }

    #[tokio::test]
    async fn stats_returns_uptime_and_intervals() {
        use http_body_util::BodyExt;
        use tower::ServiceExt;

        let (watches, pause, events) = empty_state();
        let router = test_router_full(watches, pause, events);
        let req = http::Request::get("/stats")
            .body(axum::body::Body::empty())
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 200);
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        // Uptime should be very small (just started)
        assert!(json["uptime_secs"].as_u64().unwrap() < 5);
        // Without rate limit data, should use fallback intervals
        assert_eq!(json["active_poll_secs"], 30);
        assert_eq!(json["idle_poll_secs"], 120);
        assert!(json["rate_remaining"].is_null());
    }

    #[tokio::test]
    async fn pause_toggle() {
        use http_body_util::BodyExt;
        use tower::ServiceExt;

        let (watches, pause, events) = empty_state();

        // Pause
        let router = test_router_full(watches.clone(), pause.clone(), events.clone());
        let req = http::Request::post("/pause")
            .header("content-type", "application/json")
            .body(axum::body::Body::from(r#"{"pause":true}"#))
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 200);
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["paused"], true);

        // Resume
        let router = test_router_full(watches.clone(), pause.clone(), events.clone());
        let req = http::Request::post("/pause")
            .header("content-type", "application/json")
            .body(axum::body::Body::from(r#"{"pause":false}"#))
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 200);
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["paused"], false);
    }

    #[tokio::test]
    async fn status_with_active_runs() {
        use build_watcher::watcher::ActiveRun;
        use tokio::time::Instant;

        let (watches, pause, events) = empty_state();
        let key = WatchKey::new("alice/app", "main");
        let mut entry = WatchEntry::default();
        entry.active_runs.insert(
            42,
            ActiveRun {
                status: "in_progress".to_string(),
                started_at: Instant::now(),
                workflow: "CI".to_string(),
                title: "Fix bug".to_string(),
                event: "push".to_string(),
            },
        );
        watches.lock().await.insert(key, entry);

        let json = get_status_json(test_router(watches, pause, events)).await;
        let runs = &json["watches"][0]["active_runs"];
        assert_eq!(runs.as_array().unwrap().len(), 1);
        assert_eq!(runs[0]["run_id"], 42);
        assert_eq!(runs[0]["status"], "in_progress");
        assert_eq!(runs[0]["workflow"], "CI");
        assert!(runs[0]["elapsed_secs"].as_f64().is_some());
    }

    #[tokio::test]
    async fn events_returns_text_event_stream() {
        use tower::ServiceExt;

        let (watches, pause, events) = empty_state();
        let router = test_router(watches, pause, events);
        let req = http::Request::get("/events")
            .body(axum::body::Body::empty())
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 200);
        let content_type = resp.headers()["content-type"].to_str().unwrap();
        assert!(
            content_type.contains("text/event-stream"),
            "got: {content_type}"
        );
    }

    #[tokio::test]
    async fn events_streams_run_started() {
        use http_body_util::BodyExt;
        use tower::ServiceExt;

        let (watches, pause, events) = empty_state();
        let router = test_router(watches, pause, events.clone());

        // Make the request — the handler subscribes to the event bus synchronously
        // before returning, so any events emitted after this point will be received.
        let req = http::Request::get("/events")
            .body(axum::body::Body::empty())
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 200);

        // Emit an event now that the SSE stream is subscribed.
        events.emit(WatchEvent::RunStarted(snap()));

        // Read body chunks until we find our SSE frame (with a timeout).
        let mut body = resp.into_body();
        let frame_text = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if let Some(Ok(frame)) = body.frame().await {
                    if let Ok(data) = frame.into_data() {
                        let text = String::from_utf8_lossy(&data).into_owned();
                        if !text.trim().is_empty() {
                            return text;
                        }
                    }
                }
            }
        })
        .await
        .expect("timed out waiting for SSE frame");

        assert!(
            frame_text.contains("RunStarted"),
            "expected 'RunStarted' in frame, got: {frame_text:?}"
        );
        assert!(
            frame_text.contains("alice/app"),
            "expected repo in frame, got: {frame_text:?}"
        );
    }

    #[tokio::test]
    async fn get_notifications_returns_resolved_config() {
        use http_body_util::BodyExt;
        use tower::ServiceExt;

        let mut config = build_watcher::config::Config::default();
        config.repos.insert(
            "alice/app".to_string(),
            build_watcher::config::RepoConfig {
                notifications: NotificationOverrides {
                    build_started: Some(NotificationLevel::Off),
                    build_success: None,
                    build_failure: Some(NotificationLevel::Low),
                },
                ..Default::default()
            },
        );
        let config = Arc::new(Mutex::new(config));
        let router = notifications_test_router(config);

        let req = http::Request::get("/notifications?repo=alice%2Fapp&branch=main")
            .body(axum::body::Body::empty())
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 200);
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["build_started"], "off");
        assert_eq!(body["build_success"], "normal"); // global default
        assert_eq!(body["build_failure"], "low");
    }

    #[tokio::test]
    async fn post_notifications_set_levels() {
        use http_body_util::BodyExt;
        use tower::ServiceExt;

        let mut config = build_watcher::config::Config::default();
        config.repos.insert(
            "alice/app".to_string(),
            build_watcher::config::RepoConfig::default(),
        );
        let config = Arc::new(Mutex::new(config));
        let router = notifications_test_router(config.clone());

        let body = serde_json::json!({
            "repo": "alice/app",
            "branch": "main",
            "action": "set_levels",
            "build_started": "off",
            "build_failure": "critical",
        });
        let req = http::Request::post("/notifications")
            .header("content-type", "application/json")
            .body(axum::body::Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 200);
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let resp_body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(resp_body["ok"], true);

        let cfg = config.lock().await;
        let rc = cfg.repos.get("alice/app").unwrap();
        let bn = rc.branch_notifications.get("main").unwrap();
        assert_eq!(bn.notifications.build_started, Some(NotificationLevel::Off));
        assert_eq!(bn.notifications.build_success, None); // not set
        assert_eq!(
            bn.notifications.build_failure,
            Some(NotificationLevel::Critical)
        );
    }
}
