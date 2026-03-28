use super::*;

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::Mutex;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::events::WatchEvent;
use crate::github::{GhError, RunInfo};
use crate::status::{RunConclusion, RunStatus};

use super::repo_poller::{RepoPoller, RunChange};

fn make_run(id: u64, status: RunStatus, conclusion: &str) -> RunInfo {
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
    }
}

fn make_active(status: RunStatus) -> ActiveRun {
    ActiveRun {
        status,
        started_at: Instant::now(),
        workflow: "CI".to_string(),
        title: "Test PR".to_string(),
        event: "push".to_string(),
        attempt: 1,
    }
}

fn make_entry() -> WatchEntry {
    WatchEntry {
        last_seen_run_id: 100,
        active_runs: HashMap::from([
            (101, make_active(RunStatus::InProgress)),
            (102, make_active(RunStatus::Queued)),
        ]),
        failure_counts: HashMap::new(),
        last_build: None,
    }
}

// -- WatchKey tests --

#[test]
fn watch_key_roundtrip_and_matching() {
    let k = WatchKey::new("alice/myapp", "main");
    assert_eq!(k.to_string(), "alice/myapp#main");
    assert!(k.matches_repo("alice/myapp"));
    assert!(!k.matches_repo("bob/other"));

    let parsed = WatchKey::parse("alice/myapp#main");
    assert_eq!(parsed.repo, "alice/myapp");
    assert_eq!(parsed.branch, "main");

    // Falls back to "main" when no branch in persisted key
    let legacy = WatchKey::parse("alice/myapp");
    assert_eq!(legacy.branch, "main");
}

#[test]
fn watch_key_serde_roundtrip() {
    let key = WatchKey::new("alice/app", "feature/xyz");
    let json = serde_json::to_string(&key).unwrap();
    assert_eq!(json, "\"alice/app#feature/xyz\"");
    let parsed: WatchKey = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed, key);
}

#[test]
fn active_run_display_title() {
    let push = make_active(RunStatus::InProgress);
    assert_eq!(push.display_title(), "Test PR");

    let mut pr = make_active(RunStatus::Queued);
    pr.event = "pull_request".to_string();
    assert_eq!(pr.display_title(), "PR: Test PR");
}

// -- WatchEntry state machine tests --

#[test]
fn record_completion_returns_elapsed() {
    let mut entry = make_entry();
    let run = make_run(101, RunStatus::Completed, "success");

    let elapsed = entry.record_completion(&run, None, None, 0);

    // Active run was present, so elapsed should be Some
    assert!(elapsed.is_some());
    // Should be very small since we just created it
    assert!(elapsed.unwrap() < std::time::Duration::from_secs(1));
}

#[test]
fn record_completion_returns_none_for_unknown_run() {
    let mut entry = make_entry();
    let run = make_run(999, RunStatus::Completed, "success");

    let elapsed = entry.record_completion(&run, None, None, 0);

    assert!(elapsed.is_none());
}

#[test]
fn record_completion_removes_run_and_sets_last_build() {
    let mut entry = make_entry();
    let run = make_run(101, RunStatus::Completed, "success");

    entry.record_completion(&run, None, None, 0);

    assert!(!entry.active_runs.contains_key(&101));
    assert!(entry.active_runs.contains_key(&102));
    let lb = entry.last_build.unwrap();
    assert_eq!(lb.run_id, 101);
    assert_eq!(lb.conclusion, "success");
}

#[test]
fn record_completion_stores_failing_steps() {
    let mut entry = make_entry();
    let run = make_run(101, RunStatus::Completed, "failure");

    entry.record_completion(&run, Some("Build / Run tests".to_string()), None, 0);

    let lb = entry.last_build.unwrap();
    assert_eq!(lb.failing_steps.as_deref(), Some("Build / Run tests"));
}

#[test]
fn record_completion_clears_failure_count() {
    let mut entry = make_entry();
    entry.failure_counts.insert(101, 3);
    let run = make_run(101, RunStatus::Completed, "failure");

    entry.record_completion(&run, None, None, 0);

    assert!(!entry.failure_counts.contains_key(&101));
}

#[test]
fn record_failure_increments_count() {
    let mut entry = make_entry();
    let error = GhError::Timeout {
        repo: "test".to_string(),
        timeout_secs: 30,
    };

    let removed = entry.record_failure(101, &error);

    assert!(!removed);
    assert_eq!(entry.failure_counts[&101], 1);
    assert!(entry.active_runs.contains_key(&101));
}

#[test]
fn record_failure_removes_run_at_max_failures() {
    let mut entry = make_entry();
    entry.failure_counts.insert(101, types::MAX_GH_FAILURES - 1);
    let error = GhError::Timeout {
        repo: "test".to_string(),
        timeout_secs: 30,
    };

    let removed = entry.record_failure(101, &error);

    assert!(removed);
    assert!(!entry.active_runs.contains_key(&101));
    assert!(!entry.failure_counts.contains_key(&101));
}

#[test]
fn clear_failure_count_resets_on_success() {
    let mut entry = make_entry();
    entry.failure_counts.insert(101, 3);

    entry.clear_failure_count(101);

    assert!(!entry.failure_counts.contains_key(&101));
}

#[test]
fn update_status_changes_when_different() {
    let mut entry = make_entry();

    let old = entry.update_status(101, &RunStatus::Queued);

    assert_eq!(old, Some(RunStatus::InProgress));
    assert_eq!(entry.active_runs[&101].status, RunStatus::Queued);
}

