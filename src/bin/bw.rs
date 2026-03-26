//! `bw` — live terminal dashboard for the build-watcher daemon.
//!
//! Subscribes to `GET /events` (SSE) for real-time updates and resyncs
//! from `GET /status` on connect and every 30 seconds as a fallback.

use std::future::Future;
use std::time::{Duration, Instant};

use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use crossterm::{ExecutableCommand, execute};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Table};
use tokio::sync::mpsc;
use tokio_stream::StreamExt as _;

use build_watcher::config::{NotificationConfig, NotificationLevel, state_dir};
use build_watcher::events::WatchEvent;
use build_watcher::format;
use build_watcher::github::{repo_url, run_url, validate_branch, validate_repo};
use build_watcher::status::{
    ActiveRunView, LastBuildView, StatsResponse, StatusResponse, WatchStatus,
};

// -- Daemon client --

/// HTTP client for the build-watcher daemon REST API.
#[derive(Clone)]
struct DaemonClient {
    client: reqwest::Client,
    port: u16,
}

impl DaemonClient {
    fn new(port: u16) -> Self {
        Self {
            client: reqwest::Client::new(),
            port,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }

    async fn get_json<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T, String> {
        let resp = self
            .client
            .get(self.url(path))
            .send()
            .await
            .map_err(|e| format!("connect: {e}"))?;
        resp.json::<T>().await.map_err(|e| format!("parse: {e}"))
    }

    async fn post_json(&self, path: &str, body: &serde_json::Value) -> Result<(), String> {
        let resp = self
            .client
            .post(self.url(path))
            .json(body)
            .send()
            .await
            .map_err(|e| format!("{path}: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("{path}: HTTP {}", resp.status()));
        }
        // Daemon handlers return {"error": "..."} on validation failures (with 200 status).
        let json: serde_json::Value = resp.json().await.map_err(|e| format!("{path}: {e}"))?;
        if let Some(err) = json.get("error").and_then(|v| v.as_str()) {
            return Err(err.to_string());
        }
        Ok(())
    }

    async fn pause(&self, pause: bool) -> Result<(), String> {
        self.post_json("/pause", &serde_json::json!({ "pause": pause }))
            .await
    }

    async fn watch(&self, repo: &str) -> Result<(), String> {
        self.post_json("/watch", &serde_json::json!({ "repos": [repo] }))
            .await
    }

    async fn unwatch(&self, repo: &str) -> Result<(), String> {
        self.post_json("/unwatch", &serde_json::json!({ "repos": [repo] }))
            .await
    }

    async fn set_notifications(
        &self,
        repo: &str,
        branch: &str,
        action: &str,
    ) -> Result<(), String> {
        self.post_json(
            "/notifications",
            &serde_json::json!({ "repo": repo, "branch": branch, "action": action }),
        )
        .await
    }

    async fn get_notifications(
        &self,
        repo: &str,
        branch: &str,
    ) -> Result<NotificationConfig, String> {
        let resp = self
            .client
            .get(self.url("/notifications"))
            .query(&[("repo", repo), ("branch", branch)])
            .send()
            .await
            .map_err(|e| format!("connect: {e}"))?;
        resp.json::<NotificationConfig>()
            .await
            .map_err(|e| format!("parse: {e}"))
    }

    async fn set_notification_levels(
        &self,
        repo: &str,
        branch: &str,
        started: NotificationLevel,
        success: NotificationLevel,
        failure: NotificationLevel,
    ) -> Result<(), String> {
        self.post_json(
            "/notifications",
            &serde_json::json!({
                "repo": repo,
                "branch": branch,
                "action": "set_levels",
                "build_started": started.to_string(),
                "build_success": success.to_string(),
                "build_failure": failure.to_string(),
            }),
        )
        .await
    }

    async fn set_branches(&self, repo: &str, branches: &[String]) -> Result<(), String> {
        self.post_json(
            "/branches",
            &serde_json::json!({ "repo": repo, "branches": branches }),
        )
        .await
    }

    async fn shutdown(&self) -> Result<(), String> {
        self.post_json("/shutdown", &serde_json::json!({})).await
    }

    async fn get_defaults(&self) -> Result<Defaults, String> {
        self.get_json("/defaults").await
    }

    async fn set_defaults(
        &self,
        default_branches: Option<Vec<String>>,
        ignored_workflows: Option<Vec<String>>,
    ) -> Result<(), String> {
        self.post_json(
            "/defaults",
            &serde_json::json!({
                "default_branches": default_branches,
                "ignored_workflows": ignored_workflows,
            }),
        )
        .await
    }

    /// Inner client ref for the SSE background task (which needs `bytes_stream`).
    fn inner(&self) -> &reqwest::Client {
        &self.client
    }
}

/// Global defaults returned by `GET /defaults`.
#[derive(serde::Deserialize)]
struct Defaults {
    default_branches: Vec<String>,
    ignored_workflows: Vec<String>,
}

/// What to do when the user presses a quit key.
enum QuitAction {
    None,
    Quit,
    QuitAndShutdown,
}

// -- App state --

/// What the current text input prompt is for.
enum TextAction {
    AddRepo,
    SetBranches { repo: String },
}

/// A labeled text field in a form popup.
struct FormField {
    label: String,
    buffer: String,
}

/// Text input mode for interactive prompts (e.g. "Add repo: ").
enum InputMode {
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GroupBy {
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
    fn label(self) -> &'static str {
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SortColumn {
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

struct App {
    status: StatusResponse,
    stats: StatsResponse,
    /// When we last successfully fetched /status.
    last_fetch: Instant,
    /// Error message from the most recent failed fetch, if any.
    fetch_error: Option<String>,
    sse_state: SseState,
    /// Index into the selectable (non-sub-row) display rows.
    selected: usize,
    /// Transient feedback message shown in the header (e.g. "Adding…").
    flash: Option<(String, Instant)>,
    input_mode: InputMode,
    /// Sender for background task results back to the main loop.
    bg_tx: mpsc::Sender<SseUpdate>,
    sort_column: SortColumn,
    sort_ascending: bool,
    group_by: GroupBy,
}

impl App {
    fn active_count(&self) -> usize {
        self.status
            .watches
            .iter()
            .map(|w| w.active_runs.len())
            .sum()
    }

    fn set_flash(&mut self, msg: impl Into<String>) {
        self.flash = Some((msg.into(), Instant::now()));
    }

    async fn resync(&mut self, daemon: &DaemonClient) {
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
    }

    /// Advance local elapsed/age counters by one second between resyncs.
    fn tick_timers(&mut self) {
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
    fn handle_input(&mut self, code: KeyCode, daemon: &DaemonClient) -> bool {
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
                    KeyCode::Backspace => {
                        fields[*active].buffer.pop();
                    }
                    KeyCode::Char(c) => {
                        fields[*active].buffer.push(c);
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
            d.set_defaults(Some(branches), Some(workflows))
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
    fn handle_normal_key(
        &mut self,
        code: KeyCode,
        modifiers: KeyModifiers,
        daemon: &DaemonClient,
    ) -> QuitAction {
        let flat = flatten_rows(&self.status.watches, self.group_by);
        let sel_count = flat.selectable.len();
        let selected = flat
            .selectable
            .get(self.selected)
            .map(|&idx| flat.rows[idx].repo_branch_run());

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
                    let branch = branch.to_string();
                    let action = if muted { "unmute" } else { "mute" };
                    let verb = if muted { "Unmuted" } else { "Muted" };
                    let label = format!("{repo}/{branch}");
                    self.spawn_action(format!("{verb} {label}…"), true, async move {
                        d.set_notifications(&repo, &branch, action)
                            .await
                            .map(|()| format!("{verb} {label}"))
                    });
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
                if let Some((repo, _, Some(run_id), _)) = selected {
                    open_browser(&run_url(repo, run_id));
                }
            }
            KeyCode::Char('O') => {
                if let Some((repo, _, _, _)) = selected {
                    open_browser(&repo_url(repo));
                }
            }
            KeyCode::Char('s') => {
                if self.sort_ascending {
                    self.sort_ascending = false;
                } else {
                    self.sort_column = self.sort_column.next();
                    self.sort_ascending = true;
                }
            }
            KeyCode::Char('S') => {
                if !self.sort_ascending {
                    self.sort_ascending = true;
                } else {
                    self.sort_column = self.sort_column.prev();
                    self.sort_ascending = false;
                }
            }
            KeyCode::Char('g') => {
                self.group_by = self.group_by.next();
            }
            KeyCode::Char('G') => {
                self.group_by = self.group_by.prev();
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
                                        },
                                        FormField {
                                            label: "Ignored workflows".to_string(),
                                            buffer: defaults.ignored_workflows.join(", "),
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
enum SseUpdate {
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
}

/// Tracks the SSE connection state for header display.
enum SseState {
    Connecting,
    Connected,
    Disconnected { since: Instant },
}

// -- Rows --

/// A flattened display row derived from the status snapshot.
enum DisplayRow<'a> {
    GroupHeader {
        label: String,
    },
    ActiveRun {
        repo: &'a str,
        branch: &'a str,
        run: &'a ActiveRunView,
        /// Pre-computed badge for extra active runs, e.g. "+2⏸" or "+1⏳ +1⏸".
        /// Empty when this is the only active run.
        extra_badge: String,
        muted: bool,
    },
    FailingSteps {
        steps: &'a str,
    },
    LastBuild {
        repo: &'a str,
        branch: &'a str,
        build: &'a LastBuildView,
        muted: bool,
    },
    NeverRan {
        repo: &'a str,
        branch: &'a str,
        muted: bool,
    },
}

/// Result of flattening watches into display rows.
struct FlatRows<'a> {
    rows: Vec<DisplayRow<'a>>,
    /// Indices into `rows` that are selectable (everything except `FailingSteps`).
    selectable: Vec<usize>,
}

/// Group key as a sortable string (used to ensure items with the same group are contiguous).
fn group_key_for_sort(w: &WatchStatus, group_by: GroupBy) -> String {
    group_key(w, group_by).unwrap_or_default()
}

/// Extract the group key for a watch based on the grouping mode.
fn group_key(w: &WatchStatus, group_by: GroupBy) -> Option<String> {
    match group_by {
        GroupBy::Org => Some(w.repo.split('/').next().unwrap_or(&w.repo).to_string()),
        GroupBy::Branch => Some(w.branch.clone()),
        GroupBy::Workflow => {
            let wf = watch_workflow(w);
            if wf.is_empty() {
                Some("(none)".to_string())
            } else {
                Some(wf.to_string())
            }
        }
        GroupBy::Status => {
            let (tier, status) = watch_status(w);
            Some(if tier <= 1 {
                status.to_string()
            } else {
                "idle".to_string()
            })
        }
        GroupBy::None => None,
    }
}

fn flatten_rows(watches: &[WatchStatus], group_by: GroupBy) -> FlatRows<'_> {
    let mut rows = Vec::new();
    let mut selectable = Vec::new();
    let mut current_group: Option<String> = None;

    for w in watches {
        if let Some(key) = group_key(w, group_by)
            && current_group.as_deref() != Some(&key)
        {
            current_group = Some(key.clone());
            rows.push(DisplayRow::GroupHeader { label: key });
        }

        if w.active_runs.is_empty() {
            match &w.last_build {
                Some(b) => {
                    selectable.push(rows.len());
                    rows.push(DisplayRow::LastBuild {
                        repo: &w.repo,
                        branch: &w.branch,
                        build: b,
                        muted: w.muted,
                    });
                    if b.conclusion != "success"
                        && let Some(steps) = &b.failing_steps
                    {
                        rows.push(DisplayRow::FailingSteps { steps });
                    }
                }
                None => {
                    selectable.push(rows.len());
                    rows.push(DisplayRow::NeverRan {
                        repo: &w.repo,
                        branch: &w.branch,
                        muted: w.muted,
                    });
                }
            }
        } else {
            // Prefer in_progress as the primary row; fall back to the last (newest) run.
            let primary_idx = w
                .active_runs
                .iter()
                .rposition(|r| r.status == "in_progress")
                .unwrap_or(w.active_runs.len() - 1);
            let primary = &w.active_runs[primary_idx];
            let extra_badge = extra_runs_badge(&w.active_runs, primary_idx);
            selectable.push(rows.len());
            rows.push(DisplayRow::ActiveRun {
                repo: &w.repo,
                branch: &w.branch,
                run: primary,
                extra_badge,
                muted: w.muted,
            });
        }
    }
    FlatRows { rows, selectable }
}

impl DisplayRow<'_> {
    /// Returns `(repo, branch, run_id, muted)` for the selected row. Only valid for selectable rows.
    fn repo_branch_run(&self) -> (&str, &str, Option<u64>, bool) {
        match self {
            DisplayRow::ActiveRun {
                repo,
                branch,
                run,
                muted,
                ..
            } => (repo, branch, Some(run.run_id), *muted),
            DisplayRow::LastBuild {
                repo,
                branch,
                build,
                muted,
            } => (repo, branch, Some(build.run_id), *muted),
            DisplayRow::NeverRan {
                repo,
                branch,
                muted,
            } => (repo, branch, None, *muted),
            DisplayRow::GroupHeader { .. } | DisplayRow::FailingSteps { .. } => {
                unreachable!("not selectable")
            }
        }
    }
}

/// Sort watches by the selected column. Returns a new sorted vec.
/// When `group_by` is active, the group key is used as the primary sort key
/// so that items in the same group are contiguous for header insertion.
fn sorted_watches(
    watches: &[WatchStatus],
    column: SortColumn,
    ascending: bool,
    group_by: GroupBy,
) -> Vec<WatchStatus> {
    let mut sorted = watches.to_vec();
    sorted.sort_by(|a, b| {
        // Group key as primary sort when grouping is active.
        let group_ord = match group_by {
            GroupBy::None => std::cmp::Ordering::Equal,
            _ => group_key_for_sort(a, group_by).cmp(&group_key_for_sort(b, group_by)),
        };
        if group_ord != std::cmp::Ordering::Equal {
            return group_ord;
        }
        let cmp = match column {
            SortColumn::Repo => a.repo.cmp(&b.repo).then(a.branch.cmp(&b.branch)),
            SortColumn::Branch => a.branch.cmp(&b.branch).then(a.repo.cmp(&b.repo)),
            SortColumn::Status => {
                let sa = watch_status(a);
                let sb = watch_status(b);
                sa.cmp(&sb)
            }
            SortColumn::Workflow => {
                let wa = watch_workflow(a);
                let wb = watch_workflow(b);
                wa.cmp(wb)
            }
            SortColumn::Age => {
                let aa = watch_age(a);
                let ab = watch_age(b);
                aa.partial_cmp(&ab).unwrap_or(std::cmp::Ordering::Equal)
            }
        };
        if ascending { cmp } else { cmp.reverse() }
    });
    sorted
}

/// Build a compact badge summarising the non-primary active runs.
///
/// Returns an empty string when there is only one run (primary_idx is the sole element).
/// Examples: `"+2⏸"`, `"+1⏳ +2⏸"`.
fn extra_runs_badge(runs: &[ActiveRunView], primary_idx: usize) -> String {
    if runs.len() <= 1 {
        return String::new();
    }
    let mut in_progress = 0usize;
    let mut queued = 0usize;
    let mut other = 0usize;
    for (i, r) in runs.iter().enumerate() {
        if i == primary_idx {
            continue;
        }
        match r.status.as_str() {
            "in_progress" => in_progress += 1,
            "queued" | "waiting" | "requested" | "pending" => queued += 1,
            _ => other += 1,
        }
    }
    let mut parts = Vec::new();
    if in_progress > 0 {
        parts.push(format!("+{in_progress}⏳"));
    }
    if queued > 0 {
        parts.push(format!("+{queued}⏸"));
    }
    if other > 0 {
        parts.push(format!("+{other}·"));
    }
    parts.join(" ")
}

/// Status key: active runs (tier 0), completed (tier 1), idle (tier 2).
fn watch_status(w: &WatchStatus) -> (u8, &str) {
    if let Some(run) = w.active_runs.first() {
        (0, &run.status)
    } else if let Some(b) = &w.last_build {
        (1, &b.conclusion)
    } else {
        (2, "")
    }
}

fn watch_workflow(w: &WatchStatus) -> &str {
    if let Some(run) = w.active_runs.first() {
        &run.workflow
    } else if let Some(b) = &w.last_build {
        &b.workflow
    } else {
        ""
    }
}

/// Age/elapsed key: active run elapsed, completed build age, or MAX for idle.
fn watch_age(w: &WatchStatus) -> f64 {
    if let Some(run) = w.active_runs.first() {
        run.elapsed_secs.unwrap_or(f64::MAX)
    } else if let Some(b) = &w.last_build {
        b.age_secs.unwrap_or(f64::MAX)
    } else {
        f64::MAX
    }
}

/// Extract just the repo name (after the '/') for display.
fn short_repo(repo: &str) -> &str {
    repo.rsplit_once('/').map_or(repo, |(_, name)| name)
}

// -- Event application --

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
fn apply_event(status: &mut StatusResponse, event: WatchEvent) {
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

fn status_style(conclusion_or_status: &str) -> Style {
    match conclusion_or_status {
        "success" => Style::default().fg(Color::Green),
        "failure" | "cancelled" | "timed_out" | "startup_failure" => {
            Style::default().fg(Color::Red)
        }
        "in_progress" | "queued" | "waiting" | "requested" | "pending" => {
            Style::default().fg(Color::Yellow)
        }
        _ => Style::default(),
    }
}

fn status_emoji(conclusion_or_status: &str) -> &'static str {
    match conclusion_or_status {
        "success" => "✅",
        "failure" | "cancelled" | "timed_out" | "startup_failure" => "❌",
        "in_progress" => "⏳",
        "queued" | "waiting" | "requested" | "pending" => "⏸",
        _ => "·",
    }
}

// -- Responsive column layout --

const COL_SPACING: u16 = 1;
const NUM_GAPS: usize = 5; // 6 columns → 5 gaps

// Fixed column widths (content is bounded, no truncation needed).
const BRANCH_W: usize = 12;
const STATUS_W: usize = 18;
const AGE_W: usize = 14;
const FIXED_W: usize = BRANCH_W + STATUS_W + AGE_W + NUM_GAPS * COL_SPACING as usize;

/// Variable column widths computed from terminal width.
struct ColWidths {
    repo: usize,
    workflow: usize,
    title: usize,
}

impl ColWidths {
    fn from_terminal_width(w: u16) -> Self {
        // Remaining space split among repo, workflow, title (30% / 25% / 45%).
        let remaining = (w as usize).saturating_sub(FIXED_W);
        let repo = (remaining * 30 / 100).max(10);
        let workflow = (remaining * 25 / 100).max(8);
        let title = remaining.saturating_sub(repo + workflow).max(8);

        Self {
            repo,
            workflow,
            title,
        }
    }

    fn constraints(&self) -> [Constraint; 6] {
        [
            Constraint::Length(self.repo as u16),
            Constraint::Length(BRANCH_W as u16),
            Constraint::Length(STATUS_W as u16),
            Constraint::Length(self.workflow as u16),
            Constraint::Min(self.title as u16),
            Constraint::Length(AGE_W as u16),
        ]
    }
}

const FLASH_DURATION: Duration = Duration::from_secs(3);

fn render_header(frame: &mut ratatui::Frame, area: ratatui::layout::Rect, app: &App) {
    let w = area.width as usize;
    let dim = Style::default().fg(Color::DarkGray);

    // Line 1: title + stats
    let s = &app.stats;
    let uptime = format::seconds(s.uptime_secs);
    let poll = format!("poll {}s/{}s", s.active_poll_secs, s.idle_poll_secs);
    let api = match (s.rate_remaining, s.rate_limit) {
        (Some(rem), Some(lim)) => {
            let pct = if lim > 0 { rem * 100 / lim } else { 0 };
            let reset = s
                .rate_reset_mins
                .map(|m| format!("  reset {m}m"))
                .unwrap_or_default();
            format!("API {rem}/{lim} ({pct}%){reset}")
        }
        _ => "API —".to_string(),
    };

    let left1_suffix = format!(" — up {uptime}");
    let right1 = format!("{poll}  {api}");
    let left1_len = "build-watcher".len() + left1_suffix.len();
    let gap1 = w.saturating_sub(left1_len + right1.len());
    let line1 = Line::from(vec![
        Span::styled(
            "build-watcher",
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::raw(left1_suffix),
        Span::raw(" ".repeat(gap1)),
        Span::styled(right1, dim),
    ]);

    // Line 2: watches + state
    let repo_count = app.status.watches.len();
    let active_count = app.active_count();
    let group_label = if app.group_by != GroupBy::Org {
        format!("  group: {}", app.group_by.label())
    } else {
        String::new()
    };
    let mut left2_spans: Vec<Span> = vec![Span::raw(format!(
        "{repo_count} repos, {active_count} active{group_label}"
    ))];
    if app.status.paused {
        left2_spans.push(Span::styled(
            "  ⏸ NOTIFS PAUSED",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
    }
    match &app.sse_state {
        SseState::Connecting => {
            left2_spans.push(Span::styled(
                "  ⚡ connecting…",
                Style::default().fg(Color::Yellow),
            ));
        }
        SseState::Disconnected { since } => {
            left2_spans.push(Span::styled(
                format!("  ⚡ reconnecting ({}s)", since.elapsed().as_secs()),
                Style::default().fg(Color::Yellow),
            ));
        }
        SseState::Connected => {}
    }
    if let Some(err) = &app.fetch_error {
        let stale_secs = app.last_fetch.elapsed().as_secs();
        left2_spans.push(Span::styled(
            format!("  ⚠ {err} ({stale_secs}s stale)"),
            Style::default().fg(Color::Red),
        ));
    }
    if let Some((msg, at)) = &app.flash
        && at.elapsed() < FLASH_DURATION
    {
        left2_spans.push(Span::styled(
            format!("  {msg}"),
            Style::default().fg(Color::Cyan),
        ));
    }
    let line2 = Line::from(left2_spans);

    // Line 3: separator
    let line3 = Line::from(Span::styled("─".repeat(w), dim));

    frame.render_widget(Paragraph::new(vec![line1, line2, line3]), area);
}

fn render_body(
    frame: &mut ratatui::Frame,
    heading_area: ratatui::layout::Rect,
    body_area: ratatui::layout::Rect,
    app: &App,
    cw: &ColWidths,
) {
    let header_style = Style::default()
        .fg(Color::DarkGray)
        .add_modifier(Modifier::BOLD);
    let active_style = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    let arrow = if app.sort_ascending { " ▲" } else { " ▼" };
    let hdr = |label: &str, col: SortColumn| -> Cell<'_> {
        if app.sort_column == col {
            Cell::from(format!("{label}{arrow}")).style(active_style)
        } else {
            Cell::from(label.to_string()).style(header_style)
        }
    };
    let col_header = Row::new(vec![
        hdr("REPO", SortColumn::Repo),
        hdr("BRANCH", SortColumn::Branch),
        hdr("STATUS", SortColumn::Status),
        hdr("WORKFLOW", SortColumn::Workflow),
        Cell::from("TITLE").style(header_style),
        hdr("ELAPSED / AGE", SortColumn::Age),
    ]);

    let sorted = sorted_watches(
        &app.status.watches,
        app.sort_column,
        app.sort_ascending,
        app.group_by,
    );
    let flat = flatten_rows(&sorted, app.group_by);
    let selected_display_idx = flat
        .selectable
        .get(app.selected)
        .copied()
        .unwrap_or(usize::MAX);
    let highlight_style = Style::default().bg(Color::DarkGray);

    let mute_indicator = |muted: bool| -> &'static str { if muted { " 🔇" } else { "" } };

    let rows: Vec<Row> = flat
        .rows
        .iter()
        .enumerate()
        .map(|(i, dr)| {
            let row = render_display_row(dr, cw, &mute_indicator);
            if i == selected_display_idx {
                row.style(highlight_style)
            } else {
                row
            }
        })
        .collect();

    let widths = cw.constraints();

    let heading_table = Table::new(vec![col_header], widths).column_spacing(COL_SPACING);
    frame.render_widget(heading_table, heading_area);

    let body_table = Table::new(rows, widths).column_spacing(COL_SPACING);
    frame.render_widget(body_table, body_area);
}

fn render_display_row<'a>(
    dr: &DisplayRow<'_>,
    cw: &ColWidths,
    mute_indicator: &dyn Fn(bool) -> &'static str,
) -> Row<'a> {
    match dr {
        DisplayRow::GroupHeader { label } => Row::new(vec![
            Cell::from(label.clone()).style(
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Cell::from(""),
            Cell::from(""),
            Cell::from(""),
            Cell::from(""),
            Cell::from(""),
        ]),
        DisplayRow::ActiveRun {
            repo,
            branch,
            run,
            extra_badge,
            muted,
        } => {
            let style = status_style(&run.status);
            let emoji = status_emoji(&run.status);
            let elapsed = run
                .elapsed_secs
                .map(|s| format::duration(Duration::from_secs_f64(s)))
                .unwrap_or_default();
            let name = format!("  {}{}", short_repo(repo), mute_indicator(*muted));
            let status_text = if extra_badge.is_empty() {
                format!("{emoji} {}", format::status(&run.status))
            } else {
                format!("{emoji} {} {extra_badge}", format::status(&run.status))
            };
            Row::new(vec![
                Cell::from(format::truncate(&name, cw.repo)),
                Cell::from(format::truncate(branch, BRANCH_W)),
                Cell::from(format::truncate(&status_text, STATUS_W)).style(style),
                Cell::from(format::truncate(&run.workflow, cw.workflow)),
                Cell::from(format::truncate(&run.title, cw.title)),
                Cell::from(elapsed).style(style),
            ])
        }
        DisplayRow::FailingSteps { steps } => Row::new(vec![
            Cell::from(""),
            Cell::from(""),
            Cell::from(""),
            Cell::from(""),
            Cell::from(format!("  ↳ {}", format::truncate(steps, cw.title)))
                .style(Style::default().fg(Color::Red)),
            Cell::from(""),
        ]),
        DisplayRow::LastBuild {
            repo,
            branch,
            build,
            muted,
        } => {
            let style = status_style(&build.conclusion);
            let emoji = status_emoji(&build.conclusion);
            let age = build
                .age_secs
                .map(|s| format::age(s as u64))
                .unwrap_or_default();
            let name = format!("  {}{}", short_repo(repo), mute_indicator(*muted));
            Row::new(vec![
                Cell::from(format::truncate(&name, cw.repo)),
                Cell::from(format::truncate(branch, BRANCH_W)),
                Cell::from(format!("{emoji} {}", format::status(&build.conclusion))).style(style),
                Cell::from(format::truncate(&build.workflow, cw.workflow)),
                Cell::from(format::truncate(&build.title, cw.title)),
                Cell::from(age).style(style),
            ])
        }
        DisplayRow::NeverRan {
            repo,
            branch,
            muted,
        } => {
            let name = format!("  {}{}", short_repo(repo), mute_indicator(*muted));
            Row::new(vec![
                Cell::from(format::truncate(&name, cw.repo)),
                Cell::from(format::truncate(branch, BRANCH_W)),
                Cell::from("· idle").style(Style::default().fg(Color::DarkGray)),
                Cell::from(""),
                Cell::from(""),
                Cell::from(""),
            ])
        }
    }
}

fn render_footer(frame: &mut ratatui::Frame, area: ratatui::layout::Rect, app: &App) {
    let key_style = Style::default()
        .fg(Color::DarkGray)
        .add_modifier(Modifier::BOLD);
    let footer = match &app.input_mode {
        InputMode::TextInput { prompt, buffer, .. } => Paragraph::new(Line::from(vec![
            Span::styled(prompt.as_str(), Style::default().fg(Color::Cyan)),
            Span::raw(buffer.as_str()),
            Span::styled("█", Style::default().fg(Color::Cyan)),
            Span::styled(
                "  [Enter] confirm  [Esc] cancel",
                Style::default().fg(Color::DarkGray),
            ),
        ])),
        InputMode::Form { .. } | InputMode::NotificationPicker { .. } => Paragraph::new(""),
        InputMode::Normal => Paragraph::new(Line::from(vec![
            Span::styled("[↑↓]", key_style),
            Span::raw(" select  "),
            Span::styled("[a]", key_style),
            Span::raw(" add  "),
            Span::styled("[b]", key_style),
            Span::raw(" branches  "),
            Span::styled("[d]", key_style),
            Span::raw(" remove  "),
            Span::styled("[o/O]", key_style),
            Span::raw(" open  "),
            Span::styled("[n/N]", key_style),
            Span::raw(" mute/levels  "),
            Span::styled("[p]", key_style),
            Span::raw(" pause  "),
            Span::styled("[s/S]", key_style),
            Span::raw(" sort  "),
            Span::styled("[g/G]", key_style),
            Span::raw(" group  "),
            Span::styled("[C]", key_style),
            Span::raw(" config  "),
            Span::styled("[q]", key_style),
            Span::raw(" quit  "),
            Span::styled("[Q]", key_style),
            Span::raw(" quit+stop"),
        ]))
        .style(Style::default().fg(Color::DarkGray)),
    };
    frame.render_widget(footer, area);
}

fn render(frame: &mut ratatui::Frame, app: &App) {
    let area = frame.area();
    let cw = ColWidths::from_terminal_width(area.width);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // header (2 info lines + separator)
            Constraint::Length(1), // column headings
            Constraint::Min(0),    // table body
            Constraint::Length(1), // footer
        ])
        .split(area);

    render_header(frame, chunks[0], app);
    render_body(frame, chunks[1], chunks[2], app, &cw);
    render_footer(frame, chunks[3], app);

    // Overlay the form popup if active.
    if let InputMode::Form {
        title,
        fields,
        active,
    } = &app.input_mode
    {
        render_form_popup(frame, title, fields, *active);
    }

    // Overlay the notification picker popup if active.
    if let InputMode::NotificationPicker {
        repo,
        branch,
        levels,
        active,
    } = &app.input_mode
    {
        render_notification_picker_popup(frame, repo, branch, levels, *active);
    }
}

/// Compute a centered rectangle of `percent_w` x height within `area`.
fn centered_rect(
    percent_w: u16,
    height: u16,
    area: ratatui::layout::Rect,
) -> ratatui::layout::Rect {
    let w = (area.width as u32 * percent_w as u32 / 100).min(area.width as u32) as u16;
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let h = height.min(area.height);
    ratatui::layout::Rect::new(x, y, w, h)
}

fn render_form_popup(frame: &mut ratatui::Frame, title: &str, fields: &[FormField], active: usize) {
    // 3 lines per field (label + input + blank) + 2 for border + 2 for footer hints
    let inner_height = fields.len() as u16 * 3 + 1;
    let popup_height = inner_height + 2; // borders
    let popup = centered_rect(60, popup_height, frame.area());

    frame.render_widget(Clear, popup);

    let block = Block::default()
        .title(format!(" {title} "))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let label_style = Style::default().fg(Color::DarkGray);
    let active_label_style = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    let cursor_style = Style::default().fg(Color::Cyan);

    let mut constraints: Vec<Constraint> = Vec::new();
    for _ in fields {
        constraints.push(Constraint::Length(1)); // label
        constraints.push(Constraint::Length(1)); // input
        constraints.push(Constraint::Length(1)); // spacing
    }
    // Replace last spacing with the footer hint
    if let Some(last) = constraints.last_mut() {
        *last = Constraint::Length(1);
    }

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(inner);

    for (i, field) in fields.iter().enumerate() {
        let base = i * 3;
        let is_active = i == active;
        let style = if is_active {
            active_label_style
        } else {
            label_style
        };

        // Label
        let label = Paragraph::new(Line::from(Span::styled(&field.label, style)));
        frame.render_widget(label, rows[base]);

        // Input line
        let input_text = if is_active {
            Line::from(vec![
                Span::raw(&field.buffer),
                Span::styled("█", cursor_style),
            ])
        } else {
            Line::from(Span::raw(&field.buffer))
        };
        frame.render_widget(Paragraph::new(input_text), rows[base + 1]);
    }

    // Footer hint in the last row
    let hint_row = fields.len() * 3 - 1;
    if hint_row < rows.len() {
        let hint = Paragraph::new(Line::from(vec![
            Span::styled(
                "[Tab]",
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" next  ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                "[Enter]",
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" save  ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                "[Esc]",
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" cancel", Style::default().fg(Color::DarkGray)),
        ]));
        frame.render_widget(hint, rows[hint_row]);
    }
}

fn render_notification_picker_popup(
    frame: &mut ratatui::Frame,
    repo: &str,
    branch: &str,
    levels: &[NotificationLevel; 3],
    active: usize,
) {
    // 3 data rows + 1 blank top + 1 blank bottom + 1 hint + 2 borders = 8
    let popup_height = 8u16;
    let popup = centered_rect(55, popup_height, frame.area());

    frame.render_widget(Clear, popup);

    let title = format!(" Notifications: {} @ {} ", repo, branch);
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // blank
            Constraint::Length(1), // started
            Constraint::Length(1), // success
            Constraint::Length(1), // failure
            Constraint::Length(1), // blank
            Constraint::Length(1), // hint
        ])
        .split(inner);

    let labels = ["Build started", "Build success", "Build failure"];
    let normal_style = Style::default().fg(Color::DarkGray);
    let active_style = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);

    for (i, (label, level)) in labels.iter().zip(levels.iter()).enumerate() {
        let is_active = i == active;
        let row_style = if is_active {
            active_style
        } else {
            normal_style
        };
        let arrow = if is_active { "▸ " } else { "  " };
        let level_str = format!("[{:^8}]", level.to_string());
        let line = Line::from(vec![
            Span::styled(format!("{arrow}{label:<16}"), row_style),
            Span::styled(level_str, row_style),
        ]);
        frame.render_widget(Paragraph::new(line), rows[i + 1]);
    }

    let hint = Line::from(vec![
        Span::styled(
            "[←/→]",
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" cycle  ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            "[Enter]",
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" save  ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            "[Esc]",
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" cancel", Style::default().fg(Color::DarkGray)),
    ]);
    frame.render_widget(Paragraph::new(hint), rows[5]);
}

// -- SSE background task --

/// Connect to `GET /events` and forward parsed events to `tx` until the
/// stream ends or the channel closes.
///
/// Sets `*connected` to `true` once the HTTP response is received so the
/// caller can distinguish a connection failure from a mid-stream disconnect.
async fn stream_sse(
    client: &reqwest::Client,
    port: u16,
    tx: &mpsc::Sender<SseUpdate>,
    connected: &mut bool,
) -> bool {
    let url = format!("http://127.0.0.1:{port}/events");
    let response = match client.get(&url).send().await {
        Ok(r) => r,
        Err(_) => return false,
    };

    *connected = true;
    if tx.send(SseUpdate::Connected).await.is_err() {
        return true; // channel closed — main task exited
    }

    let mut stream = response.bytes_stream();
    let mut buf = String::new();
    let mut pending_data: Option<String> = None;

    while let Some(chunk) = stream.next().await {
        let bytes = match chunk {
            Ok(b) => b,
            Err(_) => return false,
        };
        buf.push_str(&String::from_utf8_lossy(&bytes));

        while let Some(pos) = buf.find('\n') {
            let line = buf[..pos].trim_end_matches('\r').to_string();
            buf.drain(..=pos);

            if line.is_empty() {
                // End of SSE frame — dispatch accumulated data.
                if let Some(data) = pending_data.take()
                    && let Ok(event) = serde_json::from_str::<WatchEvent>(&data)
                    && tx.send(SseUpdate::Event(Box::new(event))).await.is_err()
                {
                    return true;
                }
            } else if let Some(data) = line.strip_prefix("data: ") {
                pending_data = Some(data.to_string());
                // Lines starting with "event:", "id:", or ":" (comments) are ignored.
            }
        }
    }

    false // stream ended cleanly
}

/// SSE background task: connects, streams events, reconnects with exponential backoff.
async fn sse_task(client: reqwest::Client, port: u16, tx: mpsc::Sender<SseUpdate>) {
    let mut backoff_secs = 1u64;
    loop {
        let mut connected = false;
        if stream_sse(&client, port, &tx, &mut connected).await {
            break; // channel closed
        }
        if tx.send(SseUpdate::Disconnected).await.is_err() {
            break;
        }
        if connected {
            backoff_secs = 1; // successful connection — reset backoff
        } else {
            backoff_secs = (backoff_secs * 2).min(30);
        }
        tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
    }
}

// -- Actions --

fn open_browser(url: &str) {
    let cmd = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let _ = std::process::Command::new(cmd)
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

// -- Entry point --

/// Read the daemon port from the port file, or start the daemon if it's not running.
fn discover_or_start_daemon() -> Result<u16, Box<dyn std::error::Error>> {
    let port_file = state_dir().join("port");

    // Try reading existing port file first.
    if let Ok(contents) = std::fs::read_to_string(&port_file)
        && let Ok(port) = contents.trim().parse::<u16>()
    {
        if std::net::TcpStream::connect(format!("127.0.0.1:{port}")).is_ok() {
            return Ok(port);
        }
        // Port file exists but daemon is not responding — stale file.
        let _ = std::fs::remove_file(&port_file);
    }

    // Daemon not running — try to start it.
    eprintln!("Daemon not running, starting build-watcher…");
    let exe = std::env::current_exe()?;
    let daemon_bin = exe
        .parent()
        .ok_or("cannot resolve binary directory")?
        .join("build-watcher");

    if !daemon_bin.exists() {
        return Err(format!(
            "build-watcher binary not found at {}\nInstall it with ./install.sh",
            daemon_bin.display()
        )
        .into());
    }

    std::process::Command::new(&daemon_bin)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("Failed to start daemon: {e}"))?;

    // Wait for the port file to appear (up to 5 seconds).
    for _ in 0..50 {
        std::thread::sleep(std::time::Duration::from_millis(100));
        if let Ok(contents) = std::fs::read_to_string(&port_file)
            && let Ok(port) = contents.trim().parse::<u16>()
            && std::net::TcpStream::connect(format!("127.0.0.1:{port}")).is_ok()
        {
            return Ok(port);
        }
    }

    Err("Timed out waiting for daemon to start".into())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let port = discover_or_start_daemon()?;

    // Initial fetch so there's something to display before SSE connects.
    let daemon = DaemonClient::new(port);
    let initial = daemon
        .get_json::<StatusResponse>("/status")
        .await
        .unwrap_or_else(|e| {
            eprintln!("Warning: could not fetch initial status: {e}");
            StatusResponse {
                paused: false,
                watches: vec![],
            }
        });
    let initial_stats = daemon
        .get_json::<StatsResponse>("/stats")
        .await
        .unwrap_or_default();

    // Shared channel for SSE events and background action results.
    let (sse_tx, mut sse_rx) = mpsc::channel::<SseUpdate>(64);

    let mut app = App {
        status: initial,
        stats: initial_stats,
        last_fetch: Instant::now(),
        fetch_error: None,
        sse_state: SseState::Connecting,
        selected: 0,
        flash: None,
        input_mode: InputMode::Normal,
        bg_tx: sse_tx.clone(),
        sort_column: SortColumn::Repo,
        sort_ascending: true,
        group_by: GroupBy::Org,
    };

    // Terminal setup.
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    tokio::spawn(sse_task(daemon.inner().clone(), port, sse_tx));

    let mut keyboard = EventStream::new();

    // 1-second tick to advance elapsed times locally between resyncs.
    let mut elapsed_tick = tokio::time::interval(Duration::from_secs(1));
    elapsed_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    // 30-second resync as a fallback guard against missed events.
    // First tick is delayed so it doesn't duplicate the on-connect resync.
    let mut resync_tick = tokio::time::interval_at(
        tokio::time::Instant::now() + Duration::from_secs(30),
        Duration::from_secs(30),
    );
    resync_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let result = async {
        loop {
            terminal.draw(|f| render(f, &app))?;

            tokio::select! {
                _ = elapsed_tick.tick() => {
                    app.tick_timers();
                }
                _ = resync_tick.tick() => {
                    app.resync(&daemon).await;
                }
                maybe_update = sse_rx.recv() => {
                    match maybe_update {
                        Some(SseUpdate::Event(event)) => {
                            apply_event(&mut app.status, *event);
                        }
                        Some(SseUpdate::Connected) => {
                            app.sse_state = SseState::Connected;
                            app.resync(&daemon).await;
                        }
                        Some(SseUpdate::Disconnected) => {
                            app.sse_state = SseState::Disconnected { since: Instant::now() };
                        }
                        Some(SseUpdate::BackgroundResult { flash, resync }) => {
                            app.set_flash(flash);
                            if resync {
                                app.resync(&daemon).await;
                            }
                        }
                        Some(SseUpdate::EnterForm { title, fields }) => {
                            app.input_mode = InputMode::Form {
                                title,
                                fields,
                                active: 0,
                            };
                        }
                        Some(SseUpdate::EnterNotificationPicker {
                            repo,
                            branch,
                            levels,
                        }) => {
                            app.input_mode = InputMode::NotificationPicker {
                                repo,
                                branch,
                                levels,
                                active: 0,
                            };
                        }
                        None => {}
                    }
                }
                maybe_event = keyboard.next() => {
                    match maybe_event {
                        Some(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press => {
                            if app.handle_input(key.code, &daemon) {
                                continue;
                            }
                            match app.handle_normal_key(key.code, key.modifiers, &daemon) {
                                QuitAction::Quit => break,
                                QuitAction::QuitAndShutdown => {
                                    let _ = daemon.shutdown().await;
                                    break;
                                }
                                QuitAction::None => {}
                            }
                        }
                        Some(Ok(Event::Resize(_, _))) => {} // triggers redraw at top of loop
                        Some(Err(e)) => return Err(e.into()),
                        _ => {}
                    }
                }
            }
        }
        Ok::<_, Box<dyn std::error::Error>>(())
    }
    .await;

    // Always restore the terminal, even on error.
    disable_raw_mode()?;
    terminal.backend_mut().execute(LeaveAlternateScreen)?;

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use build_watcher::events::{RunSnapshot, WatchEvent};
    use build_watcher::status::{ActiveRunView, StatusResponse, WatchStatus};

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
        let flat = flatten_rows(&[], GroupBy::Org);
        assert!(flat.rows.is_empty());
        assert!(flat.selectable.is_empty());
    }

    #[test]
    fn flatten_rows_idle_watch() {
        let watches = vec![watch("alice/app", "main")];
        let flat = flatten_rows(&watches, GroupBy::Org);
        assert_eq!(flat.rows.len(), 2); // GroupHeader + NeverRan
        assert_eq!(flat.selectable.len(), 1);
        assert!(matches!(flat.rows[0], DisplayRow::GroupHeader { .. }));
        assert!(matches!(flat.rows[1], DisplayRow::NeverRan { .. }));
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
        let flat = flatten_rows(&watches, GroupBy::Org);
        // GroupHeader + LastBuild + FailingSteps
        assert_eq!(flat.rows.len(), 3);
        // Only the LastBuild row is selectable
        assert_eq!(flat.selectable.len(), 1);
        assert_eq!(flat.selectable[0], 1); // index 1 (after GroupHeader)
        assert!(matches!(flat.rows[2], DisplayRow::FailingSteps { .. }));
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
        let flat = flatten_rows(&watches, GroupBy::Org);
        assert_eq!(flat.rows.len(), 2); // GroupHeader + LastBuild
        assert!(matches!(flat.rows[1], DisplayRow::LastBuild { .. }));
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
        let flat = flatten_rows(&watches, GroupBy::Org);
        // First row is GroupHeader, second is the active run
        let (repo, branch, run_id, _muted) = flat.rows[1].repo_branch_run();
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
        let flat = flatten_rows(&watches, GroupBy::Org);
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
        let flat = flatten_rows(&sorted, GroupBy::Branch);
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
        let flat = flatten_rows(&sorted, GroupBy::Status);
        let labels = group_header_labels(&flat);
        assert_eq!(labels.len(), 3);
        assert_eq!(labels[0], "in_progress"); // active tier
        assert_eq!(labels[1], "failure"); // completed, alphabetical
        assert_eq!(labels[2], "success");
    }

    #[test]
    fn flatten_rows_group_by_none() {
        let watches = vec![
            watch_with_build("alice/app", "main", "success", 10.0),
            watch_with_build("bob/lib", "main", "failure", 20.0),
        ];
        let flat = flatten_rows(&watches, GroupBy::None);
        assert_eq!(count_group_headers(&flat), 0);
        // Just the watch rows, no headers
        assert_eq!(flat.rows.len(), 2);
    }

    #[test]
    fn flatten_rows_group_by_workflow() {
        let watches = vec![
            watch_with_build("alice/app", "main", "success", 10.0), // CI
            watch_with_active("bob/lib", "main", "in_progress", 5.0), // Deploy
            watch("carol/api", "main"),                             // no workflow
        ];
        let flat = flatten_rows(&watches, GroupBy::Workflow);
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
