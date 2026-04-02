use std::convert::Infallible;
use std::time::Duration;

use axum::extract::{Query, State};
use axum::response::IntoResponse as _;
use axum::response::sse::{Event, KeepAlive, Sse};
use serde::Deserialize;
use tokio_stream::StreamExt as _;
use tokio_stream::wrappers::BroadcastStream;

use build_watcher::config::{NotificationConfig, NotificationLevel, PollAggression, unix_now};
use build_watcher::events::WatchEvent;
use build_watcher::github::{validate_branch, validate_repo};
use build_watcher::history::{history_all, history_for};
use build_watcher::rate_limiter::compute_intervals;
use build_watcher::status::{DefaultsConfig, HistoryEntryView, StatsResponse};
use build_watcher::watcher::{count_api_calls, is_paused};

use super::DaemonState;
use super::actions::{
    apply_pause, do_configure_branches, do_merge, do_rerun, do_stop_watches, do_watch_builds,
};
use super::{build_watch_snapshot, json_error};

/// `GET /status` — JSON snapshot of all current watches and their build state.
pub(crate) async fn status_handler(
    State(state): State<DaemonState>,
) -> axum::Json<build_watcher::status::StatusResponse> {
    let paused = is_paused(&state.pause).await;
    let watches = state.watches.lock().await;
    let cfg = state.config.read().await;
    axum::Json(build_watch_snapshot(&watches, Some(&cfg), paused))
}

/// `GET /events` — SSE stream of `WatchEvent`s as they occur.
///
/// Each frame has an event type matching the variant name and a JSON data payload.
/// A keepalive comment is sent every 30 seconds to detect dropped connections.
pub(crate) async fn events_handler(
    State(state): State<DaemonState>,
) -> impl axum::response::IntoResponse {
    let stream = BroadcastStream::new(state.handle.events.subscribe())
        .filter_map(|result| result.ok())
        .map(|event| {
            let event_type = match &event {
                WatchEvent::RunStarted(_) => "RunStarted",
                WatchEvent::RunCompleted { .. } => "RunCompleted",
                WatchEvent::StatusChanged { .. } => "StatusChanged",
                WatchEvent::PrStateChanged { .. } => "PrStateChanged",
            };
            let data = match serde_json::to_string(&event) {
                Ok(d) => d,
                Err(e) => {
                    tracing::error!("Failed to serialize SSE event: {e}");
                    return Ok::<_, Infallible>(
                        Event::default()
                            .event("error")
                            .data(format!("serialization error: {e}")),
                    );
                }
            };
            Ok::<_, Infallible>(Event::default().event(event_type).data(data))
        });

    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(30)))
}

/// `GET /stats` — Daemon stats: uptime, polling intervals, rate limit.
pub(crate) async fn stats_handler(State(state): State<DaemonState>) -> axum::Json<StatsResponse> {
    let uptime_secs = state.started_at.elapsed().as_secs();
    let api_calls = count_api_calls(&*state.watches.lock().await);
    let rl = state.rate_limit.lock().await;
    let aggression = state.config.read().await.poll_aggression;
    let (active_poll_secs, idle_poll_secs) =
        compute_intervals(rl.as_ref(), api_calls, unix_now(), aggression, 0);

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
        poll_aggression: aggression.to_string(),
        rate_remaining,
        rate_limit,
        rate_reset_mins,
        dropped_events: state.handle.events.dropped_count(),
    })
}

// -- Pause / rerun endpoints --

#[derive(Deserialize)]
pub(crate) struct PauseRequest {
    pause: bool,
}

/// `POST /pause` — Toggle notification pause.
pub(crate) async fn pause_handler(
    State(state): State<DaemonState>,
    axum::Json(body): axum::Json<PauseRequest>,
) -> axum::Json<serde_json::Value> {
    let message = apply_pause(&state.pause, body.pause, None).await;
    let paused = {
        let p = state.pause.lock().await;
        p.is_some_and(|d| tokio::time::Instant::now() < d)
    };
    axum::Json(serde_json::json!({ "paused": paused, "message": message }))
}

#[derive(Deserialize)]
pub(crate) struct RerunRequest {
    repo: String,
    run_id: Option<u64>,
    #[serde(default)]
    failed_only: bool,
}

