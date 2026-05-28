//! Watch lifecycle and pause/rerun/pin/merge endpoints.

use axum::extract::State;
use axum::response::IntoResponse as _;
use serde::Deserialize;

use build_watcher::github::{validate_branch, validate_repo};

use super::super::DaemonState;
use super::super::actions::{
    apply_pause, do_configure_branches, do_merge, do_rerun, do_stop_watches, do_watch_builds,
};
use super::super::json_error;

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
pub(crate) struct PinRequest {
    repo: String,
    /// When omitted, pins/unpins the whole repo. When present, pins/unpins
    /// just that branch (creating a `BranchConfig` entry if needed).
    #[serde(default)]
    branch: Option<String>,
    pinned: bool,
}

/// `POST /pin` — Toggle the pinned flag on a repo or a specific branch.
/// Pinned watches appear in the TUI's dedicated "Pinned" section above
/// everything else.
pub(crate) async fn pin_handler(
    State(state): State<DaemonState>,
    axum::Json(body): axum::Json<PinRequest>,
) -> axum::response::Response {
    if let Err(e) = validate_repo(&body.repo) {
        return json_error(e);
    }
    if let Some(branch) = &body.branch
        && let Err(e) = build_watcher::github::validate_branch(branch)
    {
        return json_error(e);
    }

    let result = state
        .config
        .modify(|cfg| {
            let mut messages = Vec::new();
            let rc = cfg.repos.entry(body.repo.clone()).or_default();
            match &body.branch {
                None => {
                    rc.pinned = body.pinned;
                    // A repo pin replaces any individual branch pins: the repo
                    // pin already cascades to every branch, so leaving stale
                    // per-branch flags around would resurrect them when the
                    // repo is later unpinned. Clear them, dropping entries that
                    // become empty so the config doesn't accumulate stubs.
                    if body.pinned {
                        rc.branch_notifications.retain(|_, bc| {
                            bc.pinned = false;
                            !bc.is_empty()
                        });
                    }
                    messages.push(format!(
                        "{}: {}",
                        body.repo,
                        if body.pinned { "pinned" } else { "unpinned" }
                    ));
                }
                Some(branch) => {
                    if body.pinned {
                        rc.branch_notifications
                            .entry(branch.clone())
                            .or_default()
                            .pinned = true;
                    } else if let Some(bc) = rc.branch_notifications.get_mut(branch) {
                        // Unpin, then drop the entry if nothing else is left on it.
                        bc.pinned = false;
                        if bc.is_empty() {
                            rc.branch_notifications.remove(branch);
                        }
                    }
                    messages.push(format!(
                        "{}@{}: {}",
                        body.repo,
                        branch,
                        if body.pinned { "pinned" } else { "unpinned" }
                    ));
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
            axum::Json(serde_json::json!({ "ok": false, "messages": [], "warning": warning }))
                .into_response()
        }
    }
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
) -> axum::response::Response {
    for repo in &body.repos {
        if let Err(e) = validate_repo(repo) {
            return json_error(e);
        }
    }
    let results = do_stop_watches(&state, &body.repos).await;
    let messages: Vec<&str> = results.iter().map(|o| o.message()).collect();
    axum::Json(serde_json::json!({ "ok": true, "messages": messages })).into_response()
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

/// `POST /shutdown` — Initiate graceful daemon shutdown.
pub(crate) async fn shutdown_handler(
    State(state): State<DaemonState>,
) -> axum::Json<serde_json::Value> {
    tracing::info!("Shutdown requested via REST API");
    state.handle.cancel.cancel();
    axum::Json(serde_json::json!({ "ok": true, "message": "shutting down" }))
}
