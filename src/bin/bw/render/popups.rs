//! Overlay popup rendering: PR picker, help, generic form, notification level
//! picker, history, auto-discover rules, and build-times.
//!
//! Each `render_*_popup` builds its own `centered_rect`, renders a `Clear`, and
//! draws inside a single `Block`. The top-level `render()` in `mod.rs` dispatches
//! to these based on the active `InputMode`.

use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use build_watcher::config::NotificationLevel;
use build_watcher::format;
use build_watcher::status::{AutoDiscoverRuleView, HistoryEntryView};

use super::super::app::{FormField, PrPickerEntry};
use super::super::forms::{self, BuildTimeRow};
use super::{COLOR_FAILURE, COLOR_SUCCESS, status_emoji, status_style};

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

/// Build a styled hint bar from `(key_label, description)` pairs.
///
/// Renders as: `[Key] desc  [Key] desc  …` in dim/bold styling.
fn popup_hint(pairs: &[(&str, &str)]) -> Line<'static> {
    let key_style = Style::default()
        .fg(Color::DarkGray)
        .add_modifier(Modifier::BOLD);
    let desc_style = Style::default().fg(Color::DarkGray);

    let mut spans = Vec::with_capacity(pairs.len() * 2);
    for (key, desc) in pairs {
        spans.push(Span::styled(key.to_string(), key_style));
        spans.push(Span::styled(format!(" {desc}"), desc_style));
    }
    Line::from(spans)
}

pub(crate) fn render_pr_picker_popup(
    frame: &mut ratatui::Frame,
    repo: &str,
    prs: &[PrPickerEntry],
    selected: usize,
) {
    let dim = Style::default().fg(Color::DarkGray);
    let selected_style = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);

    // 1 row per PR + 1 top padding + 1 bottom padding + 2 borders + 1 hint
    let inner_height = prs.len() as u16 + 3;
    let popup_height = inner_height + 2;
    let popup = centered_rect(70, popup_height, frame.area());

    frame.render_widget(Clear, popup);

    let title = if prs.len() == 1 {
        format!(" Confirm Merge — {repo} ")
    } else {
        format!(" Select PR to Merge — {repo} ")
    };
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let mut constraints: Vec<Constraint> = Vec::with_capacity(prs.len() + 3);
    constraints.push(Constraint::Length(1)); // top padding
    for _ in prs {
        constraints.push(Constraint::Length(1));
    }
    constraints.push(Constraint::Length(1)); // bottom padding
    constraints.push(Constraint::Length(1)); // hint

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(inner);

    for (i, pr) in prs.iter().enumerate() {
        let is_selected = i == selected;
        let marker = if is_selected { "▸ " } else { "  " };
        let style = if is_selected { selected_style } else { dim };
        let draft = if pr.draft { " (draft)" } else { "" };

        let line = Line::from(vec![
            Span::styled(marker, style),
            Span::styled(format!("#{} ", pr.number), style),
            Span::styled(
                pr.merge_state.icon(),
                status_style_for_merge(&pr.merge_state),
            ),
            Span::styled(format!(" {}", pr.title), style),
            Span::styled(format!("  @{}{draft}", pr.author), dim),
        ]);
        frame.render_widget(Paragraph::new(line), rows[i + 1]);
    }

    let hint = if prs.len() == 1 {
        popup_hint(&[("[Enter]", "merge"), ("[Esc]", "cancel")])
    } else {
        popup_hint(&[
            ("[↑↓]", "select"),
            ("[Enter]", "merge"),
            ("[Esc]", "cancel"),
        ])
    };
    frame.render_widget(
        Paragraph::new(hint).alignment(ratatui::layout::Alignment::Center),
        rows[prs.len() + 2],
    );
}

fn status_style_for_merge(state: &build_watcher::github::MergeState) -> Style {
    use build_watcher::github::MergeState;
    match state {
        MergeState::Clean => Style::default().fg(COLOR_SUCCESS),
        MergeState::Blocked | MergeState::Dirty => Style::default().fg(COLOR_FAILURE),
        MergeState::Unstable | MergeState::Behind | MergeState::HasHooks => {
            Style::default().fg(Color::Yellow)
        }
        _ => Style::default().fg(Color::DarkGray),
    }
}

