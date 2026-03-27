use std::collections::HashSet;
use std::future::Future;
use std::path::PathBuf;
use std::time::Instant;

use crossterm::event::{KeyCode, KeyModifiers};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use build_watcher::config::{NotificationLevel, state_dir};
use build_watcher::events::WatchEvent;
use build_watcher::github::{repo_url, run_url, validate_branch, validate_repo};
use build_watcher::status::{
    ActiveRunView, HistoryEntryView, LastBuildView, StatsResponse, StatusResponse, WatchStatus,
};

use super::client::{DaemonClient, open_browser};
use super::render::flatten_rows;

/// What to do when the user presses a quit key.
pub(crate) enum QuitAction {
    None,
    Quit,
    QuitAndShutdown,
}

// -- App state --

/// What the current text input prompt is for.
pub(crate) enum TextAction {
    AddRepo,
    SetBranches { repo: String },
}

/// A labeled field in a form popup.
pub(crate) struct FormField {
    pub(crate) label: String,
    pub(crate) buffer: String,
    /// If non-empty, this is a cycle field (Left/Right to cycle, no free-text entry).
    pub(crate) options: Vec<&'static str>,
}

/// Text input mode for interactive prompts (e.g. "Add repo: ").
pub(crate) enum InputMode {
    Normal,
    TextInput {
        prompt: String,
        buffer: String,
        action: TextAction,
    },
    /// Multi-field form popup (e.g. config defaults).
    Form {
        title: String,
        fields: Vec<FormField>,
        active: usize,
    },
    /// Per-event notification level picker popup (opened with `N`).
    NotificationPicker {
        repo: String,
        branch: String,
        /// [started, success, failure]
        levels: [NotificationLevel; 3],
        /// Active row index (0..3).
        active: usize,
    },
    /// Build history overlay popup (opened with `h`/`H`).
    History {
        repo: String,
        branch: Option<String>,
        entries: Vec<HistoryEntryView>,
        selected: usize,
    },
}

/// Implements `next()` and `prev()` cycling through all variants of a `Copy + Eq` enum.
macro_rules! impl_cycle {
    ($T:ty, [$($variant:expr),+ $(,)?]) => {
        impl $T {
            const ALL: &[Self] = &[$($variant),+];

            fn next(self) -> Self {
                let idx = Self::ALL.iter().position(|&v| v == self).unwrap_or(0);
                Self::ALL[(idx + 1) % Self::ALL.len()]
            }

            fn prev(self) -> Self {
                let idx = Self::ALL.iter().position(|&v| v == self).unwrap_or(0);
                Self::ALL[(idx + Self::ALL.len() - 1) % Self::ALL.len()]
            }
        }
    };
}

/// How to group rows in the watch list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum GroupBy {
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
}

/// Column used for sorting the watch list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum SortColumn {
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

/// Persisted TUI preferences (sort/group state).
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct TuiPrefs {
    pub(crate) sort_column: SortColumn,
    pub(crate) sort_ascending: bool,
    pub(crate) group_by: GroupBy,
}

impl Default for TuiPrefs {
    fn default() -> Self {
        Self {
            sort_column: SortColumn::Repo,
            sort_ascending: true,
            group_by: GroupBy::Org,
        }
    }
}

impl TuiPrefs {
    fn path() -> PathBuf {
        state_dir().join("tui-prefs.json")
    }

