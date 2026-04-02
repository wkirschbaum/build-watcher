use build_watcher::config::NotificationLevel;
use build_watcher::github::{validate_branch, validate_repo};
use build_watcher::status::{DefaultsConfig, HistoryEntryView};

use super::app::{App, SseUpdate};
use super::client::DaemonClient;

/// Split a comma-separated string into a trimmed, non-empty list.
pub(crate) fn parse_csv(s: &str) -> Vec<String> {
    s.split(',')
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .collect()
}

/// What the current text input prompt is for.
pub(crate) enum TextAction {
    AddRepo,
    SetBranches { repo: String },
}

/// A labeled field in a form popup.
pub(crate) struct FormField {
    pub(crate) label: String,
    pub(crate) editor: LineEditor,
    /// If non-empty, this is a cycle field (Left/Right to cycle, no free-text entry).
    pub(crate) options: Vec<&'static str>,
}

impl FormField {
    pub(crate) fn text(label: impl Into<String>, buffer: String) -> Self {
        Self {
            label: label.into(),
            editor: LineEditor::new(buffer),
            options: vec![],
        }
    }

    pub(crate) fn cycle(
        label: impl Into<String>,
        buffer: String,
        options: Vec<&'static str>,
    ) -> Self {
        Self {
            label: label.into(),
            editor: LineEditor::new(buffer),
            options,
        }
    }

    pub(crate) fn buffer(&self) -> &str {
        &self.editor.buf
    }
}

// ── Readline-style line editor ──────────────────────────────────────────

/// A minimal line editor with readline-compatible cursor movement and editing.
#[derive(Debug, Clone)]
pub(crate) struct LineEditor {
    pub(crate) buf: String,
    /// Cursor byte offset within `buf`.
    pub(crate) cursor: usize,
}

impl LineEditor {
    pub(crate) fn new(buf: String) -> Self {
        let cursor = buf.len();
        Self { buf, cursor }
    }

    pub(crate) fn empty() -> Self {
        Self {
            buf: String::new(),
            cursor: 0,
        }
    }

    // ── Cursor movement ─────────────────────────────────────────────

    /// Ctrl+A / Home
    pub(crate) fn move_home(&mut self) {
        self.cursor = 0;
    }

    /// Ctrl+E / End
    pub(crate) fn move_end(&mut self) {
        self.cursor = self.buf.len();
    }

    /// Ctrl+B / Left
    pub(crate) fn move_left(&mut self) {
        self.cursor = self.buf[..self.cursor]
            .char_indices()
            .next_back()
            .map(|(i, _)| i)
            .unwrap_or(0);
    }

    /// Ctrl+F / Right
    pub(crate) fn move_right(&mut self) {
        if self.cursor < self.buf.len() {
            self.cursor += self.buf[self.cursor..]
                .chars()
                .next()
                .map(|c| c.len_utf8())
                .unwrap_or(0);
        }
    }

    /// Alt+B / Alt+Left — move to start of previous word.
    pub(crate) fn move_word_left(&mut self) {
        let before = &self.buf[..self.cursor];
        let trimmed = before.trim_end();
        self.cursor = trimmed
            .rfind(|c: char| c.is_whitespace() || c == ',' || c == '/')
            .map(|i| i + trimmed[i..].chars().next().unwrap().len_utf8())
            .unwrap_or(0);
    }

    /// Alt+F / Alt+Right — move to end of next word.
    pub(crate) fn move_word_right(&mut self) {
        let after = &self.buf[self.cursor..];
        // Skip leading separators and whitespace
        let skip = after
            .find(|c: char| !c.is_whitespace() && c != ',' && c != '/')
            .unwrap_or(after.len());
        let rest = &after[skip..];
        let word_len = rest
            .find(|c: char| c.is_whitespace() || c == ',' || c == '/')
            .unwrap_or(rest.len());
        self.cursor += skip + word_len;
    }

