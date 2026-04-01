//! `bw` — live terminal dashboard for the build-watcher daemon.
//!
//! Subscribes to `GET /events` (SSE) for real-time updates and resyncs
//! from `GET /status` on connect and every 30 seconds as a fallback.

mod app;
mod client;
mod forms;
mod input;
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

/// Try to connect to an existing daemon. Returns the port if reachable, or `None`.
fn try_existing_daemon() -> Option<u16> {
    let port_file = state_dir().join("port");
    let contents = std::fs::read_to_string(&port_file).ok()?;
    let port = contents.trim().parse::<u16>().ok()?;
    std::net::TcpStream::connect(format!("127.0.0.1:{port}"))
        .ok()
        .map(|_| port)
}

/// Start the daemon process (fire and forget).
fn start_daemon() -> Result<(), Box<dyn std::error::Error>> {
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
    Ok(())
}

/// Background task: poll until the daemon is reachable, then send `DaemonReady`.
async fn wait_for_daemon(tx: mpsc::Sender<SseUpdate>) {
    let port_file = state_dir().join("port");
    for _ in 0..300 {
        // up to 30 seconds
        tokio::time::sleep(Duration::from_millis(100)).await;
        if let Ok(contents) = tokio::fs::read_to_string(&port_file).await
            && let Ok(port) = contents.trim().parse::<u16>()
        {
            // Verify the port is actually accepting connections.
            if tokio::net::TcpStream::connect(format!("127.0.0.1:{port}"))
                .await
                .is_ok()
            {
                let _ = tx.send(SseUpdate::DaemonReady(port)).await;
                return;
            }
        }
    }
    let _ = tx
        .send(SseUpdate::BackgroundResult {
            flash: "Timed out waiting for daemon to start".to_string(),
            resync: false,
        })
        .await;
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

/// Download and execute a script from GitHub.
fn run_remote_script(script: &str, label: &str) -> Result<(), Box<dyn std::error::Error>> {
    let url = format!("https://raw.githubusercontent.com/wkirschbaum/build-watcher/main/{script}");
    let status = std::process::Command::new("bash")
        .args(["-c", &format!("curl -fsSL '{url}' | bash")])
        .status()?;
    if !status.success() {
        return Err(format!("{label} failed").into());
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::args().any(|a| a == "--reset-state") {
        return reset_state();
    }
    if std::env::args().any(|a| a == "--uninstall") {
        return run_remote_script("uninstall.sh", "uninstall");
    }
    if std::env::args().any(|a| a == "--update") {
        return run_remote_script("install.sh", "update");
    }

    // Shared channel for SSE events and background action results.
    let (sse_tx, mut sse_rx) = mpsc::channel::<SseUpdate>(64);

    // Try to connect to an existing daemon, or start one and poll in the background.
    let daemon: Option<DaemonClient> = if let Some(port) = try_existing_daemon() {
        Some(DaemonClient::new(port))
    } else {
        eprintln!("Daemon not running, starting build-watcher…");
        start_daemon()?;
        tokio::spawn(wait_for_daemon(sse_tx.clone()));
        None
    };

    // Fetch initial data if daemon is already available.
    let (initial, initial_stats, initial_history) = if let Some(ref daemon) = daemon {
        let (status, stats, history) = tokio::join!(
            daemon.get_json::<StatusResponse>("/status"),
            daemon.get_json::<StatsResponse>("/stats"),
            daemon.get_all_history(20),
        );
        (
            status.unwrap_or_else(|e| {
                eprintln!("Warning: could not fetch initial status: {e}");
                StatusResponse {
                    paused: false,
                    watches: vec![],
                }
            }),
            stats.unwrap_or_default(),
            history.unwrap_or_default(),
        )
    } else {
        (
            StatusResponse {
                paused: false,
                watches: vec![],
            },
            StatsResponse::default(),
            vec![],
        )
    };

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

    if daemon.is_none() {
        app.set_flash("Starting daemon…");
    }

    // Start SSE streaming if daemon is already available.
    if let Some(ref daemon) = daemon {
        tokio::spawn(sse_task(daemon.clone(), sse_tx.clone()));
    }

    // Terminal setup.
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // Track the current daemon client — starts as None if daemon wasn't ready.
    let mut daemon = daemon;

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

            // Use a no-op daemon for input handling when the daemon isn't ready yet.
            // All actions will fail gracefully with connection errors.
            let fallback;
            let active_daemon = match &daemon {
                Some(d) => d,
                None => {
                    fallback = DaemonClient::new(0);
                    &fallback
                }
            };

            tokio::select! {
                _ = elapsed_tick.tick() => {
                    app.tick_timers();
                }
                _ = resync_tick.tick() => {
                    if daemon.is_some() {
                        app.resync(active_daemon).await;
                    }
                }
                maybe_update = sse_rx.recv() => {
                    match maybe_update {
                        Some(SseUpdate::DaemonReady(port)) => {
                            let d = DaemonClient::new(port);
                            app.resync(&d).await;
                            app.set_flash("Daemon connected");
                            tokio::spawn(sse_task(d.clone(), sse_tx.clone()));
                            daemon = Some(d);
                        }
                        Some(SseUpdate::Event(event)) => {
                            app.status.apply_event(*event);
                        }
                        Some(SseUpdate::Connected) => {
                            app.sse_state = SseState::Connected;
                            app.resync(active_daemon).await;
                        }
                        Some(SseUpdate::Disconnected) => {
                            app.sse_state = SseState::Disconnected { since: Instant::now() };
                        }
                        Some(SseUpdate::BackgroundResult { flash, resync }) => {
                            app.set_flash(flash);
                            if resync && daemon.is_some() {
                                app.resync(active_daemon).await;
                            }
                        }
                        Some(SseUpdate::EnterForm { title, kind, fields }) => {
                            app.input_mode = InputMode::Form {
                                title,
                                kind,
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
                            if app.handle_input(key.code, active_daemon) {
                                continue;
                            }
                            match app.handle_normal_key(key.code, key.modifiers, active_daemon) {
                                QuitAction::Quit => break,
                                QuitAction::QuitAndShutdown => {
                                    if let Some(d) = &daemon {
                                        let _ = d.shutdown().await;
                                    }
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
