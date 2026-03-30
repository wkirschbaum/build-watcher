# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Overview

`build-watcher` is a Rust daemon that monitors GitHub Actions builds and sends desktop notifications. It runs as an MCP (Model Context Protocol) server, exposing tools to Claude Code for managing watched repos.

## Commands

```bash
cargo build --release       # Build release binary
cargo build                 # Build debug binary
cargo test --verbose        # Run tests
cargo fmt                   # Format code
cargo clippy                # Lint
./install.sh                # Build, install, and configure as a system service
```

**Before every commit:** run `cargo fmt && cargo clippy && cargo test` and fix all issues first.

**Environment variables:**
- `BUILD_WATCHER_PORT` (default: `8417`) — HTTP port for the MCP server
- `STATE_DIRECTORY` (default: `~/.local/state/build-watcher/`) — runtime state
- `CONFIGURATION_DIRECTORY` (default: `~/.config/build-watcher/`) — config dir
- `RUST_LOG` (default: `build_watcher=info`) — log level

## Architecture

### Key files

- `src/bin/build-watcher/main.rs` — Daemon entry point, wires up config, watches, event bus, and server
- `src/bin/build-watcher/server/` — Server module directory:
  - `mod.rs` — `DaemonState`, axum router setup, `build_watch_snapshot()`, instance lock, `serve()`
  - `mcp.rs` — MCP tool handlers (`BuildWatcher` struct, 11 tools)
  - `rest.rs` — REST/SSE endpoints (`/status`, `/stats`, `/events`, `/pause`, `/rerun`, etc.)
  - `actions.rs` — MCP tool action implementations, `persist_config()`
  - `schema.rs` — JSON schema definitions for tool parameters
- `src/bin/build-watcher/notification.rs` — Notification handler; subscribes to event bus, debounces, coalesces, throttles, and dispatches desktop notifications
- `src/bin/build-watcher/register.rs` — MCP server registration in `~/.claude.json` (invoked via `--register` flag)
- `src/bin/build-watcher/platform/` — `Notifier` trait + backends:
  - `mod.rs` — Platform detection, `Notification` struct, `send()` singleton
  - `linux/mod.rs` — D-Bus via `zbus` (`org.freedesktop.Notifications`)
  - `macos/mod.rs` — `terminal-notifier` (preferred, clickable links) → `osascript` fallback (URL in body)
- `src/bin/bw/` — `bw` TUI dashboard:
  - `main.rs` — Entry point, terminal setup, event loop, daemon discovery
  - `app.rs` — App state, event application, sort/group enums, terminal title
  - `input.rs` — Keyboard input handling
  - `render.rs` — TUI rendering, display rows, sorting, grouping, detail bar, history popup
  - `client.rs` — HTTP client for daemon communication, SSE streaming
  - `forms.rs` — Form/picker UI components
  - `update.rs` — Background update checker, self-update via GitHub releases
- `src/config/` — Configuration module:
  - `mod.rs` — `load_and_normalize()`, `save_config()`, lenient recovery, draft promotion
  - `types.rs` — `Config`, `RepoConfig`, `NotificationLevel`, `PollAggression`, `QuietHours`
  - `resolve.rs` — `notifications_for()` resolution (global → repo → branch), quiet hours check
- `src/watcher/` — Watch lifecycle module:
  - `mod.rs` — Type aliases (`Watches`, `SharedConfig`, etc.), `is_paused()`, `count_api_calls()`, `filter_runs()`, re-exports
  - `types.rs` — `WatchKey`, `ActiveRun`, `WatchEntry`, `PersistedWatch`, persistence helpers, `last_failed_build()`
  - `repo_poller.rs` — `RepoPoller` async task, poll loop, active run tracking, new run detection, dead repo removal
  - `startup.rs` — `WatcherHandle`, `start_watch()`, `startup_watches()`, recovery logic
  - `tests.rs` — Mock GitHub client, unit and integration tests