pub(crate) fn render_help_popup(frame: &mut ratatui::Frame) {
    let key_style = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    let desc_style = Style::default().fg(Color::White);
    let section_style = Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD);

    let lines = vec![
        Line::from(""),
        Line::from(Span::styled("  Navigation", section_style)),
        Line::from(vec![
            Span::styled("    ↑↓/jk     ", key_style),
            Span::styled("Navigate rows", desc_style),
        ]),
        Line::from(vec![
            Span::styled("    Tab/Enter ", key_style),
            Span::styled("Expand/collapse selected", desc_style),
        ]),
        Line::from(vec![
            Span::styled("    ⇧Tab/E    ", key_style),
            Span::styled("Cycle expand level (all repos)", desc_style),
        ]),
        Line::from(""),
        Line::from(Span::styled("  Repos", section_style)),
        Line::from(vec![
            Span::styled("    a         ", key_style),
            Span::styled("Add repo", desc_style),
        ]),
        Line::from(vec![
            Span::styled("    b         ", key_style),
            Span::styled("Configure branches", desc_style),
        ]),
        Line::from(vec![
            Span::styled("    d         ", key_style),
            Span::styled("Delete repo/branch", desc_style),
        ]),
        Line::from(vec![
            Span::styled("    o/O       ", key_style),
            Span::styled("Open run in browser / open repo", desc_style),
        ]),
        Line::from(vec![
            Span::styled("    r/R       ", key_style),
            Span::styled("Rerun failed jobs / rerun all jobs", desc_style),
        ]),
        Line::from(vec![
            Span::styled("    M         ", key_style),
            Span::styled("Merge PR", desc_style),
        ]),
        Line::from(vec![
            Span::styled("    f         ", key_style),
            Span::styled("Pin/unpin repo or branch (★ section at top)", desc_style),
        ]),
        Line::from(vec![
            Span::styled("              ", key_style),
            Span::styled("a repo pin covers all its branches", desc_style),
        ]),
        Line::from(""),
        Line::from(Span::styled("  Notifications", section_style)),
        Line::from(vec![
            Span::styled("    n         ", key_style),
            Span::styled("Mute/unmute toggle", desc_style),
        ]),
        Line::from(vec![
            Span::styled("    N         ", key_style),
            Span::styled("Per-event notification picker", desc_style),
        ]),
        Line::from(vec![
            Span::styled("    p         ", key_style),
            Span::styled("Pause/resume notifications", desc_style),
        ]),
        Line::from(""),
        Line::from(Span::styled("  History & Stats", section_style)),
        Line::from(vec![
            Span::styled("    h         ", key_style),
            Span::styled("Build history for selected", desc_style),
        ]),
        Line::from(vec![
            Span::styled("    H         ", key_style),
            Span::styled("Toggle recent builds panel", desc_style),
        ]),
        Line::from(vec![
            Span::styled("    t         ", key_style),
            Span::styled("Build times for selected repo", desc_style),
        ]),
        Line::from(vec![
            Span::styled("    T         ", key_style),
            Span::styled("Build times across all repos", desc_style),
        ]),
        Line::from(""),
        Line::from(Span::styled("  Config & View", section_style)),
        Line::from(vec![
            Span::styled("    c         ", key_style),
            Span::styled("Edit repo config", desc_style),
        ]),
        Line::from(vec![
            Span::styled("    C         ", key_style),
            Span::styled("Edit global config", desc_style),
        ]),
        Line::from(vec![
            Span::styled("    s/S       ", key_style),
            Span::styled("Cycle sort / reverse", desc_style),
        ]),
        Line::from(vec![
            Span::styled("    g/G       ", key_style),
            Span::styled("Cycle group / reverse", desc_style),
        ]),
        Line::from(""),
        Line::from(Span::styled("  General", section_style)),
        Line::from(vec![
            Span::styled("    ?         ", key_style),
            Span::styled("Toggle this help", desc_style),
        ]),
        Line::from(vec![
            Span::styled("    q         ", key_style),
            Span::styled("Quit", desc_style),
        ]),
        Line::from(vec![
            Span::styled("    Q         ", key_style),
            Span::styled("Stop daemon & quit", desc_style),
        ]),
        Line::from(vec![
            Span::styled("    U         ", key_style),
            Span::styled("Self-update (when available)", desc_style),
        ]),
        Line::from(""),
    ];

    let height = lines.len() as u16 + 2; // +2 for borders
    let popup = centered_rect(50, height, frame.area());

    frame.render_widget(Clear, popup);

    let block = Block::default()
        .title(format!(" Help — v{} ", env!("CARGO_PKG_VERSION")))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let text = Paragraph::new(lines);
    frame.render_widget(text, inner);

    // Hint at the bottom
    let hint = popup_hint(&[("[?/Esc]", "close")]);
    let hint_area = ratatui::layout::Rect::new(inner.x, popup.y + popup.height - 1, inner.width, 1);
    frame.render_widget(
        Paragraph::new(hint).alignment(ratatui::layout::Alignment::Center),
        hint_area,
    );
}

