# Changelog

## [1.0.3] - 2026-05-08

### Fixed

- **`GET /history` `repo` field was always `""`** — `history_handler` passed an empty string instead of the query parameter value; every `HistoryEntryView` now carries the correct repo name.
- **`show_author` deserialized as `false` for configs missing the field** — `#[serde(default)]` resolved to `bool::default()` (`false`) rather than the intended `true`; changed to `#[serde(default = "default_true")]` so existing configs without the field enable author display as expected.

### Changed

- **`display_title` visibility corrected** — was `pub(super)` (misleading) while called cross-module; now `pub(crate)`.
- **`snap_workflow` added to lib `testutil`** — brings it on par with the binary test helper.
- **Redundant early-exit condition removed** in run detection — `new_runs.is_empty()` was always implied by `unseen.is_empty()` and is now gone.
- **Misplaced doc comment fixed** on `apply_pause` / `validate_hhmm` in `server/actions.rs`.
- **Dead `_events: EventBus` parameters removed** from `test_router` and `test_router_full` in REST tests.

## [1.0.2] - 2026-05-08

### Added

- **`cargo-binstall` support** — `[package.metadata.binstall]` in `Cargo.toml` lets users install prebuilt binaries from GitHub Releases without compiling. Run `cargo binstall --git https://github.com/wkirschbaum/build-watcher build-watcher` to install `build-watcher` and `bw` directly from a release tarball.
- **Topgrade integration documented** — README now shows a `~/.config/topgrade.toml` `[commands]` snippet that re-runs `cargo binstall --git ... -y` to keep the install up to date.

### Changed

- **Release archives repackaged** — each target now produces a single `build-watcher-v{version}-{target}.tar.gz` containing both `build-watcher` and `bw`, replacing the previous two-tarball-per-target layout. Required for `cargo-binstall`'s one-archive-per-crate model.
- **`install.sh`** — adapts to the new combined-archive format; resolves the latest tag via `gh release view --json tagName` and downloads a single asset per platform.

## [1.0.1] - 2026-05-08

### Fixed

- **`list_branches` now paginates** via `api_get_all_pages` — previously capped at 100 branches, silently dropping the rest on large repos.
- **Cancelled builds use `build_failure` notification level** instead of `build_success`, so muting successes no longer swallows cancellations.
- **`apply_quiet_hours` requires both `start` and `end`** — partial input is now rejected instead of silently filling defaults (`22:00`/`06:00`).
- **`set_repo_config_handler` validates `branch_filter` regex** before storing — invalid regex returns `400` instead of corrupting the config.
- **SSE multi-line `data:` lines are concatenated** with `\n` (RFC 8895 compliance) instead of overwriting earlier lines.
- **`backfill_failing_steps` runs once per cycle** instead of twice, halving the per-poll API cost for resolved runs.

### Changed

- **ETag cache rewritten** with `tokio::sync::Mutex` + bounded `LruCache` (cap 500) — eliminates async-thread blocking on `std::sync::Mutex` and unbounded memory growth.
- **D-Bus action listener tasks capped at 20** with a `CancellationToken` for clean shutdown — previously accumulated without bound on Linux.
- **Branch-filter regex pre-compiled and cached** on `RepoConfig`, recompiled only on config change instead of every lookup.
- **`repo_poller.rs` split** into `run_tracker.rs`, `branch_tracker.rs`, and `pr_tracker.rs`; main loop now delegates.
- **Shared test helpers consolidated** into `src/testutil.rs` and `src/bin/build-watcher/testutil.rs`.
- **Notification event match is now exhaustive** — adding a new `WatchEvent` variant produces a compile error instead of a silent fallthrough.

## [1.0.0] - 2026-05-07

### Added

- **`--config-dir <path>` flag** — run multiple daemon instances simultaneously. Config lives at `<path>/config.json`, state (socket, lock, port, watches) under `<path>/state/`. Custom instances use an OS-assigned port (no collisions). `bw --config-dir <path>` connects to the matching daemon and shows `[name]` in the header. Both `bw --reset-state` and auto-start correctly propagate the flag.

### Changed

- First stable release. All items on the pre-1.0 roadmap are complete.

## [0.20.0] - 2026-05-07

### Added

- **Unix domain socket IPC** — daemon listens on `daemon.sock` alongside TCP; `bw` prefers the socket for lower-latency local connections with graceful TCP fallback.
- **`GET /version` endpoint** — returns daemon version and API version for client compatibility checks.
- **`GET/POST /repo-config` endpoints** — read and write per-repo config (branches, notifications, ignored events, alias, watch PRs, poll aggression, branch filter).

### Changed

