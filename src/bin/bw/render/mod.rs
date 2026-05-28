use std::collections::{HashMap, HashSet};
use std::time::Duration;

use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table};

use build_watcher::format;
use build_watcher::status::{
    ActiveRunView, BuildSample, LastBuildView, PrView, RunConclusion, WatchStatus,
};

use super::app::{App, ExpandLevel, GroupBy, InputMode, SortColumn, SseState};

mod popups;
mod sparkline;

pub(crate) use popups::{
    render_auto_discover_rules_popup, render_build_times_popup, render_form_popup,
    render_help_popup, render_history_popup, render_notification_picker_popup,
    render_pr_picker_popup,
};
pub(crate) use sparkline::sparkline;

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
    /// True until the first successful poll provides data.
    pub waiting: bool,
    /// Compact PR merge-state badge with per-state colors, empty if no PRs.
    pub pr_badge: Line<'static>,
    /// Failing step names from the last build, if any.
    pub failing_steps: Option<String>,
    /// GitHub login of the user who triggered the run.
    pub actor: Option<String>,
    /// Name of the commit author.
    pub commit_author: Option<String>,
    /// The run/build this collapsed row stands for. The detail bar renders it
    /// with the *same* code as a standalone `ActiveRun`/`LastBuild` row — a
    /// single-branch repo header is just the same item shown in a different
    /// place.
    pub source: SingleBranchSource<'a>,
}

/// The underlying run/build a single-branch repo header collapses into.
pub(crate) enum SingleBranchSource<'a> {
    Active(&'a ActiveRunView),
    Last(&'a LastBuildView),
    None,
}

/// Format a compact colored PR badge from a list of `PrView`s.
fn pr_badge(prs: &[PrView]) -> Line<'static> {
    use build_watcher::github::MergeState;
    if prs.is_empty() {
        return Line::default();
    }
    let state_style = |ms: &MergeState| -> Style {
        match ms {
            MergeState::Clean => Style::default().fg(COLOR_SUCCESS),
            MergeState::Dirty | MergeState::Blocked => Style::default().fg(COLOR_FAILURE),
            MergeState::Behind | MergeState::Unstable | MergeState::HasHooks => {
                Style::default().fg(Color::Yellow)
            }
            _ => Style::default().fg(Color::DarkGray),
        }
    };
    let mut spans: Vec<Span<'static>> = vec![Span::raw("[")];
    for (i, pr) in prs.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw(" "));
        }
        let draft_suffix = if pr.draft { "~" } else { "" };
        let label = format!("#{}{}{draft_suffix}", pr.number, pr.merge_state.icon());
        spans.push(Span::styled(label, state_style(&pr.merge_state)));
    }
    spans.push(Span::raw("]"));
    Line::from(spans)
}

pub(crate) enum DisplayRow<'a> {
    GroupHeader {
        label: String,
    },
    /// Top-of-table section divider (currently used for "★ pinned" / "other"
    /// when at least one watch is pinned). Distinct from `GroupHeader` so it
    /// can render with its own style and so detail-bar logic can tell them
    /// apart.
    SectionHeader {
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
        is_workflow_child: bool,
        pr_badge: Line<'static>,
    },
    LastBuild {
        repo: &'a str,
        branch: &'a str,
        build: &'a LastBuildView,
        muted: bool,
        tree_prefix: &'static str,
        is_workflow_child: bool,
        pr_badge: Line<'static>,
    },
    NeverRan {
        repo: &'a str,
        branch: &'a str,
        muted: bool,
        tree_prefix: &'static str,
        waiting: bool,
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
        /// Colored PR badge line, empty if no PRs.
        pr_badge: Line<'static>,
    },
}

/// Aggregate stats for a repo header row.
struct RepoAggregates {
    failing: usize,
    active: usize,
    passing: usize,
    idle: usize,
    newest_age: Option<f64>,
    all_muted: bool,
    failing_workflows: Vec<String>,
}

/// Compute aggregate stats (failing/active/passing/idle counts, newest age, mute state)
/// from a set of branch watches for use in the repo header row.
fn compute_repo_aggregates(branches: &[&WatchStatus]) -> RepoAggregates {
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
        } else if w.waiting {
            // Waiting watches count towards active so they're visible.
            active += 1;
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

    RepoAggregates {
        failing,
        active,
        passing,
        idle,
        newest_age,
        all_muted,
        failing_workflows,
    }
}

/// For single-branch repos with a single workflow, compute inline display info.
/// Returns `None` for multi-branch repos or multi-workflow single-branch repos.
fn compute_single_branch_info<'a>(branches: &[&'a WatchStatus]) -> Option<SingleBranchInfo<'a>> {
    if branches.len() != 1 {
        return None;
    }
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
    if workflow_count > 1 {
        return None; // multi-workflow: will expand into child rows
    }
    let (
        title,
        status_key,
        attempt,
        run_id,
        failed,
        failing_job_id,
        failing_steps,
        actor,
        commit_author,
        source,
    ) = if let Some(run) = w.active_runs.first() {
        (
            run.title.clone(),
            run.status.as_str().to_string(),
            run.attempt,
            Some(run.run_id),
            false,
            None,
            None,
            run.actor.clone(),
            run.commit_author.clone(),
            SingleBranchSource::Active(run),
        )
    } else if let Some(b) = newest_last_build(w) {
        (
            b.title.clone(),
            b.conclusion.as_str().to_string(),
            b.attempt,
            Some(b.run_id),
            b.conclusion != RunConclusion::Success,
            b.failing_job_id,
            b.failing_steps.clone(),
            b.actor.clone(),
            b.commit_author.clone(),
            SingleBranchSource::Last(b),
        )
    } else {
        (
            String::new(),
            String::new(),
            1,
            None,
            false,
            None,
            None,
            None,
            None,
            SingleBranchSource::None,
        )
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
        waiting: w.waiting,
        pr_badge: pr_badge(&w.prs),
        failing_steps,
        actor,
        commit_author,
        source,
    })
}

