use crate::config::NotificationLevel;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
use macos as imp;

#[cfg(not(target_os = "macos"))]
mod linux;
#[cfg(not(target_os = "macos"))]
use linux as imp;

pub fn send_notification(title: &str, body: &str, level: NotificationLevel, url: Option<&str>) {
    if level == NotificationLevel::Off {
        return;
    }

    imp::send_notification(title, body, level, url);
}

/// Default state directory when STATE_DIRECTORY is not set.
pub fn default_state_dir() -> String {
    imp::default_state_dir()
}

/// Default config directory when CONFIGURATION_DIRECTORY is not set.
pub fn default_config_dir() -> String {
    imp::default_config_dir()
}