/// `POST /rerun` — Rerun a GitHub Actions build. If `run_id` is omitted, reruns
/// the last failed build (from in-memory watches or GitHub history).
pub(crate) async fn rerun_handler(
    State(state): State<DaemonState>,
    axum::Json(body): axum::Json<RerunRequest>,
) -> axum::response::Response {
    if let Err(e) = validate_repo(&body.repo) {
        return json_error(e);
    }
    match do_rerun(&state, &body.repo, body.run_id, body.failed_only).await {
        Ok(msg) => axum::Json(serde_json::json!({ "ok": true, "message": msg })).into_response(),
        Err(e) => axum::Json(serde_json::json!({ "error": e })).into_response(),
    }
}

#[derive(Deserialize)]
pub(crate) struct MergeRequest {
    repo: String,
    number: u64,
}

/// `POST /merge` — Merge a PR by number.
pub(crate) async fn merge_handler(
    State(state): State<DaemonState>,
    axum::Json(body): axum::Json<MergeRequest>,
) -> axum::response::Response {
    if let Err(e) = validate_repo(&body.repo) {
        return json_error(e);
    }
    match do_merge(&state, &body.repo, body.number).await {
        Ok(msg) => axum::Json(serde_json::json!({ "ok": true, "message": msg })).into_response(),
        Err(e) => axum::Json(serde_json::json!({ "error": e })).into_response(),
    }
}

// -- Watch / unwatch / notifications REST endpoints --

#[derive(Deserialize)]
pub(crate) struct WatchRequest {
    pub repos: Vec<String>,
}

/// `POST /watch` — Start watching one or more repos.
pub(crate) async fn watch_handler(
    State(state): State<DaemonState>,
    axum::Json(body): axum::Json<WatchRequest>,
) -> axum::response::Response {
    for repo in &body.repos {
        if let Err(e) = validate_repo(repo) {
            return json_error(e);
        }
    }

    let results = do_watch_builds(&state, &body.repos).await;
    let messages: Vec<&str> = results.iter().map(|o| o.message()).collect();
    axum::Json(serde_json::json!({ "ok": true, "messages": messages })).into_response()
}

/// `POST /unwatch` — Stop watching one or more repos.
pub(crate) async fn unwatch_handler(
    State(state): State<DaemonState>,
    axum::Json(body): axum::Json<WatchRequest>,
) -> axum::Json<serde_json::Value> {
    let results = do_stop_watches(&state, &body.repos).await;
    let messages: Vec<&str> = results.iter().map(|o| o.message()).collect();
    axum::Json(serde_json::json!({ "ok": true, "messages": messages }))
}

#[derive(Deserialize)]
pub(crate) struct BranchesRequest {
    repo: String,
    branches: Vec<String>,
}

/// `POST /branches` — Set which branches to watch for a repo.
pub(crate) async fn branches_handler(
    State(state): State<DaemonState>,
    axum::Json(body): axum::Json<BranchesRequest>,
) -> axum::response::Response {
    if let Err(e) = validate_repo(&body.repo) {
        return json_error(e);
    }
    for b in &body.branches {
        if let Err(e) = validate_branch(b) {
            return json_error(e);
        }
    }
    if body.branches.is_empty() {
        return json_error("branches must not be empty");
    }

    let results = do_configure_branches(&state, &body.repo, body.branches).await;
    let messages: Vec<&str> = results.iter().map(|o| o.message()).collect();
    axum::Json(serde_json::json!({ "ok": true, "messages": messages })).into_response()
}

#[derive(Deserialize)]
pub(crate) struct NotificationsQuery {
    repo: String,
    branch: String,
}

/// `GET /notifications` — Resolved notification config for a specific repo/branch.
pub(crate) async fn get_notifications_handler(
    State(state): State<DaemonState>,
    Query(q): Query<NotificationsQuery>,
) -> axum::Json<NotificationConfig> {
    let cfg = state.config.read().await;
    axum::Json(cfg.notifications_for(&q.repo, &q.branch))
}

