use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::PathBuf;
use std::time::Instant;

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use build_watcher::config::NotificationLevel;
use build_watcher::dirs::state_dir;
use build_watcher::events::WatchEvent;
use build_watcher::persistence::{load_json, save_json};
use build_watcher::status::{HistoryEntryView, RunConclusion, StatsResponse, StatusResponse};

use super::client::DaemonClient;

// Re-export form types so existing imports from `app::` keep working.
pub(crate) use super::forms::{FormField, FormKind, InputMode};

/// What to do when the user presses a quit key.
pub(crate) enum QuitAction {
    None,
    Quit,
    QuitAndShutdown,
}

/// Implements `next()` and `prev()` cycling through all variants of a `Copy + Eq` enum.
macro_rules! impl_cycle {
    ($T:ty, [$($variant:expr),+ $(,)?]) => {
        impl $T {
            const ALL: &[Self] = &[$($variant),+];

            pub(crate) fn next(self) -> Self {
                let idx = Self::ALL.iter().position(|&v| v == self).unwrap_or(0);
                Self::ALL[(idx + 1) % Self::ALL.len()]
            }

            pub(crate) fn prev(self) -> Self {
                let idx = Self::ALL.iter().position(|&v| v == self).unwrap_or(0);
                Self::ALL[(idx + Self::ALL.len() - 1) % Self::ALL.len()]
            }
        }
    };
}

/// How to group rows in the watch list.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum GroupBy {
    #[default]
    Org,
    Branch,
    Workflow,
    Status,
    None,
}

impl_cycle!(
    GroupBy,
    [
        GroupBy::Org,
        GroupBy::Branch,
        GroupBy::Workflow,
        GroupBy::Status,
        GroupBy::None
    ]
);

impl GroupBy {
    pub(crate) fn label(self) -> &'static str {
        match self {
            GroupBy::Org => "org",
            GroupBy::Branch => "branch",
            GroupBy::Workflow => "workflow",
            GroupBy::Status => "status",
            GroupBy::None => "none",
        }
    }

    /// Whether this group mode splits a repo's branches across groups
    /// (each branch shown only under its matching group).
    pub(crate) fn splits_repo(self) -> bool {
        matches!(self, GroupBy::Branch | GroupBy::Workflow)
    }
}

/// Column used for sorting the watch list.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum SortColumn {
    #[default]
    Repo,
    Branch,
    Status,
    Workflow,
    Age,
}

impl_cycle!(
    SortColumn,
    [
        SortColumn::Repo,
        SortColumn::Branch,
        SortColumn::Status,
        SortColumn::Workflow,
        SortColumn::Age
    ]
);

/// How far a repo is expanded in the tree view.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum ExpandLevel {
    /// Show only the repo header row.
    Collapsed,
    /// Show repo + branch rows (but hide per-workflow detail).
    Branches,
    /// Show repo + branch + per-workflow rows (fully expanded).
    #[default]
    Full,
}

impl ExpandLevel {
    /// Cycle to next expand level (expand direction): Collapsed → Branches → Full → Collapsed.
    /// When `has_workflows` is false, skip Full (Collapsed → Branches → Collapsed).
    pub(crate) fn next_expand(self, has_workflows: bool) -> Self {
        match self {
            ExpandLevel::Collapsed => ExpandLevel::Branches,
            ExpandLevel::Branches if has_workflows => ExpandLevel::Full,
            ExpandLevel::Branches | ExpandLevel::Full => ExpandLevel::Collapsed,
        }
    }
}

/// Persisted TUI preferences (sort/group/collapse state).
#[derive(Debug, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct TuiPrefs {
    pub(crate) sort_column: SortColumn,
    pub(crate) sort_ascending: bool,
    pub(crate) group_by: GroupBy,
    /// Per-repo expand level. Missing entries default to `Full`.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub(crate) expand: HashMap<String, ExpandLevel>,
    /// Branches with workflows collapsed (key: "repo#branch"). Absent = expanded.
    #[serde(default, skip_serializing_if = "HashSet::is_empty")]
    pub(crate) workflow_collapsed: HashSet<String>,
    /// Whether the help bar is visible at the bottom.
    #[serde(default = "default_true")]
    pub(crate) show_help: bool,
    /// Whether the recent builds panel is visible at the bottom.
    #[serde(default = "default_true")]
    pub(crate) show_recent_panel: bool,
}

fn default_true() -> bool {
    true
}

impl Default for TuiPrefs {
    fn default() -> Self {
        Self {
            sort_column: SortColumn::default(),
            sort_ascending: true,
            group_by: GroupBy::default(),
            expand: HashMap::new(),
            workflow_collapsed: HashSet::new(),
            show_help: true,
            show_recent_panel: true,
        }
    }
}

impl TuiPrefs {
    fn path() -> PathBuf {
        state_dir().join("tui-prefs.json")
    }

    pub(crate) fn load() -> Self {
        load_json(&Self::path()).unwrap_or_default()
    }

    pub(crate) fn save(&self) {
        if let Err(e) = save_json(&Self::path(), self) {
            tracing::warn!("Failed to save TUI preferences: {e}");
        }
    }
}

