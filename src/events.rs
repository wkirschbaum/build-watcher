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
    pub head_sha: String,
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
            head_sha: run.head_sha.clone(),
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
        crate::github::display_title(&self.event, &self.title, &self.head_sha)
    }

    fn notification_group(&self) -> String {
        format!("{}#{}#{}", self.repo, self.branch, self.workflow)
    }
}

/// Events emitted by the watcher polling loop.
#[derive(Debug, Clone)]
#[allow(dead_code)]
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

/// Listens for watch events and dispatches desktop notifications + sound.
pub async fn run_notification_handler(
    mut rx: broadcast::Receiver<WatchEvent>,
    config: Arc<Mutex<config::Config>>,
    pause: Arc<Mutex<Option<Instant>>>,
) {
    loop {
        match rx.recv().await {
            Ok(event) => {
                if !is_paused(&pause).await {
                    handle_notification(event, &config).await;
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

async fn is_paused(pause: &Arc<Mutex<Option<Instant>>>) -> bool {
    let p = pause.lock().await;
    p.is_some_and(|deadline| Instant::now() < deadline)
}

/// Returns just the repo name (e.g. `"bar"`) when it is unique among all watched
/// repos, or the full `"owner/repo"` string when another watched repo shares the
/// same name (e.g. both `"foo/bar"` and `"zoo/bar"` are watched).
fn short_repo<'a>(repo: &'a str, cfg: &config::Config) -> &'a str {
    let Some((_, name)) = repo.rsplit_once('/') else {
        return repo;
    };
    let ambiguous = cfg
        .repos
        .keys()
        .any(|r| r != repo && r.rsplit_once('/').map_or(r.as_str(), |(_, n)| n) == name);
    if ambiguous { repo } else { name }
}

async fn handle_notification(event: WatchEvent, config: &Arc<Mutex<config::Config>>) {
    match event {
        WatchEvent::RunStarted(run) => {
            let (level, repo_label) = {
                let cfg = config.lock().await;
                let level = cfg.notifications_for(&run.repo, &run.branch).build_started;
                let label = short_repo(&run.repo, &cfg).to_string();
                (level, label)
            };
            let group = run.notification_group();
            platform::send_notification(
                &format!("🔨 {} / {} - started", repo_label, run.workflow),
                &format!("[{}] {}", run.branch, run.display_title()),
                level,
                Some(&run.url()),
                Some(&group),
            )
            .await;
        }
        WatchEvent::RunCompleted {
            run,
            conclusion,
            elapsed,
            failing_steps,
        } => {
            let succeeded = conclusion == "success";
            let (level, sound_on_failure, sound_file, repo_label) = {
                let cfg = config.lock().await;
                let notif = cfg.notifications_for(&run.repo, &run.branch);
                let level = if succeeded {
                    notif.build_success
                } else {
                    notif.build_failure
                };
                (
                    level,
                    cfg.sound_on_failure_for(&run.repo),
                    cfg.sound_on_failure.sound_file.clone(),
                    short_repo(&run.repo, &cfg).to_string(),
                )
            };

            let emoji = if succeeded { "✅" } else { "❌" };
            let mut body = format!("[{}] {}", run.branch, run.display_title());
            if let Some(d) = elapsed {
                let _ = write!(body, " in {}", format::duration(d));
            }
            if let Some(steps) = &failing_steps {
                let _ = write!(body, "\nFailed: {steps}");
            }

            let group = run.notification_group();
            platform::send_notification(
                &format!("{emoji} {} / {} - {conclusion}", repo_label, run.workflow),
                &body,
                level,
                Some(&run.url()),
                Some(&group),
            )
            .await;

            if !succeeded && sound_on_failure {
                platform::play_sound(sound_file.as_deref()).await;
            }
        }
        WatchEvent::StatusChanged { .. } => {
            // No desktop notification for status changes
        }
    }
}
