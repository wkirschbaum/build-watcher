# Architecture

`build-watcher` is a Rust daemon that monitors GitHub Actions workflows, tracks PR merge readiness, and sends desktop notifications. It exposes an MCP (Model Context Protocol) server over HTTP so Claude Code can manage watched repos via tool calls, and a REST API consumed by the `bw` TUI dashboard.

## System overview

```
                          ┌─────────────────────────────────────────────────────────────┐
                          │                    build-watcher daemon                      │
                          │                                                             │
 Claude Code ──MCP/HTTP──►│  server/           ┌─────────────┐    ┌──────────────────┐  │
                          │  ├── mcp.rs ───────►│             │    │  config/         │  │
                          │  ├── rest.rs ──────►│  actions.rs │◄──►│  ├── types.rs    │  │
                          │  └── mod.rs         │             │    │  ├── resolve.rs  │  │
                          │       │             └──────┬──────┘    │  └── mod.rs      │  │
                          │       │                    │           └────────┬─────────┘  │
                          │       │                    ▼                    │             │
                          │       │            watcher/                     │             │
                          │       │            ├── repo_poller.rs ◄────────┘             │
                          │       │            │    (per-repo async task)                │
                          │       │            ├── startup.rs                            │
                          │       │            └── types.rs                              │
                          │       │                    │                                 │
                          │       │                    │ github.rs ──► gh CLI ──► GitHub │
                          │       │                    │                                 │
                          │       │                    ▼                                 │
                          │       │            events.rs (broadcast EventBus)            │
                          │       │                    │                                 │
                          │       │            ┌───────┴────────┐                        │
                          │       │            ▼                ▼                        │
                          │       │      notification.rs   SSE stream                   │
                          │       │            │           GET /events                   │
                          │       │            ▼                │                        │
                          │       │      platform/             │                        │
                          │       │      ├── linux/ (D-Bus)    │                        │
                          │       │      └── macos/ (notifier) │                        │
                          │       │                            │                        │
                          └───────┼────────────────────────────┼────────────────────────┘
                                  │                            │
                          ┌───────┼────────────────────────────┼──────┐
                          │       ▼           bw TUI           ▼      │
                          │  client.rs ──────────────► app.rs         │
                          │                            ├── render.rs  │
                          │                            ├── input.rs   │
                          │                            └── forms.rs   │
                          └───────────────────────────────────────────┘
```

## Data flow

```
                    ┌──────────┐
                    │  GitHub  │
                    │   API    │
                    └────┬─────┘
                         │ gh CLI
                         ▼
                  ┌──────────────┐
                  │ repo_poller  │──── polls runs (15s active / 60s idle)
                  │  (per repo)  │──── polls PRs (on idle cycles)
                  └──────┬───────┘
                         │ emits
                         ▼
  ┌───────────────── EventBus ──────────────────┐
  │                                             │
  │  RunStarted   RunCompleted   StatusChanged  │
  │                PrStateChanged               │
  └──────┬──────────────┬───────────────┬───────┘
         │              │               │
         ▼              ▼               ▼
   notification    SSE /events     status.rs
   handler         (to bw TUI)    (apply_event)
         │
         ▼
   desktop notif
```

## Crate structure

The project builds two binaries from a shared library crate:

```
build_watcher (lib)              Shared types and logic
├── config/                      Configuration loading, saving, resolution
│   ├── types.rs                 Config, RepoConfig, NotificationLevel, QuietHours
│   ├── resolve.rs               Hierarchical notification resolution (global → repo → branch)
│   └── mod.rs                   load_and_normalize(), save_config(), draft recovery
├── watcher/                     Watch lifecycle
│   ├── types.rs                 WatchKey, WatchEntry, ActiveRun, persistence helpers
│   ├── repo_poller.rs           Per-repo async polling: runs + PRs
│   ├── startup.rs               WatcherHandle, start_watch(), startup recovery
│   └── tests.rs                 Mock GitHub client, unit and integration tests
├── events.rs                    EventBus (broadcast), WatchEvent, RunSnapshot
├── github.rs                    GitHubClient trait, gh CLI impl, RunInfo, PrInfo, MergeState
├── status.rs                    WatchStatus, PrView, StatusResponse, StatsResponse
├── history.rs                   Per-repo/branch build history (capped ring buffer)
├── persistence.rs               Crash-safe save_json/load_json, draft recovery
├── rate_limiter.rs              API budget computation, dynamic poll interval scaling
├── format.rs                    Duration, age, and truncation formatting
├── dirs.rs                      config_dir() and state_dir() helpers
└── lib.rs                       Re-exports all modules

build-watcher (daemon binary)    Daemon-only code
├── main.rs                      Entry point, startup orchestration
├── server/
│   ├── mod.rs                   DaemonState, axum router, build_watch_snapshot()
│   ├── mcp.rs                   MCP tool handlers (13 tools)
│   ├── rest.rs                  REST/SSE endpoints
│   ├── actions.rs               Tool action implementations, burst polling, config persistence
│   └── schema.rs                JSON schema definitions for tool parameters
├── notification.rs              Debounce, coalesce, throttle, dispatch desktop notifications
├── register.rs                  MCP server registration in ~/.claude.json
└── platform/
    ├── mod.rs                   Notifier trait, platform detection, global singleton
    ├── linux/mod.rs             D-Bus via zbus, action click handling
    └── macos/mod.rs             terminal-notifier / osascript backends

bw (TUI binary)                  Terminal dashboard
├── main.rs                      Entry point, terminal setup, event loop, daemon discovery
├── app.rs                       App state, event application, sort/group enums
├── input.rs                     Keyboard input handling (normal + form modes)
├── render.rs                    Rendering, display rows, sorting, grouping, PR badges
├── client.rs                    HTTP client for daemon REST API, SSE streaming
├── forms.rs                     Form/picker UI components
└── update.rs                    Background update checker, self-update via GitHub releases
```

