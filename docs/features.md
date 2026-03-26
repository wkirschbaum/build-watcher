# Features

## GitHub Actions Build Monitoring

Watches GitHub Actions workflow runs for configured repositories by polling the `gh` CLI. Each repo/branch combination gets its own async polling task. The poller uses two speeds: fast polling (minimum 15s) when builds are actively running, and slow polling (minimum 60s) when idle. Fallback intervals before the first rate-limit fetch are 30s active / 120s idle. New workflow runs are detected by tracking a high-water mark of the most recent run ID seen per repo/branch. On startup, persisted watches are recovered from disk and in-progress runs are re-discovered from GitHub so no build completions are missed across daemon restarts.

## Desktop Notifications

Sends native desktop notifications when builds start and complete. The notification title uses the format `status: project | workflow` — for example:

```
🔨 started: build-watcher | CI
✅ succeeded: build-watcher | CI
❌ failed: build-watcher | CI
```

The project name is shortened to just the repo name (e.g. `build-watcher`) when it is unambiguous across all watched repos. If two watched repos share the same name (e.g. `foo/bar` and `zoo/bar`), the full `owner/repo` is shown instead. The notification body contains the branch, commit title (short SHA for push events, or "PR: title" for pull requests), elapsed time, failing step names for failures, and a link to the GitHub Actions run.

Notifications are grouped per `repo#branch#workflow` so each workflow slot replaces the previous notification rather than stacking.

On Linux, notifications are sent via D-Bus (`org.freedesktop.Notifications` interface, using the `zbus` crate). Uses `replaces_id` for notification replacement, urgency hints, icons, categories, expiry times, and a `desktop-entry` hint for GNOME/KDE grouping. Clicking a notification opens the GitHub Actions run URL via `xdg-open`.

On macOS, the preferred backend is `terminal-notifier` (supports URL open and notification grouping), with a fallback to `osascript` (AppleScript `display notification`). Child processes are reaped with a 10-second timeout to prevent zombies.

## Hierarchical Notification Configuration

Notification levels (`off`, `low`, `normal`, `critical`) are configurable per event type (`build_started`, `build_success`, `build_failure`) at three scopes: global defaults, per-repo overrides, and per-branch overrides. Resolution follows branch > repo > global priority — a branch override wins over a repo override, which wins over the global default. Only the events you specify are changed; others inherit from the parent scope.

Levels can be configured via the MCP `update_notifications` tool, the REST `/notifications` endpoint, or interactively from the TUI using the notification level picker (`N` key).

## Notification Pause and Resume

Notifications can be temporarily paused for a specified number of minutes or indefinitely until manually resumed or the daemon restarts. While paused, builds continue to be tracked and state is updated — only the desktop notification dispatch is suppressed. The pause state is visible in `list_watches` and `get_config` output.

## Workflow Filtering

Two complementary filters control which workflows trigger notifications:

- **Per-repo allow-list**: Only track workflows matching the specified names (case-insensitive). An empty list means all workflows. Set via `configure_workflows`.
- **Global ignore-list**: Globally suppress workflows by name across all repos (case-insensitive). Useful for noisy workflows like Semgrep or Dependabot. Managed via `ignore_workflows` / `unignore_workflows`.

Both filters apply at poll time — the ignore-list is checked first, then the allow-list.

## Failing Step Detection

When a build fails, the daemon fetches the run's job details from GitHub to identify which specific job and step failed. The failing step names (formatted as "Job / Step") are included in the desktop notification body, giving immediate visibility into what broke without opening the browser.

## Build Rerun

Reruns a GitHub Actions workflow run directly from Claude Code. Specify a run ID, or omit it to automatically find and rerun the most recent failed build across all watched branches of a repo. Supports rerunning only the failed jobs (`failed_only`) for faster iteration.

## Build History

Displays a formatted table of recent builds for a repo, showing conclusion, workflow name, commit title, duration, and relative age. Optionally filter by branch. When multiple branches are present and no branch filter is applied, the branch column is shown. Durations and ages are computed from GitHub's `createdAt`/`updatedAt` timestamps using a built-in ISO 8601 parser (avoiding the overhead of a full datetime library for this specific use case).

## Dynamic Rate-Limit-Aware Polling

Polling intervals adapt in real time to the GitHub API rate limit. Each poller refreshes the shared rate-limit state every 60 seconds via `gh api rate_limit` (a free call). The daemon computes intervals based on remaining quota:

- **Above 50% remaining** — poll at floor speed, scaled by the square root of total API calls per cycle (15s active / 60s idle for 1 call, scaling gently with more watches).
- **Below 50% remaining** — throttle: spread the remaining budget evenly across the seconds until the reset window expires, floored at the minimum values. At zero remaining, wait out the full reset window.

This keeps polling fast when quota is plentiful and backs off gracefully as it depletes, without ever hitting a hard API cap. The rate-limit state is shared across all pollers so they coordinate rather than each independently consuming quota.

## Branch Configuration

Per-repo branch lists override the global default branches (default: `["main"]`). `configure_branches` handles both: omit `repo` to set the global defaults, or pass `repo` to override for a specific repo. Changes to branch configuration require restarting watches to take effect.

## Persistent Configuration

