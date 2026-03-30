use std::collections::{HashMap, HashSet};
use std::time::Duration;

use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Table};

use build_watcher::config::NotificationLevel;
use build_watcher::format;
use build_watcher::status::{
    ActiveRunView, HistoryEntryView, LastBuildView, RunConclusion, WatchStatus,
};

use super::app::{App, ExpandLevel, FormField, GroupBy, InputMode, SortColumn, SseState};

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
        expand_level: ExpandLevel,
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
    /// Branch header for multi-workflow branches. Shows aggregate status and
    /// can be toggled to expand/collapse individual workflow rows.
    BranchHeader {
        repo: &'a str,
        branch: &'a str,
        muted: bool,
        tree_prefix: &'static str,
        /// Number of workflow items underneath.
        workflow_count: usize,
        /// Whether the workflow children are currently visible.
        expanded: bool,
        /// Aggregate: worst status text (e.g. "✗ failure") for display.
        status_text: String,
        /// Aggregate: most recent age/elapsed string.
        age_or_elapsed: String,
        /// Style matching the worst status.
        style: Style,
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
    expand: &HashMap<String, ExpandLevel>,
    workflow_collapsed: &HashSet<String>,
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
            } else if !w.last_builds.is_empty() {
                let has_failure = w
                    .last_builds
                    .iter()
                    .any(|b| b.conclusion != RunConclusion::Success);
                if has_failure {
                    failing += 1;
                    for b in &w.last_builds {
                        if b.conclusion != RunConclusion::Success
                            && !failing_workflows.contains(&b.workflow)
                        {
                            failing_workflows.push(b.workflow.clone());
                        }
                    }
                } else {
                    passing += 1;
                }
                for b in &w.last_builds {
                    if let Some(age) = b.age_secs {
                        newest_age = Some(newest_age.map_or(age, |cur: f64| cur.min(age)));
                    }
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

        let expand_level = expand.get(*repo).copied().unwrap_or(ExpandLevel::Full);
        let is_collapsed = expand_level == ExpandLevel::Collapsed;

        // For single-branch repos with a single workflow, collect info for inline display.
        // Multi-workflow branches expand into child rows instead.
        let single_branch = if branches.len() == 1 {
            let w = branches[0];
            let workflow_count = {
                let mut wfs: Vec<&str> = Vec::new();
                for run in &w.active_runs {
                    if !wfs.contains(&run.workflow.as_str()) {
                        wfs.push(&run.workflow);
                    }
                }
                for b in &w.last_builds {
                    if !wfs.contains(&b.workflow.as_str()) {
                        wfs.push(&b.workflow);
                    }
                }
                wfs.len()
            };
            if workflow_count <= 1 {
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
                    } else if let Some(b) = newest_last_build(w) {
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
                    && let Some(b) = newest_last_build(w)
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
                None // multi-workflow: will expand into child rows
            }
        } else {
            None
        };

        let is_single_branch_inline = single_branch.is_some();

        // Repo header row
        selectable.push(rows.len());
        rows.push(DisplayRow::RepoHeader {
            repo,
            branch_count: branches.len(),
            expand_level,
            failing,
            active,
            passing,
            idle,
            muted: all_muted && !branches.is_empty(),
            newest_age,
            single_branch,
            failing_workflows,
        });

        // Failing steps for single-branch, single-workflow repos (shown directly under the header)
        if is_single_branch_inline && branches.len() == 1 {
            let w = branches[0];
            if w.active_runs.is_empty() {
                for b in &w.last_builds {
                    if b.conclusion != RunConclusion::Success
                        && let Some(steps) = &b.failing_steps
                    {
                        rows.push(DisplayRow::FailingSteps {
                            steps,
                            tree_indent: "",
                        });
                    }
                }
            }
        }

        // Expand child rows when not collapsed.
        // For multi-branch repos: show branch rows (each may further expand into workflow rows).
        // For single-branch, multi-workflow repos: show workflow rows directly under the header.
        let repo_allows_workflows = expand_level == ExpandLevel::Full;
        if !is_collapsed {
            let expand_branches = branches.len() > 1;
            let expand_single_branch_workflows = branches.len() == 1 && !is_single_branch_inline;

            if expand_branches {
                let last_idx = branches.len() - 1;
                for (i, w) in branches.iter().enumerate() {
                    let is_last = i == last_idx;
                    let tree_prefix: &'static str = if is_last { "└─ " } else { "├─ " };
                    let tree_indent: &'static str = if is_last { "   " } else { "│  " };

                    let branch_key = format!("{}#{}", repo, w.branch);
                    let show_wf =
                        repo_allows_workflows && !workflow_collapsed.contains(&branch_key);
                    emit_branch_workflow_rows(
                        w,
                        tree_prefix,
                        tree_indent,
                        show_wf,
                        &mut rows,
                        &mut selectable,
                    );
                }
            } else if expand_single_branch_workflows {
                let w = branches[0];
                let branch_key = format!("{}#{}", repo, w.branch);
                let show_wf = repo_allows_workflows && !workflow_collapsed.contains(&branch_key);
                emit_branch_workflow_rows(w, "", "", show_wf, &mut rows, &mut selectable);
            }
        }
    }

    FlatRows { rows, selectable }
}