pub(crate) struct App {
    pub(crate) status: StatusResponse,
    pub(crate) stats: StatsResponse,
    /// Recent builds across all repos (from `/history/all`).
    pub(crate) recent_history: Vec<HistoryEntryView>,
    /// When we last successfully fetched /status.
    pub(crate) last_fetch: Instant,
    /// Error message from the most recent failed fetch, if any.
    pub(crate) fetch_error: Option<String>,
    pub(crate) sse_state: SseState,
    /// Index into the selectable (non-sub-row) display rows.
    pub(crate) selected: usize,
    /// Transient feedback message shown in the header (e.g. "Adding…").
    pub(crate) flash: Option<(String, Instant)>,
    pub(crate) input_mode: InputMode,
    /// Sender for background task results back to the main loop.
    pub(crate) bg_tx: mpsc::Sender<SseUpdate>,
    pub(crate) sort_column: SortColumn,
    pub(crate) sort_ascending: bool,
    pub(crate) group_by: GroupBy,
    /// Per-repo expand level in the tree view.
    pub(crate) expand: HashMap<String, ExpandLevel>,
    /// Branches with workflows collapsed (key: "repo#branch"). Absent = expanded.
    pub(crate) workflow_collapsed: HashSet<String>,
    /// Tracks the global expand level for Shift-Tab toggling.
    pub(crate) global_expand: ExpandLevel,
    /// Tag name of a newer release, if one was found by the background checker.
    pub(crate) update_available: Option<String>,
    /// Whether to show the help bar at the bottom.
    pub(crate) show_help: bool,
    /// Whether to show the recent builds panel at the bottom.
    pub(crate) show_recent_panel: bool,
}

impl App {
    pub(crate) fn new(
        status: StatusResponse,
        stats: StatsResponse,
        recent_history: Vec<HistoryEntryView>,
        prefs: TuiPrefs,
        bg_tx: mpsc::Sender<SseUpdate>,
    ) -> Self {
        Self {
            status,
            stats,
            recent_history,
            last_fetch: Instant::now(),
            fetch_error: None,
            sse_state: SseState::Connecting,
            selected: 0,
            flash: None,
            input_mode: InputMode::Normal,
            bg_tx,
            sort_column: prefs.sort_column,
            sort_ascending: prefs.sort_ascending,
            group_by: prefs.group_by,
            expand: prefs.expand,
            workflow_collapsed: prefs.workflow_collapsed,
            global_expand: ExpandLevel::Full,
            update_available: None,
            show_help: prefs.show_help,
            show_recent_panel: prefs.show_recent_panel,
        }
    }

    /// Per-branch status bucket counts used by both the header and the terminal title.
    /// Each branch is placed in exactly one bucket: active, failing, passing, or idle.
    pub(crate) fn branch_status_counts(&self) -> (usize, usize, usize, usize) {
        let mut n_active = 0usize;
        let mut n_failing = 0usize;
        let mut n_passing = 0usize;
        let mut n_idle = 0usize;
        for w in &self.status.watches {
            if !w.active_runs.is_empty() {
                n_active += 1;
            } else if w.last_builds.is_empty() {
                n_idle += 1;
            } else if w
                .last_builds
                .iter()
                .any(|b| b.conclusion != RunConclusion::Success)
            {
                n_failing += 1;
            } else {
                n_passing += 1;
            }
        }
        (n_active, n_failing, n_passing, n_idle)
    }

    /// Build a terminal title string summarising build status counts.
    pub(crate) fn terminal_title(&self) -> String {
        if self.status.watches.is_empty() {
            return "bw".to_string();
        }
        let (n_active, n_failing, n_passing, n_idle) = self.branch_status_counts();
        format!(
            "bw · {n_active} pending · {n_passing} success · {n_failing} failure · {n_idle} idle"
        )
    }

    pub(crate) fn set_flash(&mut self, msg: impl Into<String>) {
        let msg = msg.into();
        let first_line = msg.lines().next().unwrap_or(&msg).to_string();
        self.flash = Some((first_line, Instant::now()));
    }

    pub(crate) fn save_prefs(&self) {
        // Prune expand state for repos no longer watched, but only when we have a
        // non-empty watch list — avoids wiping state when the daemon is
        // unreachable and status.watches is temporarily empty.
        let expand = if self.status.watches.is_empty() {
            self.expand.clone()
        } else {
            let watched: HashSet<String> =
                self.status.watches.iter().map(|w| w.repo.clone()).collect();
            self.expand
                .iter()
                .filter(|(k, _)| watched.contains(k.as_str()))
                .map(|(k, v)| (k.clone(), *v))
                .collect()
        };
        TuiPrefs {
            sort_column: self.sort_column,
            sort_ascending: self.sort_ascending,
            group_by: self.group_by,
            expand,
            workflow_collapsed: self.workflow_collapsed.clone(),
            show_help: self.show_help,
            show_recent_panel: self.show_recent_panel,
        }
        .save();
    }

    /// Get the expand level for a repo (defaults to Full).
    pub(crate) fn expand_level(&self, repo: &str) -> ExpandLevel {
        self.expand.get(repo).copied().unwrap_or(ExpandLevel::Full)
    }

    /// Set the expand level for a repo. Full is the implicit default so it is
    /// removed from the map rather than stored explicitly.
    pub(crate) fn set_expand_level(&mut self, repo: &str, level: ExpandLevel) {
        if level == ExpandLevel::Full {
            self.expand.remove(repo);
        } else {
            self.expand.insert(repo.to_string(), level);
        }
    }

    pub(crate) async fn resync(&mut self, daemon: &DaemonClient) {
        let (status_result, stats_result, history_result) = tokio::join!(
            daemon.get_json::<StatusResponse>("/status"),
            daemon.get_json::<StatsResponse>("/stats"),
            daemon.get_all_history(20),
        );
        match status_result {
            Ok(status) => {
                self.status = status;
                self.last_fetch = Instant::now();
                self.fetch_error = None;
            }
            Err(e) => self.fetch_error = Some(e),
        }
        if let Ok(stats) = stats_result {
            self.stats = stats;
        }
        if let Ok(history) = history_result {
            self.recent_history = history;
        }
    }

