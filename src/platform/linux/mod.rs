use crate::config::NotificationLevel;
use crate::platform::Notifier;

mod dbus;
mod notify_send;

/// Shared notification properties derived from a `NotificationLevel`.
#[derive(Debug, PartialEq)]
pub(super) struct NotificationProps {
    pub icon: &'static str,
    pub category: &'static str,
    pub expire_ms: i32,
    pub urgency: &'static str,
}

pub(super) fn notification_props(level: NotificationLevel) -> NotificationProps {
    match level {
        NotificationLevel::Low => NotificationProps {
            icon: "emblem-synchronizing",
            category: "transfer",
            expire_ms: 4000,
            urgency: "low",
        },
        NotificationLevel::Normal => NotificationProps {
            icon: "emblem-ok",
            category: "transfer.complete",
            expire_ms: 6000,
            urgency: "normal",
        },
        NotificationLevel::Critical => NotificationProps {
            icon: "dialog-error",
            category: "transfer.error",
            expire_ms: 0,
            urgency: "critical",
        },
        NotificationLevel::Off => unreachable!("Off is filtered before send()"),
    }
}

/// Extract the repo name from a notification group key (`owner/repo#branch#workflow`).
/// Falls back to "Build Watcher" when no group is provided.
pub(super) fn app_name_from_group(group: Option<&str>) -> &str {
    group
        .and_then(|g| g.split('#').next())
        .unwrap_or("Build Watcher")
}

pub fn default_state_dir() -> String {
    let home = super::home_dir();
    format!("{home}/.local/state/build-watcher")
}

pub fn default_config_dir() -> String {
    let home = super::home_dir();
    format!("{home}/.config/build-watcher")
}

pub fn detect() -> Box<dyn Notifier> {
    match dbus::DbusNotifier::new() {
        Ok(n) => Box::new(n),
        Err(e) => {
            tracing::warn!("D-Bus notifications unavailable ({e}), falling back to notify-send");
            Box::new(notify_send::NotifySend::new())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notification_props_low() {
        let props = notification_props(NotificationLevel::Low);
        assert_eq!(props.icon, "emblem-synchronizing");
        assert_eq!(props.category, "transfer");
        assert_eq!(props.expire_ms, 4000);
        assert_eq!(props.urgency, "low");
    }

    #[test]
    fn notification_props_normal() {
        let props = notification_props(NotificationLevel::Normal);
        assert_eq!(props.icon, "emblem-ok");
        assert_eq!(props.category, "transfer.complete");
        assert_eq!(props.expire_ms, 6000);
        assert_eq!(props.urgency, "normal");
    }

    #[test]
    fn notification_props_critical() {
        let props = notification_props(NotificationLevel::Critical);
        assert_eq!(props.icon, "dialog-error");
        assert_eq!(props.category, "transfer.error");
        assert_eq!(props.expire_ms, 0);
        assert_eq!(props.urgency, "critical");
    }

    #[test]
    fn app_name_from_group_extracts_repo() {
        assert_eq!(app_name_from_group(Some("alice/app#main#CI")), "alice/app");
    }

    #[test]
    fn app_name_from_group_none_falls_back() {
        assert_eq!(app_name_from_group(None), "Build Watcher");
    }

    #[test]
    fn app_name_from_group_no_hash_returns_whole_string() {
        assert_eq!(app_name_from_group(Some("build-watcher")), "build-watcher");
    }

    #[test]
    fn detect_returns_a_notifier() {
        // On CI without D-Bus this falls back to notify-send; either way we get a valid backend.
        let notifier = detect();
        assert!(notifier.name() == "dbus" || notifier.name() == "notify-send");
    }
}
