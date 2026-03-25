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
- `src/server.rs` — MCP tool handlers (`BuildWatcher` struct), axum router
- `src/watcher.rs` — Watch lifecycle, `Poller` task, state persistence, rate limiting
- `src/events.rs` — `EventBus` (broadcast channel), `WatchEvent` types, notification handler
- `src/config.rs` — Config structs, crash-safe JSON persistence helpers
- `src/github.rs` — `gh` CLI wrappers, `RunInfo`/`HistoryEntry` types, input validation
- `src/format.rs` — Duration, age, and truncation formatting
- `src/platform/` — `Notifier` trait + backends (Linux: D-Bus via `zbus`; macOS: `terminal-notifier` → `osascript` fallback)

### How it works

The `BuildWatcher` struct implements 18 MCP tools. When a repo is watched, it spawns an async tokio task per repo/branch that polls GitHub via the `gh` CLI. Events are emitted onto a broadcast `EventBus`; a notification handler subscribes and dispatches desktop notifications based on config and pause/quiet-hours state.

**Polling intervals:** Minimum 15s when builds are active, 60s when idle. Intervals scale dynamically based on the GitHub API rate limit. The `gh` CLI must be authenticated (`gh auth login`).

**State persistence:**
- Config: `~/.config/build-watcher/config.json` — repos, branches, notification levels (global → per-repo → per-branch), quiet hours, ignored workflows, aliases
- Watch state: `~/.local/state/build-watcher/watches.json` — last seen run IDs and completed builds
- Actual port: `~/.local/state/build-watcher/port`

**Safe JSON writes:** Config and state are written via draft → verify → rename with automatic backups to prevent corruption.

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
