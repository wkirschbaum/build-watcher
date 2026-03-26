/// HTTP response types for `GET /status`.
///
/// Shared between the daemon (`server.rs`) and the TUI (`bin/bw.rs`).
use serde::{Deserialize, Serialize};

/// A single active run as returned by `GET /status`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActiveRunView {
    pub run_id: u64,
    pub status: String,
    pub workflow: String,
    /// Human-readable title: plain commit title for pushes, "PR: …" for PRs.
    pub title: String,
    /// GitHub event type (e.g. `"push"`, `"pull_request"`).
    pub event: String,
    pub elapsed_secs: Option<f64>,
}

/// Summary of the last completed build as returned by `GET /status`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LastBuildView {
    pub run_id: u64,
    pub conclusion: String,
    pub workflow: String,
    pub title: String,
    /// Comma-separated list of step names that failed, if available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failing_steps: Option<String>,
}

/// One watched repo/branch as returned by `GET /status`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WatchStatus {
    pub repo: String,
    pub branch: String,
    pub active_runs: Vec<ActiveRunView>,
    pub last_build: Option<LastBuildView>,
}

/// Full response body for `GET /status`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusResponse {
    pub paused: bool,
    pub watches: Vec<WatchStatus>,
}