#[test]
fn update_status_noop_when_same() {
    let mut entry = make_entry();

    let old = entry.update_status(101, &RunStatus::InProgress);

    assert!(old.is_none());
    assert_eq!(entry.active_runs[&101].status, RunStatus::InProgress);
}

#[test]
fn update_status_noop_for_unknown_run() {
    let mut entry = make_entry();

    entry.update_status(999, &RunStatus::Completed);

    assert!(!entry.active_runs.contains_key(&999));
}

#[test]
fn incorporate_new_runs_tracks_in_progress() {
    let mut entry = make_entry();
    let run = make_run(200, RunStatus::InProgress, "");
    let new_runs: Vec<&RunInfo> = vec![&run];

    entry.incorporate_new_runs(&new_runs, Instant::now(), 0);

    assert_eq!(entry.last_seen_run_id, 200);
    assert_eq!(entry.active_runs[&200].status, RunStatus::InProgress);
    assert!(entry.last_build.is_none());
}

#[test]
fn incorporate_new_runs_records_completed() {
    let mut entry = make_entry();
    let run = make_run(200, RunStatus::Completed, "success");
    let new_runs: Vec<&RunInfo> = vec![&run];

    entry.incorporate_new_runs(&new_runs, Instant::now(), 0);

    assert_eq!(entry.last_seen_run_id, 200);
    assert!(!entry.active_runs.contains_key(&200));
    assert_eq!(entry.last_build.unwrap().run_id, 200);
}

#[test]
fn incorporate_new_runs_newest_completed_wins_last_build() {
    let mut entry = make_entry();
    let old = make_run(200, RunStatus::Completed, "failure");
    let new = make_run(201, RunStatus::Completed, "success");
    let new_runs: Vec<&RunInfo> = vec![&new, &old];

    entry.incorporate_new_runs(&new_runs, Instant::now(), 0);

    assert_eq!(entry.last_seen_run_id, 201);
    let lb = entry.last_build.unwrap();
    assert_eq!(lb.run_id, 201);
    assert_eq!(lb.conclusion, "success");
}

#[test]
fn incorporate_new_runs_mixed_statuses() {
    let mut entry = make_entry();
    let completed = make_run(200, RunStatus::Completed, "success");
    let active = make_run(201, RunStatus::InProgress, "");
    let new_runs: Vec<&RunInfo> = vec![&active, &completed];

    entry.incorporate_new_runs(&new_runs, Instant::now(), 0);

    assert_eq!(entry.last_seen_run_id, 201);
    assert_eq!(entry.active_runs[&201].status, RunStatus::InProgress);
    assert!(!entry.active_runs.contains_key(&200));
    assert_eq!(entry.last_build.unwrap().run_id, 200);
}

#[test]
fn has_active_runs_reflects_state() {
    let mut entry = make_entry();
    assert!(entry.has_active_runs());

    entry.active_runs.clear();
    assert!(!entry.has_active_runs());
}

#[test]
fn persisted_roundtrip_preserves_fields() {
    let mut entry = make_entry();
    let run = make_run(101, RunStatus::Completed, "success");
    entry.record_completion(&run, None, None, 0);

    let persisted = entry.to_persisted();
    let restored = WatchEntry::from_persisted(persisted);

    assert_eq!(restored.last_seen_run_id, entry.last_seen_run_id);
    assert!(restored.active_runs.is_empty());
    assert!(restored.failure_counts.is_empty());
    assert_eq!(restored.last_build.unwrap().run_id, 101);
}

// -- runs_for_branch tests --

#[test]
fn runs_for_branch_filters_by_branch() {
    let mut r1 = make_run(1, RunStatus::InProgress, "");
    r1.head_branch = "main".to_string();
    let mut r2 = make_run(2, RunStatus::InProgress, "");
    r2.head_branch = "develop".to_string();
    let mut r3 = make_run(3, RunStatus::Completed, "success");
    r3.head_branch = "main".to_string();
    let runs = vec![r1, r2, r3];

    let main_runs = runs_for_branch(&runs, "main");
    assert_eq!(main_runs.len(), 2);
    assert_eq!(main_runs[0].id, 1);
    assert_eq!(main_runs[1].id, 3);

    let dev_runs = runs_for_branch(&runs, "develop");
    assert_eq!(dev_runs.len(), 1);
    assert_eq!(dev_runs[0].id, 2);

    let empty = runs_for_branch(&runs, "feature/xyz");
    assert!(empty.is_empty());
}

// -- filter_runs tests --

#[test]
fn filter_runs_no_filters() {
    let runs = vec![
        make_run(1, RunStatus::Completed, "success"),
        make_run(2, RunStatus::InProgress, ""),
    ];
    assert_eq!(filter_runs(&runs, &[], &[]).len(), 2);
}

#[test]
fn filter_runs_workflow_allowlist() {
    let mut r1 = make_run(1, RunStatus::Completed, "success");
    r1.workflow = "CI".to_string();
    let mut r2 = make_run(2, RunStatus::Completed, "success");
    r2.workflow = "Deploy".to_string();
    let runs = vec![r1, r2];

    let filtered = filter_runs(&runs, &["ci".to_string()], &[]);
    assert_eq!(filtered.len(), 1);
    assert_eq!(filtered[0].workflow, "CI");
}

