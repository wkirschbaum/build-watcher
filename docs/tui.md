# `bw` TUI Dashboard

A top-like live terminal dashboard for the build-watcher daemon.

## Usage

```bash
bw
```

Auto-starts the daemon if it isn't running. The TUI reads the port file from `~/.local/state/build-watcher/port` and connects to the daemon's HTTP API.

```bash
bw --update    # Self-update to the latest release
```

## Layout

```
build-watcher — up 2h 15m                    poll 15s/60s  API 4521/5000 (90%)  reset 42m
7 repos, 3 active  ↑ v0.3.0 available [U]
────────────────────────────────────────────────────────────────────────────────
REPO                BRANCH    STATUS          WORKFLOW       TITLE              ELAPSED / AGE
floatpays/benefits  main      ⏳ in_progress  CI             Fix login bug      1m 12s
floatpays/moneyclub main      ❌ failure      CI             Update deps        3m ago
  ↳ Build / Run tests
wkirschbaum/build…  main      ✅ success      CI             Add TUI            2h ago
────────────────────────────────────────────────────────────────────────────────
[↑↓] nav  [a] add  [d] remove  [o/O] open  [n/N] mute/levels  [p] pause  [s/S] sort  [g/G] group  [C] config  [q] quit  [Q] stop  [U] update
```

**Header line 1:** daemon uptime, polling intervals (active/idle), GitHub API rate limit and reset time.

**Header line 2:** watch count, active build count, plus status indicators (paused, SSE connection state, errors, flash messages).

## Keybindings

| Key | Action |
|-----|--------|
| `↑` / `k` | Move cursor up |
| `↓` / `j` | Move cursor down |
| `a` | Add a repo to watch |
| `d` | Remove selected repo |
| `b` | Set branches for selected repo |
| `r` | Rerun selected build (via `POST /rerun`) |
| `o` | Open current build run in browser |
| `O` | Open repo page in browser |
| `n` | Toggle mute for selected repo/branch |
| `N` | Open notification level picker (per-event levels) |
| `h` | Open build history for selected repo |
| `p` | Toggle notification pause (via `POST /pause`) |
| `s` / `S` | Cycle sort column forward / backward |
| `g` / `G` | Cycle group-by forward / backward |
| `C` | Edit global config (default branches, ignored workflows, poll aggression) |
| `q` | Quit |
| `Q` | Quit and shut down daemon |
| `U` | Quit and run self-update (shown when update available) |

## Architecture

The TUI connects to the daemon via HTTP endpoints:

- **`GET /status`** — Watch state snapshot (repos, active runs, last builds)
- **`GET /stats`** — Daemon stats (uptime, polling intervals, rate limit)
- **`GET /events`** — SSE stream of `WatchEvent`s for real-time updates
- **`GET /history/all`** — Recent build history across all repos
- **`GET /defaults`** and **`POST /defaults`** — Global config management
- **`GET /notifications`** and **`POST /notifications`** — Per-repo/branch notification config
- **`POST /watch`**, **`/unwatch`**, **`/branches`** — Watch management
- **`POST /pause`**, **`/rerun`**, **`/shutdown`** — Actions

Initial data is fetched concurrently via `tokio::join!`. Updates arrive via SSE and are applied in-place to the local state. A `/status` + `/stats` + `/history/all` resync runs on every SSE (re)connect and every 30 seconds as a fallback. Elapsed times and build ages tick locally every second.

A background task checks for new releases at startup (after a 10s delay), then hourly. When a newer version is found, the header shows the available version and the `[U]` keybinding appears.
