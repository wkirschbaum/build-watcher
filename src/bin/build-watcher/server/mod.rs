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
    /// Another daemon already holds the instance lock. This is benign — it's
    /// not a crash — so `main` treats it as a clean (exit 0) no-op rather than
    /// a failure, which keeps systemd/launchd from restart-looping when a
    /// second instance is launched alongside the running one.
    #[error(
        "Another build-watcher instance is already running ({0}). \
         Stop it first, or use --config-dir to run a separate instance."
    )]
    InstanceAlreadyRunning(std::io::Error),
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
use build_watcher::status::{ActiveRunView, LastBuildView, PrView, StatusResponse, WatchStatus};
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

/// Compute (avg, recent_builds) for a workflow over the 7-day window.
/// Returns `(None, vec![])` when no history is available.
///
/// `avg` is success-only (typical-runtime stat — failures often abort partway
/// and would skew it). `recent_builds` includes all conclusions so the TUI
/// sparkline can colour each bar by outcome. The trend is always computed —
/// the TUI consumes it from the detail bar only, so there's no row-clutter
/// cost that would justify gating it behind a toggle.
fn trend_for(
    history: Option<&build_watcher::history::BuildHistory>,
    key: &WatchKey,
    workflow: &str,
    now_unix: u64,
) -> (Option<u64>, Vec<build_watcher::status::BuildSample>) {
    let Some(h) = history else {
        return (None, Vec::new());
    };
    let avg = build_watcher::history::avg_duration(h, key, workflow, now_unix);
    let recent = build_watcher::history::recent_completed_builds(h, key, workflow, now_unix);
    (avg, recent)
}

/// Build a snapshot of all current watches from already-locked state.
///
/// Pure function (no async, no locks) — callers acquire the locks and pass
/// the data in. Both the `GET /status` HTTP handler and the `list_watches`
/// MCP tool call this so the watch-enumeration logic lives in one place.
pub(crate) fn build_watch_snapshot(
    watches: &HashMap<WatchKey, WatchEntry>,
    config: Option<&build_watcher::config::Config>,
    history: Option<&build_watcher::history::BuildHistory>,
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
                    !config.is_some_and(|cfg| {
                        cfg.ignored_workflows
                            .iter()
                            .any(|i| run.workflow.eq_ignore_ascii_case(i))
                            || cfg
                                .ignored_events_for(&key.repo)
                                .iter()
                                .any(|i| run.event.eq_ignore_ascii_case(i))
                    })
                })
                .map(|(run_id, run)| {
                    let elapsed_secs =
                        build_watcher::github::elapsed_since(&run.created_at, now_unix);
                    let (avg_duration_secs, recent_builds) =
                        trend_for(history, key, &run.workflow, now_unix);
                    ActiveRunView {
                        run_id: *run_id,
                        status: run.status.clone(),
                        workflow: run.workflow.clone(),
                        title: run.display_title(),
                        event: run.event.clone(),
                        elapsed_secs,
                        attempt: run.attempt,
                        url: run.url.clone(),
                        actor: run.actor.clone(),
                        commit_author: run.commit_author.clone(),
                        avg_duration_secs,
                        recent_builds,
                    }
                })
                .collect();
            active_runs.sort_by_key(|r| r.run_id);

            let mut last_builds: Vec<LastBuildView> = entry
                .last_builds
                .values()
                .filter(|lb| {
                    !config.is_some_and(|cfg| {
                        cfg.ignored_workflows
                            .iter()
                            .any(|i| lb.workflow.eq_ignore_ascii_case(i))
                            || cfg
                                .ignored_events_for(&key.repo)
                                .iter()
                                .any(|i| lb.event.eq_ignore_ascii_case(i))
                    })
                })
                .map(|lb| {
                    let age_secs = lb.completed_at.map(|t| now_unix.saturating_sub(t) as f64);
                    let (avg_duration_secs, recent_builds) =
                        trend_for(history, key, &lb.workflow, now_unix);
                    LastBuildView {
                        run_id: lb.run_id,
                        conclusion: lb.conclusion.clone(),
                        workflow: lb.workflow.clone(),
                        title: lb.display_title(),
                        failing_steps: lb.failing_steps.clone(),
                        age_secs,
                        attempt: lb.attempt,
                        failing_job_id: lb.failing_job_id,
                        url: lb.url.clone(),
                        duration_secs: lb.duration_secs,
                        actor: lb.actor.clone(),
                        commit_author: lb.commit_author.clone(),
                        flaky: lb.flaky,
                        avg_duration_secs,
                        recent_builds,
                    }
                })
                .collect();
            last_builds.sort_by(|a, b| a.workflow.cmp(&b.workflow));

            let muted = config
                .is_some_and(|cfg| cfg.notifications_for(&key.repo, &key.branch).is_all_off());

            // Pinned if the repo is pinned OR this specific branch is pinned
            // in the per-repo branch_notifications map. Repo pin cascades.
            let repo_pinned =
                config.is_some_and(|cfg| cfg.repos.get(&key.repo).is_some_and(|rc| rc.pinned));
            let pinned = repo_pinned
                || config.is_some_and(|cfg| {
                    cfg.repos.get(&key.repo).is_some_and(|rc| {
                        rc.branch_notifications
                            .get(&key.branch)
                            .is_some_and(|bc| bc.pinned)
                    })
                });

            let prs = entry
                .prs
                .iter()
                .map(|pr| PrView {
                    number: pr.number,
                    title: pr.title.clone(),
                    url: pr.url.clone(),
                    author: pr.author.clone(),
                    merge_state: pr.merge_state.clone(),
                    draft: pr.draft,
                })
                .collect();

            WatchStatus {
                repo: key.repo.clone(),
                branch: key.branch.clone(),
                active_runs,
                last_builds,
                prs,
                muted,
                waiting: entry.waiting,
                pinned,
                repo_pinned,
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
        return Err(ServerError::InstanceAlreadyRunning(
            std::io::Error::last_os_error(),
        ));
    }

    // Write our PID for observability (not used for locking).
    let _ = (&file).write_all(std::process::id().to_string().as_bytes());

    Ok(file)
}