    pub(crate) fn load() -> Self {
        std::fs::read_to_string(Self::path())
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub(crate) fn save(&self) {
        if let Ok(json) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(Self::path(), json);
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
    /// Repos whose branches are collapsed (hidden) in the tree view.
    pub(crate) collapsed: HashSet<String>,
}

impl App {
    pub(crate) fn active_count(&self) -> usize {
        self.status
            .watches
            .iter()
            .map(|w| w.active_runs.len())
            .sum()
    }

    /// Build a terminal title string summarising build status counts.
    pub(crate) fn terminal_title(&self) -> String {
        if self.status.watches.is_empty() {
            return "bw".to_string();
        }

        let active = self.active_count();
        let mut success = 0usize;
        let mut failed = 0usize;

        for w in &self.status.watches {
            if let Some(lb) = &w.last_build {
                match lb.conclusion.as_str() {
                    "success" => success += 1,
                    "failure" | "timed_out" | "startup_failure" => failed += 1,
                    _ => {}
                }
            }
        }

        format!("builds: {active} / {success} / {failed}")
    }

    pub(crate) fn set_flash(&mut self, msg: impl Into<String>) {
        self.flash = Some((msg.into(), Instant::now()));
    }

    fn save_prefs(&self) {
        TuiPrefs {
            sort_column: self.sort_column,
            sort_ascending: self.sort_ascending,
            group_by: self.group_by,
        }
        .save();
    }

    pub(crate) async fn resync(&mut self, daemon: &DaemonClient) {
        match daemon.get_json::<StatusResponse>("/status").await {
            Ok(status) => {
                self.status = status;
                self.last_fetch = Instant::now();
                self.fetch_error = None;
            }
            Err(e) => self.fetch_error = Some(e),
        }
        if let Ok(stats) = daemon.get_json::<StatsResponse>("/stats").await {
            self.stats = stats;
        }
        if let Ok(history) = daemon.get_all_history(20).await {
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
            if let Some(lb) = &mut watch.last_build
                && let Some(a) = &mut lb.age_secs
            {
                *a += 1.0;
            }
        }
        for entry in &mut self.recent_history {
            if let Some(a) = &mut entry.age_secs {
                *a += 1;
            }
        }
    }

    /// Spawn a background HTTP action that reports its result via the channel.
    fn spawn_action(
        &mut self,
        flash: impl Into<String>,
        resync: bool,
        action: impl Future<Output = Result<String, String>> + Send + 'static,
    ) {
        self.set_flash(flash);
        let tx = self.bg_tx.clone();
        tokio::spawn(async move {
            let flash = match action.await {
                Ok(msg) => msg,
                Err(e) => e,
            };
            let _ = tx.send(SseUpdate::BackgroundResult { flash, resync }).await;
        });
    }

    /// Handle a key press while in a non-normal input mode.
    /// Returns `true` if the event was consumed.
    pub(crate) fn handle_input(&mut self, code: KeyCode, daemon: &DaemonClient) -> bool {
        match &mut self.input_mode {
            InputMode::Normal => false,
            InputMode::TextInput { buffer, action, .. } => {
                match code {
                    KeyCode::Esc => {
                        self.input_mode = InputMode::Normal;
                    }
                    KeyCode::Enter => {
                        let input = buffer.trim().to_string();
                        let action = std::mem::replace(action, TextAction::AddRepo);
                        self.input_mode = InputMode::Normal;
                        if !input.is_empty() {
                            self.submit_text_input(input, action, daemon);
                        }
                    }
                    KeyCode::Backspace => {
                        buffer.pop();
                    }
                    KeyCode::Char(c) => {
                        buffer.push(c);
                    }
                    _ => {}
                }
                true
            }
            InputMode::Form { fields, active, .. } => {
                match code {
                    KeyCode::Esc => {
                        self.input_mode = InputMode::Normal;
                    }
                    KeyCode::Tab | KeyCode::Down => {
                        *active = (*active + 1) % fields.len();
                    }
                    KeyCode::BackTab | KeyCode::Up => {
                        *active = (*active + fields.len() - 1) % fields.len();
                    }
                    KeyCode::Right | KeyCode::Char(' ') => {
                        let f = &mut fields[*active];
                        if !f.options.is_empty() {
                            let idx = f.options.iter().position(|&o| o == f.buffer).unwrap_or(0);
                            f.buffer = f.options[(idx + 1) % f.options.len()].to_string();
                        }
                    }
                    KeyCode::Left => {
                        let f = &mut fields[*active];
                        if !f.options.is_empty() {
                            let n = f.options.len();
                            let idx = f.options.iter().position(|&o| o == f.buffer).unwrap_or(0);
                            f.buffer = f.options[(idx + n - 1) % n].to_string();
                        }
                    }
                    KeyCode::Backspace => {
                        let f = &mut fields[*active];
                        if f.options.is_empty() {
                            f.buffer.pop();
                        }
                    }
                    KeyCode::Char(c) => {
                        let f = &mut fields[*active];
                        if f.options.is_empty() {
                            f.buffer.push(c);
                        }
                    }
                    KeyCode::Enter => {
                        self.submit_config_form(daemon);
                    }
                    _ => {}
                }
                true
            }
            InputMode::NotificationPicker {
                repo,
                branch,
                levels,
                active,
            } => {
                match code {
                    KeyCode::Esc => {
                        self.input_mode = InputMode::Normal;
                    }
                    KeyCode::Tab | KeyCode::Down => {
                        *active = (*active + 1) % 3;
                    }
                    KeyCode::BackTab | KeyCode::Up => {
                        *active = (*active + 2) % 3;
                    }
                    KeyCode::Right | KeyCode::Char(' ') => {
                        levels[*active] = levels[*active].next();
                    }
                    KeyCode::Left => {
                        levels[*active] = levels[*active].prev();
                    }
                    KeyCode::Enter => {
                        let repo = repo.clone();
                        let branch = branch.clone();
                        let [started, success, failure] = *levels;
                        self.input_mode = InputMode::Normal;
                        let d = daemon.clone();
                        self.spawn_action("Saving notification levels…", true, async move {
                            d.set_notification_levels(&repo, &branch, started, success, failure)
                                .await
                                .map(|()| "Notification levels saved".to_string())
                        });
                    }
                    _ => {}
                }
                true
            }
            InputMode::History {
                repo,
                entries,
                selected,
                ..
            } => {
                match code {
                    KeyCode::Esc => {
                        self.input_mode = InputMode::Normal;
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        *selected = selected.saturating_sub(1);
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        if !entries.is_empty() {
                            *selected = (*selected + 1).min(entries.len() - 1);
                        }
                    }
                    KeyCode::Char('o') => {
                        if let Some(entry) = entries.get(*selected) {
                            let url = run_url(repo, entry.id);
                            open_browser(&url);
                        }
                    }
                    KeyCode::Char('q') => {
                        self.input_mode = InputMode::Normal;
                    }
                    _ => {}
                }
                true
            }
        }
    }

    /// Submit the config form fields to the daemon.
    fn submit_config_form(&mut self, daemon: &DaemonClient) {
        let InputMode::Form { fields, .. } = &self.input_mode else {
            return;
        };

        let parse_csv = |s: &str| -> Vec<String> {
            s.split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        };

        let branches: Vec<String> = fields
            .iter()
            .find(|f| f.label == "Default branches")
            .map(|f| parse_csv(&f.buffer))
            .unwrap_or_default();
        let workflows: Vec<String> = fields
            .iter()
            .find(|f| f.label == "Ignored workflows")
            .map(|f| parse_csv(&f.buffer))
            .unwrap_or_default();
        let aggression: Option<String> = fields
            .iter()
            .find(|f| f.label == "Poll aggression")
            .map(|f| f.buffer.clone());

        if branches.is_empty() {
            self.set_flash("Default branches must not be empty");
            return;
        }
        for b in &branches {
            if let Err(e) = validate_branch(b) {
                self.set_flash(e);
                return;
            }
        }

        let d = daemon.clone();
        self.input_mode = InputMode::Normal;
        self.spawn_action("Saving config…", false, async move {
            d.set_defaults(Some(branches), Some(workflows), aggression)
                .await
                .map(|()| "Config saved".to_string())
        });
    }

    fn submit_text_input(&mut self, input: String, action: TextAction, daemon: &DaemonClient) {
        match action {
            TextAction::AddRepo => {
                if let Err(e) = validate_repo(&input) {
                    self.set_flash(e);
                    return;
                }
                let d = daemon.clone();
                let repo = input.clone();
                self.spawn_action(format!("Adding {input}…"), true, async move {
                    d.watch(&repo).await.map(|()| format!("Watching {repo}"))
                });
            }
            TextAction::SetBranches { repo } => {
                let branches: Vec<String> = input
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
                if branches.is_empty() {
                    self.set_flash("No branches specified");
                    return;
                }
                for b in &branches {
                    if let Err(e) = validate_branch(b) {
                        self.set_flash(e);
                        return;
                    }
                }
                let d = daemon.clone();
                let repo_clone = repo.clone();
                self.spawn_action(format!("Setting branches for {repo}…"), true, async move {
                    d.set_branches(&repo_clone, &branches)
                        .await
                        .map(|()| format!("Branches updated for {repo_clone}"))
                });
            }
        }
    }

    /// Handle a key press in normal mode.
    pub(crate) fn handle_normal_key(
        &mut self,
        code: KeyCode,
        modifiers: KeyModifiers,
        daemon: &DaemonClient,
    ) -> QuitAction {
        let sorted = super::render::sorted_watches(
            &self.status.watches,
            self.sort_column,
            self.sort_ascending,
            self.group_by,
        );
        let flat = flatten_rows(&sorted, self.group_by, &self.collapsed);
        let sel_count = flat.selectable.len();
        let selected_display_idx = flat.selectable.get(self.selected).copied();
        let selected = selected_display_idx.map(|idx| flat.rows[idx].repo_branch_run());
        let is_repo_row = selected_display_idx
            .map(|idx| flat.rows[idx].is_repo_header())
            .unwrap_or(false);

        match code {
            KeyCode::Char('q') => return QuitAction::Quit,
            KeyCode::Char('Q') => return QuitAction::QuitAndShutdown,
            KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => {
                return QuitAction::Quit;
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.selected = self.selected.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if sel_count > 0 {
                    self.selected = (self.selected + 1).min(sel_count - 1);
                }
            }
            KeyCode::Enter | KeyCode::Right if is_repo_row => {
                if let Some((repo, _, _, _)) = selected {
                    let repo = repo.to_string();
                    if self.collapsed.contains(&repo) {
                        self.collapsed.remove(&repo);
                    } else {
                        self.collapsed.insert(repo);
                    }
                }
            }
            KeyCode::Left if !is_repo_row => {
                // Collapse parent repo and move selection to it
                if let Some((repo, _, _, _)) = selected {
                    let repo = repo.to_string();
                    self.collapsed.insert(repo.clone());
                    // Find the repo header's selectable index
                    if let Some(pos) = flat.selectable.iter().position(|&idx| {
                        flat.rows[idx].is_repo_header()
                            && flat.rows[idx].repo_branch_run().0 == repo
                    }) {
                        self.selected = pos;
                    }
                }
            }
            KeyCode::Char('e') => {
                if let Some((repo, _, _, _)) = selected {
                    let repo = repo.to_string();
                    if self.collapsed.contains(&repo) {
                        self.collapsed.remove(&repo);
                    } else {
                        self.collapsed.insert(repo);
                    }
                }
            }
            KeyCode::Char('E') => {
                // Expand all if any collapsed, collapse all if all expanded
                let all_repos: HashSet<String> =
                    self.status.watches.iter().map(|w| w.repo.clone()).collect();
                if self.collapsed.is_empty() {
                    self.collapsed = all_repos;
                } else {
                    self.collapsed.clear();
                }
            }
            KeyCode::Char('a') => {
                self.input_mode = InputMode::TextInput {
                    prompt: "Add repo (owner/repo): ".to_string(),
                    buffer: String::new(),
                    action: TextAction::AddRepo,
                };
            }
            KeyCode::Char('b') => {
                if let Some((repo, _, _, _)) = selected {
                    let repo = repo.to_string();
                    let current: Vec<&str> = self
                        .status
                        .watches
                        .iter()
                        .filter(|w| w.repo == repo)
                        .map(|w| w.branch.as_str())
                        .collect();
                    self.input_mode = InputMode::TextInput {
                        prompt: format!("Branches for {repo}: "),
                        buffer: current.join(", "),
                        action: TextAction::SetBranches { repo },
                    };
                }
            }
            KeyCode::Char('d') => {
                if let Some((repo, _, _, _)) = selected {
                    let d = daemon.clone();
                    let repo = repo.to_string();
                    self.spawn_action(format!("Removing {repo}…"), true, async move {
                        d.unwatch(&repo).await.map(|()| format!("Removed {repo}"))
                    });
                }
            }
            KeyCode::Char('n') => {
                if let Some((repo, branch, _, muted)) = selected {
                    let d = daemon.clone();
                    let repo = repo.to_string();
                    let action = if muted { "unmute" } else { "mute" };
                    let verb = if muted { "Unmuted" } else { "Muted" };
                    if is_repo_row {
                        // Mute/unmute all branches for this repo
                        let branches: Vec<String> = self
                            .status
                            .watches
                            .iter()
                            .filter(|w| w.repo == repo)
                            .map(|w| w.branch.clone())
                            .collect();
                        let label = repo.clone();
                        self.spawn_action(format!("{verb} {label}…"), true, async move {
                            for b in &branches {
                                d.set_notifications(&repo, b, action).await?;
                            }
                            Ok(format!("{verb} {label}"))
                        });
                    } else {
                        let branch = branch.to_string();
                        let label = format!("{repo}/{branch}");
                        self.spawn_action(format!("{verb} {label}…"), true, async move {
                            d.set_notifications(&repo, &branch, action)
                                .await
                                .map(|()| format!("{verb} {label}"))
                        });
                    }
                }
            }
            KeyCode::Char('N') => {
                if let Some((repo, branch, _, _)) = selected {
                    let d = daemon.clone();
                    let repo = repo.to_string();
                    let branch = branch.to_string();
                    let tx = self.bg_tx.clone();
                    self.set_flash("Loading notification levels…");
                    tokio::spawn(async move {
                        match d.get_notifications(&repo, &branch).await {
                            Ok(cfg) => {
                                let _ = tx
                                    .send(SseUpdate::EnterNotificationPicker {
                                        repo,
                                        branch,
                                        levels: [
                                            cfg.build_started,
                                            cfg.build_success,
                                            cfg.build_failure,
                                        ],
                                    })
                                    .await;
                            }
                            Err(e) => {
                                let _ = tx
                                    .send(SseUpdate::BackgroundResult {
                                        flash: e,
                                        resync: false,
                                    })
                                    .await;
                            }
                        }
                    });
                }
            }
            KeyCode::Char('p') => {
                let new_pause = !self.status.paused;
                let d = daemon.clone();
                // Optimistic update — toggle local state immediately.
                self.status.paused = new_pause;
                let msg = if new_pause { "Paused" } else { "Resumed" };
                self.spawn_action(msg.to_string(), false, async move {
                    d.pause(new_pause)
                        .await
                        .map(|()| if new_pause { "Paused" } else { "Resumed" }.to_string())
                });
            }
            KeyCode::Char('o') => {
                if is_repo_row {
                    // Open repo Actions page
                    if let Some((repo, _, _, _)) = selected {
                        open_browser(&format!("{}/actions", repo_url(repo)));
                    }
                } else if let Some((repo, _, Some(run_id), _)) = selected {
                    open_browser(&run_url(repo, run_id));
                }
            }
            KeyCode::Char('O') => {
                if let Some((repo, _, _, _)) = selected {
                    open_browser(&repo_url(repo));
                }
            }
            KeyCode::Char('h') => {
                if let Some((repo, branch, _, _)) = selected {
                    let d = daemon.clone();
                    let repo = repo.to_string();
                    let tx = self.bg_tx.clone();
                    self.set_flash("Loading history…");
                    if is_repo_row {
                        // Repo row: show all-branch history
                        tokio::spawn(async move {
                            match d.get_history(&repo, None, 20).await {
                                Ok(entries) => {
                                    let _ = tx
                                        .send(SseUpdate::EnterHistory {
                                            repo,
                                            branch: None,
                                            entries,
                                        })
                                        .await;
                                }
                                Err(e) => {
                                    let _ = tx
                                        .send(SseUpdate::BackgroundResult {
                                            flash: e,
                                            resync: false,
                                        })
                                        .await;
                                }
                            }
                        });
                    } else {
                        let branch = branch.to_string();
                        tokio::spawn(async move {
                            match d.get_history(&repo, Some(&branch), 20).await {
                                Ok(entries) => {
                                    let _ = tx
                                        .send(SseUpdate::EnterHistory {
                                            repo,
                                            branch: Some(branch),
                                            entries,
                                        })
                                        .await;
                                }
                                Err(e) => {
                                    let _ = tx
                                        .send(SseUpdate::BackgroundResult {
                                            flash: e,
                                            resync: false,
                                        })
                                        .await;
                                }
                            }
                        });
                    }
                }
            }
            KeyCode::Char('H') => {
                if let Some((repo, _, _, _)) = selected {
                    let d = daemon.clone();
                    let repo = repo.to_string();
                    let tx = self.bg_tx.clone();
                    self.set_flash("Loading history…");
                    tokio::spawn(async move {
                        match d.get_history(&repo, None, 20).await {
                            Ok(entries) => {
                                let _ = tx
                                    .send(SseUpdate::EnterHistory {
                                        repo,
                                        branch: None,
                                        entries,
                                    })
                                    .await;
                            }
                            Err(e) => {
                                let _ = tx
                                    .send(SseUpdate::BackgroundResult {
                                        flash: e,
                                        resync: false,
                                    })
                                    .await;
                            }
                        }
                    });
                }
            }
            KeyCode::Char('s') => {
                if self.sort_ascending {
                    self.sort_ascending = false;
                } else {
                    self.sort_column = self.sort_column.next();
                    self.sort_ascending = true;
                }
                self.save_prefs();
            }
            KeyCode::Char('S') => {
                if !self.sort_ascending {
                    self.sort_ascending = true;
                } else {
                    self.sort_column = self.sort_column.prev();
                    self.sort_ascending = false;
                }
                self.save_prefs();
            }
            KeyCode::Char('g') => {
                self.group_by = self.group_by.next();
                self.save_prefs();
            }
            KeyCode::Char('G') => {
                self.group_by = self.group_by.prev();
                self.save_prefs();
            }
            KeyCode::Char('C') => {
                let d = daemon.clone();
                let tx = self.bg_tx.clone();
                self.set_flash("Loading config…");
                tokio::spawn(async move {
                    match d.get_defaults().await {
                        Ok(defaults) => {
                            let _ = tx
                                .send(SseUpdate::EnterForm {
                                    title: "Config".to_string(),
                                    fields: vec![
                                        FormField {
                                            label: "Default branches".to_string(),
                                            buffer: defaults.default_branches.join(", "),
                                            options: vec![],
                                        },
                                        FormField {
                                            label: "Ignored workflows".to_string(),
                                            buffer: defaults.ignored_workflows.join(", "),
                                            options: vec![],
                                        },
                                        FormField {
                                            label: "Poll aggression".to_string(),
                                            buffer: defaults.poll_aggression,
                                            options: vec!["low", "medium", "high"],
                                        },
                                    ],
                                })
                                .await;
                        }
                        Err(e) => {
                            let _ = tx
                                .send(SseUpdate::BackgroundResult {
                                    flash: e,
                                    resync: false,
                                })
                                .await;
                        }
                    }
                });
            }
            _ => {}
        }
        QuitAction::None
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
    /// Open the config form popup (used after fetching current defaults).
    EnterForm {
        title: String,
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
}

/// Tracks the SSE connection state for header display.
pub(crate) enum SseState {
    Connecting,
    Connected,
    Disconnected { since: Instant },
}

// -- Rows --

fn find_watch_mut<'a>(
    watches: &'a mut [WatchStatus],
    repo: &str,
    branch: &str,
) -> Option<&'a mut WatchStatus> {
    watches
        .iter_mut()
        .find(|w| w.repo == repo && w.branch == branch)
}

/// Apply a watch event to the local status snapshot.
///
/// Updates only watches that already exist in the snapshot; new watches
/// appear on the next `/status` resync.
pub(crate) fn apply_event(status: &mut StatusResponse, event: WatchEvent) {
    match event {
        WatchEvent::RunStarted(snap) => {
            let Some(watch) = find_watch_mut(&mut status.watches, &snap.repo, &snap.branch) else {
                return;
            };
            if !watch.active_runs.iter().any(|r| r.run_id == snap.run_id) {
                let title = snap.display_title();
                watch.active_runs.push(ActiveRunView {
                    run_id: snap.run_id,
                    status: snap.status,
                    workflow: snap.workflow,
                    title,
                    event: snap.event,
                    elapsed_secs: Some(0.0),
                });
            }
        }
        WatchEvent::RunCompleted {
            run,
            conclusion,
            failing_steps,
            ..
        } => {
            let Some(watch) = find_watch_mut(&mut status.watches, &run.repo, &run.branch) else {
                return;
            };
            watch.active_runs.retain(|r| r.run_id != run.run_id);
            let title = run.display_title();
            watch.last_build = Some(LastBuildView {
                run_id: run.run_id,
                conclusion,
                workflow: run.workflow,
                title,
                failing_steps,
                age_secs: Some(0.0),
            });
        }
        WatchEvent::StatusChanged { run, to, .. } => {
            let Some(watch) = find_watch_mut(&mut status.watches, &run.repo, &run.branch) else {
                return;
            };
            if let Some(active) = watch
                .active_runs
                .iter_mut()
                .find(|r| r.run_id == run.run_id)
            {
                active.status = to;
            }
        }
    }
}

// -- Rendering --

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::{
        ColWidths, DisplayRow, FlatRows, flatten_rows, sorted_watches, status_emoji, status_style,
    };
    use build_watcher::events::{RunSnapshot, WatchEvent};
    use build_watcher::status::{ActiveRunView, StatusResponse, WatchStatus};
    use ratatui::style::Color;
    use std::collections::HashSet;

    fn no_collapsed() -> HashSet<String> {
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
            status: "queued".to_string(),
        }
    }

