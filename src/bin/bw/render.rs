use std::time::Duration;

use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Table};

use build_watcher::config::NotificationLevel;
use build_watcher::format;
use build_watcher::status::{ActiveRunView, HistoryEntryView, LastBuildView, WatchStatus};

use super::app::{App, FormField, GroupBy, InputMode, SortColumn, SseState};

pub(crate) enum DisplayRow<'a> {
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
pub(crate) struct FlatRows<'a> {
    pub(crate) rows: Vec<DisplayRow<'a>>,
    /// Indices into `rows` that are selectable (everything except `FailingSteps`).
    pub(crate) selectable: Vec<usize>,
}

/// Group key as a sortable string (used to ensure items with the same group are contiguous).
pub(crate) fn group_key_for_sort(w: &WatchStatus, group_by: GroupBy) -> String {
    group_key(w, group_by).unwrap_or_default()
}

/// Extract the group key for a watch based on the grouping mode.
pub(crate) fn group_key(w: &WatchStatus, group_by: GroupBy) -> Option<String> {
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

pub(crate) fn flatten_rows(watches: &[WatchStatus], group_by: GroupBy) -> FlatRows<'_> {
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
    pub(crate) fn repo_branch_run(&self) -> (&str, &str, Option<u64>, bool) {
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
pub(crate) fn sorted_watches(
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
pub(crate) fn extra_runs_badge(runs: &[ActiveRunView], primary_idx: usize) -> String {
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
pub(crate) fn watch_status(w: &WatchStatus) -> (u8, &str) {
    if let Some(run) = w.active_runs.first() {
        (0, &run.status)
    } else if let Some(b) = &w.last_build {
        (1, &b.conclusion)
    } else {
        (2, "")
    }
}

pub(crate) fn watch_workflow(w: &WatchStatus) -> &str {
    if let Some(run) = w.active_runs.first() {
        &run.workflow
    } else if let Some(b) = &w.last_build {
        &b.workflow
    } else {
        ""
    }
}

/// Age/elapsed key: active run elapsed, completed build age, or MAX for idle.
pub(crate) fn watch_age(w: &WatchStatus) -> f64 {
    if let Some(run) = w.active_runs.first() {
        run.elapsed_secs.unwrap_or(f64::MAX)
    } else if let Some(b) = &w.last_build {
        b.age_secs.unwrap_or(f64::MAX)
    } else {
        f64::MAX
    }
}

/// Extract just the repo name (after the '/') for display.
pub(crate) fn short_repo(repo: &str) -> &str {
    repo.rsplit_once('/').map_or(repo, |(_, name)| name)
}

// -- Event application --

pub(crate) fn status_style(conclusion_or_status: &str) -> Style {
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

pub(crate) fn status_emoji(conclusion_or_status: &str) -> &'static str {
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
pub(crate) struct ColWidths {
    pub(crate) repo: usize,
    pub(crate) workflow: usize,
    pub(crate) title: usize,
}

impl ColWidths {
    pub(crate) fn from_terminal_width(w: u16) -> Self {
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

pub(crate) fn render_header(frame: &mut ratatui::Frame, area: ratatui::layout::Rect, app: &App) {
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

pub(crate) fn render_body(
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

pub(crate) fn render_recent_panel(
    frame: &mut ratatui::Frame,
    sep_area: ratatui::layout::Rect,
    body_area: ratatui::layout::Rect,
    app: &App,
    cw: &ColWidths,
) {
    let w = sep_area.width as usize;
    let dim = Style::default().fg(Color::DarkGray);
    let label = " Recent ";
    let dashes = w.saturating_sub(label.len());
    let left = dashes / 2;
    let right = dashes - left;
    let sep_line = Line::from(vec![
        Span::styled("─".repeat(left), dim),
        Span::styled(label, dim),
        Span::styled("─".repeat(right), dim),
    ]);
    frame.render_widget(Paragraph::new(sep_line), sep_area);

    let rows: Vec<Row> = app
        .recent_history
        .iter()
        .take(body_area.height as usize)
        .map(|entry| {
            let style = status_style(&entry.conclusion);
            let emoji = status_emoji(&entry.conclusion);
            let repo = format::truncate(&entry.repo, cw.repo);
            let branch = format::truncate(&entry.branch, BRANCH_W);
            let status_cell = format!("{emoji} {}", format::status(&entry.conclusion));
            let workflow = format::truncate(&entry.workflow, cw.workflow);
            let title = format::truncate(&entry.title, cw.title);
            let age = entry.age_secs.map(format::age).unwrap_or_default();
            Row::new(vec![
                Cell::from(repo),
                Cell::from(branch),
                Cell::from(status_cell).style(style),
                Cell::from(workflow),
                Cell::from(title),
                Cell::from(age),
            ])
            .style(Style::default().fg(Color::DarkGray))
        })
        .collect();

    let table = Table::new(rows, cw.constraints());
    frame.render_widget(table, body_area);
}

pub(crate) fn render_footer(frame: &mut ratatui::Frame, area: ratatui::layout::Rect, app: &App) {
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
        InputMode::Form { .. }
        | InputMode::NotificationPicker { .. }
        | InputMode::History { .. } => Paragraph::new(""),
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
            Span::styled("[h/H]", key_style),
            Span::raw(" history  "),
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

pub(crate) fn render(frame: &mut ratatui::Frame, app: &App) {
    let area = frame.area();
    let cw = ColWidths::from_terminal_width(area.width);

    // Count rows in the main table to give it an exact height.
    let sorted = sorted_watches(
        &app.status.watches,
        app.sort_column,
        app.sort_ascending,
        app.group_by,
    );
    let table_rows = flatten_rows(&sorted, app.group_by).rows.len() as u16;

    let recent_count = app.recent_history.len();
    let recent_height = recent_count.min(4) as u16;
    let show_recent = recent_height > 0;

    let chunks = if show_recent {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),             // header
                Constraint::Length(1),             // column headings
                Constraint::Length(table_rows),    // table body (exact)
                Constraint::Length(1),             // margin
                Constraint::Length(1),             // recent separator
                Constraint::Length(recent_height), // recent panel
                Constraint::Min(0),                // remaining space
                Constraint::Length(1),             // footer
            ])
            .split(area)
    } else {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),          // header
                Constraint::Length(1),          // column headings
                Constraint::Length(table_rows), // table body (exact)
                Constraint::Min(0),             // remaining space
                Constraint::Length(1),          // footer
            ])
            .split(area)
    };

    render_header(frame, chunks[0], app);
    render_body(frame, chunks[1], chunks[2], app, &cw);
    if show_recent {
        render_recent_panel(frame, chunks[4], chunks[5], app, &cw);
        render_footer(frame, chunks[7], app);
    } else {
        render_footer(frame, chunks[4], app);
    }

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

    // Overlay the history popup if active.
    if let InputMode::History {
        repo,
        branch,
        entries,
        selected,
    } = &app.input_mode
    {
        render_history_popup(frame, repo, branch.as_deref(), entries, *selected);
    }
}

/// Compute a centered rectangle of `percent_w` x height within `area`.
pub(crate) fn centered_rect(
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

pub(crate) fn render_form_popup(
    frame: &mut ratatui::Frame,
    title: &str,
    fields: &[FormField],
    active: usize,
) {
    // 3 lines per field (label + input + blank) + blank separator + hint + 2 for borders
    let inner_height = fields.len() as u16 * 3 + 2;
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
    constraints.push(Constraint::Length(1)); // blank separator before hint
    constraints.push(Constraint::Length(1)); // hint

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

        // Input line — cycle fields show ◀ value ▶, text fields show buffer with cursor
        let input_text = if !field.options.is_empty() {
            let arrow_style = if is_active {
                Style::default().fg(Color::Cyan)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            Line::from(vec![
                Span::styled("◀ ", arrow_style),
                Span::raw(&field.buffer),
                Span::styled(" ▶", arrow_style),
            ])
        } else if is_active {
            Line::from(vec![
                Span::raw(&field.buffer),
                Span::styled("█", cursor_style),
            ])
        } else {
            Line::from(Span::raw(&field.buffer))
        };
        frame.render_widget(Paragraph::new(input_text), rows[base + 1]);
    }

    // Footer hint — separated by a blank row from the last field
    let hint_row = fields.len() * 3 + 1;
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
    let header_row = if branch.is_none() {
        "  STATUS        BRANCH    WORKFLOW       TITLE                           DURATION  AGE"
    } else {
        "  STATUS        WORKFLOW       TITLE                                     DURATION  AGE"
    };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(header_row, header_style))),
        rows_layout[0],
    );

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
            let status_style = if is_selected {
                base_style
            } else {
                status_style(&entry.conclusion)
            };
            let arrow = if is_selected { "▸ " } else { "  " };
            let emoji = status_emoji(&entry.conclusion);
            let status_str = format::status(&entry.conclusion);
            let duration = entry
                .duration_secs
                .map(format::seconds)
                .unwrap_or_else(|| "—".to_string());
            let age = entry
                .age_secs
                .map(format::age)
                .unwrap_or_else(|| "—".to_string());
            let title_str = format::truncate(&entry.title, 32);
            let workflow_str = format::truncate(&entry.workflow, 14);

            let line = if branch.is_none() {
                let branch_str = format::truncate(&entry.branch, 9);
                Line::from(vec![
                    Span::styled(format!("{arrow}{emoji} {status_str:<11}"), status_style),
                    Span::styled(format!("{branch_str:<10}",), base_style),
                    Span::styled(format!("{workflow_str:<15}"), base_style),
                    Span::styled(format!("{title_str:<33}"), base_style),
                    Span::styled(format!("{duration:<10}"), base_style),
                    Span::styled(age, base_style),
                ])
            } else {
                Line::from(vec![
                    Span::styled(format!("{arrow}{emoji} {status_str:<11}"), status_style),
                    Span::styled(format!("{workflow_str:<15}"), base_style),
                    Span::styled(format!("{title_str:<37}"), base_style),
                    Span::styled(format!("{duration:<10}"), base_style),
                    Span::styled(age, base_style),
                ])
            };
            frame.render_widget(Paragraph::new(line), rows_layout[layout_idx]);
        }
    }

    // Hint row (last slot before end)
    let hint_idx = rows_layout.len() - 1;
    let hint = Line::from(vec![
        Span::styled(
            "[↑↓]",
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" scroll  ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            "[o]",
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" open  ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            "[Esc]",
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" close", Style::default().fg(Color::DarkGray)),
    ]);
    frame.render_widget(Paragraph::new(hint), rows_layout[hint_idx]);
}
