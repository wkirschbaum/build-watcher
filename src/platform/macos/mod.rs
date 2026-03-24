use crate::platform::Notifier;

mod apple_script;
mod terminal_notifier;

pub use apple_script::AppleScriptNotifier;
pub use terminal_notifier::TerminalNotifier;

pub fn default_state_dir() -> String {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    format!("{home}/Library/Application Support/build-watcher/state")
}

pub fn default_config_dir() -> String {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    format!("{home}/Library/Application Support/build-watcher/config")
}

pub fn detect() -> Box<dyn Notifier> {
    if TerminalNotifier::is_available() {
        Box::new(TerminalNotifier)
    } else {
        Box::new(AppleScriptNotifier)
    }
}
