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

impl App {
    /// Submit the config form fields to the daemon.
    pub(crate) fn submit_config_form(&mut self, daemon: &DaemonClient) {
        let InputMode::Form { fields, .. } = &self.input_mode else {
            return;
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
        let auto_discover: Option<bool> = fields
            .iter()
            .find(|f| f.label == "Auto-discover branches")
            .map(|f| f.buffer == "on");
        let branch_filter: Option<String> = fields
            .iter()
            .find(|f| f.label == "Branch filter")
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
            default_branches: Some(branches),
            ignored_workflows: Some(workflows),
            poll_aggression: aggression,
            auto_discover_branches: auto_discover,
            branch_filter,
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
                            fields: vec![
                                FormField {
                                    label: "Default branches".to_string(),
                                    buffer: defaults
                                        .default_branches
                                        .unwrap_or_default()
                                        .join(", "),
                                    options: vec![],
                                },
                                FormField {
                                    label: "Ignored workflows".to_string(),
                                    buffer: defaults
                                        .ignored_workflows
                                        .unwrap_or_default()
                                        .join(", "),
                                    options: vec![],
                                },
                                FormField {
                                    label: "Poll aggression".to_string(),
                                    buffer: defaults.poll_aggression.unwrap_or_default(),
                                    options: vec!["low", "medium", "high"],
                                },
                                FormField {
                                    label: "Auto-discover branches".to_string(),
                                    buffer: if defaults.auto_discover_branches.unwrap_or(false) {
                                        "on".to_string()
                                    } else {
                                        "off".to_string()
                                    },
                                    options: vec!["off", "on"],
                                },
                                FormField {
                                    label: "Branch filter".to_string(),
                                    buffer: defaults.branch_filter.unwrap_or_default(),
                                    options: vec![],
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
}
