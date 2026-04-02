use super::*;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::config::{Config, ConfigManager, ConfigPersistence};
use crate::events::WatchEvent;
use crate::github::{GhError, RunInfo};
use crate::status::{RunConclusion, RunStatus};

use super::repo_poller::{RepoPoller, RunChange};

// -- Test helpers --

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
        created_at: "2026-01-01T10:00:00Z".to_string(),
        updated_at: "2026-01-01T10:05:00Z".to_string(),
        url: "https://github.com/test/repo/actions/runs/1".to_string(),
    }
}

fn make_active(status: RunStatus) -> ActiveRun {
    ActiveRun {
        status,
        workflow: "CI".to_string(),
        title: "Test PR".to_string(),
        event: "push".to_string(),
        attempt: 1,
        created_at: "2026-01-01T10:00:00Z".to_string(),
        updated_at: "2026-01-01T10:05:00Z".to_string(),
        url: "https://github.com/test/repo/actions/runs/1".to_string(),
        actor: None,
        commit_author: None,
    }
}

fn make_entry() -> WatchEntry {
    WatchEntry {
        last_seen_run_id: 100,
        active_runs: HashMap::from([
            (101, make_active(RunStatus::InProgress)),
            (102, make_active(RunStatus::Queued)),
        ]),
        ..Default::default()
    }
}

fn idle_entry(last_seen: u64) -> WatchEntry {
    WatchEntry {
        last_seen_run_id: last_seen,
        ..Default::default()
    }
}

fn make_last_build(run_id: u64, conclusion: &str) -> crate::github::LastBuild {
    crate::github::LastBuild {
        run_id,
        conclusion: conclusion.to_string(),
        workflow: "CI".to_string(),
        title: "Test PR".to_string(),
        head_sha: "abc1234".to_string(),
        event: "push".to_string(),
        failing_steps: None,
        failing_job_id: None,
        completed_at: Some(1000),
        duration_secs: Some(300),
        attempt: 1,
        url: "https://github.com/test/repo/actions/runs/1".to_string(),
        actor: None,
        commit_author: None,
    }
}

// -- Mock GitHub client --

#[derive(Default)]
struct MockGitHub {
    runs: Vec<RunInfo>,
    failure_msg: Option<String>,
    prs: Vec<crate::github::PrInfo>,
}

impl MockGitHub {
    fn with_runs(runs: Vec<RunInfo>) -> Arc<dyn crate::github::GitHubClient> {
        Arc::new(Self {
            runs,
            ..Default::default()
        })
    }

    fn with_runs_and_failures(
        runs: Vec<RunInfo>,
        failure_msg: &str,
    ) -> Arc<dyn crate::github::GitHubClient> {
        Arc::new(Self {
            runs,
            failure_msg: Some(failure_msg.to_string()),
            ..Default::default()
        })
    }

