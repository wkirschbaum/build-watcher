use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use notify_rust::{Hint, Notification, Urgency};

use crate::config::NotificationLevel;
use crate::platform::Notifier;

use super::{app_name_from_group, format_body, notification_props, play_sound_impl};

/// Linux desktop notifications via D-Bus using `notify-rust`.
///
/// Preferred over the `notify-send` CLI backend: no process spawning, type-safe hints,
/// and proper `desktop-entry` support for GNOME/KDE notification grouping.
///
/// The `app_name` is set to the repo name (extracted from the group key) so
/// notifications are grouped per project in the desktop notification drawer.
pub struct DbusNotifier {
    ids: Arc<Mutex<HashMap<String, u32>>>,
}

impl DbusNotifier {
    /// Create a new D-Bus notifier. Probes the session bus to verify it's available.
    pub fn new() -> Result<Self, notify_rust::error::Error> {
        // Probe the D-Bus session bus without sending a visible notification.
        notify_rust::get_server_information()?;
        Ok(Self {
            ids: Arc::new(Mutex::new(HashMap::new())),
        })
    }
}

impl Notifier for DbusNotifier {
    fn name(&self) -> &'static str {
        "dbus"
    }

    fn play_sound(&self, path: Option<&str>) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        play_sound_impl(path)
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
        let dbus_urgency = match level {
            NotificationLevel::Low => Urgency::Low,
            NotificationLevel::Normal => Urgency::Normal,
            NotificationLevel::Critical => Urgency::Critical,
            NotificationLevel::Off => unreachable!("Off is filtered before send()"),
        };

        let key = group.unwrap_or("build-watcher").to_string();
        let app_name = app_name_from_group(group).to_string();
        let display_body = format_body(body);

        let replace_id = {
            let ids = self
                .ids
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            ids.get(&key).copied()
        };

        let url_owned = url.map(str::to_string);
        let title = title.to_string();
        let ids = Arc::clone(&self.ids);

        Box::pin(async move {
            let mut notification = Notification::new();
            notification
                .appname(&app_name)
                .summary(&title)
                .body(&display_body)
                .icon(props.icon)
                .urgency(dbus_urgency)
                .timeout(props.expire_ms)
                .hint(Hint::Category(props.category.to_string()))
                .hint(Hint::Custom(
                    "desktop-entry".to_string(),
                    "build-watcher".to_string(),
                ));

            if let Some(id) = replace_id {
                notification.id(id);
            }

            if level == NotificationLevel::Critical {
                notification.hint(Hint::Resident(true));
            }

            if url_owned.is_some() {
                notification.action("open", "Open");
            }

            // notify-rust's show() is sync (blocking D-Bus call), so run on a blocking thread.
            let result = tokio::task::spawn_blocking(move || notification.show()).await;

            match result {
                Ok(Ok(handle)) => {
                    let new_id = handle.id();
                    ids.lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .insert(key, new_id);

                    // Handle action clicks in background
                    if let Some(url) = url_owned {
                        tokio::task::spawn_blocking(move || {
                            handle.wait_for_action(|action| {
                                if action == "open" {
                                    let _ = std::process::Command::new("xdg-open")
                                        .arg(&url)
                                        .stdout(Stdio::null())
                                        .stderr(Stdio::null())
                                        .spawn();
                                }
                            });
                        });
                    }
                }
                Ok(Err(e)) => tracing::warn!("D-Bus notification failed: {e}"),
                Err(e) => tracing::warn!("Blocking task panicked: {e}"),
            }
        })
    }
}