- `src/events.rs` — `EventBus` (broadcast channel), `WatchEvent` and `RunSnapshot` types (pure, no I/O)
- `src/github.rs` — `gh` CLI wrappers, `RunInfo`/`HistoryEntry` types, input validation, GitHub URL helpers
- `src/format.rs` — Duration, age, and truncation formatting
- `src/rate_limiter.rs` — API rate-limit budget computation and poll interval scaling
- `src/history.rs` — Build history management (per repo/branch, capped ring buffer)
- `src/persistence.rs` — `Persistence` trait, crash-safe `save_json`/`load_json`, draft recovery (`recover_draft`)
- `src/status.rs` — Shared HTTP response types (`StatusResponse`, `StatsResponse`) used by both daemon and TUI
- `src/dirs.rs` — `config_dir()` and `state_dir()` helpers

### How it works

The `BuildWatcher` struct implements 11 MCP tools. When a repo is watched, it spawns an async tokio task per repo that polls GitHub via the `gh` CLI. Events are emitted onto a broadcast `EventBus`; a notification handler subscribes, debounces (3s per repo/branch/kind), coalesces multiple workflows into summary notifications, throttles (10/60s), and dispatches desktop notifications based on config and pause/quiet-hours state.

**Polling intervals:** Minimum 15s when builds are active, 60s when idle. Intervals scale dynamically based on the GitHub API rate limit. The `gh` CLI must be authenticated (`gh auth login`).

**State persistence:**
- Config: `~/.config/build-watcher/config.json` — repos, branches, notification levels (global → per-repo → per-branch), quiet hours, ignored workflows, aliases
- Watch state: `~/.local/state/build-watcher/watches.json` — last seen run IDs and completed builds
- Actual port: `~/.local/state/build-watcher/port`

**Safe JSON writes:** Config and state are written via draft → fsync → verify → rename with automatic backups to prevent corruption. On load, orphaned `.draft` files from interrupted saves are automatically promoted before falling back to `.bak` backups.

**Crate layout:** `src/lib.rs` exports `config`, `dirs`, `events`, `format`, `github`, `history`, `persistence`, `rate_limiter`, `status`, `watcher` as the `build_watcher` library crate. The `build-watcher` daemon binary and the `bw` CLI binary both depend on this lib. Daemon-only code (`platform/`, `notification.rs`, `server/`, `register.rs`) lives under `src/bin/build-watcher/`.

## Design Principles

- **Pure functions low, I/O high.** Business logic and data transformations should be pure functions with no I/O or side effects. I/O (file reads, network calls, OS notifications) belongs as high up the call stack as possible — at the boundary layers (`server.rs`, `notification.rs`, polling tasks), not buried in core types.
- **Shared logic in the library crate.** If code is needed by both the daemon and the TUI/CLI, it belongs in `src/lib.rs` (and its sub-modules). Daemon-specific code (MCP server, platform notifications, service registration) stays binary-only.

**Notification levels:** `off`, `low`, `normal`, `critical` — configurable globally, per repo, or per branch.

### Service setup

`install.sh` installs the binary to `~/.local/bin/`, creates config, and registers a service:
- Linux: systemd user service (`~/.config/systemd/user/build-watcher.service`)
- macOS: launchd agent (`~/Library/LaunchAgents/com.build-watcher.plist`)

It also registers the MCP server in `~/.claude.json`.

## Dependencies

- `rmcp` — MCP server framework with HTTP transport
- `tokio` + `axum` — async runtime and HTTP
- `schemars` — JSON Schema generation for tool parameters
- `serde` + `serde_json` — serialization and config/state persistence
- `thiserror` — error type derivation
- `chrono` — local time for quiet hours
- `zbus` (Linux) — D-Bus notifications via `org.freedesktop.Notifications`
- `tracing` + `tracing-subscriber` — structured logging
