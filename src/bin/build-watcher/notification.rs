use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fmt::Write as _;
use std::time::{Duration, Instant};

use tokio::sync::broadcast;

use build_watcher::config::{self, NotificationLevel, SharedConfigManager};
use build_watcher::events::WatchEvent;
use build_watcher::format;
use build_watcher::github;
use build_watcher::status::RunConclusion;
use build_watcher::watcher::{PauseState, is_paused};

use crate::platform;

// -- Constants --

const DEBOUNCE_DELAY: Duration = Duration::from_secs(3);
const THROTTLE_WINDOW: Duration = Duration::from_secs(60);
const THROTTLE_MAX: usize = 10;

// -- Types --

/// Coarse event classification for debounce grouping.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum EventKind {
    Started,
    Succeeded,
    Failed,
}

impl EventKind {
    fn from_event(event: &WatchEvent) -> Option<Self> {
        match event {
            WatchEvent::RunStarted(_) => Some(Self::Started),
            WatchEvent::RunCompleted { conclusion, .. } => {
                if *conclusion == RunConclusion::Success {
                    Some(Self::Succeeded)
                } else {
                    Some(Self::Failed)
                }
            }
            WatchEvent::StatusChanged { .. } => None,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Started => "started",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
        }
    }

    fn emoji(self) -> &'static str {
        match self {
            Self::Started => "\u{1f528}",  // hammer
            Self::Succeeded => "\u{2705}", // check
            Self::Failed => "\u{274c}",    // cross
        }
    }
}

/// Grouping key for the debounce buffer.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct DebounceKey {
    repo: String,
    branch: String,
    kind: EventKind,
}

impl DebounceKey {
    fn from_event(event: &WatchEvent) -> Option<Self> {
        let kind = EventKind::from_event(event)?;
        match event {
            WatchEvent::RunStarted(run) | WatchEvent::RunCompleted { run, .. } => Some(Self {
                repo: run.repo.clone(),
                branch: run.branch.clone(),
                kind,
            }),
            WatchEvent::StatusChanged { .. } => None,
        }
    }
}

/// One notification waiting in the debounce buffer.
struct BufferedEvent {
    event: WatchEvent,
    repo_label: String,
    level: NotificationLevel,
}

/// Debounce buffer: holds events grouped by key until their deadline expires.
struct DebounceBuffer {
    pending: HashMap<DebounceKey, Vec<BufferedEvent>>,
    /// Deadlines ordered by time. Uses `(Instant, u64)` to guarantee uniqueness.
    deadlines: BTreeMap<(Instant, u64), DebounceKey>,
    next_id: u64,
}

impl DebounceBuffer {
    fn new() -> Self {
        Self {
            pending: HashMap::new(),
            deadlines: BTreeMap::new(),
            next_id: 0,
        }
    }

    fn insert(&mut self, key: DebounceKey, event: BufferedEvent, now: Instant) {
        let is_new = !self.pending.contains_key(&key);
        self.pending.entry(key.clone()).or_default().push(event);
        if is_new {
            let id = self.next_id;
            self.next_id += 1;
            self.deadlines.insert((now + DEBOUNCE_DELAY, id), key);
        }
    }

    fn next_deadline(&self) -> Option<Instant> {
        self.deadlines
            .first_key_value()
            .map(|((instant, _), _)| *instant)
    }

    /// Remove and return all expired groups at or before `now`.
    fn pop_expired(&mut self, now: Instant) -> Vec<(DebounceKey, Vec<BufferedEvent>)> {
        let mut result = Vec::new();
        while let Some((&(deadline, _), _)) = self.deadlines.first_key_value() {
            if deadline > now {
                break;
            }
            let (_, key) = self.deadlines.pop_first().unwrap();
            if let Some(events) = self.pending.remove(&key) {
                result.push((key, events));
            }
        }
        result
    }

    fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }
}

/// Sliding-window throttle tracking recent notification sends.
struct ThrottleWindow {
    timestamps: VecDeque<Instant>,
}

impl ThrottleWindow {
    fn new() -> Self {
        Self {
            timestamps: VecDeque::new(),
        }
    }

