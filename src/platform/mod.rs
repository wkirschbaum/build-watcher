#[cfg(target_os = "macos")]
use std::process::Command;
use std::sync::OnceLock;

use crate::config::NotificationLevel;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
use macos as imp;

#[cfg(not(target_os = "macos"))]
mod linux;
#[cfg(not(target_os = "macos"))]
use linux as imp;

pub trait Notifier: Send + Sync {
    fn name(&self) -> &'static str;
    fn send(&self, title: &str, body: &str, level: NotificationLevel, url: Option<&str>);
}

fn notifier() -> &'static Box<dyn Notifier> {
    static INSTANCE: OnceLock<Box<dyn Notifier>> = OnceLock::new();
    INSTANCE.get_or_init(|| {
        let n = imp::detect();
        tracing::info!("Using notification backend: {}", n.name());
        n
    })
}

pub fn send_notification(title: &str, body: &str, level: NotificationLevel, url: Option<&str>) {
    if level == NotificationLevel::Off {
        return;
    }
    notifier().send(title, body, level, url);
}

/// Default state directory when STATE_DIRECTORY is not set.
pub fn default_state_dir() -> String {
    imp::default_state_dir()
}

/// Default config directory when CONFIGURATION_DIRECTORY is not set.
pub fn default_config_dir() -> String {
    imp::default_config_dir()
}

/// Check if a command exists on PATH.
#[cfg(target_os = "macos")]
fn has_command(cmd: &str) -> bool {
    Command::new("which")
        .arg(cmd)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}
