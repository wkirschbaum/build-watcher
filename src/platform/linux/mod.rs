use crate::platform::Notifier;

mod notify_send;
pub use notify_send::NotifySend;

pub fn default_state_dir() -> String {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    format!("{home}/.local/state/build-watcher")
}

pub fn default_config_dir() -> String {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    format!("{home}/.config/build-watcher")
}

pub fn detect() -> Box<dyn Notifier> {
    Box::new(NotifySend)
}
