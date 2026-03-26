# `bw` TUI Plan

A top-like live terminal dashboard for build-watcher.

---

## Status — all phases complete ✅

### Phase 1 — Basic live display ✅

ratatui table, 1s polling, colour coding, `q` quit, `⏸ PAUSED` header indicator, failing-steps sub-row, column truncation.

### Phase 2 — SSE real-time updates ✅

SSE background task subscribes to `GET /events`; events applied in-place via `apply_event`. Reconnects with exponential backoff (1s → 2s → … → 30s), resetting after each successful connection. `/status` resync on every (re)connect and every 30 s as a fallback. Header shows `⚡ reconnecting (Xs)` when disconnected.

### Phase 3 — Navigation and actions ✅

Row selection (`↑`/`↓`/`j`/`k`) with highlight. `r` reruns the selected build via `POST /rerun`, `o` opens the run URL in the browser (`xdg-open` / `open`), `p` toggles notification pause via `POST /pause`. Flash messages in the header for action feedback.

### Phase 4 — Polish ✅

Completed build age ("3m ago", "2h ago") with local ticking, responsive column widths scaling to terminal width, terminal resize handling.

---

## Keybindings

| Key | Action |
|-----|--------|
| `↑` / `k` | Move cursor up |
| `↓` / `j` | Move cursor down |
| `r` | Rerun selected build |
| `o` | Open selected run in browser |
| `p` | Toggle pause notifications |
| `q` | Quit |

---

## Verification

```bash
cargo fmt && cargo clippy && cargo test
cargo run --bin bw
```