/// Result of flattening watches into display rows.
pub(crate) struct FlatRows<'a> {
    pub(crate) rows: Vec<DisplayRow<'a>>,
    /// Indices into `rows` that are selectable (everything except `GroupHeader`).
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
    worst_status: Option<(u8, u8, &str)>,
    group_by: GroupBy,
) -> Option<String> {
    match group_by {
        GroupBy::Org => Some(repo.split('/').next().unwrap_or(repo).to_string()),
        GroupBy::Branch => Some(first_branch.to_string()),
        GroupBy::Workflow => Some(workflow.unwrap_or("(none)").to_string()),
        GroupBy::Status => {
            let worst = worst_status.unwrap_or((2, 0, ""));
            Some(if worst.0 <= 1 {
                format::status(worst.2).to_string()
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

/// Group watches by repo, preserving first-seen order. Takes a slice of
/// references so it can be reused for both the pinned and unpinned subsets
/// in `flatten_rows`.
fn group_watches_by_repo<'a>(watches: &[&'a WatchStatus]) -> Vec<(&'a str, Vec<&'a WatchStatus>)> {
    let mut groups: Vec<(&str, Vec<&WatchStatus>)> = Vec::new();
    let mut index: HashMap<&str, usize> = HashMap::new();
    for w in watches {
        if let Some(&idx) = index.get(w.repo.as_str()) {
            groups[idx].1.push(*w);
        } else {
            index.insert(&w.repo, groups.len());
            groups.push((&w.repo, vec![*w]));
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

    // Split into pinned and unpinned. Pinned watches always appear in their
    // own section at the top of the table, regardless of the active grouping.
    let pinned: Vec<&WatchStatus> = watches.iter().filter(|w| w.pinned).collect();
    let unpinned: Vec<&WatchStatus> = watches.iter().filter(|w| !w.pinned).collect();

    if !pinned.is_empty() {
        rows.push(DisplayRow::SectionHeader {
            label: "\u{2605} pinned".to_string(),
        });
        emit_repo_block(
            &pinned,
            group_by,
            expand,
            workflow_collapsed,
            &mut rows,
            &mut selectable,
        );
        if !unpinned.is_empty() {
            rows.push(DisplayRow::SectionHeader {
                label: "other".to_string(),
            });
        }
    }
    if !unpinned.is_empty() {
        emit_repo_block(
            &unpinned,
            group_by,
            expand,
            workflow_collapsed,
            &mut rows,
            &mut selectable,
        );
    }

    FlatRows { rows, selectable }
}

/// Emit DisplayRows for a contiguous set of watches (pinned subset or
/// unpinned subset). Applies the active grouping inside the block.
fn emit_repo_block<'a>(
    watches: &[&'a WatchStatus],
    group_by: GroupBy,
    expand: &HashMap<String, ExpandLevel>,
    workflow_collapsed: &HashSet<String>,
    rows: &mut Vec<DisplayRow<'a>>,
    selectable: &mut Vec<usize>,
) {
    let mut current_group: Option<String> = None;

    let repo_groups = if group_by.splits_repo() {
        // Each watch gets its own group entry so repos appear under each matching group.
        watches
            .iter()
            .map(|w| (w.repo.as_str(), vec![*w]))
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

        let agg = compute_repo_aggregates(branches);

        let expand_level = expand.get(*repo).copied().unwrap_or(ExpandLevel::Full);
        let is_collapsed = expand_level == ExpandLevel::Collapsed;

        let single_branch = compute_single_branch_info(branches);

        let is_single_branch_inline = single_branch.is_some();

        // Repo header row
        selectable.push(rows.len());
        rows.push(DisplayRow::RepoHeader {
            repo,
            branch_count: branches.len(),
            expand_level,
            failing: agg.failing,
            active: agg.active,
            passing: agg.passing,
            idle: agg.idle,
            muted: agg.all_muted && !branches.is_empty(),
            newest_age: agg.newest_age,
            single_branch,
            failing_workflows: agg.failing_workflows,
        });

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
                        rows,
                        selectable,
                    );
                }
            } else if expand_single_branch_workflows {
                let w = branches[0];
                let branch_key = format!("{}#{}", repo, w.branch);
                let show_wf = repo_allows_workflows && !workflow_collapsed.contains(&branch_key);
                emit_branch_workflow_rows(w, "", "", show_wf, rows, selectable);
            }
        }
    }
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
                is_workflow_child: false,
                pr_badge: pr_badge(&w.prs),
            });
        } else if let Some(b) = w.last_builds.first() {
            selectable.push(rows.len());
            rows.push(DisplayRow::LastBuild {
                repo: &w.repo,
                branch: &w.branch,
                build: b,
                muted: w.muted,
                tree_prefix,
                is_workflow_child: false,
                pr_badge: pr_badge(&w.prs),
            });
        } else {
            selectable.push(rows.len());
            rows.push(DisplayRow::NeverRan {
                repo: &w.repo,
                branch: &w.branch,
                muted: w.muted,
                tree_prefix,
                waiting: w.waiting,
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
        pr_badge: pr_badge(&w.prs),
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
                    is_workflow_child: true,
                    pr_badge: Line::default(),
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
                    is_workflow_child: true,
                    pr_badge: Line::default(),
                });
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
    } else if w.waiting {
        (
            "⏳ waiting".to_string(),
            String::new(),
            Style::default().fg(Color::DarkGray),
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
    w.last_builds.iter().min_by_key(|b| b.conclusion.severity())
}

enum WorkflowItem<'a> {
    Active(&'a ActiveRunView),
    Completed(&'a LastBuildView),
}

impl DisplayRow<'_> {
    /// Returns `(repo, branch, run_id, muted)` for selectable rows.
    /// For multi-branch `RepoHeader`, branch is empty. For single-branch, returns the branch name.
    /// Returns `None` for non-selectable rows (`GroupHeader`).
    pub(crate) fn repo_branch_run(&self) -> Option<(&str, &str, Option<u64>, bool)> {
        match self {
            DisplayRow::RepoHeader {
                repo,
                muted,
                single_branch: Some(sb),
                ..
            } => Some((repo, sb.branch, sb.run_id, *muted)),
            DisplayRow::RepoHeader { repo, muted, .. } => Some((repo, "", None, *muted)),
            DisplayRow::ActiveRun {
                repo,
                branch,
                run,
                muted,
                ..
            } => Some((repo, branch, Some(run.run_id), *muted)),
            DisplayRow::LastBuild {
                repo,
                branch,
                build,
                muted,
                ..
            } => Some((repo, branch, Some(build.run_id), *muted)),
            DisplayRow::NeverRan {
                repo,
                branch,
                muted,
                ..
            } => Some((repo, branch, None, *muted)),
            DisplayRow::BranchHeader {
                repo,
                branch,
                muted,
                ..
            } => Some((repo, branch, None, *muted)),
            DisplayRow::GroupHeader { .. } | DisplayRow::SectionHeader { .. } => None,
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

    /// Returns `true` if this row is a workflow-level child (nested under a branch header).
    pub(crate) fn is_workflow_child(&self) -> bool {
        match self {
            DisplayRow::ActiveRun {
                is_workflow_child, ..
            }
            | DisplayRow::LastBuild {
                is_workflow_child, ..
            } => *is_workflow_child,
            _ => false,
        }
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
        // Group-by key as primary sort. For Status we compare the underlying
        // (tier, severity, str) tuple instead of the display string so groups
        // order semantically (active → failure → success → idle) rather than
        // alphabetically.
        let group_ord = match group_by {
            GroupBy::None => std::cmp::Ordering::Equal,
            GroupBy::Status => {
                let ta = a.1.iter().map(watch_status).min();
                let tb = b.1.iter().map(watch_status).min();
                ta.cmp(&tb)
            }
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

/// Status key: (tier, severity, status_str). Tier: active=0, completed=1, idle=2.
/// For completed builds, picks the worst conclusion (matching the displayed
/// branch status) and uses `severity()` so failures sort before cancellations
/// before success — not alphabetically by conclusion string.
pub(crate) fn watch_status(w: &WatchStatus) -> (u8, u8, &'static str) {
    if let Some(run) = w.active_runs.first() {
        (0, 0, run.status.as_str())
    } else if let Some(b) = worst_last_build(w) {
        (1, b.conclusion.severity(), b.conclusion.as_str())
    } else {
        (2, 0, "")
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

// -- Status colours --

pub(super) const COLOR_SUCCESS: Color = Color::Rgb(100, 180, 100);
pub(super) const COLOR_FAILURE: Color = Color::Rgb(220, 100, 100);
const COLOR_ACTIVE: Color = Color::Yellow;

// -- Event application --

/// Append `" (N)"` when attempt > 1, otherwise an empty string.
fn attempt_suffix(attempt: u32) -> String {
    if attempt > 1 {
        format!(" ({attempt})")
    } else {
        String::new()
    }
}

pub(crate) fn status_style(conclusion_or_status: &str) -> Style {
    match conclusion_or_status {
        "success" => Style::default().fg(COLOR_SUCCESS),
        "cancelled" => Style::default().fg(Color::DarkGray),
        "failure" | "timed_out" | "startup_failure" => Style::default().fg(COLOR_FAILURE),
        "in_progress" | "queued" | "waiting" | "requested" | "pending" => {
            Style::default().fg(COLOR_ACTIVE)
        }
        _ => Style::default(),
    }
}

pub(crate) fn status_emoji(conclusion_or_status: &str) -> &'static str {
    match conclusion_or_status {
        "success" => "✓",
        "cancelled" => "⊘",
        "failure" | "timed_out" | "startup_failure" => "✗",
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

    let s = &app.stats;
    let uptime = format::seconds(s.uptime_secs);
    let aggr = format!(" [{}]", s.poll_aggression);
    let poll = format!("poll {}s{aggr}", s.poll_secs);
    let api = match (s.rate_remaining, s.rate_limit) {
        (Some(rem), Some(lim)) => {
            let pct = (rem * 100).checked_div(lim).unwrap_or(0);
            let reset = s
                .rate_reset_mins
                .map(|m| format!("  reset {m}m"))
                .unwrap_or_default();
            format!("API {rem} · {lim} ({pct}%){reset}")
        }
        _ => "API —".to_string(),
    };

    // State indicators appended after stats (paused, SSE issues, errors, flash, update).
    let mut indicators: Vec<Span> = Vec::new();
    if app.status.paused {
        indicators.push(Span::styled(
            "  · NOTIFICATIONS PAUSED",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
    }
    match &app.sse_state {
        SseState::Connecting => {
            indicators.push(Span::styled(
                "  · connecting…",
                Style::default().fg(Color::Yellow),
            ));
        }
        SseState::Disconnected { since } => {
            indicators.push(Span::styled(
                format!("  · reconnecting ({}s)", since.elapsed().as_secs()),
                Style::default().fg(Color::Yellow),
            ));
        }
        SseState::Connected => {}
    }
    if let Some(err) = &app.fetch_error {
        let stale_secs = app.last_fetch.elapsed().as_secs();
        indicators.push(Span::styled(
            format!("  · {err} ({stale_secs}s stale)"),
            Style::default().fg(COLOR_FAILURE),
        ));
    }
    if let Some((msg, at)) = &app.flash
        && at.elapsed() < FLASH_DURATION
    {
        indicators.push(Span::styled(
            format!("  {msg}"),
            Style::default().fg(Color::Cyan),
        ));
    }
    if let Some(version) = &app.update_available {
        indicators.push(Span::styled(
            format!("  · {version} available"),
            Style::default().fg(Color::Yellow),
        ));
    }
    let instance_label = app
        .config_dir_label
        .as_deref()
        .map(|l| format!(" [{l}]"))
        .unwrap_or_default();
    let left_prefix = format!("build-watcher{instance_label}");
    let left_suffix = format!(" — up {uptime}");
    let right = format!("{poll}  {api}");
    let indicators_len: usize = indicators.iter().map(|s| s.content.chars().count()).sum();
    let left_len = left_prefix.chars().count() + left_suffix.chars().count();
    let gap = w.saturating_sub(left_len + right.chars().count() + indicators_len);

    let mut spans = vec![
        Span::styled(left_prefix, Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(left_suffix),
        Span::raw(" ".repeat(gap)),
        Span::styled(right, dim),
    ];
    spans.extend(indicators);

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

pub(crate) fn render_body<'a>(
    frame: &mut ratatui::Frame,
    area: ratatui::layout::Rect,
    app: &App,
    flat: &FlatRows<'a>,
    cw: &ColWidths,
) {
    let border_style = Style::default().fg(Color::DarkGray);
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

    // Bordered panel wrapping both column headings and the scrollable table.
    // Always show the active group-by in the panel title so it's discoverable.
    let dim = Style::default().fg(Color::DarkGray);
    let panel = Block::default()
        .borders(Borders::ALL)
        .border_style(border_style)
        .title_top(
            Line::from(Span::styled(
                format!(" group: {} ", app.group_by.label()),
                dim,
            ))
            .right_aligned(),
        );
    let inner = panel.inner(area);
    frame.render_widget(panel, area);

    // Split the inner area: 1 row for column headings, rest for table rows.
    let inner_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Fill(1)])
        .split(inner);
    let heading_area = inner_chunks[0];
    let body_area = inner_chunks[1];

    let selected_display_idx = flat
        .selectable
        .get(app.selected)
        .copied()
        .unwrap_or(usize::MAX);

    let total_rows = flat.rows.len();
    let body_height = body_area.height as usize;

    // Compute scroll offset so the selected row stays visible, centered when possible.
    let scroll_offset = if total_rows <= body_height {
        0
    } else {
        selected_display_idx
            .saturating_sub(body_height / 2)
            .min(total_rows - body_height)
    };

    // Use a subtle dark background for the selected row. Rgb avoids
    // terminal palette remapping that can override foreground colours
    // on some Mac terminals (Terminal.app, some iTerm2 themes).
    let highlight_style = Style::default().bg(Color::Rgb(40, 40, 50));

    let mute_indicator = |muted: bool| -> &'static str { if muted { " 🔇" } else { "" } };

    // Repos currently in quarantine (continuously 404ing). The snapshot
    // already carries `quarantined_secs` per branch row; collapse to one
    // entry per repo (the max elapsed) so the badge is rendered once on the
    // repo header and the whole row group is dimmed. See incident
    // 2026-05-28: flaky 404s used to instantly delete user config, now they
    // surface as visibly quarantined instead.
    let quarantined_secs: std::collections::HashMap<&str, u64> =
        app.status
            .watches
            .iter()
            .fold(std::collections::HashMap::new(), |mut m, w| {
                if let Some(s) = w.quarantined_secs {
                    m.entry(w.repo.as_str())
                        .and_modify(|cur| *cur = (*cur).max(s))
                        .or_insert(s);
                }
                m
            });
    let quarantined_for = |repo: &str| -> Option<u64> { quarantined_secs.get(repo).copied() };

    let rows: Vec<Row> = flat
        .rows
        .iter()
        .enumerate()
        .skip(scroll_offset)
        .take(body_height)
        .map(|(i, dr)| {
            let row = render_display_row(dr, cw, &mute_indicator, &quarantined_for);
            let is_quarantined = dr
                .repo_branch_run()
                .is_some_and(|(repo, _, _, _)| quarantined_secs.contains_key(repo));
            let is_selected = i == selected_display_idx;
            let mut row_style = Style::default();
            if is_quarantined {
                row_style = row_style.add_modifier(Modifier::DIM);
            }
            if is_selected {
                row_style = row_style.patch(highlight_style);
            }
            if is_quarantined || is_selected {
                row.style(row_style)
            } else {
                row
            }
        })
        .collect();

    let widths = cw.constraints();

    let heading_table = Table::new(vec![col_header], widths).column_spacing(COL_SPACING);
    frame.render_widget(heading_table, heading_area);

    if flat.rows.is_empty() {
        let dim = Style::default().fg(Color::DarkGray);
        let hint = if matches!(app.sse_state, SseState::Connecting) {
            Line::from(Span::styled("  Loading watches…", dim))
        } else {
            Line::from(vec![
                Span::styled("  No repos watched. Press ", dim),
                Span::styled(
                    "a",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(" to add a repo.", dim),
            ])
        };
        frame.render_widget(Paragraph::new(hint), body_area);
    } else {
        let body_table = Table::new(rows, widths).column_spacing(COL_SPACING);
        frame.render_widget(body_table, body_area);
    }

    // Overlay scroll indicators on the panel border when content overflows.
    // Rendered last so they appear on top of the border characters.
    if total_rows > body_height {
        let indicator_style = Style::default().fg(Color::DarkGray);
        let right_col = area.x + area.width.saturating_sub(2);
        if scroll_offset > 0 {
            frame.render_widget(
                Paragraph::new("▲").style(indicator_style),
                ratatui::layout::Rect::new(right_col, area.y, 1, 1),
            );
        }
        if scroll_offset + body_height < total_rows {
            frame.render_widget(
                Paragraph::new("▼").style(indicator_style),
                ratatui::layout::Rect::new(right_col, area.y + area.height.saturating_sub(1), 1, 1),
            );
        }
    }
}

/// Combine branch name with a colored PR badge into a single styled line.
fn format_branch_with_pr(branch: &str, pr_badge: Line<'static>) -> Line<'static> {
    if pr_badge.spans.is_empty() {
        Line::from(branch.to_string())
    } else {
        let mut spans = vec![Span::raw(branch.to_string()), Span::raw(" ")];
        spans.extend(pr_badge.spans);
        Line::from(spans)
    }
}

/// Build a styled title line, appending failing steps in red when present.
fn title_line(
    title: &str,
    steps: Option<&str>,
    author: Option<&str>,
    max_width: usize,
) -> Line<'static> {
    let dim = Style::default().fg(Color::DarkGray);
    let mut spans = vec![Span::raw(format::truncate(title, max_width))];
    if let Some(s) = steps.filter(|s| !s.is_empty()) {
        let sep = " · ";
        let available = max_width.saturating_sub(title.chars().count() + sep.chars().count());
        spans.push(Span::styled(sep.to_string(), dim));
        spans.push(Span::styled(
            format::truncate(s, available),
            Style::default().fg(COLOR_FAILURE),
        ));
    }
    if let Some(a) = author.filter(|a| !a.is_empty()) {
        let used: usize = spans.iter().map(|s| s.content.chars().count()).sum();
        let remaining = max_width.saturating_sub(used + 3); // " · " separator
        if remaining > 3 {
            spans.push(Span::styled(" · ".to_string(), dim));
            spans.push(Span::styled(format::truncate(a, remaining), dim));
        }
    }
    Line::from(spans)
}

/// Build a 6-cell Row for a branch-level entry (ActiveRun, LastBuild, NeverRan).
#[allow(clippy::too_many_arguments)]
fn branch_row<'a>(
    tree_label: &str,
    branch_col: Line<'static>,
    muted: bool,
    tree_prefix: &str,
    status_text: &str,
    workflow: &str,
    title: Line<'static>,
    age_or_elapsed: &str,
    style: Style,
    cw: &ColWidths,
    mute_indicator: &dyn Fn(bool) -> &'static str,
) -> Row<'a> {
    let tree_name = format!("  {tree_prefix}{}{}", tree_label, mute_indicator(muted));
    Row::new(vec![
        Cell::from(format::truncate(&tree_name, cw.repo)),
        Cell::from(branch_col),
        Cell::from(format::truncate(status_text, cw.status)).style(style),
        Cell::from(format::truncate(workflow, cw.workflow)),
        Cell::from(title),
        Cell::from(age_or_elapsed.to_string()).style(style),
    ])
}

fn render_group_header<'a>(label: &str) -> Row<'a> {
    let group_style = Style::default()
        .fg(Color::Cyan)
        .bg(Color::Rgb(25, 30, 40))
        .add_modifier(Modifier::BOLD);
    Row::new(vec![
        Cell::from(format!("  {label}")).style(group_style),
        Cell::from(""),
        Cell::from(""),
        Cell::from(""),
        Cell::from(""),
        Cell::from(""),
    ])
    .style(Style::default().bg(Color::Rgb(25, 30, 40)))
}

/// Top-level section divider (pinned / other). Rendered as a faint dashed rule
/// with the label inset — subtle enough to separate the sections without
/// competing with the data rows or the group headers nested inside them.
fn render_section_header<'a>(label: &str, cw: &ColWidths) -> Row<'a> {
    let style = Style::default().fg(Color::DarkGray);
    let dashes = |w: usize| "─".repeat(w);
    // First column: "── label ──────" padded with dashes to the column width.
    let prefix = format!("── {label} ");
    let pad = cw.repo.saturating_sub(prefix.chars().count());
    let first = format!("{prefix}{}", "─".repeat(pad));
    Row::new(vec![
        Cell::from(first).style(style),
        Cell::from(dashes(cw.branch)).style(style),
        Cell::from(dashes(cw.status)).style(style),
        Cell::from(dashes(cw.workflow)).style(style),
        Cell::from(dashes(cw.title)).style(style),
        Cell::from(dashes(AGE_W)).style(style),
    ])
}

#[allow(clippy::too_many_arguments)]
fn render_repo_header<'a>(
    repo: &str,
    branch_count: &usize,
    expand_level: &ExpandLevel,
    failing: &usize,
    active: &usize,
    passing: &usize,
    idle: &usize,
    muted: &bool,
    newest_age: &Option<f64>,
    single_branch: &Option<SingleBranchInfo<'_>>,
    failing_workflows: &[String],
    cw: &ColWidths,
    mute_indicator: &dyn Fn(bool) -> &'static str,
    quarantined_secs: Option<u64>,
) -> Row<'a> {
    // A quarantined repo overrides the per-branch/per-workflow status display
    // with a single `⊘ unreachable Xh` badge so the reason for the dimming is
    // visible. The recent-builds info is stale anyway while the repo is
    // continuously 404ing; the badge tells you the elapsed time and the
    // detail bar (when selected) shows the deletion-grace remaining.
    let quarantine_badge = quarantined_secs.map(|s| format!("⊘ unreachable {}", format::age(s)));
    let quarantine_style = Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD);
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

    let age = newest_age
        .map(|s| format::age(s as u64))
        .unwrap_or_default();

    let repo_style = Style::default().add_modifier(Modifier::BOLD);

    // Single-branch repos: show branch name, workflow, and title inline
    // with the actual status (e.g. "✅ success") instead of aggregate counts.
    if let Some(sb) = single_branch {
        let emoji = status_emoji(&sb.status_key);
        let style = status_style(&sb.status_key);
        let sfx = attempt_suffix(sb.attempt);
        let inline_status = if sb.status_key.is_empty() {
            if sb.waiting {
                "⏳ waiting".to_string()
            } else {
                "· idle".to_string()
            }
        } else {
            format!("{emoji} {}{sfx}", format::status(&sb.status_key))
        };
        let branch_cell = format_branch_with_pr(sb.branch, sb.pr_badge.clone());
        let author = sb.commit_author.as_deref().or(sb.actor.as_deref());
        let title = title_line(&sb.title, sb.failing_steps.as_deref(), author, cw.title);
        let (status_text, status_style) = match &quarantine_badge {
            Some(badge) => (badge.as_str(), quarantine_style),
            None => (inline_status.as_str(), style),
        };
        Row::new(vec![
            Cell::from(format::truncate(&name, cw.repo)).style(repo_style),
            Cell::from(branch_cell),
            Cell::from(format::truncate(status_text, cw.status)).style(status_style),
            Cell::from(format::truncate(&sb.workflows, cw.workflow)),
            Cell::from(title),
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

        // Color-coded status: active/passing/failing/idle — dimmed when zero.
        // Replaced by the quarantine badge when the repo is unreachable so
        // the user doesn't read the stale counts as live state.
        let status_cell = if let Some(badge) = &quarantine_badge {
            Cell::from(format::truncate(badge, cw.status)).style(quarantine_style)
        } else {
            let dim = |v: usize, s: Style| {
                if v > 0 {
                    s
                } else {
                    Style::default().fg(Color::DarkGray)
                }
            };
            let sep = Span::styled("·", Style::default().fg(Color::DarkGray));
            let spans = vec![
                Span::styled(
                    format!("{active}"),
                    dim(*active, Style::default().fg(COLOR_ACTIVE)),
                ),
                sep.clone(),
                Span::styled(
                    format!("{passing}"),
                    dim(*passing, Style::default().fg(COLOR_SUCCESS)),
                ),
                sep.clone(),
                Span::styled(
                    format!("{failing}"),
                    dim(*failing, Style::default().fg(COLOR_FAILURE)),
                ),
                sep,
                Span::styled(format!("{idle}"), Style::default().fg(Color::DarkGray)),
            ];
            Cell::from(Line::from(spans))
        };

        Row::new(vec![
            Cell::from(format::truncate(&name, cw.repo)).style(repo_style),
            Cell::from(format::truncate(&branch_label, cw.branch)),
            status_cell,
            Cell::from(format::truncate(&wf_label, cw.workflow)),
            Cell::from(""),
            Cell::from(age),
        ])
    }
}

#[allow(clippy::too_many_arguments)]
fn render_active_run<'a>(
    branch: &str,
    run: &ActiveRunView,
    extra_badge: &str,
    muted: bool,
    tree_prefix: &str,
    is_workflow_child: bool,
    pr_badge: &Line<'static>,
    cw: &ColWidths,
    mute_indicator: &dyn Fn(bool) -> &'static str,
) -> Row<'a> {
    let status_str = run.status.as_str();
    let style = status_style(status_str);
    let emoji = status_emoji(status_str);
    let elapsed = run
        .elapsed_secs
        .map(|s| format::duration(Duration::from_secs_f64(s)))
        .unwrap_or_default();
    let sfx = attempt_suffix(run.attempt);
    let status_text = if extra_badge.is_empty() {
        format!("{emoji} {}{sfx}", format::status(status_str))
    } else {
        format!("{emoji} {}{sfx} {extra_badge}", format::status(status_str))
    };
    let author = run.commit_author.as_deref().or(run.actor.as_deref());
    let title = title_line(&run.title, None, author, cw.title);
    if is_workflow_child {
        branch_row(
            &run.workflow,
            Line::default(),
            false,
            tree_prefix,
            &status_text,
            "",
            title,
            &elapsed,
            style,
            cw,
            mute_indicator,
        )
    } else {
        branch_row(
            branch,
            format_branch_with_pr(branch, pr_badge.clone()),
            muted,
            tree_prefix,
            &status_text,
            &run.workflow,
            title,
            &elapsed,
            style,
            cw,
            mute_indicator,
        )
    }
}

#[allow(clippy::too_many_arguments)]
fn render_last_build<'a>(
    branch: &str,
    build: &LastBuildView,
    muted: bool,
    tree_prefix: &str,
    is_workflow_child: bool,
    pr_badge: &Line<'static>,
    cw: &ColWidths,
    mute_indicator: &dyn Fn(bool) -> &'static str,
) -> Row<'a> {
    let conclusion_str = build.conclusion.as_str();
    let style = status_style(conclusion_str);
    let emoji = status_emoji(conclusion_str);
    let age = build
        .age_secs
        .map(|s| format::age(s as u64))
        .unwrap_or_default();
    let sfx = attempt_suffix(build.attempt);
    let status_text = format!("{emoji} {}{sfx}", format::status(conclusion_str));
    let author = build.commit_author.as_deref().or(build.actor.as_deref());
    let title = title_line(
        &build.title,
        build.failing_steps.as_deref(),
        author,
        cw.title,
    );
    if is_workflow_child {
        branch_row(
            &build.workflow,
            Line::default(),
            false,
            tree_prefix,
            &status_text,
            "",
            title,
            &age,
            style,
            cw,
            mute_indicator,
        )
    } else {
        branch_row(
            branch,
            format_branch_with_pr(branch, pr_badge.clone()),
            muted,
            tree_prefix,
            &status_text,
            &build.workflow,
            title,
            &age,
            style,
            cw,
            mute_indicator,
        )
    }
}

fn render_never_ran<'a>(
    branch: &str,
    muted: bool,
    tree_prefix: &str,
    waiting: bool,
    cw: &ColWidths,
    mute_indicator: &dyn Fn(bool) -> &'static str,
) -> Row<'a> {
    branch_row(
        branch,
        Line::from(branch.to_string()),
        muted,
        tree_prefix,
        if waiting { "⏳ waiting" } else { "· idle" },
        "",
        Line::default(),
        "",
        Style::default().fg(Color::DarkGray),
        cw,
        mute_indicator,
    )
}

#[allow(clippy::too_many_arguments)]
fn render_branch_header<'a>(
    branch: &str,
    muted: bool,
    tree_prefix: &str,
    workflow_count: usize,
    expanded: bool,
    status_text: &str,
    age_or_elapsed: &str,
    style: Style,
    pr_badge: &Line<'static>,
    cw: &ColWidths,
    mute_indicator: &dyn Fn(bool) -> &'static str,
) -> Row<'a> {
    let expand_indicator = if expanded { "▾" } else { "▸" };
    let wf_label = format!("{expand_indicator} {workflow_count} workflows");
    branch_row(
        branch,
        format_branch_with_pr(branch, pr_badge.clone()),
        muted,
        tree_prefix,
        status_text,
        &wf_label,
        Line::default(),
        age_or_elapsed,
        style,
        cw,
        mute_indicator,
    )
}

