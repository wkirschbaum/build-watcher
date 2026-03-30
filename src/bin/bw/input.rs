use crossterm::event::{KeyCode, KeyModifiers};

use build_watcher::config::NOTIFICATION_EVENT_COUNT;
use build_watcher::github::{job_url, repo_url, run_url};

use super::app::{App, ExpandLevel, QuitAction, SseUpdate};
use super::client::{DaemonClient, open_browser};
use super::forms::{InputMode, TextAction};
use super::render::flatten_rows;

impl App {
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
        let selected = selected_display_idx.map(|idx| flat.rows[idx].repo_branch_run());
        let is_repo_row = selected_display_idx
            .map(|idx| flat.rows[idx].is_repo_header())
            .unwrap_or(false);
        let is_collapsible = is_repo_row
            && !selected_display_idx
                .map(|idx| flat.rows[idx].is_single_branch())
                .unwrap_or(false);
        let is_branch_header = selected_display_idx
            .map(|idx| flat.rows[idx].is_branch_header())
            .unwrap_or(false);
        let is_failed = selected_display_idx
            .map(|idx| flat.rows[idx].is_failed())
            .unwrap_or(false);
        let failing_job_id = selected_display_idx.and_then(|idx| flat.rows[idx].failing_job_id());

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
            KeyCode::Enter | KeyCode::Right | KeyCode::Tab | KeyCode::Char('e')
                if is_collapsible =>
            {
                // Cycle expand level: Full → Branches → Collapsed → Full
                if let Some((repo, _, _, _)) = selected {
                    let repo = repo.to_string();
                    let current = self.expand_level(&repo);
                    let next = current.next();
                    if next == ExpandLevel::Full {
                        self.expand.remove(&repo);
                    } else {
                        self.expand.insert(repo, next);
                    }
                    self.save_prefs();
                }
            }
            KeyCode::Enter | KeyCode::Right | KeyCode::Tab | KeyCode::Char('e')
                if is_branch_header =>
            {
                // Toggle workflow expansion for this branch.
                if let Some((repo, branch, _, _)) = selected {
                    let repo = repo.to_string();
                    let key = format!("{repo}#{branch}");

                    if self.expand_level(&repo) != ExpandLevel::Full {
                        // Repo-level gate is blocking workflows. Promote to Full
                        // and ensure this branch's workflows will be visible.
                        self.expand.remove(&repo); // Full is the default
                        self.workflow_collapsed.remove(&key);
                    } else {
                        // Repo is at Full — toggle the per-branch workflow state.
                        if !self.workflow_collapsed.remove(&key) {
                            self.workflow_collapsed.insert(key);
                        }
                    }
                    self.save_prefs();
                }
            }
            KeyCode::Left if is_branch_header => {
                // Collapse from branch header up to repo level.
                if let Some((repo, _, _, _)) = selected {
                    let repo = repo.to_string();
                    let current = self.expand_level(&repo);
                    let prev = current.prev();
                    if prev == ExpandLevel::Full {
                        self.expand.remove(&repo);
                    } else {
                        self.expand.insert(repo.clone(), prev);
                    }
                    if let Some(pos) = flat.selectable.iter().position(|&idx| {
                        flat.rows[idx].is_repo_header()
                            && flat.rows[idx].repo_branch_run().0 == repo
                    }) {
                        self.selected = pos;
                    }
                    self.save_prefs();
                }
            }
            KeyCode::Left if !is_repo_row => {
                // On a workflow child row: collapse workflows to branch header.
                // On a plain branch row: collapse to repo level.
                if let Some((repo, branch, _, _)) = selected {
                    let repo = repo.to_string();
                    let branch = branch.to_string();
                    let key = format!("{repo}#{branch}");
                    // If this branch has a BranchHeader (i.e. workflows are expanded),
                    // collapse workflows first and move to the branch header.
                    if !self.workflow_collapsed.contains(&key)
                        && flat.selectable.iter().any(|&idx| {
                            flat.rows[idx].is_branch_header()
                                && flat.rows[idx].repo_branch_run().0 == repo
                                && flat.rows[idx].repo_branch_run().1 == branch
                        })
                    {
                        self.workflow_collapsed.insert(key);
                        // Move selection to the branch header
                        if let Some(pos) = flat.selectable.iter().position(|&idx| {
                            flat.rows[idx].is_branch_header()
                                && flat.rows[idx].repo_branch_run().0 == repo
                                && flat.rows[idx].repo_branch_run().1 == branch
                        }) {
                            self.selected = pos;
                        }
                    } else {
                        // No expandable branch header — collapse repo level.
                        let current = self.expand_level(&repo);
                        let prev = current.prev();
                        if prev == ExpandLevel::Full {
                            self.expand.remove(&repo);
                        } else {
                            self.expand.insert(repo.clone(), prev);
                        }
                        if let Some(pos) = flat.selectable.iter().position(|&idx| {
                            flat.rows[idx].is_repo_header()
                                && flat.rows[idx].repo_branch_run().0 == repo
                        }) {
                            self.selected = pos;
                        }
                    }
                    self.save_prefs();
                }
            }
            KeyCode::BackTab | KeyCode::Char('E') => {
                // Cycle all repos: if any are not fully expanded → expand all.
                // If all are fully expanded → collapse to branches.
                // If all at branches → collapse fully. If all collapsed → expand all.
                let expandable_repos: Vec<String> = {
                    let mut counts: std::collections::HashMap<&str, usize> =
                        std::collections::HashMap::new();
                    for w in &self.status.watches {
                        *counts.entry(w.repo.as_str()).or_insert(0) += 1;
                    }
                    counts
                        .into_iter()
                        .filter(|(_, n)| *n > 1)
                        .map(|(repo, _)| repo.to_string())
                        .collect()
                };
                if expandable_repos.is_empty() {
                    // nothing to toggle
                } else {
                    let all_full = expandable_repos
                        .iter()
                        .all(|r| self.expand_level(r) == ExpandLevel::Full);
                    let all_branches = expandable_repos
                        .iter()
                        .all(|r| self.expand_level(r) == ExpandLevel::Branches);
                    let all_collapsed = expandable_repos
                        .iter()
                        .all(|r| self.expand_level(r) == ExpandLevel::Collapsed);

                    if all_full {
                        for r in &expandable_repos {
                            self.expand.insert(r.clone(), ExpandLevel::Branches);
                        }
                    } else if all_branches {
                        for r in &expandable_repos {
                            self.expand.insert(r.clone(), ExpandLevel::Collapsed);
                        }
                    } else if all_collapsed {
                        for r in &expandable_repos {
                            self.expand.remove(r);
                        }
                    } else {
                        // Mixed state → expand all fully
                        for r in &expandable_repos {
                            self.expand.remove(r);
                        }
                    }
                }
                self.save_prefs();
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
                if let Some((repo, branch, _, _)) = selected {
                    let d = daemon.clone();
                    let repo = repo.to_string();
                    if is_repo_row || branch.is_empty() {
                        // On repo row: remove the entire repo
                        self.spawn_action(format!("Removing {repo}…"), true, async move {
                            d.unwatch(&repo).await.map(|()| format!("Removed {repo}"))
                        });
                    } else {
                        // On branch row: remove just this branch
                        let branch = branch.to_string();
                        let remaining: Vec<String> = self
                            .status
                            .watches
                            .iter()
                            .filter(|w| w.repo == repo && w.branch != branch)
                            .map(|w| w.branch.clone())
                            .collect();
                        if remaining.is_empty() {
                            // Last branch — remove the whole repo
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
                if is_failed {
                    // Open the specific failed job/run to see details
                    if let Some((repo, _, Some(run_id), _)) = selected {
                        if let Some(job_id) = failing_job_id {
                            open_browser(&job_url(repo, run_id, job_id));
                        } else {
                            open_browser(&run_url(repo, run_id));
                        }
                    }
                } else if is_repo_row {
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
                    open_browser(&format!("{}/actions", repo_url(repo)));
                }
            }
            KeyCode::Char('h') | KeyCode::Char('H') => {
                if let Some((repo, branch, _, _)) = selected {
                    // h on branch row = branch-scoped; h on repo row or H = all branches
                    let all_branches = code == KeyCode::Char('H') || is_repo_row;
                    self.open_history(daemon, repo, if all_branches { None } else { Some(branch) });
                }
            }
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
            KeyCode::Char('C') => {
                self.open_config_form(daemon);
            }
            KeyCode::Char('?') => {
                self.show_help = !self.show_help;
                self.save_prefs();
            }
            _ => {}
        }
        QuitAction::None
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
