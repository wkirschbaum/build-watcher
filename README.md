# build-watcher

A background daemon that monitors GitHub Actions builds and sends desktop notifications when builds start and complete. Runs as an [MCP](https://modelcontextprotocol.io/) server so you can manage it directly from Claude Code.

## Features

- Desktop notifications on build start, success, and failure — with a direct link to the run
- Notification titles formatted as `status: project | workflow` (e.g. `✅ succeeded: build-watcher | CI`)
- Short repo names in notifications — org prefix omitted when the name is unambiguous
- Build duration shown in completion notifications
- Failing job/step context included in failure notifications
- PR titles displayed for pull request events
- Per-repo workflow filtering and global workflow ignore list
- Quiet hours window for silencing notifications at scheduled times
- Rerun failed builds directly from Claude Code
- Build history summary with duration and age
- Pause/resume notifications temporarily
- Persistent watches that survive restarts
- Tracks multiple concurrent builds on the same branch
- Configurable notification urgency per event, per repo, or per branch
- Dynamic rate-limit-aware polling — speeds up when quota is plentiful, backs off as it depletes (minimum 15s active, 60s idle)

## Requirements

- **Rust** — to build from source. Install via [rustup](https://rustup.rs/).
- **GitHub CLI (`gh`)** — must be authenticated (`gh auth login`). Install: https://cli.github.com/
- **Claude Code** — or any MCP-compatible client.

#### Linux

- A running notification daemon (GNOME Shell, KDE Plasma, or `notification-daemon`) — notifications are sent via D-Bus (`org.freedesktop.Notifications`).
- `systemd` — the installer sets up a user service.

#### macOS

- `osascript` — pre-installed. Optionally install `terminal-notifier` for richer notifications.
- The installer sets up a launchd service.

## Installation

```sh
git clone <this-repo>
cd build-watcher
./install.sh
```

This builds the binary, installs it to `~/.local/bin/`, creates a default config, registers a system service, and configures Claude Code's MCP settings. **Restart Claude Code** after installing.

## Usage

From Claude Code, use natural language:

```
watch wkirschbaum/build-watcher
list my watched builds
stop watching wkirschbaum/build-watcher
```

Or call the MCP tools directly:

| Tool | Description |
| --- | --- |
| `watch_builds` | Add repos to watch (`owner/repo` format) |
| `stop_watches` | Remove repos and stop watching |
| `list_watches` | Show all watched repos and their status |
| `configure_branches` | Set branches for a repo, or omit repo to set global defaults |
| `configure_repo` | Set per-repo workflow allow-list and/or display alias |
| `ignore_workflows` | Globally ignore workflows (e.g. Semgrep, Dependabot) |
| `unignore_workflows` | Stop ignoring workflows |
| `update_notifications` | Set levels, quiet hours, and pause/resume in one call |
| `rerun_build` | Rerun a failed build (specific ID or last failed) |
| `build_history` | Show recent builds for a repo with duration and age |
| `get_stats` | Show live stats (uptime, rate limit, polling, pause state, config path) |

## Configuration

Config lives at `~/.config/build-watcher/config.json`:

```json
{
  "default_branches": ["main"],
  "notifications": {
    "build_started": "normal",
    "build_success": "normal",
    "build_failure": "critical"
  },
  "ignored_workflows": ["Semgrep"],
  "repos": {
    "wkirschbaum/build-watcher": {
      "branches": ["main"],
      "workflows": ["CI"]
    },
    "wkirschbaum/elixir-ts-mode": {
      "branches": ["main", "release"],
      "notifications": {
        "build_started": "off"
      },
      "branch_notifications": {
        "release": {
          "notifications": {
            "build_started": "off",
            "build_success": "normal",
            "build_failure": "off"
          }
        }
      }
    }
  }
}
```

Notification levels: `"off"`, `"low"`, `"normal"`, `"critical"`. Branch overrides take priority over repo overrides, which take priority over global settings.

### Environment variables

| Variable | Default | Description |
| --- | --- | --- |
| `BUILD_WATCHER_PORT` | `8417` | HTTP port for the MCP server |
| `STATE_DIRECTORY` | `~/.local/state/build-watcher/` | Runtime state directory |
| `CONFIGURATION_DIRECTORY` | `~/.config/build-watcher/` | Config directory |
| `RUST_LOG` | `build_watcher=info` | Log level |

## Managing the service

### Linux

```sh
journalctl --user -u build-watcher -f   # logs
systemctl --user restart build-watcher
systemctl --user stop build-watcher
systemctl --user status build-watcher
```

### macOS

```sh
tail -f ~/Library/Logs/build-watcher.log
launchctl kickstart -k "gui/$(id -u)/com.build-watcher"
launchctl bootout "gui/$(id -u)" ~/Library/LaunchAgents/com.build-watcher.plist
```

## Updating

```sh
./install.sh
```

Re-running the install script stops the service, rebuilds, and restarts.
