use std::fmt::Write as _;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{Mutex, broadcast};

use build_watcher::config::{self, NotificationLevel};
use build_watcher::events::WatchEvent;
use build_watcher::format;
use build_watcher::status::RunConclusion;
use build_watcher::watcher::{PauseState, is_paused};

use crate::platform;

/// Listens for watch events and dispatches desktop notifications.
pub async fn run_notification_handler(
    mut rx: broadcast::Receiver<WatchEvent>,
    config: Arc<Mutex<config::Config>>,
    pause: PauseState,
) {
    loop {
        match rx.recv().await {
            Ok(event) => {
                // Check pause state before acquiring the config lock to
                // avoid holding two locks simultaneously.
                let paused = is_paused(&pause).await;

                // Extract only what we need from config (level + repo label)
                // and drop the lock before dispatching (which performs async I/O).
                let dispatch = {
                    let cfg = config.lock().await;
                    let level = effective_level(&event, &cfg);
                    let suppressed = level == NotificationLevel::Off
                        || (level != NotificationLevel::Critical
                            && (paused || cfg.is_in_quiet_hours()));
                    if suppressed {
                        None
                    } else {
                        let repo_label = event_repo(&event)
                            .map(|r| cfg.short_repo(r).to_string())
                            .unwrap_or_default();
                        Some((repo_label, level))
                    }
                };
                if let Some((repo_label, level)) = &dispatch {
                    handle_notification(event, repo_label, *level).await;
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

/// Extract the repo name from an event, if applicable.
fn event_repo(event: &WatchEvent) -> Option<&str> {
    match event {
        WatchEvent::RunStarted(run) => Some(&run.repo),
        WatchEvent::RunCompleted { run, .. } => Some(&run.repo),
        WatchEvent::StatusChanged { .. } => None,
    }
}

/// Determine the effective notification level for an event without sending it.
pub(crate) fn effective_level(event: &WatchEvent, cfg: &config::Config) -> NotificationLevel {
    match event {
        WatchEvent::RunStarted(run) => cfg.notifications_for(&run.repo, &run.branch).build_started,
        WatchEvent::RunCompleted {
            run, conclusion, ..
        } => {
            let notif = cfg.notifications_for(&run.repo, &run.branch);
            if *conclusion == RunConclusion::Success {
                notif.build_success
            } else {
                notif.build_failure
            }
        }
        WatchEvent::StatusChanged { .. } => NotificationLevel::Off,
    }
}

async fn handle_notification(event: WatchEvent, repo_label: &str, level: NotificationLevel) {
    match event {
        WatchEvent::RunStarted(run) => {
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
            ..
        } => {
            let succeeded = conclusion == RunConclusion::Success;

            let (emoji, status) = if succeeded {
                ("✅", "succeeded")
            } else {
                ("❌", "failed")
            };
            let mut body = format!("[{}] {}", run.branch, run.display_title());
            if let Some(secs) = elapsed {
                let _ = write!(
                    body,
                    " in {}",
                    format::duration(Duration::from_secs_f64(secs))
                );
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
            // No desktop notification for status changes.
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use build_watcher::config::NotificationLevel::*;
    use build_watcher::events::RunSnapshot;

    fn snap() -> RunSnapshot {
        RunSnapshot {
            repo: "alice/app".to_string(),
            branch: "main".to_string(),
            run_id: 12345,
            workflow: "CI".to_string(),
            title: "Fix login bug".to_string(),
            event: "push".to_string(),
            status: build_watcher::status::RunStatus::InProgress,
            attempt: 1,
        }
    }

    fn completed(conclusion: build_watcher::status::RunConclusion) -> WatchEvent {
        WatchEvent::RunCompleted {
            run: snap(),
            conclusion,
            elapsed: None,
            failing_steps: None,
            failing_job_id: None,
        }
    }

    #[test]
    fn effective_level_by_event_type() {
        use build_watcher::status::{RunConclusion, RunStatus};

        let cfg = config::Config::default();

        assert_eq!(
            effective_level(&WatchEvent::RunStarted(snap()), &cfg),
            Normal
        );
        assert_eq!(
            effective_level(&completed(RunConclusion::Success), &cfg),
            Normal
        );
        assert_eq!(
            effective_level(&completed(RunConclusion::Failure), &cfg),
            Critical
        );

        let status = WatchEvent::StatusChanged {
            run: snap(),
            from: RunStatus::Queued,
            to: RunStatus::InProgress,
        };
        assert_eq!(effective_level(&status, &cfg), Off);
    }
}