#[test]
fn filter_runs_ignored_workflows() {
    let mut r1 = make_run(1, RunStatus::Completed, "success");
    r1.workflow = "CI".to_string();
    let mut r2 = make_run(2, RunStatus::Completed, "success");
    r2.workflow = "Semgrep".to_string();
    let runs = vec![r1, r2];

    let filtered = filter_runs(&runs, &[], &["semgrep".to_string()]);
    assert_eq!(filtered.len(), 1);
    assert_eq!(filtered[0].workflow, "CI");
}

#[test]
fn filter_runs_both_filters() {
    let mut r1 = make_run(1, RunStatus::Completed, "success");
    r1.workflow = "CI".to_string();
    let mut r2 = make_run(2, RunStatus::Completed, "success");
    r2.workflow = "Deploy".to_string();
    let mut r3 = make_run(3, RunStatus::Completed, "success");
    r3.workflow = "Semgrep".to_string();
    let runs = vec![r1, r2, r3];

    // Allow CI and Deploy, ignore Semgrep
    let filtered = filter_runs(
        &runs,
        &["CI".to_string(), "Deploy".to_string()],
        &["Semgrep".to_string()],
    );
    assert_eq!(filtered.len(), 2);
}

// -- last_failed_build tests --

#[test]
fn last_failed_build_finds_failure() {
    let mut watches = HashMap::new();
    let mut entry = make_entry();
    let run = make_run(200, RunStatus::Completed, "failure");
    entry.record_completion(&run, None, None, 0);
    watches.insert(WatchKey::new("alice/app", "main"), entry);

    let result = last_failed_build(&watches, "alice/app");
    assert!(result.is_some());
    let (key, build) = result.unwrap();
    assert_eq!(key.repo, "alice/app");
    assert_eq!(key.branch, "main");
    assert_eq!(build.run_id, 200);
}

#[test]
fn last_failed_build_ignores_success() {
    let mut watches = HashMap::new();
    let mut entry = make_entry();
    let run = make_run(200, RunStatus::Completed, "success");
    entry.record_completion(&run, None, None, 0);
    watches.insert(WatchKey::new("alice/app", "main"), entry);

    assert!(last_failed_build(&watches, "alice/app").is_none());
}

#[test]
fn last_failed_build_picks_most_recent() {
    let mut watches = HashMap::new();

    let mut entry1 = make_entry();
    let run1 = make_run(100, RunStatus::Completed, "failure");
    entry1.record_completion(&run1, None, None, 0);
    watches.insert(WatchKey::new("alice/app", "main"), entry1);

    let mut entry2 = make_entry();
    let run2 = make_run(200, RunStatus::Completed, "failure");
    entry2.record_completion(&run2, None, None, 0);
    watches.insert(WatchKey::new("alice/app", "develop"), entry2);

    let (_, build) = last_failed_build(&watches, "alice/app").unwrap();
    assert_eq!(build.run_id, 200);
}

#[test]
fn last_failed_build_ignores_other_repos() {
    let mut watches = HashMap::new();
    let mut entry = make_entry();
    let run = make_run(200, RunStatus::Completed, "failure");
    entry.record_completion(&run, None, None, 0);
    watches.insert(WatchKey::new("bob/other", "main"), entry);

    assert!(last_failed_build(&watches, "alice/app").is_none());
}

// -- count_api_calls --

#[test]
fn count_api_calls_reflects_active_runs() {
    let mut watches = HashMap::new();
    // 2 watches, one with 3 active runs, one idle
    let mut active_runs = HashMap::new();
    for id in 1..=3 {
        active_runs.insert(id, make_active(RunStatus::InProgress));
    }
    let entry1 = WatchEntry {
        last_seen_run_id: 100,
        active_runs,
        failure_counts: HashMap::new(),
        last_build: None,
    };
    let entry2 = WatchEntry {
        last_seen_run_id: 100,
        active_runs: HashMap::new(),
        failure_counts: HashMap::new(),
        last_build: None,
    };
    watches.insert(WatchKey::new("owner/repo1", "main"), entry1);
    watches.insert(WatchKey::new("owner/repo2", "main"), entry2);

    // 2 unique repos = 2 base calls, repo1 has active runs = +1 batch call = 3
    assert_eq!(count_api_calls(&watches), 3);
}

#[test]
fn count_api_calls_same_repo_multiple_branches() {
    let mut watches = HashMap::new();
    let mut active_runs = HashMap::new();
    active_runs.insert(1, make_active(RunStatus::InProgress));
    let entry1 = WatchEntry {
        last_seen_run_id: 100,
        active_runs,
        failure_counts: HashMap::new(),
        last_build: None,
    };
    let entry2 = WatchEntry {
        last_seen_run_id: 100,
        active_runs: HashMap::new(),
        failure_counts: HashMap::new(),
        last_build: None,
    };
    watches.insert(WatchKey::new("owner/repo1", "main"), entry1);
    watches.insert(WatchKey::new("owner/repo1", "develop"), entry2);

    // 1 unique repo = 1 base call, has active runs = +1 batch call = 2
    assert_eq!(count_api_calls(&watches), 2);
}

#[test]
fn count_api_calls_empty_watches() {
    let watches = HashMap::new();
    assert_eq!(count_api_calls(&watches), 0);
}

