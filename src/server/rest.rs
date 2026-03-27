use std::convert::Infallible;
use std::time::Duration;

use axum::extract::{Query, State};
use axum::response::IntoResponse as _;
use axum::response::sse::{Event, KeepAlive, Sse};
use serde::{Deserialize, Serialize};
use tokio_stream::StreamExt as _;
use tokio_stream::wrappers::BroadcastStream;

use build_watcher::config::{NotificationConfig, NotificationLevel, PollAggression, unix_now};
use build_watcher::events::WatchEvent;
use build_watcher::github::{validate_branch, validate_repo};
use build_watcher::history::{history_all, history_for};
use build_watcher::rate_limiter::compute_intervals;
use build_watcher::status::{HistoryEntryView, StatsResponse};
use build_watcher::watcher::{count_api_calls, is_paused};

use super::AppState;
use super::actions::{do_configure_branches, do_stop_watches, do_watch_builds, persist_config};
use super::{build_watch_snapshot, json_error};

/// `GET /status` — JSON snapshot of all current watches and their build state.
pub(crate) async fn status_handler(
    State(state): State<AppState>,
) -> axum::Json<build_watcher::status::StatusResponse> {
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
pub(crate) async fn events_handler(
    State(state): State<AppState>,
) -> impl axum::response::IntoResponse {
    let stream = BroadcastStream::new(state.handle.events.subscribe())
        .filter_map(|result| result.ok())
        .map(|event| {
            let event_type = match &event {
                WatchEvent::RunStarted(_) => "RunStarted",
                WatchEvent::RunCompleted { .. } => "RunCompleted",
                WatchEvent::StatusChanged { .. } => "StatusChanged",
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
pub(crate) async fn stats_handler(State(state): State<AppState>) -> axum::Json<StatsResponse> {
    let uptime_secs = state.started_at.elapsed().as_secs();
    let api_calls = count_api_calls(&*state.watches.lock().await);
    let rl = state.rate_limit.lock().await;
    let aggression = state.config.lock().await.poll_aggression;
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
pub(crate) struct RerunRequest {
    repo: String,
    run_id: u64,
}

/// `POST /rerun` — Rerun a GitHub Actions build by run ID.
pub(crate) async fn rerun_handler(
    State(state): State<AppState>,
    axum::Json(body): axum::Json<RerunRequest>,
) -> axum::response::Response {
    use axum::http::StatusCode;

    match state
        .handle
        .github
        .run_rerun(&body.repo, body.run_id, false)
        .await
    {
        Ok(_) => axum::Json(serde_json::json!({ "ok": true })).into_response(),
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            axum::Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

// -- Watch / unwatch / notifications REST endpoints --

#[derive(Deserialize)]
pub(crate) struct WatchRequest {
    pub repos: Vec<String>,
}

/// `POST /watch` — Start watching one or more repos.
pub(crate) async fn watch_handler(
    State(state): State<AppState>,
    axum::Json(body): axum::Json<WatchRequest>,
) -> axum::response::Response {
    for repo in &body.repos {
        if let Err(e) = validate_repo(repo) {
            return json_error(e);
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
pub(crate) async fn unwatch_handler(
    State(state): State<AppState>,
    axum::Json(body): axum::Json<WatchRequest>,
) -> axum::Json<serde_json::Value> {
    let results = do_stop_watches(&state.watches, &state.config, &state.handle, &body.repos).await;
    axum::Json(serde_json::json!({ "ok": true, "messages": results }))
}

#[derive(Deserialize)]
pub(crate) struct BranchesRequest {
    repo: String,
    branches: Vec<String>,
}

/// `POST /branches` — Set which branches to watch for a repo.
pub(crate) async fn branches_handler(
    State(state): State<AppState>,
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
pub(crate) struct NotificationsQuery {
    repo: String,
    branch: String,
}

/// `GET /notifications` — Resolved notification config for a specific repo/branch.
pub(crate) async fn get_notifications_handler(
    State(state): State<AppState>,
    Query(q): Query<NotificationsQuery>,
) -> axum::Json<NotificationConfig> {
    let cfg = state.config.lock().await;
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
    State(state): State<AppState>,
    axum::Json(body): axum::Json<NotificationsRequest>,
) -> axum::response::Response {
    use super::actions::do_notification_action;

    let (snapshot, msg) = {
        let mut cfg = state.config.lock().await;
        let msg = match do_notification_action(
            &mut cfg,
            &body.repo,
            body.branch.as_deref(),
            &body.action,
            body.build_started,
            body.build_success,
            body.build_failure,
        ) {
            Ok(m) => m,
            Err(e) => return json_error(e),
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
pub(crate) struct DefaultsResponse {
    default_branches: Vec<String>,
    ignored_workflows: Vec<String>,
    poll_aggression: String,
}

/// `GET /defaults` — Read global default config (branches, ignored workflows, poll aggression).
pub(crate) async fn get_defaults_handler(
    State(state): State<AppState>,
) -> axum::Json<DefaultsResponse> {
    let cfg = state.config.lock().await;
    axum::Json(DefaultsResponse {
        default_branches: cfg.default_branches.clone(),
        ignored_workflows: cfg.ignored_workflows.clone(),
        poll_aggression: cfg.poll_aggression.to_string(),
    })
}

#[derive(Deserialize)]
pub(crate) struct SetDefaultsRequest {
    #[serde(default)]
    default_branches: Option<Vec<String>>,
    #[serde(default)]
    ignored_workflows: Option<Vec<String>>,
    #[serde(default)]
    poll_aggression: Option<String>,
}

/// `POST /defaults` — Update global default config fields.
pub(crate) async fn set_defaults_handler(
    State(state): State<AppState>,
    axum::Json(body): axum::Json<SetDefaultsRequest>,
) -> axum::response::Response {
    let (snapshot, messages) = {
        let mut cfg = state.config.lock().await;
        let mut messages = Vec::new();
        if let Some(branches) = body.default_branches {
            for b in &branches {
                if let Err(e) = validate_branch(b) {
                    return json_error(e);
                }
            }
            if branches.is_empty() {
                return json_error("default_branches must not be empty");
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
        if let Some(level) = body.poll_aggression {
            let aggression = match level.to_lowercase().as_str() {
                "low" => PollAggression::Low,
                "high" => PollAggression::High,
                _ => PollAggression::Medium,
            };
            cfg.poll_aggression = aggression;
            messages.push(format!("poll aggression: {aggression}"));
        }
        (cfg.clone(), messages)
    };
    state.handle.config_changed.notify_waiters();
    if let Some(warning) = persist_config(&*state.handle.persistence, snapshot).await {
        return axum::Json(
            serde_json::json!({ "ok": true, "messages": messages, "warning": warning }),
        )
        .into_response();
    }
    axum::Json(serde_json::json!({ "ok": true, "messages": messages })).into_response()
}

/// `POST /shutdown` — Initiate graceful daemon shutdown.
pub(crate) async fn shutdown_handler(
    State(state): State<AppState>,
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
    State(state): State<AppState>,
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
    State(state): State<AppState>,
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
        created_at: String::new(),
        updated_at: String::new(),
        duration_secs: lb.duration_secs,
        age_secs,
    }
}

#[cfg(test)]
mod tests {
    use build_watcher::config::NotificationLevel;
    use build_watcher::events::{EventBus, RunSnapshot, WatchEvent};
    use build_watcher::rate_limiter::{FALLBACK_ACTIVE_SECS, FALLBACK_IDLE_SECS};
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
            Arc::new(Mutex::new(HashMap::new())),
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
        let app_state = super::super::AppState {
            watches,
            config: Arc::new(Mutex::new(build_watcher::config::Config::default())),
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

    fn notifications_test_router(
        config: Arc<Mutex<build_watcher::config::Config>>,
    ) -> axum::Router {
        let (watches, pause, _events) = empty_state();
        let handle = stub_handle();
        let app_state = super::super::AppState {
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
            completed_at: None,
            duration_secs: None,
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
        assert_eq!(repos[0], "alice/app");
        assert_eq!(repos[1], "alice/app");
        assert_eq!(repos[2], "zoo/bar");
        assert_eq!(json["watches"][0]["branch"], "develop");
        assert_eq!(json["watches"][1]["branch"], "main");
    }

    fn test_router_full(watches: Watches, pause: PauseState, _events: EventBus) -> axum::Router {
        let handle = stub_handle();
        let app_state = super::super::AppState {
            watches,
            config: Arc::new(Mutex::new(build_watcher::config::Config::default())),
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
        assert_eq!(json["active_poll_secs"], FALLBACK_ACTIVE_SECS);
        assert_eq!(json["idle_poll_secs"], FALLBACK_IDLE_SECS);
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
        assert_eq!(bn.notifications.build_success, None);
        assert_eq!(
            bn.notifications.build_failure,
            Some(NotificationLevel::Critical)
        );
    }
}
