use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
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
/// link in the body. The notification ID is read from stdout and stored before returning,
/// ensuring the replace-id is available for the next notification.
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

const DEFAULT_ERROR_SOUND: &str = "/usr/share/sounds/freedesktop/stereo/dialog-error.oga";

impl Notifier for NotifySend {
    fn name(&self) -> &'static str {
        "notify-send"
    }

    fn play_sound(&self, path: Option<&str>) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        let path = path.unwrap_or(DEFAULT_ERROR_SOUND).to_string();
        Box::pin(async move {
            // Try paplay (PulseAudio/PipeWire) first, fall back to aplay (ALSA)
            let result = tokio::process::Command::new("paplay")
                .arg(&path)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .await;
            if result.is_err() || !result.unwrap().success() {
                let _ = tokio::process::Command::new("aplay")
                    .arg(&path)
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .await;
            }
        })
    }

    fn send(
        &self,
        title: &str,
        body: &str,
        level: NotificationLevel,
        url: Option<&str>,
        group: Option<&str>,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
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

        let has_url = url.is_some();
        if has_url {
            args.push("--wait".to_string());
            args.push("--action".to_string());
            args.push("open=Open".to_string());
        }

        args.push(title.to_string());

        // Append the plain URL on a second line — most notification daemons auto-link it.
        let display_body = match url {
            Some(u) => format!("{body}\n{u}"),
            None => body.to_string(),
        };
        args.push(display_body);

        let url_owned = url.map(str::to_string);
        let ids = Arc::clone(&self.ids);

        Box::pin(async move {
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

            // Read the notification ID before returning — this ensures --replace-id is
            // available for the next call, even when multiple notifications fire in rapid
            // succession (e.g. several builds completing on the same poll cycle).
            match lines.next_line().await {
                Ok(Some(line)) => {
                    if let Ok(id) = line.trim().parse::<u32>() {
                        ids.lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .insert(key, id);
                    } else {
                        tracing::debug!("notify-send returned non-numeric ID: {line:?}");
                    }
                }
                Ok(None) => {}
                Err(e) => tracing::warn!("Failed to read notify-send output: {e}"),
            }

            // The --wait / action handling runs in the background so we don't block the caller.
            if has_url {
                tokio::spawn(async move {
                    // Second line: action name, only written when the user clicks a button.
                    match lines.next_line().await {
                        Ok(Some(action)) => {
                            if action.trim() == "open"
                                && let Some(url) = url_owned
                            {
                                match tokio::process::Command::new("xdg-open").arg(&url).spawn() {
                                    Ok(mut child) => {
                                        if let Err(e) = child.wait().await {
                                            tracing::warn!("xdg-open failed: {e}");
                                        }
                                    }
                                    Err(e) => tracing::warn!("Failed to spawn xdg-open: {e}"),
                                }
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
        })
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
