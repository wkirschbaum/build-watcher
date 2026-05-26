use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::Mutex;

use crate::dirs::state_dir;
use crate::github::LastBuild;
use crate::persistence::load_json;
use crate::watcher::WatchKey;

pub const MAX_HISTORY: usize = 20;

pub type BuildHistory = HashMap<WatchKey, Vec<LastBuild>>;
pub type SharedHistory = Arc<Mutex<BuildHistory>>;

pub fn load_history() -> BuildHistory {
    load_json(&state_dir().join("history.json")).unwrap_or_default()
}

/// Prepend `build` to the history for `key`, trimming to MAX_HISTORY.
/// Pure mutation on the in-memory map — does not persist.
pub fn push_build(history: &mut BuildHistory, key: &WatchKey, build: LastBuild) {
    let v = history.entry(key.clone()).or_default();
    v.insert(0, build);
    v.truncate(MAX_HISTORY);
}

/// Returns `(branch, LastBuild)` pairs for `repo`, optionally filtered by `branch`,
/// sorted newest-first by `completed_at`, limited to `limit` entries.
///
/// Entries with `completed_at = None` (abandoned/in-progress builds) sort last
/// because `Reverse(None) < Reverse(Some(_))` under `Ord for Option`.
pub fn history_for(
    history: &BuildHistory,
    repo: &str,
    branch: Option<&str>,
    limit: usize,
) -> Vec<(String, LastBuild)> {
    let mut entries: Vec<(String, LastBuild)> = history
        .iter()
        .filter(|(key, _)| key.matches_repo(repo) && branch.is_none_or(|b| key.branch == b))
        .flat_map(|(key, builds)| builds.iter().map(move |b| (key.branch.clone(), b.clone())))
        .collect();

    entries.sort_by_key(|b| std::cmp::Reverse(b.1.completed_at));
    entries.truncate(limit);
    entries
}

/// Returns all builds across all repos/branches, sorted newest-first, limited to `limit`.
pub fn history_all(history: &BuildHistory, limit: usize) -> Vec<(String, String, LastBuild)> {
    let mut entries: Vec<(String, String, LastBuild)> = history
        .iter()
        .flat_map(|(key, builds)| {
            builds
                .iter()
                .map(move |b| (key.repo.clone(), key.branch.clone(), b.clone()))
        })
        .collect();

    entries.sort_by_key(|b| std::cmp::Reverse(b.2.completed_at));
    entries.truncate(limit);
    entries
}

/// True when history contains a prior failed build for the same workflow on
/// the same commit. Used to flag a Success as a recovered flake.
///
/// A flake is defined as: same `WatchKey`, same `workflow`, same `head_sha`,
/// where any earlier entry has a failure-class conclusion.
pub fn is_flake(history: &BuildHistory, key: &WatchKey, workflow: &str, head_sha: &str) -> bool {
    use crate::github::RunConclusion;
    if head_sha.is_empty() {
        return false;
    }
    let Some(builds) = history.get(key) else {
        return false;
    };
    builds.iter().any(|b| {
        b.workflow == workflow
            && b.head_sha == head_sha
            && matches!(
                b.conclusion,
                RunConclusion::Failure | RunConclusion::TimedOut | RunConclusion::StartupFailure
            )
    })
}

/// Minimum number of qualifying samples required before `avg_duration` returns
/// a value. A single sample isn't an average, and small samples produce noisy
/// trends; require at least 2 successful builds.
const AVG_DURATION_MIN_SAMPLES: usize = 2;

/// Rolling average duration (seconds) across **successful** completed builds
/// with a recorded `duration_secs` for the given `(key, workflow)`.
///
/// Only Success builds are counted — Cancelled builds typically run for
/// seconds and Failures often abort partway, so mixing them in produces an
/// average that doesn't reflect actual workflow duration.
///
/// Returns `None` when there are fewer than `AVG_DURATION_MIN_SAMPLES`
/// qualifying builds.
pub fn avg_duration(history: &BuildHistory, key: &WatchKey, workflow: &str) -> Option<u64> {
    use crate::github::RunConclusion;
    let builds = history.get(key)?;
    let durations: Vec<u64> = builds
        .iter()
        .filter(|b| b.workflow == workflow && b.conclusion == RunConclusion::Success)
        .filter_map(|b| b.duration_secs)
        .collect();
    if durations.len() < AVG_DURATION_MIN_SAMPLES {
        return None;
    }
    let sum: u64 = durations.iter().sum();
    Some(sum / durations.len() as u64)
}

