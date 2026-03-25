use std::future::Future;
use std::pin::Pin;

use crate::platform::{Notification, Notifier};

/// No-op notifier for use in tests.
#[allow(dead_code)]
pub struct NullNotifier;

impl Notifier for NullNotifier {
    fn name(&self) -> &'static str {
        "null"
    }

    fn send(&self, _n: &Notification) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async {})
    }
}
