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

    entries.sort_by(|a, b| b.1.completed_at.cmp(&a.1.completed_at));
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

    entries.sort_by(|a, b| b.2.completed_at.cmp(&a.2.completed_at));
    entries.truncate(limit);
    entries
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
            conclusion: "success".to_string(),
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
