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