pub(crate) fn render_form_popup(
    frame: &mut ratatui::Frame,
    title: &str,
    fields: &[FormField],
    active: usize,
) {
    let tab_labels: Vec<&str> = fields
        .iter()
        .filter(|f| f.is_tab)
        .map(|f| f.label.as_str())
        .collect();
    let has_tabs = !tab_labels.is_empty();
    let active_tab = forms::current_tab(fields, active);

    // Collect indices of fields visible in the current view: when tabs are
    // present, only fields belonging to the active tab; otherwise all non-tab
    // fields. Fields appearing before the first tab marker are not shown when
    // tabs are active.
    let visible: Vec<usize> = if has_tabs {
        let mut out = Vec::new();
        let mut current: Option<usize> = None;
        for (i, f) in fields.iter().enumerate() {
            if f.is_tab {
                current = Some(current.map_or(0, |c| c + 1));
                continue;
            }
            if current == Some(active_tab) {
                out.push(i);
            }
        }
        out
    } else {
        fields
            .iter()
            .enumerate()
            .filter(|(_, f)| !f.is_tab)
            .map(|(i, _)| i)
            .collect()
    };

    // Layout: optional tabs row (1) + blank (1) + visible fields + blank (1) + hint (1).
    let tabs_rows: u16 = if has_tabs { 2 } else { 0 };
    let inner_height = visible.len() as u16 + 3 + tabs_rows;
    let popup_height = inner_height + 2;
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

    let label_width = visible
        .iter()
        .map(|&i| fields[i].label.len())
        .max()
        .unwrap_or(0);

    let mut constraints: Vec<Constraint> = Vec::with_capacity(visible.len() + 5);
    if has_tabs {
        constraints.push(Constraint::Length(1)); // tab bar
        constraints.push(Constraint::Length(1)); // separator/blank under tabs
    }
    constraints.push(Constraint::Length(1)); // top padding
    for _ in &visible {
        constraints.push(Constraint::Length(1)); // field row
    }
    constraints.push(Constraint::Length(1)); // bottom padding
    constraints.push(Constraint::Length(1)); // hint

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(inner);

    if has_tabs {
        let tab_active = Style::default()
            .fg(Color::Black)
            .bg(Color::Cyan)
            .add_modifier(Modifier::BOLD);
        let tab_inactive = Style::default().fg(Color::DarkGray);
        let mut spans: Vec<Span> = vec![Span::raw(" ")];
        for (i, label) in tab_labels.iter().enumerate() {
            let style = if i == active_tab {
                tab_active
            } else {
                tab_inactive
            };
            spans.push(Span::styled(format!(" {label} "), style));
            if i + 1 < tab_labels.len() {
                spans.push(Span::raw(" "));
            }
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), rows[0]);
    }

    // Fixed label column width: longest label + 2 chars padding
    let label_col = (label_width as u16) + 2;
    let field_row_offset = if has_tabs { 3 } else { 1 };

    for (vi, &fi) in visible.iter().enumerate() {
        let field = &fields[fi];
        let row = field_row_offset + vi;

        let is_active = fi == active;
        let style = if is_active {
            active_label_style
        } else {
            label_style
        };

        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(label_col), Constraint::Min(1)])
            .split(rows[row]);

        let label = Paragraph::new(Line::from(Span::styled(&field.label, style)))
            .alignment(ratatui::layout::Alignment::Right);
        frame.render_widget(label, cols[0]);

        let mut spans: Vec<Span> = vec![Span::raw("  ")];
        if !field.options.is_empty() {
            let arrow_style = if is_active {
                Style::default().fg(Color::Cyan)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            spans.push(Span::styled("◀ ", arrow_style));
            spans.push(Span::raw(field.buffer().to_string()));
            spans.push(Span::styled(" ▶", arrow_style));
        } else if is_active {
            let (before, cursor_ch, after) = field.editor.split_at_cursor();
            let cursor_str = cursor_ch.unwrap_or(' ').to_string();
            spans.push(Span::raw(before.to_string()));
            spans.push(Span::styled(
                cursor_str,
                Style::default().fg(Color::Black).bg(Color::Cyan),
            ));
            spans.push(Span::raw(after.to_string()));
        } else {
            spans.push(Span::raw(field.buffer().to_string()));
        }

        frame.render_widget(Paragraph::new(Line::from(spans)), cols[1]);
    }

    let hint_row = rows.len() - 1;
    let hint_pairs: &[(&str, &str)] = if has_tabs {
        &[
            ("[↑↓]", "field  "),
            ("[Tab]", "next tab  "),
            ("[Enter]", "save  "),
            ("[Esc]", "cancel"),
        ]
    } else {
        &[
            ("[Tab]", "next  "),
            ("[Enter]", "save  "),
            ("[Esc]", "cancel"),
        ]
    };
    frame.render_widget(Paragraph::new(popup_hint(hint_pairs)), rows[hint_row]);
}

