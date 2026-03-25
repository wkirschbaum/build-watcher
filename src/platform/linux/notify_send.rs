use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncBufReadExt, BufReader};

use crate::config::NotificationLevel;
use crate::platform::Notifier;

use super::{app_name_from_group, notification_props};

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
    ) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        let props = notification_props(level);

        let key = group.unwrap_or("build-watcher").to_string();
        let app_name = app_name_from_group(group).to_string();

        let mut args = vec![
            "--app-name".to_string(),
            app_name,
            "--urgency".to_string(),
            props.urgency.to_string(),
            "--icon".to_string(),
            props.icon.to_string(),
            "--category".to_string(),
            props.category.to_string(),
            "--expire-time".to_string(),
            props.expire_ms.to_string(),
            "--hint".to_string(),
            "string:desktop-entry:build-watcher".to_string(),
            "--print-id".to_string(),
        ];

        {
            let ids = self
                .ids
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
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

        args.push(body.to_string());

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
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
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
