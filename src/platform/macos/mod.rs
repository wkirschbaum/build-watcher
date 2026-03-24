use crate::config::NotificationLevel;
use crate::platform::Notifier;

mod apple_script;
mod terminal_notifier;

pub use apple_script::AppleScriptNotifier;
pub use terminal_notifier::TerminalNotifier;

pub fn default_state_dir() -> String {
    let home = super::home_dir();
    format!("{home}/Library/Application Support/build-watcher/state")
}

pub fn default_config_dir() -> String {
    let home = super::home_dir();
    format!("{home}/Library/Application Support/build-watcher/config")
}

pub fn detect() -> Box<dyn Notifier> {
    if TerminalNotifier::is_available() {
        Box::new(TerminalNotifier)
    } else {
        Box::new(AppleScriptNotifier)
    }
}

/// Maps a notification level to the appropriate macOS alert sound.
pub(super) fn notification_sound(level: NotificationLevel) -> &'static str {
    if level == NotificationLevel::Critical {
        "Basso"
    } else {
        "Glass"
    }
}

/// Reaps a child process in a background thread with a 10-second timeout.
/// Prevents zombie processes from accumulating when Notifier::send is synchronous.
pub(super) fn reap_with_timeout(mut child: std::process::Child, name: &'static str) {
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