    // ── Editing ─────────────────────────────────────────────────────

    /// Insert a character at cursor.
    pub(crate) fn insert(&mut self, c: char) {
        self.buf.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }

    /// Ctrl+D — delete character at cursor.
    pub(crate) fn delete(&mut self) {
        if self.cursor < self.buf.len() {
            let end = self.cursor
                + self.buf[self.cursor..]
                    .chars()
                    .next()
                    .map(|c| c.len_utf8())
                    .unwrap_or(0);
            self.buf.drain(self.cursor..end);
        }
    }

    /// Ctrl+H / Backspace — delete character before cursor.
    pub(crate) fn backspace(&mut self) {
        if self.cursor > 0 {
            let prev = self.buf[..self.cursor]
                .char_indices()
                .next_back()
                .map(|(i, _)| i)
                .unwrap_or(0);
            self.buf.drain(prev..self.cursor);
            self.cursor = prev;
        }
    }

    /// Ctrl+K — kill from cursor to end of line.
    pub(crate) fn kill_to_end(&mut self) {
        self.buf.truncate(self.cursor);
    }

    /// Ctrl+U — kill from beginning to cursor.
    pub(crate) fn kill_to_start(&mut self) {
        self.buf.drain(..self.cursor);
        self.cursor = 0;
    }

    /// Ctrl+W / Alt+Backspace — delete word before cursor.
    pub(crate) fn delete_word_left(&mut self) {
        let start = {
            let before = &self.buf[..self.cursor];
            let trimmed = before.trim_end();
            trimmed
                .rfind(|c: char| c.is_whitespace() || c == ',' || c == '/')
                .map(|i| i + trimmed[i..].chars().next().unwrap().len_utf8())
                .unwrap_or(0)
        };
        self.buf.drain(start..self.cursor);
        self.cursor = start;
    }

    /// Alt+D — delete word after cursor.
    pub(crate) fn delete_word_right(&mut self) {
        let end = {
            let after = &self.buf[self.cursor..];
            let skip = after
                .find(|c: char| !c.is_whitespace() && c != ',' && c != '/')
                .unwrap_or(after.len());
            let rest = &after[skip..];
            let word_len = rest
                .find(|c: char| c.is_whitespace() || c == ',' || c == '/')
                .unwrap_or(rest.len());
            self.cursor + skip + word_len
        };
        self.buf.drain(self.cursor..end);
    }

