use std::fmt::Write as _;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{Mutex, broadcast};
use tokio::time::Instant;

use crate::github::RunInfo;
use crate::{config, format, platform};

const CHANNEL_CAPACITY: usize = 256;

/// Snapshot of a run's identity, carried by events.
#[derive(Debug, Clone)]
pub struct RunSnapshot {
    pub repo: String,
    pub branch: String,
    pub run_id: u64,
    pub workflow: String,
    pub title: String,
    pub event: String,
}

impl RunSnapshot {
    pub fn from_run_info(run: &RunInfo, repo: &str, branch: &str) -> Self {
        Self {
            repo: repo.to_string(),
            branch: branch.to_string(),
            run_id: run.id,
            workflow: run.workflow.clone(),
            title: run.title.clone(),
            event: run.event.clone(),
        }
    }

    pub fn url(&self) -> String {
        format!(
            "https://github.com/{}/actions/runs/{}",
            self.repo, self.run_id
        )
    }

    pub fn display_title(&self) -> String {
        crate::github::display_title(&self.event, &self.title)
    }

    fn notification_group(&self) -> String {
        format!("{}#{}#{}", self.repo, self.branch, self.workflow)
    }
}

/// Events emitted by the watcher polling loop.
#[derive(Debug, Clone)]
pub enum WatchEvent {
    /// A new build was detected.
    RunStarted(RunSnapshot),

    /// A build completed (success, failure, cancelled, etc.).
    RunCompleted {
        run: RunSnapshot,
        conclusion: String,
        elapsed: Option<Duration>,
        failing_steps: Option<String>,
    },

    /// A build's status changed (e.g. queued -> `in_progress`).
    #[allow(dead_code)] // fields carried for Debug logging
    StatusChanged {
        run: RunSnapshot,
        from: String,
        to: String,
    },
}

/// Broadcast bus for watch events. Cloning shares the same underlying channel.
#[derive(Clone)]
pub struct EventBus {
    tx: broadcast::Sender<WatchEvent>,
}

impl EventBus {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(CHANNEL_CAPACITY);
        Self { tx }
    }

    pub fn emit(&self, event: WatchEvent) {
        let _ = self.tx.send(event);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<WatchEvent> {
        self.tx.subscribe()
    }
}

// -- Notification handler --

/// Listens for watch events and dispatches desktop notifications.
pub async fn run_notification_handler(
    mut rx: broadcast::Receiver<WatchEvent>,
    config: Arc<Mutex<config::Config>>,
    pause: Arc<Mutex<Option<Instant>>>,
) {
    loop {
        match rx.recv().await {
            Ok(event) => {
                // Check pause state before acquiring the config lock to
                // avoid holding two locks simultaneously.
                let paused = is_paused(&pause).await;

                // Extract what we need from config and drop the lock before
                // dispatching the notification (which performs async I/O).
                let cfg_snapshot = {
                    let cfg = config.lock().await;
                    let level = effective_level(&event, &cfg);
                    let suppressed = level == config::NotificationLevel::Off
                        || (level != config::NotificationLevel::Critical
                            && (paused || cfg.is_in_quiet_hours()));
                    if suppressed {
                        None
                    } else {
                        Some((cfg.clone(), level))
                    }
                };
                if let Some((cfg, level)) = &cfg_snapshot {
                    handle_notification(event, cfg, *level).await;
                }
            }
            Err(broadcast::error::RecvError::Lagged(n)) => {
                tracing::warn!("Notification handler dropped {n} events");
            }
            Err(broadcast::error::RecvError::Closed) => {
                tracing::debug!("Event bus closed, notification handler exiting");
                break;
            }
        }
    }
}

/// Determine the effective notification level for an event without sending it.
fn effective_level(event: &WatchEvent, cfg: &config::Config) -> config::NotificationLevel {
    match event {
        WatchEvent::RunStarted(run) => cfg.notifications_for(&run.repo, &run.branch).build_started,
        WatchEvent::RunCompleted {
            run, conclusion, ..
        } => {
            let notif = cfg.notifications_for(&run.repo, &run.branch);
            if conclusion == "success" {
                notif.build_success
            } else {
                notif.build_failure
            }
        }
        WatchEvent::StatusChanged { .. } => config::NotificationLevel::Off,
    }
}

