use std::collections::HashMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Current Unix epoch in seconds.
pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock before Unix epoch")
        .as_secs()
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
            Self::Medium => 0.40,
            Self::High => 0.80,
        }
    }

    /// The number of API calls this level allows per rate-limit window.
    pub fn target_calls(self, limit: u64) -> u64 {
        (self.target_fraction() * limit as f64) as u64
    }

    /// Multiplier applied to poll intervals in the free zone.
    /// High = 1.0 (floor speed), Medium = 1.5×, Low = 5×.
    pub fn interval_multiplier(self) -> f64 {
        match self {
            Self::High => 1.0,
            Self::Medium => 1.5,
            Self::Low => 5.0,
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

impl NotificationConfig {
    /// Returns `true` when all notification levels are set to `Off` (i.e. effectively muted).
    pub fn is_all_off(&self) -> bool {
        self.build_started == NotificationLevel::Off
            && self.build_success == NotificationLevel::Off
            && self.build_failure == NotificationLevel::Off
    }
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
    pub auto_discover_branches: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch_filter: Option<String>,
    #[serde(default)]
    pub repos: HashMap<String, RepoConfig>,
}

pub(crate) fn default_branches() -> Vec<String> {
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
            auto_discover_branches: false,
            branch_filter: None,
            repos: HashMap::new(),
        }
    }
}

impl Config {
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

    pub fn add_repos(&mut self, repos: &[String]) {
        for repo in repos {
            self.repos.entry(repo.clone()).or_default();
        }
    }
}
