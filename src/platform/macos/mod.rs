use std::process::Command;

use crate::config::NotificationLevel;

pub fn default_state_dir() -> String {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    format!("{home}/Library/Application Support/build-watcher/state")
}

pub fn default_config_dir() -> String {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    format!("{home}/Library/Application Support/build-watcher/config")
}

/// Escape a string for use inside AppleScript double quotes.
fn escape_applescript(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

pub fn send_notification(title: &str, body: &str, level: NotificationLevel, url: Option<&str>) {
    let sound = if level == NotificationLevel::Critical { "Basso" } else { "Glass" };
    let title = escape_applescript(title);
    let body = escape_applescript(body);
    let script = if let Some(url) = url {
        let url = escape_applescript(url);
        format!(
            r#"display notification "{body}" with title "{title}" sound name "{sound}"
do shell script "open {url}""#
        )
    } else {
        format!(
            r#"display notification "{body}" with title "{title}" sound name "{sound}""#
        )
    };
    let _ = Command::new("osascript").args(["-e", &script]).spawn();
}
