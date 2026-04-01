use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fmt::Write as _;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::broadcast;

use build_watcher::config::{self, NotificationLevel, SharedConfigManager};
use build_watcher::events::WatchEvent;
use build_watcher::format;
use build_watcher::github;
use build_watcher::status::RunConclusion;
use build_watcher::watcher::{PauseState, is_paused};

use crate::platform::{Notification, Notifier};

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
            WatchEvent::StatusChanged { .. } | WatchEvent::PrStateChanged { .. } => None,
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
            WatchEvent::StatusChanged { .. } | WatchEvent::PrStateChanged { .. } => None,
        }
    }
}

/// One notification waiting in the debounce buffer.
struct BufferedEvent {
    event: WatchEvent,
    repo_label: String,
    level: NotificationLevel,
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
        WatchEvent::StatusChanged { .. } | WatchEvent::PrStateChanged { .. } => None,
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
        WatchEvent::StatusChanged { .. } | WatchEvent::PrStateChanged { .. } => {
            NotificationLevel::Off
        }
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

// -- NotificationPipeline --

/// Owns all notification state: transition tracking, debounce buffer, and throttle window.
/// Testable without channels, timers, or spawned tasks.
struct NotificationPipeline {
    transitions: HashMap<(String, String), EventKind>,
    /// Debounce buffer: events grouped by key, with deadlines.
    pending: HashMap<DebounceKey, Vec<BufferedEvent>>,
    deadlines: BTreeMap<(Instant, u64), DebounceKey>,
    next_id: u64,
    /// Sliding-window throttle.
    throttle_timestamps: VecDeque<Instant>,
}

impl NotificationPipeline {
    fn new() -> Self {
        Self {
            transitions: HashMap::new(),
            pending: HashMap::new(),
            deadlines: BTreeMap::new(),
            next_id: 0,
            throttle_timestamps: VecDeque::new(),
        }
    }

    /// Check whether this event represents a branch-level transition worth notifying about.
    fn is_transition(&self, event: &WatchEvent) -> bool {
        let (run, kind) = match event {
            WatchEvent::RunStarted(run) => (run, EventKind::Started),
            WatchEvent::RunCompleted {
                run, conclusion, ..
            } => (
                run,
                if *conclusion == RunConclusion::Success {
                    EventKind::Succeeded
                } else {
                    EventKind::Failed
                },
            ),
            WatchEvent::StatusChanged { .. } | WatchEvent::PrStateChanged { .. } => return false,
        };
        let key = (run.repo.clone(), run.branch.clone());
        self.transitions.get(&key).is_none_or(|prev| *prev != kind)
    }

