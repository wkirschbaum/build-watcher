use super::*;

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::Mutex;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::github::{GhError, RunInfo};

use super::poller::{PollConfig, Poller};

fn make_run(id: u64, status: &str, conclusion: &str) -> RunInfo {
    RunInfo {
        id,
        status: status.to_string(),
        conclusion: conclusion.to_string(),
        title: "Test PR".to_string(),
        workflow: "CI".to_string(),
        head_sha: "abc1234".to_string(),
        event: "push".to_string(),
    }
}

fn make_active(status: &str) -> ActiveRun {
    ActiveRun {
        status: status.to_string(),
        started_at: Instant::now(),
        workflow: "CI".to_string(),
        title: "Test PR".to_string(),
        event: "push".to_string(),
    }
}

fn make_entry() -> WatchEntry {
    WatchEntry {
        last_seen_run_id: 100,
        active_runs: HashMap::from([
            (101, make_active("in_progress")),
            (102, make_active("queued")),
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
    let push = make_active("in_progress");
    assert_eq!(push.display_title(), "Test PR");

    let mut pr = make_active("queued");
    pr.event = "pull_request".to_string();
    assert_eq!(pr.display_title(), "PR: Test PR");
}

// -- WatchEntry state machine tests --

#[test]
fn record_completion_returns_elapsed() {
    let mut entry = make_entry();
    let run = make_run(101, "completed", "success");

    let elapsed = entry.record_completion(&run, None, 0);

    // Active run was present, so elapsed should be Some
    assert!(elapsed.is_some());
    // Should be very small since we just created it
    assert!(elapsed.unwrap() < std::time::Duration::from_secs(1));
}

#[test]
fn record_completion_returns_none_for_unknown_run() {
    let mut entry = make_entry();
    let run = make_run(999, "completed", "success");

    let elapsed = entry.record_completion(&run, None, 0);

    assert!(elapsed.is_none());
}

#[test]
fn record_completion_removes_run_and_sets_last_build() {
    let mut entry = make_entry();
    let run = make_run(101, "completed", "success");

    entry.record_completion(&run, None, 0);

    assert!(!entry.active_runs.contains_key(&101));
    assert!(entry.active_runs.contains_key(&102));
    let lb = entry.last_build.unwrap();
    assert_eq!(lb.run_id, 101);
    assert_eq!(lb.conclusion, "success");
}

#[test]
fn record_completion_stores_failing_steps() {
    let mut entry = make_entry();
    let run = make_run(101, "completed", "failure");

    entry.record_completion(&run, Some("Build / Run tests".to_string()), 0);

    let lb = entry.last_build.unwrap();
    assert_eq!(lb.failing_steps.as_deref(), Some("Build / Run tests"));
}

#[test]
fn record_completion_clears_failure_count() {
    let mut entry = make_entry();
    entry.failure_counts.insert(101, 3);
    let run = make_run(101, "completed", "failure");

    entry.record_completion(&run, None, 0);

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

    let old = entry.update_status(101, "queued");

    assert_eq!(old, Some("in_progress".to_string()));
    assert_eq!(entry.active_runs[&101].status, "queued");
}

#[test]
fn update_status_noop_when_same() {
    let mut entry = make_entry();

    let old = entry.update_status(101, "in_progress");

    assert!(old.is_none());
    assert_eq!(entry.active_runs[&101].status, "in_progress");
}

#[test]
fn update_status_noop_for_unknown_run() {
    let mut entry = make_entry();

    entry.update_status(999, "completed");

    assert!(!entry.active_runs.contains_key(&999));
}

#[test]
fn incorporate_new_runs_tracks_in_progress() {
    let mut entry = make_entry();
    let run = make_run(200, "in_progress", "");
    let new_runs: Vec<&RunInfo> = vec![&run];

    entry.incorporate_new_runs(&new_runs, Instant::now(), 0);

    assert_eq!(entry.last_seen_run_id, 200);
    assert_eq!(entry.active_runs[&200].status, "in_progress");
    assert!(entry.last_build.is_none());
}

#[test]
fn incorporate_new_runs_records_completed() {
    let mut entry = make_entry();
    let run = make_run(200, "completed", "success");
    let new_runs: Vec<&RunInfo> = vec![&run];

    entry.incorporate_new_runs(&new_runs, Instant::now(), 0);

    assert_eq!(entry.last_seen_run_id, 200);
    assert!(!entry.active_runs.contains_key(&200));
    assert_eq!(entry.last_build.unwrap().run_id, 200);
}

#[test]
fn incorporate_new_runs_newest_completed_wins_last_build() {
    let mut entry = make_entry();
    let old = make_run(200, "completed", "failure");
    let new = make_run(201, "completed", "success");
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
    let completed = make_run(200, "completed", "success");
    let active = make_run(201, "in_progress", "");
    let new_runs: Vec<&RunInfo> = vec![&active, &completed];

    entry.incorporate_new_runs(&new_runs, Instant::now(), 0);

    assert_eq!(entry.last_seen_run_id, 201);
    assert_eq!(entry.active_runs[&201].status, "in_progress");
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
    let run = make_run(101, "completed", "success");
    entry.record_completion(&run, None, 0);

    let persisted = entry.to_persisted();
    let restored = WatchEntry::from_persisted(persisted);

    assert_eq!(restored.last_seen_run_id, entry.last_seen_run_id);
    assert!(restored.active_runs.is_empty());
    assert!(restored.failure_counts.is_empty());
    assert_eq!(restored.last_build.unwrap().run_id, 101);
}

// -- filter_runs tests --

#[test]
fn filter_runs_no_filters() {
    let runs = vec![
        make_run(1, "completed", "success"),
        make_run(2, "in_progress", ""),
    ];
    assert_eq!(filter_runs(&runs, &[], &[]).len(), 2);
}

#[test]
fn filter_runs_workflow_allowlist() {
    let mut r1 = make_run(1, "completed", "success");
    r1.workflow = "CI".to_string();
    let mut r2 = make_run(2, "completed", "success");
    r2.workflow = "Deploy".to_string();
    let runs = vec![r1, r2];

    let filtered = filter_runs(&runs, &["ci".to_string()], &[]);
    assert_eq!(filtered.len(), 1);
    assert_eq!(filtered[0].workflow, "CI");
}

#[test]
fn filter_runs_ignored_workflows() {
    let mut r1 = make_run(1, "completed", "success");
    r1.workflow = "CI".to_string();
    let mut r2 = make_run(2, "completed", "success");
    r2.workflow = "Semgrep".to_string();
    let runs = vec![r1, r2];

    let filtered = filter_runs(&runs, &[], &["semgrep".to_string()]);
    assert_eq!(filtered.len(), 1);
    assert_eq!(filtered[0].workflow, "CI");
}

#[test]
fn filter_runs_both_filters() {
    let mut r1 = make_run(1, "completed", "success");
    r1.workflow = "CI".to_string();
    let mut r2 = make_run(2, "completed", "success");
    r2.workflow = "Deploy".to_string();
    let mut r3 = make_run(3, "completed", "success");
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
    let run = make_run(200, "completed", "failure");
    entry.record_completion(&run, None, 0);
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
    let run = make_run(200, "completed", "success");
    entry.record_completion(&run, None, 0);
    watches.insert(WatchKey::new("alice/app", "main"), entry);

    assert!(last_failed_build(&watches, "alice/app").is_none());
}

#[test]
fn last_failed_build_picks_most_recent() {
    let mut watches = HashMap::new();

    let mut entry1 = make_entry();
    let run1 = make_run(100, "completed", "failure");
    entry1.record_completion(&run1, None, 0);
    watches.insert(WatchKey::new("alice/app", "main"), entry1);

    let mut entry2 = make_entry();
    let run2 = make_run(200, "completed", "failure");
    entry2.record_completion(&run2, None, 0);
    watches.insert(WatchKey::new("alice/app", "develop"), entry2);

    let (_, build) = last_failed_build(&watches, "alice/app").unwrap();
    assert_eq!(build.run_id, 200);
}

#[test]
fn last_failed_build_ignores_other_repos() {
    let mut watches = HashMap::new();
    let mut entry = make_entry();
    let run = make_run(200, "completed", "failure");
    entry.record_completion(&run, None, 0);
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
        active_runs.insert(id, make_active("in_progress"));
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

    // 2 base calls (one per watch) + 3 active run calls = 5
    assert_eq!(count_api_calls(&watches), 5);
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
    async fn run_status(&self, _: &str, run_id: u64) -> Result<RunInfo, GhError> {
        self.runs
            .iter()
            .find(|r| r.id == run_id)
            .cloned()
            .ok_or(GhError::MissingFields {
                repo: "mock".into(),
            })
    }
    async fn failing_steps(&self, _: &str, _: u64) -> Option<String> {
        self.failure_msg.clone()
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

fn make_poller(
    key: &WatchKey,
    watches: &Watches,
    config: &SharedConfig,
    rate_limit: &RateLimitState,
    handle: &WatcherHandle,
) -> Poller {
    Poller {
        key: key.clone(),
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
        make_run(100, "completed", "success"),
        make_run(101, "in_progress", ""),
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
    let runs = vec![make_run(100, "completed", "success")];
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

// -- Poller: check_for_new_runs --

#[tokio::test]
async fn check_for_new_runs_detects_new_builds() {
    let key = WatchKey::new("alice/app", "main");
    // Mock returns runs 99-102; watch has seen up to 100
    let runs = vec![
        make_run(99, "completed", "success"),
        make_run(100, "completed", "success"),
        make_run(101, "in_progress", ""),
        make_run(102, "completed", "failure"),
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
    let poller = make_poller(&key, &watches, &config, &rate_limit, &handle);
    let pcfg = PollConfig {
        active_secs: 15,
        idle_secs: 60,
        workflows: vec![],
        ignored: vec![],
    };

    poller.check_for_new_runs("alice/app", "main", &pcfg).await;

    // Verify high-water mark advanced
    let w = watches.lock().await;
    let entry = &w[&key];
    assert_eq!(entry.last_seen_run_id, 102);
    // 101 is in_progress → tracked as active
    assert!(entry.active_runs.contains_key(&101));
    // 102 is completed → last_build
    assert_eq!(entry.last_build.as_ref().unwrap().run_id, 102);
    drop(w);

    // Verify events: RunStarted for 101 and 102, RunCompleted for 102
    let mut events = vec![];
    while let Ok(e) = rx.try_recv() {
        events.push(e);
    }
    assert!(
        events.len() >= 3,
        "expected ≥3 events, got {}",
        events.len()
    );

    handle.cancel.cancel();
}

#[tokio::test]
async fn check_for_new_runs_applies_workflow_filter() {
    let key = WatchKey::new("alice/app", "main");
    let mut ci = make_run(101, "in_progress", "");
    ci.workflow = "CI".to_string();
    let mut semgrep = make_run(102, "in_progress", "");
    semgrep.workflow = "Semgrep".to_string();

    let gh = MockGitHub::with_runs(vec![ci, semgrep]);
    let watches: Watches = Arc::new(Mutex::new(HashMap::new()));
    let config: SharedConfig = Arc::new(Mutex::new(Config::default()));
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

    let poller = make_poller(&key, &watches, &config, &rate_limit, &handle);
    let pcfg = PollConfig {
        active_secs: 15,
        idle_secs: 60,
        workflows: vec![],
        ignored: vec!["Semgrep".to_string()],
    };

    poller.check_for_new_runs("alice/app", "main", &pcfg).await;

    let w = watches.lock().await;
    let entry = &w[&key];
    // CI tracked, Semgrep filtered out
    assert!(entry.active_runs.contains_key(&101));
    assert!(!entry.active_runs.contains_key(&102));
    // High-water mark includes filtered runs
    assert_eq!(entry.last_seen_run_id, 102);

    handle.cancel.cancel();
}

// -- Poller: poll_active_runs --

#[tokio::test]
async fn poll_active_runs_detects_completion() {
    let key = WatchKey::new("alice/app", "main");
    // Mock: run 101 now completed
    let runs = vec![make_run(101, "completed", "success")];
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
        entry.active_runs.insert(101, make_active("in_progress"));
        w.insert(key.clone(), entry);
    }

    let mut rx = handle.events.subscribe();
    let poller = make_poller(&key, &watches, &config, &rate_limit, &handle);

    poller.poll_active_runs("alice/app", "main").await;

    let w = watches.lock().await;
    let entry = &w[&key];
    // Run removed from active, recorded as last_build
    assert!(!entry.active_runs.contains_key(&101));
    assert_eq!(entry.last_build.as_ref().unwrap().run_id, 101);
    assert_eq!(entry.last_build.as_ref().unwrap().conclusion, "success");
    drop(w);

    // RunCompleted event emitted
    match rx.try_recv() {
        Ok(crate::events::WatchEvent::RunCompleted { conclusion, .. }) => {
            assert_eq!(conclusion, "success");
        }
        other => panic!("expected RunCompleted, got {other:?}"),
    }

    handle.cancel.cancel();
}

#[tokio::test]
async fn poll_active_runs_emits_status_change() {
    let key = WatchKey::new("alice/app", "main");
    // Mock: run 101 changed from queued to in_progress
    let runs = vec![make_run(101, "in_progress", "")];
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
        entry.active_runs.insert(101, make_active("queued"));
        w.insert(key.clone(), entry);
    }

    let mut rx = handle.events.subscribe();
    let poller = make_poller(&key, &watches, &config, &rate_limit, &handle);

    poller.poll_active_runs("alice/app", "main").await;

    // Still active, status updated
    let w = watches.lock().await;
    assert_eq!(w[&key].active_runs[&101].status, "in_progress");
    drop(w);

    // StatusChanged event
    match rx.try_recv() {
        Ok(crate::events::WatchEvent::StatusChanged { from, to, .. }) => {
            assert_eq!(from, "queued");
            assert_eq!(to, "in_progress");
        }
        other => panic!("expected StatusChanged, got {other:?}"),
    }

    handle.cancel.cancel();
}

#[tokio::test]
async fn poll_active_runs_fetches_failing_steps() {
    let key = WatchKey::new("alice/app", "main");
    let runs = vec![make_run(101, "completed", "failure")];
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
        entry.active_runs.insert(101, make_active("in_progress"));
        w.insert(key.clone(), entry);
    }

    let mut rx = handle.events.subscribe();
    let poller = make_poller(&key, &watches, &config, &rate_limit, &handle);

    poller.poll_active_runs("alice/app", "main").await;

    match rx.try_recv() {
        Ok(crate::events::WatchEvent::RunCompleted {
            failing_steps,
            conclusion,
            ..
        }) => {
            assert_eq!(conclusion, "failure");
            assert_eq!(failing_steps.as_deref(), Some("Build / Run tests"));
        }
        other => panic!("expected RunCompleted, got {other:?}"),
    }

    handle.cancel.cancel();
}

// -- Startup recovery --

#[tokio::test]
async fn recover_existing_watches_recovers_active_runs() {
    let key = WatchKey::new("alice/app", "main");
    // Mock returns a mix: 100 completed, 101 in_progress
    let runs = vec![
        make_run(100, "completed", "success"),
        make_run(101, "in_progress", ""),
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
