use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::platform;

// -- Directories (computed once) --

static STATE_DIR: OnceLock<PathBuf> = OnceLock::new();
static CONFIG_DIR: OnceLock<PathBuf> = OnceLock::new();

fn init_dir(dir: &Path) {
    if let Err(e) = std::fs::create_dir_all(dir) {
        tracing::error!("Failed to create directory {}: {e}", dir.display());
    }
}

pub fn state_dir() -> &'static Path {
    STATE_DIR.get_or_init(|| {
        let dir = PathBuf::from(
            std::env::var("STATE_DIRECTORY").unwrap_or_else(|_| platform::default_state_dir()),
        );
        init_dir(&dir);
        dir
    })
}

pub fn config_dir() -> &'static Path {
    CONFIG_DIR.get_or_init(|| {
        let dir = PathBuf::from(
            std::env::var("CONFIGURATION_DIRECTORY")
                .unwrap_or_else(|_| platform::default_config_dir()),
        );
        init_dir(&dir);
        dir
    })
}

// -- Safe JSON persistence --
//
// Crash-safe write sequence:
// 1. Serialize → write to .draft → fsync  (crash here: .draft lost, primary intact)
// 2. Parse .draft back to verify           (crash here: .draft orphaned, primary intact)
// 3. Rename primary → .bak                 (crash here: .bak exists, load recovers from it)
// 4. Rename .draft → primary               (crash here: primary missing, load recovers from .bak)
//
// On load, we transparently fall back to .bak if the primary is missing or corrupt.

pub fn load_json<T: serde::de::DeserializeOwned>(path: &Path) -> Option<T> {
    if let Some(val) = try_parse_file::<T>(path) {
        return Some(val);
    }

    let bak = path.with_extension("json.bak");
    if let Some(val) = try_parse_file::<T>(&bak) {
        tracing::warn!("Primary {} corrupt, recovered from backup", path.display());
        let _ = std::fs::copy(&bak, path);
        return Some(val);
    }

    None
}

fn try_parse_file<T: serde::de::DeserializeOwned>(path: &Path) -> Option<T> {
    let data = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&data).ok()
}

#[derive(Debug, thiserror::Error)]
pub enum PersistError {
    #[error("failed to serialize: {0}")]
    Serialize(#[from] serde_json::Error),
    #[error("failed to write {path}: {source}")]
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("draft verification failed for {0}")]
    Verify(PathBuf),
    #[error("failed to rename {from} to {to}: {source}")]
    Rename {
        from: PathBuf,
        to: PathBuf,
        source: std::io::Error,
    },
}

pub fn save_json<T: Serialize>(path: PathBuf, value: &T) -> Result<(), PersistError> {
    let data = serde_json::to_string_pretty(value)?;

    let draft = path.with_extension("json.draft");

    // Write and fsync the draft file
    {
        let mut file = std::fs::File::create(&draft).map_err(|e| PersistError::Write {
            path: draft.clone(),
            source: e,
        })?;
        file.write_all(data.as_bytes())
            .map_err(|e| PersistError::Write {
                path: draft.clone(),
                source: e,
            })?;
        file.sync_all().map_err(|e| PersistError::Write {
            path: draft.clone(),
            source: e,
        })?;
    }

    // Verify the draft parses back as valid JSON before committing
    match std::fs::read_to_string(&draft) {
        Ok(readback) if serde_json::from_str::<serde_json::Value>(&readback).is_ok() => {}
        _ => {
            let _ = std::fs::remove_file(&draft);
            return Err(PersistError::Verify(draft));
        }
    }

    // Backup current file, then promote draft
    let bak = path.with_extension("json.bak");
    let _ = std::fs::rename(&path, &bak);
    if let Err(e) = std::fs::rename(&draft, &path) {
        // Try to restore backup
        let _ = std::fs::rename(&bak, &path);
        return Err(PersistError::Rename {
            from: draft,
            to: path,
            source: e,
        });
    }

    Ok(())
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
#[allow(clippy::struct_field_names)] // `build_` prefix is intentional domain naming
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
#[allow(clippy::struct_field_names)] // `build_` prefix is intentional domain naming
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

/// Sound-on-failure configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SoundConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Path to a .wav/.ogg file. None means use system default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sound_file: Option<String>,
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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub workflows: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sound_on_failure: Option<bool>,
    #[serde(default, skip_serializing_if = "NotificationOverrides::is_empty")]
    pub notifications: NotificationOverrides,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub branch_notifications: HashMap<String, BranchConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "default_branches")]
    pub default_branches: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ignored_workflows: Vec<String>,
    #[serde(default)]
    pub notifications: NotificationConfig,
    #[serde(default)]
    pub sound_on_failure: SoundConfig,
    #[serde(default)]
    pub repos: HashMap<String, RepoConfig>,
}

