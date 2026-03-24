use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::config::NotificationLevel;
use crate::platform::Notifier;

/// Linux desktop notifications via `notify-send`.
///
/// Uses `--print-id` / `--replace-id` to stack notifications per group (`owner/repo#branch`),
/// so each branch has its own notification slot. Requires notify-send ≥ 0.8 (libnotify).
///
/// When a URL is provided, adds an `--action open=Open` button that opens the URL
/// via `xdg-open` when clicked.
///
/// Note: there is a narrow race window where two simultaneous notifications for the same
/// branch may both lack a replace-id (if the first hasn't received its ID yet). In practice
/// this is harmless — both notifications are shown and the second ID wins the slot.
pub struct NotifySend {
    ids: Arc<Mutex<HashMap<String, u32>>>,
}

impl NotifySend {
    pub fn new() -> Self {
        Self {
            ids: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

impl Notifier for NotifySend {
    fn name(&self) -> &'static str {
        "notify-send"
    }

    fn send(
        &self,
        title: &str,
        body: &str,
        level: NotificationLevel,
        url: Option<&str>,
        group: Option<&str>,
    ) {
        let (icon, category, expire_ms) = match level {
            NotificationLevel::Low => ("emblem-synchronizing", "transfer", "4000"),
            NotificationLevel::Normal => ("emblem-ok", "transfer.complete", "6000"),
            NotificationLevel::Critical => ("dialog-error", "transfer.error", "0"),
            NotificationLevel::Off => unreachable!("Off is filtered before send()"),
        };
        let urgency = match level {
            NotificationLevel::Low => "low",
            NotificationLevel::Normal => "normal",
            NotificationLevel::Critical => "critical",
            NotificationLevel::Off => unreachable!("Off is filtered before send()"),
        };

        // group is "owner/repo#branch" — extract just "repo" for the visible app name
        let key = group.unwrap_or("build-watcher").to_string();
        let app_name = repo_name_from_group(&key).to_string();

        let mut args = vec![
            "--app-name".to_string(),
            app_name,
            "--urgency".to_string(),
            urgency.to_string(),
            "--icon".to_string(),
            icon.to_string(),
            "--category".to_string(),
            category.to_string(),
            "--expire-time".to_string(),
            expire_ms.to_string(),
            "--print-id".to_string(),
        ];

        {
            let ids = self.ids.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(&id) = ids.get(&key) {
                args.push("--replace-id".to_string());
                args.push(id.to_string());
            }
        }

        if url.is_some() {
            args.push("--action".to_string());
            args.push("open=Open".to_string());
        }

        args.push(title.to_string());
        args.push(body.to_string());

        let url_owned = url.map(str::to_string);
        let ids = Arc::clone(&self.ids);

        tokio::spawn(async move {
            let output = match tokio::process::Command::new("notify-send")
                .args(&args)
                .output()
                .await
            {
                Ok(o) => o,
                Err(e) => {
                    tracing::warn!("Failed to run notify-send: {e}");
                    return;
                }
            };

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                tracing::warn!("notify-send failed: {stderr}");
                return;
            }

            let stdout = String::from_utf8_lossy(&output.stdout);
            let mut lines = stdout.lines();

            // First line: notification ID from --print-id
            if let Some(id_str) = lines.next()
                && let Ok(id) = id_str.trim().parse::<u32>()
            {
                ids.lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(key, id);
            }

            // Second line: action name from --action (only present if user clicked)
            if let Some(action) = lines.next()
                && action.trim() == "open"
                && let Some(url) = url_owned
                && let Err(e) = tokio::process::Command::new("xdg-open").arg(&url).spawn()
            {
                tracing::warn!("Failed to spawn xdg-open: {e}");
            }
        });
    }
}

/// Extracts the repo name from a group key of the form `owner/repo#branch`.
/// Falls back to the full key if the expected format isn't present.
fn repo_name_from_group(group: &str) -> &str {
    group
        .split_once('/')
        .map(|(_, rest)| rest.split_once('#').map_or(rest, |(repo, _)| repo))
        .unwrap_or(group)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_name_from_full_group_key() {
        assert_eq!(repo_name_from_group("alice/myapp#main"), "myapp");
    }

    #[test]
    fn repo_name_without_branch() {
        assert_eq!(repo_name_from_group("alice/myapp"), "myapp");
    }

    #[test]
    fn repo_name_fallback_no_slash() {
        assert_eq!(repo_name_from_group("build-watcher"), "build-watcher");
    }
}
