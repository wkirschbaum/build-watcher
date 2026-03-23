# build-watcher

A background daemon that monitors GitHub Actions builds and sends desktop notifications when builds start and complete. It runs as an [MCP](https://modelcontextprotocol.io/) server, so you can manage watches from Claude Code (or any MCP client).

## What it does

- **Persistently watches repos** — once you add a repo, it stays watched across builds and restarts.
- **Notifies on build start** — get a desktop notification when a new build begins.
- **Notifies on build completion** — success or failure, with a link to the GitHub Actions run.
- **Runs independently** — the daemon runs as a system service, not tied to any Claude Code session.
- **Tracks concurrent builds** — detects and monitors multiple simultaneous builds on the same branch.
- **Polls efficiently** — active builds are polled every 10 seconds; idle repos are checked for new builds every 1 minute.
- **Configurable notification levels** — control urgency per event (started, success, failure) or suppress entirely.
- **Config-driven** — repos, branches, and notification settings are stored in a config file that persists across restarts. The config is normalized on startup, so new fields are automatically added with defaults.

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
3. Creates a default config file at `~/.config/build-watcher/config.json` (if missing).
4. Sets up a system service (launchd on macOS, systemd on Linux) that runs on login.
5. Configures Claude Code's `~/.claude.json` to connect to the MCP server.
6. Adds MCP tool permissions to Claude Code's `~/.claude/settings.json`.

After installation, **restart Claude Code** to pick up the new MCP server.

## Usage

From Claude Code, use natural language:

```
watch floatpays/moneyclub
list my watched builds
stop watching floatpays/moneyclub
```

Or use the MCP tools directly:

| Tool                  | Description                                         |
| --------------------- | --------------------------------------------------- |
| `watch_builds`        | Add repos to watch (owner/repo format)              |
| `stop_watches`        | Remove repos from config and stop watching          |
| `list_watches`        | Show all watched repos and their status             |
| `configure_branches`  | Set custom branches for a specific repo             |
| `set_default_branches`| Change the default branches (applies to all repos)  |
| `get_config`          | Show current configuration                          |
| `test_notification`   | Send a test notification to verify setup            |

## Configuration

The config file lives at `~/.config/build-watcher/config.json` (or `$CONFIGURATION_DIRECTORY/config.json`).

```json
{
  "default_branches": ["main"],
  "notifications": {
    "build_started": "normal",
    "build_success": "normal",
    "build_failure": "critical"
  },
  "repos": {
    "floatpays/benefits": {},
    "floatpays/moneyclub": {
      "branches": ["main", "develop"]
    }
  }
}
```

- **`default_branches`** — branches to watch when a repo has no explicit override (default: `["main"]`).
- **`notifications`** — per-event notification levels. Each event can be set to one of:
  - `"off"` — suppress the notification entirely.
  - `"low"` — subtle notification (Linux: low urgency; macOS: Glass sound).
  - `"normal"` — standard notification (Linux: normal urgency; macOS: Glass sound).
  - `"critical"` — persistent/attention-demanding notification (Linux: critical urgency; macOS: Basso sound).
  - Defaults: `build_started: normal`, `build_success: normal`, `build_failure: critical`.
- **`repos`** — each key is an `owner/repo` to watch. Presence in the map means "watch this repo".
  - If `branches` is empty or omitted, `default_branches` is used.
  - If `branches` is set, only those branches are watched for that repo.

Repos are added/removed automatically when you use `watch_builds` and `stop_watches`. You can also edit the config file directly and restart the service.

On startup, the daemon automatically watches all repos listed in the config. The config file is re-saved on startup to normalize its schema — any missing fields are added with their defaults.

### Environment variables

| Variable                  | Default                               | Description                         |
| ------------------------- | ------------------------------------- | ----------------------------------- |
| `BUILD_WATCHER_PORT`      | `8417`                                | HTTP port for the MCP server        |
| `STATE_DIRECTORY`          | `~/.local/state/build-watcher/`       | Runtime state directory             |
| `CONFIGURATION_DIRECTORY`  | `~/.config/build-watcher/`            | Config directory                    |
| `RUST_LOG`                 | `build_watcher=info`                  | Log level                           |

### Migration

If upgrading from an older version that stored repos only in `watches.json`, the daemon will automatically migrate them into the config on first startup.

## Files

| Path                                        | Purpose                        |
| ------------------------------------------- | ------------------------------ |
| `~/.local/bin/build-watcher`                | Binary                         |
| `~/.config/build-watcher/config.json`       | Configuration (repos, branches)|
| `~/.local/state/build-watcher/watches.json` | Persisted state (last seen run IDs)|
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
2. On startup, it reads `config.json` (normalizing any missing fields) and begins watching all configured repos.
3. For each repo/branch, it fetches the 10 most recent GitHub Actions runs using `gh run list`.
4. It records the highest run ID seen (`last_seen_run_id`) as a high-water mark. Any run with an ID above this is considered new.
5. New in-progress runs are tracked in memory and polled via `gh run view` every 10 seconds until completion.
6. When a build completes, a notification is sent with the appropriate urgency level based on the `notifications` config.
7. Every 1 minute, idle watches re-check `gh run list` for new runs. If a build started and completed between checks, both "started" and "completed" notifications are sent.
8. Multiple concurrent builds on the same branch are tracked independently.
9. The `last_seen_run_id` is persisted to `watches.json` so new-run detection survives restarts. Active run tracking is in-memory only and rediscovered on startup.
