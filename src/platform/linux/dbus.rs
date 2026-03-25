use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use futures_lite::StreamExt;
use zbus::Connection;
use zbus::proxy;

use crate::config::NotificationLevel;
use crate::platform::Notifier;

use super::{app_name_from_group, notification_props};

/// Proxy for the `org.freedesktop.Notifications` D-Bus interface.
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

/// Linux desktop notifications via D-Bus (`org.freedesktop.Notifications`).
///
/// Uses `replaces_id` to stack notifications per group (`owner/repo#branch`),
/// so each branch has its own notification slot.
///
/// When a URL is provided, clicking the notification opens it via `xdg-open`.
pub struct DbusNotifier {
    connection: Connection,
    ids: Arc<Mutex<HashMap<String, u32>>>,
}

impl DbusNotifier {
    pub async fn new() -> zbus::Result<Self> {
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

    fn send(
        &self,
        title: &str,
        body: &str,
        level: NotificationLevel,
        url: Option<&str>,
        group: Option<&str>,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        let props = notification_props(level);

        let key = group.unwrap_or("build-watcher").to_string();
        let app_name = app_name_from_group(group).to_string();

        let replaces_id = self
            .ids
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&key)
            .copied()
            .unwrap_or(0);

        let title = title.to_string();
        let body = body.to_string();
        let url = url.map(String::from);
        let ids = Arc::clone(&self.ids);
        let icon = props.icon.to_string();
        let urgency = props.urgency;
        let category = props.category.to_string();
        let expire_ms = props.expire_ms;

        Box::pin(async move {
            let proxy = match NotificationsProxy::new(&self.connection).await {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!("Failed to create D-Bus notification proxy: {e}");
                    return;
                }
            };

            let urgency_byte: u8 = match urgency {
                "low" => 0,
                "critical" => 2,
                _ => 1, // normal
            };

            let mut hints = HashMap::new();
            hints.insert("urgency", zbus::zvariant::Value::from(urgency_byte));
            hints.insert("category", zbus::zvariant::Value::from(category.as_str()));
            hints.insert(
                "desktop-entry",
                zbus::zvariant::Value::from("build-watcher"),
            );
            if level == NotificationLevel::Critical {
                hints.insert("resident", zbus::zvariant::Value::from(true));
            }

            // "default" action fires when the notification body is clicked.
            let actions = if url.is_some() {
                vec!["default", "Open"]
            } else {
                vec![]
            };

            match proxy
                .notify(
                    &app_name,
                    replaces_id,
                    &icon,
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
                        .insert(key, id);

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
