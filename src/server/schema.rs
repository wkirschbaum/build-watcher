use schemars::JsonSchema;
use serde::Deserialize;

use build_watcher::config::NotificationLevel;

/// Visitor that deserializes a `Vec<String>` from either a JSON array or a
/// JSON-encoded string (e.g. `"[\"a\",\"b\"]"`). Some MCP clients double-encode
/// array parameters; this handles both forms transparently.
struct StringVecVisitor;

impl<'de> serde::de::Visitor<'de> for StringVecVisitor {
    type Value = Vec<String>;

    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("a string array or a JSON-encoded string array")
    }

    fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
        serde_json::from_str(v).map_err(serde::de::Error::custom)
    }

    fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        let mut vec = Vec::new();
        while let Some(item) = seq.next_element()? {
            vec.push(item);
        }
        Ok(vec)
    }
}

/// Deserialize a `Vec<String>` that may arrive as a JSON array or a JSON-encoded
/// string. Use with `#[serde(deserialize_with = "deserialize_string_or_vec")]`.
pub(crate) fn deserialize_string_or_vec<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserializer.deserialize_any(StringVecVisitor)
}

/// Like `deserialize_string_or_vec` but wraps the result in `Some`, and returns `None` for null
/// or absent fields (use with `#[serde(default, deserialize_with = "...")]`).
pub(crate) fn deserialize_opt_string_or_vec<'de, D>(
    deserializer: D,
) -> Result<Option<Vec<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct OptVisitor;

    impl<'de> serde::de::Visitor<'de> for OptVisitor {
        type Value = Option<Vec<String>>;

        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("a string array, a JSON-encoded string array, or null")
        }

        fn visit_none<E: serde::de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
            StringVecVisitor.visit_str(v).map(Some)
        }

        fn visit_seq<A: serde::de::SeqAccess<'de>>(self, seq: A) -> Result<Self::Value, A::Error> {
            StringVecVisitor.visit_seq(seq).map(Some)
        }
    }

    deserializer.deserialize_any(OptVisitor)
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(crate) struct ReposParams {
    /// List of GitHub repos in "owner/repo" format
    #[serde(deserialize_with = "deserialize_string_or_vec")]
    pub repos: Vec<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(crate) struct ConfigureBranchesParams {
    /// GitHub repo in "owner/repo" format. Omit to set the global default branches.
    pub repo: Option<String>,
    /// Branches to watch (e.g. `["main", "develop"]`)
    #[serde(deserialize_with = "deserialize_string_or_vec")]
    pub branches: Vec<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(crate) struct UpdateNotificationsParams {
    // --- Notification levels ---
    /// Scope: GitHub repo in "owner/repo" format. Omit for global defaults.
    pub repo: Option<String>,
    /// Scope: branch name. Requires repo.
    pub branch: Option<String>,
    /// Level for build started events (off, low, normal, critical)
    pub build_started: Option<NotificationLevel>,
    /// Level for build success events (off, low, normal, critical)
    pub build_success: Option<NotificationLevel>,
    /// Level for build failure events (off, low, normal, critical)
    pub build_failure: Option<NotificationLevel>,

    // --- Quiet hours ---
    /// Start of quiet window in HH:MM (24h) local time. Defaults to "22:00".
    pub quiet_start: Option<String>,
    /// End of quiet window in HH:MM (24h) local time. Defaults to "06:00".
    pub quiet_end: Option<String>,
    /// Set true to disable quiet hours entirely.
    pub quiet_clear: Option<bool>,

    // --- Pause control ---
    /// true = pause, false = resume. Combine with pause_minutes for a timed pause.
    pub pause: Option<bool>,
    /// Minutes to pause (only used when pause=true). Omit for indefinite.
    pub pause_minutes: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(crate) struct ConfigureRepoParams {
    /// GitHub repo in "owner/repo" format
    pub repo: String,
    /// Workflow allow-list. Empty = all workflows. Omit to leave unchanged.
    #[serde(default, deserialize_with = "deserialize_opt_string_or_vec")]
    pub workflows: Option<Vec<String>>,
    /// Display alias for notification titles. Omit to leave unchanged.
    pub alias: Option<String>,
    /// Set true to clear the alias entirely.
    pub clear_alias: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(crate) struct ConfigureIgnoredWorkflowsParams {
    /// Workflow names to add to the global ignore list (case-insensitive)
    #[serde(default, deserialize_with = "deserialize_string_or_vec")]
    pub add: Vec<String>,
    /// Workflow names to remove from the global ignore list (case-insensitive)
    #[serde(default, deserialize_with = "deserialize_string_or_vec")]
    pub remove: Vec<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(crate) struct RerunBuildParams {
    /// GitHub repo in "owner/repo" format
    pub repo: String,
    /// Run ID to rerun. Omit to rerun the last failed build.
    pub run_id: Option<u64>,
    /// If true, only rerun failed jobs within the run (default: false)
    #[serde(default)]
    pub failed_only: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(crate) struct SetPollAggressionParams {
    /// Poll aggression level: "low" (≤10% of rate-limit/hour), "medium" (≤25%, default), or "high" (≤50%)
    pub level: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(crate) struct BuildHistoryParams {
    /// GitHub repo in "owner/repo" format
    pub repo: String,
    /// Optional branch filter. If omitted, shows all branches.
    pub branch: Option<String>,
    /// Number of builds to show (default: 10, max: 50)
    pub limit: Option<u32>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn deser(json: &str) -> Result<Vec<String>, serde_json::Error> {
        let mut de = serde_json::Deserializer::from_str(json);
        deserialize_string_or_vec(&mut de)
    }

    #[test]
    fn deserialize_string_or_vec_variants() {
        assert_eq!(deser(r#"["a","b"]"#).unwrap(), ["a", "b"]);
        assert_eq!(deser(r#""[\"a\",\"b\"]""#).unwrap(), ["a", "b"]);
        assert!(deser(r#"[]"#).unwrap().is_empty());
        assert!(deser(r#""not json""#).is_err());
    }

    fn deser_opt(json: &str) -> Result<Option<Vec<String>>, serde_json::Error> {
        let mut de = serde_json::Deserializer::from_str(json);
        deserialize_opt_string_or_vec(&mut de)
    }

    #[test]
    fn deserialize_opt_string_or_vec_variants() {
        assert_eq!(
            deser_opt(r#"["a","b"]"#).unwrap(),
            Some(vec!["a".to_string(), "b".to_string()])
        );
        assert_eq!(
            deser_opt(r#""[\"x\"]""#).unwrap(),
            Some(vec!["x".to_string()])
        );
        assert_eq!(deser_opt("null").unwrap(), None);
        assert!(deser_opt(r#""not json""#).is_err());
    }
}
