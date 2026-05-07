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
- **PR watch** -- opt-in per-repo polling of open PRs with color-coded merge-readiness badges (e.g. `[#42✓ #43⊘]`) and notifications when PRs become ready to merge
- **Per-repo config** -- `c` key in TUI to configure alias, watch PRs, and poll aggression per repo
- Per-repo poll aggression override (falls back to global when unset)
- Per-repo workflow filtering and global workflow ignore list
- Quiet hours window for silencing notifications at scheduled times
- Build history summary with duration and age
- Pause/resume notifications temporarily (timed or indefinite)
- Persistent watches that survive restarts
- Tracks multiple concurrent builds on the same branch
- Hierarchical notification levels -- `off`/`low`/`normal`/`critical` per event, per repo, per branch
- Dynamic rate-limit-aware polling with ETag caching -- speeds up when quota is plentiful, backs off as it depletes (minimum 5s). Idle polls return `304 Not Modified` at zero rate-limit cost
- Auto-discover branches with active runs or open pull requests, with optional regex filter
- **MCP server** -- manage watches, rerun builds, and configure notifications from any MCP client
- **Live TUI dashboard** (`bw`) -- top-like terminal UI with real-time SSE updates, sortable columns, grouping, and full watch management
- **Self-update** -- background update checker with in-TUI upgrade (`U`) and `bw --update` CLI flag

## Requirements

- **GitHub authentication** -- either:
  - **GitHub CLI (`gh`)** -- authenticated via `gh auth login`. Install: https://cli.github.com/
  - **`GITHUB_TOKEN` environment variable** -- a personal access token with `repo` and `actions` scopes. Works without `gh` installed.
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
| `watch_from_git_remote` | Auto-detect the GitHub repo from a local git repository's origin remote and start watching |
| `stop_watches` | Remove repos and stop watching |
| `list_watches` | Show all watched repos and their status |
| `configure_branches` | Set branches for a repo, or omit repo to set global defaults. Supports `auto_discover_branches` and `branch_filter` (regex) |
| `configure_repo` | Set per-repo workflow allow-list, display alias, PR watching, branch filter, and ignored events |
| `configure_ignored_events` | Add/remove workflow names (`kind: "workflow"`) or GitHub event types (`kind: "event"`, e.g. `schedule`, `workflow_dispatch`) from the global or per-repo ignore list |
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
build-watcher -- up 2h 15m                    poll 5s [medium]  API 4521 . 5000 (90%)  reset 42m
+----------------------------------------------------------------------------------------------+
| REPO ^              BRANCH    STATUS          WORKFLOW       TITLE             ELAPSED / AGE  |
| floatpays/benefits  main      .. in_progress  CI             Fix login bug     1m 12s         |
| floatpays/moneyclub main      x failure       CI             Update deps       3m ago         |
| wkirschbaum/build.. main      . success       CI             Add TUI           2h ago         |
+----------------------------------------------------------------------------------------------+
 floatpays/moneyclub  .  main  .  failure  .  run 12345  .  failed: Build / Run tests