async fn is_paused(pause: &Arc<Mutex<Option<Instant>>>) -> bool {
    let p = pause.lock().await;
    p.is_some_and(|deadline| Instant::now() < deadline)
}

async fn handle_notification(
    event: WatchEvent,
    cfg: &config::Config,
    level: config::NotificationLevel,
) {
    match event {
        WatchEvent::RunStarted(run) => {
            let repo_label = cfg.short_repo(&run.repo);
            platform::send(platform::Notification {
                title: format!("🔨 started: {} | {}", repo_label, run.workflow),
                body: format!("[{}] {}", run.branch, run.display_title()),
                level,
                url: Some(run.url()),
                group: run.notification_group(),
                app_name: run.repo,
            })
            .await;
        }
        WatchEvent::RunCompleted {
            run,
            conclusion,
            elapsed,
            failing_steps,
        } => {
            let succeeded = conclusion == "success";
            let repo_label = cfg.short_repo(&run.repo);

            let (emoji, status) = if succeeded {
                ("✅", "succeeded")
            } else {
                ("❌", "failed")
            };
            let mut body = format!("[{}] {}", run.branch, run.display_title());
            if let Some(d) = elapsed {
                let _ = write!(body, " in {}", format::duration(d));
            }
            if let Some(steps) = &failing_steps {
                let _ = write!(body, "\nFailed: {steps}");
            }

            platform::send(platform::Notification {
                title: format!("{emoji} {status}: {} | {}", repo_label, run.workflow),
                body,
                level,
                url: Some(run.url()),
                group: run.notification_group(),
                app_name: run.repo,
            })
            .await;
        }
        WatchEvent::StatusChanged { .. } => {
            // No desktop notification for status changes
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_snapshot() -> RunSnapshot {
        RunSnapshot {
            repo: "alice/app".to_string(),
            branch: "main".to_string(),
            run_id: 12345,
            workflow: "CI".to_string(),
            title: "Fix login bug".to_string(),
            event: "push".to_string(),
        }
    }

    #[test]
    fn run_snapshot_url() {
        let snap = make_snapshot();
        assert_eq!(
            snap.url(),
            "https://github.com/alice/app/actions/runs/12345"
        );
    }

    #[test]
    fn run_snapshot_display_title_push() {
        let snap = make_snapshot();
        assert_eq!(snap.display_title(), "Fix login bug");
    }

    #[test]
    fn run_snapshot_display_title_pr() {
        let mut snap = make_snapshot();
        snap.event = "pull_request".to_string();
        assert_eq!(snap.display_title(), "PR: Fix login bug");
    }

    #[test]
    fn run_snapshot_notification_group() {
        let snap = make_snapshot();
        assert_eq!(snap.notification_group(), "alice/app#main#CI");
    }

    #[test]
    fn effective_level_run_started() {
        let config = config::Config::default();
        let snap = make_snapshot();
        let event = WatchEvent::RunStarted(snap);
        assert_eq!(
            effective_level(&event, &config),
            config::NotificationLevel::Normal
        );
    }

    #[test]
    fn effective_level_run_completed_success() {
        let config = config::Config::default();
        let event = WatchEvent::RunCompleted {
            run: make_snapshot(),
            conclusion: "success".to_string(),
            elapsed: None,
            failing_steps: None,
        };
        assert_eq!(
            effective_level(&event, &config),
            config::NotificationLevel::Normal
        );
    }

    #[test]
    fn effective_level_run_completed_failure() {
        let config = config::Config::default();
        let event = WatchEvent::RunCompleted {
            run: make_snapshot(),
            conclusion: "failure".to_string(),
            elapsed: None,
            failing_steps: None,
        };
        assert_eq!(
            effective_level(&event, &config),
            config::NotificationLevel::Critical
        );
    }

    #[test]
    fn effective_level_status_changed_is_off() {
        let config = config::Config::default();
        let event = WatchEvent::StatusChanged {
            run: make_snapshot(),
            from: "queued".to_string(),
            to: "in_progress".to_string(),
        };
        assert_eq!(
            effective_level(&event, &config),
            config::NotificationLevel::Off
        );
    }
}
