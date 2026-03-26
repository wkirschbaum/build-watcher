# Feature Ideas

Personal desktop tool — all features should improve the local daily experience.

## High Impact, Low Effort

### Auto-watch from git remote
Detect `origin` remote of the current working directory and auto-suggest watching it via MCP. Saves the manual `owner/repo` lookup.

### Per-repo/branch snooze
Snooze a specific repo or branch for N minutes without silencing all notifications globally. Finer control than the current global pause. (Branch-level mute and per-event level overrides are now supported via the TUI `n`/`N` keys and `POST /notifications`; timed snooze with automatic expiry is still unimplemented.)

### Workflow filtering by event type
Ignore runs triggered by specific GitHub events (e.g. `schedule`, `dependabot`) globally or per-repo. Reduces noise from automated runs that don't need human attention.

### Author in notifications
Show the commit author or triggering user in the notification body (e.g. last line: "by Kynan Ware"). **Limitation:** `gh run list --json` does not expose author or actor fields. The data is available via `gh api repos/{owner}/{repo}/actions/runs/{id}` (`head_commit.author.name` and `triggering_actor.login`), but that requires one extra API call per newly detected run — too expensive given rate-limit constraints. Feasible once we track per-run state and can batch the lookup.

## Medium Effort

### Failure streak alert
Detect when a branch has failed N times in a row and send a distinct sticky notification (e.g., "main has failed 5 times in a row"). Helps catch broken branches that need attention.

### Build duration trends
Track average build duration per workflow over time. Warn when a running build exceeds the typical duration significantly (e.g., "CI is 2x slower than usual").

### Auto-watch on `gh pr create`
Integrate with the `gh` CLI workflow — automatically start watching the PR branch when a pull request is created, and stop when it is merged or closed.

### Watch all repos in a GitHub org or team
Single command to watch a curated set of repos (e.g. all repos in a GitHub team) rather than adding them one by one.

### Multi-account support
Support separate GitHub accounts (personal + work) by routing `gh` CLI calls through per-account configurations. Currently assumes a single authenticated account.

## Ambitious

### Log streaming in TUI
Pipe live build logs via `gh run view --log` into the TUI dashboard. Useful for watching a failing build without leaving the terminal.

### Smart auto-rerun
Automatically retry builds that fail with known transient errors (network timeouts, rate limits). Configurable max retry count and pattern matching on failure messages.

### Flaky test detection
Track which tests fail intermittently across runs and surface a summary. Helps identify unreliable tests that need fixing.

### Weekly digest
A scheduled summary notification (or MCP report) showing pass rate, average duration, and flakiest workflows per repo over the past week.
