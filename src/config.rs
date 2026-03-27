use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use chrono::Timelike;

/// Current Unix epoch in seconds.
pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock before Unix epoch")
        .as_secs()
}

// -- Directories (computed once) --

static STATE_DIR: OnceLock<PathBuf> = OnceLock::new();
static CONFIG_DIR: OnceLock<PathBuf> = OnceLock::new();

fn home_dir() -> String {
    std::env::var("HOME").unwrap_or_else(|_| {
        tracing::warn!("HOME is not set; falling back to /tmp for state/config directories");
        "/tmp".to_string()
    })
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
compile_error!("Unsupported platform: only Linux and macOS are supported");

#[cfg(target_os = "linux")]
fn default_state_dir() -> String {
    format!("{}/.local/state/build-watcher", home_dir())
}

#[cfg(target_os = "linux")]
fn default_config_dir() -> String {
    format!("{}/.config/build-watcher", home_dir())
}

#[cfg(target_os = "macos")]
fn default_state_dir() -> String {
    format!(
        "{}/Library/Application Support/build-watcher/state",
        home_dir()
    )
}

#[cfg(target_os = "macos")]
fn default_config_dir() -> String {
    format!(
        "{}/Library/Application Support/build-watcher/config",
        home_dir()
    )
}

fn init_dir(dir: &Path) {
    if let Err(e) = std::fs::create_dir_all(dir) {
        tracing::error!("Failed to create directory {}: {e}", dir.display());
    }
}

pub fn state_dir() -> &'static Path {
    STATE_DIR.get_or_init(|| {
        let dir =
            PathBuf::from(std::env::var("STATE_DIRECTORY").unwrap_or_else(|_| default_state_dir()));
        init_dir(&dir);
        dir
    })
}

pub fn config_dir() -> &'static Path {
    CONFIG_DIR.get_or_init(|| {
        let dir = PathBuf::from(
            std::env::var("CONFIGURATION_DIRECTORY").unwrap_or_else(|_| default_config_dir()),
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
    match serde_json::from_str(&data) {
        Ok(val) => Some(val),
        Err(e) => {
            if !data.trim().is_empty() {
                tracing::warn!("{}: parse failed: {e}", path.display());
            }
            None
        }
    }
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

/// Async wrapper around `save_json` that runs the blocking I/O on a dedicated thread.
pub async fn save_json_async<T: Serialize + Send + 'static>(
    path: PathBuf,
    value: T,
) -> Result<(), PersistError> {
    match tokio::task::spawn_blocking(move || save_json(&path, &value)).await {
        Ok(result) => result,
        Err(e) => {
            tracing::error!("save_json_async: blocking task panicked: {e}");
            Err(PersistError::Serialize(serde_json::Error::io(
                std::io::Error::other("blocking task panicked"),
            )))
        }
    }
}

pub fn save_json<T: Serialize>(path: &Path, value: &T) -> Result<(), PersistError> {
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
    if let Err(e) = std::fs::rename(path, &bak) {
        // NotFound is expected on the first save; anything else is worth logging.
        if e.kind() != std::io::ErrorKind::NotFound {
            tracing::warn!("Failed to create backup {}: {e}", bak.display());
        }
    }
    if let Err(e) = std::fs::rename(&draft, path) {
        // Try to restore backup
        let _ = std::fs::rename(&bak, path);
        return Err(PersistError::Rename {
            from: draft,
            to: path.to_path_buf(),
            source: e,
        });
    }

    Ok(())
}

// -- Configuration --

/// Notification urgency level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum NotificationLevel {
    Off,
    Low,
    Normal,
    Critical,
}

/// Number of per-event notification types (started, success, failure).
pub const NOTIFICATION_EVENT_COUNT: usize = 3;

impl<'de> Deserialize<'de> for NotificationLevel {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Ok(match s.to_lowercase().as_str() {
            "off" => Self::Off,
            "low" => Self::Low,
            "normal" => Self::Normal,
            "critical" => Self::Critical,
            other => {
                tracing::warn!("config: unknown notification level {other:?}, using 'normal'");
                Self::Normal
            }
        })
    }
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

impl NotificationLevel {
    const ALL: &[Self] = &[Self::Off, Self::Low, Self::Normal, Self::Critical];

    /// Advance to the next level, wrapping around.
    pub fn next(self) -> Self {
        let idx = Self::ALL.iter().position(|&v| v == self).unwrap_or(0);
        Self::ALL[(idx + 1) % Self::ALL.len()]
    }

    /// Retreat to the previous level, wrapping around.
    pub fn prev(self) -> Self {
        let idx = Self::ALL.iter().position(|&v| v == self).unwrap_or(0);
        Self::ALL[(idx + Self::ALL.len() - 1) % Self::ALL.len()]
    }
}

// -- Poll aggression --

/// Controls how aggressively the poller consumes the GitHub API rate-limit budget.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum PollAggression {
    /// Target ≤10% of the rate-limit per reset window.
    Low,
    /// Target ≤25% of the rate-limit per reset window (default).
    #[default]
    Medium,
    /// Target ≤50% of the rate-limit per reset window.
    High,
}

