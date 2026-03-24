# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Overview

`build-watcher` is a Rust daemon that monitors GitHub Actions builds and sends desktop notifications. It runs as an MCP (Model Context Protocol) server, exposing tools to Claude Code for managing watched repos.

## Commands

```bash
cargo build --release       # Build release binary
cargo build                 # Build debug binary
cargo test --verbose        # Run tests
./install.sh                # Build, install, and configure as a system service
```

**Environment variables:**
- `BUILD_WATCHER_PORT` (default: `8417`) — HTTP port for the MCP server
- `STATE_DIRECTORY` (default: `~/.local/state/build-watcher/`) — runtime state
- `CONFIGURATION_DIRECTORY` (default: `~/.config/build-watcher/`) — config dir
- `RUST_LOG` (default: `build_watcher=info`) — log level

## Architecture

### Key files

- `src/main.rs` — MCP server setup, tool handlers, GitHub polling logic (~1000 lines)
- `src/config.rs` — Config struct, JSON persistence with safe draft/backup pattern
- `src/platform/` — Trait-based desktop notification abstraction (Linux: `notify-send`, macOS: `terminal-notifier` → `osascript` fallback)

### How it works

The `BuildWatcher` struct implements 8 MCP tools (`watch_builds`, `stop_watches`, `list_watches`, `configure_branches`, `set_default_branches`, `configure_notifications`, `get_config`, `test_notification`). When a repo is watched, it spawns an async tokio task per repo that polls GitHub via the `gh` CLI.

**Polling intervals:** 10 seconds when builds are active, 60 seconds when idle. The `gh` CLI must be authenticated (`gh auth login`).

**State persistence:**
- Config: `~/.config/build-watcher/config.json` — hierarchical notification settings (global → per-repo → per-branch)
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
- `serde_json` — config and state serialization