fn render_display_row<'a>(
    dr: &DisplayRow<'_>,
    cw: &ColWidths,
    mute_indicator: &dyn Fn(bool) -> &'static str,
    quarantined_for: &dyn Fn(&str) -> Option<u64>,
) -> Row<'a> {
    match dr {
        DisplayRow::GroupHeader { label } => render_group_header(label),
        DisplayRow::SectionHeader { label } => render_section_header(label, cw),
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
        } => render_repo_header(
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
            cw,
            mute_indicator,
            quarantined_for(repo),
        ),
        DisplayRow::ActiveRun {
            branch,
            run,
            extra_badge,
            muted,
            tree_prefix,
            is_workflow_child,
            pr_badge,
            ..
        } => render_active_run(
            branch,
            run,
            extra_badge,
            *muted,
            tree_prefix,
            *is_workflow_child,
            pr_badge,
            cw,
            mute_indicator,
        ),
        DisplayRow::LastBuild {
            branch,
            build,
            muted,
            tree_prefix,
            is_workflow_child,
            pr_badge,
            ..
        } => render_last_build(
            branch,
            build,
            *muted,
            tree_prefix,
            *is_workflow_child,
            pr_badge,
            cw,
            mute_indicator,
        ),
        DisplayRow::NeverRan {
            branch,
            muted,
            tree_prefix,
            waiting,
            ..
        } => render_never_ran(branch, *muted, tree_prefix, *waiting, cw, mute_indicator),
        DisplayRow::BranchHeader {
            branch,
            muted,
            tree_prefix,
            workflow_count,
            expanded,
            status_text,
            age_or_elapsed,
            style,
            pr_badge,
            ..
        } => render_branch_header(
            branch,
            *muted,
            tree_prefix,
            *workflow_count,
            *expanded,
            status_text,
            age_or_elapsed,
            *style,
            pr_badge,
            cw,
            mute_indicator,
        ),
    }
}