// -- Mock GitHub client for integration tests --

struct MockGitHub {
    runs: Vec<RunInfo>,
    failure_msg: Option<String>,
}

impl MockGitHub {
    fn with_runs(runs: Vec<RunInfo>) -> Arc<dyn crate::github::GitHubClient> {
        Arc::new(Self {
            runs,
            failure_msg: None,
        })
    }

    fn with_runs_and_failures(
        runs: Vec<RunInfo>,
        failure_msg: &str,
    ) -> Arc<dyn crate::github::GitHubClient> {
        Arc::new(Self {
            runs,
            failure_msg: Some(failure_msg.to_string()),
        })
    }
}

#[async_trait::async_trait]
impl crate::github::GitHubClient for MockGitHub {
    async fn recent_runs(&self, _: &str, _: &str) -> Result<Vec<RunInfo>, GhError> {
        Ok(self.runs.clone())
    }
    async fn recent_runs_for_repo(&self, _: &str, _: u32) -> Result<Vec<RunInfo>, GhError> {
        Ok(self.runs.clone())
    }
    async fn in_progress_runs_for_repo(&self, _: &str) -> Result<Vec<RunInfo>, GhError> {
        Ok(self
            .runs
            .iter()
            .filter(|r| !r.is_completed())
            .cloned()
            .collect())
    }
    async fn run_status(&self, _: &str, run_id: u64) -> Result<RunInfo, GhError> {
        self.runs
            .iter()
            .find(|r| r.id == run_id)
            .cloned()
            .ok_or(GhError::MissingFields {
                repo: "mock".into(),
            })
    }
    async fn failing_steps(&self, _: &str, _: u64) -> Option<crate::github::FailureInfo> {
        self.failure_msg
            .clone()
            .map(|steps| crate::github::FailureInfo {
                steps,
                first_job_id: None,
            })
    }
    async fn run_rerun(&self, _: &str, _: u64, _: bool) -> Result<String, GhError> {
        Ok(String::new())
    }
    async fn run_list_history(
        &self,
        _: &str,
        _: Option<&str>,
        _: u32,
    ) -> Result<Vec<crate::github::HistoryEntry>, GhError> {
        Ok(vec![])
    }
    async fn rate_limit(&self) -> Result<crate::github::RateLimit, GhError> {
        Ok(crate::github::RateLimit {
            limit: 5000,
            remaining: 5000,
            reset: crate::config::unix_now() + 3600,
            used: 0,
        })
    }
}

fn mock_handle(github: Arc<dyn crate::github::GitHubClient>) -> WatcherHandle {
    WatcherHandle::new(
        CancellationToken::new(),
        crate::events::EventBus::new(),
        github,
        Arc::new(crate::persistence::NullPersistence),
        Arc::new(Mutex::new(HashMap::new())),
    )
}

fn make_repo_poller(
    repo: &str,
    watches: &Watches,
    config: &SharedConfig,
    rate_limit: &RateLimitState,
    handle: &WatcherHandle,
) -> RepoPoller {
    RepoPoller {
        repo: repo.to_string(),
        watches: watches.clone(),
        config: config.clone(),
        rate_limit: rate_limit.clone(),
        token: handle.cancel.child_token(),
        events: handle.events.clone(),
        github: handle.github.clone(),
        persistence: handle.persistence.clone(),
        history: handle.history.clone(),
        config_changed: handle.config_changed.clone(),
        last_active_secs: 0,
    }
}

#[tokio::test]
async fn start_watch_with_mock_github() {
    let runs = vec![
        make_run(100, RunStatus::Completed, "success"),
        make_run(101, RunStatus::InProgress, ""),
    ];
    let gh = MockGitHub::with_runs(runs);
    let watches: Watches = Arc::new(Mutex::new(HashMap::new()));
    let config: SharedConfig = Arc::new(Mutex::new(Config::default()));
    let rate_limit: RateLimitState = Arc::new(Mutex::new(None));
    let handle = mock_handle(gh);

    let result = start_watch(&watches, &config, &handle, &rate_limit, "alice/app", "main").await;
    assert!(result.is_ok());

    let w = watches.lock().await;
    let key = WatchKey::new("alice/app", "main");
    assert!(w.contains_key(&key));
    let entry = &w[&key];
    assert_eq!(entry.last_seen_run_id, 101);
    assert!(entry.active_runs.contains_key(&101));
    assert!(!entry.active_runs.contains_key(&100)); // completed, not tracked

    handle.cancel.cancel();
}

#[tokio::test]
async fn start_watch_rejects_empty_runs() {
    let gh = MockGitHub::with_runs(vec![]);
    let watches: Watches = Arc::new(Mutex::new(HashMap::new()));
    let config: SharedConfig = Arc::new(Mutex::new(Config::default()));
    let rate_limit: RateLimitState = Arc::new(Mutex::new(None));
    let handle = mock_handle(gh);

    let result = start_watch(&watches, &config, &handle, &rate_limit, "alice/app", "main").await;
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("no workflow runs found"));
}

