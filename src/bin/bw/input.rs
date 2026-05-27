use std::collections::HashSet;

use crossterm::event::{KeyCode, KeyModifiers};

use build_watcher::config::NOTIFICATION_EVENT_COUNT;
use build_watcher::github::{job_url, repo_url, run_url};
use build_watcher::status::WatchStatus;

use super::app::{App, ExpandLevel, FormKind, QuitAction, SseUpdate};
use super::client::{DaemonClient, open_browser};
use super::forms::{InputMode, LineEditor, PrPickerEntry, TextAction};
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
                    KeyCode::Tab if matches!(action, TextAction::AddRepo) => {
                        try_complete_org(editor, &self.status.watches);
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
                let is_cycle = !fields[*active].options.is_empty();
                let has_tabs = fields.iter().any(|f| f.is_tab);
                let current_tab = super::forms::current_tab(fields, *active);
                let tab_count = fields.iter().filter(|f| f.is_tab).count();
                // Cycle through non-tab fields, wrapping within the current tab when
                // the form has tabs (use PageUp/PageDown to move between tabs).
                let advance = |start: usize, dir: i32, fields: &[super::forms::FormField]| {
                    let n = fields.len();
                    let mut i = start;
                    for _ in 0..n {
                        i = if dir > 0 {
                            (i + 1) % n
                        } else {
                            (i + n - 1) % n
                        };
                        if fields[i].is_tab {
                            continue;
                        }
                        if !has_tabs || super::forms::current_tab(fields, i) == current_tab {
                            return i;
                        }
                    }
                    start
                };
                let switch_tab = |dir: i32, fields: &[super::forms::FormField]| -> Option<usize> {
                    if tab_count == 0 {
                        return None;
                    }
                    let target = if dir > 0 {
                        (current_tab + 1) % tab_count
                    } else {
                        (current_tab + tab_count - 1) % tab_count
                    };
                    super::forms::first_field_in_tab(fields, target)
                };
                match code {
                    KeyCode::Esc => {
                        self.input_mode = InputMode::Normal;
                    }
                    KeyCode::Down => {
                        *active = advance(*active, 1, fields);
                    }
                    KeyCode::Up => {
                        *active = advance(*active, -1, fields);
                    }
                    KeyCode::Tab | KeyCode::PageDown => {
                        if let Some(i) = switch_tab(1, fields) {
                            *active = i;
                        } else {
                            *active = advance(*active, 1, fields);
                        }
                    }
                    KeyCode::BackTab | KeyCode::PageUp => {
                        if let Some(i) = switch_tab(-1, fields) {
                            *active = i;
                        } else {
                            *active = advance(*active, -1, fields);
                        }
                    }
                    KeyCode::Right | KeyCode::Char(' ') if is_cycle && !ctrl && !alt => {
                        let f = &mut fields[*active];
                        let idx = f
                            .options
                            .iter()
                            .position(|&o| o == f.editor.buf)
                            .unwrap_or(0);
                        f.editor.buf = f.options[(idx + 1) % f.options.len()].to_string();
                    }
                    KeyCode::Left if is_cycle && !ctrl && !alt => {
                        let f = &mut fields[*active];
                        let n = f.options.len();
                        let idx = f
                            .options
                            .iter()
                            .position(|&o| o == f.editor.buf)
                            .unwrap_or(0);
                        f.editor.buf = f.options[(idx + n - 1) % n].to_string();
                    }
                    KeyCode::Enter => match kind {
                        FormKind::GlobalDefaults { .. } => self.submit_config_form(daemon),
                        FormKind::RepoConfig { .. } => self.submit_repo_config_form(daemon),
                        FormKind::AutoDiscoverRule { .. } => {
                            self.submit_auto_discover_rule_form(daemon)
                        }
                    },
                    _ if !is_cycle => {
                        handle_line_edit(&mut fields[*active].editor, code, ctrl, alt);
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
                    KeyCode::Down | KeyCode::Char('j') if !entries.is_empty() => {
                        *selected = (*selected + 1).min(entries.len() - 1);
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
            InputMode::PrPicker {
                repo,
                prs,
                selected,
            } => {
                match code {
                    KeyCode::Esc => {
                        self.input_mode = InputMode::Normal;
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        *selected = selected.saturating_sub(1);
                    }
                    KeyCode::Down | KeyCode::Char('j') if !prs.is_empty() => {
                        *selected = (*selected + 1).min(prs.len() - 1);
                    }
                    KeyCode::Enter => {
                        if let Some(pr) = prs.get(*selected) {
                            let number = pr.number;
                            let repo = repo.clone();
                            let d = daemon.clone();
                            self.input_mode = InputMode::Normal;
                            self.spawn_action(
                                format!("Merging PR #{number} in {repo}…"),
                                false,
                                async move { d.merge_pr(&repo, number).await },
                            );
                        }
                    }
                    _ => {}
                }
                true
            }
            InputMode::BuildTimes { rows, selected, .. } => {
                match code {
                    KeyCode::Esc | KeyCode::Char('q') => {
                        self.input_mode = InputMode::Normal;
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        *selected = selected.saturating_sub(1);
                    }
                    KeyCode::Down | KeyCode::Char('j') if !rows.is_empty() => {
                        *selected = (*selected + 1).min(rows.len() - 1);
                    }
                    _ => {}
                }
                true
            }
            InputMode::AutoDiscoverRules { rules, selected } => {
                match code {
                    KeyCode::Esc | KeyCode::Char('q') => {
                        self.input_mode = InputMode::Normal;
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        *selected = selected.saturating_sub(1);
                    }
                    KeyCode::Down | KeyCode::Char('j') if !rules.is_empty() => {
                        *selected = (*selected + 1).min(rules.len() - 1);
                    }
                    KeyCode::Char('a') | KeyCode::Char('+') => {
                        self.input_mode = InputMode::Form {
                            title: "New Auto-Discover Rule".to_string(),
                            kind: FormKind::AutoDiscoverRule { existing_id: None },
                            fields: vec![
                                super::forms::FormField::text("Repo filter", String::new()),
                                super::forms::FormField::cycle(
                                    "Updated filter",
                                    "any".to_string(),
                                    vec!["any", "week", "month", "year"],
                                ),
                            ],
                            active: 0,
                        };
                    }
                    KeyCode::Enter if !rules.is_empty() => {
                        let rule = &rules[*selected];
                        let id = rule.id.clone();
                        let initial_filter = rule
                            .repo_pattern
                            .clone()
                            .or_else(|| rule.org_pattern.clone())
                            .unwrap_or_default();
                        let recency = rule.recently_updated.clone();
                        self.input_mode = InputMode::Form {
                            title: format!("Edit Rule: {id}"),
                            kind: FormKind::AutoDiscoverRule {
                                existing_id: Some(id),
                            },
                            fields: vec![
                                super::forms::FormField::text("Repo filter", initial_filter),
                                super::forms::FormField::cycle(
                                    "Updated filter",
                                    recency,
                                    vec!["any", "week", "month", "year"],
                                ),
                            ],
                            active: 0,
                        };
                    }
                    KeyCode::Char('d') | KeyCode::Delete if !rules.is_empty() => {
                        let id = rules[*selected].id.clone();
                        let d = daemon.clone();
                        let tx = self.bg_tx.clone();
                        self.input_mode = InputMode::Normal;
                        self.set_flash(format!("Removing rule {id}…"));
                        tokio::spawn(async move {
                            match d.remove_auto_discover_rule(&id).await {
                                Ok(()) => match d.get_auto_discover_rules().await {
                                    Ok(rules) => {
                                        let _ = tx
                                            .send(SseUpdate::EnterAutoDiscoverRules { rules })
                                            .await;
                                    }
                                    Err(e) => {
                                        let _ = tx
                                            .send(SseUpdate::BackgroundResult {
                                                flash: format!("Rule removed. {e}"),
                                                resync: false,
                                            })
                                            .await;
                                    }
                                },
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
            // -- Help dismiss --
            KeyCode::Esc if self.show_help => {
                self.show_help = false;
                self.save_prefs();
            }
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
                    let current: Vec<String> = self
                        .status
                        .watches
                        .iter()
                        .filter(|w| w.repo == repo)
                        .map(|w| w.branch.clone())
                        .collect();
                    let d = daemon.clone();
                    let tx = self.bg_tx.clone();
                    self.set_flash("Checking config…");
                    tokio::spawn(async move {
                        let auto = is_auto_discover(&d, &repo).await;
                        if auto {
                            let _ = tx
                                .send(SseUpdate::BackgroundResult {
                                    flash: "Cannot edit branches: auto-discover is enabled for this repo".to_string(),
                                    resync: false,
                                })
                                .await;
                        } else {
                            let _ = tx
                                .send(SseUpdate::EnterTextInput {
                                    prompt: format!("Branches for {repo}: "),
                                    editor: LineEditor::new(current.join(", ")),
                                    action: TextAction::SetBranches { repo },
                                })
                                .await;
                        }
                    });
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
                            self.spawn_action(
                                format!("Removing {label}…"),
                                true,
                                async move {
                                    if is_auto_discover(&d, &repo).await {
                                        Err("Cannot delete branch: auto-discover is enabled for this repo".to_string())
                                    } else {
                                        d.set_branches(&repo, &remaining)
                                            .await
                                            .map(|()| format!("Removed {label}"))
                                    }
                                },
                            );
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
            KeyCode::Char('f') => {
                if let Some((repo, branch, _, _)) = selected {
                    // A repo-level pin cascades to every branch. `repo_pinned` is
                    // the same for all rows of the repo, so any watch carries it.
                    let repo_pinned = self
                        .status
                        .watches
                        .iter()
                        .any(|w| w.repo == repo && w.repo_pinned);
                    let branch_count = self
                        .status
                        .watches
                        .iter()
                        .filter(|w| w.repo == repo)
                        .count();
                    // A repo-header row means "whole repo" only when it carries
                    // no branch (a genuine multi-branch header). A single-branch
                    // header carries its branch name and must target that branch
                    // — including a lone pinned branch that has moved into the
                    // Pinned section, where its repo renders single-branch.
                    // Without this, unpinning such a branch would flip the repo
                    // flag (`RepoConfig.pinned`, already false) instead of the
                    // branch flag (`bc.pinned`), so the branch stayed pinned.
                    // The single-branch exception also lets the lone row of a
                    // repo-pinned repo lift that repo pin.
                    let is_whole_repo =
                        is_repo_row && (branch.is_empty() || (branch_count <= 1 && repo_pinned));

                    if repo_pinned && !is_whole_repo {
                        // The branch is pinned only because its repo is. It can't
                        // be unpinned on its own — the repo pin has to be lifted.
                        self.set_flash("Repo is pinned — unpin the repo to release its branches");
                    } else {
                        // Decide direction by reading the row's current effective
                        // pinned state from app.status. For a whole-repo row, "any
                        // branch pinned" counts as pinned; otherwise we look at the
                        // specific branch.
                        let currently_pinned = self.status.watches.iter().any(|w| {
                            w.repo == repo && (is_whole_repo || w.branch == branch) && w.pinned
                        });
                        let target = !currently_pinned;
                        let d = daemon.clone();
                        let repo_owned = repo.to_string();
                        let branch_owned = if is_whole_repo {
                            None
                        } else {
                            Some(branch.to_string())
                        };
                        let label = match &branch_owned {
                            Some(b) => format!("{repo_owned}/{b}"),
                            None => repo_owned.clone(),
                        };
                        let verb = if target { "Pinned" } else { "Unpinned" };
                        self.spawn_action(format!("{verb} {label}…"), true, async move {
                            d.pin(&repo_owned, branch_owned.as_deref(), target)
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
            KeyCode::Char('t') => {
                if let Some((repo, _, _, _)) = selected {
                    self.open_build_times(daemon, Some(repo));
                }
            }
            KeyCode::Char('T') => {
                self.open_build_times_from_recent();
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
                    let prs: Vec<_> = self
                        .status
                        .watches
                        .iter()
                        .find(|w| w.repo == repo && w.branch == branch)
                        .map(|w| &w.prs[..])
                        .unwrap_or_default()
                        .to_vec();
                    if prs.is_empty() {
                        self.set_flash(
                            "No open PRs targeting this branch  (enable Watch PRs via 'c')",
                        );
                    } else {
                        let entries = prs
                            .iter()
                            .map(|pr| PrPickerEntry {
                                number: pr.number,
                                title: pr.title.clone(),
                                author: pr.author.clone(),
                                merge_state: pr.merge_state.clone(),
                                draft: pr.draft,
                            })
                            .collect();
                        self.input_mode = InputMode::PrPicker {
                            repo: repo.to_string(),
                            prs: entries,
                            selected: 0,
                        };
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

/// Try to autocomplete an org name from watched repos.
fn try_complete_org(editor: &mut LineEditor, watches: &[WatchStatus]) {
    let orgs: Vec<&str> = watches
        .iter()
        .filter_map(|w| w.repo.split('/').next())
        .collect();
    if let Some(completed) = complete_org(&editor.buf, &orgs) {
        editor.buf = completed;
        editor.cursor = editor.buf.len();
    }
}

/// Pure autocomplete: given the current input and a list of known orgs,
/// return the completed string if exactly one org matches.
/// Only completes the org part (before any `/`).
fn complete_org(input: &str, orgs: &[&str]) -> Option<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() || trimmed.contains('/') {
        return None;
    }
    let input_lower = trimmed.to_lowercase();
    let mut matches: Vec<&str> = orgs
        .iter()
        .copied()
        .collect::<HashSet<_>>()
        .into_iter()
        .filter(|org| org.to_lowercase().starts_with(&input_lower))
        .collect();
    matches.sort_unstable();
    if matches.len() == 1 {
        Some(format!("{}/", matches[0]))
    } else {
        None
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

/// Check whether the repo's branch list is auto-managed — either because it
/// was discovered by a rule or because branch auto-discovery is on (per-repo
/// override or global default). When this is true, the TUI blocks manual
/// branch add/delete and the server rejects the same operations.
async fn is_auto_discover(d: &DaemonClient, repo: &str) -> bool {
    if let Ok(rc) = d.get_repo_config(repo).await {
        if rc.auto_discovered_by_rule == Some(true) {
            return true;
        }
        if let Some(val) = rc.auto_discover_branches {
            return val;
        }
    }
    d.get_defaults()
        .await
        .ok()
        .and_then(|defaults| defaults.auto_discover_branches)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn complete_org_single_match() {
        let orgs = vec!["anthropics", "wkirschbaum"];
        assert_eq!(complete_org("anth", &orgs), Some("anthropics/".to_string()));
        assert_eq!(complete_org("wk", &orgs), Some("wkirschbaum/".to_string()));
    }

    #[test]
    fn complete_org_case_insensitive() {
        let orgs = vec!["Anthropics", "wkirschbaum"];
        assert_eq!(complete_org("anth", &orgs), Some("Anthropics/".to_string()));
    }

    #[test]
    fn complete_org_ambiguous_returns_none() {
        let orgs = vec!["acme-a", "acme-b"];
        assert_eq!(complete_org("acme", &orgs), None);
    }

    #[test]
    fn complete_org_no_match() {
        let orgs = vec!["anthropics"];
        assert_eq!(complete_org("zz", &orgs), None);
    }

    #[test]
    fn complete_org_already_has_slash() {
        let orgs = vec!["anthropics"];
        assert_eq!(complete_org("anthropics/", &orgs), None);
    }

    #[test]
    fn complete_org_empty_input() {
        let orgs = vec!["anthropics"];
        assert_eq!(complete_org("", &orgs), None);
    }

    #[test]
    fn complete_org_deduplicates() {
        // Same org from multiple repos should still match as one.
        let orgs = vec!["anthropics", "anthropics", "wkirschbaum"];
        assert_eq!(complete_org("anth", &orgs), Some("anthropics/".to_string()));
    }

    #[test]
    fn complete_org_exact_match() {
        let orgs = vec!["anthropics"];
        assert_eq!(
            complete_org("anthropics", &orgs),
            Some("anthropics/".to_string())
        );
    }
}