    /// Return (before_cursor, cursor_char, after_cursor) for rendering.
    pub(crate) fn split_at_cursor(&self) -> (&str, Option<char>, &str) {
        let (before, rest) = self.buf.split_at(self.cursor);
        let mut chars = rest.chars();
        match chars.next() {
            Some(c) => (before, Some(c), chars.as_str()),
            None => (before, None, ""),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_at_end() {
        let mut ed = LineEditor::new("hello".into());
        ed.insert('!');
        assert_eq!(ed.buf, "hello!");
        assert_eq!(ed.cursor, 6);
    }

    #[test]
    fn insert_at_middle() {
        let mut ed = LineEditor::new("hllo".into());
        ed.cursor = 1;
        ed.insert('e');
        assert_eq!(ed.buf, "hello");
        assert_eq!(ed.cursor, 2);
    }

    #[test]
    fn backspace_at_end() {
        let mut ed = LineEditor::new("hello".into());
        ed.backspace();
        assert_eq!(ed.buf, "hell");
        assert_eq!(ed.cursor, 4);
    }

    #[test]
    fn backspace_at_start_is_noop() {
        let mut ed = LineEditor::new("hello".into());
        ed.cursor = 0;
        ed.backspace();
        assert_eq!(ed.buf, "hello");
        assert_eq!(ed.cursor, 0);
    }

    #[test]
    fn backspace_in_middle() {
        let mut ed = LineEditor::new("hello".into());
        ed.cursor = 3;
        ed.backspace();
        assert_eq!(ed.buf, "helo");
        assert_eq!(ed.cursor, 2);
    }

    #[test]
    fn delete_at_cursor() {
        let mut ed = LineEditor::new("hello".into());
        ed.cursor = 1;
        ed.delete();
        assert_eq!(ed.buf, "hllo");
        assert_eq!(ed.cursor, 1);
    }

    #[test]
    fn delete_at_end_is_noop() {
        let mut ed = LineEditor::new("hello".into());
        ed.delete();
        assert_eq!(ed.buf, "hello");
    }

    #[test]
    fn move_left_and_right() {
        let mut ed = LineEditor::new("abc".into());
        assert_eq!(ed.cursor, 3);
        ed.move_left();
        assert_eq!(ed.cursor, 2);
        ed.move_left();
        assert_eq!(ed.cursor, 1);
        ed.move_right();
        assert_eq!(ed.cursor, 2);
    }

    #[test]
    fn move_left_at_start() {
        let mut ed = LineEditor::empty();
        ed.buf = "abc".into();
        ed.cursor = 0;
        ed.move_left();
        assert_eq!(ed.cursor, 0);
    }

    #[test]
    fn move_right_at_end() {
        let mut ed = LineEditor::new("abc".into());
        ed.move_right();
        assert_eq!(ed.cursor, 3);
    }

    #[test]
    fn home_and_end() {
        let mut ed = LineEditor::new("hello world".into());
        ed.cursor = 5;
        ed.move_home();
        assert_eq!(ed.cursor, 0);
        ed.move_end();
        assert_eq!(ed.cursor, 11);
    }

    #[test]
    fn kill_to_end() {
        let mut ed = LineEditor::new("hello world".into());
        ed.cursor = 5;
        ed.kill_to_end();
        assert_eq!(ed.buf, "hello");
        assert_eq!(ed.cursor, 5);
    }

    #[test]
    fn kill_to_start() {
        let mut ed = LineEditor::new("hello world".into());
        ed.cursor = 5;
        ed.kill_to_start();
        assert_eq!(ed.buf, " world");
        assert_eq!(ed.cursor, 0);
    }

    #[test]
    fn delete_word_left() {
        let mut ed = LineEditor::new("hello world".into());
        ed.delete_word_left();
        assert_eq!(ed.buf, "hello ");
        assert_eq!(ed.cursor, 6);
    }

    #[test]
    fn delete_word_left_with_comma_separator() {
        let mut ed = LineEditor::new("main, develop".into());
        ed.delete_word_left();
        assert_eq!(ed.buf, "main, ");
        assert_eq!(ed.cursor, 6);
    }

    #[test]
    fn delete_word_left_at_start() {
        let mut ed = LineEditor::new("hello".into());
        ed.cursor = 0;
        ed.delete_word_left();
        assert_eq!(ed.buf, "hello");
        assert_eq!(ed.cursor, 0);
    }

    #[test]
    fn delete_word_right() {
        let mut ed = LineEditor::new("hello world".into());
        ed.cursor = 0;
        ed.delete_word_right();
        assert_eq!(ed.buf, " world");
        assert_eq!(ed.cursor, 0);
    }

    #[test]
    fn delete_word_right_at_end() {
        let mut ed = LineEditor::new("hello".into());
        ed.delete_word_right();
        assert_eq!(ed.buf, "hello");
    }

    #[test]
    fn move_word_left() {
        let mut ed = LineEditor::new("hello world foo".into());
        ed.move_word_left();
        assert_eq!(ed.cursor, 12);
        ed.move_word_left();
        assert_eq!(ed.cursor, 6);
        ed.move_word_left();
        assert_eq!(ed.cursor, 0);
        // Already at start
        ed.move_word_left();
        assert_eq!(ed.cursor, 0);
    }

    #[test]
    fn move_word_right() {
        let mut ed = LineEditor::new("hello world foo".into());
        ed.cursor = 0;
        ed.move_word_right();
        assert_eq!(ed.cursor, 5);
        ed.move_word_right();
        assert_eq!(ed.cursor, 11);
        ed.move_word_right();
        assert_eq!(ed.cursor, 15);
        // Already at end
        ed.move_word_right();
        assert_eq!(ed.cursor, 15);
    }

    #[test]
    fn move_word_with_slash_separator() {
        let mut ed = LineEditor::new("owner/repo".into());
        ed.cursor = 0;
        ed.move_word_right();
        assert_eq!(ed.cursor, 5);
        ed.move_word_right();
        assert_eq!(ed.cursor, 10);
    }

    #[test]
    fn multibyte_char_handling() {
        let mut ed = LineEditor::new("café".into());
        assert_eq!(ed.cursor, 5); // 'é' is 2 bytes
        ed.move_left();
        assert_eq!(ed.cursor, 3);
        ed.move_right();
        assert_eq!(ed.cursor, 5);
        ed.backspace();
        assert_eq!(ed.buf, "caf");
        assert_eq!(ed.cursor, 3);
    }

    #[test]
    fn split_at_cursor_middle() {
        let ed = LineEditor {
            buf: "hello".into(),
            cursor: 2,
        };
        let (before, ch, after) = ed.split_at_cursor();
        assert_eq!(before, "he");
        assert_eq!(ch, Some('l'));
        assert_eq!(after, "lo");
    }

    #[test]
    fn split_at_cursor_end() {
        let ed = LineEditor::new("hello".into());
        let (before, ch, after) = ed.split_at_cursor();
        assert_eq!(before, "hello");
        assert_eq!(ch, None);
        assert_eq!(after, "");
    }

    #[test]
    fn empty_editor() {
        let mut ed = LineEditor::empty();
        ed.backspace();
        ed.delete();
        ed.move_left();
        ed.move_right();
        ed.move_word_left();
        ed.move_word_right();
        ed.kill_to_end();
        ed.kill_to_start();
        ed.delete_word_left();
        ed.delete_word_right();
        assert_eq!(ed.buf, "");
        assert_eq!(ed.cursor, 0);
    }

    #[test]
    fn full_editing_sequence() {
        let mut ed = LineEditor::empty();
        // Type "owner/repo"
        for c in "owner/repo".chars() {
            ed.insert(c);
        }
        assert_eq!(ed.buf, "owner/repo");
        assert_eq!(ed.cursor, 10);

        // Ctrl+A, then type "my-" at the start
        ed.move_home();
        for c in "my-".chars() {
            ed.insert(c);
        }
        assert_eq!(ed.buf, "my-owner/repo");

        // Ctrl+E, backspace 4 times to remove "repo"
        ed.move_end();
        for _ in 0..4 {
            ed.backspace();
        }
        assert_eq!(ed.buf, "my-owner/");

        // Type new name
        for c in "new-repo".chars() {
            ed.insert(c);
        }
        assert_eq!(ed.buf, "my-owner/new-repo");
    }
}

/// Distinguishes which form is open so submission dispatches correctly.
pub(crate) enum FormKind {
    GlobalDefaults,
    RepoConfig { repo: String },
}

/// Text input mode for interactive prompts (e.g. "Add repo: ").
pub(crate) enum InputMode {
    Normal,
    TextInput {
        prompt: String,
        editor: LineEditor,
        action: TextAction,
    },
    /// Multi-field form popup.
    Form {
        title: String,
        kind: FormKind,
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
    /// PR picker popup (opened with `M` when multiple PRs exist).
    PrPicker {
        repo: String,
        prs: Vec<PrPickerEntry>,
        selected: usize,
    },
}

/// Compact PR entry for the picker popup.
#[derive(Debug, Clone)]
pub(crate) struct PrPickerEntry {
    pub number: u64,
    pub title: String,
    pub author: String,
    pub merge_state: build_watcher::github::MergeState,
    pub draft: bool,
}

impl App {
    /// Submit the config form fields to the daemon.
    pub(crate) fn submit_config_form(&mut self, daemon: &DaemonClient) {
        let InputMode::Form { fields, .. } = &self.input_mode else {
            return;
        };

        let workflows: Vec<String> = fields
            .iter()
            .find(|f| f.label == "Ignored workflows")
            .map(|f| parse_csv(f.buffer()))
            .unwrap_or_default();
        let events: Vec<String> = fields
            .iter()
            .find(|f| f.label == "Ignored events")
            .map(|f| parse_csv(f.buffer()))
            .unwrap_or_default();
        let aggression: Option<String> = fields
            .iter()
            .find(|f| f.label == "Poll aggression")
            .map(|f| f.buffer().to_string());
        let auto_discover: Option<bool> = fields
            .iter()
            .find(|f| f.label == "Auto-discover branches")
            .map(|f| f.buffer() == "on");
        let branch_filter: Option<String> = fields
            .iter()
            .find(|f| f.label == "Branch filter")
            .map(|f| f.buffer().to_string());
        let show_author: Option<bool> = fields
            .iter()
            .find(|f| f.label == "Show author")
            .map(|f| f.buffer() == "on");

        if let Some(ref filter) = branch_filter
            && !filter.is_empty()
            && let Err(e) = regex::Regex::new(filter)
        {
            self.set_flash(format!("Invalid branch filter regex: {e}"));
            return;
        }

        let d = daemon.clone();
        self.input_mode = InputMode::Normal;
        let defaults = DefaultsConfig {
            ignored_workflows: Some(workflows),
            ignored_events: Some(events),
            poll_aggression: aggression,
            auto_discover_branches: auto_discover,
            branch_filter,
            show_author,
        };
        self.spawn_action("Saving config…", true, async move {
            d.set_defaults(&defaults)
                .await
                .map(|()| "Config saved".to_string())
        });
    }

    /// Open the build history popup for a repo, optionally scoped to a branch.
    pub(crate) fn open_history(&mut self, daemon: &DaemonClient, repo: &str, branch: Option<&str>) {
        let d = daemon.clone();
        let repo = repo.to_string();
        let branch_owned = branch.map(|b| b.to_string());
        let tx = self.bg_tx.clone();
        self.set_flash("Loading history…");
        tokio::spawn(async move {
            match d.get_history(&repo, branch_owned.as_deref(), 20).await {
                Ok(entries) => {
                    let _ = tx
                        .send(SseUpdate::EnterHistory {
                            repo,
                            branch: branch_owned,
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

    pub(crate) fn submit_text_input(
        &mut self,
        input: String,
        action: TextAction,
        daemon: &DaemonClient,
    ) {
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
                let branches = parse_csv(&input);
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

    /// Open per-repo config form (fetches current values from daemon).
    pub(crate) fn open_repo_config_form(&mut self, daemon: &DaemonClient, repo: &str) {
        let d = daemon.clone();
        let tx = self.bg_tx.clone();
        let repo = repo.to_string();
        self.set_flash("Loading repo config…");
        tokio::spawn(async move {
            match d.get_repo_config(&repo).await {
                Ok(rc) => {
                    let _ = tx
                        .send(SseUpdate::EnterForm {
                            title: format!("Repo: {repo}"),
                            kind: FormKind::RepoConfig { repo: repo.clone() },
                            fields: vec![
                                FormField::text("Alias", rc.alias.unwrap_or_default()),
                                FormField::cycle(
                                    "Watch PRs",
                                    if rc.watch_prs.unwrap_or(false) {
                                        "on".to_string()
                                    } else {
                                        "off".to_string()
                                    },
                                    vec!["off", "on"],
                                ),
                                FormField::cycle(
                                    "Poll aggression",
                                    rc.poll_aggression.unwrap_or_else(|| "default".to_string()),
                                    vec!["default", "low", "medium", "high"],
                                ),
                                FormField::cycle(
                                    "Auto-discover branches",
                                    match rc.auto_discover_branches {
                                        Some(true) => "on".to_string(),
                                        Some(false) => "off".to_string(),
                                        None => "default".to_string(),
                                    },
                                    vec!["default", "off", "on"],
                                ),
                                FormField::text(
                                    "Branch filter",
                                    rc.branch_filter.unwrap_or_default(),
                                ),
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

    /// Submit the per-repo config form to the daemon.
    pub(crate) fn submit_repo_config_form(&mut self, daemon: &DaemonClient) {
        let InputMode::Form {
            kind: FormKind::RepoConfig { repo },
            fields,
            ..
        } = &self.input_mode
        else {
            return;
        };
        let repo = repo.clone();

        let alias = fields
            .iter()
            .find(|f| f.label == "Alias")
            .map(|f| f.buffer().to_string());
        let watch_prs: Option<bool> = fields
            .iter()
            .find(|f| f.label == "Watch PRs")
            .map(|f| f.buffer() == "on");
        let poll_aggression: Option<String> = fields
            .iter()
            .find(|f| f.label == "Poll aggression")
            .map(|f| f.buffer().to_string());
        let auto_discover_branches: Option<bool> = fields
            .iter()
            .find(|f| f.label == "Auto-discover branches")
            .and_then(|f| match f.buffer() {
                "on" => Some(true),
                "off" => Some(false),
                _ => None, // "default" → None (inherit global)
            });
        let branch_filter: Option<String> = fields
            .iter()
            .find(|f| f.label == "Branch filter")
            .map(|f| f.buffer().to_string());

        let config = build_watcher::status::RepoConfigView {
            repo,
            alias,
            workflows: None,
            watch_prs,
            poll_aggression,
            auto_discover_branches,
            branch_filter,
        };

        let d = daemon.clone();
        self.input_mode = InputMode::Normal;
        self.spawn_action("Saving repo config…", true, async move {
            d.set_repo_config(&config)
                .await
                .map(|()| "Repo config saved".to_string())
        });
    }

    /// Open the config defaults form (fetches current values from daemon).
    pub(crate) fn open_config_form(&mut self, daemon: &DaemonClient) {
        let d = daemon.clone();
        let tx = self.bg_tx.clone();
        self.set_flash("Loading config…");
        tokio::spawn(async move {
            match d.get_defaults().await {
                Ok(defaults) => {
                    let _ = tx
                        .send(SseUpdate::EnterForm {
                            title: "Config".to_string(),
                            kind: FormKind::GlobalDefaults,
                            fields: vec![
                                FormField::text(
                                    "Ignored workflows",
                                    defaults.ignored_workflows.unwrap_or_default().join(", "),
                                ),
                                FormField::text(
                                    "Ignored events",
                                    defaults.ignored_events.unwrap_or_default().join(", "),
                                ),
                                FormField::cycle(
                                    "Poll aggression",
                                    defaults.poll_aggression.unwrap_or_default(),
                                    vec!["low", "medium", "high"],
                                ),
                                FormField::cycle(
                                    "Auto-discover branches",
                                    if defaults.auto_discover_branches.unwrap_or(false) {
                                        "on".to_string()
                                    } else {
                                        "off".to_string()
                                    },
                                    vec!["off", "on"],
                                ),
                                FormField::text(
                                    "Branch filter",
                                    defaults.branch_filter.unwrap_or_default(),
                                ),
                                FormField::cycle(
                                    "Show author",
                                    if defaults.show_author.unwrap_or(true) {
                                        "on".to_string()
                                    } else {
                                        "off".to_string()
                                    },
                                    vec!["off", "on"],
                                ),
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
