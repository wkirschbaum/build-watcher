use std::process::Command;

use crate::config::NotificationLevel;
use crate::platform::Notifier;

/// macOS desktop notifications via `osascript` (AppleScript).
///
/// Fallback when `terminal-notifier` is not installed.
/// URL and group are not supported by AppleScript notifications.
pub struct AppleScriptNotifier;

impl Notifier for AppleScriptNotifier {
    fn name(&self) -> &'static str {
        "osascript"
    }

    fn send(
        &self,
        title: &str,
        body: &str,
        level: NotificationLevel,
        _url: Option<&str>,
        _group: Option<&str>,
    ) {
        let sound = super::notification_sound(level);
        let title = escape_applescript(title);
        let body = escape_applescript(body);
        let script =
            format!(r#"display notification "{body}" with title "{title}" sound name "{sound}""#);
        match Command::new("osascript").args(["-e", &script]).spawn() {
            Ok(mut child) => {
                std::thread::spawn(move || {
                    let _ = child.wait();
                });
            }
            Err(e) => tracing::warn!("Failed to spawn osascript: {e}"),
        }
    }
}

/// Escapes a string for safe embedding in an AppleScript string literal.
fn escape_applescript(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', " ")
        .replace('\r', "")
}