All settings are stored in `~/.config/build-watcher/config.json` with crash-safe writes: data is serialized to a `.draft` file, fsynced, verified by re-parsing, then atomically renamed over the primary. A `.bak` backup of the previous config is kept. On load, if the primary is corrupt or missing, the backup is transparently recovered. On first startup, the config is re-saved to normalize the schema (adding any missing fields with defaults).

## Persistent Watch State

Active watches and their last-seen run IDs are persisted to `~/.local/state/build-watcher/watches.json` using the same crash-safe write pattern. On daemon startup, persisted watches are recovered, in-progress runs are re-discovered from GitHub, and polling resumes. Repos in config that don't have persisted watch state yet are automatically started.

## Event Bus Architecture

An internal broadcast channel decouples the polling loop from notification dispatch. Pollers emit typed events (`RunStarted`, `RunCompleted`, `StatusChanged`) onto the bus. A dedicated notification handler subscribes and dispatches desktop notifications + sound based on the current configuration and pause state. This separation allows future subscribers (logging, webhooks, etc.) without modifying the poller.

## MCP Server (Model Context Protocol)

Runs as an HTTP server exposing tools via the MCP protocol, allowing Claude Code to manage watches interactively. The server uses `rmcp` with Streamable HTTP transport over `axum`. Tools exposed: `watch_builds`, `stop_watches`, `list_watches`, `configure_branches`, `configure_repo`, `configure_ignored_workflows`, `update_notifications`, `rerun_build`, `build_history`, `get_stats` (10 tools). All tool parameters support double-encoded JSON arrays (a workaround for MCP clients that stringify array parameters).

## Port Binding with Fallback

The MCP server binds to a preferred port (default 8417, configurable via `BUILD_WATCHER_PORT`), falling back to up to 9 consecutive higher ports if the preferred port is occupied. The actual bound port is written to `~/.local/state/build-watcher/port` for discovery by other tools.

## Cross-Platform Service Installation

`install.sh` builds the release binary, installs it to `~/.local/bin/`, seeds a default config, and registers the daemon as a system service:

- **Linux**: systemd user service with `systemctl --user enable --now`. Installs a `.desktop` file for notification grouping.
- **macOS**: launchd user agent via `launchctl bootstrap`.

Both platforms get the MCP server registered in `~/.claude.json` and permissions added to `~/.claude/settings.json`. A matching `uninstall.sh` reverses all changes while preserving config and state files.

## Input Validation

Repo names are validated to match the `owner/repo` format with safe characters (alphanumeric, hyphen, underscore, dot). Branch names are validated similarly. The `#` character is explicitly rejected since it serves as the internal delimiter in watch keys (`repo#branch`). Validation runs before any state mutation or GitHub API call.

## TUI Dashboard (`bw`)

A top-like live terminal dashboard for monitoring all watched builds. Run with `bw` (auto-starts the daemon if it isn't running). The TUI connects to the daemon's REST API and SSE stream for real-time updates.

Features:
- **Live build status table** with colour-coded status, failing steps sub-rows, and elapsed/age columns
- **Top-like header** showing daemon uptime, polling intervals, and GitHub API rate limit
- **Row selection** (`↑`/`↓`/`j`/`k`) with actions on the selected repo/branch
- **Watch management** — add (`a`), remove (`d`), and configure branches (`b`) without leaving the TUI
- **Sortable columns** — cycle through repo, branch, status, workflow, age with `s`/`S`
- **Configurable grouping** — cycle through org, branch, workflow, status, none with `g`/`G`
- **Notification controls** — mute/unmute toggle (`n`), per-event level picker popup (`N`) with `←`/`→` cycling through `off`/`low`/`normal`/`critical`
- **Open in browser** — `o` opens the current run, `O` opens the repo page
- **Config popup** (`C`) — edit global default branches and ignored workflows inline
- **Auto-start** — if the daemon isn't running, `bw` starts it automatically
- **SSE real-time updates** — builds appear and complete instantly without waiting for poll cycles
- **Responsive columns** that scale to terminal width
- **Reconnection** with exponential backoff when the daemon connection drops
- **Quit and shutdown** — `Q` exits the TUI and also stops the daemon

## REST API

The daemon exposes REST endpoints alongside the MCP server for the TUI and other consumers:

- `GET /status` — JSON snapshot of all watches, active runs, and last builds
- `GET /stats` — daemon stats (uptime, polling intervals, API rate limit)
- `GET /events` — SSE stream of typed watch events
- `GET /notifications?repo=&branch=` — resolved (merged) notification config for a specific repo/branch
- `POST /notifications` — mute, unmute, or set per-event levels for a repo/branch; supports `action: "mute" | "unmute" | "set_levels"` and optional `branch`
- `GET /defaults` — global config defaults (default branches and ignored workflows)
- `POST /defaults` — update global default branches and/or ignored workflows
- `POST /pause` — toggle notification pause
- `POST /rerun` — rerun a build by repo and run ID
- `POST /shutdown` — initiate graceful daemon shutdown

## Graceful Shutdown

On SIGINT (ctrl-c), the server cancels all poller tasks via a `CancellationToken`, waits for in-flight operations to complete via a `TaskTracker`, persists final watch state to disk, removes the port file, and exits cleanly.