fn default_branches() -> Vec<String> {
    vec!["main".to_string()]
}

impl Default for Config {
    fn default() -> Self {
        Self {
            default_branches: default_branches(),
            ignored_workflows: Vec::new(),
            notifications: NotificationConfig::default(),
            sound_on_failure: SoundConfig::default(),
            repos: HashMap::new(),
        }
    }
}

impl Config {
    /// Resolve effective notification levels for a repo/branch.
    /// Priority: branch overrides > repo overrides > global defaults.
    pub fn notifications_for(&self, repo: &str, branch: &str) -> NotificationConfig {
        let global = &self.notifications;
        let repo_cfg = self.repos.get(repo);
        let repo_notif = repo_cfg.map(|r| &r.notifications);
        let branch_notif = repo_cfg
            .and_then(|r| r.branch_notifications.get(branch))
            .map(|b| &b.notifications);

        let resolve = |get_field: fn(&NotificationOverrides) -> Option<NotificationLevel>,
                       global_val: NotificationLevel|
         -> NotificationLevel {
            branch_notif
                .and_then(get_field)
                .or_else(|| repo_notif.and_then(get_field))
                .unwrap_or(global_val)
        };

        NotificationConfig {
            build_started: resolve(|o| o.build_started, global.build_started),
            build_success: resolve(|o| o.build_success, global.build_success),
            build_failure: resolve(|o| o.build_failure, global.build_failure),
        }
    }

    /// Whether sound on failure is enabled for a repo (repo override > global).
    pub fn sound_on_failure_for(&self, repo: &str) -> bool {
        self.repos
            .get(repo)
            .and_then(|r| r.sound_on_failure)
            .unwrap_or(self.sound_on_failure.enabled)
    }

    /// Workflow filter for a repo. Empty slice means all workflows.
    pub fn workflows_for(&self, repo: &str) -> &[String] {
        self.repos
            .get(repo)
            .filter(|r| !r.workflows.is_empty())
            .map_or(&[], |r| r.workflows.as_slice())
    }

    pub fn branches_for(&self, repo: &str) -> &[String] {
        self.repos
            .get(repo)
            .filter(|r| !r.branches.is_empty())
            .map_or(&self.default_branches, |r| r.branches.as_slice())
    }

    pub fn watched_repos(&self) -> Vec<&String> {
        let mut repos: Vec<_> = self.repos.keys().collect();
        repos.sort();
        repos
    }

    /// Returns just the repo name (e.g. `"bar"`) when it is unique among all watched
    /// repos, or the full `"owner/repo"` string when another watched repo shares the
    /// same name (e.g. both `"foo/bar"` and `"zoo/bar"` are watched).
    pub fn short_repo<'a>(&self, repo: &'a str) -> &'a str {
        let Some((_, name)) = repo.rsplit_once('/') else {
            return repo;
        };
        let ambiguous = self
            .repos
            .keys()
            .any(|r| r != repo && r.rsplit_once('/').map_or(r.as_str(), |(_, n)| n) == name);
        if ambiguous { repo } else { name }
    }

    pub fn add_repos(&mut self, repos: &[String]) {
        for repo in repos {
            self.repos.entry(repo.clone()).or_default();
        }
    }
}

