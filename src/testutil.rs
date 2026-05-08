use crate::events::{RunSnapshot, WatchEvent};
use crate::github::RunInfo;
use crate::status::{RunConclusion, RunStatus};

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

pub fn make_run(id: u64, status: RunStatus, conclusion: &str) -> RunInfo {
    RunInfo {
        id,
        status,
        conclusion: conclusion.to_string(),
        title: "Test PR".to_string(),
        workflow: "CI".to_string(),
        head_sha: "abc1234".to_string(),
        event: "push".to_string(),
        head_branch: "main".to_string(),
        attempt: 1,
        created_at: "2026-01-01T10:00:00Z".to_string(),
        updated_at: "2026-01-01T10:05:00Z".to_string(),
        url: "https://github.com/test/repo/actions/runs/1".to_string(),
    }
}
