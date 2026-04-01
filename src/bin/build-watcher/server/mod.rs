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

use build_watcher::config::SharedConfigManager;
use build_watcher::config::unix_now;
use build_watcher::dirs::state_dir;
use build_watcher::status::{
    ActiveRunView, LastBuildView, PrView, RunConclusion, StatusResponse, WatchStatus,
};
use build_watcher::watcher::{
    PauseState, RateLimitState, WatchEntry, WatchKey, WatcherHandle, Watches, collect_persisted,
};

pub use mcp::BuildWatcher;

pub const DEFAULT_PORT: u16 = 8417;

/// Shared state for the HTTP routes.
#[derive(Clone)]
pub(crate) struct DaemonState {
    pub watches: Watches,
    pub config: SharedConfigManager,
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
) -> StatusResponse {
    let now_unix = unix_now();
    let mut watch_list: Vec<WatchStatus> = watches
        .iter()
        .map(|(key, entry)| {
            let mut active_runs: Vec<ActiveRunView> = entry
                .active_runs
                .iter()
                .filter(|(_, run)| {
                    config.is_none_or(|cfg| {
                        !cfg.ignored_workflows
                            .iter()
                            .any(|i| run.workflow.eq_ignore_ascii_case(i))
                    })
                })
                .map(|(run_id, run)| {
                    let elapsed_secs =
                        build_watcher::github::elapsed_since(&run.created_at, now_unix);
                    ActiveRunView {
                        run_id: *run_id,
                        status: run.status.clone(),
                        workflow: run.workflow.clone(),
                        title: run.display_title(),
                        event: run.event.clone(),
                        elapsed_secs,
                        attempt: run.attempt,
                        url: run.url.clone(),
                    }
                })
                .collect();
            active_runs.sort_by_key(|r| r.run_id);

            let mut last_builds: Vec<LastBuildView> = entry
                .last_builds
                .values()
                .filter(|lb| {
                    config.is_none_or(|cfg| {
                        !cfg.ignored_workflows
                            .iter()
                            .any(|i| lb.workflow.eq_ignore_ascii_case(i))
                    })
                })
                .map(|lb| {
                    let age_secs = lb.completed_at.map(|t| now_unix.saturating_sub(t) as f64);
                    let conclusion =
                        serde_json::from_value(serde_json::Value::String(lb.conclusion.clone()))
                            .unwrap_or(RunConclusion::Unknown);
                    LastBuildView {
                        run_id: lb.run_id,
                        conclusion,
                        workflow: lb.workflow.clone(),
                        title: lb.display_title(),
                        failing_steps: lb.failing_steps.clone(),
                        age_secs,
                        attempt: lb.attempt,
                        failing_job_id: lb.failing_job_id,
                        url: lb.url.clone(),
                        duration_secs: lb.duration_secs,
                    }
                })
                .collect();
            last_builds.sort_by(|a, b| a.workflow.cmp(&b.workflow));

            let muted = config
                .is_some_and(|cfg| cfg.notifications_for(&key.repo, &key.branch).is_all_off());

            let pr = entry.pr.as_ref().map(|pr| PrView {
                number: pr.number,
                title: pr.title.clone(),
                url: pr.url.clone(),
                author: pr.author.clone(),
                merge_state: pr.merge_state.clone(),
                draft: pr.draft,
            });

            WatchStatus {
                repo: key.repo.clone(),
                branch: key.branch.clone(),
                active_runs,
                last_builds,
                pr,
                muted,
                waiting: entry.waiting,
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

/// Acquire an exclusive lock file to prevent multiple daemon instances.
///
/// The kernel releases the lock automatically when the process exits (even on
/// SIGKILL), so there are no stale-lock issues. The returned `File` handle must
/// be kept alive for the lifetime of the server.
pub fn acquire_instance_lock() -> Result<std::fs::File, ServerError> {
    use std::io::Write;
    use std::os::unix::io::AsRawFd;

    let lock_path = state_dir().join("daemon.lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&lock_path)
        .map_err(|e| {
            ServerError::Other(format!(
                "Failed to open lock file {}: {e}",
                lock_path.display()
            ))
        })?;

    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 {
        let os_err = std::io::Error::last_os_error();
        return Err(ServerError::Other(format!(
            "Another build-watcher instance is already running ({os_err}). \
             Stop it first, or set BUILD_WATCHER_PORT to run a separate instance."
        )));
    }

    // Write our PID for observability (not used for locking).
    let _ = (&file).write_all(std::process::id().to_string().as_bytes());

    Ok(file)
}

/// Build the axum router with the MCP `StreamableHttpService` and SSE/status routes.
fn build_router(state: DaemonState, ct: &CancellationToken) -> axum::Router {
    let http_config = StreamableHttpServerConfig {
        stateful_mode: false,
        json_response: true,
        sse_keep_alive: None,
        cancellation_token: ct.child_token(),
        ..Default::default()
    };

    let mcp_state = state.clone();
    let service: StreamableHttpService<BuildWatcher, LocalSessionManager> =
        StreamableHttpService::new(
            move || Ok(BuildWatcher::new(mcp_state.clone())),
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
        .route(
            "/repo-config",
            axum::routing::get(rest::get_repo_config_handler).post(rest::set_repo_config_handler),
        )
        .route("/history", get(rest::history_handler))
        .route("/history/all", get(rest::history_all_handler))
        .route("/shutdown", axum::routing::post(rest::shutdown_handler))
        .with_state(state)
        .nest_service("/mcp", service)
}

/// Run the MCP HTTP server with graceful shutdown.
///
/// Binds to the configured port, writes a port-discovery file, serves until
/// ctrl-c, then shuts down pollers and persists state.
pub async fn serve(
    state: DaemonState,
    ct: CancellationToken,
    _lock: std::fs::File,
) -> Result<(), ServerError> {
    let port: u16 = std::env::var("BUILD_WATCHER_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(DEFAULT_PORT);

    let router = build_router(state.clone(), &ct);
    let listener = tokio::net::TcpListener::bind(format!("127.0.0.1:{port}"))
        .await
        .map_err(ServerError::Io)?;

    let port_file = state_dir().join("port");
    std::fs::write(&port_file, port.to_string()).map_err(|e| {
        ServerError::Other(format!(
            "Failed to write port file {}: {e}",
            port_file.display()
        ))
    })?;

    tracing::info!("build-watcher listening on http://127.0.0.1:{port}/mcp");

    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("failed to register SIGTERM handler");

    axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {
                    tracing::info!("Ctrl-C received, shutting down...");
                }
                _ = sigterm.recv() => {
                    tracing::info!("SIGTERM received, shutting down...");
                }
                _ = ct.cancelled() => {
                    tracing::info!("Shutdown requested, shutting down...");
                }
            }
            ct.cancel();
        })
        .await
        .map_err(ServerError::Io)?;

    state.handle.shutdown().await;
    let persisted = collect_persisted(&state.watches).await;
    let hist = state.handle.history.lock().await.clone();
    state.handle.persistence.save_state(&persisted, &hist).await;
    let _ = std::fs::remove_file(&port_file);
    tracing::info!("State saved, goodbye.");

    Ok(())
}
