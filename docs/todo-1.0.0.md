# Road to 1.0.0 ✓ shipped 2026-05-07

## Must do

- [x] **Empty-state TUI guidance** — when no watches exist, show "Press `a` to add a repo" instead of a blank screen.
- [x] **Complete keybindings table in README** — `c`, `C`, `t`/`T` added. Both README and docs/tui.md match reality.
- [x] **Document PR watch feature** — added PR Watch section to README with badge meanings and `M` merge key.
- [x] **Handle missing `gh` CLI gracefully** — checks `GITHUB_TOKEN` env var first, falls back to `gh auth token`, gives helpful error with install link if neither works.
- [x] **Document `show_author` API cost** — documented in README config table. Already exposed in TUI global config form (`C` key).
- [x] **Contextual error messages** — GhError messages updated to be transport-agnostic. Token errors include hints about `gh auth login` and `GITHUB_TOKEN`.

## Should do

- [x] **Troubleshooting section in README** — covers auth, daemon not running, rate limit, Linux notification daemon, macOS terminal-notifier, and stale state.
- [x] **Make `default_branches` settable via REST/MCP** — added `default_branches` to `Config`, `/defaults` GET/POST, TUI global config form (`C` key), and `branches_for` fallback.
- [x] **Standardize MCP tool descriptions** — all 12 tools now follow: summary line + defaults + gotchas format.
- [x] **Hide auto-managed config fields** — added note in README explaining `discovered_branches` is auto-managed and should not be edited by hand.
- [x] **Poll aggression guidance** — README now explains when to use low/high, what happens at 0% remaining, and reset behavior.

## Nice to have (post-1.0)

- [ ] REST API versioning strategy — mark endpoints as stable vs internal.
- [ ] Notification sound control — separate from level (some users want visual-only).
- [ ] Config file comments/schema — generate a JSON schema or annotated example config.
