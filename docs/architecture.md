# Architecture

`build-watcher` is a Rust daemon that monitors GitHub Actions workflows and delivers desktop notifications. It exposes an MCP (Model Context Protocol) server over HTTP so Claude Code can manage watched repos via tool calls.

## High-level flow

```
Claude Code ──HTTP/MCP──► BuildWatcher (MCP server)
                               │
                               ├── config.json  (persisted configuration)
                               ├── watches.json (persisted watch state)
                               └── per-repo polling tasks
                                        │
                                        └── gh CLI ──► GitHub API
                                                           │
                                                    desktop notifications
                                                   (notify-send / osascript)
```

## Source layout

```
src/
├── main.rs          — MCP tool handlers, polling logic, startup
├── config.rs        — Config structs, JSON persistence helpers
├── github.rs        — gh CLI wrappers (gh run list / gh run view)
└── platform/
    ├── mod.rs       — Notifier trait, global singleton, platform dispatch
    ├── universal/   — NullNotifier (used in tests)
    ├── linux/
    │   └── notify_send.rs   — notify-send backend with --replace-id grouping
    └── macos/
        ├── terminal_notifier.rs  — terminal-notifier backend (preferred)
        └── apple_script.rs       — osascript fallback
```

## Key types

| Type | Where | Purpose |
|------|-------|---------|
| `BuildWatcher` | `main.rs` | MCP server; owns `Watches` and `SharedConfig` |
| `Watches` | `main.rs` | `Arc<Mutex<HashMap<key, WatchEntry>>>` — runtime state |
| `WatchEntry` | `main.rs` | Per-branch state: active run IDs, failure counts, last build |
| `Config` | `config.rs` | Persisted configuration: repos, branches, notification levels |
| `RunInfo` | `github.rs` | A single GitHub Actions run returned by `gh` |

## Watch lifecycle

1. **`watch_builds` tool call** → reads branch config, calls `start_watch` per branch.
2. **`start_watch`** → fetches recent runs via `gh run list`, sets `last_seen_run_id` to the highest run ID seen, records any in-progress runs, inserts a `WatchEntry`, then spawns a `poll_repo` task.
3. **`poll_repo` loop** → runs forever until the watch entry is removed:
   - Sleeps `active_poll_secs` (10s) when builds are running, `idle_poll_secs` (60s) when idle.
   - Calls `poll_active_runs` every cycle when active.
   - Calls `check_for_new_runs` once per idle interval regardless of active state.
4. **`stop_watches` tool call** → removes the entry from the map. The polling task detects the missing key on its next iteration and exits — no explicit cancellation is needed.

## Polling strategy

Two functions handle different concerns:

- **`poll_active_runs`** — calls `gh run view` for each run ID already in `active_runs`. Detects completion and sends a notification. Tracks consecutive API failures per run; evicts a run after `MAX_GH_FAILURES` (5) failures.
- **`check_for_new_runs`** — calls `gh run list` and compares against `last_seen_run_id`. Any run with a higher ID is new: sends a "started" notification, and immediately a "completed" notification too if it already finished between polls. Advances `last_seen_run_id` to the highest new ID.

## Configuration and state persistence

All JSON files are written via a crash-safe draft/backup pattern (`save_json` in `config.rs`):

1. Serialize to a `.draft` file.
2. Read the `.draft` back and parse it to confirm it is valid JSON.
3. Rename the current file to `.bak`.
4. Rename `.draft` to the target path.

A crash at any point leaves either the previous file or the backup intact. On load, `load_json` falls back to `.bak` if the primary is missing or unparseable.

**Config** (`~/.config/build-watcher/config.json`) — watched repos, per-repo branch lists, notification level overrides (global → per-repo → per-branch), and poll intervals. Written only when the user changes settings.

**Watch state** (`~/.local/state/build-watcher/watches.json`) — `last_seen_run_id` and `last_build` per watch key (`owner/repo#branch`). Written after every meaningful state change. Runtime-only fields (`active_runs`, `failure_counts`) are not persisted; they are reconstructed at startup via `startup_watches`.

## Startup recovery

`startup_watches` (called at daemon start) does two things:

1. For each key already in `watches.json`: fetches recent runs, adds any in-progress ones back to `active_runs`, and advances `last_seen_run_id` to the latest run seen. The last step is important — without it, `check_for_new_runs` would treat runs that occurred during downtime as brand-new and fire spurious "started" notifications.

2. For each repo in `config.json` that has no entry in `watches.json`: calls `start_watch` to begin fresh.

## Desktop notifications

The `Notifier` trait (`platform/mod.rs`) abstracts the backend. A global `OnceLock` singleton is initialised on first use. The active backend is chosen at startup:

- **Linux** — `notify-send` with `--print-id` / `--replace-id` so each branch gets its own notification slot (old notifications are replaced rather than stacked). The `--wait` / `--action` flags are used to add an "Open" button; a click spawns `xdg-open` with the build URL and waits for it to exit.
- **macOS** — `terminal-notifier` if available (supports URL open and grouping), otherwise `osascript`.

Both macOS backends spawn a background thread that reaps the child process and kills it after 10 seconds if it hasn't exited, to prevent zombie processes from a hung notification daemon.

## Concurrency notes

- Each watched branch has exactly one `poll_repo` task. The `Watches` mutex is held only for brief in-memory reads/writes — never across `await` points or file I/O.
- `start_watch` performs a double-checked lock: it first checks for a duplicate outside the lock (fast path), makes the `gh` network call, then re-checks inside the lock before inserting. This prevents duplicate pollers if two concurrent `watch_builds` calls race for the same key.
- `save_watches` serialises state while holding the lock, then drops the lock before calling `spawn_blocking` for the actual file I/O, keeping the async executor free.
