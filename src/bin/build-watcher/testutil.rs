use build_watcher::events::{RunSnapshot, WatchEvent};
use build_watcher::status::{RunConclusion, RunStatus};

pub fn snap() -> RunSnapshot {
    RunSnapshot {
        repo: "alice/app".to_string(),
        branch: "main".to_string(),
        run_id: 12345,
        workflow: "CI".to_string(),
        title: "Fix login bug".to_string(),
        event: "push".to_string(),
        status: RunStatus::InProgress,
        attempt: 1,
        url: "https://github.com/alice/app/actions/runs/12345".to_string(),
        actor: None,
        commit_author: None,
    }
}

pub fn snap_workflow(name: &str) -> RunSnapshot {
    let mut s = snap();
    s.workflow = name.to_string();
    s
}

pub fn completed(conclusion: RunConclusion) -> WatchEvent {
    WatchEvent::RunCompleted {
        run: snap(),
        conclusion,
        elapsed: None,
        failing_steps: None,
        failing_job_id: None,
    }
}