pub(crate) fn render_recent_panel(
    frame: &mut ratatui::Frame,
    area: ratatui::layout::Rect,
    app: &App,
    cw: &ColWidths,
) {
    let dim = Style::default().fg(Color::DarkGray);
    let border_style = Style::default().fg(Color::DarkGray);

    let panel = Block::default()
        .borders(Borders::ALL)
        .border_style(border_style)
        .title(Span::styled(" Recent ", dim));
    let inner = panel.inner(area);
    frame.render_widget(panel, area);

    let rows: Vec<Row> = app
        .recent_history
        .iter()
        .take(inner.height as usize)
        .map(|entry| {
            let style = status_style(entry.conclusion.as_str());
            let emoji = status_emoji(entry.conclusion.as_str());
            let repo = format::truncate(&entry.repo, cw.repo);
            let branch = format::truncate(&entry.branch, cw.branch);
            let status_cell = format!("{emoji} {}", format::status(entry.conclusion.as_str()));
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
    frame.render_widget(table, inner);
}

/// Separator span used between detail bar fields.
fn detail_sep() -> Span<'static> {
    Span::styled("  ·  ", Style::default().fg(Color::DarkGray))
}

/// Append author info to a detail bar span list.
/// Shows commit author name; if the triggering actor differs, appends `[by actor]`.
fn push_author<'a>(
    s: &mut Vec<Span<'a>>,
    actor: &Option<String>,
    commit_author: &Option<String>,
    label_style: Style,
    dim: Style,
) {
    let author = commit_author.as_deref().filter(|a| !a.is_empty());
    let actor = actor.as_deref().filter(|a| !a.is_empty());
    match (author, actor) {
        (Some(author), Some(actor)) if author != actor => {
            s.push(detail_sep());
            s.push(Span::styled(author.to_string(), dim));
            s.push(Span::styled(format!(" [by {actor}]"), label_style));
        }
        (Some(author), _) => {
            s.push(detail_sep());
            s.push(Span::styled(author.to_string(), dim));
        }
        (None, Some(actor)) => {
            s.push(detail_sep());
            s.push(Span::styled(actor.to_string(), dim));
        }
        (None, None) => {}
    }
}