## Key types

| Type | Module | Purpose |
|------|--------|---------|
| `DaemonState` | `server` | Shared state: watches, config, watcher handle, github client |
| `WatcherHandle` | `watcher` | `TaskTracker` + `CancellationToken` + `EventBus` for poller lifecycle |
| `Watches` | `watcher` | `Arc<Mutex<HashMap<WatchKey, WatchEntry>>>` — runtime watch state |
| `WatchKey` | `watcher` | Type-safe `repo#branch` key, serializes as string |
| `WatchEntry` | `watcher` | Per-branch state: active runs, last builds, PRs |
| `RepoPoller` | `watcher` | Per-repo async polling task (runs + PRs) |
| `Config` | `config` | Persisted config: repos, branches, notifications, quiet hours, ignored workflows |
| `EventBus` | `events` | Broadcast channel for `WatchEvent`s |
| `WatchEvent` | `events` | `RunStarted`, `RunCompleted`, `StatusChanged`, `PrStateChanged` |
| `RunSnapshot` | `events` | Immutable snapshot of a run's identity, carried by events |
| `GitHubClient` | `github` | Trait abstracting the `gh` CLI (real impl + test mocks) |
| `RunInfo` | `github` | A GitHub Actions run parsed from `gh` CLI output |
| `PrInfo` | `github` | An open PR: number, branches, merge state, author, draft status |
| `MergeState` | `github` | PR merge readiness: Clean, Blocked, Unstable, Behind, Dirty, HasHooks |
| `StatusResponse` | `status` | JSON snapshot of all watches, used by `GET /status` and the TUI |
| `WatchStatus` | `status` | Per-branch view: active runs, last builds, PRs |
| `PrView` | `status` | Compact PR for wire format: number, title, merge state, draft |
| `Notifier` | `platform` | Trait for desktop notification backends |

## Startup sequence

`main.rs` orchestrates startup:

1. Load and normalize config from disk.
2. Load persisted watch state from disk.
3. Create the `EventBus` and subscribe the notification handler.
4. Create a `WatcherHandle` (tracker + cancellation token + event bus).
5. Run `startup_watches` — recover existing watches and start new ones from config.
6. Start the MCP HTTP server.

## Watch lifecycle

```
watch_builds (MCP/REST)
       │
       ▼
  start_watch()
       │
       ├── fetch recent runs via gh
       ├── apply workflow filters
       ├── set last_seen_run_id to highest
       ├── record any in-progress runs
       ├── insert WatchEntry
       │
       └── spawn RepoPoller ──────────────┐
                                          │
                                   poll loop:
                                   ├── sleep (15s active / 60s idle)
                                   ├── poll_active_runs() — gh run view per active run
                                   ├── check_for_new_runs() — gh run list, compare IDs
                                   ├── poll_prs() — gh pr list (when idle, if enabled)
                                   └── emit events → EventBus
```

### PR polling

When `watch_prs` is enabled for a repo, `poll_prs()` runs on idle cycles:

1. Fetches all open PRs via `gh pr list` (up to 50).
2. Groups PRs by their **target branch** (baseRefName).
3. Updates each `WatchEntry.prs` with PRs targeting that watched branch.
4. Detects merge-state transitions and emits `PrStateChanged` events.

