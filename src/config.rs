use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::platform;

// -- Directories --

pub fn state_dir() -> PathBuf {
    let dir = PathBuf::from(
        std::env::var("STATE_DIRECTORY")
            .unwrap_or_else(|_| platform::default_state_dir()),
    );
    std::fs::create_dir_all(&dir).ok();
    dir
}

pub fn config_dir() -> PathBuf {
    let dir = PathBuf::from(
        std::env::var("CONFIGURATION_DIRECTORY")
            .unwrap_or_else(|_| platform::default_config_dir()),
    );
    std::fs::create_dir_all(&dir).ok();
    dir
}

// -- Safe JSON persistence --
// Writes to a .draft file, validates it parses back, then renames over the target.
// On load, falls back to .bak if the primary file is corrupt.

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

fn try_parse_file<T: serde::de::DeserializeOwned>(path: &PathBuf) -> Option<T> {
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

    // Verify the draft parses back before committing
    match std::fs::read_to_string(&draft) {
        Ok(readback) if readback == data => {}
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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

/// Per-repo settings. Presence in the map means the repo is watched.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RepoConfig {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub branches: Vec<String>,
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

    pub fn remove_repos(&mut self, repos: &[String]) {
        for repo in repos {
            self.repos.remove(repo);
        }
    }

    /// Migrate repos from watches.json if config has none yet.
    pub fn migrate_from_watches(&mut self, watch_keys: &[String]) {
        if !self.repos.is_empty() {
            return;
        }
        let mut migrated = 0;
        for key in watch_keys {
            if let Some((repo, _)) = key.rsplit_once('#') {
                self.repos.entry(repo.to_string()).or_default();
                migrated += 1;
            }
        }
        if migrated > 0 {
            tracing::info!("Migrated {} repos from watches into config", self.repos.len());
        }
    }
}

pub fn load_config() -> Config {
    load_json(config_dir().join("config.json")).unwrap_or_default()
}

pub fn save_config(config: &Config) {
    save_json(config_dir().join("config.json"), config);
}