    /// Advance local elapsed/age counters by one second between resyncs.
    pub(crate) fn tick_timers(&mut self) {
        for watch in &mut self.status.watches {
            for run in &mut watch.active_runs {
                if let Some(e) = &mut run.elapsed_secs {
                    *e += 1.0;
                }
            }
            for lb in &mut watch.last_builds {
                if let Some(a) = &mut lb.age_secs {
                    *a += 1.0;
                }
            }
        }
        for entry in &mut self.recent_history {
            if let Some(a) = &mut entry.age_secs {
                *a += 1;
            }
        }
    }

    /// Spawn a background HTTP action that reports its result via the channel.
    ///
    /// `resync_on_success` controls whether to resync after a successful action.
    /// On error, we **always** resync to clear any stale local state.
    pub(crate) fn spawn_action(
        &mut self,
        flash: impl Into<String>,
        resync_on_success: bool,
        action: impl Future<Output = Result<String, String>> + Send + 'static,
    ) {
        self.set_flash(flash);
        let tx = self.bg_tx.clone();
        tokio::spawn(async move {
            let (flash, resync) = match action.await {
                Ok(msg) => (msg, resync_on_success),
                Err(e) => (e, true),
            };
            let _ = tx.send(SseUpdate::BackgroundResult { flash, resync }).await;
        });
    }
}

// -- SSE state --

/// Message sent from background tasks to the main render loop.
pub(crate) enum SseUpdate {
    /// A watch event received from the SSE stream.
    Event(Box<WatchEvent>),
    /// SSE stream successfully connected.
    Connected,
    /// SSE stream disconnected; task will retry with backoff.
    Disconnected,
    /// Result from a background HTTP action (e.g. adding a repo).
    BackgroundResult { flash: String, resync: bool },
    /// Open a config form popup.
    EnterForm {
        title: String,
        kind: FormKind,
        fields: Vec<FormField>,
    },
    /// Open the per-event notification level picker popup.
    EnterNotificationPicker {
        repo: String,
        branch: String,
        levels: [NotificationLevel; 3],
    },
    /// Open the build history popup.
    EnterHistory {
        repo: String,
        branch: Option<String>,
        entries: Vec<HistoryEntryView>,
    },
    /// Daemon became reachable after a background startup wait.
    DaemonReady(u16),
    /// A newer release was found; tag name to display in the header.
    UpdateAvailable(String),
}

/// Tracks the SSE connection state for header display.
pub(crate) enum SseState {
    Connecting,
    Connected,
    Disconnected { since: Instant },
}

// -- Rows --

