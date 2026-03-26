//! `bw` — live terminal dashboard for the build-watcher daemon.
//!
//! Subscribes to `GET /events` (SSE) for real-time updates and resyncs
//! from `GET /status` on connect and every 30 seconds as a fallback.

use std::time::{Duration, Instant};

use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use crossterm::{ExecutableCommand, execute};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Cell, Paragraph, Row, Table};
use tokio::sync::mpsc;
use tokio_stream::StreamExt as _;

use build_watcher::config::state_dir;
use build_watcher::events::WatchEvent;
use build_watcher::format;
use build_watcher::status::{ActiveRunView, LastBuildView, StatusResponse, WatchStatus};

// -- App state --

struct App {
    status: StatusResponse,
    /// When we last successfully fetched /status.
    last_fetch: Instant,
    /// Error message from the most recent failed fetch, if any.
    fetch_error: Option<String>,
    sse_state: SseState,
}

impl App {
    fn active_count(&self) -> usize {
        self.status
            .watches
            .iter()
            .map(|w| w.active_runs.len())
            .sum()
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

fn flatten_rows(watches: &[WatchStatus]) -> Vec<DisplayRow<'_>> {
    let mut rows = Vec::new();
    for w in watches {
        if w.active_runs.is_empty() {
            match &w.last_build {
                Some(b) => {
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
                None => rows.push(DisplayRow::NeverRan {
                    repo: &w.repo,
                    branch: &w.branch,
                }),
            }
        } else {
            for run in &w.active_runs {
                rows.push(DisplayRow::ActiveRun {
                    repo: &w.repo,
                    branch: &w.branch,
                    run,
                });
            }
        }
    }
    rows
}

// -- Event application --

/// Apply a watch event to the local status snapshot.
///
/// Updates only watches that already exist in the snapshot; new watches
/// appear on the next `/status` resync.
fn apply_event(status: &mut StatusResponse, event: WatchEvent) {
    match event {
        WatchEvent::RunStarted(snap) => {
            let Some(watch) = status
                .watches
                .iter_mut()
                .find(|w| w.repo == snap.repo && w.branch == snap.branch)
            else {
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
            let Some(watch) = status
                .watches
                .iter_mut()
                .find(|w| w.repo == run.repo && w.branch == run.branch)
            else {
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
            });
        }
        WatchEvent::StatusChanged { run, to, .. } => {
            let Some(watch) = status
                .watches
                .iter_mut()
                .find(|w| w.repo == run.repo && w.branch == run.branch)
            else {
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

fn render(frame: &mut ratatui::Frame, app: &App) {
    let area = frame.area();

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // header
            Constraint::Length(1), // column headings
            Constraint::Min(0),    // table body
            Constraint::Length(1), // footer
        ])
        .split(area);

    // -- Header --
    let repo_count = app.status.watches.len();
    let active_count = app.active_count();
    let mut header_spans = vec![
        Span::styled(
            "build-watcher",
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!("  {repo_count} repos  {active_count} active")),
    ];
    if app.status.paused {
        header_spans.push(Span::styled(
            "  ⏸ PAUSED",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
    }
    if let SseState::Disconnected { since } = &app.sse_state {
        header_spans.push(Span::styled(
            format!("  ⚡ reconnecting ({}s)", since.elapsed().as_secs()),
            Style::default().fg(Color::Yellow),
        ));
    }
    if let Some(err) = &app.fetch_error {
        let stale_secs = app.last_fetch.elapsed().as_secs();
        header_spans.push(Span::styled(
            format!("  ⚠ {err} ({stale_secs}s stale)"),
            Style::default().fg(Color::Red),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(header_spans)), chunks[0]);

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
    let display_rows = flatten_rows(&app.status.watches);
    let rows: Vec<Row> = display_rows
        .iter()
        .map(|dr| match dr {
            DisplayRow::ActiveRun { repo, branch, run } => {
                let style = status_style(&run.status);
                let emoji = status_emoji(&run.status);
                let elapsed = run
                    .elapsed_secs
                    .map(|s| format::duration(Duration::from_secs_f64(s)))
                    .unwrap_or_default();
                Row::new(vec![
                    Cell::from(format::truncate(repo, 24)),
                    Cell::from(format::truncate(branch, 12)),
                    Cell::from(format!("{emoji} {}", run.status)).style(style),
                    Cell::from(format::truncate(&run.workflow, 20)),
                    Cell::from(format::truncate(&run.title, 30)),
                    Cell::from(elapsed).style(style),
                ])
            }
            DisplayRow::FailingSteps { steps } => Row::new(vec![
                Cell::from(""),
                Cell::from(""),
                Cell::from(""),
                Cell::from(""),
                Cell::from(format!("  ↳ {}", format::truncate(steps, 50)))
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
                Row::new(vec![
                    Cell::from(format::truncate(repo, 24)),
                    Cell::from(format::truncate(branch, 12)),
                    Cell::from(format!("{emoji} {}", build.conclusion)).style(style),
                    Cell::from(format::truncate(&build.workflow, 20)),
                    Cell::from(format::truncate(&build.title, 30)),
                    Cell::from("").style(style),
                ])
            }
            DisplayRow::NeverRan { repo, branch } => Row::new(vec![
                Cell::from(format::truncate(repo, 24)),
                Cell::from(format::truncate(branch, 12)),
                Cell::from("· idle").style(Style::default().fg(Color::DarkGray)),
                Cell::from(""),
                Cell::from(""),
                Cell::from(""),
            ]),
        })
        .collect();

    let widths = [
        Constraint::Length(25),
        Constraint::Length(13),
        Constraint::Length(18),
        Constraint::Length(21),
        Constraint::Min(20),
        Constraint::Length(14),
    ];

    // Render headings row as a table so columns align with the body.
    let heading_table = Table::new(vec![col_header], widths)
        .block(Block::default())
        .column_spacing(1);
    frame.render_widget(heading_table, chunks[1]);

    let body_table = Table::new(rows, widths)
        .block(Block::default())
        .column_spacing(1);
    frame.render_widget(body_table, chunks[2]);

    // -- Footer --
    let footer = Paragraph::new(Line::from(vec![
        Span::styled("[q]", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(" quit"),
    ]))
    .style(Style::default().fg(Color::DarkGray));
    frame.render_widget(footer, chunks[3]);
}

// -- HTTP --

async fn fetch_status(client: &reqwest::Client, port: u16) -> Result<StatusResponse, String> {
    let url = format!("http://127.0.0.1:{port}/status");
    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("connect: {e}"))?;
    resp.json::<StatusResponse>()
        .await
        .map_err(|e| format!("parse: {e}"))
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

        loop {
            let Some(pos) = buf.find('\n') else { break };
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
    let initial = fetch_status(&client, port).await.unwrap_or_else(|e| {
        eprintln!("Warning: could not fetch initial status: {e}");
        StatusResponse {
            paused: false,
            watches: vec![],
        }
    });

    let mut app = App {
        status: initial,
        last_fetch: Instant::now(),
        fetch_error: None,
        sse_state: SseState::Connecting,
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
                    }
                }
                _ = resync_tick.tick() => {
                    match fetch_status(&client, port).await {
                        Ok(status) => {
                            app.status = status;
                            app.last_fetch = Instant::now();
                            app.fetch_error = None;
                        }
                        Err(e) => app.fetch_error = Some(e),
                    }
                }
                maybe_update = sse_rx.recv() => {
                    match maybe_update {
                        Some(SseUpdate::Event(event)) => {
                            apply_event(&mut app.status, *event);
                        }
                        Some(SseUpdate::Connected) => {
                            app.sse_state = SseState::Connected;
                            // Resync on connect to recover any events missed during startup
                            // or reconnect.
                            match fetch_status(&client, port).await {
                                Ok(status) => {
                                    app.status = status;
                                    app.last_fetch = Instant::now();
                                    app.fetch_error = None;
                                }
                                Err(e) => app.fetch_error = Some(e),
                            }
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
                            if key.code == KeyCode::Char('q') {
                                break;
                            }
                        }
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
