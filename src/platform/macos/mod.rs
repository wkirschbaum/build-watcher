use std::process::Command;

use crate::config::NotificationLevel;
use super::{Notifier, has_command};

pub fn default_state_dir() -> String {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    format!("{home}/Library/Application Support/build-watcher/state")
}

pub fn default_config_dir() -> String {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    format!("{home}/Library/Application Support/build-watcher/config")
}

pub fn detect() -> Box<dyn Notifier> {
    if has_command("terminal-notifier") {
        Box::new(TerminalNotifier)
    } else {
        Box::new(AppleScriptNotifier)
    }
}

struct TerminalNotifier;

impl Notifier for TerminalNotifier {
    fn name(&self) -> &'static str { "terminal-notifier" }

    fn send(&self, title: &str, body: &str, level: NotificationLevel, url: Option<&str>) {
        let sound = if level == NotificationLevel::Critical { "Basso" } else { "Glass" };
        let mut cmd = Command::new("terminal-notifier");
        cmd.args(["-title", title, "-message", body, "-sound", sound, "-group", "build-watcher"]);
        if let Some(url) = url {
            cmd.args(["-open", url]);
        }
        let _ = cmd.spawn();
    }
}

struct AppleScriptNotifier;

impl Notifier for AppleScriptNotifier {
    fn name(&self) -> &'static str { "osascript" }

    fn send(&self, title: &str, body: &str, level: NotificationLevel, _url: Option<&str>) {
        let sound = if level == NotificationLevel::Critical { "Basso" } else { "Glass" };
        let title = title.replace('\\', "\\\\").replace('"', "\\\"");
        let body = body.replace('\\', "\\\\").replace('"', "\\\"");
        let script = format!(
            r#"display notification "{body}" with title "{title}" sound name "{sound}""#
        );
        let _ = Command::new("osascript").args(["-e", &script]).spawn();
    }
}