// -- Rendering --

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::{
        ColWidths, DisplayRow, FlatRows, flatten_rows, sorted_watches, status_emoji, status_style,
    };
    use build_watcher::events::{RunSnapshot, WatchEvent};
    use build_watcher::status::{
        ActiveRunView, LastBuildView, RunConclusion, RunStatus, StatusResponse, WatchStatus,
    };
    use ratatui::style::Color;

    fn no_collapsed() -> HashMap<String, ExpandLevel> {
        HashMap::new()
    }

    fn no_wf_collapsed() -> HashSet<String> {
        HashSet::new()
    }

    fn snap(repo: &str, branch: &str, run_id: u64) -> RunSnapshot {
        RunSnapshot {
            repo: repo.to_string(),
            branch: branch.to_string(),
            run_id,
            workflow: "CI".to_string(),
            title: "Fix bug".to_string(),
            event: "push".to_string(),
            status: RunStatus::Queued,
            attempt: 1,
            url: format!("https://github.com/{repo}/actions/runs/{run_id}"),
        }
    }

    fn watch(repo: &str, branch: &str) -> WatchStatus {
        WatchStatus {
            repo: repo.to_string(),
            branch: branch.to_string(),
            ..Default::default()
        }
    }

    fn status_with(watches: Vec<WatchStatus>) -> StatusResponse {
        StatusResponse {
            paused: false,
            watches,
        }
    }

    // -- RunStarted --

    #[test]
    fn run_started_inserts_active_run() {
        let mut status = status_with(vec![watch("alice/app", "main")]);
        status.apply_event(WatchEvent::RunStarted(snap("alice/app", "main", 1)));

        let runs = &status.watches[0].active_runs;
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].run_id, 1);
        assert_eq!(runs[0].status, RunStatus::Queued);
        assert_eq!(runs[0].workflow, "CI");
        assert_eq!(runs[0].elapsed_secs, Some(0.0));
    }

    #[test]
    fn run_started_dedup_same_run_id() {
        let mut status = status_with(vec![watch("alice/app", "main")]);
        status.apply_event(WatchEvent::RunStarted(snap("alice/app", "main", 42)));
        status.apply_event(WatchEvent::RunStarted(snap("alice/app", "main", 42)));

        assert_eq!(status.watches[0].active_runs.len(), 1);
    }

    #[test]
    fn run_started_ignores_unknown_watch() {
        let mut status = status_with(vec![watch("alice/app", "main")]);
        status.apply_event(WatchEvent::RunStarted(snap("alice/app", "release", 1)));

        assert!(status.watches[0].active_runs.is_empty());
    }

    // -- RunCompleted --

    #[test]
    fn run_completed_removes_active_run_and_updates_last_build() {
        let mut status = status_with(vec![WatchStatus {
            repo: "alice/app".to_string(),
            branch: "main".to_string(),
            active_runs: vec![ActiveRunView {
                run_id: 7,
                status: RunStatus::InProgress,
                workflow: "CI".to_string(),
                title: "Fix bug".to_string(),
                event: "push".to_string(),
                elapsed_secs: Some(30.0),
                attempt: 1,
                ..Default::default()
            }],
            last_builds: vec![],
            ..Default::default()
        }]);

        status.apply_event(WatchEvent::RunCompleted {
            run: snap("alice/app", "main", 7),
            conclusion: RunConclusion::Success,
            elapsed: Some(35.0),
            failing_steps: None,
            failing_job_id: None,
        });

        assert!(status.watches[0].active_runs.is_empty());
        assert_eq!(status.watches[0].last_builds.len(), 1);
        let lb = &status.watches[0].last_builds[0];
        assert_eq!(lb.run_id, 7);
        assert_eq!(lb.conclusion, RunConclusion::Success);
        assert_eq!(lb.workflow, "CI");
        assert!(lb.failing_steps.is_none());
        assert_eq!(lb.age_secs, Some(0.0));
    }

    #[test]
    fn run_completed_sets_failing_steps() {
        let mut status = status_with(vec![watch("alice/app", "main")]);
        status.apply_event(WatchEvent::RunCompleted {
            run: snap("alice/app", "main", 5),
            conclusion: RunConclusion::Failure,
            elapsed: None,
            failing_steps: Some("Build / tests".to_string()),
            failing_job_id: None,
        });

        assert_eq!(status.watches[0].last_builds.len(), 1);
        let lb = &status.watches[0].last_builds[0];
        assert_eq!(lb.failing_steps.as_deref(), Some("Build / tests"));
    }

    #[test]
    fn run_completed_ignores_unknown_watch() {
        let mut status = status_with(vec![watch("alice/app", "main")]);
        status.apply_event(WatchEvent::RunCompleted {
            run: snap("other/repo", "main", 1),
            conclusion: RunConclusion::Success,
            elapsed: None,
            failing_steps: None,
            failing_job_id: None,
        });

        assert!(status.watches[0].last_builds.is_empty());
    }

    // -- StatusChanged --

    #[test]
    fn status_changed_updates_active_run_status() {
        let mut status = status_with(vec![WatchStatus {
            repo: "alice/app".to_string(),
            branch: "main".to_string(),
            active_runs: vec![ActiveRunView {
                run_id: 3,
                status: RunStatus::Queued,
                workflow: "CI".to_string(),
                title: "Fix bug".to_string(),
                event: "push".to_string(),
                elapsed_secs: None,
                attempt: 1,
                ..Default::default()
            }],
            last_builds: vec![],
            ..Default::default()
        }]);

        status.apply_event(WatchEvent::StatusChanged {
            run: snap("alice/app", "main", 3),
            from: RunStatus::Queued,
            to: RunStatus::InProgress,
        });

        assert_eq!(
            status.watches[0].active_runs[0].status,
            RunStatus::InProgress
        );
    }

    #[test]
    fn status_changed_ignores_unknown_run_id() {
        let mut status = status_with(vec![WatchStatus {
            repo: "alice/app".to_string(),
            branch: "main".to_string(),
            active_runs: vec![ActiveRunView {
                run_id: 3,
                status: RunStatus::Queued,
                workflow: "CI".to_string(),
                title: "Fix bug".to_string(),
                event: "push".to_string(),
                elapsed_secs: None,
                attempt: 1,
                ..Default::default()
            }],
            last_builds: vec![],
            ..Default::default()
        }]);

        status.apply_event(WatchEvent::StatusChanged {
            run: snap("alice/app", "main", 999),
            from: RunStatus::Queued,
            to: RunStatus::InProgress,
        });

        assert_eq!(status.watches[0].active_runs[0].status, RunStatus::Queued);
    }

    #[test]
    fn status_changed_ignores_unknown_watch() {
        let mut status = status_with(vec![watch("alice/app", "main")]);
        status.apply_event(WatchEvent::StatusChanged {
            run: snap("other/repo", "main", 1),
            from: RunStatus::Queued,
            to: RunStatus::InProgress,
        });
        // No panic, no state change.
        assert!(status.watches[0].active_runs.is_empty());
    }

    // -- flatten_rows --

    #[test]
    fn flatten_rows_empty() {
        let flat = flatten_rows(&[], GroupBy::Org, &no_collapsed(), &no_wf_collapsed());
        assert!(flat.rows.is_empty());
        assert!(flat.selectable.is_empty());
    }

    #[test]
    fn flatten_rows_idle_watch() {
        let watches = vec![watch("alice/app", "main")];
        let flat = flatten_rows(&watches, GroupBy::Org, &no_collapsed(), &no_wf_collapsed());
        // Single-branch: GroupHeader + RepoHeader (no child row)
        assert_eq!(flat.rows.len(), 2);
        assert_eq!(flat.selectable.len(), 1); // RepoHeader only
        assert!(matches!(flat.rows[0], DisplayRow::GroupHeader { .. }));
        assert!(matches!(flat.rows[1], DisplayRow::RepoHeader { .. }));
    }

    #[test]
    fn flatten_rows_with_failing_steps_single_branch() {
        // Single-branch repo with a failed build: GroupHeader + RepoHeader (steps shown inline).
        let watches = vec![WatchStatus {
            repo: "alice/app".to_string(),
            branch: "main".to_string(),
            active_runs: vec![],
            last_builds: vec![LastBuildView {
                run_id: 1,
                conclusion: RunConclusion::Failure,
                workflow: "CI".to_string(),
                title: "Fix".to_string(),
                failing_steps: Some("Build / tests".to_string()),
                age_secs: Some(60.0),
                attempt: 1,
                failing_job_id: None,
                ..Default::default()
            }],
            ..Default::default()
        }];
        let flat = flatten_rows(&watches, GroupBy::Org, &no_collapsed(), &no_wf_collapsed());
        assert_eq!(flat.rows.len(), 2); // GroupHeader + RepoHeader (failing steps inline)
        assert_eq!(flat.selectable.len(), 1);
        assert!(matches!(flat.rows[1], DisplayRow::RepoHeader { .. }));
    }

    #[test]
    fn flatten_rows_multi_branch_with_failing_steps() {
        // Multi-branch repo: failing steps shown inline in LastBuild title.
        let watches = vec![
            WatchStatus {
                repo: "alice/app".to_string(),
                branch: "main".to_string(),
                active_runs: vec![],
                last_builds: vec![LastBuildView {
                    run_id: 1,
                    conclusion: RunConclusion::Failure,
                    workflow: "CI".to_string(),
                    title: "Fix".to_string(),
                    failing_steps: Some("Build / tests".to_string()),
                    age_secs: Some(60.0),
                    attempt: 1,
                    failing_job_id: None,
                    ..Default::default()
                }],
                ..Default::default()
            },
            WatchStatus {
                repo: "alice/app".to_string(),
                branch: "develop".to_string(),
                ..Default::default()
            },
        ];
        let flat = flatten_rows(&watches, GroupBy::Org, &no_collapsed(), &no_wf_collapsed());
        // GroupHeader + RepoHeader + LastBuild + NeverRan (no separate FailingSteps row)
        assert_eq!(flat.rows.len(), 4);
    }

    #[test]
    fn flatten_rows_success_single_branch() {
        let watches = vec![WatchStatus {
            repo: "alice/app".to_string(),
            branch: "main".to_string(),
            active_runs: vec![],
            last_builds: vec![LastBuildView {
                run_id: 1,
                conclusion: RunConclusion::Success,
                workflow: "CI".to_string(),
                title: "Fix".to_string(),
                failing_steps: None,
                age_secs: Some(60.0),
                attempt: 1,
                failing_job_id: None,
                ..Default::default()
            }],
            ..Default::default()
        }];
        let flat = flatten_rows(&watches, GroupBy::Org, &no_collapsed(), &no_wf_collapsed());
        // Single-branch: GroupHeader + RepoHeader (no child row)
        assert_eq!(flat.rows.len(), 2);
        assert!(matches!(flat.rows[1], DisplayRow::RepoHeader { .. }));
    }

    #[test]
    fn flatten_rows_workflow_collapsed_hides_workflow_children() {
        // Multi-branch repo where one branch has 2 workflows → should expand.
        // When that branch is in workflow_collapsed, it should collapse to one row.
        let watches = vec![
            WatchStatus {
                repo: "alice/app".to_string(),
                branch: "main".to_string(),
                active_runs: vec![],
                last_builds: vec![
                    LastBuildView {
                        run_id: 1,
                        conclusion: RunConclusion::Success,
                        workflow: "CI".to_string(),
                        title: "Fix".to_string(),
                        failing_steps: None,
                        age_secs: Some(60.0),
                        attempt: 1,
                        failing_job_id: None,
                        ..Default::default()
                    },
                    LastBuildView {
                        run_id: 2,
                        conclusion: RunConclusion::Success,
                        workflow: "Deploy".to_string(),
                        title: "Fix".to_string(),
                        failing_steps: None,
                        age_secs: Some(30.0),
                        attempt: 1,
                        failing_job_id: None,
                        ..Default::default()
                    },
                ],
                ..Default::default()
            },
            WatchStatus {
                repo: "alice/app".to_string(),
                branch: "develop".to_string(),
                ..Default::default()
            },
        ];

        // Without collapse: should have expanded workflow rows for main
        let flat_expanded =
            flatten_rows(&watches, GroupBy::Org, &no_collapsed(), &no_wf_collapsed());
        // With collapse: main's workflows should be collapsed to one row
        let mut wf_collapsed = HashSet::new();
        wf_collapsed.insert("alice/app#main".to_string());
        let flat_collapsed = flatten_rows(&watches, GroupBy::Org, &no_collapsed(), &wf_collapsed);

        // Collapsed should have fewer rows than expanded
        assert!(
            flat_collapsed.rows.len() < flat_expanded.rows.len(),
            "collapsed {} should be less than expanded {}",
            flat_collapsed.rows.len(),
            flat_expanded.rows.len(),
        );
    }

    // -- status_style / status_emoji --

    #[test]
    fn status_style_colors() {
        assert_eq!(status_style("success").fg, Some(Color::Rgb(100, 180, 100)));
        assert_eq!(status_style("failure").fg, Some(Color::Rgb(220, 100, 100)));
        assert_eq!(
            status_style("cancelled").fg,
            Some(Color::Rgb(220, 100, 100))
        );
        assert_eq!(status_style("in_progress").fg, Some(Color::Yellow));
        assert_eq!(status_style("queued").fg, Some(Color::Yellow));
        assert_eq!(status_style("unknown").fg, None);
    }

    #[test]
    fn status_emoji_variants() {
        assert_eq!(status_emoji("success"), "✓");
        assert_eq!(status_emoji("failure"), "✗");
        assert_eq!(status_emoji("in_progress"), "⏳");
        assert_eq!(status_emoji("queued"), "⏸");
        assert_eq!(status_emoji("something_else"), "·");
    }

    // -- ColWidths --

    #[test]
    fn col_widths_minimum_values() {
        // Very narrow terminal — should not panic and should use minimums
        let cw = ColWidths::from_terminal_width(20);
        assert!(cw.repo >= 10);
        assert!(cw.workflow >= 8);
        assert!(cw.title >= 8);
    }

    #[test]
    fn col_widths_wide_terminal() {
        let cw = ColWidths::from_terminal_width(200);
        // Should get reasonable proportions
        assert!(cw.repo > 10);
        assert!(cw.workflow > 8);
        assert!(cw.title > cw.workflow); // title gets 45% vs workflow 25%
    }

    // -- DisplayRow::repo_branch_run --

    #[test]
    fn display_row_repo_branch_run_single_branch() {
        let watches = vec![WatchStatus {
            repo: "alice/app".to_string(),
            branch: "main".to_string(),
            active_runs: vec![ActiveRunView {
                run_id: 42,
                status: RunStatus::InProgress,
                workflow: "CI".to_string(),
                title: "Fix".to_string(),
                event: "push".to_string(),
                elapsed_secs: Some(10.0),
                attempt: 1,
                ..Default::default()
            }],
            last_builds: vec![],
            ..Default::default()
        }];
        let flat = flatten_rows(&watches, GroupBy::Org, &no_collapsed(), &no_wf_collapsed());
        // Single-branch: Row 0: GroupHeader, Row 1: RepoHeader (inline)
        assert_eq!(flat.rows.len(), 2);
        let (repo, branch, _run_id, _muted) = flat.rows[1].repo_branch_run().unwrap();
        assert_eq!(repo, "alice/app");
        assert_eq!(branch, "main");
    }

    #[test]
    fn display_row_group_header_returns_none() {
        let watches = vec![WatchStatus {
            repo: "alice/app".to_string(),
            branch: "main".to_string(),
            ..Default::default()
        }];
        let flat = flatten_rows(&watches, GroupBy::Org, &no_collapsed(), &no_wf_collapsed());
        // Row 0 is a GroupHeader — repo_branch_run should return None.
        assert!(flat.rows[0].repo_branch_run().is_none());
    }

    #[test]
    fn run_completed_sets_display_title_for_pr() {
        let mut pr_snap = snap("alice/app", "main", 10);
        pr_snap.event = "pull_request".to_string();
        pr_snap.title = "Add feature".to_string();

        let mut status = status_with(vec![watch("alice/app", "main")]);
        status.apply_event(WatchEvent::RunCompleted {
            run: pr_snap,
            conclusion: RunConclusion::Success,
            elapsed: None,
            failing_steps: None,
            failing_job_id: None,
        });

        assert_eq!(status.watches[0].last_builds.len(), 1);
        let lb = &status.watches[0].last_builds[0];
        assert_eq!(lb.title, "PR: Add feature");
    }

    // -- SortColumn --

    #[test]
    fn sort_column_next_cycles_through_all() {
        let mut col = SortColumn::Repo;
        let mut seen = vec![col];
        for _ in 0..SortColumn::ALL.len() {
            col = col.next();
            seen.push(col);
        }
        // Should cycle back to Repo after going through all columns.
        assert_eq!(seen.first(), seen.last());
        assert_eq!(seen.len(), 6);
    }

    // -- sorted_watches --

    fn watch_with_build(
        repo: &str,
        branch: &str,
        conclusion: RunConclusion,
        age: f64,
    ) -> WatchStatus {
        WatchStatus {
            repo: repo.to_string(),
            branch: branch.to_string(),
            active_runs: vec![],
            last_builds: vec![LastBuildView {
                run_id: 1,
                conclusion,
                workflow: "CI".to_string(),
                title: "Fix".to_string(),
                failing_steps: None,
                age_secs: Some(age),
                attempt: 1,
                failing_job_id: None,
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    fn watch_with_active(repo: &str, branch: &str, status: RunStatus, elapsed: f64) -> WatchStatus {
        WatchStatus {
            repo: repo.to_string(),
            branch: branch.to_string(),
            active_runs: vec![ActiveRunView {
                run_id: 1,
                status,
                workflow: "Deploy".to_string(),
                title: "Ship".to_string(),
                event: "push".to_string(),
                elapsed_secs: Some(elapsed),
                attempt: 1,
                ..Default::default()
            }],
            last_builds: vec![],
            ..Default::default()
        }
    }

    #[test]
    fn sorted_watches_by_repo() {
        let watches = vec![
            watch_with_build("zoo/app", "main", RunConclusion::Success, 10.0),
            watch_with_build("alice/lib", "main", RunConclusion::Failure, 20.0),
        ];
        let sorted = sorted_watches(&watches, SortColumn::Repo, true, GroupBy::None);
        assert_eq!(sorted[0].repo, "alice/lib");
        assert_eq!(sorted[1].repo, "zoo/app");

        let desc = sorted_watches(&watches, SortColumn::Repo, false, GroupBy::None);
        assert_eq!(desc[0].repo, "zoo/app");
    }

    #[test]
    fn sorted_watches_by_branch() {
        let watches = vec![
            watch_with_build("alice/app", "release", RunConclusion::Success, 10.0),
            watch_with_build("alice/app", "develop", RunConclusion::Success, 20.0),
            watch_with_build("alice/app", "main", RunConclusion::Success, 30.0),
        ];
        let sorted = sorted_watches(&watches, SortColumn::Branch, true, GroupBy::None);
        assert_eq!(sorted[0].branch, "develop");
        assert_eq!(sorted[1].branch, "main");
        assert_eq!(sorted[2].branch, "release");
    }

    #[test]
    fn sorted_watches_by_status_active_before_completed() {
        let watches = vec![
            watch_with_build("alice/app", "main", RunConclusion::Success, 10.0),
            watch_with_active("bob/lib", "main", RunStatus::InProgress, 5.0),
            watch("carol/api", "main"),
        ];
        let sorted = sorted_watches(&watches, SortColumn::Status, true, GroupBy::None);
        // Active (tier 0) < completed (tier 1) < idle (tier 2)
        assert_eq!(sorted[0].repo, "bob/lib");
        assert_eq!(sorted[1].repo, "alice/app");
        assert_eq!(sorted[2].repo, "carol/api");
    }

    #[test]
    fn sorted_watches_by_workflow() {
        let watches = vec![
            watch_with_build("alice/app", "main", RunConclusion::Success, 10.0), // CI
            watch_with_active("bob/lib", "main", RunStatus::InProgress, 5.0),    // Deploy
        ];
        let sorted = sorted_watches(&watches, SortColumn::Workflow, true, GroupBy::None);
        assert_eq!(sorted[0].repo, "alice/app"); // CI < Deploy
        assert_eq!(sorted[1].repo, "bob/lib");
    }

    #[test]
    fn sorted_watches_by_age() {
        let watches = vec![
            watch_with_build("alice/app", "main", RunConclusion::Success, 100.0),
            watch_with_build("bob/lib", "main", RunConclusion::Failure, 10.0),
            watch_with_active("carol/api", "main", RunStatus::InProgress, 5.0),
        ];
        let sorted = sorted_watches(&watches, SortColumn::Age, true, GroupBy::None);
        assert_eq!(sorted[0].repo, "carol/api"); // 5s elapsed
        assert_eq!(sorted[1].repo, "bob/lib"); // 10s age
        assert_eq!(sorted[2].repo, "alice/app"); // 100s age
    }

    #[test]
    fn sorted_watches_descending_reverses() {
        let watches = vec![
            watch_with_build("alice/app", "main", RunConclusion::Success, 10.0),
            watch_with_build("bob/lib", "main", RunConclusion::Failure, 100.0),
        ];
        let asc = sorted_watches(&watches, SortColumn::Age, true, GroupBy::None);
        let desc = sorted_watches(&watches, SortColumn::Age, false, GroupBy::None);
        assert_eq!(asc[0].repo, "alice/app");
        assert_eq!(desc[0].repo, "bob/lib");
    }

    // -- GroupBy --

    #[test]
    fn group_by_next_cycles_through_all() {
        let mut g = GroupBy::Org;
        let mut seen = vec![g];
        for _ in 0..GroupBy::ALL.len() {
            g = g.next();
            seen.push(g);
        }
        assert_eq!(seen.first(), seen.last());
        assert_eq!(seen.len(), 6);
    }

    fn count_group_headers(flat: &FlatRows) -> usize {
        flat.rows
            .iter()
            .filter(|r| matches!(r, DisplayRow::GroupHeader { .. }))
            .count()
    }

    fn group_header_labels(flat: &FlatRows) -> Vec<String> {
        flat.rows
            .iter()
            .filter_map(|r| match r {
                DisplayRow::GroupHeader { label } => Some(label.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn flatten_rows_group_by_org() {
        let watches = vec![
            watch_with_build("alice/app", "main", RunConclusion::Success, 10.0),
            watch_with_build("alice/lib", "main", RunConclusion::Success, 20.0),
            watch_with_build("bob/api", "main", RunConclusion::Failure, 30.0),
        ];
        let flat = flatten_rows(&watches, GroupBy::Org, &no_collapsed(), &no_wf_collapsed());
        assert_eq!(group_header_labels(&flat), vec!["alice", "bob"]);
    }

    #[test]
    fn flatten_rows_group_by_branch() {
        let watches = vec![
            watch_with_build("alice/app", "main", RunConclusion::Success, 10.0),
            watch_with_build("alice/app", "develop", RunConclusion::Success, 20.0),
            watch_with_build("bob/lib", "main", RunConclusion::Failure, 30.0),
        ];
        let sorted = sorted_watches(&watches, SortColumn::Branch, true, GroupBy::Branch);
        let flat = flatten_rows(
            &sorted,
            GroupBy::Branch,
            &no_collapsed(),
            &no_wf_collapsed(),
        );
        // "develop" group has alice/app, "main" group has alice/app and bob/lib
        assert_eq!(group_header_labels(&flat), vec!["develop", "main"]);
    }

    #[test]
    fn flatten_rows_group_by_status() {
        let watches = vec![
            watch_with_build("alice/app", "main", RunConclusion::Success, 10.0),
            watch_with_build("bob/lib", "main", RunConclusion::Failure, 20.0),
            watch_with_active("carol/api", "main", RunStatus::InProgress, 5.0),
        ];
        let sorted = sorted_watches(&watches, SortColumn::Status, true, GroupBy::None);
        let flat = flatten_rows(
            &sorted,
            GroupBy::Status,
            &no_collapsed(),
            &no_wf_collapsed(),
        );
        let labels = group_header_labels(&flat);
        assert_eq!(labels.len(), 3);
        assert_eq!(labels[0], "in progress"); // active tier
        assert_eq!(labels[1], "failure"); // completed, alphabetical
        assert_eq!(labels[2], "success");
    }

    #[test]
    fn flatten_rows_group_by_none() {
        let watches = vec![
            watch_with_build("alice/app", "main", RunConclusion::Success, 10.0),
            watch_with_build("bob/lib", "main", RunConclusion::Failure, 20.0),
        ];
        let flat = flatten_rows(&watches, GroupBy::None, &no_collapsed(), &no_wf_collapsed());
        assert_eq!(count_group_headers(&flat), 0);
        // 2 single-branch RepoHeaders (no group headers, no child rows)
        assert_eq!(flat.rows.len(), 2);
    }

    #[test]
    fn flatten_rows_group_by_workflow() {
        let watches = vec![
            watch_with_build("alice/app", "main", RunConclusion::Success, 10.0), // CI
            watch_with_active("bob/lib", "main", RunStatus::InProgress, 5.0),    // Deploy
            watch("carol/api", "main"),                                          // no workflow
        ];
        let flat = flatten_rows(
            &watches,
            GroupBy::Workflow,
            &no_collapsed(),
            &no_wf_collapsed(),
        );
        let labels = group_header_labels(&flat);
        assert_eq!(labels, vec!["CI", "Deploy", "(none)"]);
    }

    #[test]
    fn notification_level_next_wraps() {
        assert_eq!(NotificationLevel::Off.next(), NotificationLevel::Low);
        assert_eq!(NotificationLevel::Low.next(), NotificationLevel::Normal);
        assert_eq!(
            NotificationLevel::Normal.next(),
            NotificationLevel::Critical
        );
        assert_eq!(NotificationLevel::Critical.next(), NotificationLevel::Off);
    }

    #[test]
    fn notification_level_prev_wraps() {
        assert_eq!(NotificationLevel::Off.prev(), NotificationLevel::Critical);
        assert_eq!(
            NotificationLevel::Critical.prev(),
            NotificationLevel::Normal
        );
        assert_eq!(NotificationLevel::Normal.prev(), NotificationLevel::Low);
        assert_eq!(NotificationLevel::Low.prev(), NotificationLevel::Off);
    }

    // -- TuiPrefs persistence tests --

    fn temp_prefs_path(suffix: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("bw-prefs-{}-{suffix}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("tui-prefs.json")
    }

    #[test]
    fn tui_prefs_roundtrip() {
        let path = temp_prefs_path("roundtrip");
        let prefs = TuiPrefs {
            sort_column: SortColumn::Workflow,
            sort_ascending: false,
            group_by: GroupBy::Status,
            expand: HashMap::from([
                ("alice/app".to_string(), ExpandLevel::Collapsed),
                ("bob/lib".to_string(), ExpandLevel::Branches),
            ]),
            workflow_collapsed: HashSet::from(["alice/app#main".to_string()]),
            show_help: false,
            show_recent_panel: false,
        };
        save_json(&path, &prefs).unwrap();
        let loaded: TuiPrefs = load_json(&path).unwrap();
        assert_eq!(loaded.sort_column, SortColumn::Workflow);
        assert!(!loaded.sort_ascending);
        assert_eq!(loaded.group_by, GroupBy::Status);
        assert_eq!(loaded.expand.len(), 2);
        assert_eq!(loaded.expand["alice/app"], ExpandLevel::Collapsed);
        assert!(loaded.workflow_collapsed.contains("alice/app#main"));
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn tui_prefs_corrupt_file_returns_defaults() {
        let path = temp_prefs_path("corrupt");
        std::fs::write(&path, "not json at all {{{").unwrap();
        let loaded: Option<TuiPrefs> = load_json(&path);
        assert!(loaded.is_none());
        // Callers use unwrap_or_default:
        let prefs = loaded.unwrap_or_default();
        assert_eq!(prefs.sort_column, SortColumn::Repo);
        assert!(prefs.sort_ascending);
        assert_eq!(prefs.group_by, GroupBy::Org);
        assert!(prefs.expand.is_empty());
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn tui_prefs_missing_file_returns_defaults() {
        let dir = std::env::temp_dir().join(format!("bw-prefs-{}-missing", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("tui-prefs.json");
        // Don't create the file
        let loaded: Option<TuiPrefs> = load_json(&path);
        assert!(loaded.is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn tui_prefs_old_format_without_expand() {
        let path = temp_prefs_path("old-format");
        // Simulate a prefs file written before the expand field existed.
        std::fs::write(
            &path,
            r#"{"sort_column":"Branch","sort_ascending":false,"group_by":"Workflow"}"#,
        )
        .unwrap();
        let loaded: TuiPrefs = load_json(&path).unwrap();
        assert_eq!(loaded.sort_column, SortColumn::Branch);
        assert!(!loaded.sort_ascending);
        assert_eq!(loaded.group_by, GroupBy::Workflow);
        assert!(loaded.expand.is_empty());
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn tui_prefs_partial_json_fills_defaults() {
        let path = temp_prefs_path("partial");
        // Only sort_column present — everything else should get defaults.
        std::fs::write(&path, r#"{"sort_column":"Age"}"#).unwrap();
        let loaded: TuiPrefs = load_json(&path).unwrap();
        assert_eq!(loaded.sort_column, SortColumn::Age);
        assert!(loaded.sort_ascending); // default: true
        assert_eq!(loaded.group_by, GroupBy::Org); // default
        assert!(loaded.expand.is_empty()); // default
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn tui_prefs_unknown_enum_value_returns_none() {
        let path = temp_prefs_path("bad-enum");
        // Invalid sort_column value — strict deserialization fails.
        std::fs::write(
            &path,
            r#"{"sort_column":"NonExistent","sort_ascending":true,"group_by":"Org"}"#,
        )
        .unwrap();
        let loaded: Option<TuiPrefs> = load_json(&path);
        assert!(loaded.is_none());
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn tui_prefs_backup_recovery() {
        let path = temp_prefs_path("backup");
        let bak = path.with_extension("json.bak");
        let prefs = TuiPrefs {
            sort_column: SortColumn::Status,
            sort_ascending: false,
            group_by: GroupBy::Branch,
            expand: HashMap::from([("repo/x".to_string(), ExpandLevel::Collapsed)]),
            workflow_collapsed: HashSet::new(),
            show_help: true,
            show_recent_panel: true,
        };
        // Write valid backup, corrupt primary.
        std::fs::write(&bak, serde_json::to_string(&prefs).unwrap()).unwrap();
        std::fs::write(&path, "corrupt!!!").unwrap();
        let loaded: TuiPrefs = load_json(&path).unwrap();
        assert_eq!(loaded.sort_column, SortColumn::Status);
        assert!(!loaded.sort_ascending);
        assert_eq!(loaded.group_by, GroupBy::Branch);
        assert_eq!(loaded.expand["repo/x"], ExpandLevel::Collapsed);
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }
}