#[derive(Deserialize)]
pub(crate) struct NotificationsRequest {
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
pub(crate) async fn notifications_handler(
    State(state): State<DaemonState>,
    axum::Json(body): axum::Json<NotificationsRequest>,
) -> axum::response::Response {
    use super::actions::do_notification_action;

    let result = state
        .config
        .modify(|cfg| {
            do_notification_action(
                cfg,
                &body.repo,
                body.branch.as_deref(),
                &body.action,
                body.build_started,
                body.build_success,
                body.build_failure,
            )
        })
        .await;
    match result {
        Ok(Ok(msg)) => {
            axum::Json(serde_json::json!({ "ok": true, "message": msg })).into_response()
        }
        Ok(Err(e)) => json_error(e),
        Err(e) => {
            let warning =
                format!("\u{26a0}\u{fe0f} Warning: config could not be saved to disk: {e}");
            axum::Json(serde_json::json!({ "ok": false, "warning": warning })).into_response()
        }
    }
}

/// `GET /defaults` — Read global default config (ignored workflows, poll aggression, auto-discover, branch filter).
pub(crate) async fn get_defaults_handler(
    State(state): State<DaemonState>,
) -> axum::Json<DefaultsConfig> {
    let cfg = state.config.read().await;
    axum::Json(DefaultsConfig {
        ignored_workflows: Some(cfg.ignored_workflows.clone()),
        ignored_events: Some(cfg.ignored_events.clone()),
        poll_aggression: Some(cfg.poll_aggression.to_string()),
        auto_discover_branches: Some(cfg.auto_discover_branches),
        branch_filter: cfg.branch_filter.clone(),
    })
}

/// `POST /defaults` — Update global default config fields.
/// Accepts the same `DefaultsConfig` shape — `None` fields are left unchanged.
pub(crate) async fn set_defaults_handler(
    State(state): State<DaemonState>,
    axum::Json(body): axum::Json<DefaultsConfig>,
) -> axum::response::Response {
    // Validate inputs before taking the config lock.
    if let Some(level) = &body.poll_aggression
        && let Err(e) = level.parse::<PollAggression>()
    {
        return json_error(e);
    }
    if let Some(filter) = &body.branch_filter
        && !filter.is_empty()
        && let Err(e) = regex::Regex::new(filter)
    {
        return json_error(format!("invalid branch filter regex: {e}"));
    }

    let result = state
        .config
        .modify(|cfg| {
            let mut messages = Vec::new();
            if let Some(workflows) = body.ignored_workflows {
                cfg.ignored_workflows = workflows.clone();
                if workflows.is_empty() {
                    messages.push("ignored workflows cleared".to_string());
                } else {
                    messages.push(format!("ignored workflows: {}", workflows.join(", ")));
                }
            }
            if let Some(events) = body.ignored_events {
                cfg.ignored_events = events.clone();
                if events.is_empty() {
                    messages.push("ignored events cleared".to_string());
                } else {
                    messages.push(format!("ignored events: {}", events.join(", ")));
                }
            }
            if let Some(level) = body.poll_aggression {
                let aggression = level
                    .parse::<PollAggression>()
                    .expect("already validated above");
                cfg.poll_aggression = aggression;
                messages.push(format!("poll aggression: {aggression}"));
            }
            if let Some(enabled) = body.auto_discover_branches {
                cfg.auto_discover_branches = enabled;
                messages.push(format!(
                    "auto-discover branches: {}",
                    if enabled { "on" } else { "off" }
                ));
            }
            if let Some(filter) = body.branch_filter {
                if filter.is_empty() {
                    cfg.branch_filter = None;
                    messages.push("branch filter cleared".to_string());
                } else {
                    cfg.branch_filter = Some(filter.clone());
                    messages.push(format!("branch filter: {filter}"));
                }
            }
            messages
        })
        .await;
    match result {
        Ok(messages) => {
            axum::Json(serde_json::json!({ "ok": true, "messages": messages })).into_response()
        }
        Err(e) => {
            let warning =
                format!("\u{26a0}\u{fe0f} Warning: config could not be saved to disk: {e}");
            axum::Json(serde_json::json!({ "ok": true, "messages": [], "warning": warning }))
                .into_response()
        }
    }
}

#[derive(Deserialize)]
pub(crate) struct RepoQuery {
    repo: String,
}

/// `GET /repo-config?repo=owner/name` — Read per-repo config.
pub(crate) async fn get_repo_config_handler(
    State(state): State<DaemonState>,
    Query(q): Query<RepoQuery>,
) -> axum::Json<build_watcher::status::RepoConfigView> {
    let cfg = state.config.read().await;
    let rc = cfg.repos.get(&q.repo);
    axum::Json(build_watcher::status::RepoConfigView {
        repo: q.repo,
        alias: rc.and_then(|r| r.alias.clone()),
        workflows: Some(rc.map(|r| r.workflows.clone()).unwrap_or_default()),
        watch_prs: Some(rc.is_some_and(|r| r.watch_prs)),
        poll_aggression: rc.and_then(|r| r.poll_aggression.map(|a| a.to_string())),
        auto_discover_branches: rc.and_then(|r| r.auto_discover_branches),
        branch_filter: rc.and_then(|r| r.branch_filter.clone()),
    })
}

/// `POST /repo-config` — Update per-repo config fields.
pub(crate) async fn set_repo_config_handler(
    State(state): State<DaemonState>,
    axum::Json(body): axum::Json<build_watcher::status::RepoConfigView>,
) -> axum::response::Response {
    if let Err(e) = validate_repo(&body.repo) {
        return json_error(e);
    }

    let result = state
        .config
        .modify(|cfg| {
            let rc = cfg.repos.entry(body.repo.clone()).or_default();
            let mut messages = Vec::new();
            if let Some(alias) = &body.alias {
                if alias.is_empty() {
                    rc.alias = None;
                    messages.push("alias cleared".to_string());
                } else {
                    rc.alias = Some(alias.clone());
                    messages.push(format!("alias: {alias}"));
                }
            }
            if let Some(workflows) = &body.workflows {
                rc.workflows = workflows.clone();
                if workflows.is_empty() {
                    messages.push("workflow filter cleared".to_string());
                } else {
                    messages.push(format!("workflows: {}", workflows.join(", ")));
                }
            }
            if let Some(watch_prs) = body.watch_prs {
                rc.watch_prs = watch_prs;
                messages.push(format!(
                    "watch PRs: {}",
                    if watch_prs { "on" } else { "off" }
                ));
            }
            if let Some(level) = &body.poll_aggression {
                if level.is_empty() || level == "default" {
                    rc.poll_aggression = None;
                    messages.push("poll aggression: default (global)".to_string());
                } else if let Ok(aggression) = level.parse::<PollAggression>() {
                    rc.poll_aggression = Some(aggression);
                    messages.push(format!("poll aggression: {aggression}"));
                }
            }
            if let Some(enabled) = body.auto_discover_branches {
                rc.auto_discover_branches = Some(enabled);
                messages.push(format!(
                    "auto-discover branches: {}",
                    if enabled { "on" } else { "off" }
                ));
            }
            if let Some(filter) = &body.branch_filter {
                if filter.is_empty() {
                    rc.branch_filter = None;
                    messages.push("branch filter: default (global)".to_string());
                } else {
                    rc.branch_filter = Some(filter.clone());
                    messages.push(format!("branch filter: {filter}"));
                }
            }
            messages
        })
        .await;
    match result {
        Ok(messages) => {
            axum::Json(serde_json::json!({ "ok": true, "messages": messages })).into_response()
        }
        Err(e) => {
            let warning =
                format!("\u{26a0}\u{fe0f} Warning: config could not be saved to disk: {e}");
            axum::Json(serde_json::json!({ "ok": true, "messages": [], "warning": warning }))
                .into_response()
        }
    }
}

/// `POST /shutdown` — Initiate graceful daemon shutdown.
pub(crate) async fn shutdown_handler(
    State(state): State<DaemonState>,
) -> axum::Json<serde_json::Value> {
    tracing::info!("Shutdown requested via REST API");
    state.handle.cancel.cancel();
    axum::Json(serde_json::json!({ "ok": true, "message": "shutting down" }))
}

#[derive(Deserialize)]
pub(crate) struct HistoryQuery {
    repo: String,
    #[serde(default)]
    branch: Option<String>,
    #[serde(default)]
    limit: Option<u32>,
}

/// `GET /history` — Persisted build history for a repo, optionally filtered by branch.
pub(crate) async fn history_handler(
    State(state): State<DaemonState>,
    Query(q): Query<HistoryQuery>,
) -> axum::response::Response {
    let limit = q.limit.unwrap_or(15).min(50) as usize;
    let branch = q.branch.as_deref();
    let now = unix_now();
    let hist = state.handle.history.lock().await;
    let entries = history_for(&hist, &q.repo, branch, limit);
    drop(hist);
    let views: Vec<HistoryEntryView> = entries
        .into_iter()
        .map(|(br, lb)| to_history_view(String::new(), br, lb, now))
        .collect();
    axum::Json(views).into_response()
}

#[derive(Deserialize)]
pub(crate) struct LimitQuery {
    #[serde(default)]
    limit: Option<u32>,
}

/// `GET /history/all` — Recent builds across all repos, ungrouped, newest-first.
pub(crate) async fn history_all_handler(
    State(state): State<DaemonState>,
    Query(q): Query<LimitQuery>,
) -> axum::response::Response {
    let limit = q.limit.unwrap_or(20).min(50) as usize;
    let now = unix_now();
    let hist = state.handle.history.lock().await;
    let entries = history_all(&hist, limit);
    drop(hist);
    let views: Vec<HistoryEntryView> = entries
        .into_iter()
        .map(|(repo, branch, lb)| to_history_view(repo, branch, lb, now))
        .collect();
    axum::Json(views).into_response()
}

fn to_history_view(
    repo: String,
    branch: String,
    lb: build_watcher::github::LastBuild,
    now: u64,
) -> HistoryEntryView {
    let title = lb.display_title();
    let age_secs = lb.completed_at.map(|t| now.saturating_sub(t));
    HistoryEntryView {
        id: lb.run_id,
        conclusion: lb.conclusion,
        workflow: lb.workflow,
        title,
        repo,
        branch,
        event: lb.event,
        duration_secs: lb.duration_secs,
        age_secs,
    }
}

#[cfg(test)]
mod tests {
    use build_watcher::config::{
        ConfigManager, ConfigPersistence, NotificationLevel, SharedConfigManager,
    };
    use build_watcher::events::{EventBus, RunSnapshot, WatchEvent};
    use build_watcher::rate_limiter::MIN_ACTIVE_SECS;
    use build_watcher::watcher::{PauseState, WatchEntry, WatchKey, Watches};
    use std::collections::HashMap;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    fn null_config(config: build_watcher::config::Config) -> SharedConfigManager {
        Arc::new(ConfigManager::new(config, ConfigPersistence::Null))
    }

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
        async fn recent_runs_for_repo(
            &self,
            _: &str,
            _: u32,
        ) -> Result<Vec<build_watcher::github::RunInfo>, build_watcher::github::GhError> {
            Ok(vec![])
        }
        async fn in_progress_runs_for_repo(
            &self,
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
        async fn failing_steps(
            &self,
            _: &str,
            _: u64,
        ) -> Option<build_watcher::github::FailureInfo> {
            None
        }
        async fn list_tags(&self, _: &str) -> Result<Vec<String>, build_watcher::github::GhError> {
            Ok(vec![])
        }
        async fn list_branches(
            &self,
            _: &str,
        ) -> Result<Vec<String>, build_watcher::github::GhError> {
            Ok(vec!["main".to_string()])
        }
        async fn default_branch(&self, _: &str) -> Result<String, build_watcher::github::GhError> {
            Ok("main".to_string())
        }
        async fn open_prs(
            &self,
            _: &str,
        ) -> Result<Vec<build_watcher::github::PrInfo>, build_watcher::github::GhError> {
            Ok(vec![])
        }
        async fn pr_merge(
            &self,
            _: &str,
            _: u64,
        ) -> Result<String, build_watcher::github::GhError> {
            Ok("Merged".to_string())
        }
        async fn run_author(
            &self,
            _: &str,
            _: u64,
        ) -> Option<build_watcher::github::RunAuthorInfo> {
            None
        }
    }

