use std::collections::HashMap;
use std::process::Command;
use std::sync::Mutex;

use crate::config::NotificationLevel;
use crate::platform::Notifier;

/// Linux desktop notifications via `notify-send`.
///
/// Uses `--print-id` / `--replace-id` to stack notifications per group (project),
/// so each watched repo has its own notification slot.
pub struct NotifySend {
    ids: Mutex<HashMap<String, u32>>,
}

impl NotifySend {
    pub fn new() -> Self {
        Self {
            ids: Mutex::new(HashMap::new()),
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
        let notification_body = match url {
            Some(u) => format!("{body}\n{u}"),
            None => body.to_string(),
        };

        let mut cmd = Command::new("notify-send");
        cmd.args([
            "--app-name",
            "Build Watcher",
            "--urgency",
            urgency,
            "--icon",
            icon,
            "--category",
            category,
            "--expire-time",
            expire_ms,
            "--print-id",
        ]);

        let key = group.unwrap_or("build-watcher").to_string();
        let mut ids = self.ids.lock().unwrap();
        if let Some(&id) = ids.get(&key) {
            cmd.args(["--replace-id", &id.to_string()]);
        }

        cmd.args([title, &notification_body]);

        if let Ok(output) = cmd.output()
            && let Ok(id) = String::from_utf8_lossy(&output.stdout)
                .trim()
                .parse::<u32>()
        {
            ids.insert(key, id);
        }
    }
}