pub(crate) fn render_notification_picker_popup(
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

    frame.render_widget(
        Paragraph::new(popup_hint(&[
            ("[←/→]", "cycle  "),
            ("[Enter]", "save  "),
            ("[Esc]", "cancel"),
        ])),
        rows[5],
    );
}

/// Column widths for the history popup, computed from available inner width.
struct HistoryColWidths {
    status: usize,
    branch: usize,
    workflow: usize,
    title: usize,
    duration: usize,
}

impl HistoryColWidths {
    fn new(w: usize, show_branch: bool) -> Self {
        let duration = 8;
        let age = 8;
        let status = 14; // "▸ ✗ failure  " with arrow
        let fixed = status + duration + age;
        let remaining = w.saturating_sub(fixed);
        if show_branch {
            // branch 15%, workflow 20%, title 65%
            let branch = (remaining * 15 / 100).max(6);
            let workflow = (remaining * 20 / 100).max(6);
            let title = remaining.saturating_sub(branch + workflow).max(6);
            Self {
                status,
                branch,
                workflow,
                title,
                duration,
            }
        } else {
            let workflow = (remaining * 25 / 100).max(6);
            let title = remaining.saturating_sub(workflow).max(6);
            Self {
                status,
                branch: 0,
                workflow,
                title,
                duration,
            }
        }
    }
}

