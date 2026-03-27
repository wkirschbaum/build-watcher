use tokio::sync::broadcast;

use serde::{Deserialize, Serialize};

use crate::github::RunInfo;

const CHANNEL_CAPACITY: usize = 256;

/// Snapshot of a run's identity, carried by events.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunSnapshot {
    pub repo: String,
    pub branch: String,
    pub run_id: u64,
    pub workflow: String,
    pub title: String,
    pub event: String,
    /// GitHub run status at the moment this snapshot was taken (e.g. `"queued"`, `"in_progress"`).
    /// Allows TUI clients to populate `ActiveRunView.status` from a `RunStarted` event
    /// without re-fetching `/status`.
    pub status: String,
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
            status: run.status.clone(),
        }
    }

    pub fn url(&self) -> String {
        crate::github::run_url(&self.repo, self.run_id)
    }

    pub fn display_title(&self) -> String {
        crate::github::display_title(&self.event, &self.title)
    }

    pub fn notification_group(&self) -> String {
        format!("{}#{}#{}", self.repo, self.branch, self.workflow)
    }
}

/// Events emitted by the watcher polling loop.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum WatchEvent {
    /// A new build was detected.
    RunStarted(RunSnapshot),

    /// A build completed (success, failure, cancelled, etc.).
    RunCompleted {
        run: RunSnapshot,
        conclusion: String,
        /// Elapsed seconds from when the poller first saw the run until completion.
        /// `None` for runs that were already completed when first detected.
        elapsed: Option<f64>,
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

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap() -> RunSnapshot {
        RunSnapshot {
            repo: "alice/app".to_string(),
            branch: "main".to_string(),
            run_id: 12345,
            workflow: "CI".to_string(),
            title: "Fix login bug".to_string(),
            event: "push".to_string(),
            status: "in_progress".to_string(),
        }
    }

    fn completed(conclusion: &str) -> WatchEvent {
        WatchEvent::RunCompleted {
            run: snap(),
            conclusion: conclusion.to_string(),
            elapsed: None,
            failing_steps: None,
        }
    }

    #[test]
    fn run_snapshot_methods() {
        let s = snap();
        assert_eq!(s.url(), "https://github.com/alice/app/actions/runs/12345");
        assert_eq!(s.display_title(), "Fix login bug");
        assert_eq!(s.notification_group(), "alice/app#main#CI");

        let mut pr = snap();
        pr.event = "pull_request".to_string();
        assert_eq!(pr.display_title(), "PR: Fix login bug");
    }

    #[test]
    fn from_run_info_copies_fields() {
        let run = crate::github::RunInfo {
            id: 99,
            status: "in_progress".to_string(),
            conclusion: String::new(),
            title: "Update deps".to_string(),
            workflow: "Deploy".to_string(),
            head_sha: "abc1234".to_string(),
            event: "pull_request".to_string(),
        };
        let s = RunSnapshot::from_run_info(&run, "alice/app", "release");
        assert_eq!(s.repo, "alice/app");
        assert_eq!(s.branch, "release");
        assert_eq!(s.run_id, 99);
        assert_eq!(s.workflow, "Deploy");
        assert_eq!(s.title, "Update deps");
        assert_eq!(s.event, "pull_request");
        assert_eq!(s.status, "in_progress");
    }

    #[test]
    fn elapsed_serializes_as_float() {
        let json = serde_json::to_value(completed("success")).unwrap();
        assert_eq!(json["RunCompleted"]["elapsed"], serde_json::Value::Null);

        let event = WatchEvent::RunCompleted {
            run: snap(),
            conclusion: "success".to_string(),
            elapsed: Some(134.5),
            failing_steps: None,
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["RunCompleted"]["elapsed"], 134.5);
    }

    #[test]
    fn elapsed_round_trips_through_json() {
        let event = WatchEvent::RunCompleted {
            run: snap(),
            conclusion: "failure".to_string(),
            elapsed: Some(42.0),
            failing_steps: Some("Build / Run tests".to_string()),
        };
        let json = serde_json::to_string(&event).unwrap();
        let decoded: WatchEvent = serde_json::from_str(&json).unwrap();
        match decoded {
            WatchEvent::RunCompleted {
                elapsed,
                failing_steps,
                conclusion,
                ..
            } => {
                assert_eq!(elapsed, Some(42.0));
                assert_eq!(failing_steps.as_deref(), Some("Build / Run tests"));
                assert_eq!(conclusion, "failure");
            }
            other => panic!("expected RunCompleted, got {other:?}"),
        }
    }

    #[test]
    fn run_started_round_trips_through_json() {
        let event = WatchEvent::RunStarted(snap());
        let json = serde_json::to_string(&event).unwrap();
        let decoded: WatchEvent = serde_json::from_str(&json).unwrap();
        match decoded {
            WatchEvent::RunStarted(s) => assert_eq!(s.repo, "alice/app"),
            other => panic!("expected RunStarted, got {other:?}"),
        }
    }

    #[test]
    fn event_bus_emit_and_subscribe() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();

        bus.emit(WatchEvent::RunStarted(snap()));

        match rx.try_recv() {
            Ok(WatchEvent::RunStarted(s)) => assert_eq!(s.repo, "alice/app"),
            other => panic!("expected RunStarted, got {other:?}"),
        }
    }
}
