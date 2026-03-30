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
7 repos, 3 active
┌──────────────────────────────────────────────────────────────────────────────┐
│ REPO ↑              BRANCH    STATUS          WORKFLOW       TITLE           ELAPSED / AGE │
│ floatpays/benefits  main      ⏳ in_progress  CI             Fix login bug   1m 12s        │
│ floatpays/moneyclub main      ✗ failure       CI             Update deps     3m ago        │
│ wkirschbaum/build…  main      ✓ success       CI             Add TUI         2h ago        │
└──────────────────────────────────────────────────────────────────────────────────────────▼┘
┌─ Recent ────────────────────────────────────────────────────────────────────────────────────┐
│ floatpays/benefits  main  ✓ success  CI  Fix login bug  2m 01s  5m ago                      │
└─────────────────────────────────────────────────────────────────────────────────────────────┘
 floatpays/moneyclub  ·  main  ·  failure  ·  run 12345  ·  failed: Build / Run tests
─[↑↓/jk] nav  [e/E] expand  │  [a] add  [b] branch  [d] del  [o/O] open  [r/R] rerun  │  [n/N] mute  [p] pause  [h] hist  [H] recent  │  [s/S] sort  [g/G] group  [C] config  │  [q] quit  [Q] stop  [?] hide
```

**Header line 1:** daemon uptime, polling intervals (active/idle), GitHub API rate limit and reset time.

**Header line 2:** watch count, active build count, plus status indicators (paused, SSE connection state, errors, flash messages).

**Watches panel:** bordered, scrollable. `▲`/`▼` appear on the panel border when there is hidden content above or below. Column headings are inside the panel. The current sort column is highlighted with `▲`/`▼`. When a non-default group-by is active, the mode is shown right-aligned in the panel's top border.

**Detail bar:** single row below the watches panel showing contextual info for the selected row (repo, branch, status, run ID, failing steps, etc.).

**Recent panel:** optional bordered panel showing the latest builds across all repos. Toggle with `H`.

**Help bar:** key reference. Toggle with `?`.

## Keybindings

| Key | Action |
|-----|--------|
| `↑` / `k` | Move cursor up |
| `↓` / `j` | Move cursor down |
| `e` | Cycle expand level for selected repo (Full → Branches → Collapsed) |
| `E` | Cycle expand level for all repos simultaneously |
| `←` | Collapse selected row (repo → branches → collapsed) |
| `→` / `Tab` / `Enter` | Expand selected row |
| `a` | Add a repo to watch |
| `d` | Remove selected repo or branch |
| `b` | Set branches for selected repo |
| `r` | Rerun failed jobs for selected build |
| `R` | Rerun all jobs for selected build |
| `o` | Open failed job or current run in browser |
| `O` | Open repo Actions page in browser |
| `n` | Toggle mute for selected repo/branch |
| `N` | Open notification level picker (per-event levels) |
| `h` | Open build history popup for selected item |
| `H` | Toggle the Recent builds panel |
| `p` | Toggle notification pause |
| `s` / `S` | Cycle sort column forward / backward |
| `g` / `G` | Cycle group-by forward / backward |
| `C` | Edit global config (default branches, ignored workflows, poll aggression) |
| `?` | Toggle help bar |
| `q` | Quit |
| `Q` | Quit and shut down daemon |
| `U` | Quit and run self-update (shown when update available) |

**Sort columns:** repo, branch, status, workflow, age

**Group-by modes:** org (default), branch, workflow, status, none

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
