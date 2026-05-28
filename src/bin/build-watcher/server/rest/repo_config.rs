//! Per-repo config endpoints (`GET`/`POST /repo-config`).

use axum::extract::{Query, State};
use axum::response::IntoResponse as _;
use serde::Deserialize;

use build_watcher::github::validate_repo;

use super::super::DaemonState;
use super::super::json_error;

#[derive(Deserialize)]
pub(crate) struct RepoQuery {
    repo: String,
}

/// `GET /repo-config?repo=owner/name` — Read per-repo config.
pub(crate) async fn get_repo_config_handler(
    State(state): State<DaemonState>,
    Query(q): Query<RepoQuery>,
) -> axum::Json<build_watcher::status::RepoConfigView> {
    let auto_discovered_by_rule = state.handle.discovered_repos.lock().await.contains(&q.repo);
    let cfg = state.config.read().await;
    let rc = cfg.repos.get(&q.repo).cloned().unwrap_or_default();
    axum::Json(build_watcher::status::RepoConfigView {
        repo: q.repo,
        alias: rc.alias,
        workflows: Some(rc.workflows),
        watch_prs: Some(rc.watch_prs),
        poll_aggression: rc.poll_aggression,
        clear_poll_aggression: None,
        auto_discover_branches: rc.auto_discover_branches,
        branch_filter: rc.branch_filter,
        ignored_events: Some(rc.ignored_events),
        branches: Some(rc.branches),
        notifications: Some(rc.notifications),
        auto_discovered_by_rule: Some(auto_discovered_by_rule),
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

    if let Some(filter) = &body.branch_filter
        && !filter.is_empty()
        && let Err(e) = regex::Regex::new(filter)
    {
        return json_error(format!("invalid branch filter regex: {e}"));
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
            if body.clear_poll_aggression == Some(true) {
                rc.poll_aggression = None;
                messages.push("poll aggression: default (global)".to_string());
            } else if let Some(aggression) = body.poll_aggression {
                rc.poll_aggression = Some(aggression);
                messages.push(format!("poll aggression: {aggression}"));
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
            if let Some(events) = &body.ignored_events {
                rc.ignored_events = events.clone();
                if events.is_empty() {
                    messages.push("ignored events cleared".to_string());
                } else {
                    messages.push(format!("ignored events: {}", events.join(", ")));
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