- **`configure_ignored_events` replaces `configure_ignored_workflows`** — single tool now handles both GitHub event types (`schedule`, `workflow_dispatch`, etc.) via `kind: "event"` and workflow names via `kind: "workflow"`. Supports per-repo scoping for events.
- **`--with-claude` required for MCP registration** — `build-watcher --register` no longer modifies `~/.claude.json` by default; pass `--with-claude` explicitly.
- **`auto_discover_branches` and `watch_prs` default to `true`** for new repos.
- **`discovered_branches` migrated to state file** — auto-discovered branches moved from `config.json` to `discovered.json` in the state directory; existing installs migrate automatically.
- **Versioned watch state** — `watches.json` stored as `{ schema_version, watches }`; legacy flat-map format migrated automatically on first load.
- **`configure_repo` MCP tool complete** — now exposes `auto_discover_branches`, `branch_filter`, and `ignored_events` for per-repo configuration.
- **Typed `RunConclusion`, `RunStatus`, `PollAggression`** — proper Rust enums with serde-derived JSON; `StatsResponse::poll_aggression` is now typed.

## [0.19.2] - 2026-04-29

### Added

- **`default_branches` config** — global fallback branch list for repos with no per-repo branch config. Settable via the `C` key in TUI, the `/defaults` REST endpoint, and `config.json`. When set, new repos will automatically watch these branches instead of only the GitHub default branch.

### Changed

- **Standardized MCP tool descriptions** — all 13 tools now follow a consistent format: summary line + defaults + gotchas. Improves discoverability when browsing tools from any MCP client.

### Docs

- **Troubleshooting section in README** — covers: no notifications (auth, daemon not running), rate limit exhausted, Linux notification daemon setup, macOS `terminal-notifier` for clickable links, and stale state after restart.
- Document `default_branches` config field in README configuration table.

## [0.19.1] - 2026-04-29

### Changed

- **Merge always confirms** — pressing `m` now always shows a popup before merging, even when there is only one open PR. Single-PR popup shows "Confirm Merge" with `Enter`/`Esc` only; multi-PR popup shows "Select PR to Merge" with `↑↓` navigation as before.

## [0.19.0] - 2026-04-29

### Added

- **Colored PR badges** — each open PR now shows `#<number><icon>` in the branch column, color-coded by merge state: green for ready (✓), red for blocked/conflict (⊘/✗), yellow for behind/unstable (!). Multiple PRs are shown individually (`[#42✓ #43⊘]`) instead of a count. The badge now also appears on multi-workflow branch header rows.

### Fixed

- **PR polling always runs** — previously PRs were only polled when no builds were active, so a branch with a long-running CI job would never show its PR state. PR polling now runs every cycle regardless of build activity.
- **PRs now appear for repos without review requirements** — GitHub returns `null` for `reviewDecision` when no review policy is configured. The field was typed as `String` with `#[serde(default)]` which handles absent fields but not explicit JSON `null`, silently dropping all PR data. Changed to `Option<String>`.
- **Branch sync safety** — skip auto-branch sync when the GitHub `list_branches` API fails, preventing valid branches from being removed due to a transient API error.

## [0.18.4] - 2026-04-22

### Fixed

- Auto-discovered branches are now only removed when they are deleted on GitHub — branches that still exist but have no recent activity are kept, preventing valid branches from flapping out of the watch list

## [0.18.3] - 2026-04-10

### Fixed

- Use explicit RGB background for selected row highlight instead of named `DarkGray` palette colour — avoids foreground colour remapping on Mac terminals (Terminal.app, some iTerm2 themes)
- Fix install.sh default config: remove non-existent `default_branches` field, add `show_author` and `poll_aggression`

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

[1.0.3]: https://github.com/wkirschbaum/build-watcher/releases/tag/v1.0.3
[1.0.2]: https://github.com/wkirschbaum/build-watcher/releases/tag/v1.0.2
[1.0.1]: https://github.com/wkirschbaum/build-watcher/releases/tag/v1.0.1
[1.0.0]: https://github.com/wkirschbaum/build-watcher/releases/tag/v1.0.0
[0.20.0]: https://github.com/wkirschbaum/build-watcher/releases/tag/v0.20.0
[0.19.2]: https://github.com/wkirschbaum/build-watcher/releases/tag/v0.19.2
[0.19.1]: https://github.com/wkirschbaum/build-watcher/releases/tag/v0.19.1
[0.19.0]: https://github.com/wkirschbaum/build-watcher/releases/tag/v0.19.0
[0.18.4]: https://github.com/wkirschbaum/build-watcher/releases/tag/v0.18.4
[0.18.3]: https://github.com/wkirschbaum/build-watcher/releases/tag/v0.18.3
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
