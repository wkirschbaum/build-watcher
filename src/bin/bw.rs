//! `bw` — live terminal dashboard for the build-watcher daemon.
//!
//! Subscribes to `GET /events` (SSE) for real-time updates and resyncs
//! from `GET /status` on connect and every 30 seconds as a fallback.

use std::time::{Duration, Instant};

use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use crossterm::{ExecutableCommand, execute};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Cell, Paragraph, Row, Table};
use tokio::sync::mpsc;
use tokio_stream::StreamExt as _;

use build_watcher::config::state_dir;
use build_watcher::events::WatchEvent;
use build_watcher::format;
use build_watcher::status::{
    ActiveRunView, LastBuildView, StatsResponse, StatusResponse, WatchStatus,
};

// -- App state --

struct App {
    status: StatusResponse,
    stats: StatsResponse,
    /// When we last successfully fetched /status.
    last_fetch: Instant,
    /// Error message from the most recent failed fetch, if any.
    fetch_error: Option<String>,
    sse_state: SseState,
    /// Index into the selectable (non-sub-row) display rows.
    selected: usize,
    /// Transient feedback message shown in the header (e.g. "Rerunning…").
    flash: Option<(String, Instant)>,
}

impl App {
    fn active_count(&self) -> usize {
        self.status
            .watches
            .iter()
            .map(|w| w.active_runs.len())
            .sum()
    }

    async fn resync(&mut self, client: &reqwest::Client, port: u16) {
        match fetch_json::<StatusResponse>(client, port, "/status").await {
            Ok(status) => {
                self.status = status;
                self.last_fetch = Instant::now();
                self.fetch_error = None;
            }
            Err(e) => self.fetch_error = Some(e),
        }
        if let Ok(stats) = fetch_json::<StatsResponse>(client, port, "/stats").await {
            self.stats = stats;
        }
    }
}

// -- SSE state --

/// Message sent from the SSE background task to the main render loop.
enum SseUpdate {
    /// A watch event received from the stream.
    Event(Box<WatchEvent>),
    /// SSE stream successfully connected.
    Connected,
    /// SSE stream disconnected; task will retry with backoff.
    Disconnected,
}

/// Tracks the SSE connection state for header display.
enum SseState {
    Connecting,
    Connected,
    Disconnected { since: Instant },
}

// -- Rows --

/// A flattened display row derived from the status snapshot.
enum DisplayRow<'a> {
    ActiveRun {
        repo: &'a str,
        branch: &'a str,
        run: &'a ActiveRunView,
    },
    FailingSteps {
        steps: &'a str,
    },
    LastBuild {
        repo: &'a str,
        branch: &'a str,
        build: &'a LastBuildView,
    },
    NeverRan {
        repo: &'a str,
        branch: &'a str,
    },
}

/// Result of flattening watches into display rows.
struct FlatRows<'a> {
    rows: Vec<DisplayRow<'a>>,
    /// Indices into `rows` that are selectable (everything except `FailingSteps`).
    selectable: Vec<usize>,
}

fn flatten_rows(watches: &[WatchStatus]) -> FlatRows<'_> {
    let mut rows = Vec::new();
    let mut selectable = Vec::new();
    for w in watches {
        if w.active_runs.is_empty() {
            match &w.last_build {
                Some(b) => {
                    selectable.push(rows.len());
                    rows.push(DisplayRow::LastBuild {
                        repo: &w.repo,
                        branch: &w.branch,
                        build: b,
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
                    });
                }
            }
        } else {
            for run in &w.active_runs {
                selectable.push(rows.len());
                rows.push(DisplayRow::ActiveRun {
                    repo: &w.repo,
                    branch: &w.branch,
                    run,
                });
            }
        }
    }
    FlatRows { rows, selectable }
}