impl<'de> Deserialize<'de> for PollAggression {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Ok(match s.to_lowercase().as_str() {
            "low" => Self::Low,
            "medium" => Self::Medium,
            "high" => Self::High,
            other => {
                tracing::warn!("config: unknown poll_aggression {other:?}, using 'medium'");
                Self::Medium
            }
        })
    }
}

impl std::fmt::Display for PollAggression {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Low => write!(f, "low"),
            Self::Medium => write!(f, "medium"),
            Self::High => write!(f, "high"),
        }
    }
}

impl PollAggression {
    const ALL: &[Self] = &[Self::Low, Self::Medium, Self::High];

    /// Advance to the next level, wrapping around.
    pub fn next(self) -> Self {
        let idx = Self::ALL.iter().position(|&v| v == self).unwrap_or(1);
        Self::ALL[(idx + 1) % Self::ALL.len()]
    }

    /// Retreat to the previous level, wrapping around.
    pub fn prev(self) -> Self {
        let idx = Self::ALL.iter().position(|&v| v == self).unwrap_or(1);
        Self::ALL[(idx + Self::ALL.len() - 1) % Self::ALL.len()]
    }

    /// The fraction of the GitHub rate-limit budget this level targets per window.
    pub fn target_fraction(self) -> f64 {
        match self {
            Self::Low => 0.10,
            Self::Medium => 0.25,
            Self::High => 0.50,
        }
    }

    /// The number of API calls this level allows per rate-limit window.
    pub fn target_calls(self, limit: u64) -> u64 {
        (self.target_fraction() * limit as f64) as u64
    }

    /// Multiplier applied to poll intervals in the free zone.
    /// High = 1.0 (floor speed), Medium = 1.5×, Low = 3×.
    pub fn interval_multiplier(self) -> f64 {
        match self {
            Self::High => 1.0,
            Self::Medium => 1.5,
            Self::Low => 3.0,
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
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
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

/// Daily time window during which desktop notifications are suppressed.
/// Times are in 24-hour HH:MM format using local system time.
/// Overnight ranges are supported (e.g. `start = "22:00"`, `end = "08:00"`).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct QuietHours {
    /// Start of quiet period, e.g. `"22:00"`
    pub start: String,
    /// End of quiet period, e.g. `"08:00"`
    pub end: String,
}

/// Per-branch notification overrides.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct BranchConfig {
    #[serde(default, skip_serializing_if = "NotificationOverrides::is_empty")]
    pub notifications: NotificationOverrides,
}

/// Per-repo settings. Presence in the map means the repo is watched.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RepoConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub branches: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub workflows: Vec<String>,
    #[serde(default, skip_serializing_if = "NotificationOverrides::is_empty")]
    pub notifications: NotificationOverrides,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub branch_notifications: HashMap<String, BranchConfig>,
}

/// Current schema version. Bump when making breaking changes to the config format.
pub const CURRENT_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Schema version for forward-compatible migrations. Old files without this
    /// field deserialize as 0; `load_and_normalize` migrates them to `CURRENT_SCHEMA_VERSION`.
    #[serde(default)]
    pub schema_version: u32,
    #[serde(default = "default_branches")]
    pub default_branches: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ignored_workflows: Vec<String>,
    #[serde(default)]
    pub poll_aggression: PollAggression,
    #[serde(default)]
    pub notifications: NotificationConfig,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quiet_hours: Option<QuietHours>,
    #[serde(default)]
    pub repos: HashMap<String, RepoConfig>,
}

fn default_branches() -> Vec<String> {
    vec!["main".to_string()]
}

