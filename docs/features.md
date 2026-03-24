# Features

## GitHub Actions Build Monitoring

Watches GitHub Actions workflow runs for configured repositories by polling the `gh` CLI. Each repo/branch combination gets its own async polling task. The poller uses two speeds: fast polling (default 10s) when builds are actively running, and slow polling (default 60s) when idle. New workflow runs are detected by tracking a high-water mark of the most recent run ID seen per repo/branch. On startup, persisted watches are recovered from disk and in-progress runs are re-discovered from GitHub so no build completions are missed across daemon restarts.

## Desktop Notifications

Sends native desktop notifications when builds start and complete. Notifications include the workflow name, branch, commit title (with short SHA for push events, or "PR: title" for pull requests), build conclusion, elapsed time, and a clickable link to the GitHub Actions run. Notifications are grouped per repo/branch/workflow so each slot replaces the previous one rather than stacking.

On Linux, the preferred backend is D-Bus via the `notify-rust` crate, with a fallback to the `notify-send` CLI. Both support notification replacement via stored IDs, urgency levels, icons, categories, expiry times, a `desktop-entry` hint for GNOME/KDE grouping, and an "Open" action button that launches `xdg-open` with the run URL.

On macOS, the preferred backend is `terminal-notifier` (supports URL open and notification grouping), with a fallback to `osascript` (AppleScript `display notification`). Child processes are reaped with a 10-second timeout to prevent zombies.

## Hierarchical Notification Configuration

Notification levels (`off`, `low`, `normal`, `critical`) are configurable per event type (`build_started`, `build_success`, `build_failure`) at three scopes: global defaults, per-repo overrides, and per-branch overrides. Resolution follows branch > repo > global priority — a branch override wins over a repo override, which wins over the global default. Only the events you specify are changed; others inherit from the parent scope.

## Notification Pause and Resume

Notifications can be temporarily paused for a specified number of minutes or indefinitely until manually resumed or the daemon restarts. While paused, builds continue to be tracked and state is updated — only the desktop notification dispatch is suppressed. The pause state is visible in `list_watches` and `get_config` output.

## Sound on Failure

An optional audio alert plays when a build fails. Disabled by default. Configurable globally (enable/disable + custom sound file path) and per-repo (enable/disable override). On Linux, plays via `paplay` (PulseAudio/PipeWire) with a fallback to `aplay` (ALSA), defaulting to `/usr/share/sounds/freedesktop/stereo/dialog-error.oga`.

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

Displays a formatted table of recent builds for a repo, showing conclusion, workflow name, commit title, duration, and relative age. Optionally filter by branch. When multiple branches are present and no branch filter is applied, the branch column is shown. Durations and ages are computed from GitHub's `createdAt`/`updatedAt` timestamps using a built-in ISO 8601 parser (no datetime library dependency).

## Branch Configuration

Per-repo branch lists override the global default branches (default: `["main"]`). The default branches can be changed globally via `set_default_branches`. Per-repo branches are set via `configure_branches`. Changes to branch configuration require restarting watches to take effect.

## Persistent Configuration

All settings are stored in `~/.config/build-watcher/config.json` with crash-safe writes: data is serialized to a `.draft` file, fsynced, verified by re-parsing, then atomically renamed over the primary. A `.bak` backup of the previous config is kept. On load, if the primary is corrupt or missing, the backup is transparently recovered. On first startup, the config is re-saved to normalize the schema (adding any missing fields with defaults).

## Persistent Watch State

Active watches and their last-seen run IDs are persisted to `~/.local/state/build-watcher/watches.json` using the same crash-safe write pattern. On daemon startup, persisted watches are recovered, in-progress runs are re-discovered from GitHub, and polling resumes. Repos in config that don't have persisted watch state yet are automatically started.

## Event Bus Architecture

An internal broadcast channel decouples the polling loop from notification dispatch. Pollers emit typed events (`RunStarted`, `RunCompleted`, `StatusChanged`) onto the bus. A dedicated notification handler subscribes and dispatches desktop notifications + sound based on the current configuration and pause state. This separation allows future subscribers (logging, webhooks, etc.) without modifying the poller.

## MCP Server (Model Context Protocol)

Runs as an HTTP server exposing tools via the MCP protocol, allowing Claude Code to manage watches interactively. The server uses `rmcp` with Streamable HTTP transport over `axum`. Tools exposed: `watch_builds`, `stop_watches`, `list_watches`, `configure_branches`, `set_default_branches`, `configure_notifications`, `configure_workflows`, `ignore_workflows`, `unignore_workflows`, `pause_notifications`, `resume_notifications`, `configure_sound`, `rerun_build`, `build_history`, `get_config`, `test_notification`. All tool parameters support double-encoded JSON arrays (a workaround for MCP clients that stringify array parameters).

## Port Binding with Fallback

The MCP server binds to a preferred port (default 8417, configurable via `BUILD_WATCHER_PORT`), falling back to up to 9 consecutive higher ports if the preferred port is occupied. The actual bound port is written to `~/.local/state/build-watcher/port` for discovery by other tools.

## Cross-Platform Service Installation

`install.sh` builds the release binary, installs it to `~/.local/bin/`, seeds a default config, and registers the daemon as a system service:

- **Linux**: systemd user service with `systemctl --user enable --now`. Installs a `.desktop` file for notification grouping.
- **macOS**: launchd user agent via `launchctl bootstrap`.

Both platforms get the MCP server registered in `~/.claude.json` and permissions added to `~/.claude/settings.json`. A matching `uninstall.sh` reverses all changes while preserving config and state files.

## Input Validation

Repo names are validated to match the `owner/repo` format with safe characters (alphanumeric, hyphen, underscore, dot). Branch names are validated similarly. The `#` character is explicitly rejected since it serves as the internal delimiter in watch keys (`repo#branch`). Validation runs before any state mutation or GitHub API call.

## Graceful Shutdown

On SIGINT (ctrl-c), the server cancels all poller tasks via a `CancellationToken`, waits for in-flight operations to complete via a `TaskTracker`, persists final watch state to disk, removes the port file, and exits cleanly.
