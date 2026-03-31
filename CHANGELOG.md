# Changelog

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

[0.8.8]: https://github.com/wkirschbaum/build-watcher/releases/tag/v0.8.8
[0.8.7]: https://github.com/wkirschbaum/build-watcher/releases/tag/v0.8.7
[0.8.6]: https://github.com/wkirschbaum/build-watcher/releases/tag/v0.8.6
[0.8.5]: https://github.com/wkirschbaum/build-watcher/releases/tag/v0.8.5
[0.8.4]: https://github.com/wkirschbaum/build-watcher/releases/tag/v0.8.4
[0.8.3]: https://github.com/wkirschbaum/build-watcher/releases/tag/v0.8.3
[0.8.2]: https://github.com/wkirschbaum/build-watcher/releases/tag/v0.8.2
[0.8.1]: https://github.com/wkirschbaum/build-watcher/releases/tag/v0.8.1