/// Append the 7-day duration trend to a detail bar span list.
///
/// Layout: `· avg 4:10 (3:42–5:18) ▂▃▅▄▂▃▆▄`
///
/// The trend is one logical chunk — average with the (min–max) range that
/// jitters around it, then the sparkline visualising the spread. Min/max are
/// included only when there's actual variance; otherwise the parenthesised
/// range is omitted (avoiding noisy `(4:10–4:10)`).
///
/// Skips entirely when there are no successful samples in the window
/// (avg is `None`).
fn push_trend_spans<'a>(
    s: &mut Vec<Span<'a>>,
    avg: Option<u64>,
    samples: &[BuildSample],
    label_style: Style,
    dim: Style,
) {
    let fmt = |secs: u64| format::duration(Duration::from_secs(secs));

    let Some(avg_s) = avg.filter(|&a| a > 0) else {
        return;
    };

    s.push(detail_sep());
    s.push(Span::styled("avg ", label_style));
    s.push(Span::styled(fmt(avg_s), dim));

    // Min/max are derived from Success samples only — the avg they bracket is
    // the typical-runtime stat, so the range should track the same population.
    let success_durations: Vec<u64> = samples
        .iter()
        .filter(|b| b.conclusion == RunConclusion::Success)
        .map(|b| b.duration_secs)
        .collect();
    if let (Some(&min), Some(&max)) = (
        success_durations.iter().min(),
        success_durations.iter().max(),
    ) && min != max
    {
        s.push(Span::styled(
            format!(" ({}\u{2013}{})", fmt(min), fmt(max)),
            dim,
        ));
    }

    let spark = sparkline(samples);
    if !spark.is_empty() {
        s.push(Span::raw(" "));
        s.extend(spark);
    }
}