impl DisplayRow<'_> {
    /// Returns `(repo, run_id)` for the selected row. Only valid for selectable rows.
    fn repo_and_run_id(&self) -> (&str, Option<u64>) {
        match self {
            DisplayRow::ActiveRun { repo, run, .. } => (repo, Some(run.run_id)),
            DisplayRow::LastBuild { repo, build, .. } => (repo, Some(build.run_id)),
            DisplayRow::NeverRan { repo, .. } => (repo, None),
            DisplayRow::FailingSteps { .. } => unreachable!("FailingSteps is not selectable"),
        }
    }
}

// -- Event application --

fn find_watch_mut<'a>(
    watches: &'a mut [WatchStatus],
    repo: &str,
    branch: &str,
) -> Option<&'a mut WatchStatus> {
    watches
        .iter_mut()
        .find(|w| w.repo == repo && w.branch == branch)
}

/// Apply a watch event to the local status snapshot.
///
/// Updates only watches that already exist in the snapshot; new watches
/// appear on the next `/status` resync.
fn apply_event(status: &mut StatusResponse, event: WatchEvent) {
    match event {
        WatchEvent::RunStarted(snap) => {
            let Some(watch) = find_watch_mut(&mut status.watches, &snap.repo, &snap.branch) else {
                return;
            };
            if !watch.active_runs.iter().any(|r| r.run_id == snap.run_id) {
                let title = snap.display_title();
                watch.active_runs.push(ActiveRunView {
                    run_id: snap.run_id,
                    status: snap.status,
                    workflow: snap.workflow,
                    title,
                    event: snap.event,
                    elapsed_secs: Some(0.0),
                });
            }
        }
        WatchEvent::RunCompleted {
            run,
            conclusion,
            failing_steps,
            ..
        } => {
            let Some(watch) = find_watch_mut(&mut status.watches, &run.repo, &run.branch) else {
                return;
            };
            watch.active_runs.retain(|r| r.run_id != run.run_id);
            let title = run.display_title();
            watch.last_build = Some(LastBuildView {
                run_id: run.run_id,
                conclusion,
                workflow: run.workflow,
                title,
                failing_steps,
                age_secs: Some(0.0),
            });
        }
        WatchEvent::StatusChanged { run, to, .. } => {
            let Some(watch) = find_watch_mut(&mut status.watches, &run.repo, &run.branch) else {
                return;
            };
            if let Some(active) = watch
                .active_runs
                .iter_mut()
                .find(|r| r.run_id == run.run_id)
            {
                active.status = to;
            }
        }
    }
}

// -- Rendering --