    /// Check whether a notification may be sent. Returns `true` if allowed.
    /// Critical notifications always pass but still consume budget.
    fn allows(&mut self, now: Instant, is_critical: bool) -> bool {
        // Drain entries older than the window.
        while self
            .timestamps
            .front()
            .is_some_and(|&t| now.duration_since(t) > THROTTLE_WINDOW)
        {
            self.timestamps.pop_front();
        }
        if is_critical {
            self.timestamps.push_back(now);
            return true;
        }
        if self.timestamps.len() < THROTTLE_MAX {
            self.timestamps.push_back(now);
            true
        } else {
            false
        }
    }
}

// -- Transition tracking --

/// Key for tracking per-workflow notification state.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct TransitionKey {
    repo: String,
    branch: String,
    workflow: String,
}

/// Tracks the last conclusion we notified about per (repo, branch, workflow).
/// Only fires a notification when the conclusion *changes* (or on first occurrence).
struct TransitionTracker {
    last: HashMap<TransitionKey, RunConclusion>,
}

impl TransitionTracker {
    fn new() -> Self {
        Self {
            last: HashMap::new(),
        }
    }

    /// Returns `true` if this event represents a status transition worth notifying about.
    fn should_notify(&self, event: &WatchEvent) -> bool {
        match event {
            // RunStarted: suppress — the user only wants transition notifications.
            WatchEvent::RunStarted(_) => false,
            WatchEvent::RunCompleted {
                run, conclusion, ..
            } => {
                let key = TransitionKey {
                    repo: run.repo.clone(),
                    branch: run.branch.clone(),
                    workflow: run.workflow.clone(),
                };
                match self.last.get(&key) {
                    Some(prev) => prev != conclusion,
                    None => true,
                }
            }
            WatchEvent::StatusChanged { .. } => false,
        }
    }

    /// Record that we notified about this event's conclusion.
    fn record(&mut self, event: &WatchEvent) {
        if let WatchEvent::RunCompleted {
            run, conclusion, ..
        } = event
        {
            let key = TransitionKey {
                repo: run.repo.clone(),
                branch: run.branch.clone(),
                workflow: run.workflow.clone(),
            };
            self.last.insert(key, conclusion.clone());
        }
    }
}

// -- Helpers --

/// Numeric rank for comparing notification levels (higher = more urgent).
fn level_rank(level: NotificationLevel) -> u8 {
    match level {
        NotificationLevel::Off => 0,
        NotificationLevel::Low => 1,
        NotificationLevel::Normal => 2,
        NotificationLevel::Critical => 3,
    }
}

/// Pick the highest urgency level from a slice.
fn max_level(levels: impl Iterator<Item = NotificationLevel>) -> NotificationLevel {
    levels
        .max_by_key(|l| level_rank(*l))
        .unwrap_or(NotificationLevel::Normal)
}

/// Extract the repo name from an event, if applicable.
fn event_repo(event: &WatchEvent) -> Option<&str> {
    match event {
        WatchEvent::RunStarted(run) => Some(&run.repo),
        WatchEvent::RunCompleted { run, .. } => Some(&run.repo),
        WatchEvent::StatusChanged { .. } => None,
    }
}

/// Determine the effective notification level for an event without sending it.
pub(crate) fn effective_level(event: &WatchEvent, cfg: &config::Config) -> NotificationLevel {
    match event {
        WatchEvent::RunStarted(run) => cfg.notifications_for(&run.repo, &run.branch).build_started,
        WatchEvent::RunCompleted {
            run, conclusion, ..
        } => {
            let notif = cfg.notifications_for(&run.repo, &run.branch);
            if *conclusion == RunConclusion::Success {
                notif.build_success
            } else {
                notif.build_failure
            }
        }
        WatchEvent::StatusChanged { .. } => NotificationLevel::Off,
    }
}

/// Check suppression (pause, quiet hours, level=Off) and return dispatch info if not suppressed.
async fn check_suppression(
    event: &WatchEvent,
    config: &SharedConfigManager,
    pause: &PauseState,
) -> Option<(String, NotificationLevel)> {
    let paused = is_paused(pause).await;
    let cfg = config.read().await;
    let level = effective_level(event, &cfg);
    let suppressed = level == NotificationLevel::Off
        || (level != NotificationLevel::Critical && (paused || cfg.is_in_quiet_hours()));
    if suppressed {
        None
    } else {
        let repo_label = event_repo(event)
            .map(|r| cfg.short_repo(r).to_string())
            .unwrap_or_default();
        Some((repo_label, level))
    }
}

