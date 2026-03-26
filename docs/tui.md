# `bw` TUI Dashboard

A top-like live terminal dashboard for the build-watcher daemon.

## Usage

```bash
cargo run --bin bw
```

Requires the `build-watcher` daemon to be running. The TUI reads the port file from `~/.local/state/build-watcher/port` and connects to the daemon's HTTP API.

## Layout

```
build-watcher — up 2h 15m                    poll 15s/60s  API 4521/5000 (90%)  reset 42m
7 repos, 3 active
────────────────────────────────────────────────────────────────────────────────
REPO                BRANCH    STATUS          WORKFLOW       TITLE              ELAPSED / AGE
floatpays/benefits  main      ⏳ in_progress  CI             Fix login bug      1m 12s
floatpays/moneyclub main      ❌ failure      CI             Update deps        3m ago
  ↳ Build / Run tests
wkirschbaum/build…  main      ✅ success      CI             Add TUI            2h ago
────────────────────────────────────────────────────────────────────────────────
[↑↓] select  [r] rerun  [o] open  [p] pause notifs  [q] quit
```

**Header line 1:** daemon uptime, polling intervals (active/idle), GitHub API rate limit and reset time.

**Header line 2:** watch count, active build count, plus status indicators (paused, SSE connection state, errors, flash messages).

## Keybindings

| Key | Action |
|-----|--------|
| `↑` / `k` | Move cursor up |
| `↓` / `j` | Move cursor down |
| `r` | Rerun selected build (via `POST /rerun`) |
| `o` | Open selected run in browser |
| `p` | Toggle notification pause (via `POST /pause`) |
| `q` | Quit |

## Architecture

The TUI connects to the daemon via three HTTP endpoints:

- **`GET /status`** — Watch state snapshot (repos, active runs, last builds)
- **`GET /stats`** — Daemon stats (uptime, polling intervals, rate limit)
- **`GET /events`** — SSE stream of `WatchEvent`s for real-time updates

Updates arrive via SSE and are applied in-place to the local state. A `/status` + `/stats` resync runs on every SSE (re)connect and every 30 seconds as a fallback. Elapsed times and build ages tick locally every second.

Actions (`r`, `p`) call `POST /rerun` or `POST /pause` on the daemon, then resync to reflect the new state immediately.

## Implementation phases (all complete)

1. **Basic display** — ratatui table, 1s polling, colour coding, failing-steps sub-rows
2. **SSE real-time** — background SSE task, `apply_event`, reconnect with exponential backoff
3. **Navigation & actions** — row selection, rerun, open in browser, pause toggle, flash messages
4. **Polish** — build age, responsive columns, resize handling, top-like multi-line header with stats
