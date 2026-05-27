# Changelog

## [1.2.0] - 2026-05-27

### Added

- **Notification warmup gates** — two grace windows stop notification floods. A 60s startup grace silences the burst that would otherwise fire for the current state of every watched branch when the daemon (re)starts with an empty in-memory transition map; transition state is still recorded during the window, so only genuinely-new events fire once it expires. A post-suspend wake grace triggers on a detected wall-clock gap > 10 minutes (NTP backward jumps are ignored), catching the machine-woke-from-sleep case the poller-level wake detection misses.

### Fixed

- **Notification pipeline edge cases** found while building the warmup gates: the wall-clock marker is now updated on every event (not just transitions), so a long-running build no longer looks like a suspend gap and silences its own completion; PR notifications now pass through the startup-warmup gate and update the wall-clock marker like build events; and a double config read in `ingest` was collapsed back to one.
- **Ignored-workflow runs could permanently hide real builds.** The poll high-water mark (`last_seen_run_id`) was advanced past *all* unseen runs, including ones filtered out by the ignored-workflows list. Because an ignored workflow that shares a branch (e.g. a `pull_request` Semgrep run created alongside a `push` build) often carries a higher run id, the mark leapfrogged the real run, which then never registered as "unseen" on a later poll — the repo stayed stuck on a stale build. The mark now advances only past runs we actually track.
- **Unpinning a branch in the TUI didn't work.** A pinned branch in a multi-branch repo renders as a single-branch repo header in the Pinned section; the toggle was flipping the repo-level flag instead of the branch flag, so the branch stayed pinned. It now targets the branch.
- **Build graph missing for pinned / single-branch rows.** The 7-day duration sparkline now shows in the detail bar for single-branch repo headers (which is how pinned branches render), via a shared detail-span builder so all row types render identically.
- **`install.sh` could fail with "Text file busy".** Binaries are now installed via atomic rename, so an in-progress install no longer races a still-running (or respawning) daemon.
- **Daemon could hang on SIGTERM.** A `bw` dashboard's long-lived `/events` SSE stream kept axum's graceful shutdown waiting forever, so the daemon only died on SIGKILL. Shutdown is now bounded: it persists state and exits cleanly within a few seconds even with a connected client.

### Changed

- **Pinning is now repo-or-branch with clear precedence.** Pinning a repo cascades to all its branches and clears any individual branch pins; you can't unpin a single branch out of a repo-level pin (the TUI explains how to lift it instead). Empty `BranchConfig` entries are pruned so the config doesn't accumulate stubs.
- **The daemon is owned by the platform service.** `bw` now starts the installed systemd user unit / launchd agent and connects to it instead of spawning its own orphan daemon that competed for the instance lock (which could put systemd into a restart loop). It still spawns a daemon directly when no service is installed or under `--config-dir`. The daemon exits 0 (benign) when another instance already holds the lock; the systemd unit uses `Restart=on-failure` + a start-limit, and the launchd agent uses `KeepAlive={SuccessfulExit:false}`.
- **Pinned / other section dividers** in the TUI are now a subtle dashed rule instead of a bold filled bar.
- **`install.sh --local`** uses the working-tree service/plist/desktop files instead of fetching them from the release branch, and escalates stale-process cleanup to SIGKILL.

## [1.1.0] - 2026-05-26

### Added

- **Flake detection** (`detect_flakes`, default on) — a Success that follows a failed attempt on the same commit is flagged as a recovered flake. The desktop notification swaps from "✅ succeeded" to "⚡ flake recovered", and the `flaky` bit is persisted on each `LastBuild` so the signal survives daemon restarts. New helper `history::is_flake`.
- **Build duration trend** — the detail bar at the bottom of the TUI now shows a 7-day duration trend for the selected active or completed build: `avg 4:10 (3:42–5:18) ▂▃▅▄▆▃▄▅`. The avg + (min–max) range come from successful samples only (the typical-runtime stat); the sparkline bars include every conclusion colour-coded — green for Success, red for Failure/TimedOut/StartupFailure, yellow for Cancelled, gray for Unknown. New helpers `history::avg_duration` and `history::recent_completed_builds`. New `BuildSample` type and `avg_duration_secs` / `recent_builds` fields on `ActiveRunView` and `LastBuildView`.
- **Notify mode** (`notify_mode`, default `every_build`) — controls which build events fire desktop notifications. `every_build` notifies on every kind change and every new-commit build (repeat polls of the same commit + same state are still suppressed). `failures_and_recoveries` only fires on Failed events and the Success that ends a failure streak — pick this for the quietest mode that still tells you when red turns green. Backed by `head_sha` now carried on `RunSnapshot` so the pipeline can distinguish same-commit repeats from new-commit builds.
- TUI config form exposes the new toggles: `Detect flakes` on the Display tab and `Notify mode` on a new Notifications tab.

