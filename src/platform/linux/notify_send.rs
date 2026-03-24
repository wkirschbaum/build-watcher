use std::collections::HashMap;
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncBufReadExt, BufReader};

use crate::config::NotificationLevel;
use crate::platform::Notifier;

/// Linux desktop notifications via `notify-send`.
///
/// Uses `--print-id` / `--replace-id` to stack notifications per group (`owner/repo#branch`),
/// so each branch has its own notification slot. Requires notify-send ≥ 0.8 (libnotify).
///
/// When a URL is provided, adds an `--action open=Open` button and embeds a clickable
/// link in the body. The notification ID is read from the first line of stdout immediately
/// after display (before `--wait` blocks), ensuring the replace-id is available for the
/// next notification without waiting for the user to dismiss the current one.
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

        let key = group.unwrap_or("build-watcher").to_string();
        let app_name = format!("Github Actions [{}]", repo_name_from_group(&key));

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

        // Embed a clickable link in the body — libnotify supports <a href> markup.
        let display_body = match url {
            Some(u) => {
                let run_id = u.rsplit('/').next().unwrap_or(u);
                format!("{body}\n<a href=\"{u}\">#{run_id}</a>")
            }
            None => body.to_string(),
        };
        args.push(display_body);

        let url_owned = url.map(str::to_string);
        let ids = Arc::clone(&self.ids);

        tokio::spawn(async move {
            let mut child = match tokio::process::Command::new("notify-send")
                .args(&args)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
            {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!("Failed to spawn notify-send: {e}");
                    return;
                }
            };

            let mut lines = BufReader::new(child.stdout.take().expect("stdout is piped")).lines();

            // First line: notification ID, printed immediately on display before --wait blocks.
            // Store it right away so the next notification can use --replace-id.
            match lines.next_line().await {
                Ok(Some(line)) => {
                    if let Ok(id) = line.trim().parse::<u32>() {
                        ids.lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .insert(key, id);
                    }
                }
                Ok(None) => {}
                Err(e) => tracing::warn!("Failed to read notify-send output: {e}"),
            }

            // Second line: action name, only written when the user clicks a button.
            match lines.next_line().await {
                Ok(Some(action)) => {
                    if action.trim() == "open"
                        && let Some(url) = url_owned
                        && let Err(e) = tokio::process::Command::new("xdg-open").arg(&url).spawn()
                    {
                        tracing::warn!("Failed to spawn xdg-open: {e}");
                    }
                }
                Ok(None) => {}
                Err(e) => tracing::warn!("Failed to read notify-send action: {e}"),
            }

            if let Err(e) = child.wait().await {
                tracing::warn!("notify-send exited with error: {e}");
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