    /// Ingest an event: check transition + suppression, buffer if appropriate.
    async fn ingest(
        &mut self,
        event: WatchEvent,
        config: &SharedConfigManager,
        pause: &PauseState,
        now: Instant,
    ) {
        if !self.is_transition(&event) {
            return;
        }

        let paused = is_paused(pause).await;
        let cfg = config.read().await;
        let level = effective_level(&event, &cfg);
        let suppressed = level == NotificationLevel::Off
            || (level != NotificationLevel::Critical && (paused || cfg.is_in_quiet_hours()));
        if suppressed {
            return;
        }

        let repo_label = event_repo(&event)
            .map(|r| cfg.short_repo(r).to_string())
            .unwrap_or_default();
        drop(cfg);

        let Some(key) = DebounceKey::from_event(&event) else {
            return;
        };

        // Record transition.
        if let Some(kind) = EventKind::from_event(&event) {
            let tk = match &event {
                WatchEvent::RunStarted(run) | WatchEvent::RunCompleted { run, .. } => {
                    (run.repo.clone(), run.branch.clone())
                }
                _ => unreachable!(),
            };
            self.transitions.insert(tk, kind);
        }

        // Buffer for debounce.
        let is_new = !self.pending.contains_key(&key);
        self.pending
            .entry(key.clone())
            .or_default()
            .push(BufferedEvent {
                event,
                repo_label,
                level,
            });
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

    fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    /// Dispatch all expired debounce groups via the notifier.
    async fn dispatch_expired(&mut self, now: Instant, notifier: &dyn Notifier) {
        while let Some(&(deadline, _)) = self.deadlines.first_key_value().map(|(k, _)| k) {
            if deadline > now {
                break;
            }
            let (_, key) = self
                .deadlines
                .pop_first()
                .expect("already verified non-empty");
            if let Some(events) = self.pending.remove(&key) {
                self.dispatch_group(key, events, notifier).await;
            }
        }
    }

    /// Dispatch a single debounce group.
    async fn dispatch_group(
        &mut self,
        key: DebounceKey,
        events: Vec<BufferedEvent>,
        notifier: &dyn Notifier,
    ) {
        if events.is_empty() {
            return;
        }

        let (level, is_single) = if events.len() == 1 {
            (events[0].level, true)
        } else {
            (max_level(events.iter().map(|e| e.level)), false)
        };
        let is_critical = level == NotificationLevel::Critical;

        if !self.throttle_allows(Instant::now(), is_critical) {
            tracing::warn!("Throttled notification for {} (budget exhausted)", key.repo);
            return;
        }

        if is_single {
            let e = events
                .into_iter()
                .next()
                .expect("is_single guarantees non-empty");
            dispatch_single(e.event, &e.repo_label, e.level, notifier).await;
        } else {
            let repo_label = events
                .first()
                .map(|e| e.repo_label.as_str())
                .unwrap_or(&key.repo);
            let title = coalesced_title(key.kind, repo_label, &key.branch, events.len());
            let body = coalesced_body(key.kind, &events);
            let group = format!("{}#{}#{}", key.repo, key.branch, key.kind.label());
            let url = github::actions_url(&key.repo, &key.branch);

            notifier
                .send(&Notification {
                    title,
                    body,
                    level,
                    url: Some(url),
                    group,
                    app_name: key.repo,
                })
                .await;
        }
    }

    fn throttle_allows(&mut self, now: Instant, is_critical: bool) -> bool {
        while self
            .throttle_timestamps
            .front()
            .is_some_and(|&t| now.duration_since(t) > THROTTLE_WINDOW)
        {
            self.throttle_timestamps.pop_front();
        }
        if is_critical {
            self.throttle_timestamps.push_back(now);
            return true;
        }
        if self.throttle_timestamps.len() < THROTTLE_MAX {
            self.throttle_timestamps.push_back(now);
            true
        } else {
            false
        }
    }
}

/// Format and send a single-event notification.
async fn dispatch_single(
    event: WatchEvent,
    repo_label: &str,
    level: NotificationLevel,
    notifier: &dyn Notifier,
) {
    match event {
        WatchEvent::RunStarted(run) => {
            notifier
                .send(&Notification {
                    title: format!("\u{1f528} started: {} | {}", repo_label, run.workflow),
                    body: format!("[{}] {}", run.branch, run.display_title()),
                    level,
                    url: Some(run.url.clone()),
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

            notifier
                .send(&Notification {
                    title: format!("{emoji} {status}: {} | {}", repo_label, run.workflow),
                    body,
                    level,
                    url: Some(run.url.clone()),
                    group: run.notification_group(),
                    app_name: run.repo,
                })
                .await;
        }
        WatchEvent::StatusChanged { .. } | WatchEvent::PrStateChanged { .. } => {}
    }
}

// -- PR notification --

/// Build and send a desktop notification for a PR state change.
/// Returns immediately if paused, in quiet hours, or the state is uninteresting.
async fn dispatch_pr_notification(
    event: &WatchEvent,
    config: &SharedConfigManager,
    pause: &PauseState,
    notifier: &dyn Notifier,
) {
    let WatchEvent::PrStateChanged {
        repo,
        number,
        title,
        url,
        to,
        ..
    } = event
    else {
        return;
    };

    if is_paused(pause).await {
        return;
    }
    let cfg = config.read().await;
    if cfg.is_in_quiet_hours() {
        return;
    }
    let repo_label = cfg.short_repo(repo).to_string();
    drop(cfg);

    let (emoji, label) = match to {
        github::MergeState::Clean => ("\u{2705}", "ready to merge"),
        github::MergeState::Blocked => ("\u{1f6d1}", "blocked"),
        github::MergeState::Unstable => ("\u{26a0}\u{fe0f}", "unstable"),
        github::MergeState::Behind => ("\u{2b07}\u{fe0f}", "behind"),
        github::MergeState::Dirty => ("\u{274c}", "has conflicts"),
        _ => return,
    };

    notifier
        .send(&Notification {
            title: format!("{emoji} PR #{number} {label}: {repo_label}"),
            body: title.to_string(),
            level: if *to == github::MergeState::Clean {
                NotificationLevel::Normal
            } else {
                NotificationLevel::Low
            },
            url: Some(url.to_string()),
            group: format!("{repo}#pr#{number}"),
            app_name: repo.to_string(),
        })
        .await;
}

// -- Main handler --

/// Listens for watch events and dispatches desktop notifications
/// with debounce (3s per repo/branch/kind) and throttle (10/60s).
pub async fn run_notification_handler(
    mut rx: broadcast::Receiver<WatchEvent>,
    config: SharedConfigManager,
    pause: PauseState,
    notifier: Arc<dyn Notifier>,
) {
    let mut pipeline = NotificationPipeline::new();

    loop {
        let deadline = pipeline
            .next_deadline()
            .unwrap_or_else(|| Instant::now() + Duration::from_secs(86400));

        tokio::select! {
            result = rx.recv() => {
                match result {
                    Ok(event) => {
                        if matches!(event, WatchEvent::PrStateChanged { .. }) {
                            dispatch_pr_notification(&event, &config, &pause, &*notifier).await;
                        } else {
                            pipeline.ingest(event, &config, &pause, Instant::now()).await;
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

        pipeline.dispatch_expired(Instant::now(), &*notifier).await;
    }

    // Flush remaining on shutdown.
    if !pipeline.is_empty() {
        pipeline
            .dispatch_expired(Instant::now() + DEBOUNCE_DELAY, &*notifier)
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use build_watcher::config::NotificationLevel::*;
    use build_watcher::events::RunSnapshot;
    use build_watcher::status::RunStatus;
    use std::pin::Pin;
    use tokio::sync::Mutex;

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
            url: "https://github.com/alice/app/actions/runs/12345".to_string(),
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

    // -- Recording notifier --

    struct RecordingNotifier {
        sent: Mutex<Vec<String>>,
    }

    impl RecordingNotifier {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                sent: Mutex::new(Vec::new()),
            })
        }

        async fn titles(&self) -> Vec<String> {
            self.sent.lock().await.clone()
        }
    }

    impl Notifier for RecordingNotifier {
        fn name(&self) -> &'static str {
            "recording"
        }

        fn send(
            &self,
            n: &Notification,
        ) -> Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
            let title = n.title.clone();
            Box::pin(async move {
                self.sent.lock().await.push(title);
            })
        }
    }

    // -- Test helpers --

    fn default_config_manager() -> SharedConfigManager {
        Arc::new(config::ConfigManager::new(
            config::Config::default(),
            config::ConfigPersistence::Null,
        ))
    }

    fn unpaused() -> PauseState {
        Arc::new(Mutex::new(None))
    }

    /// Ingest events and flush, returning dispatched notification titles.
    async fn dispatched_titles(events: Vec<WatchEvent>) -> Vec<String> {
        let config = default_config_manager();
        let pause = unpaused();
        let recorder = RecordingNotifier::new();
        let mut pipeline = NotificationPipeline::new();
        let now = Instant::now();

        for event in events {
            pipeline.ingest(event, &config, &pause, now).await;
        }

        pipeline
            .dispatch_expired(now + DEBOUNCE_DELAY, &*recorder)
            .await;
        recorder.titles().await
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

    // -- effective_level tests --

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

    // -- Pipeline transition tests --

    #[test]
    fn transition_allows_first_started() {
        let pipeline = NotificationPipeline::new();
        assert!(pipeline.is_transition(&WatchEvent::RunStarted(snap())));
    }

    #[test]
    fn transition_allows_first_completion() {
        let pipeline = NotificationPipeline::new();
        assert!(pipeline.is_transition(&completed(RunConclusion::Success)));
    }

    #[tokio::test]
    async fn transition_suppresses_same_kind() {
        let titles = dispatched_titles(vec![
            completed(RunConclusion::Success),
            completed(RunConclusion::Success),
        ])
        .await;
        assert_eq!(titles.len(), 1);
        assert!(titles[0].contains("succeeded"));
    }

    #[tokio::test]
    async fn transition_allows_changed_conclusion() {
        let titles = dispatched_titles(vec![
            completed(RunConclusion::Success),
            completed(RunConclusion::Failure),
        ])
        .await;
        assert_eq!(titles.len(), 2);
    }

    #[tokio::test]
    async fn transition_started_after_completion() {
        let titles = dispatched_titles(vec![
            completed(RunConclusion::Success),
            WatchEvent::RunStarted(snap()),
        ])
        .await;
        assert_eq!(titles.len(), 2);
        assert!(titles.iter().any(|t| t.contains("succeeded")));
        assert!(titles.iter().any(|t| t.contains("started")));
    }

    #[tokio::test]
    async fn transition_suppresses_started_while_started() {
        let titles = dispatched_titles(vec![
            WatchEvent::RunStarted(snap_workflow("CI")),
            WatchEvent::RunStarted(snap_workflow("Lint")),
        ])
        .await;
        assert_eq!(titles.len(), 1);
        assert!(titles[0].contains("started"));
    }

    #[tokio::test]
    async fn transition_tracks_per_branch_not_workflow() {
        let titles = dispatched_titles(vec![completed(RunConclusion::Success), {
            let mut s = snap();
            s.workflow = "Lint".to_string();
            WatchEvent::RunCompleted {
                run: s,
                conclusion: RunConclusion::Success,
                elapsed: None,
                failing_steps: None,
                failing_job_id: None,
            }
        }])
        .await;
        // Same branch, same conclusion kind — second is suppressed.
        assert_eq!(titles.len(), 1);
    }

    // -- Pipeline dispatch tests --

    #[tokio::test]
    async fn debounce_coalesces_same_kind_into_one_notification() {
        let titles = dispatched_titles(vec![
            WatchEvent::RunStarted(snap()),
            completed(RunConclusion::Success),
            WatchEvent::RunStarted(snap()),
        ])
        .await;
        // Two "started" events share a debounce key and coalesce into one notification.
        assert_eq!(titles.len(), 2);
        assert!(titles.iter().any(|t| t.contains("started")));
        assert!(titles.iter().any(|t| t.contains("succeeded")));
    }

    #[tokio::test]
    async fn debounce_does_not_fire_before_deadline() {
        let config = default_config_manager();
        let pause = unpaused();
        let recorder = RecordingNotifier::new();
        let mut pipeline = NotificationPipeline::new();
        let now = Instant::now();

        pipeline
            .ingest(WatchEvent::RunStarted(snap()), &config, &pause, now)
            .await;

        // Before deadline: nothing dispatched.
        pipeline
            .dispatch_expired(now + Duration::from_secs(1), &*recorder)
            .await;
        assert!(recorder.titles().await.is_empty());

        // After deadline: dispatched.
        pipeline
            .dispatch_expired(now + DEBOUNCE_DELAY, &*recorder)
            .await;
        assert_eq!(recorder.titles().await.len(), 1);
    }

    #[tokio::test]
    async fn throttle_limits_normal_notifications() {
        let config = default_config_manager();
        let pause = unpaused();
        let recorder = RecordingNotifier::new();
        let mut pipeline = NotificationPipeline::new();
        let now = Instant::now();

        // Alternate started/success on distinct branches to create transitions.
        // Both are Normal level, so throttle applies equally.
        for i in 0..(THROTTLE_MAX + 2) {
            let mut s = snap();
            s.run_id = i as u64;
            s.branch = format!("branch-{i}");
            let event = if i % 2 == 0 {
                WatchEvent::RunStarted(s)
            } else {
                WatchEvent::RunCompleted {
                    run: s,
                    conclusion: RunConclusion::Success,
                    elapsed: None,
                    failing_steps: None,
                    failing_job_id: None,
                }
            };
            pipeline.ingest(event, &config, &pause, now).await;
        }

        pipeline
            .dispatch_expired(now + DEBOUNCE_DELAY, &*recorder)
            .await;
        assert_eq!(recorder.titles().await.len(), THROTTLE_MAX);
    }
}
