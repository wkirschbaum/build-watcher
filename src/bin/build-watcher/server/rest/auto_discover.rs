//! Auto-discover rule endpoints.

use axum::extract::State;
use axum::response::IntoResponse as _;
use serde::Deserialize;

use super::super::DaemonState;
use super::super::actions::{do_add_auto_discover_rule, do_remove_auto_discover_rule};
use super::super::json_error;

/// `GET /auto-discover-rules` — List all auto-discover rules.
pub(crate) async fn get_auto_discover_rules_handler(
    State(state): State<DaemonState>,
) -> axum::Json<Vec<build_watcher::status::AutoDiscoverRuleView>> {
    let cfg = state.config.read().await;
    let rules = cfg
        .auto_discover_rules
        .iter()
        .map(|r| build_watcher::status::AutoDiscoverRuleView {
            id: r.id.clone(),
            org_pattern: r.org_pattern.clone(),
            repo_pattern: r.repo_pattern.clone(),
            recently_updated: r.recently_updated.to_string(),
        })
        .collect();
    axum::Json(rules)
}

#[derive(Deserialize)]
pub(crate) struct AutoDiscoverRuleRequest {
    id: String,
    #[serde(default)]
    org_pattern: Option<String>,
    #[serde(default)]
    repo_pattern: Option<String>,
    #[serde(default)]
    recently_updated: Option<String>,
}

/// `POST /auto-discover-rules` — Add or replace an auto-discover rule.
pub(crate) async fn add_auto_discover_rule_handler(
    State(state): State<DaemonState>,
    axum::Json(body): axum::Json<AutoDiscoverRuleRequest>,
) -> axum::response::Response {
    use build_watcher::config::RecentlyUpdated;

    let recently_updated = match body
        .recently_updated
        .as_deref()
        .unwrap_or("any")
        .parse::<RecentlyUpdated>()
    {
        Ok(v) => v,
        Err(e) => return json_error(e),
    };
    match do_add_auto_discover_rule(
        &state,
        body.id,
        body.org_pattern,
        body.repo_pattern,
        recently_updated,
    )
    .await
    {
        Ok(msg) => axum::Json(serde_json::json!({ "ok": true, "message": msg })).into_response(),
        Err(e) => json_error(e),
    }
}

#[derive(Deserialize)]
pub(crate) struct RemoveRuleRequest {
    id: String,
}

/// `POST /auto-discover-rules/remove` — Remove an auto-discover rule by ID.
pub(crate) async fn remove_auto_discover_rule_handler(
    State(state): State<DaemonState>,
    axum::Json(body): axum::Json<RemoveRuleRequest>,
) -> axum::response::Response {
    match do_remove_auto_discover_rule(&state, &body.id).await {
        Ok(msg) => axum::Json(serde_json::json!({ "ok": true, "message": msg })).into_response(),
        Err(e) => json_error(e),
    }
}
