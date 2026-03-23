use std::process::Command;

use crate::config::NotificationLevel;

pub fn default_state_dir() -> String {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    format!("{home}/.local/state/build-watcher")
}

pub fn default_config_dir() -> String {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    format!("{home}/.config/build-watcher")
}

pub fn send_notification(title: &str, body: &str, level: NotificationLevel, url: Option<&str>) {
    let icon = if level == NotificationLevel::Critical { "dialog-error" } else { "dialog-information" };
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
        .args(["--urgency", urgency, "--icon", icon, title, &notification_body])
        .spawn();
}
