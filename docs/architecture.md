# Architecture

`build-watcher` is a Rust daemon that monitors GitHub Actions workflows and delivers desktop notifications. It exposes an MCP (Model Context Protocol) server over HTTP so Claude Code can manage watched repos via tool calls.

## High-level flow

```
Claude Code ──HTTP/MCP──► server.rs (axum + rmcp)
                               │
                               ├── config.json  (persisted configuration)
                               ├── watches.json (persisted watch state)
                               │
                               ▼
                          watcher.rs (per-repo/branch polling tasks)
                               │
                               ├── github.rs ──► gh CLI ──► GitHub API
                               │
                               ▼
                          events.rs (broadcast EventBus)
                               │
                               ▼
                          platform/ (desktop notifications + sound)
```

## Source layout

```
src/
├── main.rs          — entry point, wires up config, watches, event bus, server
├── server.rs        — MCP tool handlers, BuildWatcher struct, axum router
├── watcher.rs       — watch lifecycle, Poller task, state persistence
├── events.rs        — EventBus (broadcast channel), WatchEvent types, notification handler
├── config.rs        — Config structs, crash-safe JSON persistence helpers
├── format.rs        — duration, age, and truncation formatting
├── github.rs        — gh CLI wrappers, RunInfo/HistoryEntry types, input validation
└── platform/
    ├── mod.rs       — Notifier trait, global singleton, platform dispatch
    ├── universal/   — NullNotifier (used in tests)
    ├── linux/
    │   ├── mod.rs   — detection, shared helpers (notification props, app name)
    │   └── dbus.rs  — D-Bus backend via zbus (org.freedesktop.Notifications)
    └── macos/
        ├── mod.rs                — detection (terminal-notifier → osascript), sound mapping
        ├── terminal_notifier.rs  — terminal-notifier backend (preferred)
        └── apple_script.rs       — osascript fallback
```

## Key types

| Type | Module | Purpose |
|------|--------|---------|
| `BuildWatcher` | `server` | MCP server handler; owns shared state, routes tool calls |
| `WatcherHandle` | `watcher` | `TaskTracker` + `CancellationToken` + `EventBus` for poller lifecycle |
| `Watches` | `watcher` | `Arc<Mutex<HashMap<WatchKey, WatchEntry>>>` — runtime watch state |
| `WatchKey` | `watcher` | Type-safe `repo#branch` key, serializes as string |
| `WatchEntry` | `watcher` | Per-branch state: active runs, failure counts, last build |
| `Poller` | `watcher` | Per-repo/branch async polling task |
| `Config` | `config` | Persisted configuration: repos, branches, notification levels, quiet hours, ignored workflows. `short_repo(&str)` returns an unambiguous display name |
| `EventBus` | `events` | Broadcast channel for `WatchEvent`s |
| `WatchEvent` | `events` | `RunStarted`, `RunCompleted`, `StatusChanged` |
| `RunSnapshot` | `events` | Immutable snapshot of a run's identity, carried by events |
| `RunInfo` | `github` | A GitHub Actions run parsed from `gh` CLI output |
| `HistoryEntry` | `github` | A build history entry with timestamps for duration/age |
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

1. **`watch_builds` tool call** → validates repo names, reads branch config, calls `start_watch` per branch. Only persists repos to config after at least one branch successfully starts (prevents typos from polluting config).
2. **`start_watch`** → fetches recent runs via `gh run list`, applies workflow filters, sets `last_seen_run_id` to the highest run ID, records any in-progress runs, inserts a `WatchEntry`, then spawns a `Poller` task.
3. **`Poller::run` loop** → runs until the watch entry is removed or cancellation:
   - Sleeps `active_secs` (minimum 15s) when builds are running, `idle_secs` (minimum 60s) when idle.
   - Calls `poll_active_runs` every cycle when active.
   - Calls `check_for_new_runs` at least every idle interval regardless of active state.
4. **`stop_watches` tool call** → removes entries from the watch map. Pollers detect the missing key on their next iteration and exit. Also removes from config.

## Event bus

The `EventBus` (`events.rs`) is a `tokio::sync::broadcast` channel that decouples polling from notification dispatch:

- **Producers**: Pollers emit `WatchEvent::RunStarted`, `RunCompleted`, and `StatusChanged`.
- **Consumer**: The notification handler (`run_notification_handler`) subscribes at startup, resolves notification levels from config, checks pause/quiet-hours state, and dispatches to the platform notification backend.

This separation means adding new consumers (logging, webhooks) doesn't require modifying the poller.

## Polling strategy

Each `Poller` task refreshes the shared `RateLimitState` every 60 seconds via `gh api rate_limit`. `compute_intervals` uses the current quota to derive dynamic sleep durations: floor speed (15s/60s, scaled by √calls) above 50% remaining, throttled proportionally below that, waiting out the reset window at zero. Before the first rate-limit fetch, fallback intervals of 30s/120s are used. All pollers share the same `Arc<Mutex<Option<RateLimit>>>` so they coordinate rather than independently consuming quota.

