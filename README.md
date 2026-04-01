# build-watcher

A background daemon that monitors GitHub Actions builds and sends desktop notifications when builds start and complete. Exposes an [MCP](https://modelcontextprotocol.io/) server so you can manage it from any MCP-compatible client, plus a live TUI dashboard for at-a-glance monitoring.

![TUI Dashboard](screenshots/tui.png)

## Features

- Desktop notifications on build start, success, and failure with a direct link to the run
- Notification titles formatted as `status: project | workflow` (e.g. `✅ succeeded: build-watcher | CI`)
- Short repo names in notifications -- org prefix omitted when the name is unambiguous
- Build duration shown in completion notifications
- Failing job/step context included in failure notifications
- PR titles displayed for pull request events; compact event prefixes (`PR:`, `cron:`, `manual:`)
- **PR watch** -- opt-in per-repo polling of open PRs with merge-readiness badges (`PR:✓`/`PR:⊘`/`PR:✗`) and notifications when PRs become ready to merge
- **Per-repo config** -- `c` key in TUI to configure alias, watch PRs, and poll aggression per repo
- Per-repo poll aggression override (falls back to global when unset)
- Per-repo workflow filtering and global workflow ignore list
- Quiet hours window for silencing notifications at scheduled times
- Build history summary with duration and age
- Pause/resume notifications temporarily (timed or indefinite)
- Persistent watches that survive restarts
- Tracks multiple concurrent builds on the same branch
- Hierarchical notification levels -- `off`/`low`/`normal`/`critical` per event, per repo, per branch
- Dynamic rate-limit-aware polling -- speeds up when quota is plentiful, backs off as it depletes (minimum 15s active, 60s idle)
- Auto-discover branches with active runs, with optional regex filter
- **MCP server** -- manage watches, rerun builds, and configure notifications from any MCP client
- **Live TUI dashboard** (`bw`) -- top-like terminal UI with real-time SSE updates, sortable columns, grouping, and full watch management
- **Self-update** -- background update checker with in-TUI upgrade (`U`) and `bw --update` CLI flag

## Requirements

- **GitHub CLI (`gh`)** -- must be authenticated (`gh auth login`). Install: https://cli.github.com/
- **Rust** -- only needed if building from source. Install via [rustup](https://rustup.rs/).

#### Linux

- A running notification daemon (GNOME Shell, KDE Plasma, or `notification-daemon`) -- notifications are sent via D-Bus (`org.freedesktop.Notifications`).
- `systemd` -- the installer sets up a user service.

#### macOS

- `osascript` -- pre-installed. Notifications are sent via AppleScript; the GitHub link is shown in the notification body.
- Optionally install [`terminal-notifier`](https://github.com/julienXX/terminal-notifier) (`brew install terminal-notifier`) for clickable notification links that open directly in the browser.
- The installer sets up a launchd service.

## Installation

```sh
curl -fsSL https://raw.githubusercontent.com/wkirschbaum/build-watcher/main/install.sh | bash
```

Or clone the repo and run `./install.sh` manually. The script downloads pre-built binaries from the latest GitHub release for your platform (Linux x86_64/aarch64, macOS x86_64/aarch64), installs them to `~/.local/bin/`, creates a default config, registers a system service, and configures the MCP server in `~/.claude.json`.

To install from source without cloning the repo:

```sh
cargo install --git https://github.com/wkirschbaum/build-watcher.git
```

This builds and installs both binaries to `~/.cargo/bin/`. Note: this skips service registration and MCP setup -- run `build-watcher --register --port 8417` afterwards to configure the MCP server.

To build and install from a local checkout (useful during development):

```sh
./install.sh --local
```

This runs `cargo build --release` and installs the resulting binaries with full service and MCP setup.

## Usage

### MCP Server

Once installed, the MCP server is registered in `~/.claude.json` and available to any MCP-compatible client. From Claude Code, use natural language to manage your builds:

![MCP Usage in Claude Code](screenshots/mcp.png)

| Tool | Description |
| --- | --- |
| `watch_builds` | Add repos to watch (`owner/repo` format) |
| `stop_watches` | Remove repos and stop watching |
| `list_watches` | Show all watched repos and their status |
| `configure_branches` | Set branches for a repo, or omit repo to set global defaults. Supports `auto_discover_branches` and `branch_filter` (regex) |
| `configure_repo` | Set per-repo workflow allow-list and/or display alias |
| `configure_ignored_workflows` | Add/remove from the global workflow ignore list |
| `update_notifications` | Set levels, quiet hours, and pause/resume in one call |
| `rerun_build` | Rerun a failed build (specific ID or last failed) |
| `build_history` | Show recent builds for a repo with duration and age |
| `get_stats` | Show live stats (uptime, rate limit, polling, pause state, config path) |
| `set_poll_aggression` | Set how much of the GitHub rate-limit budget the daemon uses per hour (`low`/`medium`/`high`) |

### TUI Dashboard

Run `bw` for a live terminal dashboard (auto-starts the daemon if it isn't running):

```sh
bw
```

```
build-watcher -- up 2h 15m                    poll 15s . 60s [medium]  API 4521 . 5000 (90%)  reset 42m
+----------------------------------------------------------------------------------------------+
| REPO ^              BRANCH    STATUS          WORKFLOW       TITLE             ELAPSED / AGE  |
| floatpays/benefits  main      .. in_progress  CI             Fix login bug     1m 12s         |
| floatpays/moneyclub main      x failure       CI             Update deps       3m ago         |
| wkirschbaum/build.. main      . success       CI             Add TUI           2h ago         |
+----------------------------------------------------------------------------------------------+
 floatpays/moneyclub  .  main  .  failure  .  run 12345  .  failed: Build / Run tests
-[..../jk] nav  [Tab/⇧Tab] expand  |  [a] add  [b] branch  [d] del  [o/O] open  [r/R] rerun  |  [n/N] mute  [p] pause  [h] hist  [H] recent  |  [s/S] sort  [g/G] group  [C] config  |  [q] quit  [Q] stop  [?] hide
```

The **header** shows daemon uptime, current poll intervals, API rate limit usage, and status indicators (paused, connecting, update available).

The **detail bar** below the table shows contextual information for the selected row -- repo/branch status summary, run ID, failing steps, duration, and age.

#### Expand and Collapse

Repos can be expanded to three levels:

- **Collapsed** -- repo header only (one row per repo)
- **Branches** -- repo + branch rows
- **Full** -- repo + branch + per-workflow detail rows (default)

Use `Tab`/`Enter` to cycle expand level on the selected row, or `Shift-Tab` to cycle all repos at once. On a repo row, it cycles Collapsed → Branches → Full. On a branch row, it toggles workflow visibility. On workflow rows, it does nothing. Expand state is persisted across sessions.

#### Sorting and Grouping

**Sort columns:** repo, branch, status, workflow, age (cycle with `s`/`S`)

**Group-by modes:** org (default), branch, workflow, status, none (cycle with `g`/`G`)

#### Keybindings

| Key | Action |
| --- | --- |
| `Up`/`Down` or `j`/`k` | Navigate rows |
| `Tab` / `Enter` | Cycle expand level (repo: Collapsed → Branches → Full; branch: toggle workflows) |
| `Shift-Tab` / `E` | Cycle expand level for all repos |
| `a` | Add a repo to watch |
| `d` | Remove selected repo or branch |
| `b` | Set branches for selected repo |
| `r` / `R` | Rerun failed jobs / all jobs for selected build |
| `M` | Merge the first PR targeting the selected branch |
| `o` | Open failed job or current run in browser |
| `O` | Open repo Actions page in browser |
| `n` | Toggle mute for selected repo/branch |
| `N` | Open notification level picker (per-event levels) |
| `h` | Open build history popup for selected item |
| `H` | Toggle the Recent builds panel |
| `p` | Toggle global notification pause |
| `s` / `S` | Cycle sort column forward / backward |
| `g` / `G` | Cycle group-by forward / backward |
| `C` | Edit global config (default branches, ignored workflows, auto-discover, branch filter) |
| `?` | Toggle help bar |
| `q` | Quit |
| `Q` | Quit and shut down daemon |
| `U` | Quit and run self-update (shown when update available) |
| `Ctrl-C` | Quit |

## Configuration

Config lives at `~/.config/build-watcher/config.json`:

```json
{
  "default_branches": ["main"],
  "auto_discover_branches": false,
  "branch_filter": null,
  "poll_aggression": "medium",
  "notifications": {
    "build_started": "normal",
    "build_success": "normal",
    "build_failure": "critical"
  },
  "quiet_hours": {
    "start": "22:00",
    "end": "07:00"
  },
  "ignored_workflows": ["Semgrep"],
  "repos": {
    "wkirschbaum/build-watcher": {
      "branches": ["main"],
      "workflows": ["CI"]
    },
    "wkirschbaum/elixir-ts-mode": {
      "alias": "ts-mode",
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

| Field | Description |
| --- | --- |
| `default_branches` | Branches watched when a repo has no explicit branch config (default: `["main"]`) |
| `auto_discover_branches` | Automatically discover branches with active runs (default: `false`) |
| `branch_filter` | Regex pattern to filter discovered branches (only applies when auto-discover is enabled) |
| `poll_aggression` | Rate-limit budget usage: `"low"` (<=10%), `"medium"` (<=40%, default), `"high"` (<=80%) |
| `notifications` | Global per-event notification levels |
| `quiet_hours` | Time window (local time, 24h format) during which non-critical notifications are suppressed |
| `ignored_workflows` | Workflow names hidden from the TUI and excluded from notifications |
| `repos` | Per-repo config: `branches`, `workflows` (allow-list), `alias` (display name), `notifications` (overrides), `branch_notifications` |

Notification levels: `"off"`, `"low"`, `"normal"`, `"critical"`. Branch overrides take priority over repo overrides, which take priority over global settings.

### Environment variables

| Variable | Default | Description |
| --- | --- | --- |
| `BUILD_WATCHER_PORT` | `8417` | HTTP port for the MCP server |
| `STATE_DIRECTORY` | `~/.local/state/build-watcher/` | Runtime state directory |
| `CONFIGURATION_DIRECTORY` | `~/.config/build-watcher/` | Config directory |
| `RUST_LOG` | `build_watcher=info` | Log level |

## REST API

The daemon exposes REST endpoints on the same port for the TUI and other consumers:

| Endpoint | Method | Description |
| --- | --- | --- |
| `/status` | GET | JSON snapshot of all watches, active runs, and last builds |
| `/stats` | GET | Daemon stats: uptime, polling intervals, API rate limit |
| `/events` | GET | SSE stream of watch events (RunStarted, RunCompleted, StatusChanged) |
| `/notifications` | GET | Resolved notification config for `?repo=&branch=` |
| `/notifications` | POST | Mute, unmute, or set per-event levels for a repo/branch |
| `/defaults` | GET | Global config defaults (branches, ignored workflows, auto-discover, branch filter) |
| `/defaults` | POST | Update global config defaults |
| `/history` | GET | Build history for a repo (`?repo=&branch=&limit=`) |
| `/history/all` | GET | Recent builds across all repos (`?limit=`) |
| `/watch` | POST | Add a repo to watches |
| `/unwatch` | POST | Remove a repo from watches |
| `/branches` | POST | Update branch config for a repo |
| `/pause` | POST | Toggle notification pause |
| `/rerun` | POST | Rerun a build by repo and run ID |
| `/merge` | POST | Merge a PR by repo and PR number |
| `/shutdown` | POST | Graceful daemon shutdown |

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

From the TUI, press `U` when an update is available. Or run:

```sh
bw --update
```

This downloads and installs the latest release. Alternatively, re-run `./install.sh` to upgrade from a GitHub release.

To reset watch state (clears active runs and build history, keeps config):

```sh
bw --reset-state
```

## Uninstalling

```sh
./uninstall.sh
```

Stops the service, removes binaries and the MCP registration. Config and state files are preserved.
