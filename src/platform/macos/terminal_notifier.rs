use std::process::{Command, Stdio};

use crate::config::NotificationLevel;
use crate::platform::Notifier;

/// macOS desktop notifications via `terminal-notifier`.
///
/// Preferred over AppleScript when available: supports URL open and notification grouping.
pub struct TerminalNotifier;

impl TerminalNotifier {
    pub fn is_available() -> bool {
        // Try executing the binary — this verifies both existence and executability,
        // and works correctly when PATH differs from the login shell (e.g. in a daemon).
        Command::new("terminal-notifier")
            .arg("-help")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok()
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
        match cmd.spawn() {
            Ok(child) => super::reap_with_timeout(child, "terminal-notifier"),
            Err(e) => tracing::warn!("Failed to spawn terminal-notifier: {e}"),
        }
    }
}