/// Emit rows for a single branch's workflows. Each active run gets its own row,
/// and each last_build (not covered by an active run) gets its own row.
/// When there are multiple workflow items, a `BranchHeader` is emitted first,
/// followed by per-workflow children when `show_workflows` is true.
fn emit_branch_workflow_rows<'a>(
    w: &'a WatchStatus,
    tree_prefix: &'static str,
    tree_indent: &'static str,
    show_workflows: bool,
    rows: &mut Vec<DisplayRow<'a>>,
    selectable: &mut Vec<usize>,
) {
    // Collect all workflow items: active runs first, then last_builds not covered by active runs.
    let active_wfs: Vec<&str> = w.active_runs.iter().map(|r| r.workflow.as_str()).collect();

    let items: Vec<WorkflowItem<'a>> = {
        let mut v = Vec::new();
        for run in &w.active_runs {
            v.push(WorkflowItem::Active(run));
        }
        for b in &w.last_builds {
            if !active_wfs.contains(&b.workflow.as_str()) {
                v.push(WorkflowItem::Completed(b));
            }
        }
        v
    };

    let has_multiple_items = items.len() > 1;

    if !has_multiple_items {
        // Single workflow — show a single row (no branch header needed).
        if let Some(run) = w.active_runs.first() {
            selectable.push(rows.len());
            rows.push(DisplayRow::ActiveRun {
                repo: &w.repo,
                branch: &w.branch,
                run,
                extra_badge: String::new(),
                muted: w.muted,
                tree_prefix,
            });
        } else if let Some(b) = w.last_builds.first() {
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
        } else {
            selectable.push(rows.len());
            rows.push(DisplayRow::NeverRan {
                repo: &w.repo,
                branch: &w.branch,
                muted: w.muted,
                tree_prefix,
            });
        }
        return;
    }

    // Multiple workflows: emit a BranchHeader, then conditionally workflow children.
    let (status_text, age_or_elapsed, style) = branch_aggregate_status(w);
    selectable.push(rows.len());
    rows.push(DisplayRow::BranchHeader {
        repo: &w.repo,
        branch: &w.branch,
        muted: w.muted,
        tree_prefix,
        workflow_count: items.len(),
        expanded: show_workflows,
        status_text,
        age_or_elapsed,
        style,
    });

    if !show_workflows {
        return;
    }

    // Emit one child row per workflow item, indented under the branch header.
    let last_idx = items.len().saturating_sub(1);
    for (i, item) in items.iter().enumerate() {
        let is_last = i == last_idx;
        // Sub-tree prefixes: combine branch indent with workflow connector.
        let wf_prefix: &'static str = match (tree_indent, is_last) {
            ("", true) => "└─ ",
            ("", false) => "├─ ",
            ("│  ", true) => "│  └─ ",
            ("│  ", false) => "│  ├─ ",
            ("   ", true) => "   └─ ",
            ("   ", false) => "   ├─ ",
            (_, true) => "└─ ",
            (_, false) => "├─ ",
        };
        let wf_indent: &'static str = match (tree_indent, is_last) {
            ("", true) => "   ",
            ("", false) => "│  ",
            ("│  ", true) => "│     ",
            ("│  ", false) => "│  │  ",
            ("   ", true) => "      ",
            ("   ", false) => "   │  ",
            (_, true) => "   ",
            (_, false) => "│  ",
        };

        match item {
            WorkflowItem::Active(run) => {
                selectable.push(rows.len());
                rows.push(DisplayRow::ActiveRun {
                    repo: &w.repo,
                    branch: &w.branch,
                    run,
                    extra_badge: String::new(),
                    muted: w.muted,
                    tree_prefix: wf_prefix,
                });
            }
            WorkflowItem::Completed(b) => {
                selectable.push(rows.len());
                rows.push(DisplayRow::LastBuild {
                    repo: &w.repo,
                    branch: &w.branch,
                    build: b,
                    muted: w.muted,
                    tree_prefix: wf_prefix,
                });
                if b.conclusion != RunConclusion::Success
                    && let Some(steps) = &b.failing_steps
                {
                    rows.push(DisplayRow::FailingSteps {
                        steps,
                        tree_indent: wf_indent,
                    });
                }
            }
        }
    }
}