pub(crate) fn render_history_popup(
    frame: &mut ratatui::Frame,
    repo: &str,
    branch: Option<&str>,
    entries: &[HistoryEntryView],
    selected: usize,
) {
    let area = frame.area();
    // 1 header row + data rows + 1 blank + 1 hint + 2 borders, capped to terminal height
    let data_rows = entries.len().max(1) as u16;
    let popup_height = (data_rows + 5).min(area.height.saturating_sub(4));
    let visible_rows = popup_height.saturating_sub(5) as usize; // rows available for data

    let popup = centered_rect(85, popup_height, area);
    frame.render_widget(Clear, popup);

    let title = match branch {
        Some(b) => format!(" History: {repo} @ {b} "),
        None => format!(" History: {repo} "),
    };
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let show_branch = branch.is_none();
    let hcw = HistoryColWidths::new(inner.width as usize, show_branch);

    // Layout: header row + data rows (fill remaining) + blank + hint
    let inner_height = inner.height as usize;
    let mut constraints = vec![Constraint::Length(1)]; // column header
    for _ in 0..inner_height.saturating_sub(3) {
        constraints.push(Constraint::Length(1));
    }
    constraints.push(Constraint::Length(1)); // blank
    constraints.push(Constraint::Length(1)); // hint

    let rows_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(inner);

    // Column header
    let header_style = Style::default()
        .fg(Color::DarkGray)
        .add_modifier(Modifier::BOLD);
    let header_line = if show_branch {
        Line::from(vec![
            Span::styled(format!("{:<w$}", "STATUS", w = hcw.status), header_style),
            Span::styled(format!("{:<w$}", "BRANCH", w = hcw.branch), header_style),
            Span::styled(
                format!("{:<w$}", "WORKFLOW", w = hcw.workflow),
                header_style,
            ),
            Span::styled(format!("{:<w$}", "TITLE", w = hcw.title), header_style),
            Span::styled(
                format!("{:<w$}", "DURATION", w = hcw.duration),
                header_style,
            ),
            Span::styled("AGE", header_style),
        ])
    } else {
        Line::from(vec![
            Span::styled(format!("{:<w$}", "STATUS", w = hcw.status), header_style),
            Span::styled(
                format!("{:<w$}", "WORKFLOW", w = hcw.workflow),
                header_style,
            ),
            Span::styled(format!("{:<w$}", "TITLE", w = hcw.title), header_style),
            Span::styled(
                format!("{:<w$}", "DURATION", w = hcw.duration),
                header_style,
            ),
            Span::styled("AGE", header_style),
        ])
    };
    frame.render_widget(Paragraph::new(header_line), rows_layout[0]);

    // Scroll offset: keep selected centered
    let offset = if visible_rows == 0 {
        0
    } else {
        selected.saturating_sub(visible_rows / 2)
    };

    if entries.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "  No history found.",
                Style::default().fg(Color::DarkGray),
            ))),
            rows_layout[1],
        );
    } else {
        for (slot, entry) in entries.iter().skip(offset).enumerate() {
            let layout_idx = slot + 1; // offset by header row
            if layout_idx >= rows_layout.len().saturating_sub(2) {
                break; // stop before blank + hint rows
            }
            let is_selected = offset + slot == selected;
            let base_style = if is_selected {
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Reset)
            };
            let sstyle = if is_selected {
                base_style
            } else {
                status_style(entry.conclusion.as_str())
            };
            let arrow = if is_selected { "▸ " } else { "  " };
            let emoji = status_emoji(entry.conclusion.as_str());
            let status_str = format::status(entry.conclusion.as_str());
            let duration = entry
                .duration_secs
                .map(format::seconds)
                .unwrap_or_else(|| "—".to_string());
            let age = entry
                .age_secs
                .map(format::age)
                .unwrap_or_else(|| "—".to_string());

            let mut spans = vec![Span::styled(
                format!("{arrow}{emoji} {status_str:<w$}", w = hcw.status - 4),
                sstyle,
            )];
            if show_branch {
                spans.push(Span::styled(
                    format!(
                        "{:<w$}",
                        format::truncate(&entry.branch, hcw.branch - 1),
                        w = hcw.branch
                    ),
                    base_style,
                ));
            }
            spans.extend([
                Span::styled(
                    format!(
                        "{:<w$}",
                        format::truncate(&entry.workflow, hcw.workflow - 1),
                        w = hcw.workflow
                    ),
                    base_style,
                ),
                Span::styled(
                    format!(
                        "{:<w$}",
                        format::truncate(&entry.title, hcw.title - 1),
                        w = hcw.title
                    ),
                    base_style,
                ),
                Span::styled(format!("{duration:<w$}", w = hcw.duration), base_style),
                Span::styled(age, base_style),
            ]);
            frame.render_widget(Paragraph::new(Line::from(spans)), rows_layout[layout_idx]);
        }
    }

    // Hint row (last slot before end)
    let hint_idx = rows_layout.len() - 1;
    frame.render_widget(
        Paragraph::new(popup_hint(&[
            ("[↑↓]", "scroll  "),
            ("[o]", "open  "),
            ("[r/R]", "rerun  "),
            ("[Esc]", "close"),
        ])),
        rows_layout[hint_idx],
    );
}

