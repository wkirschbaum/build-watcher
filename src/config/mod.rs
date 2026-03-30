mod resolve;
mod types;

pub use types::*;

use std::path::Path;

use crate::dirs::config_dir;
use crate::persistence::{PersistError, recover_draft, save_json, save_json_async, try_parse_file};

/// Attempt to load a Config by merging each top-level field from the file into
/// `Config::default()`, falling back to the default when a field is missing or
/// has an invalid value.
///
/// Works automatically for any field on `Config` — no manual field list to
/// maintain. The `repos` map gets special treatment: entries are loaded
/// individually so a single bad repo doesn't drop the rest.
///
/// Returns `None` only when the file cannot be read or is not a JSON object.
fn load_config_lenient(path: &Path) -> Option<Config> {
    let data = std::fs::read_to_string(path).ok()?;
    let file_obj: serde_json::Value = serde_json::from_str(&data).ok()?;
    let file_map = file_obj.as_object()?;

    // Start from a serialized default so every field is present.
    let mut base: serde_json::Map<String, serde_json::Value> =
        serde_json::to_value(Config::default())
            .ok()?
            .as_object()
            .cloned()?;

    for (key, file_val) in file_map {
        if key == "repos" {
            // Repos: load entry-by-entry so one bad repo doesn't drop the rest.
            let mut repos = serde_json::Map::new();
            if let Some(repos_obj) = file_val.as_object() {
                for (repo, repo_val) in repos_obj {
                    match serde_json::from_value::<RepoConfig>(repo_val.clone()) {
                        Ok(_) => {
                            repos.insert(repo.clone(), repo_val.clone());
                        }
                        Err(e) => {
                            tracing::warn!(
                                "config: invalid entry for repo {repo:?}: {e}, skipping"
                            );
                        }
                    }
                }
            }
            base.insert("repos".to_string(), serde_json::Value::Object(repos));
            continue;
        }

        // For all other fields: try inserting the file's value. If the result
        // still deserializes into a valid Config, keep it; otherwise revert.
        let old = base.insert(key.clone(), file_val.clone());
        let candidate = serde_json::Value::Object(base.clone());
        if serde_json::from_value::<Config>(candidate).is_err() {
            tracing::warn!("config: invalid field {key:?}, using default");
            match old {
                Some(v) => base.insert(key.clone(), v),
                None => base.remove(key),
            };
        }
    }

    match serde_json::from_value::<Config>(serde_json::Value::Object(base)) {
        Ok(cfg) => Some(cfg),
        Err(e) => {
            tracing::error!("config: lenient merge produced invalid Config: {e}");
            None
        }
    }
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
    recover_draft(&path);

    if let Some(val) = try_parse_file::<Config>(&path) {
        // Only re-save if a migration or correction is needed (checked by caller).
        return (val, false);
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

/// Mutex that serializes config writes so concurrent `save_config_async` calls
/// don't race on the `.draft` temp file (which could cause an older snapshot to
/// overwrite a newer one).
static CONFIG_SAVE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

pub async fn save_config_async(config: &Config) -> Result<(), PersistError> {
    let _guard = CONFIG_SAVE_LOCK.lock().await;
    save_json_async(config_dir().join("config.json"), config.clone()).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persistence::{load_json, recover_draft, save_json};

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

    #[test]
    fn load_returns_none_when_both_missing() {
        let dir = std::env::temp_dir().join(format!("bw-test-none-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        let result: Option<Config> = load_json(&path);
        assert!(result.is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn save_creates_backup_of_previous() {
        let dir = std::env::temp_dir().join(format!("bw-test-bak2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("data.json");
        let bak = dir.join("data.json.bak");

        // First save — no backup should be created.
        save_json(&path, &vec!["v1"]).unwrap();
        assert!(!bak.exists());

        // Second save — previous version becomes .bak.
        save_json(&path, &vec!["v2"]).unwrap();
        assert!(bak.exists());
        let backup: Vec<String> = load_json(&bak).unwrap();
        assert_eq!(backup, vec!["v1"]);

        // Primary has the new version.
        let current: Vec<String> = load_json(&path).unwrap();
        assert_eq!(current, vec!["v2"]);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn save_no_draft_left_behind() {
        let dir = std::env::temp_dir().join(format!("bw-test-draft-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("clean.json");
        let draft = dir.join("clean.json.draft");

        save_json(&path, &"hello").unwrap();
        assert!(path.exists());
        assert!(
            !draft.exists(),
            "draft file should be cleaned up after save"
        );

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

    #[test]
    fn draft_promoted_when_primary_missing() {
        let dir =
            std::env::temp_dir().join(format!("bw-test-draft-recover-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        let draft = dir.join("config.json.draft");

        // Simulate interrupted save: draft exists, primary does not.
        let mut config = Config::default();
        config
            .repos
            .insert("alice/app".to_string(), RepoConfig::default());
        std::fs::write(&draft, serde_json::to_string_pretty(&config).unwrap()).unwrap();

        recover_draft(&path);
        assert!(path.exists(), "draft should be promoted to primary");
        assert!(!draft.exists(), "draft should be removed after promotion");

        let loaded: Config = load_json(&path).unwrap();
        assert!(loaded.repos.contains_key("alice/app"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn draft_preferred_over_backup_when_primary_missing() {
        let dir = std::env::temp_dir().join(format!("bw-test-draft-vs-bak-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        let draft = dir.join("config.json.draft");
        let bak = dir.join("config.json.bak");

        // Simulate interrupted save: draft has latest data, bak has older data.
        let mut new_config = Config::default();
        new_config
            .repos
            .insert("alice/app".to_string(), RepoConfig::default());
        new_config
            .repos
            .insert("bob/lib".to_string(), RepoConfig::default());
        std::fs::write(&draft, serde_json::to_string_pretty(&new_config).unwrap()).unwrap();

        let old_config = Config::default(); // no repos
        std::fs::write(&bak, serde_json::to_string_pretty(&old_config).unwrap()).unwrap();

        let loaded: Config = load_json(&path).unwrap();
        assert_eq!(loaded.repos.len(), 2, "should load from draft, not backup");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn stale_draft_removed_when_primary_valid() {
        let dir = std::env::temp_dir().join(format!("bw-test-stale-draft-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        let draft = dir.join("config.json.draft");

        // Primary is valid, draft is leftover from a completed save.
        let config = Config::default();
        std::fs::write(&path, serde_json::to_string_pretty(&config).unwrap()).unwrap();
        std::fs::write(&draft, r#"{"stale": true}"#).unwrap();

        recover_draft(&path);
        assert!(!draft.exists(), "stale draft should be removed");
        assert!(path.exists());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn corrupt_draft_removed() {
        let dir = std::env::temp_dir().join(format!("bw-test-bad-draft-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        let draft = dir.join("config.json.draft");

        // No primary, corrupt draft — should fall through cleanly.
        std::fs::write(&draft, "not json {{{").unwrap();

        recover_draft(&path);
        assert!(!draft.exists(), "corrupt draft should be removed");
        assert!(
            !path.exists(),
            "no primary should be created from corrupt draft"
        );

        std::fs::remove_dir_all(&dir).ok();
    }
}
