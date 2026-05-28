use std::convert::Infallible;
use std::time::Duration;

use axum::extract::State;
use axum::response::sse::{Event, KeepAlive, Sse};
use tokio_stream::StreamExt as _;
use tokio_stream::wrappers::BroadcastStream;

use build_watcher::config::unix_now;
use build_watcher::events::WatchEvent;
use build_watcher::rate_limiter::{PollInput, compute_interval};
use build_watcher::status::StatsResponse;
use build_watcher::watcher::{count_api_calls, is_paused};

use super::DaemonState;
use super::build_watch_snapshot;

mod auto_discover;
mod history;
mod notifications;
mod repo_config;
mod watches;

pub(crate) use auto_discover::{
    add_auto_discover_rule_handler, get_auto_discover_rules_handler,
    remove_auto_discover_rule_handler,
};
pub(crate) use history::{history_all_handler, history_handler};
pub(crate) use notifications::{
    get_defaults_handler, get_notifications_handler, notifications_handler, set_defaults_handler,
};
pub(crate) use repo_config::{get_repo_config_handler, set_repo_config_handler};
pub(crate) use watches::{
    branches_handler, merge_handler, pause_handler, pin_handler, rerun_handler, shutdown_handler,
    unwatch_handler, watch_handler,
};

/// `GET /status` — JSON snapshot of all current watches and their build state.
pub(crate) async fn status_handler(
    State(state): State<DaemonState>,
) -> axum::Json<build_watcher::status::StatusResponse> {
    let paused = is_paused(&state.pause).await;
    let watches = state.watches.lock().await;
    let cfg = state.config.read().await;
    let history = state.handle.history.lock().await;
    axum::Json(build_watch_snapshot(
        &watches,
        Some(&cfg),
        Some(&history),
        paused,
    ))
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

/// `GET /version` — Daemon version and API version for client compatibility checks.
pub(crate) async fn version_handler() -> axum::Json<serde_json::Value> {
    axum::Json(serde_json::json!({
        "version": env!("CARGO_PKG_VERSION"),
        "api_version": 1
    }))
}

/// `GET /stats` — Daemon stats: uptime, polling intervals, rate limit.
pub(crate) async fn stats_handler(State(state): State<DaemonState>) -> axum::Json<StatsResponse> {
    let uptime_secs = state.started_at.elapsed().as_secs();
    let api_calls = count_api_calls(&*state.watches.lock().await);
    let rl = state.rate_limit.lock().await;
    let aggression = state.config.read().await.poll_aggression;
    let poll_secs = compute_interval(&PollInput {
        rate_limit: rl.clone(),
        calls_per_cycle: api_calls,
        now: unix_now(),
        aggression,
    });

    let (rate_remaining, rate_limit, rate_reset_mins) = match rl.as_ref() {
        Some(r) => {
            let reset_mins = r.reset.saturating_sub(unix_now()) / 60;
            (Some(r.remaining), Some(r.limit), Some(reset_mins))
        }
        None => (None, None, None),
    };

    axum::Json(StatsResponse {
        uptime_secs,
        poll_secs,
        poll_aggression: aggression,
        rate_remaining,
        rate_limit,
        rate_reset_mins,
        dropped_events: state.handle.events.dropped_count(),
    })
}

#[cfg(test)]
mod tests {
    use build_watcher::config::{
        ConfigManager, ConfigPersistence, NotificationLevel, SharedConfigManager,
    };
    use build_watcher::events::{EventBus, WatchEvent};
    use build_watcher::rate_limiter::MIN_POLL_SECS;
    use build_watcher::watcher::{PauseState, WatchEntry, WatchKey, Watches};
    use std::collections::HashMap;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    use crate::testutil::snap;

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
        async fn list_accessible_repos(
            &self,
        ) -> Result<Vec<build_watcher::github::RepoInfo>, build_watcher::github::GhError> {
            Ok(vec![])
        }
    }

    fn stub_handle() -> build_watcher::watcher::WatcherHandle {
        build_watcher::watcher::WatcherHandle::new(
            tokio_util::sync::CancellationToken::new(),
            EventBus::new(),
            Arc::new(StubGitHub),
            Arc::new(build_watcher::persistence::NullPersistence),
            Arc::new(Mutex::new(HashMap::new())),
            Arc::new(Mutex::new(HashMap::new())),
            Arc::new(Mutex::new(std::collections::HashSet::new())),
            Arc::new(tokio::sync::Notify::new()),
        )
    }

    fn test_router(watches: Watches, pause: PauseState) -> axum::Router {
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

    #[tokio::test]
    async fn status_empty_watches() {
        let (watches, pause, _) = empty_state();
        let json = get_status_json(test_router(watches, pause)).await;
        assert_eq!(json["paused"], false);
        assert_eq!(json["watches"], serde_json::json!([]));
    }

    #[tokio::test]
    async fn status_paused_flag() {
        let (watches, pause, _) = empty_state();
        *pause.lock().await =
            Some(tokio::time::Instant::now() + std::time::Duration::from_secs(300));
        let json = get_status_json(test_router(watches, pause)).await;
        assert_eq!(json["paused"], true);
    }

    #[tokio::test]
    async fn status_with_last_build() {
        use build_watcher::github::LastBuild;

        let (watches, pause, _) = empty_state();
        let key = WatchKey::new("alice/app", "main");
        let mut entry = WatchEntry::default();
        entry.last_builds.insert(
            "CI".to_string(),
            LastBuild {
                run_id: 99,
                conclusion: build_watcher::github::RunConclusion::Failure,
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
                flaky: false,
            },
        );
        watches.lock().await.insert(key, entry);

        let json = get_status_json(test_router(watches, pause)).await;
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
        let (watches, pause, _) = empty_state();
        {
            let mut w = watches.lock().await;
            w.insert(WatchKey::new("zoo/bar", "main"), WatchEntry::default());
            w.insert(WatchKey::new("alice/app", "main"), WatchEntry::default());
            w.insert(WatchKey::new("alice/app", "develop"), WatchEntry::default());
        }
        let json = get_status_json(test_router(watches, pause)).await;
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

    fn test_router_full(watches: Watches, pause: PauseState) -> axum::Router {
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

        let (watches, pause, _) = empty_state();
        let router = test_router_full(watches, pause);
        let req = http::Request::get("/stats")
            .body(axum::body::Body::empty())
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 200);
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(json["uptime_secs"].as_u64().unwrap() < 5);
        // Default aggression is Medium: 40% of 5000 = 2000 budget.
        // 1 call/cycle, 3600s → 3600/2000 < 1 → floor interval.
        assert_eq!(json["poll_secs"], MIN_POLL_SECS);
        assert!(json["rate_remaining"].is_null());
    }

    #[tokio::test]
    async fn pause_toggle() {
        use http_body_util::BodyExt;
        use tower::ServiceExt;

        let (watches, pause, _) = empty_state();

        let router = test_router_full(watches.clone(), pause.clone());
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

        let router = test_router_full(watches.clone(), pause.clone());
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

        let (watches, pause, _) = empty_state();
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

        let json = get_status_json(test_router(watches, pause)).await;
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

        let (watches, pause, _) = empty_state();
        let router = test_router(watches, pause);
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
        json_post_with_status(router, path, body).await.1
    }

    async fn json_post_with_status(
        router: &axum::Router,
        path: &str,
        body: &impl serde::Serialize,
    ) -> (u16, serde_json::Value) {
        use http_body_util::BodyExt;
        use tower::ServiceExt;
        let req = http::Request::post(path)
            .header("content-type", "application/json")
            .body(axum::body::Body::from(serde_json::to_vec(body).unwrap()))
            .unwrap();
        let resp = router.clone().oneshot(req).await.unwrap();
        let status = resp.status().as_u16();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let json = serde_json::from_slice(&bytes).unwrap();
        (status, json)
    }

    #[tokio::test]
    async fn get_repo_config_returns_defaults_for_unknown_repo() {
        let router = repo_config_router(null_config(build_watcher::config::Config::default()));
        let json = json_get(&router, "/repo-config?repo=alice/app").await;
        assert_eq!(json["repo"], "alice/app");
        assert_eq!(json["watch_prs"], true);
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
            poll_aggression: Some(build_watcher::config::PollAggression::High),
            clear_poll_aggression: None,
            auto_discover_branches: None,
            branch_filter: None,
            ignored_events: None,
            branches: None,
            notifications: None,
            auto_discovered_by_rule: None,
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

    #[tokio::test]
    async fn set_repo_config_rejects_invalid_branch_filter_regex() {
        let config = null_config(build_watcher::config::Config::default());
        let router = repo_config_router(config.clone());

        let body = build_watcher::status::RepoConfigView {
            repo: "alice/app".to_string(),
            alias: None,
            workflows: None,
            watch_prs: None,
            poll_aggression: None,
            clear_poll_aggression: None,
            auto_discover_branches: None,
            branch_filter: Some("[invalid regex".to_string()),
            ignored_events: None,
            branches: None,
            notifications: None,
            auto_discovered_by_rule: None,
        };
        let resp = json_post(&router, "/repo-config", &body).await;
        assert!(
            resp.get("error").is_some(),
            "expected error field, got: {resp}"
        );
        assert!(
            resp["error"]
                .as_str()
                .unwrap()
                .contains("invalid branch filter regex"),
            "expected regex error message, got: {}",
            resp["error"]
        );

        // Config should not have been modified.
        let cfg = config.read().await;
        assert!(!cfg.repos.contains_key("alice/app"));
    }

    // -- Branch-edit guards on auto-managed repos --

    fn branches_router_with_state(
        config: SharedConfigManager,
        discovered_repos: build_watcher::persistence::DiscoveredRepoSet,
    ) -> (axum::Router, build_watcher::watcher::DiscoveredRepos) {
        let (watches, pause, _events) = empty_state();
        let discovered_repos_handle: build_watcher::watcher::DiscoveredRepos =
            Arc::new(Mutex::new(discovered_repos));
        let handle = build_watcher::watcher::WatcherHandle::new(
            tokio_util::sync::CancellationToken::new(),
            EventBus::new(),
            Arc::new(StubGitHub),
            Arc::new(build_watcher::persistence::NullPersistence),
            Arc::new(Mutex::new(HashMap::new())),
            Arc::new(Mutex::new(HashMap::new())),
            discovered_repos_handle.clone(),
            Arc::new(tokio::sync::Notify::new()),
        );
        let app_state = super::super::DaemonState {
            watches,
            config,
            handle,
            pause,
            rate_limit: Arc::new(Mutex::new(None)),
            started_at: std::time::Instant::now(),
        };
        let router = axum::Router::new()
            .route("/branches", axum::routing::post(super::branches_handler))
            .with_state(app_state);
        (router, discovered_repos_handle)
    }

    #[tokio::test]
    async fn branches_rejected_when_repo_is_rule_discovered() {
        // Branches list is owned by the discovery rule; manual edits must be
        // rejected even when the request bypasses the TUI's pre-check.
        let cfg = build_watcher::config::Config {
            auto_discover_branches: false, // isolate to the rule path
            ..Default::default()
        };
        let mut discovered = std::collections::HashSet::new();
        discovered.insert("alice/app".to_string());
        let (router, _) = branches_router_with_state(null_config(cfg), discovered);

        let body = serde_json::json!({
            "repo": "alice/app",
            "branches": ["main"],
        });
        let resp = json_post(&router, "/branches", &body).await;
        let messages = resp["messages"].as_array().expect("messages array");
        assert!(
            messages.iter().any(|m| m
                .as_str()
                .unwrap_or("")
                .contains("auto-discovered by a rule")),
            "expected rule-discovery rejection, got: {resp}"
        );
    }

    #[tokio::test]
    async fn branches_rejected_when_branch_auto_discover_is_on() {
        let cfg = build_watcher::config::Config {
            auto_discover_branches: true,
            ..Default::default()
        };
        let (router, _) =
            branches_router_with_state(null_config(cfg), std::collections::HashSet::new());

        let body = serde_json::json!({
            "repo": "alice/app",
            "branches": ["main"],
        });
        let resp = json_post(&router, "/branches", &body).await;
        let messages = resp["messages"].as_array().expect("messages array");
        assert!(
            messages.iter().any(|m| m
                .as_str()
                .unwrap_or("")
                .contains("branch auto-discovery is enabled")),
            "expected branch-auto-discovery rejection, got: {resp}"
        );
    }

    // -- Defaults round-trip --

    fn defaults_router(config: SharedConfigManager) -> axum::Router {
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
                "/defaults",
                axum::routing::get(super::get_defaults_handler).post(super::set_defaults_handler),
            )
            .with_state(app_state)
    }

    #[tokio::test]
    async fn get_defaults_includes_new_toggles() {
        use http_body_util::BodyExt;
        use tower::ServiceExt;

        let config = null_config(build_watcher::config::Config::default());
        let router = defaults_router(config);
        let req = http::Request::get("/defaults")
            .body(axum::body::Body::empty())
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 200);
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["detect_flakes"], true);
        assert_eq!(json["notify_mode"], "every_build");
    }

    #[tokio::test]
    async fn set_defaults_updates_new_toggles() {
        use http_body_util::BodyExt;
        use tower::ServiceExt;

        let config = null_config(build_watcher::config::Config::default());
        let router = defaults_router(config.clone());
        let body = r#"{
            "detect_flakes": false,
            "notify_mode": "failure_only"
        }"#;
        let req = http::Request::post("/defaults")
            .header("content-type", "application/json")
            .body(axum::body::Body::from(body))
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 200);
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["ok"], true);

        // Confirm the config was actually mutated.
        let cfg = config.read().await;
        assert!(!cfg.detect_flakes);
        assert_eq!(
            cfg.notify_mode,
            build_watcher::config::NotifyMode::FailuresAndRecoveries
        );
    }

    #[tokio::test]
    async fn set_defaults_rejects_unknown_notify_mode() {
        use http_body_util::BodyExt;
        use tower::ServiceExt;

        let config = null_config(build_watcher::config::Config::default());
        let router = defaults_router(config.clone());
        let body = r#"{"notify_mode": "nonsense"}"#;
        let req = http::Request::post("/defaults")
            .header("content-type", "application/json")
            .body(axum::body::Body::from(body))
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        // `json_error` returns the error in the body (project convention), not
        // via HTTP status. Match that convention here.
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let err = json["error"].as_str().unwrap_or_default();
        assert!(
            err.contains("notify_mode"),
            "error should mention field: {err}"
        );
        assert!(
            err.contains("nonsense"),
            "error should echo offending value: {err}"
        );
        assert!(
            json.get("ok").is_none(),
            "rejected requests should not say ok: {json}"
        );

        // Confirm config was NOT mutated.
        let cfg = config.read().await;
        assert_eq!(
            cfg.notify_mode,
            build_watcher::config::NotifyMode::EveryBuild,
            "default should remain unchanged"
        );
    }

    #[tokio::test]
    async fn set_defaults_accepts_failure_only_alias() {
        // The legacy "failure_only" name remains accepted as an alias for
        // failures_and_recoveries — documented at NotifyMode::Deserialize.
        use tower::ServiceExt;
        let config = null_config(build_watcher::config::Config::default());
        let router = defaults_router(config.clone());
        let body = r#"{"notify_mode": "failure_only"}"#;
        let req = http::Request::post("/defaults")
            .header("content-type", "application/json")
            .body(axum::body::Body::from(body))
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 200);
        let cfg = config.read().await;
        assert_eq!(
            cfg.notify_mode,
            build_watcher::config::NotifyMode::FailuresAndRecoveries
        );
    }

    // -- /pin endpoint --

    fn pin_router(config: SharedConfigManager) -> axum::Router {
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
            .route("/pin", axum::routing::post(super::pin_handler))
            .with_state(app_state)
    }

    #[tokio::test]
    async fn pin_repo_sets_repo_pinned_flag() {
        use tower::ServiceExt;
        let config = null_config(build_watcher::config::Config::default());
        let router = pin_router(config.clone());
        let req = http::Request::post("/pin")
            .header("content-type", "application/json")
            .body(axum::body::Body::from(
                r#"{"repo":"alice/app","pinned":true}"#,
            ))
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 200);

        let cfg = config.read().await;
        let rc = cfg.repos.get("alice/app").expect("repo entry created");
        assert!(rc.pinned, "repo should be pinned");
    }

    #[tokio::test]
    async fn pin_branch_sets_branch_pinned_flag() {
        use tower::ServiceExt;
        let config = null_config(build_watcher::config::Config::default());
        let router = pin_router(config.clone());
        let req = http::Request::post("/pin")
            .header("content-type", "application/json")
            .body(axum::body::Body::from(
                r#"{"repo":"alice/app","branch":"main","pinned":true}"#,
            ))
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 200);

        let cfg = config.read().await;
        let rc = cfg.repos.get("alice/app").expect("repo entry created");
        assert!(!rc.pinned, "repo itself should not be pinned");
        let bc = rc
            .branch_notifications
            .get("main")
            .expect("branch entry created");
        assert!(bc.pinned, "branch should be pinned");
    }

    #[tokio::test]
    async fn pin_rejects_invalid_repo() {
        use http_body_util::BodyExt;
        use tower::ServiceExt;
        let config = null_config(build_watcher::config::Config::default());
        let router = pin_router(config);
        let req = http::Request::post("/pin")
            .header("content-type", "application/json")
            .body(axum::body::Body::from(
                r#"{"repo":"not-a-valid-repo","pinned":true}"#,
            ))
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(json["error"].is_string(), "should error on invalid repo");
    }

    #[tokio::test]
    async fn pin_unpin_clears_flag() {
        use tower::ServiceExt;
        let mut cfg = build_watcher::config::Config::default();
        let rc = build_watcher::config::RepoConfig {
            pinned: true,
            ..Default::default()
        };
        cfg.repos.insert("alice/app".to_string(), rc);
        let config = null_config(cfg);

        let router = pin_router(config.clone());
        let req = http::Request::post("/pin")
            .header("content-type", "application/json")
            .body(axum::body::Body::from(
                r#"{"repo":"alice/app","pinned":false}"#,
            ))
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 200);

        let cfg = config.read().await;
        assert!(!cfg.repos.get("alice/app").unwrap().pinned);
    }

    #[tokio::test]
    async fn pin_repo_clears_individual_branch_pins() {
        use build_watcher::config::{BranchConfig, NotificationLevel, NotificationOverrides};
        use tower::ServiceExt;
        // One branch pinned with nothing else; another pinned but also carrying
        // a notification override.
        let mut cfg = build_watcher::config::Config::default();
        let mut rc = build_watcher::config::RepoConfig::default();
        rc.branch_notifications.insert(
            "main".to_string(),
            BranchConfig {
                pinned: true,
                ..Default::default()
            },
        );
        rc.branch_notifications.insert(
            "dev".to_string(),
            BranchConfig {
                pinned: true,
                notifications: NotificationOverrides {
                    build_failure: Some(NotificationLevel::Off),
                    ..Default::default()
                },
            },
        );
        cfg.repos.insert("alice/app".to_string(), rc);
        let config = null_config(cfg);

        // Pinning the whole repo should drop the redundant branch pins so they
        // can't resurface when the repo is later unpinned.
        let router = pin_router(config.clone());
        let req = http::Request::post("/pin")
            .header("content-type", "application/json")
            .body(axum::body::Body::from(
                r#"{"repo":"alice/app","pinned":true}"#,
            ))
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 200);

        let cfg = config.read().await;
        let rc = cfg.repos.get("alice/app").unwrap();
        assert!(rc.pinned, "repo should be pinned");
        // The pin-only entry is dropped entirely; the one with a notification
        // override is kept but unpinned.
        assert!(
            !rc.branch_notifications.contains_key("main"),
            "empty branch entry should be removed"
        );
        let dev = rc
            .branch_notifications
            .get("dev")
            .expect("branch with notification override is preserved");
        assert!(!dev.pinned, "branch pin should be cleared");
        assert_eq!(
            dev.notifications.build_failure,
            Some(NotificationLevel::Off)
        );
    }

    #[tokio::test]
    async fn unpin_branch_drops_empty_entry() {
        use build_watcher::config::BranchConfig;
        use tower::ServiceExt;
        let mut cfg = build_watcher::config::Config::default();
        let mut rc = build_watcher::config::RepoConfig::default();
        rc.branch_notifications.insert(
            "main".to_string(),
            BranchConfig {
                pinned: true,
                ..Default::default()
            },
        );
        cfg.repos.insert("alice/app".to_string(), rc);
        let config = null_config(cfg);

        let router = pin_router(config.clone());
        let req = http::Request::post("/pin")
            .header("content-type", "application/json")
            .body(axum::body::Body::from(
                r#"{"repo":"alice/app","branch":"main","pinned":false}"#,
            ))
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 200);

        let cfg = config.read().await;
        let rc = cfg.repos.get("alice/app").unwrap();
        assert!(
            !rc.branch_notifications.contains_key("main"),
            "unpinning a branch with no other overrides should drop its entry"
        );
    }

    #[tokio::test]
    async fn set_defaults_omitted_fields_leave_existing_values() {
        let config = null_config(build_watcher::config::Config {
            detect_flakes: false,
            notify_mode: build_watcher::config::NotifyMode::FailuresAndRecoveries,
            ..build_watcher::config::Config::default()
        });
        let router = defaults_router(config.clone());

        // Empty body: nothing changes.
        let req = http::Request::post("/defaults")
            .header("content-type", "application/json")
            .body(axum::body::Body::from("{}"))
            .unwrap();
        use tower::ServiceExt;
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 200);

        let cfg = config.read().await;
        assert!(!cfg.detect_flakes);
        assert_eq!(
            cfg.notify_mode,
            build_watcher::config::NotifyMode::FailuresAndRecoveries
        );
    }
}
