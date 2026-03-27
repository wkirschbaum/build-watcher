# Plan: Build History in `bw` TUI

**Feature:** Show a history of previous builds in the `bw` TUI dashboard.

## UX

- Press **`h`** on a selected row → opens a full-screen overlay popup for that repo/branch
- Press **`H`** → same but without branch filter (whole repo history)
- The popup opens immediately in a "loading" state; history fetches async in the background
- Inside the popup: `↑`/`↓`/`j`/`k` scroll, `o` opens the selected run in browser, `Esc` closes

Popup layout:
```
┌─ History: floatpays/moneyclub @ main ──────────────────────────────────────┐
│  STATUS     WORKFLOW  TITLE                              DURATION  AGE      │
│  ✅ success  CI        Fix login bug                      3m 12s    2h ago   │
│  ❌ failure  CI        Update deps                        1m 05s    5h ago   │
│  ✅ success  CI        Add feature                        4m 30s    1d ago   │
│                                                                              │
│  [↑↓] scroll  [o] open  [Esc] close                                         │
└──────────────────────────────────────────────────────────────────────────────┘
```

## Files to Change

1. **`src/status.rs`** — add `HistoryEntryView` struct (serializable view type)
2. **`src/server.rs`** — add `GET /history?repo=&branch=&limit=` REST endpoint
3. **`src/bin/bw.rs`** — TUI changes:
   - `DaemonClient::get_history()`
   - `InputMode::History { repo, branch, entries, selected, scroll_offset, loading }`
   - `SseUpdate::EnterHistory { repo, branch, entries }`
   - `SseUpdate::HistoryError { flash }` (to close popup on failure)
   - `handle_input` arms for `InputMode::History`
   - `handle_normal_key` arms for `h` and `H`
   - `render_history_popup()` function
   - Footer: add `[h/H] history` hint; hide footer in history mode

## New REST Endpoint

```
GET /history?repo=owner/repo&branch=main&limit=15
```

Response: `Vec<HistoryEntryView>` (JSON array), HTTP 502 on `gh` failure.

## New Types

### `HistoryEntryView` (in `src/status.rs`)
```rust
pub struct HistoryEntryView {
    pub id: u64,
    pub conclusion: String,
    pub workflow: String,
    pub title: String,
    pub branch: String,
    pub event: String,
    pub created_at: String,
    pub updated_at: String,
    pub duration_secs: Option<u64>,
    pub age_secs: Option<u64>,
}
```

## Open Questions / Tradeoffs

- **Latency:** `gh run list` can be slow — loading state in popup handles this.
- **Error handling:** On failure, close popup and show flash message.
- **Caching:** Fresh `gh` call per `h` press — acceptable for now; LRU cache (60s TTL) is a future improvement.
- **Scroll offset:** Store `scroll_offset: usize` in `InputMode::History` alongside `selected`; render function slices `entries` to only visible rows (pure render, no mutable widget state).
- **`h`/`H` split:** `h` = this branch, `H` = whole repo (no branch filter).
