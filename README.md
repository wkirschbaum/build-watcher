# build-watcher

A background daemon that monitors GitHub Actions builds and sends desktop notifications when builds start and complete. It runs as an [MCP](https://modelcontextprotocol.io/) server, so you can manage watches from Claude Code (or any MCP client).

## What it does

- **Persistently watches repos** — once you add a repo, it stays watched across builds and restarts.
- **Notifies on build start** — get a desktop notification when a new build begins on `main`.
- **Notifies on build completion** — success or failure, with a link to the GitHub Actions run.
- **Runs independently** — the daemon runs as a system service, not tied to any Claude Code session.
- **Polls efficiently** — active builds are polled every 10 seconds; idle repos are checked for new builds every 10 minutes.
- **State persists** — watches survive daemon restarts and machine reboots.

## Requirements

- **Rust** (stable) — to build from source. Install via [rustup](https://rustup.rs/).
- **GitHub CLI (`gh`)** — used to query GitHub Actions. Install: https://cli.github.com/
  - Must be authenticated: run `gh auth login` before first use.
- **Claude Code** — or any MCP-compatible client.

### Platform-specific

#### macOS

- **osascript** — pre-installed on all Macs. Used for desktop notifications.
- The installer sets up a **launchd** service that starts on login and auto-restarts on failure.

#### Linux (Ubuntu/Debian)

- **notify-send** — for desktop notifications. Install if not already present:
  ```sh
  sudo apt install libnotify-bin
  ```
- **systemd** — the installer sets up a user service that starts on login and auto-restarts on failure.

## Installation

```sh
git clone <this-repo>
cd build-watcher
./install.sh
```

The install script:

1. Builds the release binary with `cargo build --release`.
2. Installs it to `~/.local/bin/build-watcher`.
3. Sets up a system service (launchd on macOS, systemd on Linux) that runs on login.
4. Configures Claude Code's `~/.claude.json` to connect to the MCP server.
5. Adds MCP tool permissions to Claude Code's `~/.claude/settings.json`.

After installation, **restart Claude Code** to pick up the new MCP server.

## Usage

From Claude Code, use natural language:

```
watch floatpays/moneyclub
list my watched builds
stop watching floatpays/moneyclub
```

Or use the MCP tools directly:

| Tool                | Description                              |
| ------------------- | ---------------------------------------- |
| `watch_builds`      | Add repos to watch (owner/repo format)   |
| `stop_watches`      | Remove repos from the watch list         |
| `list_watches`      | Show all watched repos and their status  |
| `test_notification` | Send a test notification to verify setup |

## Configuration

| Setting              | Default                               | Override                            |
| -------------------- | ------------------------------------- | ----------------------------------- |
| HTTP port            | `8417`                                | `BUILD_WATCHER_PORT` env var        |
| State directory      | `~/.local/state/build-watcher/`       | `STATE_DIRECTORY` env var           |
| Log level            | `build_watcher=info`                  | `RUST_LOG` env var                  |

## Files

| Path                                        | Purpose                        |
| ------------------------------------------- | ------------------------------ |
| `~/.local/bin/build-watcher`                | Binary                         |
| `~/.local/state/build-watcher/watches.json` | Persisted watch list           |
| (macOS) `~/Library/LaunchAgents/com.build-watcher.plist` | launchd service |
| (Linux) `~/.config/systemd/user/build-watcher.service`   | systemd service |

## Managing the service

### macOS

```sh
# View logs
tail -f ~/Library/Logs/build-watcher.log

# Restart
launchctl kickstart -k "gui/$(id -u)/com.build-watcher"

# Stop
launchctl bootout "gui/$(id -u)" ~/Library/LaunchAgents/com.build-watcher.plist
```

### Linux

```sh
# View logs
journalctl --user -u build-watcher -f

# Restart
systemctl --user restart build-watcher

# Stop
systemctl --user stop build-watcher

# Status
systemctl --user status build-watcher
```

## Updating

Re-run the install script — it stops the service, rebuilds, installs the new binary, and restarts:

```sh
./install.sh
```

## How it works

1. The daemon starts an HTTP server on `127.0.0.1:8417` serving MCP over Streamable HTTP.
2. When you add a repo via `watch_builds`, it checks the latest GitHub Actions run on `main` using `gh run list`.
3. If a build is in progress, it polls `gh run view` every 10 seconds until completion, then sends a notification.
4. After a build completes (or if the latest was already done), it enters idle mode and checks `gh run list` every 10 minutes for new builds.
5. When a new build is detected, it sends a "build started" notification and switches back to active polling.
6. All watches are saved to `watches.json` on disk, so they survive restarts.
