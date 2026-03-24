use crate::config::NotificationLevel;
use crate::platform::Notifier;

/// No-op notifier for use in tests.
#[allow(dead_code)]
pub struct NullNotifier;

impl Notifier for NullNotifier {
    fn name(&self) -> &'static str {
        "null"
    }

    fn send(
        &self,
        _title: &str,
        _body: &str,
        _level: NotificationLevel,
        _url: Option<&str>,
        _group: Option<&str>,
    ) {
    }
}
