use std::process::Command;

use crate::config::NotificationLevel;
use crate::platform::Notifier;

/// macOS desktop notifications via `terminal-notifier`.
///
/// Preferred over AppleScript when available: supports URL open and notification grouping.
pub struct TerminalNotifier;

impl TerminalNotifier {
    pub fn is_available() -> bool {
        std::env::var("PATH")
            .unwrap_or_default()
            .split(':')
            .any(|dir| {
                std::path::Path::new(dir)
                    .join("terminal-notifier")
                    .is_file()
            })
    }
}

impl Notifier for TerminalNotifier {
    fn name(&self) -> &'static str {
        "terminal-notifier"
    }

    fn send(
        &self,
        title: &str,
        body: &str,
        level: NotificationLevel,
        url: Option<&str>,
        group: Option<&str>,
    ) {
        let sound = super::notification_sound(level);
        let mut cmd = Command::new("terminal-notifier");
        cmd.args([
            "-title",
            title,
            "-message",
            body,
            "-sound",
            sound,
            "-group",
            group.unwrap_or("build-watcher"),
        ]);
        if let Some(url) = url {
            cmd.args(["-open", url]);
        }
        if let Err(e) = cmd.spawn() {
            tracing::warn!("Failed to spawn terminal-notifier: {e}");
        }
    }
}
