//! Notification level config endpoints — per-repo/branch resolved view and
//! mutation, plus the global defaults config.

use axum::extract::{Query, State};
use axum::response::IntoResponse as _;
use serde::Deserialize;

use build_watcher::config::{NotificationConfig, NotificationLevel};
use build_watcher::status::DefaultsConfig;

use super::super::DaemonState;
use super::super::json_error;

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
    use super::super::actions::do_notification_action;

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

/// `GET /defaults` — Read global default config.
pub(crate) async fn get_defaults_handler(
    State(state): State<DaemonState>,
) -> axum::Json<DefaultsConfig> {
    let cfg = state.config.read().await;
    axum::Json(DefaultsConfig {
        ignored_workflows: Some(cfg.ignored_workflows.clone()),
        ignored_events: Some(cfg.ignored_events.clone()),
        poll_aggression: Some(cfg.poll_aggression),
        auto_discover_branches: Some(cfg.auto_discover_branches),
        branch_filter: cfg.branch_filter.clone(),
        default_branches: Some(cfg.default_branches.clone()),
        show_author: Some(cfg.show_author),
        detect_flakes: Some(cfg.detect_flakes),
        notify_mode: Some(cfg.notify_mode),
    })
}

/// `POST /defaults` — Update global default config fields.
/// Accepts the same `DefaultsConfig` shape — `None` fields are left unchanged.
///
/// Validates request body before forwarding to the config writer:
/// - `branch_filter` must compile as a regex
/// - `notify_mode` must be a recognised mode string (no silent fallback)
///
/// `NotifyMode`'s `Deserialize` impl is intentionally lenient (so a hand-edited
/// config file with a typo doesn't refuse to load), but the REST API is strict —
/// callers deserve a 400 with the allowed values rather than a silent default.
pub(crate) async fn set_defaults_handler(
    State(state): State<DaemonState>,
    axum::Json(raw): axum::Json<serde_json::Value>,
) -> axum::response::Response {
    if let Some(mode) = raw.get("notify_mode").and_then(|v| v.as_str())
        && build_watcher::config::parse_notify_mode(mode).is_none()
    {
        return json_error(format!(
            "invalid notify_mode '{mode}', expected 'every_build' or 'failures_and_recoveries'"
        ));
    }

    let body: DefaultsConfig = match serde_json::from_value(raw) {
        Ok(b) => b,
        Err(e) => return json_error(format!("invalid request body: {e}")),
    };

    // Validate inputs before taking the config lock.
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
            if let Some(aggression) = body.poll_aggression {
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
            if let Some(branches) = body.default_branches {
                cfg.default_branches = branches.clone();
                if branches.is_empty() {
                    messages.push("default branches cleared".to_string());
                } else {
                    messages.push(format!("default branches: {}", branches.join(", ")));
                }
            }
            if let Some(enabled) = body.show_author {
                cfg.show_author = enabled;
                messages.push(format!(
                    "show author: {}",
                    if enabled { "on" } else { "off" }
                ));
            }
            if let Some(enabled) = body.detect_flakes {
                cfg.detect_flakes = enabled;
                messages.push(format!(
                    "flake detection: {}",
                    if enabled { "on" } else { "off" }
                ));
            }
            if let Some(mode) = body.notify_mode {
                cfg.notify_mode = mode;
                messages.push(format!("notify mode: {mode}"));
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