/// Render the build times popup (opened with `b`/`B`).
pub(crate) fn render_auto_discover_rules_popup(
    frame: &mut ratatui::Frame,
    rules: &[AutoDiscoverRuleView],
    selected: usize,
) {
    let area = frame.area();
    let data_rows = rules.len().max(1) as u16;
    let popup_height = (data_rows + 5).min(area.height.saturating_sub(4));
    let visible_rows = popup_height.saturating_sub(5) as usize;

    let popup = centered_rect(80, popup_height, area);
    frame.render_widget(Clear, popup);

    let block = Block::default()
        .title(" Auto-Discover Rules ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let inner_height = inner.height as usize;
    let mut constraints = vec![Constraint::Length(1)]; // header
    for _ in 0..inner_height.saturating_sub(3) {
        constraints.push(Constraint::Length(1));
    }
    constraints.push(Constraint::Length(1)); // blank
    constraints.push(Constraint::Length(1)); // hint
    let rows_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(inner);

    let w = inner.width as usize;
    // Fixed columns: org (20) + repo (20) + recency (8)
    let fixed = 20 + 20 + 8;
    let id_w = w.saturating_sub(fixed).max(8);

    let header_style = Style::default()
        .fg(Color::DarkGray)
        .add_modifier(Modifier::BOLD);
    let header = Line::from(vec![
        Span::styled(format!("{:<id_w$}", "ID"), header_style),
        Span::styled(format!("{:<20}", "ORG PATTERN"), header_style),
        Span::styled(format!("{:<20}", "REPO PATTERN"), header_style),
        Span::styled(format!("{:<8}", "RECENCY"), header_style),
    ]);
    frame.render_widget(Paragraph::new(header), rows_layout[0]);

    if rules.is_empty() {
        let dim = Style::default().fg(Color::DarkGray);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "  No rules — press [a] to add one",
                dim,
            ))),
            rows_layout[1],
        );
    } else {
        let scroll_offset = if selected >= visible_rows {
            selected - visible_rows + 1
        } else {
            0
        };
        let dim = Style::default().fg(Color::DarkGray);
        let selected_style = Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD);

        for (i, rule) in rules.iter().enumerate().skip(scroll_offset) {
            let layout_idx = 1 + i - scroll_offset;
            if layout_idx >= rows_layout.len() - 2 {
                break;
            }
            let is_sel = i == selected;
            let base_style = if is_sel { selected_style } else { dim };
            let prefix = if is_sel { "▸ " } else { "  " };

            let id = format::truncate(&rule.id, id_w.saturating_sub(2));
            let org = rule
                .org_pattern
                .as_deref()
                .map(|s| format::truncate(s, 18))
                .unwrap_or_else(|| "—".to_string());
            let repo = rule
                .repo_pattern
                .as_deref()
                .map(|s| format::truncate(s, 18))
                .unwrap_or_else(|| "—".to_string());

            let spans = vec![
                Span::styled(format!("{prefix}{id:<w$}", w = id_w - 2), base_style),
                Span::styled(format!("{org:<20}"), base_style),
                Span::styled(format!("{repo:<20}"), base_style),
                Span::styled(format!("{:<8}", rule.recently_updated), base_style),
            ];
            frame.render_widget(Paragraph::new(Line::from(spans)), rows_layout[layout_idx]);
        }
    }

    let hint_idx = rows_layout.len() - 1;
    frame.render_widget(
        Paragraph::new(popup_hint(&[
            ("[↑↓]", "scroll  "),
            ("[a]", "add  "),
            ("[Enter]", "edit  "),
            ("[d]", "delete  "),
            ("[Esc]", "close"),
        ])),
        rows_layout[hint_idx],
    );
}