// -- Coalescing --

/// Build a coalesced notification title for multiple events.
fn coalesced_title(kind: EventKind, repo_label: &str, branch: &str, count: usize) -> String {
    format!(
        "{} {} workflows {}: {} | {}",
        kind.emoji(),
        count,
        kind.label(),
        repo_label,
        branch,
    )
}

/// Build the body for a coalesced notification.
fn coalesced_body(kind: EventKind, events: &[BufferedEvent]) -> String {
    let workflows: Vec<&str> = events
        .iter()
        .filter_map(|e| match &e.event {
            WatchEvent::RunStarted(run) | WatchEvent::RunCompleted { run, .. } => {
                Some(run.workflow.as_str())
            }
            _ => None,
        })
        .collect();

    let mut body = workflows.join(", ");

    if kind == EventKind::Failed {
        for e in events {
            if let WatchEvent::RunCompleted {
                run,
                failing_steps: Some(steps),
                ..
            } = &e.event
            {
                let _ = write!(body, "\n{}: {steps}", run.workflow);
            }
        }
    }

    body
}

/// Dispatch a group of buffered events, coalescing if there are multiple.
async fn dispatch_coalesced(
    key: DebounceKey,
    events: Vec<BufferedEvent>,
    throttle: &mut ThrottleWindow,
) {
    if events.is_empty() {
        return;
    }

    // Single event: dispatch normally (unchanged behavior).
    if events.len() == 1 {
        let e = events.into_iter().next().expect("checked len == 1 above");
        let is_critical = e.level == NotificationLevel::Critical;
        if !throttle.allows(Instant::now(), is_critical) {
            tracing::warn!("Throttled notification for {} (budget exhausted)", key.repo);
            return;
        }
        handle_notification(e.event, &e.repo_label, e.level).await;
        return;
    }

    // Multiple events: coalesce into a summary.
    let level = max_level(events.iter().map(|e| e.level));
    let is_critical = level == NotificationLevel::Critical;
    if !throttle.allows(Instant::now(), is_critical) {
        tracing::warn!(
            "Throttled coalesced notification ({} events) for {} (budget exhausted)",
            events.len(),
            key.repo
        );
        return;
    }

    let repo_label = events
        .first()
        .map(|e| e.repo_label.as_str())
        .unwrap_or(&key.repo);
    let title = coalesced_title(key.kind, repo_label, &key.branch, events.len());
    let body = coalesced_body(key.kind, &events);
    let group = format!("{}#{}#{}", key.repo, key.branch, key.kind.label());
    let url = github::actions_url(&key.repo, &key.branch);

    platform::send(platform::Notification {
        title,
        body,
        level,
        url: Some(url),
        group,
        app_name: key.repo,
    })
    .await;
}

// -- Single-event dispatch (unchanged from original) --

async fn handle_notification(event: WatchEvent, repo_label: &str, level: NotificationLevel) {
    match event {
        WatchEvent::RunStarted(run) => {
            platform::send(platform::Notification {
                title: format!("\u{1f528} started: {} | {}", repo_label, run.workflow),
                body: format!("[{}] {}", run.branch, run.display_title()),
                level,
                url: Some(run.url()),
                group: run.notification_group(),
                app_name: run.repo,
            })
            .await;
        }
        WatchEvent::RunCompleted {
            run,
            conclusion,
            elapsed,
            failing_steps,
            ..
        } => {
            let succeeded = conclusion == RunConclusion::Success;

            let (emoji, status) = if succeeded {
                ("\u{2705}", "succeeded")
            } else {
                ("\u{274c}", "failed")
            };
            let mut body = format!("[{}] {}", run.branch, run.display_title());
            if let Some(secs) = elapsed {
                let _ = write!(
                    body,
                    " in {}",
                    format::duration(Duration::from_secs_f64(secs))
                );
            }
            if let Some(steps) = &failing_steps {
                let _ = write!(body, "\nFailed: {steps}");
            }

            platform::send(platform::Notification {
                title: format!("{emoji} {status}: {} | {}", repo_label, run.workflow),
                body,
                level,
                url: Some(run.url()),
                group: run.notification_group(),
                app_name: run.repo,
            })
            .await;
        }
        WatchEvent::StatusChanged { .. } => {}
    }
}

