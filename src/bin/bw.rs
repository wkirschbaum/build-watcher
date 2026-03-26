//! `bw` — live terminal dashboard for the build-watcher daemon.
//!
//! Phase 1: polls `GET /status` every second and renders a top-like table.

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
use tokio_stream::StreamExt as _;

use build_watcher::config::state_dir;
use build_watcher::format;
use build_watcher::status::{ActiveRunView, LastBuildView, StatusResponse, WatchStatus};

// -- App state --

struct App {
    status: StatusResponse,
    /// When we last successfully fetched /status.
    last_fetch: Instant,
    /// Error message from the most recent failed fetch, if any.
    fetch_error: Option<String>,
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

    // Initial fetch.
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
    };

    // Terminal setup.
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut events = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let result = async {
        loop {
            terminal.draw(|f| render(f, &app))?;

            tokio::select! {
                _ = tick.tick() => {
                    match fetch_status(&client, port).await {
                        Ok(status) => {
                            app.status = status;
                            app.last_fetch = Instant::now();
                            app.fetch_error = None;
                        }
                        Err(e) => {
                            app.fetch_error = Some(e);
                        }
                    }
                }
                maybe_event = events.next() => {
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