This means watching `main` shows all PRs that target `main`, not PRs whose source branch happens to be named `main`.

## Event bus

The `EventBus` (`events.rs`) is a `tokio::sync::broadcast` channel that decouples polling from consumers:

- **Producers**: Pollers emit `RunStarted`, `RunCompleted`, `StatusChanged`, and `PrStateChanged`.
- **Consumers**:
  - **Notification handler** — debounces (3s per repo/branch/kind), coalesces multiple workflows, throttles (10/60s), checks pause/quiet-hours, dispatches desktop notifications.
  - **SSE stream** — `GET /events` forwards events to the TUI for real-time updates.
  - **TUI local state** — `apply_event` on `StatusResponse` updates the TUI's in-memory copy without a full resync.

## Polling strategy

All pollers share a `RateLimitState` refreshed every 60 seconds via `gh api rate_limit`. `compute_intervals` derives dynamic sleep durations:

- **Above 50% remaining** — floor speed (15s/60s), scaled by √(api_calls_per_cycle).
- **Below 50% remaining** — spread remaining budget across seconds until reset.
- **At zero** — wait out the full reset window.

Poll aggression (`low`/`medium`/`high`) controls what fraction of the hourly budget to use.

## Configuration and state persistence

All JSON files use crash-safe writes:

1. Serialize to `.draft`, fsync.
2. Re-read and parse `.draft` to verify.
3. Rename current to `.bak`.
4. Rename `.draft` to target path.

| File | Path | Contents |
|------|------|----------|
| Config | `~/.config/build-watcher/config.json` | Repos, branches, notifications, quiet hours, ignored workflows, aliases |
| Watch state | `~/.local/state/build-watcher/watches.json` | `last_seen_run_id` and `last_builds` per watch key |
| Port | `~/.local/state/build-watcher/port` | Actual bound port for daemon discovery |

## REST API

| Endpoint | Method | Purpose |
|----------|--------|---------|
| `/status` | GET | JSON snapshot of all watches, active runs, last builds, PRs |
| `/stats` | GET | Daemon stats: uptime, polling intervals, API rate limit |
| `/events` | GET | SSE stream of `WatchEvent`s |
| `/notifications` | GET | Resolved notification config for `?repo=&branch=` |
| `/notifications` | POST | Mute, unmute, or set per-event levels |
| `/defaults` | GET | Global config defaults |
| `/defaults` | POST | Update global config defaults |
| `/history` | GET | Build history for a repo (`?repo=&branch=&limit=`) |
| `/history/all` | GET | Recent builds across all repos |
| `/watch` | POST | Add repos to watches |
| `/unwatch` | POST | Remove repos from watches |
| `/branches` | POST | Update branch config for a repo |
| `/pause` | POST | Toggle notification pause |
| `/rerun` | POST | Rerun a build (specific or last failed) |
| `/merge` | POST | Merge a PR by number |
| `/shutdown` | POST | Graceful daemon shutdown |

## TUI dashboard (`bw`)

The `bw` binary connects to the daemon via HTTP:

```
bw startup
    │
    ├── read port file (~/.local/state/build-watcher/port)
    │   └── if missing/unreachable → spawn daemon, wait up to 5s
    │
    ├── tokio::join! {
    │     GET /status   → initial watch state
    │     GET /stats    → daemon stats
    │     GET /history/all → recent builds
    │   }
    │
    └── event loop
         ├── SSE /events → apply_event() on local state (real-time)
         ├── periodic resync (GET /status + /stats + /history/all every 30s)
         ├── local tick every 1s (elapsed times, build ages)
         ├── keyboard input → actions (POST /rerun, /merge, /watch, etc.)
         └── background update checker (10s delay, then hourly)
```

The TUI shares types from the library crate (`status.rs`, `events.rs`, `format.rs`, `github.rs`) but has no dependency on daemon-only code.

## Graceful shutdown

Triggered by SIGINT (ctrl-c) or `POST /shutdown`:

1. Cancel the `CancellationToken` — all pollers exit their loops.
2. Close the `TaskTracker` and wait for all tasks.
3. Save final watch state to disk.
4. Remove the port file.

## Concurrency notes

- One `RepoPoller` task per watched repo. The `Watches` mutex is held only for brief reads/writes — never across awaits or API calls.
- `start_watch` uses double-checked locking: checks before the `gh` call, makes the call, re-checks before inserting.
- The D-Bus notification backend is fully async. `replaces_id` tracking and action click handling use `Arc<Mutex<_>>`.
- Burst polling (1s, 5s, 10s) after rerun/merge triggers the pollers to pick up changes quickly without waiting for the normal interval.
