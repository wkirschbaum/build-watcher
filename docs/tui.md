# SSE Endpoint + TUI Client Plan

## Context

The build-watcher daemon has an `EventBus` (tokio broadcast channel) that emits `WatchEvent`s as builds start, complete, or change status. Desktop notifications are already a subscriber. The next step is exposing these events over HTTP SSE so an external TUI client can render a real-time dashboard.

**Prep already done:**
- `RunSnapshot` and `WatchEvent` derive `Serialize` (with `elapsed` serialized as `Option<f64>` seconds)
- `EventBus` is already in `WatcherHandle`, which is passed to `build_router` — no new plumbing needed
- `WatcherHandle.events.subscribe()` creates new SSE subscribers
- `display_title()` is a pure function (no SHA suffix) — TUI can call it directly

---

## Part 1: Daemon SSE Endpoint

### Overview

Add two HTTP routes alongside the existing `/mcp` MCP service:
- `GET /status` — JSON snapshot of all current watches
- `GET /events` — SSE stream of `WatchEvent`s as they occur

### Files to modify

| File | Change |
|------|--------|
| `Cargo.toml` | Add `tokio-stream` as direct dependency (already transitive) |
| `src/server.rs` | Add `/status` and `/events` routes using `handle.events` |

### Status endpoint

`GET /status` returns a JSON snapshot:

```json
{
  "paused": false,
  "watches": [
    {
      "repo": "flt/moneyclub",
      "branch": "main",
      "active_runs": [
        {
          "run_id": 12345,
          "status": "in_progress",
          "workflow": "Lint and Test",
          "title": "PR: Fix auth timeout",
          "elapsed_secs": 134.2
        }
      ],
      "last_build": {
        "run_id": 12300,
        "conclusion": "success",
        "workflow": "CI",
        "title": "Update deps"
      }
    }
  ]
}
```

Implementation: handler locks `watches` and `pause`, serializes the HashMap into a sorted Vec, returns `axum::Json`.

### SSE endpoint

`GET /events` returns an SSE stream. Each frame is a JSON-encoded `WatchEvent`:

```
event: RunStarted
data: {"RunStarted":{"repo":"flt/moneyclub","branch":"main",...}}

event: RunCompleted
data: {"RunCompleted":{"run":{...},"conclusion":"success","elapsed":134.2,...}}
```

Implementation using axum's built-in SSE support:

```rust
use axum::response::sse::{Event, Sse};
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt;

async fn events_handler(
    State(events): State<EventBus>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let stream = BroadcastStream::new(events.subscribe())
        .filter_map(|result| result.ok())
        .map(|event| {
            let event_type = match &event {
                WatchEvent::RunStarted(_) => "RunStarted",
                WatchEvent::RunCompleted { .. } => "RunCompleted",
                WatchEvent::StatusChanged { .. } => "StatusChanged",
            };
            Ok(Event::default()
                .event(event_type)
                .json_data(&event)
                .unwrap())
        });
    Sse::new(stream).keep_alive(
        axum::response::sse::KeepAlive::new()
            .interval(Duration::from_secs(30))
    )
}
```

### Router changes

`build_router` already receives `handle: WatcherHandle` which contains `events: EventBus`. Mount the new routes alongside `/mcp`:

```rust
let app_state = AppState {
    watches: watches.clone(),
    pause: pause.clone(),
    events: handle.events.clone(),
};

axum::Router::new()
    .route("/status", get(status_handler))
    .route("/events", get(events_handler))
    .with_state(app_state)
    .nest_service("/mcp", service)
```

### Estimated daemon-side effort

~50 lines of new code in `server.rs`. No new parameters to `build_router` or `serve`.

---

## Part 2: TUI Binary

### Overview

A separate binary `bw` that connects to the daemon's HTTP endpoints and renders a real-time terminal dashboard using `ratatui` + `crossterm`.

### New dependencies (TUI binary only)

- `ratatui` — terminal UI framework
- `crossterm` — terminal backend
- `reqwest` — HTTP client for SSE + JSON

### Binary location

Add as a second binary in the same crate. Extract shared types to `src/lib.rs`:

