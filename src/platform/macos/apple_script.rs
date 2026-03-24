use std::process::Command;

use crate::config::NotificationLevel;
use crate::platform::Notifier;

/// macOS desktop notifications via `osascript` (AppleScript).
///
/// Fallback when `terminal-notifier` is not installed.
pub struct AppleScriptNotifier;

impl Notifier for AppleScriptNotifier {
    fn name(&self) -> &'static str {
        "osascript"
    }

    fn send(&self, title: &str, body: &str, level: NotificationLevel, _url: Option<&str>) {
        let sound = if level == NotificationLevel::Critical {
            "Basso"
        } else {
            "Glass"
        };
        let title = title.replace('\\', "\\\\").replace('"', "\\\"");
        let body = body.replace('\\', "\\\\").replace('"', "\\\"");
        let script =
            format!(r#"display notification "{body}" with title "{title}" sound name "{sound}""#);
        let _ = Command::new("osascript").args(["-e", &script]).spawn();
    }
}