Two functions handle different concerns:

- **`poll_active_runs`** — calls `gh run view` for each run ID in `active_runs`. Detects completion and emits `RunCompleted` (with elapsed time and failing step names for failures). Tracks consecutive API failures per run; evicts a run after `MAX_GH_FAILURES` (5) failures. The watch lock is released during each GitHub API call to avoid holding it across awaits.
- **`check_for_new_runs`** — calls `gh run list` and compares against `last_seen_run_id`. Any run with a higher ID is new: emits `RunStarted`, and immediately `RunCompleted` too if it already finished between polls. Advances `last_seen_run_id` past all unseen runs (including filtered-out ones) to avoid re-checking.

## Configuration and state persistence

All JSON files are written via a crash-safe draft/backup pattern (`save_json` in `config.rs`):

1. Serialize to a `.draft` file and `fsync`.
2. Read the `.draft` back and parse it to confirm it is valid JSON.
3. Rename the current file to `.bak`.
4. Rename `.draft` to the target path.

A crash at any point leaves either the previous file or the backup intact. On load, `load_json` falls back to `.bak` if the primary is missing or unparseable.

**Config** (`~/.config/build-watcher/config.json`) — watched repos, per-repo branch/workflow lists, hierarchical notification level overrides (global → per-repo → per-branch), quiet hours, ignored workflows, and repo aliases. Written only when the user changes settings. On first startup, re-saved to normalize the schema (add missing fields with defaults).

**Watch state** (`~/.local/state/build-watcher/watches.json`) — `last_seen_run_id` and `last_build` per watch key (`owner/repo#branch`). Written after every meaningful state change. Runtime-only fields (`active_runs`, `failure_counts`) are not persisted; they are reconstructed at startup via `startup_watches`.

## Startup recovery

`startup_watches` (called at daemon start) does two things:

1. **Recover existing watches**: For each key in `watches.json`, fetches recent runs concurrently via a `JoinSet`, adds any in-progress ones back to `active_runs`, and advances `last_seen_run_id` to the latest run seen. Without this, `check_for_new_runs` would treat runs from during downtime as new and fire spurious notifications.

2. **Start new config watches**: For each repo in `config.json` that has no entry in `watches.json`, calls `start_watch` to begin fresh.

## Desktop notifications

The `Notifier` trait (`platform/mod.rs`) abstracts the backend. A global `OnceLock` singleton is initialized on first use. The active backend is chosen at startup via platform-specific detection:

- **Linux** — D-Bus via `zbus` (`org.freedesktop.Notifications` interface). Uses `replaces_id` for notification stacking per group, urgency hints, icons, categories, expiry times, and a `desktop-entry` hint for GNOME/KDE grouping. Clicking a notification opens the GitHub Actions run URL via `xdg-open` (using the D-Bus `ActionInvoked` signal with a 10-minute listener timeout).
- **macOS** — `terminal-notifier` if available (supports URL open, grouping, and sound), otherwise `osascript` (AppleScript `display notification` with sound). Both macOS backends reap child processes with a 10-second timeout to prevent zombies.

Notifications are grouped per `repo#branch#workflow` so each workflow slot replaces rather than stacks.

## MCP server

The `BuildWatcher` struct implements 18 MCP tools via `rmcp`'s `#[tool]` / `#[tool_router]` macros. The server uses Streamable HTTP transport in stateless mode over axum. A `StreamableHttpService` wraps the handler with `LocalSessionManager`.

Port binding tries the preferred port (default 8417), falling back to up to 9 consecutive ports. The bound port is written to `~/.local/state/build-watcher/port`.

Tool parameters use a custom `deserialize_string_or_vec` deserializer to handle MCP clients that double-encode JSON arrays as strings.

## Graceful shutdown

On SIGINT (ctrl-c):
1. Cancel the `CancellationToken` — all pollers exit their sleep loops.
2. Close the `TaskTracker` and wait for all poller tasks to complete.
3. Save final watch state to disk.
4. Remove the port file.

## Concurrency notes

- Each watched branch has exactly one `Poller` task. The `Watches` mutex is held only for brief in-memory reads/writes — never across `await` points or GitHub API calls.
- `start_watch` performs a double-checked lock: checks for a duplicate before the `gh` call, makes the network call, then re-checks before inserting. This prevents duplicate pollers if concurrent `watch_builds` calls race for the same key.
- The D-Bus notification backend (`zbus`) is fully async. The `replaces_id` for notification grouping is tracked in an `Arc<Mutex<HashMap>>` keyed by group. Action click handling (for opening URLs) spawns a background task per notification with a 10-minute timeout.