    fn with_prs(prs: Vec<crate::github::PrInfo>) -> Arc<dyn crate::github::GitHubClient> {
        Arc::new(Self {
            prs,
            ..Default::default()
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
    async fn list_tags(&self, _: &str) -> Result<Vec<String>, GhError> {
        Ok(vec![])
    }
    async fn list_branches(&self, _: &str) -> Result<Vec<String>, GhError> {
        // Return branch names from the runs so tests behave as before.
        let branches: HashSet<String> = self.runs.iter().map(|r| r.head_branch.clone()).collect();
        Ok(branches.into_iter().collect())
    }
    async fn default_branch(&self, _: &str) -> Result<String, GhError> {
        Ok("main".to_string())
    }
    async fn open_prs(&self, _: &str) -> Result<Vec<crate::github::PrInfo>, GhError> {
        Ok(self.prs.clone())
    }
    async fn pr_merge(&self, _: &str, _: u64) -> Result<String, GhError> {
        Ok("Merged".to_string())
    }
    async fn run_author(&self, _: &str, _: u64) -> Option<crate::github::RunAuthorInfo> {
        None
    }
}

// -- Test harness for async integration tests --

struct TestHarness {
    watches: Watches,
    config: SharedConfig,
    rate_limit: RateLimitState,
    handle: WatcherHandle,
}

impl TestHarness {
    fn new(gh: Arc<dyn crate::github::GitHubClient>) -> Self {
        Self::with_config(gh, Config::default())
    }

    fn with_config(gh: Arc<dyn crate::github::GitHubClient>, cfg: Config) -> Self {
        let handle = WatcherHandle::new(
            CancellationToken::new(),
            crate::events::EventBus::new(),
            gh,
            Arc::new(crate::persistence::NullPersistence),
            Arc::new(Mutex::new(HashMap::new())),
            Arc::new(tokio::sync::Notify::new()),
        );
        Self {
            watches: Arc::new(Mutex::new(HashMap::new())),
            config: Arc::new(ConfigManager::new(cfg, ConfigPersistence::Null)),
            rate_limit: Arc::new(Mutex::new(None)),
            handle,
        }
    }

    async fn seed(&self, key: WatchKey, entry: WatchEntry) {
        self.watches.lock().await.insert(key, entry);
    }

    fn poller(&self, repo: &str) -> RepoPoller {
        RepoPoller {
            repo: repo.to_string(),
            watches: self.watches.clone(),
            config: self.config.clone(),
            rate_limit: self.rate_limit.clone(),
            token: self.handle.cancel.child_token(),
            events: self.handle.events.clone(),
            github: self.handle.github.clone(),
            persistence: self.handle.persistence.clone(),
            history: self.handle.history.clone(),
            config_changed: self.handle.config_changed.clone(),
            last_active_secs: 0,
            first_poll: false,
            pr_states: HashMap::new(),
        }
    }

    async fn entry(&self, key: &WatchKey) -> WatchEntry {
        self.watches.lock().await[key].clone()
    }

    fn subscribe(&self) -> tokio::sync::broadcast::Receiver<WatchEvent> {
        self.handle.events.subscribe()
    }

    fn cancel(&self) {
        self.handle.cancel.cancel();
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
fn record_completion_removes_active_run() {
    let mut entry = make_entry();
    assert!(entry.active_runs.contains_key(&101));
    let run = make_run(101, RunStatus::Completed, "success");
    entry.record_completion(&run, None, None);
    assert!(!entry.active_runs.contains_key(&101));
}

#[test]
fn record_completion_noop_for_unknown_run() {
    let mut entry = make_entry();
    let run = make_run(999, RunStatus::Completed, "success");
    entry.record_completion(&run, None, None);
    // Still has the original active runs
    assert!(entry.active_runs.contains_key(&101));
}

#[test]
fn record_completion_removes_run_and_sets_last_build() {
    let mut entry = make_entry();
    let run = make_run(101, RunStatus::Completed, "success");
    entry.record_completion(&run, None, None);

    assert!(!entry.active_runs.contains_key(&101));
    assert!(entry.active_runs.contains_key(&102));
    let lb = entry.last_builds.get("CI").unwrap();
    assert_eq!(lb.run_id, 101);
    assert_eq!(lb.conclusion, "success");
}

#[test]
fn record_completion_stores_failing_steps() {
    let mut entry = make_entry();
    let run = make_run(101, RunStatus::Completed, "failure");
    entry.record_completion(&run, Some("Build / Run tests".to_string()), None);
    assert_eq!(
        entry
            .last_builds
            .get("CI")
            .unwrap()
            .failing_steps
            .as_deref(),
        Some("Build / Run tests")
    );
}

#[test]
fn record_completion_clears_failure_count() {
    let mut entry = make_entry();
    entry.failure_counts.insert(101, 3);
    let run = make_run(101, RunStatus::Completed, "failure");
    entry.record_completion(&run, None, None);
    assert!(!entry.failure_counts.contains_key(&101));
}

#[test]
fn record_failure_increments_count() {
    let mut entry = make_entry();
    let error = GhError::Timeout {
        repo: "test".to_string(),
        timeout_secs: 30,
    };
    assert!(!entry.record_failure(101, &error));
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
    assert!(entry.record_failure(101, &error));
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
    assert_eq!(
        entry.update_status(101, &RunStatus::Queued),
        Some(RunStatus::InProgress)
    );
    assert_eq!(entry.active_runs[&101].status, RunStatus::Queued);
}

#[test]
fn update_status_noop_when_same() {
    let mut entry = make_entry();
    assert!(entry.update_status(101, &RunStatus::InProgress).is_none());
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
    entry.incorporate_new_runs(&[&run]);

    assert_eq!(entry.last_seen_run_id, 200);
    assert_eq!(entry.active_runs[&200].status, RunStatus::InProgress);
    assert!(entry.last_builds.is_empty());
}

#[test]
fn incorporate_new_runs_records_completed() {
    let mut entry = make_entry();
    let run = make_run(200, RunStatus::Completed, "success");
    entry.incorporate_new_runs(&[&run]);

    assert_eq!(entry.last_seen_run_id, 200);
    assert!(!entry.active_runs.contains_key(&200));
    assert_eq!(entry.last_builds.get("CI").unwrap().run_id, 200);
}

#[test]
fn incorporate_new_runs_newest_completed_wins_last_build() {
    let mut entry = make_entry();
    let old = make_run(200, RunStatus::Completed, "failure");
    let new = make_run(201, RunStatus::Completed, "success");
    entry.incorporate_new_runs(&[&new, &old]);

    assert_eq!(entry.last_seen_run_id, 201);
    let lb = entry.last_builds.get("CI").unwrap();
    assert_eq!(lb.run_id, 201);
    assert_eq!(lb.conclusion, "success");
}

#[test]
fn incorporate_new_runs_mixed_statuses() {
    let mut entry = make_entry();
    let completed = make_run(200, RunStatus::Completed, "success");
    let active = make_run(201, RunStatus::InProgress, "");
    entry.incorporate_new_runs(&[&active, &completed]);

    assert_eq!(entry.last_seen_run_id, 201);
    assert_eq!(entry.active_runs[&201].status, RunStatus::InProgress);
    assert!(!entry.active_runs.contains_key(&200));
    assert_eq!(entry.last_builds.get("CI").unwrap().run_id, 200);
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
    entry.record_completion(&run, None, None);

    let restored = WatchEntry::from_persisted(entry.to_persisted());
    assert_eq!(restored.last_seen_run_id, 101);
    assert!(restored.active_runs.is_empty());
    assert!(restored.failure_counts.is_empty());
    assert_eq!(restored.last_builds.get("CI").unwrap().run_id, 101);
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

    assert_eq!(runs_for_branch(&runs, "main").len(), 2);
    assert_eq!(runs_for_branch(&runs, "develop").len(), 1);
    assert!(runs_for_branch(&runs, "feature/xyz").is_empty());
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

    let ignored = ["semgrep".to_string()];
    let filters = [IgnoreFilter {
        field: |r| &r.workflow,
        ignored: &ignored,
    }];
    let filtered = filter_runs(&runs, &[], &filters);
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

    let ignored = ["Semgrep".to_string()];
    let filters = [IgnoreFilter {
        field: |r| &r.workflow,
        ignored: &ignored,
    }];
    let filtered = filter_runs(&runs, &["CI".to_string(), "Deploy".to_string()], &filters);
    assert_eq!(filtered.len(), 2);
}

#[test]
fn filter_runs_multiple_ignore_dimensions() {
    let mut r1 = make_run(1, RunStatus::Completed, "success");
    r1.workflow = "CI".to_string();
    r1.event = "push".to_string();
    let mut r2 = make_run(2, RunStatus::Completed, "success");
    r2.workflow = "CI".to_string();
    r2.event = "schedule".to_string();
    let mut r3 = make_run(3, RunStatus::Completed, "success");
    r3.workflow = "Semgrep".to_string();
    r3.event = "push".to_string();
    let runs = vec![r1, r2, r3];

    let ignored_workflows = ["Semgrep".to_string()];
    let ignored_events = ["schedule".to_string()];
    let filters = [
        IgnoreFilter {
            field: |r| &r.workflow,
            ignored: &ignored_workflows,
        },
        IgnoreFilter {
            field: |r| &r.event,
            ignored: &ignored_events,
        },
    ];
    let filtered = filter_runs(&runs, &[], &filters);
    assert_eq!(filtered.len(), 1);
    assert_eq!(filtered[0].id, 1);
}

// -- last_failed_build tests --

#[test]
fn last_failed_build_finds_failure() {
    let mut watches = HashMap::new();
    let mut entry = make_entry();
    entry.record_completion(&make_run(200, RunStatus::Completed, "failure"), None, None);
    watches.insert(WatchKey::new("alice/app", "main"), entry);

    let (key, build) = last_failed_build(&watches, "alice/app").unwrap();
    assert_eq!(key.repo, "alice/app");
    assert_eq!(build.run_id, 200);
}

#[test]
fn last_failed_build_ignores_success() {
    let mut watches = HashMap::new();
    let mut entry = make_entry();
    entry.record_completion(&make_run(200, RunStatus::Completed, "success"), None, None);
    watches.insert(WatchKey::new("alice/app", "main"), entry);
    assert!(last_failed_build(&watches, "alice/app").is_none());
}

#[test]
fn last_failed_build_picks_most_recent() {
    let mut watches = HashMap::new();

    let mut entry1 = make_entry();
    entry1.record_completion(&make_run(100, RunStatus::Completed, "failure"), None, None);
    watches.insert(WatchKey::new("alice/app", "main"), entry1);

    let mut entry2 = make_entry();
    entry2.record_completion(&make_run(200, RunStatus::Completed, "failure"), None, None);
    watches.insert(WatchKey::new("alice/app", "develop"), entry2);

    assert_eq!(
        last_failed_build(&watches, "alice/app").unwrap().1.run_id,
        200
    );
}

#[test]
fn last_failed_build_ignores_other_repos() {
    let mut watches = HashMap::new();
    let mut entry = make_entry();
    entry.record_completion(&make_run(200, RunStatus::Completed, "failure"), None, None);
    watches.insert(WatchKey::new("bob/other", "main"), entry);
    assert!(last_failed_build(&watches, "alice/app").is_none());
}

// -- count_api_calls --

#[test]
fn count_api_calls_reflects_active_runs() {
    let mut watches = HashMap::new();
    let mut active_runs = HashMap::new();
    for id in 1..=3 {
        active_runs.insert(id, make_active(RunStatus::InProgress));
    }
    watches.insert(
        WatchKey::new("owner/repo1", "main"),
        WatchEntry {
            active_runs,
            ..idle_entry(100)
        },
    );
    watches.insert(WatchKey::new("owner/repo2", "main"), idle_entry(100));

    // 2 unique repos = 2 base calls, repo1 has active runs = +1 batch call = 3
    assert_eq!(count_api_calls(&watches), 3);
}

#[test]
fn count_api_calls_same_repo_multiple_branches() {
    let mut watches = HashMap::new();
    watches.insert(
        WatchKey::new("owner/repo1", "main"),
        WatchEntry {
            active_runs: HashMap::from([(1, make_active(RunStatus::InProgress))]),
            ..idle_entry(100)
        },
    );
    watches.insert(WatchKey::new("owner/repo1", "develop"), idle_entry(100));

    // 1 unique repo = 1 base call, has active runs = +1 batch call = 2
    assert_eq!(count_api_calls(&watches), 2);
}

#[test]
fn count_api_calls_empty_watches() {
    assert_eq!(count_api_calls(&HashMap::new()), 0);
}

// -- scaled_repo_limit --

#[test]
fn scaled_repo_limit_uses_default_for_few_branches() {
    use super::scaled_repo_limit;
    use crate::github::DEFAULT_REPO_LIMIT;
    assert_eq!(scaled_repo_limit(1), DEFAULT_REPO_LIMIT);
    assert_eq!(scaled_repo_limit(6), DEFAULT_REPO_LIMIT);
}

#[test]
fn scaled_repo_limit_scales_with_branches() {
    use super::scaled_repo_limit;
    assert_eq!(scaled_repo_limit(10), 30);
    assert_eq!(scaled_repo_limit(50), 150);
}

#[test]
fn scaled_repo_limit_caps_at_max() {
    use super::scaled_repo_limit;
    assert_eq!(scaled_repo_limit(100), 200);
    assert_eq!(scaled_repo_limit(500), 200);
}

// -- Integration tests using TestHarness --

#[tokio::test]
async fn start_watch_with_mock_github() {
    let runs = vec![
        make_run(100, RunStatus::Completed, "success"),
        make_run(101, RunStatus::InProgress, ""),
    ];
    let h = TestHarness::new(MockGitHub::with_runs(runs));

    let result = start_watch(
        &h.watches,
        &h.config,
        &h.handle,
        &h.rate_limit,
        "alice/app",
        "main",
    )
    .await;
    assert!(result.is_ok());

    // start_watch inserts a waiting entry; the poller handles initial data on its first cycle.
    let entry = h.entry(&WatchKey::new("alice/app", "main")).await;
    assert!(entry.waiting);
    assert_eq!(entry.last_seen_run_id, 0);

    h.cancel();
}

#[tokio::test]
async fn start_watch_registers_idle_watch_for_empty_runs() {
    let h = TestHarness::new(MockGitHub::with_runs(vec![]));

    let result = start_watch(
        &h.watches,
        &h.config,
        &h.handle,
        &h.rate_limit,
        "alice/app",
        "main",
    )
    .await;
    // start_watch now inserts a waiting entry; the poller discovers empty runs on first cycle.
    assert!(result.unwrap().contains("watching"));
    assert!(
        h.watches
            .lock()
            .await
            .contains_key(&WatchKey::new("alice/app", "main"))
    );
}

#[tokio::test]
async fn start_watch_deduplicates() {
    let runs = vec![make_run(100, RunStatus::Completed, "success")];
    let h = TestHarness::new(MockGitHub::with_runs(runs));

    let r1 = start_watch(
        &h.watches,
        &h.config,
        &h.handle,
        &h.rate_limit,
        "alice/app",
        "main",
    )
    .await;
    assert!(r1.is_ok());

    let r2 = start_watch(
        &h.watches,
        &h.config,
        &h.handle,
        &h.rate_limit,
        "alice/app",
        "main",
    )
    .await;
    assert!(r2.unwrap().contains("already being watched"));

    h.cancel();
}

// -- RepoPoller: check_for_new_runs_repo_wide --

#[tokio::test]
async fn check_for_new_runs_detects_new_builds() {
    let key = WatchKey::new("alice/app", "main");
    let runs = vec![
        make_run(99, RunStatus::Completed, "success"),
        make_run(100, RunStatus::Completed, "success"),
        make_run(101, RunStatus::InProgress, ""),
        make_run(102, RunStatus::Completed, "failure"),
    ];
    let h = TestHarness::new(MockGitHub::with_runs_and_failures(
        runs,
        "Build / Run tests",
    ));
    h.seed(key.clone(), idle_entry(100)).await;

    let mut rx = h.subscribe();
    let changes = h.poller("alice/app").check_for_new_runs_repo_wide().await;

    let entry = h.entry(&key).await;
    assert_eq!(entry.last_seen_run_id, 102);
    assert!(entry.active_runs.contains_key(&101));
    assert_eq!(entry.last_builds.get("CI").unwrap().run_id, 102);

    assert_eq!(changes.len(), 2);
    assert_eq!(changes[0].run_id(), 101);
    assert_eq!(changes[1].run_id(), 102);

    for c in changes {
        h.handle.events.emit(c.into_event());
    }
    let mut events = vec![];
    while let Ok(e) = rx.try_recv() {
        events.push(e);
    }
    assert!(matches!(&events[0], WatchEvent::RunStarted(s) if s.run_id == 101));
    assert!(matches!(&events[1], WatchEvent::RunCompleted { run, .. } if run.run_id == 102));

    h.cancel();
}

#[tokio::test]
async fn check_for_new_runs_applies_workflow_filter() {
    let key = WatchKey::new("alice/app", "main");
    let mut ci = make_run(101, RunStatus::InProgress, "");
    ci.workflow = "CI".to_string();
    let mut semgrep = make_run(102, RunStatus::InProgress, "");
    semgrep.workflow = "Semgrep".to_string();

    let mut cfg = Config::default();
    cfg.ignored_workflows = vec!["Semgrep".to_string()];
    let h = TestHarness::with_config(MockGitHub::with_runs(vec![ci, semgrep]), cfg);
    h.seed(key.clone(), idle_entry(100)).await;

    h.poller("alice/app").check_for_new_runs_repo_wide().await;

    let entry = h.entry(&key).await;
    assert!(entry.active_runs.contains_key(&101));
    assert!(!entry.active_runs.contains_key(&102));
    assert_eq!(entry.last_seen_run_id, 102);

    h.cancel();
}

// -- RepoPoller: poll_active_runs_batch --

#[tokio::test]
async fn poll_active_runs_detects_completion() {
    let key = WatchKey::new("alice/app", "main");
    let runs = vec![make_run(101, RunStatus::Completed, "success")];
    let h = TestHarness::new(MockGitHub::with_runs(runs));
    h.seed(
        key.clone(),
        WatchEntry {
            active_runs: HashMap::from([(101, make_active(RunStatus::InProgress))]),
            ..idle_entry(101)
        },
    )
    .await;

    let mut rx = h.subscribe();
    let changes = h.poller("alice/app").poll_active_runs_batch().await;

    let entry = h.entry(&key).await;
    assert!(!entry.active_runs.contains_key(&101));
    let lb = entry.last_builds.get("CI").unwrap();
    assert_eq!(lb.run_id, 101);
    assert_eq!(lb.conclusion, "success");

    assert_eq!(changes.len(), 1);
    for c in changes {
        h.handle.events.emit(c.into_event());
    }
    match rx.try_recv() {
        Ok(WatchEvent::RunCompleted { conclusion, .. }) => {
            assert_eq!(conclusion, RunConclusion::Success);
        }
        other => panic!("expected RunCompleted, got {other:?}"),
    }

    h.cancel();
}

#[tokio::test]
async fn poll_active_runs_emits_status_change() {
    let key = WatchKey::new("alice/app", "main");
    let runs = vec![make_run(101, RunStatus::InProgress, "")];
    let h = TestHarness::new(MockGitHub::with_runs(runs));
    h.seed(
        key.clone(),
        WatchEntry {
            active_runs: HashMap::from([(101, make_active(RunStatus::Queued))]),
            ..idle_entry(101)
        },
    )
    .await;

    let mut rx = h.subscribe();
    let changes = h.poller("alice/app").poll_active_runs_batch().await;

    assert_eq!(
        h.entry(&key).await.active_runs[&101].status,
        RunStatus::InProgress
    );

    assert_eq!(changes.len(), 1);
    for c in changes {
        h.handle.events.emit(c.into_event());
    }
    match rx.try_recv() {
        Ok(WatchEvent::StatusChanged { from, to, .. }) => {
            assert_eq!(from, RunStatus::Queued);
            assert_eq!(to, RunStatus::InProgress);
        }
        other => panic!("expected StatusChanged, got {other:?}"),
    }

    h.cancel();
}

#[tokio::test]
async fn poll_active_runs_fetches_failing_steps() {
    let key = WatchKey::new("alice/app", "main");
    let runs = vec![make_run(101, RunStatus::Completed, "failure")];
    let h = TestHarness::new(MockGitHub::with_runs_and_failures(
        runs,
        "Build / Run tests",
    ));
    h.seed(
        key.clone(),
        WatchEntry {
            active_runs: HashMap::from([(101, make_active(RunStatus::InProgress))]),
            ..idle_entry(101)
        },
    )
    .await;

    let mut rx = h.subscribe();
    let changes = h.poller("alice/app").poll_active_runs_batch().await;

    assert_eq!(changes.len(), 1);
    for c in changes {
        h.handle.events.emit(c.into_event());
    }
    match rx.try_recv() {
        Ok(WatchEvent::RunCompleted {
            failing_steps,
            conclusion,
            ..
        }) => {
            assert_eq!(conclusion, RunConclusion::Failure);
            assert_eq!(failing_steps.as_deref(), Some("Build / Run tests"));
        }
        other => panic!("expected RunCompleted, got {other:?}"),
    }

    h.cancel();
}

// -- Startup recovery --

#[tokio::test]
async fn check_for_new_runs_skips_already_active() {
    let key = WatchKey::new("alice/app", "main");
    let runs = vec![
        make_run(100, RunStatus::Completed, "success"),
        make_run(101, RunStatus::InProgress, ""),
    ];
    let h = TestHarness::new(MockGitHub::with_runs(runs));
    h.seed(
        key.clone(),
        WatchEntry {
            active_runs: HashMap::from([(101, make_active(RunStatus::InProgress))]),
            ..idle_entry(100)
        },
    )
    .await;

    let changes = h.poller("alice/app").check_for_new_runs_repo_wide().await;
    assert!(
        changes.is_empty(),
        "expected no changes for already-active run"
    );

    h.cancel();
}

#[tokio::test]
async fn record_completion_bumps_last_seen() {
    let mut entry = WatchEntry {
        active_runs: HashMap::from([(100, make_active(RunStatus::InProgress))]),
        ..idle_entry(50)
    };
    let run = make_run(100, RunStatus::Completed, "success");
    entry.record_completion(&run, None, None);

    assert_eq!(entry.last_seen_run_id, 100);
    assert!(entry.active_runs.is_empty());
    assert!(!entry.last_builds.is_empty());
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
        url: format!("https://github.com/alice/app/actions/runs/{id}"),
        actor: None,
        commit_author: None,
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

    match &emitted[0] {
        RunChange::Completed { elapsed, .. } => assert_eq!(*elapsed, Some(42.0)),
        other => panic!("expected Completed, got {other:?}"),
    }
}

// -- Re-run detection tests --

#[tokio::test]
async fn check_for_new_runs_detects_rerun_with_different_conclusion() {
    let key = WatchKey::new("alice/app", "main");
    let runs = vec![make_run(200, RunStatus::Completed, "success")];
    let h = TestHarness::new(MockGitHub::with_runs(runs));
    h.seed(
        key.clone(),
        WatchEntry {
            last_builds: HashMap::from([("CI".to_string(), {
                let mut lb = make_last_build(200, "failure");
                lb.failing_steps = Some("Build / Run tests".to_string());
                lb.duration_secs = Some(60);
                lb
            })]),
            ..idle_entry(200)
        },
    )
    .await;

    let changes = h.poller("alice/app").check_for_new_runs_repo_wide().await;

    assert_eq!(changes.len(), 1);
    match &changes[0] {
        RunChange::Completed {
            run, conclusion, ..
        } => {
            assert_eq!(run.run_id, 200);
            assert_eq!(*conclusion, RunConclusion::Success);
        }
        other => panic!("expected Completed, got {other:?}"),
    }

    let lb = h.entry(&key).await.last_builds.get("CI").unwrap().clone();
    assert_eq!(lb.conclusion, "success");

    h.cancel();
}

#[tokio::test]
async fn check_for_new_runs_detects_rerun_in_progress() {
    let key = WatchKey::new("alice/app", "main");
    let runs = vec![make_run(200, RunStatus::InProgress, "")];
    let h = TestHarness::new(MockGitHub::with_runs(runs));
    h.seed(
        key.clone(),
        WatchEntry {
            last_builds: HashMap::from([("CI".to_string(), {
                let mut lb = make_last_build(200, "failure");
                lb.failing_steps = Some("Build / Run tests".to_string());
                lb.duration_secs = Some(60);
                lb
            })]),
            ..idle_entry(200)
        },
    )
    .await;

    let changes = h.poller("alice/app").check_for_new_runs_repo_wide().await;

    assert_eq!(changes.len(), 1);
    match &changes[0] {
        RunChange::Started { run } => assert_eq!(run.run_id, 200),
        other => panic!("expected Started, got {other:?}"),
    }

    assert!(h.entry(&key).await.active_runs.contains_key(&200));

    // Second call: run 200 already active, no re-emit.
    let changes2 = h.poller("alice/app").check_for_new_runs_repo_wide().await;
    assert!(changes2.is_empty());

    h.cancel();
}

#[tokio::test]
async fn check_for_new_runs_ignores_rerun_with_same_conclusion() {
    let key = WatchKey::new("alice/app", "main");
    let runs = vec![make_run(200, RunStatus::Completed, "failure")];
    let h = TestHarness::new(MockGitHub::with_runs(runs));
    h.seed(
        key.clone(),
        WatchEntry {
            last_builds: HashMap::from([("CI".to_string(), make_last_build(200, "failure"))]),
            ..idle_entry(200)
        },
    )
    .await;

    let changes = h.poller("alice/app").check_for_new_runs_repo_wide().await;
    assert!(changes.is_empty());

    h.cancel();
}

// -- PR polling tests --

fn make_pr(number: u64, branch: &str, state: crate::github::MergeState) -> crate::github::PrInfo {
    crate::github::PrInfo {
        number,
        title: format!("PR #{number}"),
        branch: branch.to_string(),
        target_branch: "main".to_string(),
        url: format!("https://github.com/alice/app/pull/{number}"),
        author: "alice".to_string(),
        draft: false,
        merge_state: state,
        review_decision: String::new(),
    }
}

#[tokio::test]
async fn poll_prs_skips_when_not_enabled() {
    let h = TestHarness::new(MockGitHub::with_prs(vec![make_pr(
        1,
        "main",
        crate::github::MergeState::Clean,
    )]));
    h.seed(WatchKey::new("alice/app", "main"), idle_entry(100))
        .await;

    let mut rx = h.subscribe();
    let mut poller = h.poller("alice/app");
    poller.poll_prs().await;

    // No events — watch_prs is not enabled.
    assert!(rx.try_recv().is_err());
}

#[tokio::test]
async fn poll_prs_emits_on_transition() {
    let prs = vec![make_pr(42, "feat/login", crate::github::MergeState::Clean)];
    let h = TestHarness::with_config(MockGitHub::with_prs(prs), {
        let mut cfg = Config::default();
        cfg.repos
            .entry("alice/app".to_string())
            .or_default()
            .watch_prs = true;
        cfg
    });
    h.seed(WatchKey::new("alice/app", "main"), idle_entry(100))
        .await;

    let mut rx = h.subscribe();
    let mut poller = h.poller("alice/app");

    // First poll: records state, no event (first occurrence).
    poller.poll_prs().await;
    assert!(rx.try_recv().is_err());
    assert_eq!(
        poller.pr_states.get(&42),
        Some(&crate::github::MergeState::Clean)
    );

    // Simulate state change by updating pr_states directly.
    poller
        .pr_states
        .insert(42, crate::github::MergeState::Blocked);
    poller.poll_prs().await;

    // Now a transition event should be emitted.
    match rx.try_recv() {
        Ok(crate::events::WatchEvent::PrStateChanged {
            number, from, to, ..
        }) => {
            assert_eq!(number, 42);
            assert_eq!(from, crate::github::MergeState::Blocked);
            assert_eq!(to, crate::github::MergeState::Clean);
        }
        other => panic!("expected PrStateChanged, got {other:?}"),
    }
}