/// Compute aggregate status for a branch header from its active runs and last builds.
/// Returns `(status_text, age_or_elapsed, style)`.
fn branch_aggregate_status(w: &WatchStatus) -> (String, String, Style) {
    use std::time::Duration;
    if let Some(run) = w.active_runs.first() {
        let status_str = run.status.as_str();
        let emoji = status_emoji(status_str);
        let elapsed = run
            .elapsed_secs
            .map(|s| format::duration(Duration::from_secs_f64(s)))
            .unwrap_or_default();
        let extra = if w.active_runs.len() > 1 {
            format!(" +{}", w.active_runs.len() - 1)
        } else {
            String::new()
        };
        (
            format!("{emoji} {}{extra}", format::status(status_str)),
            elapsed,
            status_style(status_str),
        )
    } else if let Some(b) = worst_last_build(w) {
        let conclusion_str = b.conclusion.as_str();
        let emoji = status_emoji(conclusion_str);
        let age = b
            .age_secs
            .map(|s| format::age(s as u64))
            .unwrap_or_default();
        (
            format!("{emoji} {}", format::status(conclusion_str)),
            age,
            status_style(conclusion_str),
        )
    } else {
        (
            "· idle".to_string(),
            String::new(),
            Style::default().fg(Color::DarkGray),
        )
    }
}

/// Find the "worst" last build (failure > other > success) for branch aggregate display.
fn worst_last_build(w: &WatchStatus) -> Option<&LastBuildView> {
    w.last_builds.iter().min_by_key(|b| match b.conclusion {
        RunConclusion::Failure | RunConclusion::TimedOut | RunConclusion::StartupFailure => 0u8,
        RunConclusion::Cancelled => 1,
        RunConclusion::Success => 3,
        _ => 2,
    })
}