### Changed

- `POST /defaults` REST endpoint now validates `notify_mode` strings up-front and returns an error for unknown values, matching the existing `branch_filter` regex validation pattern. The legacy `"failure_only"` string remains accepted as an alias for `failures_and_recoveries`.
- `NotificationPipeline.ingest` now reads the config lock once per ingest call instead of twice.
- **TUI detail bar tidied up** — dropped repo / branch / workflow / status / age from the bar across all row types, since the selected row already shows those in its columns. The bar is now purely the deep-dive details: run id, retry, failing steps, exact duration, 7-day trend, author.

### Fixed

- `is_transition` was tracking per-(repo, branch, kind) only — a new commit in the same state as the previous one was suppressed indefinitely. The transition key now also includes `head_sha`, so a new-commit build always notifies even when the kind hasn't changed.

## [1.0.7] - 2026-05-26

### Changed

- **Dependency bumps** — `lru` 0.12 → 0.18, `ratatui` 0.29 → 0.30 (via the `crossterm_0_29` feature), `crossterm` 0.28 → 0.29, plus 22 semver-compatible updates including `tokio`, `rmcp`, `axum`/`tower-http`, and `serde_json`. No user-visible behavior change; no breaking ratatui 0.30 APIs are in use.

### Fixed

- **Non-exhaustive `PrMergeState` match in the PR notification handler** — the `_ => return` fallback meant a new `MergeState` variant would compile silently with no notification path. Now matches `HasHooks | Unknown` explicitly so future variants surface at compile time.
- **Clippy `field_reassign_with_default` warnings** in five test/setup sites converted to struct-literal initialization.

## [1.0.6] - 2026-05-12

### Fixed

- **Duplicate PR notifications when GitHub briefly returns `Unknown` merge state** — after a push, GitHub transiently sets `mergeStateStatus` to `UNKNOWN` while recomputing merge readiness. This was overwriting the tracked state, making the PR's return to its prior state (e.g. `Blocked`) look like a new transition and firing a duplicate desktop notification. `Unknown` and `HasHooks` states are now skipped entirely in the PR state tracker so they never reset the transition baseline.
- **Stale PR data shown in TUI after `watch_prs` is disabled** — open PR entries were not cleared from watch state when `watch_prs` was turned off, causing the TUI to keep displaying them until the daemon restarted. They are now cleared on the next poll cycle after `watch_prs` is disabled.
- **PR notifications not throttled** — `PrStateChanged` events bypassed the shared 10/60s notification budget used for build events. They now consume from the same budget, preventing notification bursts when many PRs change state simultaneously.
- **Missing author and draft status on PR entries created via SSE** — `PrStateChanged` events did not carry `author` or `draft` fields, so PR rows inserted into the TUI's local cache via the SSE event stream showed an empty author and incorrect draft state. Both fields are now included in the event.

## [1.0.5] - 2026-05-11

### Fixed

- **Archived repos included in auto-discovery** — `list_accessible_repos` now filters out repos where `archived: true`, so they are never matched against auto-discover rules.
- **Removing all auto-discover rules left previously-discovered repos still watched** — the discovery cycle returned early when the rule list was empty instead of treating the empty set as "nothing should be discovered", causing repos to linger. Now runs the cleanup pass regardless.

## [1.0.4] - 2026-05-09

### Added

