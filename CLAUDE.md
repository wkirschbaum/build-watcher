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

- `src/main.rs` — Entry point, wires up config, watches, event bus, and server
- `src/server/` — Server module directory:
  - `mod.rs` — App state, axum router setup, snapshot building
  - `mcp.rs` — MCP tool handlers (`BuildWatcher` struct)
  - `rest.rs` — REST/SSE endpoints (`/status`, `/stats`, `/events`, `/pause`, `/rerun`)
  - `actions.rs` — MCP tool action implementations
  - `schema.rs` — JSON schema definitions for tool parameters
- `src/watcher/` — Watch lifecycle module directory:
  - `mod.rs` — Type aliases (`Watches`, `SharedConfig`, etc.), `is_paused()`, `count_api_calls()`, `filter_runs()`, re-exports
  - `types.rs` — `WatchKey`, `ActiveRun`, `WatchEntry`, `PersistedWatch`, persistence helpers, `last_failed_build()`
  - `poller.rs` — `Poller` async task, poll loop, active run tracking, new run detection
  - `startup.rs` — `WatcherHandle`, `start_watch()`, `startup_watches()`, recovery logic
  - `tests.rs` — Mock GitHub client, unit and integration tests
- `src/events.rs` — `EventBus` (broadcast channel), `WatchEvent` and `RunSnapshot` types (pure, no I/O)
- `src/config.rs` — Config structs, crash-safe JSON persistence helpers
- `src/github.rs` — `gh` CLI wrappers, `RunInfo`/`HistoryEntry` types, input validation, GitHub URL helpers
- `src/format.rs` — Duration, age, and truncation formatting
- `src/rate_limiter.rs` — API rate-limit budget computation and poll interval scaling
- `src/history.rs` — Build history management (per repo/branch, capped ring buffer)
- `src/persistence.rs` — `Persistence` trait abstraction (file I/O vs. null for tests)
- `src/register.rs` — MCP server registration in `~/.claude.json` (invoked via `--register` flag)
- `src/notification.rs` — Daemon-only notification handler; subscribes to the event bus and dispatches platform notifications
- `src/status.rs` — Shared HTTP response types (`StatusResponse`, `StatsResponse`) used by both daemon and TUI
- `src/bin/bw/` — `bw` TUI dashboard module directory:
  - `main.rs` — Entry point, terminal setup, event loop, daemon discovery
  - `app.rs` — App state, input handling, event application
  - `client.rs` — HTTP client for daemon communication, SSE streaming
  - `render.rs` — TUI rendering, display rows, sorting, grouping
  - `update.rs` — Background update checker, self-update via GitHub releases
- `src/platform/` — `Notifier` trait + backends (Linux: D-Bus via `zbus`; macOS: `terminal-notifier` → `osascript` fallback)

### How it works

The `BuildWatcher` struct implements 10 MCP tools. When a repo is watched, it spawns an async tokio task per repo/branch that polls GitHub via the `gh` CLI. Events are emitted onto a broadcast `EventBus`; a notification handler subscribes and dispatches desktop notifications based on config and pause/quiet-hours state.

**Polling intervals:** Minimum 15s when builds are active, 60s when idle. Intervals scale dynamically based on the GitHub API rate limit. The `gh` CLI must be authenticated (`gh auth login`).

**State persistence:**
- Config: `~/.config/build-watcher/config.json` — repos, branches, notification levels (global → per-repo → per-branch), quiet hours, ignored workflows, aliases
- Watch state: `~/.local/state/build-watcher/watches.json` — last seen run IDs and completed builds
- Actual port: `~/.local/state/build-watcher/port`

**Safe JSON writes:** Config and state are written via draft → verify → rename with automatic backups to prevent corruption.

**Crate layout:** `src/lib.rs` exports `config`, `events`, `format`, `github`, `history`, `persistence`, `rate_limiter`, `status`, `watcher` as the `build_watcher` library crate. The `build-watcher` daemon binary and the `bw` CLI binary both depend on this lib. Daemon-only code (`platform/`, `notification.rs`, `server/`, `register.rs`) stays alongside `main.rs`.

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
