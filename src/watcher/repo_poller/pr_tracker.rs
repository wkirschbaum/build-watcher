use std::collections::{HashMap, HashSet};

use crate::events::WatchEvent;
use crate::github::{MergeState, PrInfo};

use super::RepoPoller;

impl RepoPoller {
    /// Update PR display state and emit state-change events.
    /// Uses `cached_prs` when provided; falls back to an API call when `None`.
    /// No-ops when `watch_prs` is not enabled for this repo.
    pub(in crate::watcher) async fn poll_prs_with(&mut self, cached_prs: Option<Vec<PrInfo>>) {
        let watch_prs = {
            let cfg = self.config.read().await;
            cfg.repos.get(&self.repo).is_some_and(|rc| rc.watch_prs)
        };
        if !watch_prs {
            // Clear any previously populated PR display data so the TUI doesn't
            // show stale PRs after watch_prs is disabled.
            let mut w = self.watches.lock().await;
            for (key, entry) in w.iter_mut() {
                if key.repo == self.repo && !entry.prs.is_empty() {
                    entry.prs.clear();
                }
            }
            return;
        }

        let prs = match cached_prs {
            Some(p) => p,
            None => match self.github.open_prs(&self.repo).await {
                Ok(p) => p,
                Err(e) => {
                    tracing::debug!(repo = %self.repo, error = %e, "Failed to poll PRs");
                    return;
                }
            },
        };

        // Detect transitions and emit events.
        let current_ids: HashSet<u64> = prs.iter().map(|pr| pr.number).collect();
        for pr in &prs {
            // Unknown and HasHooks are transient states GitHub emits while recomputing
            // merge readiness (e.g. after a push). They don't produce notifications.
            // Skipping them prevents the oscillation Unknown → Blocked from looking
            // like a new Blocked transition and firing a duplicate notification.
            if matches!(pr.merge_state, MergeState::Unknown | MergeState::HasHooks) {
                continue;
            }
            let old = self.pr_states.get(&pr.number);
            if old.is_none_or(|prev| *prev != pr.merge_state) {
                if let Some(from) = old.cloned() {
                    self.events.emit(WatchEvent::PrStateChanged {
                        repo: self.repo.clone(),
                        branch: pr.branch.clone(),
                        target_branch: pr.target_branch.clone(),
                        number: pr.number,
                        title: pr.title.clone(),
                        url: pr.url.clone(),
                        author: pr.author.clone(),
                        draft: pr.draft,
                        from,
                        to: pr.merge_state.clone(),
                    });
                }
                self.pr_states.insert(pr.number, pr.merge_state.clone());
            }
        }
        // Remove closed PRs from state.
        self.pr_states.retain(|id, _| current_ids.contains(id));

        // Update watch entries with PR data for display.
        let mut w = self.watches.lock().await;
        let mut prs_by_target: HashMap<&str, Vec<&PrInfo>> = HashMap::new();
        for pr in &prs {
            prs_by_target
                .entry(pr.target_branch.as_str())
                .or_default()
                .push(pr);
        }
        for (key, entry) in w.iter_mut() {
            if key.repo == self.repo {
                entry.prs = prs_by_target
                    .get(key.branch.as_str())
                    .map(|prs| prs.iter().map(|pr| (*pr).clone()).collect())
                    .unwrap_or_default();
            }
        }
    }
}
