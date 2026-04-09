# Changelog

## [0.18.2] - 2026-04-09

### Polish

- Show commit author inline in the TUI title column (dimmed, after title text)
- Deduplicate actor/author in detail bar — show author name, append `[by actor]` only when they differ
- Preserve author info across daemon restarts (don't overwrite persisted data on initial seed)
- Show "Loading watches…" during daemon startup instead of "No repos watched"
- Drop `bw ·` prefix from terminal title
- Support `GITHUB_TOKEN` environment variable as an alternative to `gh auth login`
- Add missing keys to help popup (`?`, `U`)
- Add Cargo.toml crates.io metadata

### Docs

- Fix README example config (remove non-existent `default_branches` field)
- Add missing MCP tools to README (`watch_from_git_remote`, `configure_ignored_events`)
- Add PR Watch section to README with badge meanings
- Complete keybindings table in README and docs/tui.md (`c`, `C`, `t`/`T`)
- Document `show_author` config field and API cost
- Add 1.0 roadmap (`docs/todo-1.0.0.md`)

## [0.18.1] - 2026-04-09

### Fixed

- Cancelled builds now use `build_success` notification level (normal) instead of `build_failure` (critical)
- Cancelled builds show as gray `⊘` in TUI instead of red `✗`

## [0.18.0] - 2026-04-09

### Added

- **Build times popup** — press `t` for per-repo build durations (by workflow, sorted slowest first), `T` for cross-repo summary from already-loaded history
- Each row shows avg/min/max duration, run count, and colour-coded pass rate

## [0.17.1] - 2026-04-09

### Fixed

- Auto-refresh GitHub token on `401 Unauthorized` — re-acquires via `gh auth token` and retries once
- Applies to all request paths (GET, POST, PUT, GraphQL)

## [0.17.0] - 2026-04-09

### Changed

- **Direct HTTP client** — replaced `gh` CLI process spawning with `reqwest` for all GitHub API calls. Eliminates fork/exec overhead and enables HTTP connection reuse.
- **ETag caching** — conditional requests (`If-None-Match`) return `304 Not Modified` at zero rate-limit cost, making idle repo polling essentially free.
- **Unified poll interval** — single interval (minimum 5s) replaces separate active/idle intervals, since ETags eliminate the cost difference.
- Falls back to `gh` CLI if token acquisition fails at startup.

### Fixed

- "just now ago" double-suffix bug in TUI detail bar
- Stale poll aggression percentages in docs and tool descriptions (Low is 15%, not 10%)

## [0.14.0] - 2026-04-09

### Added

- **Auto-discover from open PRs** -- branches with open pull requests are now discovered even when their CI runs have fallen outside the recent-runs window

## [0.13.0] - 2026-04-02

### Added

- **Author info in detail bar** -- show triggering actor and commit author for the selected build
- `show_author` toggle in global config form (`C` key) -- controls the extra API call per new run

## [0.12.1] - 2026-04-02

### Fixed

- Prune deleted branches from auto-discover -- branches removed from GitHub are no longer kept as stale watches

## [0.12.0] - 2026-04-02

### Added

- **Help popup** (`?` key) -- full keybinding reference overlay
- **PR picker** -- select which PR to merge when multiple target the same branch
- Auto-discover branch guards -- prevent watching branches that don't exist on GitHub

## [0.11.1] - 2026-04-01

### Changed

- Simplify `start_watch` -- entries start in `waiting` state; the poller's first cycle fetches initial data
- Remove `recover_watches` -- startup now uses a unified path through `startup_watches`

## [0.11.0] - 2026-04-01

### Added

- **Auto-discover branches** -- automatically watch branches with active GitHub Actions runs, with optional regex filter (`branch_filter`)
- **Per-repo auto-discover override** -- enable or disable branch discovery per repo
- **Per-repo branch filter** -- regex pattern scoped to a single repo

### Changed

- Remove global `default_branches` config -- replaced by auto-discover and per-repo branch config

### Fixed

- Improve error handling throughout -- remove panics, use `Result` returns, and add proper error context

## [0.10.0] - 2026-04-01

### Added

- **PR watch**: opt-in per-repo feature to poll open PRs and track merge-readiness
  - Enable via `c` per-repo config form or `watch_prs` in config.json
  - Detects merge-state transitions: Clean, Blocked, Unstable, Behind, Dirty
  - Desktop notifications when PRs become ready to merge or blocked
  - Compact PR badge in TUI branch column: `PR:✓` / `PR:⊘` / `PR:!` / `PR:↓` / `PR:✗`
- **Per-repo config form** (`c` key): edit alias, watch PRs, and poll aggression per repo
- **Per-repo poll aggression**: override the global poll aggression per repo (falls back to global when unset)
- Compact event prefixes in titles: `PR:`, `cron:`, `manual:`

### Changed

- `C` key remains global config; `c` key opens per-repo config
- Form dispatch uses `FormKind` enum instead of string matching
- REST `GET/POST /repo-config` endpoints for per-repo settings
- Derive `Default` on `WatchStatus`, `ActiveRunView`, `LastBuildView`, `RunStatus`, `RunConclusion` — reduces test boilerplate

## [0.9.0] - 2026-03-31

### Added

- Fetch `createdAt`, `updatedAt`, `url` from GitHub API — durations and URLs now come from real timestamps instead of local tracking
- `RunInfo.duration_secs()` computes real duration from GitHub timestamps
- `elapsed_since()` helper for computing elapsed time from ISO timestamps

### Changed

- Replace `Instant`-based elapsed tracking with GitHub timestamps — durations survive daemon restarts
- `ActiveRun` stores `created_at`/`updated_at`/`url` instead of `tokio::time::Instant`
- Simplify `record_completion` (3 params, no return), `incorporate_new_runs` (1 param), `build_watch_snapshot` (no Instant param)
- Remove `elapsed_map` from repo poller — use timestamp-based duration directly
- Propagate `url` field through `RunSnapshot`, `ActiveRunView`, `LastBuildView`

## [0.8.8] - 2026-03-31

### Fixed

- Notification bug: `RunStarted` events were silently suppressed — now triggers desktop notifications on branch-level transitions (started/succeeded/failed)
- Transition tracking now operates per (repo, branch) instead of per workflow, so redundant notifications for multiple workflows on the same branch are suppressed

### Changed

- Extract `NotificationPipeline` struct owning all notification state (transition tracking, debounce buffer, throttle window)
- Inject `Notifier` trait into notification handler instead of using global platform singleton — enables proper test assertions on dispatched notifications
- Remove `NullNotifier` / `universal.rs` (replaced by `RecordingNotifier` in tests)
- Notification tests now call pipeline methods directly — no channels, spawned tasks, or sleeps
- Add `TestHarness` to watcher tests, eliminating repeated setup boilerplate
- Remove redundant tests that duplicated coverage between unit and integration layers

## [0.8.7] - 2026-03-31

### Changed

- Simplify shared types: flatten re-exports, remove redundant type aliases
- Update poll aggression documentation
- Update README

## [0.8.6] - 2026-03-31

### Fixed

- Draft recovery for interrupted config saves — orphaned `.draft` files are automatically promoted on load
- TUI status bar consistency improvements
- Rename "NOTIFS PAUSED" label, remove dead `active_count` method

### Changed

- Centralize all config mutations behind `ConfigManager` — eliminates direct field access from server actions
- TUI: remove header status summary, collapse to single line
- TUI: align terminal title counts with header summary
- TUI: skip Branches expand level when no branch has multiple workflows
- TUI: extract colour constants, `attempt_suffix` helper, `set_expand_level` method
- TUI: use middle dot separator consistently throughout UI
- TUI: header status order active-first, always show counts

## [0.8.5] - 2026-03-30

### Fixed

- Ignored workflows (e.g. `Semgrep`) now hidden from TUI — snapshot builder filters `active_runs` and `last_builds` against `ignored_workflows` config at serve time, so stale entries are never displayed

### Changed

- **Poll aggression**: Medium target raised from 30% → 40% of rate-limit budget (interval multiplier 2.0× → 1.5×); High target raised from 70% → 80% (unchanged 1.0× multiplier)
- **Header status summary** — line 2 shows `{N}r/{N}b  ✗ {N}  ⏳ {N}  ✓ {N}  · {N}` with colour coding (red failures, yellow active, green passing)

## [0.8.4] - 2026-03-30

### Changed

- **TUI: panel layout redesign** — the watches list is now a proper bordered panel with column headings inside; the recent builds panel is a bordered box with a "Recent" title; both panels have a consistent visual frame
- **TUI: scrollable watches panel** — the body no longer allocates exact height for rows; it fills available space and scrolls, keeping the selected row centered; `▲`/`▼` indicators appear on the panel border when content is hidden above or below
- **TUI: detail bar snapped to bottom** — the detail bar is now a single plain row that always sits directly above the help bar, regardless of how many repos are listed; the previous TOP+BOTTOM borders are removed (surrounding panel borders provide the visual separation)
- **TUI: `H` toggles recent panel, `h` shows history popup** — `H` now toggles the Recent builds panel on/off (persisted in preferences); `h` opens a history popup scoped to the hovered item (branch or repo)
- **TUI: header reduced to 2 lines** — the manual separator line is removed; the body panel's top border provides visual separation
- **TUI: group-by shown in panel border** — when a non-default group-by mode is active, the label appears right-aligned in the watches panel's top border
- **TUI: group header rows** — group headers now render with a dark background across the full row for clear visual weight as section dividers
- **TUI: attempt count** — retry indicator changed from `(r:N)` to `(N)` for brevity
- **TUI: column widths** — widths now correctly account for the 2-character panel border padding so table content stays within bounds

## [0.8.3] - 2026-03-30

### Fixed

- Serialize config saves to prevent race conditions between concurrent writes
- Async daemon startup to avoid blocking the event loop during initial service registration

## [0.8.2] - 2026-03-29

### Fixed

- Auto-create config entry when muting or configuring a repo that has no existing config entry

## [0.8.1] - 2026-03-29

### Fixed

- Avoid unnecessary config re-save on reads; improve persistence error logging

[0.18.2]: https://github.com/wkirschbaum/build-watcher/releases/tag/v0.18.2
[0.18.1]: https://github.com/wkirschbaum/build-watcher/releases/tag/v0.18.1
[0.18.0]: https://github.com/wkirschbaum/build-watcher/releases/tag/v0.18.0
[0.17.1]: https://github.com/wkirschbaum/build-watcher/releases/tag/v0.17.1
[0.17.0]: https://github.com/wkirschbaum/build-watcher/releases/tag/v0.17.0
[0.14.0]: https://github.com/wkirschbaum/build-watcher/releases/tag/v0.14.0
[0.13.0]: https://github.com/wkirschbaum/build-watcher/releases/tag/v0.13.0
[0.12.1]: https://github.com/wkirschbaum/build-watcher/releases/tag/v0.12.1
[0.12.0]: https://github.com/wkirschbaum/build-watcher/releases/tag/v0.12.0
[0.11.1]: https://github.com/wkirschbaum/build-watcher/releases/tag/v0.11.1
[0.11.0]: https://github.com/wkirschbaum/build-watcher/releases/tag/v0.11.0
[0.10.0]: https://github.com/wkirschbaum/build-watcher/releases/tag/v0.10.0
[0.9.0]: https://github.com/wkirschbaum/build-watcher/releases/tag/v0.9.0
[0.8.8]: https://github.com/wkirschbaum/build-watcher/releases/tag/v0.8.8
[0.8.7]: https://github.com/wkirschbaum/build-watcher/releases/tag/v0.8.7
[0.8.6]: https://github.com/wkirschbaum/build-watcher/releases/tag/v0.8.6
[0.8.5]: https://github.com/wkirschbaum/build-watcher/releases/tag/v0.8.5
[0.8.4]: https://github.com/wkirschbaum/build-watcher/releases/tag/v0.8.4
[0.8.3]: https://github.com/wkirschbaum/build-watcher/releases/tag/v0.8.3
[0.8.2]: https://github.com/wkirschbaum/build-watcher/releases/tag/v0.8.2
[0.8.1]: https://github.com/wkirschbaum/build-watcher/releases/tag/v0.8.1