    fn watch(repo: &str, branch: &str) -> WatchStatus {
        WatchStatus {
            repo: repo.to_string(),
            branch: branch.to_string(),
            active_runs: vec![],
            last_build: None,
            muted: false,
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
        apply_event(
            &mut status,
            WatchEvent::RunStarted(snap("alice/app", "main", 1)),
        );

        let runs = &status.watches[0].active_runs;
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].run_id, 1);
        assert_eq!(runs[0].status, "queued");
        assert_eq!(runs[0].workflow, "CI");
        assert_eq!(runs[0].elapsed_secs, Some(0.0));
    }

    #[test]
    fn run_started_dedup_same_run_id() {
        let mut status = status_with(vec![watch("alice/app", "main")]);
        apply_event(
            &mut status,
            WatchEvent::RunStarted(snap("alice/app", "main", 42)),
        );
        apply_event(
            &mut status,
            WatchEvent::RunStarted(snap("alice/app", "main", 42)),
        );

        assert_eq!(status.watches[0].active_runs.len(), 1);
    }

    #[test]
    fn run_started_ignores_unknown_watch() {
        let mut status = status_with(vec![watch("alice/app", "main")]);
        apply_event(
            &mut status,
            WatchEvent::RunStarted(snap("alice/app", "release", 1)),
        );

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
                status: "in_progress".to_string(),
                workflow: "CI".to_string(),
                title: "Fix bug".to_string(),
                event: "push".to_string(),
                elapsed_secs: Some(30.0),
            }],
            last_build: None,
            muted: false,
        }]);

        apply_event(
            &mut status,
            WatchEvent::RunCompleted {
                run: snap("alice/app", "main", 7),
                conclusion: "success".to_string(),
                elapsed: Some(35.0),
                failing_steps: None,
            },
        );

        assert!(status.watches[0].active_runs.is_empty());
        let lb = status.watches[0].last_build.as_ref().unwrap();
        assert_eq!(lb.run_id, 7);
        assert_eq!(lb.conclusion, "success");
        assert_eq!(lb.workflow, "CI");
        assert!(lb.failing_steps.is_none());
        assert_eq!(lb.age_secs, Some(0.0));
    }

    #[test]
    fn run_completed_sets_failing_steps() {
        let mut status = status_with(vec![watch("alice/app", "main")]);
        apply_event(
            &mut status,
            WatchEvent::RunCompleted {
                run: snap("alice/app", "main", 5),
                conclusion: "failure".to_string(),
                elapsed: None,
                failing_steps: Some("Build / tests".to_string()),
            },
        );

        let lb = status.watches[0].last_build.as_ref().unwrap();
        assert_eq!(lb.failing_steps.as_deref(), Some("Build / tests"));
    }

    #[test]
    fn run_completed_ignores_unknown_watch() {
        let mut status = status_with(vec![watch("alice/app", "main")]);
        apply_event(
            &mut status,
            WatchEvent::RunCompleted {
                run: snap("other/repo", "main", 1),
                conclusion: "success".to_string(),
                elapsed: None,
                failing_steps: None,
            },
        );

        assert!(status.watches[0].last_build.is_none());
    }

    // -- StatusChanged --

    #[test]
    fn status_changed_updates_active_run_status() {
        let mut status = status_with(vec![WatchStatus {
            repo: "alice/app".to_string(),
            branch: "main".to_string(),
            active_runs: vec![ActiveRunView {
                run_id: 3,
                status: "queued".to_string(),
                workflow: "CI".to_string(),
                title: "Fix bug".to_string(),
                event: "push".to_string(),
                elapsed_secs: None,
            }],
            last_build: None,
            muted: false,
        }]);

        apply_event(
            &mut status,
            WatchEvent::StatusChanged {
                run: snap("alice/app", "main", 3),
                from: "queued".to_string(),
                to: "in_progress".to_string(),
            },
        );

        assert_eq!(status.watches[0].active_runs[0].status, "in_progress");
    }

    #[test]
    fn status_changed_ignores_unknown_run_id() {
        let mut status = status_with(vec![WatchStatus {
            repo: "alice/app".to_string(),
            branch: "main".to_string(),
            active_runs: vec![ActiveRunView {
                run_id: 3,
                status: "queued".to_string(),
                workflow: "CI".to_string(),
                title: "Fix bug".to_string(),
                event: "push".to_string(),
                elapsed_secs: None,
            }],
            last_build: None,
            muted: false,
        }]);

        apply_event(
            &mut status,
            WatchEvent::StatusChanged {
                run: snap("alice/app", "main", 999),
                from: "queued".to_string(),
                to: "in_progress".to_string(),
            },
        );

        assert_eq!(status.watches[0].active_runs[0].status, "queued");
    }

    #[test]
    fn status_changed_ignores_unknown_watch() {
        let mut status = status_with(vec![watch("alice/app", "main")]);
        apply_event(
            &mut status,
            WatchEvent::StatusChanged {
                run: snap("other/repo", "main", 1),
                from: "queued".to_string(),
                to: "in_progress".to_string(),
            },
        );
        // No panic, no state change.
        assert!(status.watches[0].active_runs.is_empty());
    }

    // -- flatten_rows --

    #[test]
    fn flatten_rows_empty() {
        let flat = flatten_rows(&[], GroupBy::Org, &no_collapsed());
        assert!(flat.rows.is_empty());
        assert!(flat.selectable.is_empty());
    }

    #[test]
    fn flatten_rows_idle_watch() {
        let watches = vec![watch("alice/app", "main")];
        let flat = flatten_rows(&watches, GroupBy::Org, &no_collapsed());
        // GroupHeader + RepoHeader + NeverRan
        assert_eq!(flat.rows.len(), 3);
        assert_eq!(flat.selectable.len(), 2); // RepoHeader + NeverRan
        assert!(matches!(flat.rows[0], DisplayRow::GroupHeader { .. }));
        assert!(matches!(flat.rows[1], DisplayRow::RepoHeader { .. }));
        assert!(matches!(flat.rows[2], DisplayRow::NeverRan { .. }));
    }

    #[test]
    fn flatten_rows_with_failing_steps_not_selectable() {
        let watches = vec![WatchStatus {
            repo: "alice/app".to_string(),
            branch: "main".to_string(),
            active_runs: vec![],
            last_build: Some(LastBuildView {
                run_id: 1,
                conclusion: "failure".to_string(),
                workflow: "CI".to_string(),
                title: "Fix".to_string(),
                failing_steps: Some("Build / tests".to_string()),
                age_secs: Some(60.0),
            }),
            muted: false,
        }];
        let flat = flatten_rows(&watches, GroupBy::Org, &no_collapsed());
        // GroupHeader + RepoHeader + LastBuild + FailingSteps
        assert_eq!(flat.rows.len(), 4);
        // RepoHeader + LastBuild are selectable
        assert_eq!(flat.selectable.len(), 2);
        assert_eq!(flat.selectable[0], 1); // RepoHeader (index 1, after GroupHeader)
        assert_eq!(flat.selectable[1], 2); // LastBuild (index 2)
        assert!(matches!(flat.rows[3], DisplayRow::FailingSteps { .. }));
    }

    #[test]
    fn flatten_rows_success_no_failing_steps_row() {
        let watches = vec![WatchStatus {
            repo: "alice/app".to_string(),
            branch: "main".to_string(),
            active_runs: vec![],
            last_build: Some(LastBuildView {
                run_id: 1,
                conclusion: "success".to_string(),
                workflow: "CI".to_string(),
                title: "Fix".to_string(),
                failing_steps: None,
                age_secs: Some(60.0),
            }),
            muted: false,
        }];
        let flat = flatten_rows(&watches, GroupBy::Org, &no_collapsed());
        // GroupHeader + RepoHeader + LastBuild
        assert_eq!(flat.rows.len(), 3);
        assert!(matches!(flat.rows[1], DisplayRow::RepoHeader { .. }));
        assert!(matches!(flat.rows[2], DisplayRow::LastBuild { .. }));
    }

    // -- status_style / status_emoji --

    #[test]
    fn status_style_colors() {
        assert_eq!(status_style("success").fg, Some(Color::Green));
        assert_eq!(status_style("failure").fg, Some(Color::Red));
        assert_eq!(status_style("cancelled").fg, Some(Color::Red));
        assert_eq!(status_style("in_progress").fg, Some(Color::Yellow));
        assert_eq!(status_style("queued").fg, Some(Color::Yellow));
        assert_eq!(status_style("unknown").fg, None);
    }

    #[test]
    fn status_emoji_variants() {
        assert_eq!(status_emoji("success"), "✅");
        assert_eq!(status_emoji("failure"), "❌");
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
    fn display_row_repo_branch_run() {
        let watches = vec![WatchStatus {
            repo: "alice/app".to_string(),
            branch: "main".to_string(),
            active_runs: vec![ActiveRunView {
                run_id: 42,
                status: "in_progress".to_string(),
                workflow: "CI".to_string(),
                title: "Fix".to_string(),
                event: "push".to_string(),
                elapsed_secs: Some(10.0),
            }],
            last_build: None,
            muted: false,
        }];
        let flat = flatten_rows(&watches, GroupBy::Org, &no_collapsed());
        // Row 0: GroupHeader, Row 1: RepoHeader, Row 2: ActiveRun
        let (repo, branch, run_id, _muted) = flat.rows[2].repo_branch_run();
        assert_eq!(repo, "alice/app");
        assert_eq!(branch, "main");
        assert_eq!(run_id, Some(42));
    }

    #[test]
    fn run_completed_sets_display_title_for_pr() {
        let mut pr_snap = snap("alice/app", "main", 10);
        pr_snap.event = "pull_request".to_string();
        pr_snap.title = "Add feature".to_string();

        let mut status = status_with(vec![watch("alice/app", "main")]);
        apply_event(
            &mut status,
            WatchEvent::RunCompleted {
                run: pr_snap,
                conclusion: "success".to_string(),
                elapsed: None,
                failing_steps: None,
            },
        );

        let lb = status.watches[0].last_build.as_ref().unwrap();
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

    fn watch_with_build(repo: &str, branch: &str, conclusion: &str, age: f64) -> WatchStatus {
        WatchStatus {
            repo: repo.to_string(),
            branch: branch.to_string(),
            active_runs: vec![],
            last_build: Some(LastBuildView {
                run_id: 1,
                conclusion: conclusion.to_string(),
                workflow: "CI".to_string(),
                title: "Fix".to_string(),
                failing_steps: None,
                age_secs: Some(age),
            }),
            muted: false,
        }
    }

    fn watch_with_active(repo: &str, branch: &str, status: &str, elapsed: f64) -> WatchStatus {
        WatchStatus {
            repo: repo.to_string(),
            branch: branch.to_string(),
            active_runs: vec![ActiveRunView {
                run_id: 1,
                status: status.to_string(),
                workflow: "Deploy".to_string(),
                title: "Ship".to_string(),
                event: "push".to_string(),
                elapsed_secs: Some(elapsed),
            }],
            last_build: None,
            muted: false,
        }
    }

    #[test]
    fn sorted_watches_by_repo() {
        let watches = vec![
            watch_with_build("zoo/app", "main", "success", 10.0),
            watch_with_build("alice/lib", "main", "failure", 20.0),
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
            watch_with_build("alice/app", "release", "success", 10.0),
            watch_with_build("alice/app", "develop", "success", 20.0),
            watch_with_build("alice/app", "main", "success", 30.0),
        ];
        let sorted = sorted_watches(&watches, SortColumn::Branch, true, GroupBy::None);
        assert_eq!(sorted[0].branch, "develop");
        assert_eq!(sorted[1].branch, "main");
        assert_eq!(sorted[2].branch, "release");
    }

    #[test]
    fn sorted_watches_by_status_active_before_completed() {
        let watches = vec![
            watch_with_build("alice/app", "main", "success", 10.0),
            watch_with_active("bob/lib", "main", "in_progress", 5.0),
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
            watch_with_build("alice/app", "main", "success", 10.0), // CI
            watch_with_active("bob/lib", "main", "in_progress", 5.0), // Deploy
        ];
        let sorted = sorted_watches(&watches, SortColumn::Workflow, true, GroupBy::None);
        assert_eq!(sorted[0].repo, "alice/app"); // CI < Deploy
        assert_eq!(sorted[1].repo, "bob/lib");
    }

    #[test]
    fn sorted_watches_by_age() {
        let watches = vec![
            watch_with_build("alice/app", "main", "success", 100.0),
            watch_with_build("bob/lib", "main", "failure", 10.0),
            watch_with_active("carol/api", "main", "in_progress", 5.0),
        ];
        let sorted = sorted_watches(&watches, SortColumn::Age, true, GroupBy::None);
        assert_eq!(sorted[0].repo, "carol/api"); // 5s elapsed
        assert_eq!(sorted[1].repo, "bob/lib"); // 10s age
        assert_eq!(sorted[2].repo, "alice/app"); // 100s age
    }

    #[test]
    fn sorted_watches_descending_reverses() {
        let watches = vec![
            watch_with_build("alice/app", "main", "success", 10.0),
            watch_with_build("bob/lib", "main", "failure", 100.0),
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
            watch_with_build("alice/app", "main", "success", 10.0),
            watch_with_build("alice/lib", "main", "success", 20.0),
            watch_with_build("bob/api", "main", "failure", 30.0),
        ];
        let flat = flatten_rows(&watches, GroupBy::Org, &no_collapsed());
        assert_eq!(group_header_labels(&flat), vec!["alice", "bob"]);
    }

    #[test]
    fn flatten_rows_group_by_branch() {
        let watches = vec![
            watch_with_build("alice/app", "main", "success", 10.0),
            watch_with_build("alice/app", "develop", "success", 20.0),
            watch_with_build("bob/lib", "main", "failure", 30.0),
        ];
        let sorted = sorted_watches(&watches, SortColumn::Branch, true, GroupBy::None);
        let flat = flatten_rows(&sorted, GroupBy::Branch, &no_collapsed());
        assert_eq!(group_header_labels(&flat), vec!["develop", "main"]);
    }

    #[test]
    fn flatten_rows_group_by_status() {
        let watches = vec![
            watch_with_build("alice/app", "main", "success", 10.0),
            watch_with_build("bob/lib", "main", "failure", 20.0),
            watch_with_active("carol/api", "main", "in_progress", 5.0),
        ];
        let sorted = sorted_watches(&watches, SortColumn::Status, true, GroupBy::None);
        let flat = flatten_rows(&sorted, GroupBy::Status, &no_collapsed());
        let labels = group_header_labels(&flat);
        assert_eq!(labels.len(), 3);
        assert_eq!(labels[0], "in progress"); // active tier
        assert_eq!(labels[1], "failure"); // completed, alphabetical
        assert_eq!(labels[2], "success");
    }

    #[test]
    fn flatten_rows_group_by_none() {
        let watches = vec![
            watch_with_build("alice/app", "main", "success", 10.0),
            watch_with_build("bob/lib", "main", "failure", 20.0),
        ];
        let flat = flatten_rows(&watches, GroupBy::None, &no_collapsed());
        assert_eq!(count_group_headers(&flat), 0);
        // 2 RepoHeaders + 2 branch rows (no group headers)
        assert_eq!(flat.rows.len(), 4);
    }

    #[test]
    fn flatten_rows_group_by_workflow() {
        let watches = vec![
            watch_with_build("alice/app", "main", "success", 10.0), // CI
            watch_with_active("bob/lib", "main", "in_progress", 5.0), // Deploy
            watch("carol/api", "main"),                             // no workflow
        ];
        let flat = flatten_rows(&watches, GroupBy::Workflow, &no_collapsed());
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
}