// -- Main handler --

/// Listens for watch events and dispatches desktop notifications
/// with debounce (3s per repo/branch/kind) and throttle (10/60s).
pub async fn run_notification_handler(
    mut rx: broadcast::Receiver<WatchEvent>,
    config: SharedConfigManager,
    pause: PauseState,
) {
    let mut buffer = DebounceBuffer::new();
    let mut throttle = ThrottleWindow::new();
    let mut transitions = TransitionTracker::new();

    loop {
        // Determine sleep target: next deadline, or far future if buffer is empty.
        let deadline = buffer
            .next_deadline()
            .unwrap_or_else(|| Instant::now() + Duration::from_secs(86400));

        tokio::select! {
            result = rx.recv() => {
                match result {
                    Ok(event) => {
                        if transitions.should_notify(&event)
                            && let Some((repo_label, level)) = check_suppression(&event, &config, &pause).await
                            && let Some(key) = DebounceKey::from_event(&event)
                        {
                            buffer.insert(
                                key,
                                BufferedEvent { event, repo_label, level },
                                Instant::now(),
                            );
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!("Notification handler dropped {n} events");
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        tracing::debug!("Event bus closed, notification handler exiting");
                        break;
                    }
                }
            }
            _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => {}
        }

        // Dispatch any expired groups after every iteration, so a continuous
        // stream of recv events cannot starve pending deadlines.
        let expired = buffer.pop_expired(Instant::now());
        for (key, events) in expired {
            for e in &events {
                transitions.record(&e.event);
            }
            dispatch_coalesced(key, events, &mut throttle).await;
        }
    }

    // Flush any remaining buffered events on shutdown.
    if !buffer.is_empty() {
        let expired = buffer.pop_expired(Instant::now() + DEBOUNCE_DELAY);
        for (key, events) in expired {
            for e in &events {
                transitions.record(&e.event);
            }
            dispatch_coalesced(key, events, &mut throttle).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use build_watcher::config::NotificationLevel::*;
    use build_watcher::events::RunSnapshot;
    use build_watcher::status::RunStatus;

    fn snap() -> RunSnapshot {
        RunSnapshot {
            repo: "alice/app".to_string(),
            branch: "main".to_string(),
            run_id: 12345,
            workflow: "CI".to_string(),
            title: "Fix login bug".to_string(),
            event: "push".to_string(),
            status: RunStatus::InProgress,
            attempt: 1,
        }
    }

    fn snap_workflow(name: &str) -> RunSnapshot {
        let mut s = snap();
        s.workflow = name.to_string();
        s
    }

    fn completed(conclusion: RunConclusion) -> WatchEvent {
        WatchEvent::RunCompleted {
            run: snap(),
            conclusion,
            elapsed: None,
            failing_steps: None,
            failing_job_id: None,
        }
    }

    // -- EventKind tests --

    #[test]
    fn event_kind_from_started() {
        let event = WatchEvent::RunStarted(snap());
        assert_eq!(EventKind::from_event(&event), Some(EventKind::Started));
    }

    #[test]
    fn event_kind_from_succeeded() {
        assert_eq!(
            EventKind::from_event(&completed(RunConclusion::Success)),
            Some(EventKind::Succeeded)
        );
    }

    #[test]
    fn event_kind_from_failed() {
        assert_eq!(
            EventKind::from_event(&completed(RunConclusion::Failure)),
            Some(EventKind::Failed)
        );
        assert_eq!(
            EventKind::from_event(&completed(RunConclusion::Cancelled)),
            Some(EventKind::Failed)
        );
    }

    #[test]
    fn event_kind_from_status_changed_is_none() {
        let event = WatchEvent::StatusChanged {
            run: snap(),
            from: RunStatus::Queued,
            to: RunStatus::InProgress,
        };
        assert_eq!(EventKind::from_event(&event), None);
    }

    // -- level_rank tests --

    #[test]
    fn level_rank_ordering() {
        assert!(level_rank(Off) < level_rank(Low));
        assert!(level_rank(Low) < level_rank(Normal));
        assert!(level_rank(Normal) < level_rank(Critical));
    }

    #[test]
    fn max_level_picks_highest() {
        assert_eq!(max_level([Low, Normal, Critical].into_iter()), Critical);
        assert_eq!(max_level([Low, Normal].into_iter()), Normal);
        assert_eq!(max_level([Low].into_iter()), Low);
    }

    // -- DebounceBuffer tests --

    #[test]
    fn buffer_insert_same_key_groups() {
        let mut buf = DebounceBuffer::new();
        let now = Instant::now();
        let key = DebounceKey {
            repo: "alice/app".into(),
            branch: "main".into(),
            kind: EventKind::Started,
        };
        buf.insert(
            key.clone(),
            BufferedEvent {
                event: WatchEvent::RunStarted(snap_workflow("CI")),
                repo_label: "app".into(),
                level: Normal,
            },
            now,
        );
        buf.insert(
            key.clone(),
            BufferedEvent {
                event: WatchEvent::RunStarted(snap_workflow("Lint")),
                repo_label: "app".into(),
                level: Normal,
            },
            now,
        );

        assert_eq!(buf.pending.len(), 1);
        assert_eq!(buf.pending[&key].len(), 2);
        assert_eq!(buf.deadlines.len(), 1);
    }

    #[test]
    fn buffer_different_keys_separate() {
        let mut buf = DebounceBuffer::new();
        let now = Instant::now();
        let key1 = DebounceKey {
            repo: "alice/app".into(),
            branch: "main".into(),
            kind: EventKind::Started,
        };
        let key2 = DebounceKey {
            repo: "alice/app".into(),
            branch: "main".into(),
            kind: EventKind::Succeeded,
        };
        buf.insert(
            key1,
            BufferedEvent {
                event: WatchEvent::RunStarted(snap()),
                repo_label: "app".into(),
                level: Normal,
            },
            now,
        );
        buf.insert(
            key2,
            BufferedEvent {
                event: completed(RunConclusion::Success),
                repo_label: "app".into(),
                level: Normal,
            },
            now,
        );

        assert_eq!(buf.pending.len(), 2);
        assert_eq!(buf.deadlines.len(), 2);
    }

    #[test]
    fn buffer_pop_expired_respects_deadline() {
        let mut buf = DebounceBuffer::new();
        let now = Instant::now();
        let key = DebounceKey {
            repo: "alice/app".into(),
            branch: "main".into(),
            kind: EventKind::Started,
        };
        buf.insert(
            key,
            BufferedEvent {
                event: WatchEvent::RunStarted(snap()),
                repo_label: "app".into(),
                level: Normal,
            },
            now,
        );

        // Before deadline: nothing expired.
        let expired = buf.pop_expired(now + Duration::from_secs(1));
        assert!(expired.is_empty());
        assert_eq!(buf.pending.len(), 1);

        // After deadline: pops the group.
        let expired = buf.pop_expired(now + DEBOUNCE_DELAY + Duration::from_millis(1));
        assert_eq!(expired.len(), 1);
        assert!(buf.pending.is_empty());
        assert!(buf.deadlines.is_empty());
    }

    // -- ThrottleWindow tests --

    #[test]
    fn throttle_allows_up_to_max() {
        let mut tw = ThrottleWindow::new();
        let now = Instant::now();
        for _ in 0..THROTTLE_MAX {
            assert!(tw.allows(now, false));
        }
        assert!(!tw.allows(now, false));
    }

    #[test]
    fn throttle_critical_always_allowed() {
        let mut tw = ThrottleWindow::new();
        let now = Instant::now();
        // Exhaust budget.
        for _ in 0..THROTTLE_MAX {
            tw.allows(now, false);
        }
        // Critical still passes.
        assert!(tw.allows(now, true));
    }

    #[test]
    fn throttle_drains_old_entries() {
        let mut tw = ThrottleWindow::new();
        let start = Instant::now();
        // Fill budget.
        for _ in 0..THROTTLE_MAX {
            tw.allows(start, false);
        }
        assert!(!tw.allows(start, false));

        // After the window expires, budget is available again.
        let later = start + THROTTLE_WINDOW + Duration::from_millis(1);
        assert!(tw.allows(later, false));
    }

    // -- Coalescing format tests --

    #[test]
    fn coalesced_title_format() {
        let title = coalesced_title(EventKind::Started, "app", "main", 5);
        assert_eq!(title, "\u{1f528} 5 workflows started: app | main");

        let title = coalesced_title(EventKind::Succeeded, "app", "main", 3);
        assert_eq!(title, "\u{2705} 3 workflows succeeded: app | main");

        let title = coalesced_title(EventKind::Failed, "app", "main", 2);
        assert_eq!(title, "\u{274c} 2 workflows failed: app | main");
    }

    #[test]
    fn coalesced_body_lists_workflows() {
        let events = vec![
            BufferedEvent {
                event: WatchEvent::RunStarted(snap_workflow("CI")),
                repo_label: "app".into(),
                level: Normal,
            },
            BufferedEvent {
                event: WatchEvent::RunStarted(snap_workflow("Lint")),
                repo_label: "app".into(),
                level: Normal,
            },
            BufferedEvent {
                event: WatchEvent::RunStarted(snap_workflow("Deploy")),
                repo_label: "app".into(),
                level: Normal,
            },
        ];
        let body = coalesced_body(EventKind::Started, &events);
        assert_eq!(body, "CI, Lint, Deploy");
    }

    #[test]
    fn coalesced_body_includes_failing_steps() {
        let events = vec![
            BufferedEvent {
                event: WatchEvent::RunCompleted {
                    run: snap_workflow("CI"),
                    conclusion: RunConclusion::Failure,
                    elapsed: None,
                    failing_steps: Some("Build / Run tests".into()),
                    failing_job_id: None,
                },
                repo_label: "app".into(),
                level: Critical,
            },
            BufferedEvent {
                event: WatchEvent::RunCompleted {
                    run: snap_workflow("Deploy"),
                    conclusion: RunConclusion::Failure,
                    elapsed: None,
                    failing_steps: None,
                    failing_job_id: None,
                },
                repo_label: "app".into(),
                level: Critical,
            },
        ];
        let body = coalesced_body(EventKind::Failed, &events);
        assert_eq!(body, "CI, Deploy\nCI: Build / Run tests");
    }

    // -- effective_level tests (preserved from original) --

    #[test]
    fn effective_level_by_event_type() {
        let cfg = config::Config::default();

        assert_eq!(
            effective_level(&WatchEvent::RunStarted(snap()), &cfg),
            Normal
        );
        assert_eq!(
            effective_level(&completed(RunConclusion::Success), &cfg),
            Normal
        );
        assert_eq!(
            effective_level(&completed(RunConclusion::Failure), &cfg),
            Critical
        );

        let status = WatchEvent::StatusChanged {
            run: snap(),
            from: RunStatus::Queued,
            to: RunStatus::InProgress,
        };
        assert_eq!(effective_level(&status, &cfg), Off);
    }

    // -- TransitionTracker tests --

    #[test]
    fn transition_suppresses_run_started() {
        let tracker = TransitionTracker::new();
        let event = WatchEvent::RunStarted(snap());
        assert!(!tracker.should_notify(&event));
    }

    #[test]
    fn transition_allows_first_completion() {
        let tracker = TransitionTracker::new();
        let event = completed(RunConclusion::Success);
        assert!(tracker.should_notify(&event));
    }

    #[test]
    fn transition_suppresses_same_conclusion() {
        let mut tracker = TransitionTracker::new();
        let event = completed(RunConclusion::Success);
        assert!(tracker.should_notify(&event));
        tracker.record(&event);
        // Same conclusion again — suppress.
        let event2 = completed(RunConclusion::Success);
        assert!(!tracker.should_notify(&event2));
    }

    #[test]
    fn transition_allows_changed_conclusion() {
        let mut tracker = TransitionTracker::new();
        let success = completed(RunConclusion::Success);
        tracker.record(&success);
        // Different conclusion — notify.
        let failure = completed(RunConclusion::Failure);
        assert!(tracker.should_notify(&failure));
    }

    #[test]
    fn transition_tracks_per_workflow() {
        let mut tracker = TransitionTracker::new();
        let ci_success = completed(RunConclusion::Success);
        tracker.record(&ci_success);

        // Different workflow — should notify even with same conclusion.
        let mut lint_snap = snap();
        lint_snap.workflow = "Lint".to_string();
        let lint_success = WatchEvent::RunCompleted {
            run: lint_snap,
            conclusion: RunConclusion::Success,
            elapsed: None,
            failing_steps: None,
            failing_job_id: None,
        };
        assert!(tracker.should_notify(&lint_success));
    }
}