- **Auto-discover repos by rule** — the daemon can automatically watch repos matching configurable rules. Each rule has an `id`, an optional `repo_pattern` (regex matched against the full `owner/name` path, e.g. `^myorg/foo-.*$`), an optional `org_pattern` (legacy: matched against the owner only), and a `recently_updated` filter (`any` | `week` | `month` | `year`). Auto-discovered repos are tracked separately from manually-added repos.
- **REST API for auto-discover rules** — `GET /auto-discover-rules` lists all rules; `POST /auto-discover-rules` adds or replaces a rule by ID; `POST /auto-discover-rules/remove` removes a rule by ID.
- **`auto_discovered_by_rule` field** in `GET /repo-config` response — `true` when the repo is being auto-watched by a discovery rule; branches for these repos cannot be edited manually.
- **TUI: auto-discover rules popup** — press `D` to view, add (`a`/`+`), edit (`Enter`), and delete (`d`/`Delete`) discovery rules without leaving the dashboard.
- **TUI: status-based sorting** now uses the worst conclusion across all workflows for a branch (not just the newest), matching the display logic.
- **Paginated ETag caching** — `api_get_all_pages` now chains through the `Link: rel="next"` header so unchanged pages return 304s at zero rate-limit cost.
- **Pagination guard** — `api_get_all_pages` truncates at 100 pages with a warning, preventing unbounded API loops for very large orgs.
- **HH:MM zero-padding enforcement** — the quiet-hours validator now rejects single-digit hours or minutes (`9:05` must be `09:05`).

### Fixed

- **Actor and commit author missing from notifications and TUI** — `RunSnapshot::from_run_info()` and `ActiveRun::from_run_info()` were hard-coding `actor: None` / `commit_author: None`; now copies the fields from the run data.
- **`set_defaults_handler` and `set_repo_config_handler` returned `ok: true` on disk-save errors** — now correctly returns `ok: false` so callers can detect the failure.
- **`POST /unwatch` and the `stop_watches` MCP tool** did not validate repo names; now return an error for invalid inputs before touching any watches.
- **`repo_pattern` matched against short name instead of full path** — was comparing against `repo.name` (e.g. `foo-bar`); now compares against `repo.full_name` (e.g. `myorg/foo-bar`). See **Changed** for migration notes.
- **`last_failed_build`** filtered on `conclusion != Success`, which included `Cancelled` and `Skipped` as failures; now uses an explicit set (`Failure | TimedOut | StartupFailure`).
- **Branch resolution task panics** in `run_discovery_cycle` and `resolve_config_keys` were silently swallowed by an `Ok`-only pattern on the `JoinHandle`; now logged as warnings and the cycle continues.
- **`GroupBy::Status` splits-repo flag missing** — branches were landing in the wrong status group when grouping by status.

### Changed

- **Author info is now inline in `RunInfo`** — `actor` (GitHub login of the triggering actor) and `commit_author` (head-commit author name) are populated directly by the REST client at fetch time, eliminating a separate `GET /repos/{repo}/actions/runs/{id}` call per new run. The `gh` CLI fallback leaves these fields `None`.
- **`run_author()` removed from the `GitHubClient` trait** — author info is embedded in `RunInfo` at fetch time. Code implementing `GitHubClient` must remove this method.
- **`repo_pattern` now matches the full `owner/name` path** — previously matched only the repo short name. `org_pattern` retains its legacy owner-only semantics and is kept for backwards compatibility. **Migration:** if you have rules with `repo_pattern` anchored to the name only (e.g. `^foo-.*$`), update them to include the owner prefix (e.g. `^myorg/foo-.*$`). Rules that use only `org_pattern` are unaffected.
- **Branch edits on auto-managed repos are now rejected** — `POST /branches` (REST) and `configure_branches` (MCP) return an error when the repo is rule-discovered or when branch auto-discovery is enabled. The TUI also blocks these operations before sending the request.
- **`RwLock` replaces `Mutex` for the GitHub token** in `ReqwestClient` — concurrent poll tasks no longer block each other during token reads.

### Backwards compatibility

Config files, watch-state files, and the REST API are fully compatible with 1.0.0. All new response fields are optional and default-safe; old `bw` binaries connecting to a new daemon will simply ignore unknown fields. The only breaking change is `repo_pattern` semantics (see **Changed** above), which only affects auto-discover rules — a feature not present in any release before 1.0.4.

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

[1.2.0]: https://github.com/wkirschbaum/build-watcher/compare/v1.1.0...v1.2.0
[1.1.0]: https://github.com/wkirschbaum/build-watcher/compare/v1.0.7...v1.1.0
[1.0.7]: https://github.com/wkirschbaum/build-watcher/compare/v1.0.6...v1.0.7
[1.0.6]: https://github.com/wkirschbaum/build-watcher/releases/tag/v1.0.6
[1.0.5]: https://github.com/wkirschbaum/build-watcher/releases/tag/v1.0.5
[1.0.4]: https://github.com/wkirschbaum/build-watcher/releases/tag/v1.0.4
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
