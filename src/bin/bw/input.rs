use std::collections::HashSet;

use crossterm::event::{KeyCode, KeyModifiers};

use build_watcher::config::NOTIFICATION_EVENT_COUNT;
use build_watcher::github::{job_url, repo_url, run_url};
use build_watcher::status::WatchStatus;

use super::app::{App, ExpandLevel, FormKind, QuitAction, SseUpdate};
use super::client::{DaemonClient, open_browser};
use super::forms::{InputMode, LineEditor, TextAction};
use super::render::flatten_rows;

impl App {
    /// Handle a key press while in a non-normal input mode.
    /// Returns `true` if the event was consumed.
    pub(crate) fn handle_input(
        &mut self,
        code: KeyCode,
        modifiers: KeyModifiers,
        daemon: &DaemonClient,
    ) -> bool {
        let ctrl = modifiers.contains(KeyModifiers::CONTROL);
        let alt = modifiers.contains(KeyModifiers::ALT);

        match &mut self.input_mode {
            InputMode::Normal => false,
            InputMode::TextInput { editor, action, .. } => {
                match code {
                    KeyCode::Esc => {
                        self.input_mode = InputMode::Normal;
                    }
                    KeyCode::Enter => {
                        let input = editor.buf.trim().to_string();
                        let action = std::mem::replace(action, TextAction::AddRepo);
                        self.input_mode = InputMode::Normal;
                        if !input.is_empty() {
                            self.submit_text_input(input, action, daemon);
                        }
                    }
                    _ => handle_line_edit(editor, code, ctrl, alt),
                }
                true
            }
            InputMode::Form {
                kind,
                fields,
                active,
                ..
            } => {
                let f = &mut fields[*active];
                let is_cycle = !f.options.is_empty();
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
                    KeyCode::Right | KeyCode::Char(' ') if is_cycle && !ctrl && !alt => {
                        let idx = f
                            .options
                            .iter()
                            .position(|&o| o == f.editor.buf)
                            .unwrap_or(0);
                        f.editor.buf = f.options[(idx + 1) % f.options.len()].to_string();
                    }
                    KeyCode::Left if is_cycle && !ctrl && !alt => {
                        let n = f.options.len();
                        let idx = f
                            .options
                            .iter()
                            .position(|&o| o == f.editor.buf)
                            .unwrap_or(0);
                        f.editor.buf = f.options[(idx + n - 1) % n].to_string();
                    }
                    KeyCode::Enter => match kind {
                        FormKind::GlobalDefaults => self.submit_config_form(daemon),
                        FormKind::RepoConfig { .. } => self.submit_repo_config_form(daemon),
                    },
                    _ if !is_cycle => {
                        handle_line_edit(&mut f.editor, code, ctrl, alt);
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
                        *active = (*active + 1) % NOTIFICATION_EVENT_COUNT;
                    }
                    KeyCode::BackTab | KeyCode::Up => {
                        *active =
                            (*active + NOTIFICATION_EVENT_COUNT - 1) % NOTIFICATION_EVENT_COUNT;
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
                    KeyCode::Char('r') | KeyCode::Char('R') => {
                        if let Some(entry) = entries.get(*selected) {
                            let run_id = entry.id;
                            let repo = repo.clone();
                            let failed_only = code == KeyCode::Char('r');
                            let d = daemon.clone();
                            let label = if failed_only {
                                "failed jobs"
                            } else {
                                "all jobs"
                            };
                            self.input_mode = InputMode::Normal;
                            self.spawn_action(
                                format!("Rerunning {label} for run {run_id}…"),
                                false,
                                async move { d.rerun(&repo, Some(run_id), failed_only).await },
                            );
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
        let flat = flatten_rows(
            &sorted,
            self.group_by,
            &self.expand,
            &self.workflow_collapsed,
        );
        let sel_count = flat.selectable.len();
        let selected_display_idx = flat.selectable.get(self.selected).copied();
        let selected = selected_display_idx.and_then(|idx| flat.rows[idx].repo_branch_run());
        let row = selected_display_idx.map(|idx| &flat.rows[idx]);
        let is_repo_row = row.is_some_and(|r| r.is_repo_header());
        let is_branch_header = row.is_some_and(|r| r.is_branch_header());
        let is_workflow_child = row.is_some_and(|r| r.is_workflow_child());
        let is_failed = row.is_some_and(|r| r.is_failed());
        let failing_job_id = row.and_then(|r| r.failing_job_id());

        match code {
            // -- Quit / Navigation --
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
            // -- Sort / Group --
            KeyCode::Char('s') | KeyCode::Char('S') => {
                self.cycle_sort(code == KeyCode::Char('S'));
            }
            KeyCode::Char('g') => {
                self.group_by = self.group_by.next();
                self.save_prefs();
            }
            KeyCode::Char('G') => {
                self.group_by = self.group_by.prev();
                self.save_prefs();
            }
            // -- Expand / Collapse --
            // Repo row (multi-branch): cycle Collapsed → Branches → Full
            // Skip Full when no branch has multiple workflows (nothing to show).
            KeyCode::Tab | KeyCode::Enter if is_repo_row && !row.unwrap().is_single_branch() => {
                if let Some((repo, _, _, _)) = selected {
                    let has_workflows = repo_has_multi_workflow_branch(&self.status.watches, repo);
                    let next = self.expand_level(repo).next_expand(has_workflows);
                    self.set_expand_level(repo, next);
                    self.save_prefs();
                }
            }
            // Branch header: toggle workflow children visible/hidden
            KeyCode::Tab | KeyCode::Enter if is_branch_header => {
                if let Some((repo, branch, _, _)) = selected {
                    let key = format!("{repo}#{branch}");
                    if self.expand_level(repo) != ExpandLevel::Full {
                        // First expand the repo to Full so workflows are visible
                        self.set_expand_level(repo, ExpandLevel::Full);
                    } else if !self.workflow_collapsed.remove(&key) {
                        self.workflow_collapsed.insert(key);
                    }
                    self.save_prefs();
                }
            }
            // Workflow row: no toggle
            KeyCode::Tab | KeyCode::Enter if is_workflow_child => {}
            KeyCode::BackTab | KeyCode::Char('E') => {
                self.handle_expand_all();
            }
            // -- Actions --
            _ => {
                self.handle_action_key(
                    code,
                    selected,
                    is_repo_row,
                    is_failed,
                    failing_job_id,
                    daemon,
                );
            }
        }
        QuitAction::None
    }

    /// Handle BackTab/E for global expand/collapse toggle.
    /// Cycles the global expand level and forces it on all repos.
    fn handle_expand_all(&mut self) {
        // Use `true` so the global cycle always includes Full.
        self.global_expand = self.global_expand.next_expand(true);
        let repos: Vec<String> = self
            .status
            .watches
            .iter()
            .map(|w| w.repo.clone())
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        for repo in &repos {
            self.set_expand_level(repo, self.global_expand);
        }
        self.save_prefs();
    }

    /// Handle action keys (add, delete, mute, open, history, rerun, config, help, etc.).
    #[allow(clippy::too_many_arguments)]
    fn handle_action_key(
        &mut self,
        code: KeyCode,
        selected: Option<(&str, &str, Option<u64>, bool)>,
        is_repo_row: bool,
        is_failed: bool,
        failing_job_id: Option<u64>,
        daemon: &DaemonClient,
    ) {
        match code {
            KeyCode::Char('a') => {
                self.input_mode = InputMode::TextInput {
                    prompt: "Add repo (owner/repo): ".to_string(),
                    editor: LineEditor::empty(),
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
                        editor: LineEditor::new(current.join(", ")),
                        action: TextAction::SetBranches { repo },
                    };
                }
            }
            KeyCode::Char('d') => {
                if let Some((repo, branch, _, _)) = selected {
                    let d = daemon.clone();
                    let repo = repo.to_string();
                    if is_repo_row || branch.is_empty() {
                        self.spawn_action(format!("Removing {repo}…"), true, async move {
                            d.unwatch(&repo).await.map(|()| format!("Removed {repo}"))
                        });
                    } else {
                        let branch = branch.to_string();
                        let remaining: Vec<String> = self
                            .status
                            .watches
                            .iter()
                            .filter(|w| w.repo == repo && w.branch != branch)
                            .map(|w| w.branch.clone())
                            .collect();
                        if remaining.is_empty() {
                            self.spawn_action(format!("Removing {repo}…"), true, async move {
                                d.unwatch(&repo).await.map(|()| format!("Removed {repo}"))
                            });
                        } else {
                            let label = format!("{repo} [{branch}]");
                            self.spawn_action(format!("Removing {label}…"), true, async move {
                                d.set_branches(&repo, &remaining)
                                    .await
                                    .map(|()| format!("Removed {label}"))
                            });
                        }
                    }
                }
            }
            KeyCode::Char('n') => {
                if let Some((repo, branch, _, muted)) = selected {
                    let d = daemon.clone();
                    let repo = repo.to_string();
                    let action = if muted { "unmute" } else { "mute" };
                    let verb = if muted { "Unmuted" } else { "Muted" };
                    if is_repo_row {
                        let label = repo.clone();
                        self.spawn_action(format!("{verb} {label}…"), true, async move {
                            d.set_repo_notifications(&repo, action)
                                .await
                                .map(|()| format!("{verb} {label}"))
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
                self.status.paused = new_pause;
                let msg = if new_pause { "Paused" } else { "Resumed" };
                self.spawn_action(msg.to_string(), false, async move {
                    d.pause(new_pause)
                        .await
                        .map(|()| if new_pause { "Paused" } else { "Resumed" }.to_string())
                });
            }
            KeyCode::Char('o') => {
                if is_failed {
                    if let Some((repo, _, Some(run_id), _)) = selected {
                        if let Some(job_id) = failing_job_id {
                            open_browser(&job_url(repo, run_id, job_id));
                        } else {
                            open_browser(&run_url(repo, run_id));
                        }
                    }
                } else if is_repo_row {
                    if let Some((repo, _, _, _)) = selected {
                        open_browser(&format!("{}/actions", repo_url(repo)));
                    }
                } else if let Some((repo, _, Some(run_id), _)) = selected {
                    open_browser(&run_url(repo, run_id));
                }
            }
            KeyCode::Char('O') => {
                if let Some((repo, _, _, _)) = selected {
                    open_browser(&format!("{}/actions", repo_url(repo)));
                }
            }
            KeyCode::Char('h') => {
                if let Some((repo, branch, _, _)) = selected {
                    self.open_history(daemon, repo, if is_repo_row { None } else { Some(branch) });
                }
            }
            KeyCode::Char('H') => {
                self.show_recent_panel = !self.show_recent_panel;
                self.save_prefs();
            }
            KeyCode::Char('r') | KeyCode::Char('R') => {
                if let Some((repo, _, run_id, _)) = selected {
                    let repo = repo.to_string();
                    let failed_only = code == KeyCode::Char('r');
                    let label = if failed_only {
                        "failed jobs"
                    } else {
                        "all jobs"
                    };
                    let d = daemon.clone();
                    self.spawn_action(
                        format!("Rerunning {label} for {repo}…"),
                        false,
                        async move { d.rerun(&repo, run_id, failed_only).await },
                    );
                }
            }
            KeyCode::Char('M') => {
                if let Some((repo, branch, _, _)) = selected {
                    // Find the first PR targeting this branch.
                    let pr = self
                        .status
                        .watches
                        .iter()
                        .find(|w| w.repo == repo && w.branch == branch)
                        .and_then(|w| w.prs.first());
                    if let Some(pr) = pr {
                        let repo = repo.to_string();
                        let number = pr.number;
                        let d = daemon.clone();
                        self.spawn_action(
                            format!("Merging PR #{number} in {repo}…"),
                            false,
                            async move { d.merge_pr(&repo, number).await },
                        );
                    } else {
                        self.set_flash("No PR found for this branch");
                    }
                }
            }
            KeyCode::Char('c') => {
                if let Some((repo, _, _, _)) = selected {
                    self.open_repo_config_form(daemon, repo);
                }
            }
            KeyCode::Char('C') => {
                self.open_config_form(daemon);
            }
            KeyCode::Char('?') => {
                self.show_help = !self.show_help;
                self.save_prefs();
            }
            _ => {}
        }
    }

    fn cycle_sort(&mut self, reverse: bool) {
        if reverse {
            if !self.sort_ascending {
                self.sort_ascending = true;
            } else {
                self.sort_column = self.sort_column.prev();
                self.sort_ascending = false;
            }
        } else if self.sort_ascending {
            self.sort_ascending = false;
        } else {
            self.sort_column = self.sort_column.next();
            self.sort_ascending = true;
        }
        self.save_prefs();
    }
}

/// Dispatch a key event to a `LineEditor` using readline-style shortcuts.
fn handle_line_edit(ed: &mut LineEditor, code: KeyCode, ctrl: bool, alt: bool) {
    match code {
        // Movement
        KeyCode::Char('a') if ctrl => ed.move_home(),
        KeyCode::Char('e') if ctrl => ed.move_end(),
        KeyCode::Char('b') if ctrl => ed.move_left(),
        KeyCode::Char('f') if ctrl => ed.move_right(),
        KeyCode::Char('b') if alt => ed.move_word_left(),
        KeyCode::Char('f') if alt => ed.move_word_right(),
        KeyCode::Left if alt => ed.move_word_left(),
        KeyCode::Left => ed.move_left(),
        KeyCode::Right if alt => ed.move_word_right(),
        KeyCode::Right => ed.move_right(),
        KeyCode::Home => ed.move_home(),
        KeyCode::End => ed.move_end(),
        // Deletion
        KeyCode::Char('d') if ctrl => ed.delete(),
        KeyCode::Char('h') if ctrl => ed.backspace(),
        KeyCode::Char('k') if ctrl => ed.kill_to_end(),
        KeyCode::Char('u') if ctrl => ed.kill_to_start(),
        KeyCode::Char('w') if ctrl => ed.delete_word_left(),
        KeyCode::Char('d') if alt => ed.delete_word_right(),
        KeyCode::Backspace if alt => ed.delete_word_left(),
        KeyCode::Backspace => ed.backspace(),
        // Insert
        KeyCode::Char(c) if !ctrl && !alt => ed.insert(c),
        _ => {}
    }
}

/// Returns true if any branch of `repo` has more than one workflow item
/// (i.e. expanding to Full would show workflow children).
fn repo_has_multi_workflow_branch(watches: &[WatchStatus], repo: &str) -> bool {
    watches.iter().filter(|w| w.repo == repo).any(|w| {
        let active_wfs: HashSet<&str> = w.active_runs.iter().map(|r| r.workflow.as_str()).collect();
        let extra = w
            .last_builds
            .iter()
            .filter(|b| !active_wfs.contains(b.workflow.as_str()))
            .count();
        active_wfs.len() + extra > 1
    })
}