/// Build the axum router with the MCP `StreamableHttpService` and SSE/status routes.
fn build_router(state: DaemonState, ct: &CancellationToken) -> axum::Router {
    let http_config = StreamableHttpServerConfig::default()
        .with_stateful_mode(false)
        .with_json_response(true)
        .with_sse_keep_alive(None)
        .with_cancellation_token(ct.child_token());

    let mcp_state = state.clone();
    let service: StreamableHttpService<BuildWatcher, LocalSessionManager> =
        StreamableHttpService::new(
            move || Ok(BuildWatcher::new(mcp_state.clone())),
            Arc::default(),
            http_config,
        );

    axum::Router::new()
        .route("/version", get(rest::version_handler))
        .route("/status", get(rest::status_handler))
        .route("/stats", get(rest::stats_handler))
        .route("/events", get(rest::events_handler))
        .route("/pause", axum::routing::post(rest::pause_handler))
        .route("/pin", axum::routing::post(rest::pin_handler))
        .route("/rerun", axum::routing::post(rest::rerun_handler))
        .route("/merge", axum::routing::post(rest::merge_handler))
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
        .route(
            "/auto-discover-rules",
            axum::routing::get(rest::get_auto_discover_rules_handler)
                .post(rest::add_auto_discover_rule_handler),
        )
        .route(
            "/auto-discover-rules/remove",
            axum::routing::post(rest::remove_auto_discover_rule_handler),
        )
        .route("/shutdown", axum::routing::post(rest::shutdown_handler))
        .with_state(state)
        .nest_service("/mcp", service)
}