/// Return a copy of `history` with each key pruned to at most MAX_HISTORY entries.
pub fn pruned(history: &BuildHistory) -> BuildHistory {
    history
        .iter()
        .map(|(k, v)| (k.clone(), v.iter().take(MAX_HISTORY).cloned().collect()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_build(run_id: u64, completed_at: Option<u64>) -> LastBuild {
        LastBuild {
            run_id,
            conclusion: crate::github::RunConclusion::Success,
            workflow: "CI".to_string(),
            title: "test".to_string(),
            head_sha: String::new(),
            event: "push".to_string(),
            failing_steps: None,
            failing_job_id: None,
            completed_at,
            duration_secs: None,
            attempt: 1,
            url: String::new(),
            actor: None,
            commit_author: None,
            flaky: false,
        }
    }

    fn make_key(repo: &str, branch: &str) -> WatchKey {
        WatchKey::new(repo, branch)
    }

    #[test]
    fn push_build_prepends_and_trims() {
        let mut hist = BuildHistory::new();
        let key = make_key("alice/app", "main");

        for i in 0..=(MAX_HISTORY as u64) {
            push_build(&mut hist, &key, make_build(i, Some(i)));
        }

        let v = hist.get(&key).unwrap();
        assert_eq!(v.len(), MAX_HISTORY);
        // Newest (highest run_id) should be at index 0
        assert_eq!(v[0].run_id, MAX_HISTORY as u64);
        assert_eq!(v[MAX_HISTORY - 1].run_id, 1);
    }

    #[test]
    fn history_for_branch_filter() {
        let mut hist = BuildHistory::new();
        let main_key = make_key("alice/app", "main");
        let dev_key = make_key("alice/app", "develop");

        push_build(&mut hist, &main_key, make_build(1, Some(100)));
        push_build(&mut hist, &dev_key, make_build(2, Some(200)));

        let results = history_for(&hist, "alice/app", Some("main"), 10);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, "main");
        assert_eq!(results[0].1.run_id, 1);
    }

    #[test]
    fn history_for_cross_branch_sorted_newest_first() {
        let mut hist = BuildHistory::new();
        let main_key = make_key("alice/app", "main");
        let dev_key = make_key("alice/app", "develop");

        push_build(&mut hist, &main_key, make_build(1, Some(100)));
        push_build(&mut hist, &dev_key, make_build(2, Some(200)));

        let results = history_for(&hist, "alice/app", None, 10);
        assert_eq!(results.len(), 2);
        // newest first (completed_at 200 > 100)
        assert_eq!(results[0].1.run_id, 2);
        assert_eq!(results[1].1.run_id, 1);
    }

    #[test]
    fn history_for_respects_limit() {
        let mut hist = BuildHistory::new();
        let key = make_key("alice/app", "main");
        for i in 0..10u64 {
            push_build(&mut hist, &key, make_build(i, Some(i)));
        }

        let results = history_for(&hist, "alice/app", None, 3);
        assert_eq!(results.len(), 3);
    }

    #[test]
    fn history_for_different_repo_excluded() {
        let mut hist = BuildHistory::new();
        push_build(
            &mut hist,
            &make_key("alice/app", "main"),
            make_build(1, Some(100)),
        );
        push_build(
            &mut hist,
            &make_key("bob/other", "main"),
            make_build(2, Some(200)),
        );

        let results = history_for(&hist, "alice/app", None, 10);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].1.run_id, 1);
    }

    #[test]
    fn history_for_none_completed_at_sorts_last() {
        let mut hist = BuildHistory::new();
        let key = make_key("alice/app", "main");

        // One build with a timestamp, one abandoned (None).
        push_build(&mut hist, &key, make_build(1, Some(100)));
        push_build(&mut hist, &key, make_build(2, None));

        let results = history_for(&hist, "alice/app", None, 10);
        assert_eq!(results.len(), 2);
        // Timestamped build should come first.
        assert_eq!(
            results[0].1.run_id, 1,
            "timestamped build should sort first"
        );
        assert_eq!(results[1].1.run_id, 2, "None completed_at should sort last");
    }

    fn build_with(
        run_id: u64,
        workflow: &str,
        head_sha: &str,
        conclusion: crate::github::RunConclusion,
        duration_secs: Option<u64>,
    ) -> LastBuild {
        LastBuild {
            run_id,
            conclusion,
            workflow: workflow.to_string(),
            title: "test".to_string(),
            head_sha: head_sha.to_string(),
            event: "push".to_string(),
            failing_steps: None,
            failing_job_id: None,
            completed_at: Some(run_id),
            duration_secs,
            attempt: 1,
            url: String::new(),
            actor: None,
            commit_author: None,
            flaky: false,
        }
    }

    #[test]
    fn is_flake_detects_prior_failure_on_same_sha() {
        use crate::github::RunConclusion;
        let mut hist = BuildHistory::new();
        let key = make_key("alice/app", "main");
        push_build(
            &mut hist,
            &key,
            build_with(1, "CI", "deadbee", RunConclusion::Failure, Some(100)),
        );
        assert!(is_flake(&hist, &key, "CI", "deadbee"));
    }

    #[test]
    fn is_flake_false_on_different_sha() {
        use crate::github::RunConclusion;
        let mut hist = BuildHistory::new();
        let key = make_key("alice/app", "main");
        push_build(
            &mut hist,
            &key,
            build_with(1, "CI", "deadbee", RunConclusion::Failure, Some(100)),
        );
        assert!(!is_flake(&hist, &key, "CI", "cafefee"));
    }

    #[test]
    fn is_flake_false_on_different_workflow() {
        use crate::github::RunConclusion;
        let mut hist = BuildHistory::new();
        let key = make_key("alice/app", "main");
        push_build(
            &mut hist,
            &key,
            build_with(1, "CI", "deadbee", RunConclusion::Failure, Some(100)),
        );
        assert!(!is_flake(&hist, &key, "Deploy", "deadbee"));
    }

    #[test]
    fn is_flake_false_when_prior_was_cancelled_not_failure() {
        use crate::github::RunConclusion;
        let mut hist = BuildHistory::new();
        let key = make_key("alice/app", "main");
        push_build(
            &mut hist,
            &key,
            build_with(1, "CI", "deadbee", RunConclusion::Cancelled, Some(100)),
        );
        assert!(!is_flake(&hist, &key, "CI", "deadbee"));
    }

    #[test]
    fn is_flake_false_for_empty_sha() {
        use crate::github::RunConclusion;
        let mut hist = BuildHistory::new();
        let key = make_key("alice/app", "main");
        push_build(
            &mut hist,
            &key,
            build_with(1, "CI", "", RunConclusion::Failure, Some(100)),
        );
        assert!(!is_flake(&hist, &key, "CI", ""));
    }

    #[test]
    fn avg_duration_averages_successful_builds_only() {
        use crate::github::RunConclusion;
        let mut hist = BuildHistory::new();
        let key = make_key("alice/app", "main");
        // Two successful 100s + 200s = avg 150.
        // The 5s Cancelled would skew badly if counted: (100+200+5)/3 = 101.
        push_build(
            &mut hist,
            &key,
            build_with(1, "CI", "a", RunConclusion::Success, Some(100)),
        );
        push_build(
            &mut hist,
            &key,
            build_with(2, "CI", "b", RunConclusion::Success, Some(200)),
        );
        push_build(
            &mut hist,
            &key,
            build_with(3, "CI", "c", RunConclusion::Cancelled, Some(5)),
        );
        push_build(
            &mut hist,
            &key,
            build_with(4, "CI", "d", RunConclusion::Failure, Some(30)),
        );
        assert_eq!(avg_duration(&hist, &key, "CI"), Some(150));
    }

    #[test]
    fn avg_duration_requires_min_samples() {
        use crate::github::RunConclusion;
        let mut hist = BuildHistory::new();
        let key = make_key("alice/app", "main");
        // Single success is not enough for a meaningful "average".
        push_build(
            &mut hist,
            &key,
            build_with(1, "CI", "a", RunConclusion::Success, Some(100)),
        );
        assert_eq!(
            avg_duration(&hist, &key, "CI"),
            None,
            "single sample is not an average"
        );
        push_build(
            &mut hist,
            &key,
            build_with(2, "CI", "b", RunConclusion::Success, Some(200)),
        );
        assert_eq!(avg_duration(&hist, &key, "CI"), Some(150));
    }

    #[test]
    fn avg_duration_ignores_builds_with_no_duration() {
        use crate::github::RunConclusion;
        let mut hist = BuildHistory::new();
        let key = make_key("alice/app", "main");
        push_build(
            &mut hist,
            &key,
            build_with(1, "CI", "a", RunConclusion::Success, Some(100)),
        );
        push_build(
            &mut hist,
            &key,
            build_with(2, "CI", "b", RunConclusion::Success, Some(200)),
        );
        push_build(
            &mut hist,
            &key,
            build_with(3, "CI", "c", RunConclusion::Success, None),
        );
        assert_eq!(avg_duration(&hist, &key, "CI"), Some(150));
    }

    #[test]
    fn avg_duration_none_for_unknown_workflow() {
        let hist = BuildHistory::new();
        let key = make_key("alice/app", "main");
        assert_eq!(avg_duration(&hist, &key, "CI"), None);
    }

    #[test]
    fn avg_duration_none_when_only_failures() {
        use crate::github::RunConclusion;
        let mut hist = BuildHistory::new();
        let key = make_key("alice/app", "main");
        push_build(
            &mut hist,
            &key,
            build_with(1, "CI", "a", RunConclusion::Failure, Some(100)),
        );
        push_build(
            &mut hist,
            &key,
            build_with(2, "CI", "b", RunConclusion::Failure, Some(200)),
        );
        assert_eq!(
            avg_duration(&hist, &key, "CI"),
            None,
            "no successful samples means no average"
        );
    }

    #[test]
    fn pruned_caps_at_max_history() {
        let mut hist = BuildHistory::new();
        let key = make_key("alice/app", "main");
        let v = hist.entry(key.clone()).or_default();
        for i in 0..30u64 {
            v.push(make_build(i, Some(i)));
        }
        assert_eq!(v.len(), 30);

        let result = pruned(&hist);
        assert_eq!(result[&key].len(), MAX_HISTORY);
        // Preserves order (oldest entries kept since they were pushed, not prepended).
        assert_eq!(result[&key][0].run_id, 0);
    }
}
