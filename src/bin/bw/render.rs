use std::collections::HashSet;
use std::time::Duration;

use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Table};

use build_watcher::config::NotificationLevel;
use build_watcher::format;
use build_watcher::status::{
    ActiveRunView, HistoryEntryView, LastBuildView, RunConclusion, RunStatus, WatchStatus,
};

use super::app::{App, FormField, GroupBy, InputMode, SortColumn, SseState};

/// Inline info shown on the repo header when there's exactly one watched branch.
pub(crate) struct SingleBranchInfo<'a> {
    pub branch: &'a str,
    pub workflows: String,
    pub title: String,
    /// The status string for styling (e.g. "in_progress", "success", "failure").
    pub status_key: String,
    /// GitHub Actions attempt number. Only shown when > 1.
    pub attempt: u32,
    /// Run ID of the most relevant run (active or last build).
    pub run_id: Option<u64>,
    /// Whether the last build was a failure (used for `o` key behavior).
    pub failed: bool,
    /// Database ID of the first failed job (for opening the job URL directly).
    pub failing_job_id: Option<u64>,
}

pub(crate) enum DisplayRow<'a> {
    GroupHeader {
        label: String,
    },
    RepoHeader {
        repo: &'a str,
        branch_count: usize,
        collapsed: bool,
        failing: usize,
        active: usize,
        passing: usize,
        idle: usize,
        muted: bool,
        newest_age: Option<f64>,
        /// When there's exactly 1 branch: its name, workflow(s), and title for inline display.
        single_branch: Option<SingleBranchInfo<'a>>,
        /// Workflow names of failing builds (for multi-branch repo headers).
        failing_workflows: Vec<String>,
    },
    ActiveRun {
        repo: &'a str,
        branch: &'a str,
        run: &'a ActiveRunView,
        /// Pre-computed badge for extra active runs, e.g. "+2⏸" or "+1⏳ +1⏸".
        /// Empty when this is the only active run.
        extra_badge: String,
        muted: bool,
        tree_prefix: &'static str,
    },
    FailingSteps {
        steps: &'a str,
        tree_indent: &'static str,
    },
    LastBuild {
        repo: &'a str,
        branch: &'a str,
        build: &'a LastBuildView,
        muted: bool,
        tree_prefix: &'static str,
    },
    NeverRan {
        repo: &'a str,
        branch: &'a str,
        muted: bool,
        tree_prefix: &'static str,
    },
}

/// Result of flattening watches into display rows.
pub(crate) struct FlatRows<'a> {
    pub(crate) rows: Vec<DisplayRow<'a>>,
    /// Indices into `rows` that are selectable (everything except `FailingSteps`).
    pub(crate) selectable: Vec<usize>,
}

/// Compute the group key for a set of watches sharing a repo.
/// Returns `None` for `GroupBy::None`.
///
/// `workflow_fn` and `status_fn` abstract over the watch slice element type
/// so this works with both `&[WatchStatus]` and `&[&WatchStatus]`.
fn group_key_impl(
    repo: &str,
    first_branch: &str,
    workflow: Option<&str>,
    worst_status: Option<(u8, &str)>,
    group_by: GroupBy,
) -> Option<String> {
    match group_by {
        GroupBy::Org => Some(repo.split('/').next().unwrap_or(repo).to_string()),
        GroupBy::Branch => Some(first_branch.to_string()),
        GroupBy::Workflow => Some(workflow.unwrap_or("(none)").to_string()),
        GroupBy::Status => {
            let worst = worst_status.unwrap_or((2, ""));
            Some(if worst.0 <= 1 {
                format::status(worst.1).to_string()
            } else {
                "idle".to_string()
            })
        }
        GroupBy::None => None,
    }
}

/// Group-by sort key for owned watch slices (used in `sorted_watches`).
fn repo_group_key(repo: &str, branches: &[WatchStatus], group_by: GroupBy) -> String {
    let first_branch = branches.first().map(|w| w.branch.as_str()).unwrap_or("");
    let workflow = branches.iter().map(watch_workflow).find(|w| !w.is_empty());
    let worst = branches.iter().map(watch_status).min();
    group_key_impl(repo, first_branch, workflow, worst, group_by).unwrap_or_default()
}