/// Run the MCP HTTP server with graceful shutdown.
///
/// Binds to the configured port, writes a port-discovery file, serves until
/// ctrl-c, then shuts down pollers and persists state.
///
/// Pass `port = 0` to let the OS pick a free port (used for `--config-dir` instances).
pub async fn serve(
    state: DaemonState,
    ct: CancellationToken,
    _lock: std::fs::File,
    port: u16,
) -> Result<(), ServerError> {
    let router = build_router(state.clone(), &ct);
    let listener = tokio::net::TcpListener::bind(format!("127.0.0.1:{port}"))
        .await
        .map_err(ServerError::Io)?;

    // Bind a Unix domain socket alongside TCP for faster local client connections.
    #[cfg(unix)]
    let (unix_sock_path, unix_task) = {
        let socket_path = state_dir().join("daemon.sock");
        let _ = std::fs::remove_file(&socket_path);
        match tokio::net::UnixListener::bind(&socket_path) {
            Ok(unix_listener) => {
                let router_unix = router.clone();
                let ct_unix = ct.clone();
                let task = tokio::spawn(async move {
                    axum::serve(unix_listener, router_unix)
                        .with_graceful_shutdown(ct_unix.cancelled_owned())
                        .await
                        .ok();
                });
                tracing::info!("build-watcher also listening on {}", socket_path.display());
                (Some(socket_path), Some(task))
            }
            Err(e) => {
                tracing::warn!("Unix socket unavailable: {e}");
                (None, None)
            }
        }
    };

    // After binding, resolve the real port (important when port=0, where the OS picks).
    let bound_port = listener.local_addr().map(|a| a.port()).unwrap_or(port);

    let port_file = state_dir().join("port");
    std::fs::write(&port_file, bound_port.to_string()).map_err(|e| {
        ServerError::Other(format!(
            "Failed to write port file {}: {e}",
            port_file.display()
        ))
    })?;

    tracing::info!("build-watcher listening on http://127.0.0.1:{bound_port}/mcp");

    let sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate());

    // Clone for the shutdown watchdog below, which may need to persist state
    // from a spawned task while the main path is still blocked draining.
    let watchdog_state = state.clone();

    axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            match sigterm {
                Ok(mut sig) => {
                    tokio::select! {
                        _ = tokio::signal::ctrl_c() => {
                            tracing::info!("Ctrl-C received, shutting down...");
                        }
                        _ = sig.recv() => {
                            tracing::info!("SIGTERM received, shutting down...");
                        }
                        _ = ct.cancelled() => {
                            tracing::info!("Shutdown requested, shutting down...");
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!("Failed to register SIGTERM handler: {e}, using Ctrl-C only");
                    tokio::select! {
                        _ = tokio::signal::ctrl_c() => {
                            tracing::info!("Ctrl-C received, shutting down...");
                        }
                        _ = ct.cancelled() => {
                            tracing::info!("Shutdown requested, shutting down...");
                        }
                    }
                }
            }
            // axum (and the Unix-socket server) now wait for in-flight
            // connections to drain, but a `bw` dashboard holds a `/events` SSE
            // stream that never closes, so the drain would hang forever — the
            // daemon would only die when the service manager escalates to
            // SIGKILL. Persist state and force a clean exit if the drain doesn't
            // finish promptly. If the normal path completes first it returns and
            // the runtime drops this task, so the save runs exactly once.
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                tracing::warn!(
                    "Shutdown drain timed out (long-lived SSE client); saving state and forcing exit."
                );
                let persisted = collect_persisted(&watchdog_state.watches).await;
                let hist = watchdog_state.handle.history.lock().await.clone();
                watchdog_state
                    .handle
                    .persistence
                    .save_state(&persisted, &hist)
                    .await;
                std::process::exit(0);
            });
            ct.cancel();
        })
        .await
        .map_err(ServerError::Io)?;

    // Wait for the Unix socket server to finish draining in-flight requests before
    // we save state, so no concurrent mutations are lost.
    #[cfg(unix)]
    if let Some(task) = unix_task {
        let _ = task.await;
    }

    state.handle.shutdown().await;
    let persisted = collect_persisted(&state.watches).await;
    let hist = state.handle.history.lock().await.clone();
    state.handle.persistence.save_state(&persisted, &hist).await;
    let _ = std::fs::remove_file(&port_file);
    #[cfg(unix)]
    if let Some(path) = unix_sock_path {
        let _ = std::fs::remove_file(&path);
    }
    tracing::info!("State saved, goodbye.");

    Ok(())
}
