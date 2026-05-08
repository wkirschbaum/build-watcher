use std::collections::HashSet;

use crate::github::RunInfo;
use crate::watcher::{WatchEntry, WatchKey};

use super::RepoPoller;

impl RepoPoller {
    /// Sync watched branches based on recent runs: add newly discovered branches,
    /// remove stale branches that no longer have runs. Never removes branches that
    /// are explicitly configured for the repo.
    pub(in crate::watcher::repo_poller) async fn sync_branches(
        &self,
        all_runs: &[RunInfo],
        current: Vec<WatchKey>,
        cached_prs: Option<&[crate::github::PrInfo]>,
    ) -> Vec<WatchKey> {
        let (enabled, filter_re, pinned) = {
            let cfg = self.config.read().await;
            let enabled = cfg.auto_discover_for(&self.repo);
            let filter_re = cfg.branch_filter_for(&self.repo);
            // User-configured branches should never be auto-removed.
            let pinned: HashSet<String> = cfg
                .pinned_branches_for(&self.repo)
                .iter()
                .cloned()
                .collect();
            (enabled, filter_re, pinned)
        };
        if !enabled {
            return current;
        }

        // list_branches is the source of truth for which branches exist — if it
        // fails, skip sync entirely to avoid incorrectly removing non-pinned branches.
        let existing_branches: HashSet<String> = match self.github.list_branches(&self.repo).await {
            Ok(branches) => branches.into_iter().collect(),
            Err(e) => {
                tracing::debug!(
                    repo = %self.repo, error = %e,
                    "Failed to list branches, skipping branch sync"
                );
                return current;
            }
        };

        // Branches with open PRs should also be discoverable, even when their
        // runs have fallen outside the recent-runs window.
        // Use cached PR data fetched earlier in the cycle (no extra API call).
        let pr_branches: Vec<String> = cached_prs
            .unwrap_or_default()
            .iter()
            .map(|pr| pr.branch.clone())
            .collect();

        let active_branches: HashSet<&str> = all_runs
            .iter()
            .map(|r| r.head_branch.as_str())
            .chain(pr_branches.iter().map(|b| b.as_str()))
            .filter(|b| existing_branches.contains(*b))
            .filter(|b| filter_re.as_ref().is_none_or(|re| re.is_match(b)))
            .collect();

        let current_branches: HashSet<&str> = current.iter().map(|k| k.branch.as_str()).collect();

        // Add new branches.
        let to_add: Vec<String> = active_branches
            .iter()
            .filter(|b| !current_branches.contains(**b))
            .map(|b| b.to_string())
            .collect();

        // Remove discovered branches that no longer exist on GitHub (deleted).
        // Branches that still exist are kept even if quiet — they'll become active again.
        let to_remove: Vec<WatchKey> = current
            .iter()
            .filter(|k| !active_branches.contains(k.branch.as_str()))
            .filter(|k| !pinned.contains(&k.branch))
            .filter(|k| !existing_branches.contains(&k.branch))
            .cloned()
            .collect();

        if to_add.is_empty() && to_remove.is_empty() {
            return current;
        }

        if !to_add.is_empty() {
            tracing::info!(
                repo = %self.repo,
                branches = ?to_add,
                "Auto-discovered new branches"
            );
        }
        if !to_remove.is_empty() {
            let names: Vec<&str> = to_remove.iter().map(|k| k.branch.as_str()).collect();
            tracing::info!(
                repo = %self.repo,
                branches = ?names,
                "Removing stale discovered branches"
            );
        }

        // Persist discovered state. If save fails, skip the in-memory update so
        // watches and discovered stay in sync — the next cycle will retry.
        let remove_names: HashSet<String> = to_remove.iter().map(|k| k.branch.clone()).collect();
        let snapshot = {
            let mut disc = self.discovered.lock().await;
            let entry = disc.entry(self.repo.clone()).or_default();
            for branch in &to_add {
                if !entry.contains(branch) {
                    entry.push(branch.clone());
                }
            }
            entry.retain(|b| !remove_names.contains(b));
            disc.clone()
        };
        if let Err(e) = self.persistence.save_discovered(&snapshot).await {
            tracing::error!(error = %e, "Failed to persist branch discovery changes");
            // Undo the in-memory change so next cycle retries.
            let mut disc = self.discovered.lock().await;
            if let Some(entry) = disc.get_mut(&self.repo) {
                for branch in &to_add {
                    entry.retain(|b| b != branch);
                }
                for name in &remove_names {
                    entry.push(name.clone());
                }
            }
            return current;
        }

        // Config saved — apply the same changes to in-memory watches.
        {
            let mut w = self.watches.lock().await;
            for key in &to_remove {
                w.remove(key);
            }
            for branch in &to_add {
                let key = WatchKey::new(&self.repo, branch);
                w.entry(key).or_insert_with(|| WatchEntry {
                    waiting: true,
                    ..Default::default()
                });
            }
        }

        // Return the updated branch list.
        self.watched_branches().await
    }
}