/// Load config from disk. Returns the config and whether the primary file was valid
/// (as opposed to falling back to backup or defaults). Callers should only re-save
/// to normalize schema when the primary loaded successfully.
fn load_config() -> (Config, bool) {
    let path = config_dir().join("config.json");
    if let Some(val) = try_parse_file::<Config>(&path) {
        return (val, true);
    }
    let bak = path.with_extension("json.bak");
    if let Some(val) = try_parse_file::<Config>(&bak) {
        tracing::warn!("Primary config corrupt, recovered from backup");
        let _ = std::fs::copy(&bak, &path);
        return (val, false);
    }
    (Config::default(), false)
}

/// Load config and re-save to normalize the schema (adds missing fields).
/// Skips re-save when the primary was corrupt or missing to avoid overwriting
/// a user-edited file with defaults.
pub fn load_and_normalize() -> Config {
    let (cfg, primary_ok) = load_config();
    if primary_ok && let Err(e) = save_config(&cfg) {
        tracing::error!("Failed to save config on startup: {e}");
    }
    cfg
}

pub fn save_config(config: &Config) -> Result<(), PersistError> {
    save_json(config_dir().join("config.json"), config)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notifications_for_global_defaults() {
        let config = Config::default();
        let n = config.notifications_for("any/repo", "main");
        assert_eq!(n.build_started, NotificationLevel::Normal);
        assert_eq!(n.build_success, NotificationLevel::Normal);
        assert_eq!(n.build_failure, NotificationLevel::Critical);
    }

    #[test]
    fn notifications_for_repo_override() {
        let mut config = Config::default();
        config.repos.insert(
            "alice/app".to_string(),
            RepoConfig {
                notifications: NotificationOverrides {
                    build_started: Some(NotificationLevel::Off),
                    build_success: None,
                    build_failure: Some(NotificationLevel::Low),
                },
                ..Default::default()
            },
        );
        let n = config.notifications_for("alice/app", "main");
        assert_eq!(n.build_started, NotificationLevel::Off);
        assert_eq!(n.build_success, NotificationLevel::Normal); // inherited from global
        assert_eq!(n.build_failure, NotificationLevel::Low);
    }

    #[test]
    fn notifications_for_branch_override() {
        let mut config = Config::default();
        let mut branch_notifications = HashMap::new();
        branch_notifications.insert(
            "release".to_string(),
            BranchConfig {
                notifications: NotificationOverrides {
                    build_started: Some(NotificationLevel::Off),
                    build_success: Some(NotificationLevel::Critical),
                    build_failure: None,
                },
            },
        );
        config.repos.insert(
            "alice/app".to_string(),
            RepoConfig {
                notifications: NotificationOverrides {
                    build_failure: Some(NotificationLevel::Low),
                    ..Default::default()
                },
                branch_notifications,
                ..Default::default()
            },
        );
        let n = config.notifications_for("alice/app", "release");
        assert_eq!(n.build_started, NotificationLevel::Off); // from branch
        assert_eq!(n.build_success, NotificationLevel::Critical); // from branch
        assert_eq!(n.build_failure, NotificationLevel::Low); // from repo (branch is None)
    }

    #[test]
    fn save_load_roundtrip() {
        let dir = std::env::temp_dir().join(format!("bw-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");

        let mut config = Config::default();
        config.repos.insert(
            "alice/app".to_string(),
            RepoConfig {
                branches: vec!["main".to_string(), "release".to_string()],
                ..Default::default()
            },
        );

        save_json(path.clone(), &config).unwrap();
        let loaded: Config = load_json(&path).unwrap();
        assert_eq!(loaded.repos.len(), 1);
        assert_eq!(
            loaded.repos["alice/app"].branches,
            vec!["main".to_string(), "release".to_string()]
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_falls_back_to_backup() {
        let dir = std::env::temp_dir().join(format!("bw-test-bak-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        let bak = dir.join("config.json.bak");

        let config = Config::default();
        // Write backup only
        std::fs::write(&bak, serde_json::to_string_pretty(&config).unwrap()).unwrap();
        // Write corrupt primary
        std::fs::write(&path, "not json").unwrap();

        let loaded: Config = load_json(&path).unwrap();
        assert_eq!(loaded.default_branches, vec!["main".to_string()]);

        std::fs::remove_dir_all(&dir).ok();
    }
}
