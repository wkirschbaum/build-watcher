use std::future::Future;
use std::pin::Pin;

use tokio::sync::OnceCell;

use build_watcher::config::NotificationLevel;

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

/// Pre-computed notification data. All formatting happens before this is
/// passed to the platform backend — the backend only does the OS dispatch.
pub struct Notification {
    pub title: String,
    pub body: String,
    pub level: NotificationLevel,
    pub url: Option<String>,
    /// Grouping key — notifications with the same group replace each other.
    pub group: String,
    /// Human-readable source identifier shown in the OS notification chrome.
    pub app_name: String,
    /// Run ID to offer a "Rerun" action on failure notifications (Linux D-Bus).
    pub rerun_run_id: Option<u64>,
}

pub trait Notifier: Send + Sync {
    fn name(&self) -> &'static str;
    fn send(&self, n: &Notification) -> Pin<Box<dyn Future<Output = ()> + Send + '_>>;
}

static INSTANCE: OnceCell<Box<dyn Notifier>> = OnceCell::const_new();

async fn notifier() -> &'static dyn Notifier {
    &**INSTANCE
        .get_or_init(|| async {
            let n = imp::detect().await;
            tracing::info!("Using notification backend: {}", n.name());
            n
        })
        .await
}

pub async fn send(n: Notification) {
    if n.level == NotificationLevel::Off {
        return;
    }
    notifier().await.send(&n).await;
}