impl Default for Config {
    fn default() -> Self {
        Self {
            schema_version: CURRENT_SCHEMA_VERSION,
            default_branches: default_branches(),
            ignored_workflows: Vec::new(),
            notifications: NotificationConfig::default(),
            quiet_hours: None,
            poll_aggression: PollAggression::default(),
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

    /// Returns the display name for a repo. If an alias is set, returns it.
    /// Otherwise returns just the repo name (e.g. `"bar"`) when it is unique among
    /// all watched repos, or the full `"owner/repo"` when another repo shares the name.
    pub fn short_repo<'a>(&'a self, repo: &'a str) -> &'a str {
        if let Some(alias) = self.repos.get(repo).and_then(|r| r.alias.as_deref()) {
            return alias;
        }
        let Some((_, name)) = repo.rsplit_once('/') else {
            return repo;
        };
        let ambiguous = self
            .repos
            .keys()
            .any(|r| r != repo && r.rsplit_once('/').map_or(r.as_str(), |(_, n)| n) == name);
        if ambiguous { repo } else { name }
    }

    /// Returns `true` if the current local time falls within the configured quiet hours.
    pub fn is_in_quiet_hours(&self) -> bool {
        let Some(qh) = &self.quiet_hours else {
            return false;
        };
        let cur_mins = local_time_minutes();
        is_in_quiet_hours_at(qh, cur_mins)
    }

    pub fn add_repos(&mut self, repos: &[String]) {
        for repo in repos {
            self.repos.entry(repo.clone()).or_default();
        }
    }
}

/// Returns the current local time as minutes since midnight.
fn local_time_minutes() -> u32 {
    let now = chrono::Local::now();
    now.hour() * 60 + now.minute()
}

/// Pure helper for testability — takes the current time as `cur_mins` (minutes since midnight).
fn is_in_quiet_hours_at(qh: &QuietHours, cur_mins: u32) -> bool {
    let parse = |s: &str| -> Option<u32> {
        let (h, m) = s.split_once(':')?;
        let h: u32 = h.parse().ok()?;
        let m: u32 = m.parse().ok()?;
        if h > 23 || m > 59 {
            return None;
        }
        Some(h * 60 + m)
    };
    let (Some(start), Some(end)) = (parse(&qh.start), parse(&qh.end)) else {
        return false; // invalid config — never suppress
    };
    if start <= end {
        // Same-day range e.g. 09:00–17:00
        cur_mins >= start && cur_mins < end
    } else {
        // Overnight range e.g. 22:00–08:00
        cur_mins >= start || cur_mins < end
    }
}

/// Attempt to load a Config by deserializing each top-level field individually,
/// using the field's default when it is missing or has an invalid value.
/// Also recovers individual repo entries, skipping those that cannot be parsed.
/// Returns `None` only when the file cannot be read or is not a JSON object.
fn load_config_lenient(path: &Path) -> Option<Config> {
    let data = std::fs::read_to_string(path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&data).ok()?;
    let obj = value.as_object()?;

    let mut cfg = Config::default();

    macro_rules! field {
        ($key:literal, $field:expr) => {
            if let Some(v) = obj.get($key) {
                match serde_json::from_value(v.clone()) {
                    Ok(val) => $field = val,
                    Err(e) => {
                        tracing::warn!("config: invalid field {:?}: {e}, using default", $key)
                    }
                }
            }
        };
    }

    field!("default_branches", cfg.default_branches);
    field!("ignored_workflows", cfg.ignored_workflows);
    field!("notifications", cfg.notifications);
    field!("quiet_hours", cfg.quiet_hours);

    // Repos: load the map entry-by-entry so a single bad repo doesn't drop the rest.
    if let Some(serde_json::Value::Object(repos_obj)) = obj.get("repos") {
        for (repo, repo_val) in repos_obj {
            match serde_json::from_value::<RepoConfig>(repo_val.clone()) {
                Ok(rc) => {
                    cfg.repos.insert(repo.clone(), rc);
                }
                Err(e) => {
                    tracing::warn!("config: invalid entry for repo {repo:?}: {e}, skipping");
                }
            }
        }
    }

    Some(cfg)
}

/// Load config from disk.
///
/// Returns `(config, should_resave)`. `should_resave` is `true` when the caller
/// should write the config back to disk — either to normalise the schema after a
/// clean load, or to persist field-level corrections made during lenient recovery.
/// It is `false` when we fell back to the backup (we don't want to overwrite the
/// primary with backup data until the user has verified it).
fn load_config() -> (Config, bool) {
    let path = config_dir().join("config.json");

    if let Some(val) = try_parse_file::<Config>(&path) {
        return (val, true);
    }

    // Strict parse failed — try field-by-field recovery before falling back to backup.
    if let Some(val) = load_config_lenient(&path) {
        tracing::warn!(
            "Config has invalid field values; loaded with partial recovery. \
             The corrected config will be saved on startup."
        );
        return (val, true);
    }

    let bak = path.with_extension("json.bak");
    if let Some(val) = try_parse_file::<Config>(&bak) {
        tracing::warn!("Primary config corrupt, recovered from backup");
        let _ = std::fs::copy(&bak, &path);
        return (val, false);
    }

    if let Some(val) = load_config_lenient(&bak) {
        tracing::warn!("Backup config has invalid field values; loaded with partial recovery");
        return (val, true);
    }

    tracing::warn!("Config missing or unreadable; starting with defaults");
    (Config::default(), false)
}

/// Load config, apply startup validation, and re-save to normalise the schema.
///
/// Validation rules applied after loading:
/// - `default_branches` must not be empty (reset to `["main"]` if so).
///
/// Re-save is skipped when we fell back to the backup file, to avoid overwriting
/// the primary with backup data before the user has a chance to inspect it.
pub fn load_and_normalize() -> Config {
    let (mut cfg, should_resave) = load_config();
    let mut needs_save = should_resave;

    // Migrate from v0 (files saved before schema_version existed).
    if cfg.schema_version < CURRENT_SCHEMA_VERSION {
        tracing::info!(
            from = cfg.schema_version,
            to = CURRENT_SCHEMA_VERSION,
            "Migrating config schema"
        );
        cfg.schema_version = CURRENT_SCHEMA_VERSION;
        needs_save = true;
    }

    if cfg.default_branches.is_empty() {
        tracing::warn!(
            "config: 'default_branches' is empty — no branches would be watched. \
             Resetting to [\"main\"]."
        );
        cfg.default_branches = default_branches();
    }

    // Validate repo and branch names loaded from config.
    for branch in &cfg.default_branches {
        if let Err(e) = crate::github::validate_branch(branch) {
            tracing::warn!("config: invalid default branch: {e}");
        }
    }
    let repo_names: Vec<String> = cfg.repos.keys().cloned().collect();
    for repo in &repo_names {
        if let Err(e) = crate::github::validate_repo(repo) {
            tracing::warn!("config: invalid repo name: {e}");
            cfg.repos.remove(repo);
            needs_save = true;
        } else {
            let rc = &cfg.repos[repo];
            for branch in &rc.branches {
                if let Err(e) = crate::github::validate_branch(branch) {
                    tracing::warn!("config: {repo}: invalid branch: {e}");
                }
            }
        }
    }

    if needs_save && let Err(e) = save_config(&cfg) {
        tracing::error!("Failed to save config on startup: {e}");
    }
    cfg
}

pub fn save_config(config: &Config) -> Result<(), PersistError> {
    save_json(&config_dir().join("config.json"), config)
}

pub async fn save_config_async(config: &Config) -> Result<(), PersistError> {
    save_json_async(config_dir().join("config.json"), config.clone()).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn qh(start: &str, end: &str) -> QuietHours {
        QuietHours {
            start: start.to_string(),
            end: end.to_string(),
        }
    }

    #[test]
    fn quiet_hours_same_day_inside() {
        assert!(is_in_quiet_hours_at(&qh("09:00", "17:00"), 9 * 60));
        assert!(is_in_quiet_hours_at(&qh("09:00", "17:00"), 12 * 60));
        assert!(is_in_quiet_hours_at(&qh("09:00", "17:00"), 17 * 60 - 1));
    }

    #[test]
    fn quiet_hours_same_day_outside() {
        assert!(!is_in_quiet_hours_at(&qh("09:00", "17:00"), 8 * 60 + 59));
        assert!(!is_in_quiet_hours_at(&qh("09:00", "17:00"), 17 * 60));
        assert!(!is_in_quiet_hours_at(&qh("09:00", "17:00"), 23 * 60));
    }

    #[test]
    fn quiet_hours_overnight_inside() {
        assert!(is_in_quiet_hours_at(&qh("22:00", "08:00"), 22 * 60));
        assert!(is_in_quiet_hours_at(&qh("22:00", "08:00"), 23 * 60 + 59));
        assert!(is_in_quiet_hours_at(&qh("22:00", "08:00"), 0));
        assert!(is_in_quiet_hours_at(&qh("22:00", "08:00"), 7 * 60 + 59));
    }

    #[test]
    fn quiet_hours_overnight_outside() {
        assert!(!is_in_quiet_hours_at(&qh("22:00", "08:00"), 8 * 60));
        assert!(!is_in_quiet_hours_at(&qh("22:00", "08:00"), 21 * 60 + 59));
        assert!(!is_in_quiet_hours_at(&qh("22:00", "08:00"), 12 * 60));
    }

    #[test]
    fn quiet_hours_invalid_config_never_suppresses() {
        assert!(!is_in_quiet_hours_at(&qh("bad", "08:00"), 12 * 60));
        assert!(!is_in_quiet_hours_at(&qh("22:00", "99:00"), 23 * 60));
    }

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

        save_json(&path, &config).unwrap();
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

    fn config_with_repos(repos: &[&str]) -> Config {
        let mut config = Config::default();
        for repo in repos {
            config.repos.insert(repo.to_string(), RepoConfig::default());
        }
        config
    }

    #[test]
    fn short_repo_unique_name_returns_short() {
        let config = config_with_repos(&["alice/app"]);
        assert_eq!(config.short_repo("alice/app"), "app");
    }

    #[test]
    fn short_repo_ambiguous_name_returns_full() {
        let config = config_with_repos(&["alice/app", "bob/app"]);
        assert_eq!(config.short_repo("alice/app"), "alice/app");
        assert_eq!(config.short_repo("bob/app"), "bob/app");
    }

    #[test]
    fn short_repo_alias_overrides_auto_name() {
        let mut config = config_with_repos(&["alice/app"]);
        config.repos.get_mut("alice/app").unwrap().alias = Some("API".to_string());
        assert_eq!(config.short_repo("alice/app"), "API");
    }

    #[test]
    fn short_repo_alias_overrides_even_when_ambiguous() {
        let mut config = config_with_repos(&["alice/app", "bob/app"]);
        config.repos.get_mut("alice/app").unwrap().alias = Some("Alice API".to_string());
        assert_eq!(config.short_repo("alice/app"), "Alice API");
        assert_eq!(config.short_repo("bob/app"), "bob/app"); // still ambiguous, no alias
    }

    #[test]
    fn workflows_for_defaults_to_empty() {
        let config = Config::default();
        assert!(config.workflows_for("unknown/repo").is_empty());
        // Repo with no workflow filter also returns empty (= all workflows)
        let config = config_with_repos(&["alice/app"]);
        assert!(config.workflows_for("alice/app").is_empty());
    }

    #[test]
    fn workflows_for_returns_configured() {
        let mut config = config_with_repos(&["alice/app"]);
        config.repos.get_mut("alice/app").unwrap().workflows =
            vec!["CI".to_string(), "Deploy".to_string()];
        assert_eq!(config.workflows_for("alice/app"), ["CI", "Deploy"]);
    }

    #[test]
    fn branches_for_defaults_to_main() {
        let config = Config::default();
        assert_eq!(config.branches_for("unknown/repo"), ["main"]);
        // Repo with no branch config also falls back to defaults
        let config = config_with_repos(&["alice/app"]);
        assert_eq!(config.branches_for("alice/app"), ["main"]);
    }

    #[test]
    fn branches_for_returns_configured() {
        let mut config = config_with_repos(&["alice/app"]);
        config.repos.get_mut("alice/app").unwrap().branches =
            vec!["main".to_string(), "develop".to_string()];
        assert_eq!(config.branches_for("alice/app"), ["main", "develop"]);
    }

    #[test]
    fn add_repos_inserts_without_overwriting() {
        let mut config = Config::default();
        config.repos.insert(
            "alice/app".to_string(),
            RepoConfig {
                alias: Some("API".to_string()),
                ..Default::default()
            },
        );
        config.add_repos(&["alice/app".to_string(), "bob/lib".to_string()]);
        // Existing repo keeps its config
        assert_eq!(config.repos["alice/app"].alias.as_deref(), Some("API"));
        // New repo is added
        assert!(config.repos.contains_key("bob/lib"));
    }

    #[test]
    fn watched_repos_sorted() {
        assert!(Config::default().watched_repos().is_empty());

        let config = config_with_repos(&["zoo/app", "alice/lib", "bob/api"]);
        let repos: Vec<&str> = config.watched_repos().iter().map(|s| s.as_str()).collect();
        assert_eq!(repos, ["alice/lib", "bob/api", "zoo/app"]);
    }

    #[test]
    fn notification_overrides_is_empty() {
        assert!(NotificationOverrides::default().is_empty());
        assert!(
            !NotificationOverrides {
                build_started: Some(NotificationLevel::Off),
                ..Default::default()
            }
            .is_empty()
        );
    }

    #[test]
    fn try_parse_file_returns_none_for_missing_file() {
        let path = std::env::temp_dir().join("bw-nonexistent-99999.json");
        let result: Option<Config> = try_parse_file(&path);
        assert!(result.is_none());
    }

    #[test]
    fn try_parse_file_returns_none_for_corrupt_json() {
        let dir = std::env::temp_dir().join(format!("bw-test-corrupt-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("corrupt.json");
        std::fs::write(&path, "{ this is not json }").unwrap();
        let result: Option<Config> = try_parse_file(&path);
        assert!(result.is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn default_branches_empty_is_reset_to_main() {
        // load_config_lenient accepts an empty array for default_branches;
        // load_and_normalize must then reset it.
        let dir = std::env::temp_dir().join(format!("bw-test-empty-br-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        std::fs::write(&path, r#"{"default_branches": []}"#).unwrap();

        let cfg = load_config_lenient(&path).unwrap();
        assert!(
            cfg.default_branches.is_empty(),
            "lenient load keeps the empty vec as-is"
        );

        // Simulate load_and_normalize validation
        let mut cfg = cfg;
        if cfg.default_branches.is_empty() {
            cfg.default_branches = default_branches();
        }
        assert_eq!(cfg.default_branches, vec!["main".to_string()]);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn notification_level_unknown_variant_falls_back_to_normal() {
        let level: NotificationLevel = serde_json::from_str("\"urgent\"").unwrap();
        assert_eq!(level, NotificationLevel::Normal);
    }

    #[test]
    fn notification_level_case_insensitive() {
        let level: NotificationLevel = serde_json::from_str("\"CRITICAL\"").unwrap();
        assert_eq!(level, NotificationLevel::Critical);
        let level: NotificationLevel = serde_json::from_str("\"Off\"").unwrap();
        assert_eq!(level, NotificationLevel::Off);
    }

    #[test]
    fn notification_level_display_roundtrip() {
        for level in [
            NotificationLevel::Off,
            NotificationLevel::Low,
            NotificationLevel::Normal,
            NotificationLevel::Critical,
        ] {
            let s = level.to_string();
            let parsed: NotificationLevel =
                serde_json::from_value(serde_json::Value::String(s)).unwrap();
            assert_eq!(parsed, level);
        }
    }

    #[test]
    fn lenient_load_recovers_bad_field_value() {
        let dir = std::env::temp_dir().join(format!("bw-test-lenient-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");

        // Write config with a wrong type for default_branches (number instead of array)
        std::fs::write(
            &path,
            r#"{"default_branches": 42, "repos": {"alice/app": {}}}"#,
        )
        .unwrap();

        let cfg = load_config_lenient(&path).unwrap();
        // Bad field falls back to default
        assert_eq!(cfg.default_branches, vec!["main".to_string()]);
        // Good field is preserved
        assert!(cfg.repos.contains_key("alice/app"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn lenient_load_skips_bad_repo_entry() {
        let dir = std::env::temp_dir().join(format!("bw-test-repo-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");

        // One repo has branches as a number (invalid), the other is fine
        std::fs::write(
            &path,
            r#"{"repos": {"alice/app": {"branches": 999}, "bob/lib": {}}}"#,
        )
        .unwrap();

        let cfg = load_config_lenient(&path).unwrap();
        assert!(
            !cfg.repos.contains_key("alice/app"),
            "bad repo should be skipped"
        );
        assert!(
            cfg.repos.contains_key("bob/lib"),
            "good repo should be kept"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn schema_version_defaults_to_zero_for_old_configs() {
        let cfg: Config = serde_json::from_str(r#"{"default_branches": ["main"]}"#).unwrap();
        assert_eq!(cfg.schema_version, 0);
    }

    #[test]
    fn schema_version_round_trips() {
        let cfg = Config::default();
        assert_eq!(cfg.schema_version, CURRENT_SCHEMA_VERSION);
        let json = serde_json::to_string(&cfg).unwrap();
        let loaded: Config = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.schema_version, CURRENT_SCHEMA_VERSION);
    }
}
