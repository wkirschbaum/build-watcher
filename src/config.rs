use std::collections::HashMap;
use std::path::{Path, PathBuf};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::platform;

// -- Directories --

pub fn state_dir() -> PathBuf {
    let dir = PathBuf::from(
        std::env::var("STATE_DIRECTORY").unwrap_or_else(|_| platform::default_state_dir()),
    );
    std::fs::create_dir_all(&dir).ok();
    dir
}

pub fn config_dir() -> PathBuf {
    let dir = PathBuf::from(
        std::env::var("CONFIGURATION_DIRECTORY").unwrap_or_else(|_| platform::default_config_dir()),
    );
    std::fs::create_dir_all(&dir).ok();
    dir
}

// -- Safe JSON persistence --
//
// The write sequence is: serialize → write to .draft → parse .draft back to
// confirm it is valid JSON → rename current file to .bak → rename .draft to
// the target path.
//
// This means a crash at any point leaves either the previous file or the
// backup intact — we never end up with a half-written primary. On load we
// transparently fall back to .bak if the primary is missing or corrupt.

pub fn load_json<T: serde::de::DeserializeOwned>(path: PathBuf) -> Option<T> {
    if let Some(val) = try_parse_file::<T>(&path) {
        return Some(val);
    }

    let bak = path.with_extension("json.bak");
    if let Some(val) = try_parse_file::<T>(&bak) {
        tracing::warn!("Primary {} corrupt, recovered from backup", path.display());
        let _ = std::fs::copy(&bak, &path);
        return Some(val);
    }

    None
}

fn try_parse_file<T: serde::de::DeserializeOwned>(path: &Path) -> Option<T> {
    let data = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&data).ok()
}

pub fn save_json<T: Serialize>(path: PathBuf, value: &T) {
    let data = match serde_json::to_string_pretty(value) {
        Ok(d) => d,
        Err(e) => {
            tracing::error!("Failed to serialize {}: {e}", path.display());
            return;
        }
    };

    let draft = path.with_extension("json.draft");
    if std::fs::write(&draft, &data).is_err() {
        tracing::error!("Failed to write draft {}", draft.display());
        return;
    }

    // Verify the draft parses back as valid JSON before committing
    match std::fs::read_to_string(&draft) {
        Ok(readback) if serde_json::from_str::<serde_json::Value>(&readback).is_ok() => {}
        _ => {
            tracing::error!("Draft verification failed for {}", draft.display());
            let _ = std::fs::remove_file(&draft);
            return;
        }
    }

    // Backup current file, then promote draft
    let bak = path.with_extension("json.bak");
    let _ = std::fs::rename(&path, &bak);
    if let Err(e) = std::fs::rename(&draft, &path) {
        tracing::error!("Failed to promote draft to {}: {e}", path.display());
        // Try to restore backup
        let _ = std::fs::rename(&bak, &path);
    }
}

// -- Configuration --

/// Notification urgency level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum NotificationLevel {
    Off,
    Low,
    Normal,
    Critical,
}

impl std::fmt::Display for NotificationLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Off => write!(f, "off"),
            Self::Low => write!(f, "low"),
            Self::Normal => write!(f, "normal"),
            Self::Critical => write!(f, "critical"),
        }
    }
}

/// Per-event notification levels.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotificationConfig {
    #[serde(default = "default_normal")]
    pub build_started: NotificationLevel,
    #[serde(default = "default_normal")]
    pub build_success: NotificationLevel,
    #[serde(default = "default_critical")]
    pub build_failure: NotificationLevel,
}

/// Optional per-event notification overrides. `None` means inherit from parent.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NotificationOverrides {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_started: Option<NotificationLevel>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_success: Option<NotificationLevel>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_failure: Option<NotificationLevel>,
}

impl NotificationOverrides {
    pub fn is_empty(&self) -> bool {
        self.build_started.is_none() && self.build_success.is_none() && self.build_failure.is_none()
    }
}

fn default_normal() -> NotificationLevel {
    NotificationLevel::Normal
}

fn default_critical() -> NotificationLevel {
    NotificationLevel::Critical
}

impl Default for NotificationConfig {
    fn default() -> Self {
        Self {
            build_started: NotificationLevel::Normal,
            build_success: NotificationLevel::Normal,
            build_failure: NotificationLevel::Critical,
        }
    }
}

/// Per-branch notification overrides.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BranchConfig {
    #[serde(default, skip_serializing_if = "NotificationOverrides::is_empty")]
    pub notifications: NotificationOverrides,
}

/// Per-repo settings. Presence in the map means the repo is watched.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RepoConfig {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub branches: Vec<String>,
    #[serde(default, skip_serializing_if = "NotificationOverrides::is_empty")]
    pub notifications: NotificationOverrides,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub branch_notifications: HashMap<String, BranchConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "default_branches")]
    pub default_branches: Vec<String>,
    #[serde(default)]
    pub notifications: NotificationConfig,
    #[serde(default = "default_active_poll_seconds")]
    pub active_poll_seconds: u64,
    #[serde(default = "default_idle_poll_seconds")]
    pub idle_poll_seconds: u64,
    #[serde(default)]
    pub repos: HashMap<String, RepoConfig>,
}

fn default_active_poll_seconds() -> u64 {
    10
}

fn default_idle_poll_seconds() -> u64 {
    60
}

fn default_branches() -> Vec<String> {
    vec!["main".to_string()]
}

impl Default for Config {
    fn default() -> Self {
        Self {
            default_branches: default_branches(),
            notifications: NotificationConfig::default(),
            active_poll_seconds: default_active_poll_seconds(),
            idle_poll_seconds: default_idle_poll_seconds(),
            repos: HashMap::new(),
        }
    }
}

impl Config {
    /// Resolve effective notification levels for a repo/branch.
    /// Priority: branch_notifications > repo notifications > global notifications.
    pub fn notifications_for(&self, repo: &str, branch: &str) -> NotificationConfig {
        let global = &self.notifications;
        let repo_cfg = self.repos.get(repo);
        let repo_overrides = repo_cfg.map(|r| &r.notifications);
        let branch_overrides = repo_cfg
            .and_then(|r| r.branch_notifications.get(branch))
            .map(|b| &b.notifications);

        NotificationConfig {
            build_started: branch_overrides
                .and_then(|o| o.build_started)
                .or_else(|| repo_overrides.and_then(|o| o.build_started))
                .unwrap_or(global.build_started),
            build_success: branch_overrides
                .and_then(|o| o.build_success)
                .or_else(|| repo_overrides.and_then(|o| o.build_success))
                .unwrap_or(global.build_success),
            build_failure: branch_overrides
                .and_then(|o| o.build_failure)
                .or_else(|| repo_overrides.and_then(|o| o.build_failure))
                .unwrap_or(global.build_failure),
        }
    }

    pub fn branches_for(&self, repo: &str) -> &[String] {
        self.repos
            .get(repo)
            .filter(|r| !r.branches.is_empty())
            .map(|r| r.branches.as_slice())
            .unwrap_or(&self.default_branches)
    }

    pub fn watched_repos(&self) -> Vec<&String> {
        let mut repos: Vec<_> = self.repos.keys().collect();
        repos.sort();
        repos
    }

    pub fn add_repos(&mut self, repos: &[String]) {
        for repo in repos {
            self.repos.entry(repo.clone()).or_default();
        }
    }
}

pub fn load_config() -> Config {
    load_json(config_dir().join("config.json")).unwrap_or_default()
}

pub fn save_config(config: &Config) {
    save_json(config_dir().join("config.json"), config);
}