    fn stub_handle() -> build_watcher::watcher::WatcherHandle {
        build_watcher::watcher::WatcherHandle::new(
            tokio_util::sync::CancellationToken::new(),
            EventBus::new(),
            Arc::new(StubGitHub),
            Arc::new(build_watcher::persistence::NullPersistence),
            Arc::new(Mutex::new(HashMap::new())),
            Arc::new(tokio::sync::Notify::new()),
        )
    }

    fn test_router(watches: Watches, pause: PauseState, _events: EventBus) -> axum::Router {
        test_router_with_handle(watches, pause, stub_handle())
    }

    fn test_router_with_handle(
        watches: Watches,
        pause: PauseState,
        handle: build_watcher::watcher::WatcherHandle,
    ) -> axum::Router {
        let app_state = super::super::DaemonState {
            watches,
            config: null_config(build_watcher::config::Config::default()),
            handle,
            pause,
            rate_limit: Arc::new(Mutex::new(None)),
            started_at: std::time::Instant::now(),
        };
        axum::Router::new()
            .route("/status", axum::routing::get(super::status_handler))
            .route("/events", axum::routing::get(super::events_handler))
            .with_state(app_state)
    }

    fn notifications_test_router(config: SharedConfigManager) -> axum::Router {
        let (watches, pause, _events) = empty_state();
        let handle = stub_handle();
        let app_state = super::super::DaemonState {
            watches,
            config,
            handle,
            pause,
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
            status: build_watcher::status::RunStatus::InProgress,
            attempt: 1,
            url: String::new(),
            actor: None,
            commit_author: None,
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
        entry.last_builds.insert(
            "CI".to_string(),
            LastBuild {
                run_id: 99,
                conclusion: "failure".to_string(),
                workflow: "CI".to_string(),
                title: "Initial commit".to_string(),
                head_sha: "abc1234".to_string(),
                event: "push".to_string(),
                failing_steps: Some("Build / Run tests".to_string()),
                failing_job_id: None,
                completed_at: None,
                duration_secs: None,
                attempt: 1,
                url: String::new(),
                actor: None,
                commit_author: None,
            },
        );
        watches.lock().await.insert(key, entry);

        let json = get_status_json(test_router(watches, pause, events)).await;
        let watches_arr = &json["watches"];
        assert_eq!(watches_arr.as_array().unwrap().len(), 1);
        let w = &watches_arr[0];
        assert_eq!(w["repo"], "alice/app");
        assert_eq!(w["branch"], "main");
        assert_eq!(w["active_runs"], serde_json::json!([]));
        let lb = &w["last_builds"][0];
        assert_eq!(lb["run_id"], 99);
        assert_eq!(lb["conclusion"], "failure");
        assert_eq!(lb["title"], "Initial commit");
        assert_eq!(lb["failing_steps"], "Build / Run tests");
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
        assert_eq!(repos[0], "alice/app");
        assert_eq!(repos[1], "alice/app");
        assert_eq!(repos[2], "zoo/bar");
        assert_eq!(json["watches"][0]["branch"], "develop");
        assert_eq!(json["watches"][1]["branch"], "main");
    }

    fn test_router_full(watches: Watches, pause: PauseState, _events: EventBus) -> axum::Router {
        let handle = stub_handle();
        let app_state = super::super::DaemonState {
            watches,
            config: null_config(build_watcher::config::Config::default()),
            handle,
            pause,
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
        assert!(json["uptime_secs"].as_u64().unwrap() < 5);
        // Default aggression is Medium (mult=1.5): fallback = (15×1.5, 30×1.5) = (22, 45)
        assert_eq!(
            json["active_poll_secs"],
            (MIN_ACTIVE_SECS as f64 * 1.5) as u64
        );
        assert_eq!(json["idle_poll_secs"], (30f64 * 1.5) as u64);
        assert!(json["rate_remaining"].is_null());
    }

    #[tokio::test]
    async fn pause_toggle() {
        use http_body_util::BodyExt;
        use tower::ServiceExt;

        let (watches, pause, events) = empty_state();

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
        assert!(
            json["message"].as_str().unwrap().contains("paused"),
            "should include message"
        );

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
        assert!(
            json["message"].as_str().unwrap().contains("resumed"),
            "should include message"
        );
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
                status: build_watcher::status::RunStatus::InProgress,
                workflow: "CI".to_string(),
                title: "Fix bug".to_string(),
                event: "push".to_string(),
                attempt: 1,
                created_at: "2026-01-01T10:00:00Z".to_string(),
                updated_at: "2026-01-01T10:05:00Z".to_string(),
                url: String::new(),
                actor: None,
                commit_author: None,
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

        let (watches, pause, _events) = empty_state();
        let handle = stub_handle();
        let events = handle.events.clone();
        let router = test_router_with_handle(watches, pause, handle);

        let req = http::Request::get("/events")
            .body(axum::body::Body::empty())
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 200);

        events.emit(WatchEvent::RunStarted(snap()));

        let mut body = resp.into_body();
        let frame_text = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if let Some(Ok(frame)) = body.frame().await
                    && let Ok(data) = frame.into_data()
                {
                    let text = String::from_utf8_lossy(&data).into_owned();
                    if !text.trim().is_empty() {
                        return text;
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
        use build_watcher::config::NotificationOverrides;
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
        let config = null_config(config);
        let router = notifications_test_router(config);

        let req = http::Request::get("/notifications?repo=alice%2Fapp&branch=main")
            .body(axum::body::Body::empty())
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 200);
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["build_started"], "off");
        assert_eq!(body["build_success"], "normal");
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
        let config = null_config(config);
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

        let cfg = config.read().await;
        let rc = cfg.repos.get("alice/app").unwrap();
        let bn = rc.branch_notifications.get("main").unwrap();
        assert_eq!(bn.notifications.build_started, Some(NotificationLevel::Off));
        assert_eq!(bn.notifications.build_success, None);
        assert_eq!(
            bn.notifications.build_failure,
            Some(NotificationLevel::Critical)
        );
    }

    // -- Repo config tests --

    fn repo_config_router(config: SharedConfigManager) -> axum::Router {
        let (watches, pause, _events) = empty_state();
        let handle = stub_handle();
        let app_state = super::super::DaemonState {
            watches,
            config,
            handle,
            pause,
            rate_limit: Arc::new(Mutex::new(None)),
            started_at: std::time::Instant::now(),
        };
        axum::Router::new()
            .route(
                "/repo-config",
                axum::routing::get(super::get_repo_config_handler)
                    .post(super::set_repo_config_handler),
            )
            .with_state(app_state)
    }

    async fn json_get(router: &axum::Router, path: &str) -> serde_json::Value {
        use http_body_util::BodyExt;
        use tower::ServiceExt;
        let req = http::Request::get(path)
            .body(axum::body::Body::empty())
            .unwrap();
        let resp = router.clone().oneshot(req).await.unwrap();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap()
    }

    async fn json_post(
        router: &axum::Router,
        path: &str,
        body: &impl serde::Serialize,
    ) -> serde_json::Value {
        use http_body_util::BodyExt;
        use tower::ServiceExt;
        let req = http::Request::post(path)
            .header("content-type", "application/json")
            .body(axum::body::Body::from(serde_json::to_vec(body).unwrap()))
            .unwrap();
        let resp = router.clone().oneshot(req).await.unwrap();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn get_repo_config_returns_defaults_for_unknown_repo() {
        let router = repo_config_router(null_config(build_watcher::config::Config::default()));
        let json = json_get(&router, "/repo-config?repo=alice/app").await;
        assert_eq!(json["repo"], "alice/app");
        assert_eq!(json["watch_prs"], false);
        assert_eq!(json["workflows"], serde_json::json!([]));
        assert!(json["alias"].is_null());
    }

    #[tokio::test]
    async fn get_repo_config_returns_configured_values() {
        let mut cfg = build_watcher::config::Config::default();
        cfg.repos.insert(
            "alice/app".to_string(),
            build_watcher::config::RepoConfig {
                alias: Some("myapp".to_string()),
                workflows: vec!["CI".to_string()],
                watch_prs: true,
                ..Default::default()
            },
        );
        let router = repo_config_router(null_config(cfg));
        let json = json_get(&router, "/repo-config?repo=alice/app").await;
        assert_eq!(json["alias"], "myapp");
        assert_eq!(json["workflows"], serde_json::json!(["CI"]));
        assert_eq!(json["watch_prs"], true);
    }

    #[tokio::test]
    async fn set_repo_config_updates_fields() {
        let config = null_config(build_watcher::config::Config::default());
        let router = repo_config_router(config.clone());

        let body = build_watcher::status::RepoConfigView {
            repo: "alice/app".to_string(),
            alias: Some("myapp".to_string()),
            workflows: Some(vec!["CI".to_string(), "Deploy".to_string()]),
            watch_prs: Some(true),
            poll_aggression: Some("high".to_string()),
            auto_discover_branches: None,
            branch_filter: None,
        };
        let resp = json_post(&router, "/repo-config", &body).await;
        assert_eq!(resp["ok"], true);

        // Verify the config was updated.
        let cfg = config.read().await;
        let rc = cfg.repos.get("alice/app").unwrap();
        assert_eq!(rc.alias.as_deref(), Some("myapp"));
        assert_eq!(rc.workflows, vec!["CI", "Deploy"]);
        assert!(rc.watch_prs);
        assert_eq!(
            rc.poll_aggression,
            Some(build_watcher::config::PollAggression::High)
        );
    }
}
