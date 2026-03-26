# `bw` TUI Plan

A top-like live terminal dashboard for build-watcher.

---

## Status

### Phase 1 — complete ✅

ratatui table, 1s polling, colour coding, `q` quit, `⏸ PAUSED` header indicator, failing-steps sub-row, column truncation.

### Phase 2 — complete ✅

SSE background task subscribes to `GET /events`; events applied in-place via `apply_event`. Reconnects with exponential backoff (1s → 2s → … → 30s), resetting after each successful connection. `/status` resync on every (re)connect and every 30 s as a fallback. Header shows `⚡ reconnecting (Xs)` when disconnected.

---

## Dependencies to add

```toml
ratatui = "0.29"
crossterm = "0.28"
reqwest = { version = "0.12", default-features = false, features = ["json", "rustls-tls", "stream"] }
```

`reqwest` with `stream` is needed for SSE. `ratatui` + `crossterm` for the terminal UI.

---

## Phase 1 — Basic live display

**Goal:** Replace the stub with a ratatui app that auto-refreshes from `/status` every second.

### Layout

```
build-watcher  7 repos  1 active
────────────────────────────────────────────────────────────────────────────────
REPO                    BRANCH    STATUS       WORKFLOW          ELAPSED / AGE
────────────────────────────────────────────────────────────────────────────────
floatpays/benefits      main      ⏳ running   CI                1m 12s
floatpays/moneyclub     main      ❌ failure   CI                3m ago
floatpays/moneyclub     release   ✅ success   CI                1h ago
wkirschbaum/build-…     main      ✅ success   CI                2h ago
────────────────────────────────────────────────────────────────────────────────
[q] quit
```

### Behaviour

- Poll `GET /status` every second with `reqwest`
- Elapsed time ticks live from `elapsed_secs` in the response
- Colour: green success, red failure, yellow running/queued
- `q` to quit

### Internal state

```rust
struct App {
    status: StatusResponse,  // deserialized from /status
    last_fetch: Instant,
}
```

No SSE yet — pure polling. Simple to build and test.

---

## Phase 2 — SSE real-time updates

**Goal:** Replace polling with the SSE stream so updates appear the moment the daemon emits them.

### Behaviour

- Background tokio task reads `GET /events` SSE stream
- Events passed to the render loop via `tokio::sync::mpsc`
- `apply_event(WatchEvent)` updates `App` state in-place:
  - `RunStarted` → insert active run row
  - `RunCompleted` → remove active run, update `last_build`
  - `StatusChanged` → update status on active run row
- Reconnection: if stream drops, show `DISCONNECTED` in header; retry with 1s/2s/4s/…/30s backoff; re-fetch `/status` on reconnect to resync
- Keep a `/status` resync every 30s as a fallback guard against missed events

---

## Phase 3 — Navigation and actions

**Goal:** Make it interactive.

| Key | Action |
|-----|--------|
| `↑` / `↓` | Move cursor between rows |
| `r` | Rerun last failed build for selected watch |
| `o` | Open selected run URL in browser (`xdg-open` / `open`) |
| `p` | Toggle pause notifications |
| `q` | Quit |

---

## Phase 4 — Polish

**Goal:** Daily-driver quality.

- Failing steps shown as an indented sub-row under failed builds
- Long repo/title names truncated to fit terminal width responsively
- Header shows paused indicator and current poll interval
- Elapsed time uses `build_watcher::format::duration` (already exists in lib)
- Completed build age formatted as "3m ago", "2h ago"
- Terminal resize handling (redraw on `SIGWINCH`)

---

## Implementation order

| Phase | Approx lines | Value |
|-------|-------------|-------|
| 1 — basic table, polling | ~150 | Immediately useful |
| 2 — SSE real-time | ~100 | Zero-lag updates |
| 3 — navigation + actions | ~80 | Interactive |
| 4 — polish | ~80 | Daily-driver quality |

---

## Verification

```bash
# Daemon endpoints (already working)
curl -s http://127.0.0.1:8417/status | jq .
curl -N http://127.0.0.1:8417/events   # leave running, trigger a build

# TUI
cargo run --bin bw

# All existing tests must still pass
cargo fmt && cargo clippy && cargo test
```