#[tokio::test]
async fn start_watch_deduplicates() {
    let runs = vec![make_run(100, RunStatus::Completed, "success")];
    let gh = MockGitHub::with_runs(runs);
    let watches: Watches = Arc::new(Mutex::new(HashMap::new()));
    let config: SharedConfig = Arc::new(Mutex::new(Config::default()));
    let rate_limit: RateLimitState = Arc::new(Mutex::new(None));
    let handle = mock_handle(gh);

    let r1 = start_watch(&watches, &config, &handle, &rate_limit, "alice/app", "main").await;
    assert!(r1.is_ok());

    let r2 = start_watch(&watches, &config, &handle, &rate_limit, "alice/app", "main").await;
    assert!(r2.unwrap().contains("already being watched"));

    handle.cancel.cancel();
}

// -- RepoPoller: check_for_new_runs_repo_wide --

#[tokio::test]
async fn check_for_new_runs_detects_new_builds() {
    let key = WatchKey::new("alice/app", "main");
    // Mock returns runs 99-102; watch has seen up to 100
    let runs = vec![
        make_run(99, RunStatus::Completed, "success"),
        make_run(100, RunStatus::Completed, "success"),
        make_run(101, RunStatus::InProgress, ""),
        make_run(102, RunStatus::Completed, "failure"),
    ];
    let gh = MockGitHub::with_runs_and_failures(runs, "Build / Run tests");
    let watches: Watches = Arc::new(Mutex::new(HashMap::new()));
    let config: SharedConfig = Arc::new(Mutex::new(Config::default()));
    let rate_limit: RateLimitState = Arc::new(Mutex::new(None));
    let handle = mock_handle(gh);

    // Seed a watch entry with last_seen=100
    {
        let mut w = watches.lock().await;
        w.insert(
            key.clone(),
            WatchEntry {
                last_seen_run_id: 100,
                active_runs: HashMap::new(),
                failure_counts: HashMap::new(),
                last_build: None,
            },
        );
    }

    let mut rx = handle.events.subscribe();
    let poller = make_repo_poller("alice/app", &watches, &config, &rate_limit, &handle);

    let changes = poller.check_for_new_runs_repo_wide().await;

    // Verify high-water mark advanced
    let w = watches.lock().await;
    let entry = &w[&key];
    assert_eq!(entry.last_seen_run_id, 102);
    // 101 is in_progress → tracked as active
    assert!(entry.active_runs.contains_key(&101));
    // 102 is completed → last_build
    assert_eq!(entry.last_build.as_ref().unwrap().run_id, 102);
    drop(w);

    // Verify returned changes: RunStarted for 101, RunCompleted for 102.
    // No RunStarted for 102 — it was already completed when discovered.
    assert_eq!(changes.len(), 2, "expected 2 changes, got {changes:?}");
    assert_eq!(changes[0].run_id(), 101);
    assert_eq!(changes[1].run_id(), 102);

    // Emit and verify event content via bus
    for c in changes {
        handle.events.emit(c.into_event());
    }
    let mut events = vec![];
    while let Ok(e) = rx.try_recv() {
        events.push(e);
    }
    assert!(
        matches!(&events[0], WatchEvent::RunStarted(s) if s.run_id == 101),
        "expected RunStarted(101), got {:?}",
        events[0]
    );
    assert!(
        matches!(&events[1], WatchEvent::RunCompleted { run, .. } if run.run_id == 102),
        "expected RunCompleted(102), got {:?}",
        events[1]
    );

    handle.cancel.cancel();
}

#[tokio::test]
async fn check_for_new_runs_applies_workflow_filter() {
    let key = WatchKey::new("alice/app", "main");
    let mut ci = make_run(101, RunStatus::InProgress, "");
    ci.workflow = "CI".to_string();
    let mut semgrep = make_run(102, RunStatus::InProgress, "");
    semgrep.workflow = "Semgrep".to_string();

    let gh = MockGitHub::with_runs(vec![ci, semgrep]);
    let watches: Watches = Arc::new(Mutex::new(HashMap::new()));
    // Set ignored_workflows via config so the RepoPoller picks them up.
    let mut cfg = Config::default();
    cfg.ignored_workflows = vec!["Semgrep".to_string()];
    let config: SharedConfig = Arc::new(Mutex::new(cfg));
    let rate_limit: RateLimitState = Arc::new(Mutex::new(None));
    let handle = mock_handle(gh);

    {
        let mut w = watches.lock().await;
        w.insert(
            key.clone(),
            WatchEntry {
                last_seen_run_id: 100,
                active_runs: HashMap::new(),
                failure_counts: HashMap::new(),
                last_build: None,
            },
        );
    }

    let poller = make_repo_poller("alice/app", &watches, &config, &rate_limit, &handle);

    poller.check_for_new_runs_repo_wide().await;

    let w = watches.lock().await;
    let entry = &w[&key];
    // CI tracked, Semgrep filtered out
    assert!(entry.active_runs.contains_key(&101));
    assert!(!entry.active_runs.contains_key(&102));
    // High-water mark includes filtered runs
    assert_eq!(entry.last_seen_run_id, 102);

    handle.cancel.cancel();
}

// -- RepoPoller: poll_active_runs_batch --

