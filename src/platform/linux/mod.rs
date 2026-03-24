use crate::platform::Notifier;

mod notify_send;
pub use notify_send::NotifySend;

pub fn default_state_dir() -> String {
    let home = super::home_dir();
    format!("{home}/.local/state/build-watcher")
}

pub fn default_config_dir() -> String {
    let home = super::home_dir();
    format!("{home}/.config/build-watcher")
}

pub fn detect() -> Box<dyn Notifier> {
    Box::new(NotifySend::new())
}