/// Group consecutive watches by repo, preserving input order.
fn group_watches_by_repo(watches: &[WatchStatus]) -> Vec<(&str, Vec<&WatchStatus>)> {
    let mut groups: Vec<(&str, Vec<&WatchStatus>)> = Vec::new();
    for w in watches {
        if let Some(g) = groups.iter_mut().find(|(r, _)| *r == w.repo.as_str()) {
            g.1.push(w);
        } else {
            groups.push((w.repo.as_str(), vec![w]));
        }
    }
    groups
}

pub(crate) fn flatten_rows<'a>(
    watches: &'a [WatchStatus],
    group_by: GroupBy,
    collapsed: &HashSet<String>,
) -> FlatRows<'a> {
    let mut rows = Vec::new();
    let mut selectable = Vec::new();
    let mut current_group: Option<String> = None;

    let repo_groups = if group_by.splits_repo() {
        // Each watch gets its own group entry so repos appear under each matching group.
        watches
            .iter()
            .map(|w| (w.repo.as_str(), vec![w]))
            .collect::<Vec<_>>()
    } else {
        group_watches_by_repo(watches)
    };

    for (repo, branches) in &repo_groups {
        // Group header (from group-by mode)
        let first_branch = branches.first().map(|w| w.branch.as_str()).unwrap_or("");
        let workflow = branches
            .iter()
            .map(|w| watch_workflow(w))
            .find(|w| !w.is_empty());
        let worst = branches.iter().map(|w| watch_status(w)).min();
        if let Some(key) = group_key_impl(repo, first_branch, workflow, worst, group_by)
            && current_group.as_deref() != Some(&key)
        {
            current_group = Some(key.clone());
            rows.push(DisplayRow::GroupHeader { label: key });
        }

        // Compute aggregate stats for repo header
        let mut failing = 0usize;
        let mut active = 0usize;
        let mut passing = 0usize;
        let mut idle = 0usize;
        let mut newest_age: Option<f64> = None;
        let mut all_muted = true;
        let mut failing_workflows: Vec<String> = Vec::new();

        for w in branches {
            if !w.active_runs.is_empty() {
                active += 1;
            } else if let Some(b) = &w.last_build {
                match b.conclusion {
                    RunConclusion::Success => passing += 1,
                    _ => {
                        failing += 1;
                        if !failing_workflows.contains(&b.workflow) {
                            failing_workflows.push(b.workflow.clone());
                        }
                    }
                }
                if let Some(age) = b.age_secs {
                    newest_age = Some(newest_age.map_or(age, |cur: f64| cur.min(age)));
                }
            } else {
                idle += 1;
            }
            if !w.muted {
                all_muted = false;
            }
            for run in &w.active_runs {
                if let Some(e) = run.elapsed_secs {
                    newest_age = Some(newest_age.map_or(e, |cur: f64| cur.min(e)));
                }
            }
        }

        let is_collapsed = collapsed.contains(*repo);

        // For single-branch repos, collect workflow/title info for inline display.
        let single_branch = if branches.len() == 1 {
            let w = branches[0];
            let (title, status_key, attempt, run_id, failed, failing_job_id) =
                if let Some(run) = w.active_runs.first() {
                    (
                        run.title.clone(),
                        run.status.as_str().to_string(),
                        run.attempt,
                        Some(run.run_id),
                        false,
                        None,
                    )
                } else if let Some(b) = &w.last_build {
                    (
                        b.title.clone(),
                        b.conclusion.as_str().to_string(),
                        b.attempt,
                        Some(b.run_id),
                        b.conclusion != RunConclusion::Success,
                        b.failing_job_id,
                    )
                } else {
                    (String::new(), String::new(), 1, None, false, None)
                };
            let mut wf_set: Vec<&str> = Vec::new();
            for run in &w.active_runs {
                if !wf_set.contains(&run.workflow.as_str()) {
                    wf_set.push(&run.workflow);
                }
            }
            if wf_set.is_empty()
                && let Some(b) = &w.last_build
            {
                wf_set.push(&b.workflow);
            }
            Some(SingleBranchInfo {
                branch: &w.branch,
                workflows: wf_set.join(", "),
                attempt,
                title,
                status_key,
                run_id,
                failed,
                failing_job_id,
            })
        } else {
            None
        };

        // Repo header row
        selectable.push(rows.len());
        rows.push(DisplayRow::RepoHeader {
            repo,
            branch_count: branches.len(),
            collapsed: is_collapsed,
            failing,
            active,
            passing,
            idle,
            muted: all_muted && !branches.is_empty(),
            newest_age,
            single_branch,
            failing_workflows,
        });

        // Failing steps for single-branch repos (shown directly under the header)
        if branches.len() == 1 {
            let w = branches[0];
            if w.active_runs.is_empty()
                && let Some(b) = &w.last_build
                && b.conclusion != RunConclusion::Success
                && let Some(steps) = &b.failing_steps
            {
                rows.push(DisplayRow::FailingSteps {
                    steps,
                    tree_indent: "",
                });
            }
        }

        // Branch rows (only for multi-branch repos when expanded)
        if !is_collapsed && branches.len() > 1 {
            let last_idx = branches.len() - 1;
            for (i, w) in branches.iter().enumerate() {
                let is_last = i == last_idx;
                let tree_prefix: &'static str = if is_last { "└─ " } else { "├─ " };
                let tree_indent: &'static str = if is_last { "   " } else { "│  " };

                if w.active_runs.is_empty() {
                    match &w.last_build {
                        Some(b) => {
                            selectable.push(rows.len());
                            rows.push(DisplayRow::LastBuild {
                                repo: &w.repo,
                                branch: &w.branch,
                                build: b,
                                muted: w.muted,
                                tree_prefix,
                            });
                            if b.conclusion != RunConclusion::Success
                                && let Some(steps) = &b.failing_steps
                            {
                                rows.push(DisplayRow::FailingSteps { steps, tree_indent });
                            }
                        }
                        None => {
                            selectable.push(rows.len());
                            rows.push(DisplayRow::NeverRan {
                                repo: &w.repo,
                                branch: &w.branch,
                                muted: w.muted,
                                tree_prefix,
                            });
                        }
                    }
                } else {
                    let primary_idx = w
                        .active_runs
                        .iter()
                        .rposition(|r| r.status == RunStatus::InProgress)
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
                        tree_prefix,
                    });
                }
            }
        }
    }

    FlatRows { rows, selectable }
}