#[tokio::test]
async fn poll_active_runs_detects_completion() {
    let key = WatchKey::new("alice/app", "main");
    // Mock: run 101 now completed (not in in_progress list, so fallback to run_status)
    let runs = vec![make_run(101, RunStatus::Completed, "success")];
    let gh = MockGitHub::with_runs(runs);
    let watches: Watches = Arc::new(Mutex::new(HashMap::new()));
    let config: SharedConfig = Arc::new(Mutex::new(Config::default()));
    let rate_limit: RateLimitState = Arc::new(Mutex::new(None));
    let handle = mock_handle(gh);

    // Seed watch with run 101 as active
    {
        let mut w = watches.lock().await;
        let mut entry = WatchEntry {
            last_seen_run_id: 101,
            active_runs: HashMap::new(),
            failure_counts: HashMap::new(),
            last_build: None,
        };
        entry
            .active_runs
            .insert(101, make_active(RunStatus::InProgress));
        w.insert(key.clone(), entry);
    }

    let mut rx = handle.events.subscribe();
    let poller = make_repo_poller("alice/app", &watches, &config, &rate_limit, &handle);

    let changes = poller.poll_active_runs_batch().await;

    let w = watches.lock().await;
    let entry = &w[&key];
    // Run removed from active, recorded as last_build
    assert!(!entry.active_runs.contains_key(&101));
    assert_eq!(entry.last_build.as_ref().unwrap().run_id, 101);
    assert_eq!(entry.last_build.as_ref().unwrap().conclusion, "success");
    drop(w);

    // RunCompleted change returned
    assert_eq!(changes.len(), 1);
    for c in changes {
        handle.events.emit(c.into_event());
    }
    match rx.try_recv() {
        Ok(crate::events::WatchEvent::RunCompleted { conclusion, .. }) => {
            assert_eq!(conclusion, RunConclusion::Success);
        }
        other => panic!("expected RunCompleted, got {other:?}"),
    }

    handle.cancel.cancel();
}

#[tokio::test]
async fn poll_active_runs_emits_status_change() {
    let key = WatchKey::new("alice/app", "main");
    // Mock: run 101 changed from queued to in_progress (found in batch response)
    let runs = vec![make_run(101, RunStatus::InProgress, "")];
    let gh = MockGitHub::with_runs(runs);
    let watches: Watches = Arc::new(Mutex::new(HashMap::new()));
    let config: SharedConfig = Arc::new(Mutex::new(Config::default()));
    let rate_limit: RateLimitState = Arc::new(Mutex::new(None));
    let handle = mock_handle(gh);

    {
        let mut w = watches.lock().await;
        let mut entry = WatchEntry {
            last_seen_run_id: 101,
            active_runs: HashMap::new(),
            failure_counts: HashMap::new(),
            last_build: None,
        };
        entry
            .active_runs
            .insert(101, make_active(RunStatus::Queued));
        w.insert(key.clone(), entry);
    }

    let mut rx = handle.events.subscribe();
    let poller = make_repo_poller("alice/app", &watches, &config, &rate_limit, &handle);

    let changes = poller.poll_active_runs_batch().await;

    // Still active, status updated
    let w = watches.lock().await;
    assert_eq!(w[&key].active_runs[&101].status, RunStatus::InProgress);
    drop(w);

    // StatusChanged change returned
    assert_eq!(changes.len(), 1);
    for c in changes {
        handle.events.emit(c.into_event());
    }
    match rx.try_recv() {
        Ok(crate::events::WatchEvent::StatusChanged { from, to, .. }) => {
            assert_eq!(from, RunStatus::Queued);
            assert_eq!(to, RunStatus::InProgress);
        }
        other => panic!("expected StatusChanged, got {other:?}"),
    }

    handle.cancel.cancel();
}

#[tokio::test]
async fn poll_active_runs_fetches_failing_steps() {
    let key = WatchKey::new("alice/app", "main");
    // Mock: run 101 completed with failure (falls back to run_status since not in_progress)
    let runs = vec![make_run(101, RunStatus::Completed, "failure")];
    let gh = MockGitHub::with_runs_and_failures(runs, "Build / Run tests");
    let watches: Watches = Arc::new(Mutex::new(HashMap::new()));
    let config: SharedConfig = Arc::new(Mutex::new(Config::default()));
    let rate_limit: RateLimitState = Arc::new(Mutex::new(None));
    let handle = mock_handle(gh);

    {
        let mut w = watches.lock().await;
        let mut entry = WatchEntry {
            last_seen_run_id: 101,
            active_runs: HashMap::new(),
            failure_counts: HashMap::new(),
            last_build: None,
        };
        entry
            .active_runs
            .insert(101, make_active(RunStatus::InProgress));
        w.insert(key.clone(), entry);
    }

    let mut rx = handle.events.subscribe();
    let poller = make_repo_poller("alice/app", &watches, &config, &rate_limit, &handle);

    let changes = poller.poll_active_runs_batch().await;

    assert_eq!(changes.len(), 1);
    for c in changes {
        handle.events.emit(c.into_event());
    }
    match rx.try_recv() {
        Ok(crate::events::WatchEvent::RunCompleted {
            failing_steps,
            conclusion,
            ..
        }) => {
            assert_eq!(conclusion, RunConclusion::Failure);
            assert_eq!(failing_steps.as_deref(), Some("Build / Run tests"));
        }
        other => panic!("expected RunCompleted, got {other:?}"),
    }

    handle.cancel.cancel();
}

// -- Startup recovery --