fn status_style(conclusion_or_status: &str) -> Style {
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

fn status_emoji(conclusion_or_status: &str) -> &'static str {
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
struct ColWidths {
    repo: usize,
    workflow: usize,
    title: usize,
}

impl ColWidths {
    fn from_terminal_width(w: u16) -> Self {
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

fn render(frame: &mut ratatui::Frame, app: &App) {
    let area = frame.area();
    let cw = ColWidths::from_terminal_width(area.width);
    let w = area.width as usize;
    let dim = Style::default().fg(Color::DarkGray);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // header (2 info lines + separator)
            Constraint::Length(1), // column headings
            Constraint::Min(0),    // table body
            Constraint::Length(1), // footer
        ])
        .split(area);

    // -- Header line 1: title + stats --
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

    // -- Header line 2: watches + state --
    let repo_count = app.status.watches.len();
    let active_count = app.active_count();
    let mut left2_spans: Vec<Span> = vec![Span::raw(format!(
        "{repo_count} repos, {active_count} active"
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

    // -- Header line 3: separator --
    let line3 = Line::from(Span::styled("─".repeat(w), dim));

    frame.render_widget(Paragraph::new(vec![line1, line2, line3]), chunks[0]);

    // -- Column headings --
    let header_style = Style::default()
        .fg(Color::DarkGray)
        .add_modifier(Modifier::BOLD);
    let col_header = Row::new(vec![
        Cell::from("REPO").style(header_style),
        Cell::from("BRANCH").style(header_style),
        Cell::from("STATUS").style(header_style),
        Cell::from("WORKFLOW").style(header_style),
        Cell::from("TITLE").style(header_style),
        Cell::from("ELAPSED / AGE").style(header_style),
    ]);

    // -- Table rows --
    let flat = flatten_rows(&app.status.watches);
    let selected_display_idx = flat
        .selectable
        .get(app.selected)
        .copied()
        .unwrap_or(usize::MAX);
    let highlight_style = Style::default().bg(Color::DarkGray);

    let rows: Vec<Row> = flat
        .rows
        .iter()
        .enumerate()
        .map(|(i, dr)| {
            let is_selected = i == selected_display_idx;
            let row = match dr {
                DisplayRow::ActiveRun { repo, branch, run } => {
                    let style = status_style(&run.status);
                    let emoji = status_emoji(&run.status);
                    let elapsed = run
                        .elapsed_secs
                        .map(|s| format::duration(Duration::from_secs_f64(s)))
                        .unwrap_or_default();
                    Row::new(vec![
                        Cell::from(format::truncate(repo, cw.repo)),
                        Cell::from(format::truncate(branch, BRANCH_W)),
                        Cell::from(format!("{emoji} {}", run.status)).style(style),
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
                } => {
                    let style = status_style(&build.conclusion);
                    let emoji = status_emoji(&build.conclusion);
                    let age = build
                        .age_secs
                        .map(|s| format::age(s as u64))
                        .unwrap_or_default();
                    Row::new(vec![
                        Cell::from(format::truncate(repo, cw.repo)),
                        Cell::from(format::truncate(branch, BRANCH_W)),
                        Cell::from(format!("{emoji} {}", build.conclusion)).style(style),
                        Cell::from(format::truncate(&build.workflow, cw.workflow)),
                        Cell::from(format::truncate(&build.title, cw.title)),
                        Cell::from(age).style(style),
                    ])
                }
                DisplayRow::NeverRan { repo, branch } => Row::new(vec![
                    Cell::from(format::truncate(repo, cw.repo)),
                    Cell::from(format::truncate(branch, BRANCH_W)),
                    Cell::from("· idle").style(Style::default().fg(Color::DarkGray)),
                    Cell::from(""),
                    Cell::from(""),
                    Cell::from(""),
                ]),
            };
            if is_selected {
                row.style(highlight_style)
            } else {
                row
            }
        })
        .collect();

    let widths = cw.constraints();

    // Render headings row as a table so columns align with the body.
    let heading_table = Table::new(vec![col_header], widths).column_spacing(COL_SPACING);
    frame.render_widget(heading_table, chunks[1]);

    let body_table = Table::new(rows, widths).column_spacing(COL_SPACING);
    frame.render_widget(body_table, chunks[2]);

    // -- Footer --
    let key_style = Style::default()
        .fg(Color::DarkGray)
        .add_modifier(Modifier::BOLD);
    let footer = Paragraph::new(Line::from(vec![
        Span::styled("[↑↓]", key_style),
        Span::raw(" select  "),
        Span::styled("[r]", key_style),
        Span::raw(" rerun  "),
        Span::styled("[o]", key_style),
        Span::raw(" open  "),
        Span::styled("[p]", key_style),
        Span::raw(" pause notifs  "),
        Span::styled("[q]", key_style),
        Span::raw(" quit"),
    ]))
    .style(Style::default().fg(Color::DarkGray));
    frame.render_widget(footer, chunks[3]);
}

// -- HTTP --

async fn fetch_json<T: serde::de::DeserializeOwned>(
    client: &reqwest::Client,
    port: u16,
    path: &str,
) -> Result<T, String> {
    let url = format!("http://127.0.0.1:{port}{path}");
    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("connect: {e}"))?;
    resp.json::<T>().await.map_err(|e| format!("parse: {e}"))
}

// -- SSE background task --

/// Connect to `GET /events` and forward parsed events to `tx` until the
/// stream ends or the channel closes.
///
/// Sets `*connected` to `true` once the HTTP response is received so the
/// caller can distinguish a connection failure from a mid-stream disconnect.
async fn stream_sse(
    client: &reqwest::Client,
    port: u16,
    tx: &mpsc::Sender<SseUpdate>,
    connected: &mut bool,
) -> bool {
    let url = format!("http://127.0.0.1:{port}/events");
    let response = match client.get(&url).send().await {
        Ok(r) => r,
        Err(_) => return false,
    };

    *connected = true;
    if tx.send(SseUpdate::Connected).await.is_err() {
        return true; // channel closed — main task exited
    }

    let mut stream = response.bytes_stream();
    let mut buf = String::new();
    let mut pending_data: Option<String> = None;

    while let Some(chunk) = stream.next().await {
        let bytes = match chunk {
            Ok(b) => b,
            Err(_) => return false,
        };
        buf.push_str(&String::from_utf8_lossy(&bytes));

        while let Some(pos) = buf.find('\n') {
            let line = buf[..pos].trim_end_matches('\r').to_string();
            buf.drain(..=pos);

            if line.is_empty() {
                // End of SSE frame — dispatch accumulated data.
                if let Some(data) = pending_data.take()
                    && let Ok(event) = serde_json::from_str::<WatchEvent>(&data)
                    && tx.send(SseUpdate::Event(Box::new(event))).await.is_err()
                {
                    return true;
                }
            } else if let Some(data) = line.strip_prefix("data: ") {
                pending_data = Some(data.to_string());
                // Lines starting with "event:", "id:", or ":" (comments) are ignored.
            }
        }
    }

    false // stream ended cleanly
}

/// SSE background task: connects, streams events, reconnects with exponential backoff.
async fn sse_task(client: reqwest::Client, port: u16, tx: mpsc::Sender<SseUpdate>) {
    let mut backoff_secs = 1u64;
    loop {
        let mut connected = false;
        if stream_sse(&client, port, &tx, &mut connected).await {
            break; // channel closed
        }
        if tx.send(SseUpdate::Disconnected).await.is_err() {
            break;
        }
        if connected {
            backoff_secs = 1; // successful connection — reset backoff
        } else {
            backoff_secs = (backoff_secs * 2).min(30);
        }
        tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
    }
}

// -- Actions --

async fn post_pause(client: &reqwest::Client, port: u16, pause: bool) -> Result<(), String> {
    let url = format!("http://127.0.0.1:{port}/pause");
    let resp = client
        .post(&url)
        .json(&serde_json::json!({ "pause": pause }))
        .send()
        .await
        .map_err(|e| format!("pause: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("pause: HTTP {}", resp.status()));
    }
    Ok(())
}

async fn post_rerun(
    client: &reqwest::Client,
    port: u16,
    repo: &str,
    run_id: u64,
) -> Result<(), String> {
    let url = format!("http://127.0.0.1:{port}/rerun");
    let resp = client
        .post(&url)
        .json(&serde_json::json!({ "repo": repo, "run_id": run_id }))
        .send()
        .await
        .map_err(|e| format!("rerun: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("rerun: HTTP {}", resp.status()));
    }
    Ok(())
}

fn open_url(repo: &str, run_id: u64) {
    let url = format!("https://github.com/{repo}/actions/runs/{run_id}");
    let cmd = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open" // Linux and other Unix-likes
    };
    let _ = std::process::Command::new(cmd)
        .arg(&url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

// -- Entry point --

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Port discovery.
    let port_file = state_dir().join("port");
    let port_str = std::fs::read_to_string(&port_file).map_err(|e| {
        format!(
            "Could not read port file {}: {e}\nIs build-watcher running?",
            port_file.display()
        )
    })?;
    let port: u16 = port_str
        .trim()
        .parse()
        .map_err(|e| format!("Invalid port in {}: {e}", port_file.display()))?;

    // Initial fetch so there's something to display before SSE connects.
    let client = reqwest::Client::new();
    let initial = fetch_json::<StatusResponse>(&client, port, "/status")
        .await
        .unwrap_or_else(|e| {
            eprintln!("Warning: could not fetch initial status: {e}");
            StatusResponse {
                paused: false,
                watches: vec![],
            }
        });
    let initial_stats = fetch_json::<StatsResponse>(&client, port, "/stats")
        .await
        .unwrap_or_default();

    let mut app = App {
        status: initial,
        stats: initial_stats,
        last_fetch: Instant::now(),
        fetch_error: None,
        sse_state: SseState::Connecting,
        selected: 0,
        flash: None,
    };

    // Terminal setup.
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // SSE background task feeds events into this channel.
    let (sse_tx, mut sse_rx) = mpsc::channel::<SseUpdate>(64);
    tokio::spawn(sse_task(client.clone(), port, sse_tx));

    let mut keyboard = EventStream::new();

    // 1-second tick to advance elapsed times locally between resyncs.
    let mut elapsed_tick = tokio::time::interval(Duration::from_secs(1));
    elapsed_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    // 30-second resync as a fallback guard against missed events.
    // First tick is delayed so it doesn't duplicate the on-connect resync.
    let mut resync_tick = tokio::time::interval_at(
        tokio::time::Instant::now() + Duration::from_secs(30),
        Duration::from_secs(30),
    );
    resync_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let result = async {
        loop {
            terminal.draw(|f| render(f, &app))?;

            tokio::select! {
                _ = elapsed_tick.tick() => {
                    for watch in &mut app.status.watches {
                        for run in &mut watch.active_runs {
                            if let Some(e) = &mut run.elapsed_secs {
                                *e += 1.0;
                            }
                        }
                        if let Some(lb) = &mut watch.last_build
                            && let Some(a) = &mut lb.age_secs
                        {
                            *a += 1.0;
                        }
                    }
                }
                _ = resync_tick.tick() => {
                    app.resync(&client, port).await;
                }
                maybe_update = sse_rx.recv() => {
                    match maybe_update {
                        Some(SseUpdate::Event(event)) => {
                            apply_event(&mut app.status, *event);
                        }
                        Some(SseUpdate::Connected) => {
                            app.sse_state = SseState::Connected;
                            // Resync on connect to recover any events missed during
                            // startup or reconnect.
                            app.resync(&client, port).await;
                        }
                        Some(SseUpdate::Disconnected) => {
                            app.sse_state = SseState::Disconnected { since: Instant::now() };
                        }
                        None => {} // sse_task exited
                    }
                }
                maybe_event = keyboard.next() => {
                    match maybe_event {
                        Some(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press => {
                            let flat = flatten_rows(&app.status.watches);
                            let sel_count = flat.selectable.len();
                            let selected = flat.selectable.get(app.selected)
                                .map(|&idx| flat.rows[idx].repo_and_run_id());

                            match key.code {
                                KeyCode::Char('q') => break,
                                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => break,
                                KeyCode::Up | KeyCode::Char('k') => {
                                    app.selected = app.selected.saturating_sub(1);
                                }
                                KeyCode::Down | KeyCode::Char('j') => {
                                    if sel_count > 0 {
                                        app.selected = (app.selected + 1).min(sel_count - 1);
                                    }
                                }
                                KeyCode::Char('p') => {
                                    let new_pause = !app.status.paused;
                                    if let Err(e) = post_pause(&client, port, new_pause).await {
                                        app.flash = Some((e, Instant::now()));
                                    } else {
                                        app.status.paused = new_pause;
                                        let msg = if new_pause { "Paused" } else { "Resumed" };
                                        app.flash = Some((msg.to_string(), Instant::now()));
                                    }
                                }
                                KeyCode::Char('r') => {
                                    if let Some((repo, Some(run_id))) = selected {
                                        if let Err(e) = post_rerun(&client, port, repo, run_id).await {
                                            app.flash = Some((e, Instant::now()));
                                        } else {
                                            app.flash = Some((format!("Rerun started: {run_id}"), Instant::now()));
                                            app.resync(&client, port).await;
                                        }
                                    }
                                }
                                KeyCode::Char('o') => {
                                    if let Some((repo, Some(run_id))) = selected {
                                        open_url(repo, run_id);
                                    }
                                }
                                _ => {}
                            }
                        }
                        Some(Ok(Event::Resize(_, _))) => {} // triggers redraw at top of loop
                        Some(Err(e)) => return Err(e.into()),
                        _ => {}
                    }
                }
            }
        }
        Ok::<_, Box<dyn std::error::Error>>(())
    }
    .await;

    // Always restore the terminal, even on error.
    disable_raw_mode()?;
    terminal.backend_mut().execute(LeaveAlternateScreen)?;

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use build_watcher::events::{RunSnapshot, WatchEvent};
    use build_watcher::status::{ActiveRunView, StatusResponse, WatchStatus};

    fn snap(repo: &str, branch: &str, run_id: u64) -> RunSnapshot {
        RunSnapshot {
            repo: repo.to_string(),
            branch: branch.to_string(),
            run_id,
            workflow: "CI".to_string(),
            title: "Fix bug".to_string(),
            event: "push".to_string(),
            status: "queued".to_string(),
        }
    }

    fn watch(repo: &str, branch: &str) -> WatchStatus {
        WatchStatus {
            repo: repo.to_string(),
            branch: branch.to_string(),
            active_runs: vec![],
            last_build: None,
        }
    }

    fn status_with(watches: Vec<WatchStatus>) -> StatusResponse {
        StatusResponse {
            paused: false,
            watches,
        }
    }

    // -- RunStarted --

    #[test]
    fn run_started_inserts_active_run() {
        let mut status = status_with(vec![watch("alice/app", "main")]);
        apply_event(
            &mut status,
            WatchEvent::RunStarted(snap("alice/app", "main", 1)),
        );

        let runs = &status.watches[0].active_runs;
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].run_id, 1);
        assert_eq!(runs[0].status, "queued");
        assert_eq!(runs[0].workflow, "CI");
        assert_eq!(runs[0].elapsed_secs, Some(0.0));
    }

    #[test]
    fn run_started_dedup_same_run_id() {
        let mut status = status_with(vec![watch("alice/app", "main")]);
        apply_event(
            &mut status,
            WatchEvent::RunStarted(snap("alice/app", "main", 42)),
        );
        apply_event(
            &mut status,
            WatchEvent::RunStarted(snap("alice/app", "main", 42)),
        );

        assert_eq!(status.watches[0].active_runs.len(), 1);
    }

    #[test]
    fn run_started_ignores_unknown_watch() {
        let mut status = status_with(vec![watch("alice/app", "main")]);
        apply_event(
            &mut status,
            WatchEvent::RunStarted(snap("alice/app", "release", 1)),
        );

        assert!(status.watches[0].active_runs.is_empty());
    }

    // -- RunCompleted --

    #[test]
    fn run_completed_removes_active_run_and_updates_last_build() {
        let mut status = status_with(vec![WatchStatus {
            repo: "alice/app".to_string(),
            branch: "main".to_string(),
            active_runs: vec![ActiveRunView {
                run_id: 7,
                status: "in_progress".to_string(),
                workflow: "CI".to_string(),
                title: "Fix bug".to_string(),
                event: "push".to_string(),
                elapsed_secs: Some(30.0),
            }],
            last_build: None,
        }]);

        apply_event(
            &mut status,
            WatchEvent::RunCompleted {
                run: snap("alice/app", "main", 7),
                conclusion: "success".to_string(),
                elapsed: Some(35.0),
                failing_steps: None,
            },
        );

        assert!(status.watches[0].active_runs.is_empty());
        let lb = status.watches[0].last_build.as_ref().unwrap();
        assert_eq!(lb.run_id, 7);
        assert_eq!(lb.conclusion, "success");
        assert_eq!(lb.workflow, "CI");
        assert!(lb.failing_steps.is_none());
        assert_eq!(lb.age_secs, Some(0.0));
    }

    #[test]
    fn run_completed_sets_failing_steps() {
        let mut status = status_with(vec![watch("alice/app", "main")]);
        apply_event(
            &mut status,
            WatchEvent::RunCompleted {
                run: snap("alice/app", "main", 5),
                conclusion: "failure".to_string(),
                elapsed: None,
                failing_steps: Some("Build / tests".to_string()),
            },
        );

        let lb = status.watches[0].last_build.as_ref().unwrap();
        assert_eq!(lb.failing_steps.as_deref(), Some("Build / tests"));
    }

    #[test]
    fn run_completed_ignores_unknown_watch() {
        let mut status = status_with(vec![watch("alice/app", "main")]);
        apply_event(
            &mut status,
            WatchEvent::RunCompleted {
                run: snap("other/repo", "main", 1),
                conclusion: "success".to_string(),
                elapsed: None,
                failing_steps: None,
            },
        );

        assert!(status.watches[0].last_build.is_none());
    }

    // -- StatusChanged --

    #[test]
    fn status_changed_updates_active_run_status() {
        let mut status = status_with(vec![WatchStatus {
            repo: "alice/app".to_string(),
            branch: "main".to_string(),
            active_runs: vec![ActiveRunView {
                run_id: 3,
                status: "queued".to_string(),
                workflow: "CI".to_string(),
                title: "Fix bug".to_string(),
                event: "push".to_string(),
                elapsed_secs: None,
            }],
            last_build: None,
        }]);

        apply_event(
            &mut status,
            WatchEvent::StatusChanged {
                run: snap("alice/app", "main", 3),
                from: "queued".to_string(),
                to: "in_progress".to_string(),
            },
        );

        assert_eq!(status.watches[0].active_runs[0].status, "in_progress");
    }

    #[test]
    fn status_changed_ignores_unknown_run_id() {
        let mut status = status_with(vec![WatchStatus {
            repo: "alice/app".to_string(),
            branch: "main".to_string(),
            active_runs: vec![ActiveRunView {
                run_id: 3,
                status: "queued".to_string(),
                workflow: "CI".to_string(),
                title: "Fix bug".to_string(),
                event: "push".to_string(),
                elapsed_secs: None,
            }],
            last_build: None,
        }]);

        apply_event(
            &mut status,
            WatchEvent::StatusChanged {
                run: snap("alice/app", "main", 999),
                from: "queued".to_string(),
                to: "in_progress".to_string(),
            },
        );

        assert_eq!(status.watches[0].active_runs[0].status, "queued");
    }

    #[test]
    fn status_changed_ignores_unknown_watch() {
        let mut status = status_with(vec![watch("alice/app", "main")]);
        apply_event(
            &mut status,
            WatchEvent::StatusChanged {
                run: snap("other/repo", "main", 1),
                from: "queued".to_string(),
                to: "in_progress".to_string(),
            },
        );
        // No panic, no state change.
        assert!(status.watches[0].active_runs.is_empty());
    }

    // -- LastBuildView title --

    // -- flatten_rows --

    #[test]
    fn flatten_rows_empty() {
        let flat = flatten_rows(&[]);
        assert!(flat.rows.is_empty());
        assert!(flat.selectable.is_empty());
    }

    #[test]
    fn flatten_rows_idle_watch() {
        let watches = vec![watch("alice/app", "main")];
        let flat = flatten_rows(&watches);
        assert_eq!(flat.rows.len(), 1);
        assert_eq!(flat.selectable.len(), 1);
        assert!(matches!(flat.rows[0], DisplayRow::NeverRan { .. }));
    }

    #[test]
    fn flatten_rows_with_failing_steps_not_selectable() {
        let watches = vec![WatchStatus {
            repo: "alice/app".to_string(),
            branch: "main".to_string(),
            active_runs: vec![],
            last_build: Some(LastBuildView {
                run_id: 1,
                conclusion: "failure".to_string(),
                workflow: "CI".to_string(),
                title: "Fix".to_string(),
                failing_steps: Some("Build / tests".to_string()),
                age_secs: Some(60.0),
            }),
        }];
        let flat = flatten_rows(&watches);
        // LastBuild row + FailingSteps sub-row
        assert_eq!(flat.rows.len(), 2);
        // Only the LastBuild row is selectable
        assert_eq!(flat.selectable.len(), 1);
        assert_eq!(flat.selectable[0], 0);
        assert!(matches!(flat.rows[1], DisplayRow::FailingSteps { .. }));
    }

    #[test]
    fn flatten_rows_success_no_failing_steps_row() {
        let watches = vec![WatchStatus {
            repo: "alice/app".to_string(),
            branch: "main".to_string(),
            active_runs: vec![],
            last_build: Some(LastBuildView {
                run_id: 1,
                conclusion: "success".to_string(),
                workflow: "CI".to_string(),
                title: "Fix".to_string(),
                failing_steps: None,
                age_secs: Some(60.0),
            }),
        }];
        let flat = flatten_rows(&watches);
        assert_eq!(flat.rows.len(), 1);
        assert!(matches!(flat.rows[0], DisplayRow::LastBuild { .. }));
    }

    // -- status_style / status_emoji --

    #[test]
    fn status_style_colors() {
        assert_eq!(status_style("success").fg, Some(Color::Green));
        assert_eq!(status_style("failure").fg, Some(Color::Red));
        assert_eq!(status_style("cancelled").fg, Some(Color::Red));
        assert_eq!(status_style("in_progress").fg, Some(Color::Yellow));
        assert_eq!(status_style("queued").fg, Some(Color::Yellow));
        assert_eq!(status_style("unknown").fg, None);
    }

    #[test]
    fn status_emoji_variants() {
        assert_eq!(status_emoji("success"), "✅");
        assert_eq!(status_emoji("failure"), "❌");
        assert_eq!(status_emoji("in_progress"), "⏳");
        assert_eq!(status_emoji("queued"), "⏸");
        assert_eq!(status_emoji("something_else"), "·");
    }

    // -- ColWidths --

    #[test]
    fn col_widths_minimum_values() {
        // Very narrow terminal — should not panic and should use minimums
        let cw = ColWidths::from_terminal_width(20);
        assert!(cw.repo >= 10);
        assert!(cw.workflow >= 8);
        assert!(cw.title >= 8);
    }

    #[test]
    fn col_widths_wide_terminal() {
        let cw = ColWidths::from_terminal_width(200);
        // Should get reasonable proportions
        assert!(cw.repo > 10);
        assert!(cw.workflow > 8);
        assert!(cw.title > cw.workflow); // title gets 45% vs workflow 25%
    }

    // -- DisplayRow::repo_and_run_id --

    #[test]
    fn display_row_repo_and_run_id() {
        let watches = vec![WatchStatus {
            repo: "alice/app".to_string(),
            branch: "main".to_string(),
            active_runs: vec![ActiveRunView {
                run_id: 42,
                status: "in_progress".to_string(),
                workflow: "CI".to_string(),
                title: "Fix".to_string(),
                event: "push".to_string(),
                elapsed_secs: Some(10.0),
            }],
            last_build: None,
        }];
        let flat = flatten_rows(&watches);
        let (repo, run_id) = flat.rows[0].repo_and_run_id();
        assert_eq!(repo, "alice/app");
        assert_eq!(run_id, Some(42));
    }

    #[test]
    fn run_completed_sets_display_title_for_pr() {
        let mut pr_snap = snap("alice/app", "main", 10);
        pr_snap.event = "pull_request".to_string();
        pr_snap.title = "Add feature".to_string();

        let mut status = status_with(vec![watch("alice/app", "main")]);
        apply_event(
            &mut status,
            WatchEvent::RunCompleted {
                run: pr_snap,
                conclusion: "success".to_string(),
                elapsed: None,
                failing_steps: None,
            },
        );

        let lb = status.watches[0].last_build.as_ref().unwrap();
        assert_eq!(lb.title, "PR: Add feature");
    }
}
