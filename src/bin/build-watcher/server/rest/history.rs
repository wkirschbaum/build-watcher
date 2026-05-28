//! Build history endpoints. Returns persisted `LastBuild` entries flattened
//! into `HistoryEntryView` for the client.

use axum::extract::{Query, State};
use axum::response::IntoResponse as _;
use serde::Deserialize;

use build_watcher::config::unix_now;
use build_watcher::history::{history_all, history_for};
use build_watcher::status::HistoryEntryView;

use super::super::DaemonState;

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
        .map(|(br, lb)| to_history_view(q.repo.clone(), br, lb, now))
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