impl DisplayRow<'_> {
    /// Returns `(repo, branch, run_id, muted)` for the selected row.
    /// For multi-branch `RepoHeader`, branch is empty. For single-branch, returns the branch name.
    pub(crate) fn repo_branch_run(&self) -> (&str, &str, Option<u64>, bool) {
        match self {
            DisplayRow::RepoHeader {
                repo,
                muted,
                single_branch: Some(sb),
                ..
            } => (repo, sb.branch, sb.run_id, *muted),
            DisplayRow::RepoHeader { repo, muted, .. } => (repo, "", None, *muted),
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
                ..
            } => (repo, branch, Some(build.run_id), *muted),
            DisplayRow::NeverRan {
                repo,
                branch,
                muted,
                ..
            } => (repo, branch, None, *muted),
            DisplayRow::GroupHeader { .. } | DisplayRow::FailingSteps { .. } => {
                unreachable!("not selectable")
            }
        }
    }

    /// Returns `true` if this is a `RepoHeader` row.
    pub(crate) fn is_repo_header(&self) -> bool {
        matches!(self, DisplayRow::RepoHeader { .. })
    }

    /// Returns `true` if the selected row represents a failed build.
    pub(crate) fn is_failed(&self) -> bool {
        match self {
            DisplayRow::RepoHeader {
                single_branch: Some(sb),
                ..
            } => sb.failed,
            DisplayRow::LastBuild { build, .. } => build.conclusion != RunConclusion::Success,
            _ => false,
        }
    }

    /// Returns the failing job ID if this row represents a failed build with a known job.
    pub(crate) fn failing_job_id(&self) -> Option<u64> {
        match self {
            DisplayRow::RepoHeader {
                single_branch: Some(sb),
                ..
            } => sb.failing_job_id,
            DisplayRow::LastBuild { build, .. } => build.failing_job_id,
            _ => None,
        }
    }

    /// Returns `true` if this is a single-branch repo header (not collapsible).
    pub(crate) fn is_single_branch(&self) -> bool {
        matches!(
            self,
            DisplayRow::RepoHeader {
                single_branch: Some(_),
                ..
            }
        )
    }
}