```toml
[lib]
name = "build_watcher"
path = "src/lib.rs"

[[bin]]
name = "build-watcher"
path = "src/main.rs"

[[bin]]
name = "bw"
path = "src/bin/bw.rs"
```

`src/lib.rs` re-exports the modules both binaries need (`events`, `github`, `config`, `format`, `watcher` types).

### Startup flow

1. Read port from `~/.local/state/build-watcher/port`
2. `GET /status` for initial snapshot — populate internal state
3. Connect to `GET /events` SSE stream
4. Enter ratatui main loop

### Internal state

```rust
struct App {
    watches: Vec<WatchStatus>,  // from /status, updated by events
    selected: usize,            // cursor position for keybinds
    port: u16,
    connected: bool,
}

struct WatchStatus {
    repo: String,
    branch: String,
    active_runs: Vec<ActiveRunView>,
    last_build: Option<LastBuildView>,
}

struct ActiveRunView {
    run_id: u64,
    status: String,
    workflow: String,
    title: String,
    event: String,              // for display_title() — "push" or "pull_request"
    started_at: Instant,        // local clock when first seen
}
```

### Event loop

Main loop on a 1-second tick for live elapsed time updates:

```
loop {
    terminal.draw(|f| render(f, &app))?;

    if event::poll(Duration::from_secs(1))? {
        match event::read()? {
            Key('q') => break,
            Key('r') => rerun_selected(&app),
            Key('o') => open_in_browser(&app),
            Key(Up)  => app.move_up(),
            Key(Down) => app.move_down(),
            _ => {}
        }
    }

    // Drain SSE events (non-blocking)
    while let Ok(event) = sse_rx.try_recv() {
        app.apply_event(event);
    }
}
```

### Layout

```
 build-watcher ── 4 repos ── 3 active ── polling 10s
────────────────────────────────────────────────────────────────────────
 REPO                  BRANCH   STATUS        WORKFLOW          TITLE                    ELAPSED
────────────────────────────────────────────────────────────────────────
 flt/moneyclub         main     ⏳ running     Lint and Test     PR: Fix auth timeout       2m 14s
 flt/moneyclub         main     ⏳ queued      Deploy Staging    PR: Fix auth timeout          12s
 flt/employer          main     ✅ success     CI                Update deps                1m ago
 flt/gateway           main     ❌ failure     CI                Fix rate limiter           5m ago
                                                                  Failed: Build / Run tests
 wkirschbaum/build-…   main     ✅ success     CI                Refactor format           22m ago
────────────────────────────────────────────────────────────────────────
 [q] quit  [r] rerun failed  [o] open in browser  [↑↓] navigate
```

**Row types:**
- Active runs: `⏳` + status, elapsed time ticking live
- Last completed (idle): result emoji + conclusion, age as "Xm ago"
- Failed builds: extra indented line showing failing step names

**Header bar:**
- Total repos, active count, poll interval
- "PAUSED" indicator when notifications paused
- "DISCONNECTED" when SSE drops

### Keybindings

| Key | Action |
|-----|--------|
| `q` | Quit |
| `r` | Rerun selected build's last failure (`gh run rerun`) |
| `o` | Open selected build URL in browser (`xdg-open` / `open`) |
| `↑`/`↓` | Navigate rows |

### Reconnection

If the SSE connection drops:
1. Show "disconnected" indicator in header
2. Retry with exponential backoff (1s, 2s, 4s, max 30s)
3. On reconnect, re-fetch `/status` to resync full state

### Estimated TUI effort

~400-500 lines:
- HTTP/SSE client: ~80 lines
- State management: ~60 lines
- Ratatui rendering: ~200 lines
- Input handling + main loop: ~100 lines

---

## Implementation order

1. **Daemon: SSE + status endpoints** — small, testable with `curl`
2. **Extract `src/lib.rs`** — re-export shared modules for the TUI binary
3. **TUI: basic rendering** — connect, render table, live updates
4. **TUI: keybindings** — rerun, open, navigate
5. **TUI: reconnection** — handle daemon restarts

## Verification

```bash
# Part 1: test with curl
curl -s http://127.0.0.1:8417/status | jq .
curl -N http://127.0.0.1:8417/events   # leave running, trigger a build

# Part 2: run TUI
cargo run --bin bw

# All existing tests must still pass
cargo test --verbose
```