#[tokio::test]
async fn check_for_new_runs_skips_already_active() {
    let key = WatchKey::new("alice/app", "main");
    // Run 101 is in_progress and already tracked as active.
    // A stale last_seen_run_id (100) should NOT cause 101 to be re-emitted.
    let runs = vec![
        make_run(100, RunStatus::Completed, "success"),
        make_run(101, RunStatus::InProgress, ""),
    ];
    let gh = MockGitHub::with_runs(runs);
    let watches: Watches = Arc::new(Mutex::new(HashMap::new()));
    let config: SharedConfig = Arc::new(Mutex::new(Config::default()));
    let rate_limit: RateLimitState = Arc::new(Mutex::new(None));
    let handle = mock_handle(gh);

    // Seed: last_seen=100, run 101 already in active_runs (e.g. from recovery).
    {
        let mut w = watches.lock().await;
        w.insert(
            key.clone(),
            WatchEntry {
                last_seen_run_id: 100,
                active_runs: HashMap::from([(101, make_active(RunStatus::InProgress))]),
                failure_counts: HashMap::new(),
                last_build: None,
            },
        );
    }

    let poller = make_repo_poller("alice/app", &watches, &config, &rate_limit, &handle);
    let changes = poller.check_for_new_runs_repo_wide().await;

    // No changes should be returned — run 101 was already being tracked.
    assert!(
        changes.is_empty(),
        "expected no changes for already-active run, got {} changes",
        changes.len()
    );

    handle.cancel.cancel();
}

#[tokio::test]
async fn record_completion_bumps_last_seen() {
    let mut entry = WatchEntry {
        last_seen_run_id: 50,
        active_runs: HashMap::from([(100, make_active(RunStatus::InProgress))]),
        failure_counts: HashMap::new(),
        last_build: None,
    };
    let run = make_run(100, RunStatus::Completed, "success");
    entry.record_completion(&run, None, None, 999);

    assert_eq!(
        entry.last_seen_run_id, 100,
        "record_completion should bump last_seen_run_id"
    );
    assert!(entry.active_runs.is_empty());
    assert!(entry.last_build.is_some());
}

#[tokio::test]
async fn recover_existing_watches_recovers_active_runs() {
    let key = WatchKey::new("alice/app", "main");
    // Mock returns a mix: 100 completed, 101 in_progress
    let runs = vec![
        make_run(100, RunStatus::Completed, "success"),
        make_run(101, RunStatus::InProgress, ""),
    ];
    let gh = MockGitHub::with_runs(runs);
    let watches: Watches = Arc::new(Mutex::new(HashMap::new()));
    let config: SharedConfig = Arc::new(Mutex::new(Config::default()));
    let rate_limit: RateLimitState = Arc::new(Mutex::new(None));
    let handle = mock_handle(gh);

    // Seed: persisted watch with last_seen=99, no active runs
    {
        let mut w = watches.lock().await;
        w.insert(
            key.clone(),
            WatchEntry {
                last_seen_run_id: 99,
                active_runs: HashMap::new(),
                failure_counts: HashMap::new(),
                last_build: None,
            },
        );
    }

    let snapshot = vec![key.clone()];
    startup::recover_existing_watches(&watches, &config, &handle, &rate_limit, &snapshot).await;

    let w = watches.lock().await;
    let entry = &w[&key];
    // In-progress run recovered
    assert!(entry.active_runs.contains_key(&101));
    // High-water mark bumped
    assert_eq!(entry.last_seen_run_id, 101);

    handle.cancel.cancel();
}

#[test]
fn run_change_dedup_keeps_first() {
    use crate::events::RunSnapshot;
    use std::collections::HashSet;

    let snap = |id| RunSnapshot {
        repo: "alice/app".to_string(),
        branch: "main".to_string(),
        run_id: id,
        workflow: "CI".to_string(),
        title: "Test".to_string(),
        event: "push".to_string(),
        status: RunStatus::InProgress,
        attempt: 1,
    };

    let changes = vec![
        RunChange::Completed {
            run: snap(101),
            conclusion: RunConclusion::Success,
            elapsed: Some(42.0),
            failing_steps: None,
            failing_job_id: None,
        },
        RunChange::Started { run: snap(102) },
        // Duplicate of 101 — should be suppressed
        RunChange::Completed {
            run: snap(101),
            conclusion: RunConclusion::Success,
            elapsed: None,
            failing_steps: None,
            failing_job_id: None,
        },
    ];

    let mut seen = HashSet::new();
    let emitted: Vec<_> = changes
        .into_iter()
        .filter(|c| seen.insert(c.run_id()))
        .collect();

    assert_eq!(emitted.len(), 2);
    assert_eq!(emitted[0].run_id(), 101);
    assert_eq!(emitted[1].run_id(), 102);

    // First-wins: the kept 101 has elapsed data
    match &emitted[0] {
        RunChange::Completed { elapsed, .. } => assert_eq!(*elapsed, Some(42.0)),
        other => panic!("expected Completed, got {other:?}"),
    }
}

// -- Re-run detection tests --

