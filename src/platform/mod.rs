use std::future::Future;
use std::pin::Pin;
use std::sync::OnceLock;

use crate::config::NotificationLevel;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
use linux as imp;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
use macos as imp;

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
compile_error!("Unsupported platform: only Linux and macOS are supported");

#[cfg(test)]
mod universal;
#[cfg(test)]
#[allow(unused_imports)]
pub use universal::NullNotifier;

pub trait Notifier: Send + Sync {
    fn name(&self) -> &'static str;
    fn send(
        &self,
        title: &str,
        body: &str,
        level: NotificationLevel,
        url: Option<&str>,
        group: Option<&str>,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + '_>>;

    /// Play a sound file. `path` is an optional custom path; None means use system default.
    fn play_sound(&self, _path: Option<&str>) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async {})
    }
}

static INSTANCE: OnceLock<Box<dyn Notifier>> = OnceLock::new();

/// Override the notifier backend for testing. Must be called before any
/// notification is sent. Subsequent calls are silently ignored (OnceLock semantics).
#[cfg(test)]
#[allow(dead_code)]
pub fn init(notifier: Box<dyn Notifier>) {
    INSTANCE.set(notifier).ok();
}

pub fn notifier() -> &'static dyn Notifier {
    &**INSTANCE.get_or_init(|| {
        let n = imp::detect();
        tracing::info!("Using notification backend: {}", n.name());
        n
    })
}

pub async fn send_notification(
    title: &str,
    body: &str,
    level: NotificationLevel,
    url: Option<&str>,
    group: Option<&str>,
) {
    if level == NotificationLevel::Off {
        return;
    }
    notifier().send(title, body, level, url, group).await;
}

pub async fn play_sound(path: Option<&str>) {
    notifier().play_sound(path).await;
}

/// Default state directory when `STATE_DIRECTORY` is not set.
pub fn default_state_dir() -> String {
    imp::default_state_dir()
}

/// Default config directory when `CONFIGURATION_DIRECTORY` is not set.
pub fn default_config_dir() -> String {
    imp::default_config_dir()
}

/// Returns the user's home directory, falling back to `/tmp` with a warning if `HOME` is unset.
pub(super) fn home_dir() -> String {
    std::env::var("HOME").unwrap_or_else(|_| {
        tracing::warn!("HOME is not set; falling back to /tmp for state/config directories");
        "/tmp".to_string()
    })
}
