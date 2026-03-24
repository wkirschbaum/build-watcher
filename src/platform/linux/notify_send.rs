use std::process::Command;

use crate::config::NotificationLevel;
use crate::platform::Notifier;

/// Linux desktop notifications via `notify-send`.
pub struct NotifySend;

impl Notifier for NotifySend {
    fn name(&self) -> &'static str {
        "notify-send"
    }

    fn send(&self, title: &str, body: &str, level: NotificationLevel, url: Option<&str>) {
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
        let _ = Command::new("notify-send")
            .args([
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
                title,
                &notification_body,
            ])
            .spawn();
    }
}