-[..../jk] nav  [Tab/⇧Tab] expand  |  [a] add  [b] branch  [d] del  [o/O] open  [r/R] rerun  |  [n/N] mute  [p] pause  [h] hist  [H] recent  |  [s/S] sort  [g/G] group  [C] config  |  [q] quit  [Q] stop  [?] hide
```

The **header** shows daemon uptime, current poll interval, API rate limit usage, and status indicators (paused, connecting, update available).

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
| `b` | Set branches for selected repo |
| `d` | Remove selected repo or branch |
| `o` / `O` | Open run in browser / open repo Actions page |
| `r` / `R` | Rerun failed jobs / rerun all jobs |
| `M` | Merge the first PR targeting the selected branch |
| `n` | Toggle mute for selected repo/branch |
| `N` | Open notification level picker (per-event levels) |
| `p` | Toggle global notification pause |
| `h` | Open build history popup for selected item |
| `H` | Toggle the Recent builds panel |
| `t` | Build times for selected repo (avg/min/max by workflow) |
| `T` | Build times across all repos (sorted slowest first) |
| `c` | Edit per-repo config (alias, watch PRs, poll aggression) |
| `C` | Edit global config (ignored workflows, auto-discover, branch filter) |
| `s` / `S` | Cycle sort column forward / backward |
| `g` / `G` | Cycle group-by forward / backward |
| `?` | Toggle help popup |
| `q` | Quit |
| `Q` | Quit and shut down daemon |
| `U` | Quit and run self-update (shown when update available) |
| `Ctrl-C` | Quit |

#### PR Watch

Enable per-repo with the `c` key (repo config → Watch PRs: yes). When enabled, the daemon polls open PRs targeting each watched branch and shows color-coded merge-readiness badges in the branch column (e.g. `[#42✓ #43⊘]`):

| Icon | Color | Meaning |
| --- | --- | --- |
| `✓` | Green | Ready to merge (checks pass, no blocking reviews) |
| `⊘` | Red | Blocked (pending reviews or failing checks) |
| `✗` | Red | Merge conflict |
| `!` | Yellow | Unstable |
| `↓` | Yellow | Behind base branch |
| `~` | Gray suffix | Draft PR |

Multiple open PRs are shown individually. Desktop notifications fire when a PR transitions to "ready to merge". Press `M` on a branch row to open the merge popup — a single PR shows a confirmation dialog, multiple PRs show a picker first. Press `Enter` to confirm or `Esc` to cancel.

## Configuration

Config lives at `~/.config/build-watcher/config.json`:

```json
{
  "poll_aggression": "medium",
  "show_author": true,
  "auto_discover_branches": true,
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
      "watch_prs": true,
      "notifications": {
        "build_started": "off"
      }
    }
  }
}
```

| Field | Description |
| --- | --- |
| `auto_discover_branches` | Automatically discover branches with active runs or open PRs (default: `true`) |
| `branch_filter` | Regex pattern to filter discovered branches (only applies when auto-discover is enabled) |
| `default_branches` | Branch names watched for repos with no per-repo branch config. When empty, only the repo's GitHub default branch is watched. Settable via the `C` key in TUI or `/defaults` REST endpoint. |
| `poll_aggression` | Rate-limit budget usage: `"low"` (<=15%), `"medium"` (<=40%, default), `"high"` (<=80%) |
| `notifications` | Global per-event notification levels |
| `quiet_hours` | Time window (local time, 24h format) during which non-critical notifications are suppressed |
| `ignored_workflows` | Workflow names hidden from the TUI and excluded from notifications |
| `show_author` | Show the commit author and triggering actor in the TUI and notifications (default: `true`). Costs one extra API call per new run |
| `repos` | Per-repo config: `branches`, `workflows` (allow-list), `alias` (display name), `notifications` (overrides), `branch_notifications`, `watch_prs` |

Notification levels: `"off"`, `"low"`, `"normal"`, `"critical"`. Branch overrides take priority over repo overrides, which take priority over global settings.

> **Note:** `discovered_branches` may appear inside per-repo entries in `config.json`. This field is auto-managed by the daemon when `auto_discover_branches` is enabled — do not edit it by hand. It is persisted so discovered branches survive restarts, and pruned automatically when branches are deleted on GitHub.

### Poll aggression tuning

The default `"medium"` aggression targets ≤40% of GitHub's 5000 req/hr rate-limit budget, giving a minimum poll interval of 5 seconds under ideal conditions.

**When to lower to `"low"` (≤15%):**
- You watch many repos (10+) and regularly hit the rate limit
- You share a GitHub token with other tools that also consume API quota
- You only need near-real-time updates during business hours (pair with quiet hours)

**When to raise to `"high"` (≤80%):**
- You need the fastest possible notification latency (e.g. you're actively waiting on a build)
- You watch only a handful of repos and have plenty of quota headroom

**When the rate limit hits 0:** The daemon does not stop — it pauses polling and waits for the limit to reset (GitHub resets every hour). The TUI header shows the remaining quota and reset countdown. No builds are missed; the daemon catches up on the next poll cycle after the reset. To check current usage: `get_stats` via MCP, or read the header bar in `bw`.

### Environment variables

| Variable | Default | Description |
| --- | --- | --- |
| `BUILD_WATCHER_PORT` | `8417` | HTTP port for the default daemon instance (ignored when `--config-dir` is used) |
| `STATE_DIRECTORY` | `~/.local/state/build-watcher/` | Runtime state directory (overridden by `--config-dir`) |
| `CONFIGURATION_DIRECTORY` | `~/.config/build-watcher/` | Config directory (overridden by `--config-dir`) |
| `RUST_LOG` | `build_watcher=info` | Log level |

### Multiple instances

Run separate `build-watcher` instances simultaneously — useful for watching repos across multiple GitHub accounts or for isolated project sets:

```sh
build-watcher --config-dir ~/.config/build-watcher-work
build-watcher --config-dir ~/.config/build-watcher-personal
```

Each instance gets its own `config.json` and a `state/` subdirectory containing the socket, lock, port file, and watch state. Custom instances use an OS-assigned port so there are no collisions. Connect `bw` to a specific instance:

```sh
bw --config-dir ~/.config/build-watcher-work
```

The TUI header shows `build-watcher [work]` when a non-default instance is active. `bw --reset-state --config-dir <path>` resets state for the specified instance.

## REST API

The daemon exposes REST endpoints on the same port for the TUI and other consumers:

| Endpoint | Method | Description |
| --- | --- | --- |
| `/version` | GET | Daemon version and API version |
| `/status` | GET | JSON snapshot of all watches, active runs, and last builds |
| `/stats` | GET | Daemon stats: uptime, poll interval, API rate limit |
| `/events` | GET | SSE stream of watch events (RunStarted, RunCompleted, StatusChanged) |
| `/notifications` | GET | Resolved notification config for `?repo=&branch=` |
| `/notifications` | POST | Mute, unmute, or set per-event levels for a repo/branch |
| `/defaults` | GET | Global config defaults (branches, ignored workflows, auto-discover, branch filter) |
| `/defaults` | POST | Update global config defaults |
| `/repo-config` | GET | Per-repo config for `?repo=owner/name` |
| `/repo-config` | POST | Update per-repo config fields |
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

## Troubleshooting

### No builds appearing / no notifications

1. **Check authentication** — run `gh auth status` to confirm `gh` is authenticated, or verify `GITHUB_TOKEN` is set and has `repo` + `actions` scopes. The daemon logs a clear error on startup if no token is found.
2. **Check the daemon is running** — run `bw` to open the TUI; it auto-starts the daemon. Or check the service: `systemctl --user status build-watcher` (Linux) / `launchctl list com.build-watcher` (macOS).
3. **Check the logs** for errors:
   - Linux: `journalctl --user -u build-watcher -n 50`
   - macOS: `tail -n 50 ~/Library/Logs/build-watcher.log`
   - Set `RUST_LOG=build_watcher=debug` in the service environment for verbose output.

### Rate limit exhausted

When the GitHub API rate limit hits 0, the daemon backs off automatically and resumes polling once the limit resets (typically within an hour). Check `get_stats` from the MCP server or the header in `bw` to see current usage and reset time.

To reduce usage: lower `poll_aggression` to `low` (targets ≤15% of 5000/hr), watch fewer repos, or set `auto_discover_branches: false` to avoid polling for new branches.

### Notifications not appearing (Linux)

Notifications are sent via D-Bus (`org.freedesktop.Notifications`). A notification daemon must be running:

- **GNOME / KDE / XFCE** — built-in, should work out of the box.
- **Standalone WM** (i3, sway, etc.) — install and start `dunst`, `mako`, or `notification-daemon`.
- Test with: `notify-send "test" "hello"` — if that works, build-watcher notifications should too.

### Notifications not clickable (macOS)

By default, notifications use `osascript` and show the GitHub URL in the notification body. Install [`terminal-notifier`](https://github.com/julienXX/terminal-notifier) for clickable links that open directly in the browser:

```sh
brew install terminal-notifier
```

The daemon auto-detects `terminal-notifier` on startup; restart the service after installing.

### Builds showing stale / missing after restart

Run `bw --reset-state` to clear cached run state (active runs and build history) while keeping your config. The daemon will re-fetch current build state on the next poll cycle.

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