enum WorkflowItem<'a> {
    Active(&'a ActiveRunView),
    Completed(&'a LastBuildView),
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
            DisplayRow::BranchHeader {
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

    /// Returns `true` if this is a `BranchHeader` row (multi-workflow branch).
    pub(crate) fn is_branch_header(&self) -> bool {
        matches!(self, DisplayRow::BranchHeader { .. })
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

/// Status key: active runs (tier 0), completed (tier 1), idle (tier 2).
pub(crate) fn watch_status(w: &WatchStatus) -> (u8, &'static str) {
    if let Some(run) = w.active_runs.first() {
        (0, run.status.as_str())
    } else if let Some(b) = newest_last_build(w) {
        (1, b.conclusion.as_str())
    } else {
        (2, "")
    }
}

pub(crate) fn watch_workflow(w: &WatchStatus) -> &str {
    if let Some(run) = w.active_runs.first() {
        &run.workflow
    } else if let Some(b) = newest_last_build(w) {
        &b.workflow
    } else {
        ""
    }
}

/// Age/elapsed key: active run elapsed, completed build age, or MAX for idle.
pub(crate) fn watch_age(w: &WatchStatus) -> f64 {
    if let Some(run) = w.active_runs.first() {
        run.elapsed_secs.unwrap_or(f64::MAX)
    } else if let Some(b) = newest_last_build(w) {
        b.age_secs.unwrap_or(f64::MAX)
    } else {
        f64::MAX
    }
}

/// The most recently completed build (by run_id) across all workflows.
fn newest_last_build(w: &WatchStatus) -> Option<&LastBuildView> {
    w.last_builds.iter().max_by_key(|b| b.run_id)
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

const AGE_W: usize = 10;
const FIXED_W: usize = AGE_W + NUM_GAPS * COL_SPACING as usize;

/// Column widths computed from terminal width.
pub(crate) struct ColWidths {
    pub(crate) repo: usize,
    pub(crate) branch: usize,
    pub(crate) status: usize,
    pub(crate) workflow: usize,
    pub(crate) title: usize,
}

impl ColWidths {
    pub(crate) fn from_terminal_width(w: u16) -> Self {
        // All non-age columns share the remaining space proportionally:
        // repo 18%, branch 12%, status 10%, workflow 20%, title 40%.
        let remaining = (w as usize).saturating_sub(FIXED_W);
        let repo = (remaining * 18 / 100).max(10);
        let branch = (remaining * 12 / 100).max(10);
        let status = (remaining * 10 / 100).max(8);
        let workflow = (remaining * 20 / 100).max(8);
        let title = remaining
            .saturating_sub(repo + branch + status + workflow)
            .max(8);

        Self {
            repo,
            branch,
            status,
            workflow,
            title,
        }
    }

    fn constraints(&self) -> [Constraint; 6] {
        [
            Constraint::Length(self.repo as u16),
            Constraint::Length(self.branch as u16),
            Constraint::Length(self.status as u16),
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
            format!("  ↑ {version} available"),
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
        Cell::from(format::truncate(branch, cw.branch)),
        Cell::from(format::truncate(status_text, cw.status)).style(style),
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
            expand_level,
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
                let arrow = match expand_level {
                    ExpandLevel::Collapsed => "›",
                    ExpandLevel::Branches => "⌄",
                    ExpandLevel::Full => "⌄",
                };
                format!("{arrow} {}{}", short_repo(repo), mute_indicator(*muted))
            };

            // Compact status summary: active / passing / failing (/ idle when present)
            let status_text = if *idle > 0 {
                format!("{active}/{passing}/{failing}/{idle}")
            } else {
                format!("{active}/{passing}/{failing}")
            };

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
                    format!(" (r:{})", sb.attempt)
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
                    Cell::from(format::truncate(sb.branch, cw.branch)),
                    Cell::from(format::truncate(&inline_status, cw.status)).style(style),
                    Cell::from(format::truncate(&sb.workflows, cw.workflow)),
                    Cell::from(format::truncate(&sb.title, cw.title)),
                    Cell::from(age).style(style),
                ])
            } else {
                let branch_label = if *expand_level == ExpandLevel::Collapsed {
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
                    Cell::from(format::truncate(&branch_label, cw.branch)),
                    Cell::from(format::truncate(&status_text, cw.status)),
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
                format!(" (r:{})", run.attempt)
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
                format!(" (r:{})", build.attempt)
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
        DisplayRow::BranchHeader {
            branch,
            muted,
            tree_prefix,
            workflow_count,
            expanded,
            status_text,
            age_or_elapsed,
            style,
            ..
        } => {
            let expand_indicator = if *expanded { "▾" } else { "▸" };
            let wf_label = format!("{expand_indicator} {workflow_count} workflows");
            branch_row(
                branch,
                *muted,
                tree_prefix,
                status_text,
                &wf_label,
                "",
                age_or_elapsed,
                *style,
                cw,
                mute_indicator,
            )
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
            let branch = format::truncate(&entry.branch, cw.branch);
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

/// Separator span used between detail bar fields.
fn detail_sep() -> Span<'static> {
    Span::styled("  ·  ", Style::default().fg(Color::DarkGray))
}

/// Render a detail bar with a border showing contextual info for the currently selected row.
fn render_detail_bar(
    frame: &mut ratatui::Frame,
    area: ratatui::layout::Rect,
    app: &App,
    flat: &FlatRows,
) {
    let dim = Style::default().fg(Color::DarkGray);
    let label_style = Style::default()
        .fg(Color::DarkGray)
        .add_modifier(Modifier::BOLD);

    let row_idx = flat
        .selectable
        .get(app.selected)
        .and_then(|&i| flat.rows.get(i));

    let spans: Vec<Span> = match row_idx {
        Some(DisplayRow::RepoHeader {
            repo,
            branch_count,
            failing,
            active,
            passing,
            idle,
            single_branch,
            ..
        }) => {
            let mut s = vec![Span::styled(*repo, dim)];
            if let Some(sb) = single_branch {
                s.push(detail_sep());
                s.push(Span::styled(sb.branch, dim));
                if !sb.status_key.is_empty() {
                    s.push(detail_sep());
                    s.push(Span::styled(
                        format::status(&sb.status_key),
                        status_style(&sb.status_key),
                    ));
                }
                if let Some(run_id) = sb.run_id {
                    s.push(detail_sep());
                    s.push(Span::styled("run ", label_style));
                    s.push(Span::styled(run_id.to_string(), dim));
                }
                if sb.attempt > 1 {
                    s.push(detail_sep());
                    s.push(Span::styled(format!("attempt {}", sb.attempt), dim));
                }
            } else {
                s.push(detail_sep());
                s.push(Span::styled(format!("{} branches", branch_count), dim));
                if *failing > 0 {
                    s.push(detail_sep());
                    s.push(Span::styled(
                        format!("{} failing", failing),
                        Style::default().fg(Color::Rgb(220, 100, 100)),
                    ));
                }
                if *active > 0 {
                    s.push(detail_sep());
                    s.push(Span::styled(
                        format!("{} active", active),
                        Style::default().fg(Color::Yellow),
                    ));
                }
                if *passing > 0 {
                    s.push(detail_sep());
                    s.push(Span::styled(
                        format!("{} passing", passing),
                        Style::default().fg(Color::Rgb(100, 180, 100)),
                    ));
                }
                if *idle > 0 {
                    s.push(detail_sep());
                    s.push(Span::styled(format!("{} idle", idle), dim));
                }
            }
            s
        }
        Some(DisplayRow::ActiveRun {
            repo, branch, run, ..
        }) => {
            let mut s = vec![
                Span::styled(
                    format::status(run.status.as_str()),
                    status_style(run.status.as_str()),
                ),
                detail_sep(),
                Span::styled(format!("{} / {} / {}", repo, branch, run.workflow), dim),
                detail_sep(),
                Span::styled("run ", label_style),
                Span::styled(run.run_id.to_string(), dim),
            ];
            if !run.event.is_empty() {
                s.push(detail_sep());
                s.push(Span::styled(&run.event, dim));
            }
            if run.attempt > 1 {
                s.push(detail_sep());
                s.push(Span::styled(format!("attempt {}", run.attempt), dim));
            }
            if let Some(elapsed) = run.elapsed_secs {
                s.push(detail_sep());
                s.push(Span::styled(format::age(elapsed as u64), dim));
            }
            s
        }
        Some(DisplayRow::LastBuild {
            repo,
            branch,
            build,
            ..
        }) => {
            let mut s = vec![
                Span::styled(
                    format::status(build.conclusion.as_str()),
                    status_style(build.conclusion.as_str()),
                ),
                detail_sep(),
                Span::styled(format!("{} / {} / {}", repo, branch, build.workflow), dim),
                detail_sep(),
                Span::styled("run ", label_style),
                Span::styled(build.run_id.to_string(), dim),
            ];
            if build.attempt > 1 {
                s.push(detail_sep());
                s.push(Span::styled(format!("attempt {}", build.attempt), dim));
            }
            if let Some(steps) = &build.failing_steps {
                s.push(detail_sep());
                s.push(Span::styled("failed: ", label_style));
                s.push(Span::styled(
                    steps.as_str(),
                    Style::default().fg(Color::Rgb(220, 100, 100)),
                ));
            }
            if let Some(age) = build.age_secs {
                s.push(detail_sep());
                s.push(Span::styled(
                    format!("{} ago", format::age(age as u64)),
                    dim,
                ));
            }
            s
        }
        Some(DisplayRow::NeverRan { repo, branch, .. }) => {
            vec![
                Span::styled(format!("{} / {}", repo, branch), dim),
                detail_sep(),
                Span::styled("no builds yet", dim),
            ]
        }
        Some(DisplayRow::GroupHeader { label }) => {
            vec![
                Span::styled("group ", label_style),
                Span::styled(label.as_str(), dim),
            ]
        }
        Some(DisplayRow::BranchHeader {
            repo,
            branch,
            workflow_count,
            expanded,
            ..
        }) => {
            let state = if *expanded { "expanded" } else { "collapsed" };
            vec![
                Span::styled(format!("{repo} / {branch}"), dim),
                detail_sep(),
                Span::styled(format!("{workflow_count} workflows ({state})"), dim),
            ]
        }
        Some(DisplayRow::FailingSteps { .. }) | None => vec![],
    };

    let block = Block::default()
        .borders(Borders::TOP | Borders::BOTTOM)
        .border_style(Style::default().fg(Color::DarkGray));

    if !spans.is_empty() {
        let bar = Paragraph::new(Line::from(spans)).block(block);
        frame.render_widget(bar, area);
    } else {
        frame.render_widget(block, area);
    }
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
            let spans = vec![
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
                Span::raw(" stop  "),
                Span::styled("[?]", key_style),
                Span::raw(" hide"),
            ];
            Paragraph::new(Line::from(spans))
        }
        .style(Style::default().fg(Color::DarkGray)),
    };

    let use_border = app.show_help && matches!(app.input_mode, InputMode::Normal);
    if use_border {
        let border_style = Style::default().fg(Color::DarkGray);
        let block = Block::default()
            .borders(Borders::TOP)
            .border_style(border_style);
        let inner = block.inner(area);
        let footer = footer.block(block);
        frame.render_widget(footer, area);

        let version = Paragraph::new(Line::from(Span::styled(
            concat!("v", env!("CARGO_PKG_VERSION")),
            Style::default().fg(Color::DarkGray),
        )))
        .alignment(ratatui::layout::Alignment::Right);
        frame.render_widget(version, inner);
    } else {
        frame.render_widget(footer, area);
    }
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
    let flat = flatten_rows(&sorted, app.group_by, &app.expand, &app.workflow_collapsed);
    let table_rows = flat.rows.len() as u16;

    let recent_count = app.recent_history.len();
    let recent_height = recent_count.min(10) as u16;
    let show_recent = recent_height > 0;

    let needs_input_line = matches!(app.input_mode, InputMode::TextInput { .. });
    let footer_height = if app.show_help {
        3 // top border + content + bottom border
    } else if needs_input_line {
        1 // just the text input prompt
    } else {
        0
    };

    let chunks = if show_recent {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),             // header
                Constraint::Length(1),             // column headings
                Constraint::Length(table_rows),    // table body (exact)
                Constraint::Length(3),             // detail bar (border + content + border)
                Constraint::Min(0),                // remaining space (pushes recent down)
                Constraint::Length(1),             // recent separator
                Constraint::Length(recent_height), // recent panel
                Constraint::Length(footer_height), // footer
            ])
            .split(area)
    } else {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),             // header
                Constraint::Length(1),             // column headings
                Constraint::Length(table_rows),    // table body (exact)
                Constraint::Length(3),             // detail bar (border + content + border)
                Constraint::Min(0),                // remaining space
                Constraint::Length(footer_height), // footer
            ])
            .split(area)
    };

    render_header(frame, chunks[0], app);
    render_body(frame, chunks[1], chunks[2], app, &flat, &cw);
    render_detail_bar(frame, chunks[3], app, &flat);
    if show_recent {
        render_recent_panel(frame, chunks[5], chunks[6], app, &cw);
        if footer_height > 0 {
            render_footer(frame, chunks[7], app);
        }
    } else if footer_height > 0 {
        render_footer(frame, chunks[5], app);
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
    // 1 row per field + 1 blank top + 1 blank bottom + 1 hint + 2 borders
    let inner_height = fields.len() as u16 + 3;
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
    let cursor_style = Style::default().fg(Color::Cyan);

    // Find the longest label for alignment
    let label_width = fields.iter().map(|f| f.label.len()).max().unwrap_or(0);

    let mut constraints: Vec<Constraint> = Vec::with_capacity(fields.len() + 3);
    constraints.push(Constraint::Length(1)); // top padding
    for _ in fields {
        constraints.push(Constraint::Length(1)); // field row
    }
    constraints.push(Constraint::Length(1)); // bottom padding
    constraints.push(Constraint::Length(1)); // hint

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(inner);

    // Fixed label column width: longest label + 2 chars padding
    let label_col = (label_width as u16) + 2;

    for (i, field) in fields.iter().enumerate() {
        let row = i + 1; // offset by top padding
        let is_active = i == active;
        let style = if is_active {
            active_label_style
        } else {
            label_style
        };

        // Split row into label column (fixed) and value column (fill)
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(label_col), Constraint::Min(1)])
            .split(rows[row]);

        // Right-aligned label
        let label = Paragraph::new(Line::from(Span::styled(&field.label, style)))
            .alignment(ratatui::layout::Alignment::Right);
        frame.render_widget(label, cols[0]);

        // Value with leading gap
        let mut spans: Vec<Span> = vec![Span::raw("  ")];
        if !field.options.is_empty() {
            let arrow_style = if is_active {
                Style::default().fg(Color::Cyan)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            spans.push(Span::styled("◀ ", arrow_style));
            spans.push(Span::raw(&field.buffer));
            spans.push(Span::styled(" ▶", arrow_style));
        } else if is_active {
            spans.push(Span::raw(&field.buffer));
            spans.push(Span::styled("█", cursor_style));
        } else {
            spans.push(Span::raw(&field.buffer));
        }

        frame.render_widget(Paragraph::new(Line::from(spans)), cols[1]);
    }

    // Footer hint
    let hint_row = fields.len() + 2;
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
