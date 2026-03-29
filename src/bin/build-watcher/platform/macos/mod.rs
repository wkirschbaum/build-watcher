use std::future::Future;
use std::pin::Pin;
use std::process::{Command, Stdio};

use crate::platform::{Notification, Notifier};
use build_watcher::config::NotificationLevel;

// -- Shared helpers --

fn notification_sound(level: NotificationLevel) -> &'static str {
    if level == NotificationLevel::Critical {
        "Basso"
    } else {
        "Glass"
    }
}

/// Reaps a child process in a background thread with a 10-second timeout.
fn reap_with_timeout(mut child: std::process::Child, name: &'static str) {
    std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if std::time::Instant::now() >= deadline => {
                    let _ = child.kill();
                    let _ = child.wait();
                    tracing::warn!("{name} timed out, killed");
                    break;
                }
                Ok(None) => std::thread::sleep(std::time::Duration::from_millis(200)),
                Err(_) => break,
            }
        }
    });
}

fn escape_applescript(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', " ")
        .replace('\r', "")
}

// -- terminal-notifier backend (preferred) --

struct TerminalNotifier;

impl TerminalNotifier {
    fn is_available() -> bool {
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

    fn send(&self, n: &Notification) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        let sound = notification_sound(n.level);
        let mut cmd = Command::new("terminal-notifier");
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .args([
                "-title", &n.title, "-message", &n.body, "-sound", sound, "-group", &n.group,
            ]);
        if let Some(url) = &n.url {
            cmd.args(["-open", url]);
        }
        match cmd.spawn() {
            Ok(child) => reap_with_timeout(child, "terminal-notifier"),
            Err(e) => tracing::warn!("Failed to spawn terminal-notifier: {e}"),
        }
        Box::pin(async {})
    }
}

// -- osascript fallback --

/// Fallback when `terminal-notifier` is not installed.
/// URL and group are not supported by AppleScript notifications.
struct AppleScriptNotifier;

impl Notifier for AppleScriptNotifier {
    fn name(&self) -> &'static str {
        "osascript"
    }

    fn send(&self, n: &Notification) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        let sound = notification_sound(n.level);
        let title = escape_applescript(&n.title);
        let body = escape_applescript(&n.body);
        let script =
            format!(r#"display notification "{body}" with title "{title}" sound name "{sound}""#);
        match Command::new("osascript")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .args(["-e", &script])
            .spawn()
        {
            Ok(child) => reap_with_timeout(child, "osascript"),
            Err(e) => tracing::warn!("Failed to spawn osascript: {e}"),
        }
        Box::pin(async {})
    }
}

// -- Platform API --

pub async fn detect() -> Box<dyn Notifier> {
    if TerminalNotifier::is_available() {
        Box::new(TerminalNotifier)
    } else {
        Box::new(AppleScriptNotifier)
    }
}
