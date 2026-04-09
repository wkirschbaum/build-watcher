# Road to 1.0.0

## Must do

- [x] **Empty-state TUI guidance** — when no watches exist, show "Press `a` to add a repo" instead of a blank screen.
- [x] **Complete keybindings table in README** — `c`, `C`, `t`/`T` added. Both README and docs/tui.md match reality.
- [x] **Document PR watch feature** — added PR Watch section to README with badge meanings and `M` merge key.
- [x] **Handle missing `gh` CLI gracefully** — checks `GITHUB_TOKEN` env var first, falls back to `gh auth token`, gives helpful error with install link if neither works.
- [x] **Document `show_author` API cost** — documented in README config table. Already exposed in TUI global config form (`C` key).
- [x] **Contextual error messages** — GhError messages updated to be transport-agnostic. Token errors include hints about `gh auth login` and `GITHUB_TOKEN`.

## Should do

- [ ] **Troubleshooting section in README** — cover common issues: API rate limit exhausted, `gh auth login` not run, notification daemon not running (Linux), `terminal-notifier` not installed (macOS).
- [ ] **Make `default_branches` settable via REST/MCP** — currently only editable by hand in `config.json`. The `/defaults` POST endpoint and global config form should support it for consistency.
- [ ] **Standardize MCP tool descriptions** — some are one-liners, others are paragraphs. Each tool should have a consistent format: summary line + key defaults + gotchas.
- [ ] **Hide auto-managed config fields** — `discovered_branches` in `RepoConfig` is not user-editable but appears in `config.json`. Document it as auto-managed or move it to state.
- [ ] **Poll aggression guidance** — README should explain when to change from "medium" default, what happens at 0% remaining, and how to tune for many repos.

## Nice to have (post-1.0)

- [ ] REST API versioning strategy — mark endpoints as stable vs internal.
- [ ] Notification sound control — separate from level (some users want visual-only).
- [ ] Config file comments/schema — generate a JSON schema or annotated example config.
