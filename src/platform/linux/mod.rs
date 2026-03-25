// D-Bus Notify method has 8 params per spec; the zbus #[proxy] macro generates
// a wrapper with one extra (&self), triggering too_many_arguments on the expansion.
#![allow(clippy::too_many_arguments)]

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use futures_lite::StreamExt;
use zbus::Connection;
use zbus::proxy;

use crate::config::NotificationLevel;
use crate::platform::{Notification, Notifier};

// -- D-Bus interface proxy --

#[proxy(
    interface = "org.freedesktop.Notifications",
    default_service = "org.freedesktop.Notifications",
    default_path = "/org/freedesktop/Notifications"
)]
trait Notifications {
    fn notify(
        &self,
        app_name: &str,
        replaces_id: u32,
        app_icon: &str,
        summary: &str,
        body: &str,
        actions: Vec<&str>,
        hints: HashMap<&str, zbus::zvariant::Value<'_>>,
        expire_timeout: i32,
    ) -> zbus::Result<u32>;

    #[zbus(signal)]
    fn action_invoked(&self, id: u32, action_key: &str) -> zbus::Result<()>;
}

// -- Level → D-Bus property mapping --

struct DbusProps {
    icon: &'static str,
    category: &'static str,
    expire_ms: i32,
    /// D-Bus urgency hint: 0 = low, 1 = normal, 2 = critical.
    urgency: u8,
}

fn dbus_props(level: NotificationLevel) -> DbusProps {
    match level {
        NotificationLevel::Low => DbusProps {
            icon: "emblem-synchronizing",
            category: "transfer",
            expire_ms: 4000,
            urgency: 0,
        },
        NotificationLevel::Normal => DbusProps {
            icon: "emblem-ok",
            category: "transfer.complete",
            expire_ms: 6000,
            urgency: 1,
        },
        NotificationLevel::Critical => DbusProps {
            icon: "dialog-error",
            category: "transfer.error",
            expire_ms: 0,
            urgency: 2,
        },
        NotificationLevel::Off => unreachable!("Off is filtered before send()"),
    }
}

// -- D-Bus notifier --

/// Linux desktop notifications via D-Bus (`org.freedesktop.Notifications`).
///
/// Uses `replaces_id` to stack notifications per group, so each
/// repo/branch/workflow has its own notification slot.
///
/// When a URL is provided, clicking the notification opens it via `xdg-open`.
struct DbusNotifier {
    connection: Connection,
    ids: Arc<Mutex<HashMap<String, u32>>>,
}

impl DbusNotifier {
    async fn new() -> zbus::Result<Self> {
        let connection = Connection::session().await?;
        Ok(Self {
            connection,
            ids: Arc::new(Mutex::new(HashMap::new())),
        })
    }
}

impl Notifier for DbusNotifier {
    fn name(&self) -> &'static str {
        "dbus"
    }

    fn send(&self, n: &Notification) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        let props = dbus_props(n.level);

        let replaces_id = self
            .ids
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&n.group)
            .copied()
            .unwrap_or(0);

        let title = n.title.clone();
        let body = n.body.clone();
        let url = n.url.clone();
        let group = n.group.clone();
        let app_name = n.app_name.clone();
        let ids = Arc::clone(&self.ids);
        let icon = props.icon;
        let category = props.category;
        let expire_ms = props.expire_ms;
        let urgency = props.urgency;
        let level = n.level;

        Box::pin(async move {
            let proxy = match NotificationsProxy::new(&self.connection).await {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!("Failed to create D-Bus notification proxy: {e}");
                    return;
                }
            };

            let mut hints = HashMap::new();
            hints.insert("urgency", zbus::zvariant::Value::from(urgency));
            hints.insert("category", zbus::zvariant::Value::from(category));
            hints.insert(
                "desktop-entry",
                zbus::zvariant::Value::from("build-watcher"),
            );
            if level == NotificationLevel::Critical {
                hints.insert("resident", zbus::zvariant::Value::from(true));
            }

            let actions = if url.is_some() {
                vec!["default", "Open"]
            } else {
                vec![]
            };

            match proxy
                .notify(
                    &app_name,
                    replaces_id,
                    icon,
                    &title,
                    &body,
                    actions,
                    hints,
                    expire_ms,
                )
                .await
            {
                Ok(id) => {
                    ids.lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .insert(group, id);

                    if let Some(url) = url {
                        spawn_action_listener(proxy, id, url);
                    }
                }
                Err(e) => {
                    tracing::warn!("D-Bus notification failed: {e}");
                }
            }
        })
    }
}

/// Spawn a background task that waits for the user to click the notification,
/// then opens the URL via `xdg-open`. Times out after 10 minutes.
fn spawn_action_listener(proxy: NotificationsProxy<'static>, notification_id: u32, url: String) {
    tokio::spawn(async move {
        let mut stream = match proxy.receive_action_invoked().await {
            Ok(s) => s,
            Err(e) => {
                tracing::debug!("Failed to subscribe to ActionInvoked signal: {e}");
                return;
            }
        };

        let timeout = tokio::time::sleep(std::time::Duration::from_secs(600));
        tokio::pin!(timeout);

        loop {
            tokio::select! {
                signal = stream.next() => {
                    let Some(signal) = signal else { break };
                    let Ok(args) = signal.args() else { continue };
                    if args.id == notification_id && args.action_key == "default" {
                        if let Err(e) = tokio::process::Command::new("xdg-open")
                            .arg(&url)
                            .stdin(std::process::Stdio::null())
                            .stdout(std::process::Stdio::null())
                            .stderr(std::process::Stdio::null())
                            .spawn()
                        {
                            tracing::warn!("Failed to open URL: {e}");
                        }
                        break;
                    }
                }
                () = &mut timeout => break,
            }
        }
    });
}

// -- Platform API --

pub fn default_state_dir() -> String {
    let home = super::home_dir();
    format!("{home}/.local/state/build-watcher")
}

pub fn default_config_dir() -> String {
    let home = super::home_dir();
    format!("{home}/.config/build-watcher")
}

pub async fn detect() -> Box<dyn Notifier> {
    match DbusNotifier::new().await {
        Ok(n) => Box::new(n),
        Err(e) => {
            panic!("D-Bus session bus unavailable: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dbus_props_by_level() {
        let low = dbus_props(NotificationLevel::Low);
        assert_eq!(
            (low.icon, low.urgency, low.expire_ms),
            ("emblem-synchronizing", 0, 4000)
        );

        let normal = dbus_props(NotificationLevel::Normal);
        assert_eq!(
            (normal.icon, normal.urgency, normal.expire_ms),
            ("emblem-ok", 1, 6000)
        );

        let critical = dbus_props(NotificationLevel::Critical);
        assert_eq!(
            (critical.icon, critical.urgency, critical.expire_ms),
            ("dialog-error", 2, 0)
        );
    }
}