#[tokio::test]
async fn check_for_new_runs_detects_rerun_with_different_conclusion() {
    let key = WatchKey::new("alice/app", "main");
    // The API now returns run 200 as success (it was re-run after initially failing).
    let runs = vec![make_run(200, RunStatus::Completed, "success")];
    let gh = MockGitHub::with_runs(runs);
    let watches: Watches = Arc::new(Mutex::new(HashMap::new()));
    let config: SharedConfig = Arc::new(Mutex::new(Config::default()));
    let rate_limit: RateLimitState = Arc::new(Mutex::new(None));
    let handle = mock_handle(gh);

    // Seed watch with last_seen=200 and last_build=200 (failure).
    {
        let mut w = watches.lock().await;
        w.insert(
            key.clone(),
            WatchEntry {
                last_seen_run_id: 200,
                active_runs: HashMap::new(),
                failure_counts: HashMap::new(),
                last_build: Some(crate::github::LastBuild {
                    run_id: 200,
                    conclusion: "failure".to_string(),
                    workflow: "CI".to_string(),
                    title: "Test PR".to_string(),
                    head_sha: "abc1234".to_string(),
                    event: "push".to_string(),
                    failing_steps: Some("Build / Run tests".to_string()),
                    failing_job_id: None,
                    completed_at: Some(1000),
                    duration_secs: Some(60),
                    attempt: 1,
                }),
            },
        );
    }

    let poller = make_repo_poller("alice/app", &watches, &config, &rate_limit, &handle);
    let changes = poller.check_for_new_runs_repo_wide().await;

    // Should detect the re-run and emit a RunCompleted with new conclusion.
    assert_eq!(changes.len(), 1, "expected 1 change, got {changes:?}");
    match &changes[0] {
        RunChange::Completed {
            run, conclusion, ..
        } => {
            assert_eq!(run.run_id, 200);
            assert_eq!(*conclusion, RunConclusion::Success);
        }
        other => panic!("expected Completed, got {other:?}"),
    }

    // Verify last_build was updated to success.
    let w = watches.lock().await;
    let entry = &w[&key];
    let lb = entry.last_build.as_ref().unwrap();
    assert_eq!(lb.run_id, 200);
    assert_eq!(lb.conclusion, "success");

    handle.cancel.cancel();
}

#[tokio::test]
async fn check_for_new_runs_detects_rerun_in_progress() {
    let key = WatchKey::new("alice/app", "main");
    // The API now returns run 200 as in_progress (it was re-run).
    let runs = vec![make_run(200, RunStatus::InProgress, "")];
    let gh = MockGitHub::with_runs(runs);
    let watches: Watches = Arc::new(Mutex::new(HashMap::new()));
    let config: SharedConfig = Arc::new(Mutex::new(Config::default()));
    let rate_limit: RateLimitState = Arc::new(Mutex::new(None));
    let handle = mock_handle(gh);

    // Seed watch with last_seen=200 and last_build=200 (failure).
    {
        let mut w = watches.lock().await;
        w.insert(
            key.clone(),
            WatchEntry {
                last_seen_run_id: 200,
                active_runs: HashMap::new(),
                failure_counts: HashMap::new(),
                last_build: Some(crate::github::LastBuild {
                    run_id: 200,
                    conclusion: "failure".to_string(),
                    workflow: "CI".to_string(),
                    title: "Test PR".to_string(),
                    head_sha: "abc1234".to_string(),
                    event: "push".to_string(),
                    failing_steps: Some("Build / Run tests".to_string()),
                    failing_job_id: None,
                    completed_at: Some(1000),
                    duration_secs: Some(60),
                    attempt: 1,
                }),
            },
        );
    }

    let poller = make_repo_poller("alice/app", &watches, &config, &rate_limit, &handle);
    let changes = poller.check_for_new_runs_repo_wide().await;

    // Should detect the re-run in progress and emit RunStarted.
    assert_eq!(changes.len(), 1, "expected 1 change, got {changes:?}");
    assert_eq!(changes[0].run_id(), 200);
    match &changes[0] {
        RunChange::Started { run } => assert_eq!(run.run_id, 200),
        other => panic!("expected Started, got {other:?}"),
    }

    // Verify the run was added back to active_runs.
    let w = watches.lock().await;
    let entry = &w[&key];
    assert!(
        entry.active_runs.contains_key(&200),
        "run 200 should be tracked as active"
    );

    handle.cancel.cancel();
}

#[tokio::test]
async fn check_for_new_runs_ignores_rerun_with_same_conclusion() {
    let key = WatchKey::new("alice/app", "main");
    // The API returns run 200 as failure (same as what we already recorded).
    let runs = vec![make_run(200, RunStatus::Completed, "failure")];
    let gh = MockGitHub::with_runs(runs);
    let watches: Watches = Arc::new(Mutex::new(HashMap::new()));
    let config: SharedConfig = Arc::new(Mutex::new(Config::default()));
    let rate_limit: RateLimitState = Arc::new(Mutex::new(None));
    let handle = mock_handle(gh);

    {
        let mut w = watches.lock().await;
        w.insert(
            key.clone(),
            WatchEntry {
                last_seen_run_id: 200,
                active_runs: HashMap::new(),
                failure_counts: HashMap::new(),
                last_build: Some(crate::github::LastBuild {
                    run_id: 200,
                    conclusion: "failure".to_string(),
                    workflow: "CI".to_string(),
                    title: "Test PR".to_string(),
                    head_sha: "abc1234".to_string(),
                    event: "push".to_string(),
                    failing_steps: None,
                    failing_job_id: None,
                    completed_at: Some(1000),
                    duration_secs: None,
                    attempt: 1,
                }),
            },
        );
    }

    let poller = make_repo_poller("alice/app", &watches, &config, &rate_limit, &handle);
    let changes = poller.check_for_new_runs_repo_wide().await;

    // No changes — same conclusion, nothing to update.
    assert!(changes.is_empty(), "expected no changes, got {changes:?}");

    handle.cancel.cancel();
}
