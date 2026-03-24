use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::config::NotificationLevel;
use crate::platform::Notifier;

/// Linux desktop notifications via `notify-send`.
///
/// Uses `--print-id` / `--replace-id` to stack notifications per group (project),
/// so each watched repo has its own notification slot.
///
/// When a URL is provided, adds an `--action open=Open` button. Clicking it opens
/// the URL via `xdg-open`. Requires notify-send ≥ 0.8 (libnotify).
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
            NotificationLevel::Off => unreachable!(),
        };
        let urgency = match level {
            NotificationLevel::Low => "low",
            NotificationLevel::Normal => "normal",
            NotificationLevel::Critical => "critical",
            NotificationLevel::Off => unreachable!(),
        };

        let mut args = vec![
            "--app-name".to_string(),
            "Build Watcher".to_string(),
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

        let key = group.unwrap_or("build-watcher").to_string();
        {
            let ids = self.ids.lock().unwrap();
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
            let Ok(output) = tokio::process::Command::new("notify-send")
                .args(&args)
                .output()
                .await
            else {
                return;
            };

            let stdout = String::from_utf8_lossy(&output.stdout);
            let mut lines = stdout.lines();

            // First line: notification ID from --print-id
            if let Some(id_str) = lines.next()
                && let Ok(id) = id_str.trim().parse::<u32>()
            {
                ids.lock().unwrap().insert(key, id);
            }

            // Second line: action name from --action (only present if user clicked)
            if let Some(action) = lines.next()
                && action.trim() == "open"
                && let Some(url) = url_owned
            {
                let _ = tokio::process::Command::new("xdg-open")
                    .arg(&url)
                    .spawn();
            }
        });
    }
}
