//! `bw` — live terminal dashboard for the build-watcher daemon.
//!
//! Subscribes to `GET /events` (SSE) for real-time updates and resyncs
//! from `GET /status` on connect and every 30 seconds as a fallback.

mod app;
mod client;
mod render;

use std::time::{Duration, Instant};

use crossterm::event::{Event, EventStream, KeyEventKind};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, SetTitle, disable_raw_mode, enable_raw_mode,
};
use crossterm::{ExecutableCommand, execute};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tokio::sync::mpsc;
use tokio_stream::StreamExt as _;

use build_watcher::status::{StatsResponse, StatusResponse};

use app::{App, InputMode, QuitAction, SseState, SseUpdate, TuiPrefs, apply_event};
use client::{DaemonClient, discover_or_start_daemon, sse_task};
use render::render;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let port = discover_or_start_daemon()?;

    // Initial fetch so there's something to display before SSE connects.
    let daemon = DaemonClient::new(port);
    let initial = daemon
        .get_json::<StatusResponse>("/status")
        .await
        .unwrap_or_else(|e| {
            eprintln!("Warning: could not fetch initial status: {e}");
            StatusResponse {
                paused: false,
                watches: vec![],
            }
        });
    let initial_stats = daemon
        .get_json::<StatsResponse>("/stats")
        .await
        .unwrap_or_default();
    let initial_history = daemon.get_all_history(20).await.unwrap_or_default();

    // Shared channel for SSE events and background action results.
    let (sse_tx, mut sse_rx) = mpsc::channel::<SseUpdate>(64);

    let prefs = TuiPrefs::load();
    let mut app = App {
        status: initial,
        stats: initial_stats,
        recent_history: initial_history,
        last_fetch: Instant::now(),
        fetch_error: None,
        sse_state: SseState::Connecting,
        selected: 0,
        flash: None,
        input_mode: InputMode::Normal,
        bg_tx: sse_tx.clone(),
        sort_column: prefs.sort_column,
        sort_ascending: prefs.sort_ascending,
        group_by: prefs.group_by,
        collapsed: std::collections::HashSet::new(),
    };

    // Terminal setup.
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    tokio::spawn(sse_task(daemon.inner().clone(), port, sse_tx));

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
            execute!(terminal.backend_mut(), SetTitle(app.terminal_title()))?;
            terminal.draw(|f| render(f, &app))?;

            tokio::select! {
                _ = elapsed_tick.tick() => {
                    app.tick_timers();
                }
                _ = resync_tick.tick() => {
                    app.resync(&daemon).await;
                }
                maybe_update = sse_rx.recv() => {
                    match maybe_update {
                        Some(SseUpdate::Event(event)) => {
                            apply_event(&mut app.status, *event);
                        }
                        Some(SseUpdate::Connected) => {
                            app.sse_state = SseState::Connected;
                            app.resync(&daemon).await;
                        }
                        Some(SseUpdate::Disconnected) => {
                            app.sse_state = SseState::Disconnected { since: Instant::now() };
                        }
                        Some(SseUpdate::BackgroundResult { flash, resync }) => {
                            app.set_flash(flash);
                            if resync {
                                app.resync(&daemon).await;
                            }
                        }
                        Some(SseUpdate::EnterForm { title, fields }) => {
                            app.input_mode = InputMode::Form {
                                title,
                                fields,
                                active: 0,
                            };
                        }
                        Some(SseUpdate::EnterNotificationPicker {
                            repo,
                            branch,
                            levels,
                        }) => {
                            app.input_mode = InputMode::NotificationPicker {
                                repo,
                                branch,
                                levels,
                                active: 0,
                            };
                        }
                        Some(SseUpdate::EnterHistory {
                            repo,
                            branch,
                            entries,
                        }) => {
                            app.input_mode = InputMode::History {
                                repo,
                                branch,
                                entries,
                                selected: 0,
                            };
                        }
                        None => {}
                    }
                }
                maybe_event = keyboard.next() => {
                    match maybe_event {
                        Some(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press => {
                            if app.handle_input(key.code, &daemon) {
                                continue;
                            }
                            match app.handle_normal_key(key.code, key.modifiers, &daemon) {
                                QuitAction::Quit => break,
                                QuitAction::QuitAndShutdown => {
                                    let _ = daemon.shutdown().await;
                                    break;
                                }
                                QuitAction::None => {}
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
    terminal
        .backend_mut()
        .execute(SetTitle(""))
        .and_then(|s| s.execute(LeaveAlternateScreen))?;

    result
}
