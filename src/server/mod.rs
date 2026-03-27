mod actions;
pub(crate) mod mcp;
mod rest;
mod schema;

use std::collections::HashMap;
use std::sync::Arc;

use axum::response::IntoResponse as _;

#[derive(Debug, thiserror::Error)]
pub(crate) enum ServerError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Other(String),
}
use axum::routing::get;
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use tokio_util::sync::CancellationToken;

use build_watcher::config::{NotificationLevel, state_dir, unix_now};
use build_watcher::status::{ActiveRunView, LastBuildView, StatusResponse, WatchStatus};
use build_watcher::watcher::{
    PauseState, RateLimitState, SharedConfig, WatchEntry, WatchKey, WatcherHandle, Watches,
    collect_persisted,
};

pub use mcp::BuildWatcher;

pub const DEFAULT_PORT: u16 = 8417;

/// Shared state for the HTTP routes.
#[derive(Clone)]
pub(crate) struct AppState {
    pub watches: Watches,
    pub config: SharedConfig,
    pub handle: WatcherHandle,
    pub pause: PauseState,
    pub rate_limit: RateLimitState,
    pub started_at: std::time::Instant,
}

/// Build a snapshot of all current watches from already-locked state.
///
/// Pure function (no async, no locks) — callers acquire the locks and pass
/// the data in. Both the `GET /status` HTTP handler and the `list_watches`
/// MCP tool call this so the watch-enumeration logic lives in one place.
pub(crate) fn build_watch_snapshot(
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
                let age_secs = lb.completed_at.map(|t| unix_now().saturating_sub(t) as f64);
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

/// Return a `400`-style JSON error response: `{"error": "<msg>"}`.
pub(crate) fn json_error(msg: impl std::fmt::Display) -> axum::response::Response {
    axum::Json(serde_json::json!({ "error": msg.to_string() })).into_response()
}

/// Bind to the preferred port, trying up to 9 consecutive ports on conflict.
async fn bind_with_fallback(preferred: u16) -> Result<tokio::net::TcpListener, ServerError> {
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
        .route("/status", get(rest::status_handler))
        .route("/stats", get(rest::stats_handler))
        .route("/events", get(rest::events_handler))
        .route("/pause", axum::routing::post(rest::pause_handler))
        .route("/rerun", axum::routing::post(rest::rerun_handler))
        .route("/watch", axum::routing::post(rest::watch_handler))
        .route("/unwatch", axum::routing::post(rest::unwatch_handler))
        .route(
            "/notifications",
            axum::routing::get(rest::get_notifications_handler).post(rest::notifications_handler),
        )
        .route("/branches", axum::routing::post(rest::branches_handler))
        .route(
            "/defaults",
            axum::routing::get(rest::get_defaults_handler).post(rest::set_defaults_handler),
        )
        .route("/history", get(rest::history_handler))
        .route("/history/all", get(rest::history_all_handler))
        .route("/shutdown", axum::routing::post(rest::shutdown_handler))
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
) -> Result<(), ServerError> {
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
    std::fs::write(&port_file, bound_port.to_string()).map_err(|e| {
        ServerError::Other(format!(
            "Failed to write port file {}: {e}",
            port_file.display()
        ))
    })?;

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
        .await
        .map_err(ServerError::Io)?;

    handle.shutdown().await;
    let persisted = collect_persisted(&watches).await;
    if let Err(e) = handle.persistence.save_watches(&persisted).await {
        tracing::error!(error = %e, "Failed to save watches on shutdown");
    }
    let hist = handle.history.lock().await.clone();
    if let Err(e) = handle.persistence.save_history(&hist).await {
        tracing::error!(error = %e, "Failed to save history on shutdown");
    }
    let _ = std::fs::remove_file(&port_file);
    tracing::info!("State saved, goodbye.");

    Ok(())
}