/// Detail-bar spans for an active run: run id, event, retry, duration trend,
/// author. Shared by the `ActiveRun` row and a single-branch repo header that
/// collapses one — the bar content is identical regardless of where the item
/// is shown. Status/repo/branch/workflow already appear in the row's columns.
fn active_run_detail_spans<'a>(
    run: &'a ActiveRunView,
    label_style: Style,
    dim: Style,
) -> Vec<Span<'a>> {
    let mut s = vec![
        Span::styled("run ", label_style),
        Span::styled(run.run_id.to_string(), dim),
    ];
    if !run.event.is_empty() {
        s.push(detail_sep());
        s.push(Span::styled(&run.event, dim));
    }
    if run.attempt > 1 {
        s.push(detail_sep());
        s.push(Span::styled("retry ", label_style));
        s.push(Span::styled(
            format!("#{}", run.attempt),
            Style::default().fg(Color::Yellow),
        ));
    }
    push_trend_spans(
        &mut s,
        run.avg_duration_secs,
        &run.recent_builds,
        label_style,
        dim,
    );
    push_author(&mut s, &run.actor, &run.commit_author, label_style, dim);
    s
}

/// Detail-bar spans for a completed build: run id, retry, failing steps, exact
/// duration, 7-day trend, author. Shared by the `LastBuild` row and a
/// single-branch repo header that collapses one.
fn last_build_detail_spans<'a>(
    build: &'a LastBuildView,
    label_style: Style,
    dim: Style,
) -> Vec<Span<'a>> {
    let mut s = vec![
        Span::styled("run ", label_style),
        Span::styled(build.run_id.to_string(), dim),
    ];
    if build.attempt > 1 {
        s.push(detail_sep());
        s.push(Span::styled("retry ", label_style));
        s.push(Span::styled(
            format!("#{}", build.attempt),
            Style::default().fg(Color::Yellow),
        ));
    }
    if let Some(steps) = &build.failing_steps {
        s.push(detail_sep());
        s.push(Span::styled("failed: ", label_style));
        s.push(Span::styled(
            steps.as_str(),
            Style::default().fg(COLOR_FAILURE),
        ));
    }
    if let Some(d) = build.duration_secs {
        s.push(detail_sep());
        s.push(Span::styled("took ", label_style));
        s.push(Span::styled(format::duration(Duration::from_secs(d)), dim));
    }
    push_trend_spans(
        &mut s,
        build.avg_duration_secs,
        &build.recent_builds,
        label_style,
        dim,
    );
    push_author(&mut s, &build.actor, &build.commit_author, label_style, dim);
    s
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

    let sel_idx = flat
        .selectable
        .get(app.selected)
        .copied()
        .and_then(|i| flat.rows.get(i));

    // If the selected row belongs to a quarantined repo, prepend the
    // unreachable badge + remaining-grace countdown so it's obvious why the
    // row is dimmed and when the daemon will give up. Pulled from the
    // snapshot (max elapsed across the repo's branch entries — they all share
    // the same `quarantined_at`, but max is robust to staggered updates).
    let quarantined_selected_secs: Option<u64> = sel_idx
        .and_then(|dr| dr.repo_branch_run())
        .and_then(|(repo, _, _, _)| {
            app.status
                .watches
                .iter()
                .filter(|w| w.repo == repo)
                .filter_map(|w| w.quarantined_secs)
                .max()
        });

    let spans: Vec<Span<'_>> = match sel_idx {
        Some(DisplayRow::RepoHeader {
            branch_count,
            failing,
            active,
            passing,
            idle,
            single_branch,
            ..
        }) => {
            // A single-branch repo header collapses one run/build: render its
            // detail bar with the very same builder a standalone row would use,
            // so the trend/sparkline and everything else appear identically. For
            // a true multi-branch header, summarise the branch counts instead.
            let mut s: Vec<Span<'_>> = Vec::new();
            if let Some(sb) = single_branch {
                match &sb.source {
                    SingleBranchSource::Active(run) => {
                        s = active_run_detail_spans(run, label_style, dim);
                    }
                    SingleBranchSource::Last(build) => {
                        s = last_build_detail_spans(build, label_style, dim);
                    }
                    SingleBranchSource::None => {
                        let msg = if sb.waiting {
                            "waiting for first poll"
                        } else {
                            "no builds yet"
                        };
                        s.push(Span::styled(msg, dim));
                    }
                }
            } else {
                s.push(Span::styled(format!("{} branches", branch_count), dim));
                s.push(detail_sep());
                s.push(Span::styled(
                    format!("{} pending", active),
                    if *active > 0 {
                        Style::default().fg(Color::Yellow)
                    } else {
                        dim
                    },
                ));
                s.push(detail_sep());
                s.push(Span::styled(
                    format!("{} success", passing),
                    if *passing > 0 {
                        Style::default().fg(COLOR_SUCCESS)
                    } else {
                        dim
                    },
                ));
                s.push(detail_sep());
                s.push(Span::styled(
                    format!("{} failure", failing),
                    if *failing > 0 {
                        Style::default().fg(COLOR_FAILURE)
                    } else {
                        dim
                    },
                ));
                s.push(detail_sep());
                s.push(Span::styled(format!("{} idle", idle), dim));
            }
            s
        }
        Some(DisplayRow::ActiveRun { run, .. }) => active_run_detail_spans(run, label_style, dim),
        Some(DisplayRow::LastBuild { build, .. }) => {
            last_build_detail_spans(build, label_style, dim)
        }
        Some(DisplayRow::NeverRan { waiting, .. }) => {
            // Repo/branch already in the selected row's columns.
            let msg = if *waiting {
                "waiting for first poll"
            } else {
                "no builds yet"
            };
            vec![Span::styled(msg, dim)]
        }
        Some(DisplayRow::GroupHeader { label }) => {
            vec![
                Span::styled("group ", label_style),
                Span::styled(label.as_str(), dim),
            ]
        }
        Some(DisplayRow::BranchHeader {
            workflow_count,
            expanded,
            ..
        }) => {
            // Repo/branch already in the selected row's columns.
            let state = if *expanded { "expanded" } else { "collapsed" };
            vec![Span::styled(
                format!("{workflow_count} workflows ({state})"),
                dim,
            )]
        }
        Some(DisplayRow::SectionHeader { .. }) | None => vec![],
    };

    // Pad with a leading space to align with the body panel's inner content.
    let mut all_spans = vec![Span::raw(" ")];
    if let Some(secs) = quarantined_selected_secs {
        const SIX_HOURS: u64 = 6 * 60 * 60;
        let remaining = SIX_HOURS.saturating_sub(secs);
        all_spans.push(Span::styled(
            format!("⊘ unreachable {}", format::age(secs)),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
        all_spans.push(Span::styled(
            format!(" · deletes in {}", format::age(remaining)),
            dim,
        ));
        all_spans.push(detail_sep());
    }
    all_spans.extend(spans);

    // Right-align repos/branches counts and the "? help" hint with version.
    let hint_style = Style::default().fg(Color::DarkGray);
    let branch_count = app.status.watches.len();
    let repo_count = app
        .status
        .watches
        .iter()
        .map(|w| w.repo.as_str())
        .collect::<std::collections::HashSet<_>>()
        .len();
    let counts = format!("{repo_count} repos · {branch_count} branches");
    let hint = format!("? help · v{}", env!("CARGO_PKG_VERSION"));
    let right = format!("{counts}  ·  {hint}");
    // chars().count() approximates display width (matches `format::truncate` semantics
    // used elsewhere). Avoids byte-count over-counting from multi-byte UTF-8 chars
    // like `·` that appear in the detail spans.
    let content_len: usize = all_spans.iter().map(|s| s.content.chars().count()).sum();
    let pad = (area.width as usize).saturating_sub(content_len + right.chars().count());
    all_spans.push(Span::raw(" ".repeat(pad)));
    all_spans.push(Span::styled(counts, hint_style));
    all_spans.push(Span::styled("  ·  ", hint_style));
    all_spans.push(Span::styled(hint, hint_style));

    frame.render_widget(Paragraph::new(Line::from(all_spans)), area);
}

pub(crate) fn render_footer(frame: &mut ratatui::Frame, area: ratatui::layout::Rect, app: &App) {
    let footer = match &app.input_mode {
        InputMode::TextInput { prompt, editor, .. } => {
            let (before, cursor_ch, after) = editor.split_at_cursor();
            let cursor_str = cursor_ch.unwrap_or(' ').to_string();
            Paragraph::new(Line::from(vec![
                Span::styled(prompt.as_str(), Style::default().fg(Color::Cyan)),
                Span::raw(before.to_string()),
                Span::styled(
                    cursor_str,
                    Style::default().fg(Color::Black).bg(Color::Cyan),
                ),
                Span::raw(after.to_string()),
                Span::styled(
                    "  [Enter] confirm  [Esc] cancel",
                    Style::default().fg(Color::DarkGray),
                ),
            ]))
        }
        InputMode::Form { .. }
        | InputMode::NotificationPicker { .. }
        | InputMode::History { .. }
        | InputMode::PrPicker { .. }
        | InputMode::BuildTimes { .. }
        | InputMode::AutoDiscoverRules { .. } => Paragraph::new(""),
        InputMode::Normal => Paragraph::new(""),
    };

    frame.render_widget(footer, area);
}

pub(crate) fn render(frame: &mut ratatui::Frame, app: &App) {
    let area = frame.area();
    // Subtract 2 for the left/right border of the body and recent panels.
    let cw = ColWidths::from_terminal_width(area.width.saturating_sub(2));

    // Sort and flatten watches once for the entire render pass.
    let sorted = sorted_watches(
        &app.status.watches,
        app.sort_column,
        app.sort_ascending,
        app.group_by,
    );
    let flat = flatten_rows(&sorted, app.group_by, &app.expand, &app.workflow_collapsed);

    let recent_count = app.recent_history.len();
    // recent panel height: recent_height rows of content + 2 for top/bottom borders
    let recent_height = recent_count.min(10) as u16;
    let recent_panel_height = recent_height + 2;
    let show_recent = app.show_recent_panel && recent_height > 0;

    let needs_input_line = matches!(app.input_mode, InputMode::TextInput { .. });
    let footer_height = if needs_input_line {
        1 // just the text input prompt
    } else {
        0
    };

    // Layout: header, then body panel fills available space, then optional recent panel,
    // then detail bar (1 row) and footer snap to the bottom.
    let chunks = if show_recent {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),                   // [0] header
                Constraint::Fill(1),                     // [1] body panel (bordered, scrollable)
                Constraint::Length(recent_panel_height), // [2] recent panel (bordered)
                Constraint::Length(1),                   // [3] detail bar (1 plain row)
                Constraint::Length(footer_height),       // [4] footer
            ])
            .split(area)
    } else {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),             // [0] header
                Constraint::Fill(1),               // [1] body panel (bordered, scrollable)
                Constraint::Length(1),             // [2] detail bar (1 plain row)
                Constraint::Length(footer_height), // [3] footer
            ])
            .split(area)
    };

    render_header(frame, chunks[0], app);
    render_body(frame, chunks[1], app, &flat, &cw);
    if show_recent {
        render_recent_panel(frame, chunks[2], app, &cw);
        render_detail_bar(frame, chunks[3], app, &flat);
        if footer_height > 0 {
            render_footer(frame, chunks[4], app);
        }
    } else {
        render_detail_bar(frame, chunks[2], app, &flat);
        if footer_height > 0 {
            render_footer(frame, chunks[3], app);
        }
    }

    // Overlay the form popup if active.
    if let InputMode::Form {
        title,
        fields,
        active,
        ..
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

    // Overlay the PR picker popup if active.
    if let InputMode::PrPicker {
        repo,
        prs,
        selected,
    } = &app.input_mode
    {
        render_pr_picker_popup(frame, repo, prs, *selected);
    }

    // Overlay the build times popup if active.
    if let InputMode::BuildTimes {
        title,
        rows,
        selected,
    } = &app.input_mode
    {
        render_build_times_popup(frame, title, rows, *selected);
    }

    // Overlay the auto-discover rules popup if active.
    if let InputMode::AutoDiscoverRules { rules, selected } = &app.input_mode {
        render_auto_discover_rules_popup(frame, rules, *selected);
    }

    // Overlay the help popup if active.
    if app.show_help && matches!(app.input_mode, InputMode::Normal) {
        render_help_popup(frame);
    }
}
