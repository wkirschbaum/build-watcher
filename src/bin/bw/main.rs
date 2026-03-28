//! `bw` — live terminal dashboard for the build-watcher daemon.
//!
//! Subscribes to `GET /events` (SSE) for real-time updates and resyncs
//! from `GET /status` on connect and every 30 seconds as a fallback.

mod app;
mod client;
mod render;
mod update;

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

use build_watcher::dirs::state_dir;
use build_watcher::status::{StatsResponse, StatusResponse};

use app::{App, InputMode, QuitAction, SseState, SseUpdate, TuiPrefs};
use client::{DaemonClient, sse_task};
use render::render;

/// Read the daemon port from the port file, or start the daemon if it's not running.
fn discover_or_start_daemon() -> Result<u16, Box<dyn std::error::Error>> {
    let port_file = state_dir().join("port");

    // Try reading existing port file first.
    if let Ok(contents) = std::fs::read_to_string(&port_file)
        && let Ok(port) = contents.trim().parse::<u16>()
    {
        if std::net::TcpStream::connect(format!("127.0.0.1:{port}")).is_ok() {
            return Ok(port);
        }
        // Port file exists but daemon is not responding — stale file.
        let _ = std::fs::remove_file(&port_file);
    }

    // Daemon not running — try to start it.
    eprintln!("Daemon not running, starting build-watcher…");
    let exe = std::env::current_exe()?;
    let daemon_bin = exe
        .parent()
        .ok_or("cannot resolve binary directory")?
        .join("build-watcher");

    if !daemon_bin.exists() {
        return Err(format!(
            "build-watcher binary not found at {}\nInstall it with ./install.sh",
            daemon_bin.display()
        )
        .into());
    }

    std::process::Command::new(&daemon_bin)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("Failed to start daemon: {e}"))?;

    // Wait for the port file to appear (up to 5 seconds).
    for _ in 0..50 {
        std::thread::sleep(std::time::Duration::from_millis(100));
        if let Ok(contents) = std::fs::read_to_string(&port_file)
            && let Ok(port) = contents.trim().parse::<u16>()
            && std::net::TcpStream::connect(format!("127.0.0.1:{port}")).is_ok()
        {
            return Ok(port);
        }
    }

    Err("Timed out waiting for daemon to start".into())
}

/// Delete watches and history state files, leaving config intact.
fn reset_state() -> Result<(), Box<dyn std::error::Error>> {
    let dir = state_dir();
    let mut removed = 0;
    for name in ["watches.json", "history.json"] {
        let path = dir.join(name);
        match std::fs::remove_file(&path) {
            Ok(()) => {
                eprintln!("Removed {}", path.display());
                removed += 1;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => eprintln!("Failed to remove {}: {e}", path.display()),
        }
    }
    if removed == 0 {
        eprintln!("No state files to remove.");
    } else {
        eprintln!("State reset. Restart the daemon to pick up changes.");
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::args().any(|a| a == "--reset-state") {
        return reset_state();
    }

    let port = discover_or_start_daemon()?;

    // Fetch initial data concurrently so there's something to display before SSE connects.
    let daemon = DaemonClient::new(port);
    let (initial, initial_stats, initial_history) = tokio::join!(
        daemon.get_json::<StatusResponse>("/status"),
        daemon.get_json::<StatsResponse>("/stats"),
        daemon.get_all_history(20),
    );
    let initial = initial.unwrap_or_else(|e| {
        eprintln!("Warning: could not fetch initial status: {e}");
        StatusResponse {
            paused: false,
            watches: vec![],
        }
    });
    let initial_stats = initial_stats.unwrap_or_default();
    let initial_history = initial_history.unwrap_or_default();

    // Shared channel for SSE events and background action results.
    let (sse_tx, mut sse_rx) = mpsc::channel::<SseUpdate>(64);

    // Check for a newer release at startup (10s delay), then every hour.
    tokio::spawn({
        let tx = sse_tx.clone();
        async move {
            tokio::time::sleep(Duration::from_secs(10)).await;
            loop {
                if let Some(version) = update::check_latest().await {
                    let _ = tx.send(SseUpdate::UpdateAvailable(version)).await;
                    break;
                }
                tokio::time::sleep(Duration::from_secs(3600)).await;
            }
        }
    });

    let prefs = TuiPrefs::load();
    let mut app = App::new(
        initial,
        initial_stats,
        initial_history,
        prefs,
        sse_tx.clone(),
    );

    // Terminal setup.
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    tokio::spawn(sse_task(daemon.clone(), sse_tx));

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
                            app.status.apply_event(*event);
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
                        Some(SseUpdate::UpdateAvailable(version)) => {
                            app.update_available = Some(version);
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