pub(crate) fn render_build_times_popup(
    frame: &mut ratatui::Frame,
    title: &str,
    rows: &[BuildTimeRow],
    selected: usize,
) {
    let area = frame.area();
    let data_rows = rows.len().max(1) as u16;
    let popup_height = (data_rows + 5).min(area.height.saturating_sub(4));
    let visible_rows = popup_height.saturating_sub(5) as usize;

    let popup = centered_rect(80, popup_height, area);
    frame.render_widget(Clear, popup);

    let block = Block::default()
        .title(title.to_string())
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let inner_height = inner.height as usize;
    let mut constraints = vec![Constraint::Length(1)]; // header
    for _ in 0..inner_height.saturating_sub(3) {
        constraints.push(Constraint::Length(1));
    }
    constraints.push(Constraint::Length(1)); // blank
    constraints.push(Constraint::Length(1)); // hint
    let rows_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(inner);

    let w = inner.width as usize;
    // Column widths: NAME (flexible), AVG, MIN, MAX, RUNS, PASS%
    // Each numeric column gets 10 chars for breathing room.
    let fixed = 10 + 10 + 10 + 8 + 8; // avg + min + max + runs + pass%
    let name_w = w.saturating_sub(fixed).max(10);

    let header_style = Style::default()
        .fg(Color::DarkGray)
        .add_modifier(Modifier::BOLD);
    let header = Line::from(vec![
        Span::styled(format!("{:<name_w$}", "NAME"), header_style),
        Span::styled(format!("{:>10}", "AVG"), header_style),
        Span::styled(format!("{:>10}", "MIN"), header_style),
        Span::styled(format!("{:>10}", "MAX"), header_style),
        Span::styled(format!("{:>8}", "RUNS"), header_style),
        Span::styled(format!("{:>8}", "PASS%"), header_style),
    ]);
    frame.render_widget(Paragraph::new(header), rows_layout[0]);

    if rows.is_empty() {
        let dim = Style::default().fg(Color::DarkGray);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled("  No build data", dim))),
            rows_layout[1],
        );
    } else {
        let scroll_offset = if selected >= visible_rows {
            selected - visible_rows + 1
        } else {
            0
        };
        let dim = Style::default().fg(Color::DarkGray);
        let selected_style = Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD);

        for (i, row) in rows.iter().enumerate().skip(scroll_offset) {
            let layout_idx = 1 + i - scroll_offset;
            if layout_idx >= rows_layout.len() - 2 {
                break;
            }
            let is_sel = i == selected;
            let base_style = if is_sel { selected_style } else { dim };
            let prefix = if is_sel { "▸ " } else { "  " };
            let label = format::truncate(&row.label, name_w.saturating_sub(2));

            // Color the pass rate: green ≥80%, yellow ≥50%, red <50%
            let pass_color = if row.pass_rate >= 80 {
                Color::Green
            } else if row.pass_rate >= 50 {
                Color::Yellow
            } else {
                Color::Red
            };
            let pass_style = if is_sel {
                Style::default().fg(pass_color).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(pass_color)
            };

            let spans = vec![
                Span::styled(format!("{prefix}{label:<w$}", w = name_w - 2), base_style),
                Span::styled(format!("{:>10}", format::seconds(row.avg_secs)), base_style),
                Span::styled(format!("{:>10}", format::seconds(row.min_secs)), base_style),
                Span::styled(format!("{:>10}", format::seconds(row.max_secs)), base_style),
                Span::styled(format!("{:>8}", row.count), base_style),
                Span::styled(format!("{:>7}%", row.pass_rate), pass_style),
            ];
            frame.render_widget(Paragraph::new(Line::from(spans)), rows_layout[layout_idx]);
        }
    }

    let hint_idx = rows_layout.len() - 1;
    frame.render_widget(
        Paragraph::new(popup_hint(&[("[↑↓]", "scroll  "), ("[Esc]", "close")])),
        rows_layout[hint_idx],
    );
}