/// Sort watches as repo groups. Repos are sorted by aggregate column value;
/// branches within each repo are sorted by the same column.
/// When `group_by` is active, the group key is the primary sort key.
pub(crate) fn sorted_watches(
    watches: &[WatchStatus],
    column: SortColumn,
    ascending: bool,
    group_by: GroupBy,
) -> Vec<WatchStatus> {
    // Group by repo (or keep individual when splitting)
    let mut groups: Vec<(String, Vec<WatchStatus>)> = Vec::new();
    if group_by.splits_repo() {
        for w in watches {
            groups.push((w.repo.clone(), vec![w.clone()]));
        }
    } else {
        for w in watches {
            if let Some(g) = groups.iter_mut().find(|(r, _)| r == &w.repo) {
                g.1.push(w.clone());
            } else {
                groups.push((w.repo.clone(), vec![w.clone()]));
            }
        }
    }

    // Sort branches within each repo
    for (_, branches) in &mut groups {
        branches.sort_by(|a, b| {
            let cmp = match column {
                SortColumn::Repo | SortColumn::Branch => a.branch.cmp(&b.branch),
                SortColumn::Status => watch_status(a).cmp(&watch_status(b)),
                SortColumn::Workflow => watch_workflow(a).cmp(watch_workflow(b)),
                SortColumn::Age => watch_age(a)
                    .partial_cmp(&watch_age(b))
                    .unwrap_or(std::cmp::Ordering::Equal),
            };
            if ascending { cmp } else { cmp.reverse() }
        });
    }

    // Sort repo groups
    groups.sort_by(|a, b| {
        // Group-by key as primary sort
        let group_ord = match group_by {
            GroupBy::None => std::cmp::Ordering::Equal,
            _ => {
                let ka = repo_group_key(&a.0, &a.1, group_by);
                let kb = repo_group_key(&b.0, &b.1, group_by);
                ka.cmp(&kb)
            }
        };
        if group_ord != std::cmp::Ordering::Equal {
            return group_ord;
        }

        // Then by aggregate column value
        let cmp = match column {
            SortColumn::Repo => a.0.cmp(&b.0),
            SortColumn::Branch => {
                let ba = a.1.first().map(|w| w.branch.as_str()).unwrap_or("");
                let bb = b.1.first().map(|w| w.branch.as_str()).unwrap_or("");
                ba.cmp(bb).then(a.0.cmp(&b.0))
            }
            SortColumn::Status => {
                let sa = a.1.iter().map(watch_status).min();
                let sb = b.1.iter().map(watch_status).min();
                sa.cmp(&sb)
            }
            SortColumn::Workflow => {
                let wa = a.1.iter().map(watch_workflow).min();
                let wb = b.1.iter().map(watch_workflow).min();
                wa.cmp(&wb)
            }
            SortColumn::Age => {
                let aa = a.1.iter().map(watch_age).fold(f64::MAX, f64::min);
                let ab = b.1.iter().map(watch_age).fold(f64::MAX, f64::min);
                aa.partial_cmp(&ab).unwrap_or(std::cmp::Ordering::Equal)
            }
        };
        if ascending { cmp } else { cmp.reverse() }
    });

    // Flatten back to a flat vec (repos contiguous)
    groups
        .into_iter()
        .flat_map(|(_, branches)| branches)
        .collect()
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
pub(crate) fn watch_status(w: &WatchStatus) -> (u8, &'static str) {
    if let Some(run) = w.active_runs.first() {
        (0, run.status.as_str())
    } else if let Some(b) = &w.last_build {
        (1, b.conclusion.as_str())
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
        "success" => Style::default().fg(Color::Rgb(100, 180, 100)),
        "failure" | "cancelled" | "timed_out" | "startup_failure" => {
            Style::default().fg(Color::Rgb(220, 100, 100))
        }
        "in_progress" | "queued" | "waiting" | "requested" | "pending" => {
            Style::default().fg(Color::Yellow)
        }
        _ => Style::default(),
    }
}

pub(crate) fn status_emoji(conclusion_or_status: &str) -> &'static str {
    match conclusion_or_status {
        "success" => "✓",
        "failure" | "cancelled" | "timed_out" | "startup_failure" => "✗",
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
const STATUS_W: usize = 15;
const AGE_W: usize = 10;
const FIXED_W: usize = BRANCH_W + STATUS_W + AGE_W + NUM_GAPS * COL_SPACING as usize;

/// Variable column widths computed from terminal width.
pub(crate) struct ColWidths {
    pub(crate) repo: usize,
    pub(crate) workflow: usize,
    pub(crate) title: usize,
}

impl ColWidths {
    pub(crate) fn from_terminal_width(w: u16) -> Self {
        // Remaining space split among repo, workflow, title (20% / 25% / 55%).
        let remaining = (w as usize).saturating_sub(FIXED_W);
        let repo = (remaining * 20 / 100).max(10);
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
    let aggr = if s.poll_aggression.is_empty() {
        String::new()
    } else {
        format!(" [{}]", s.poll_aggression)
    };
    let poll = format!("poll {}s/{}s{aggr}", s.active_poll_secs, s.idle_poll_secs);
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
    let repo_count = {
        let mut repos: Vec<&str> = app.status.watches.iter().map(|w| w.repo.as_str()).collect();
        repos.sort_unstable();
        repos.dedup();
        repos.len()
    };
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
            Style::default().fg(Color::Rgb(220, 100, 100)),
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
    if let Some(version) = &app.update_available {
        left2_spans.push(Span::styled(
            format!("  ↑ {version} available [U]"),
            Style::default().fg(Color::Yellow),
        ));
    }
    let line2 = Line::from(left2_spans);

    // Line 3: separator
    let line3 = Line::from(Span::styled("─".repeat(w), dim));

    frame.render_widget(Paragraph::new(vec![line1, line2, line3]), area);
}

pub(crate) fn render_body<'a>(
    frame: &mut ratatui::Frame,
    heading_area: ratatui::layout::Rect,
    body_area: ratatui::layout::Rect,
    app: &App,
    flat: &FlatRows<'a>,
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

/// Build a 6-cell Row for a branch-level entry (ActiveRun, LastBuild, NeverRan).
#[allow(clippy::too_many_arguments)]
fn branch_row<'a>(
    branch: &str,
    muted: bool,
    tree_prefix: &str,
    status_text: &str,
    workflow: &str,
    title: &str,
    age_or_elapsed: &str,
    style: Style,
    cw: &ColWidths,
    mute_indicator: &dyn Fn(bool) -> &'static str,
) -> Row<'a> {
    let tree_name = format!("  {tree_prefix}{}{}", branch, mute_indicator(muted));
    Row::new(vec![
        Cell::from(format::truncate(&tree_name, cw.repo)),
        Cell::from(format::truncate(branch, BRANCH_W)),
        Cell::from(format::truncate(status_text, STATUS_W)).style(style),
        Cell::from(format::truncate(workflow, cw.workflow)),
        Cell::from(format::truncate(title, cw.title)),
        Cell::from(age_or_elapsed.to_string()).style(style),
    ])
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
        DisplayRow::RepoHeader {
            repo,
            branch_count,
            collapsed,
            failing,
            active,
            passing,
            idle,
            muted,
            newest_age,
            single_branch,
            failing_workflows,
        } => {
            let name = if single_branch.is_some() {
                format!("  {}{}", short_repo(repo), mute_indicator(*muted))
            } else {
                let arrow = if *collapsed { "›" } else { "⌄" };
                format!("{arrow} {}{}", short_repo(repo), mute_indicator(*muted))
            };

            // Compact status summary
            let mut parts = Vec::new();
            if *failing > 0 {
                parts.push(format!("✗ {failing}"));
            }
            if *active > 0 {
                parts.push(format!("⏳ {active}"));
            }
            if *passing > 0 {
                parts.push(format!("✓ {passing}"));
            }
            if *idle > 0 {
                parts.push(format!("· {idle}"));
            }
            let status_text = parts.join("  ");

            let age = newest_age
                .map(|s| format::age(s as u64))
                .unwrap_or_default();

            let repo_style = Style::default().add_modifier(Modifier::BOLD);

            // Single-branch repos: show branch name, workflow, and title inline
            // with the actual status (e.g. "✅ success") instead of aggregate counts.
            if let Some(sb) = single_branch {
                let emoji = status_emoji(&sb.status_key);
                let style = status_style(&sb.status_key);
                let attempt_suffix = if sb.attempt > 1 {
                    format!(" (attempt {})", sb.attempt)
                } else {
                    String::new()
                };
                let inline_status = if sb.status_key.is_empty() {
                    "· idle".to_string()
                } else {
                    format!("{emoji} {}{attempt_suffix}", format::status(&sb.status_key))
                };
                Row::new(vec![
                    Cell::from(format::truncate(&name, cw.repo)).style(repo_style),
                    Cell::from(format::truncate(sb.branch, BRANCH_W)),
                    Cell::from(format::truncate(&inline_status, STATUS_W)).style(style),
                    Cell::from(format::truncate(&sb.workflows, cw.workflow)),
                    Cell::from(format::truncate(&sb.title, cw.title)),
                    Cell::from(age).style(style),
                ])
            } else {
                let branch_label = if *collapsed {
                    format!("{branch_count} branches")
                } else {
                    String::new()
                };
                let wf_label = if !failing_workflows.is_empty() {
                    failing_workflows.join(", ")
                } else {
                    String::new()
                };
                Row::new(vec![
                    Cell::from(format::truncate(&name, cw.repo)).style(repo_style),
                    Cell::from(format::truncate(&branch_label, BRANCH_W)),
                    Cell::from(format::truncate(&status_text, STATUS_W)),
                    Cell::from(format::truncate(&wf_label, cw.workflow)),
                    Cell::from(""),
                    Cell::from(age),
                ])
            }
        }
        DisplayRow::ActiveRun {
            branch,
            run,
            extra_badge,
            muted,
            tree_prefix,
            ..
        } => {
            let status_str = run.status.as_str();
            let style = status_style(status_str);
            let emoji = status_emoji(status_str);
            let elapsed = run
                .elapsed_secs
                .map(|s| format::duration(Duration::from_secs_f64(s)))
                .unwrap_or_default();
            let attempt_suffix = if run.attempt > 1 {
                format!(" (attempt {})", run.attempt)
            } else {
                String::new()
            };
            let status_text = if extra_badge.is_empty() {
                format!("{emoji} {}{attempt_suffix}", format::status(status_str))
            } else {
                format!(
                    "{emoji} {}{attempt_suffix} {extra_badge}",
                    format::status(status_str)
                )
            };
            branch_row(
                branch,
                *muted,
                tree_prefix,
                &status_text,
                &run.workflow,
                &run.title,
                &elapsed,
                style,
                cw,
                mute_indicator,
            )
        }
        DisplayRow::FailingSteps { steps, tree_indent } => Row::new(vec![
            Cell::from(format!("  {tree_indent}")),
            Cell::from(""),
            Cell::from(""),
            Cell::from(""),
            Cell::from(format!("↳ {}", format::truncate(steps, cw.title)))
                .style(Style::default().fg(Color::Rgb(220, 100, 100))),
            Cell::from(""),
        ]),
        DisplayRow::LastBuild {
            branch,
            build,
            muted,
            tree_prefix,
            ..
        } => {
            let conclusion_str = build.conclusion.as_str();
            let style = status_style(conclusion_str);
            let emoji = status_emoji(conclusion_str);
            let age = build
                .age_secs
                .map(|s| format::age(s as u64))
                .unwrap_or_default();
            let attempt_suffix = if build.attempt > 1 {
                format!(" (attempt {})", build.attempt)
            } else {
                String::new()
            };
            let status_text = format!("{emoji} {}{attempt_suffix}", format::status(conclusion_str));
            branch_row(
                branch,
                *muted,
                tree_prefix,
                &status_text,
                &build.workflow,
                &build.title,
                &age,
                style,
                cw,
                mute_indicator,
            )
        }
        DisplayRow::NeverRan {
            branch,
            muted,
            tree_prefix,
            ..
        } => branch_row(
            branch,
            *muted,
            tree_prefix,
            "· idle",
            "",
            "",
            "",
            Style::default().fg(Color::DarkGray),
            cw,
            mute_indicator,
        ),
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
        InputMode::Normal => {
            let sep = Span::styled("  │  ", Style::default().fg(Color::DarkGray));
            let mut spans = vec![
                // ── Navigate ──────────────────────────────────────────────
                Span::styled("[↑↓/jk]", key_style),
                Span::raw(" nav  "),
                Span::styled("[e/E]", key_style),
                Span::raw(" expand"),
                sep.clone(),
                // ── Repos ─────────────────────────────────────────────────
                Span::styled("[a]", key_style),
                Span::raw(" add  "),
                Span::styled("[b]", key_style),
                Span::raw(" branch  "),
                Span::styled("[d]", key_style),
                Span::raw(" del  "),
                Span::styled("[o/O]", key_style),
                Span::raw(" open"),
                sep.clone(),
                // ── Notifications ─────────────────────────────────────────
                Span::styled("[n/N]", key_style),
                Span::raw(" mute  "),
                Span::styled("[p]", key_style),
                Span::raw(" pause  "),
                Span::styled("[h/H]", key_style),
                Span::raw(" hist"),
                sep.clone(),
                // ── View ──────────────────────────────────────────────────
                Span::styled("[s/S]", key_style),
                Span::raw(" sort  "),
                Span::styled("[g/G]", key_style),
                Span::raw(" group  "),
                Span::styled("[C]", key_style),
                Span::raw(" config"),
                sep.clone(),
                // ── Quit ──────────────────────────────────────────────────
                Span::styled("[q]", key_style),
                Span::raw(" quit  "),
                Span::styled("[Q]", key_style),
                Span::raw(" stop"),
            ];
            if app.update_available.is_some() {
                spans.extend([
                    Span::raw("  "),
                    Span::styled("[U]", key_style),
                    Span::raw(" update"),
                ]);
            }
            Paragraph::new(Line::from(spans))
        }
        .style(Style::default().fg(Color::DarkGray)),
    };
    frame.render_widget(footer, area);

    let version = Paragraph::new(Line::from(Span::styled(
        concat!("v", env!("CARGO_PKG_VERSION")),
        Style::default().fg(Color::DarkGray),
    )))
    .alignment(ratatui::layout::Alignment::Right);
    frame.render_widget(version, area);
}

pub(crate) fn render(frame: &mut ratatui::Frame, app: &App) {
    let area = frame.area();
    let cw = ColWidths::from_terminal_width(area.width);

    // Sort and flatten watches once for the entire render pass.
    let sorted = sorted_watches(
        &app.status.watches,
        app.sort_column,
        app.sort_ascending,
        app.group_by,
    );
    let flat = flatten_rows(&sorted, app.group_by, &app.collapsed);
    let table_rows = flat.rows.len() as u16;

    let recent_count = app.recent_history.len();
    let recent_height = recent_count.min(10) as u16;
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
    render_body(frame, chunks[1], chunks[2], app, &flat, &cw);
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
    frame.render_widget(
        Paragraph::new(popup_hint(&[
            ("[Tab]", "next  "),
            ("[Enter]", "save  "),
            ("[Esc]", "cancel"),
        ])),
        rows[hint_row],
    );
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
