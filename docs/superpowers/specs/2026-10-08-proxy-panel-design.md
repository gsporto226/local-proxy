# Claude Code proxy panel design

## Goal

Make `/proxy` a focused, keyboard-friendly account and usage panel that follows the Claude Code Settings screens.

## Panel behavior

- Open on Accounts and request 90 percent of available pane rows and columns.
- Keep Claude Code's native Pane. The host may cap or resize it.
- Use Claude's compact terminal style: top tabs, a highlighted active tab, short text rows, quota meters, and a keyboard hint line.
- Use Tab and Shift+Tab to move among tab and panel controls. Use Up and Down to move through controls. Press Enter to activate a control. Do not add numeric tab shortcuts.
- Claude Code accepts a user-level Pane binding for `tabs:next` and `tabs:previous` on built-in tabs. A throwaway v2.1.293 test showed that the same actions do not activate tab Buttons in a custom Mod Pane. Keep the native Pane navigation.
- Show a fuzzy account filter above the account groups. Match provider and alias names by case-insensitive character sequence. Press `f` to focus the Input. The Mods API can focus the Input from a Button hotkey.
- Group accounts by provider. Put providers with session-pinned accounts first, the default provider next, then sort the rest alphabetically. Show all groups expanded.
- Each account row shows its session/default marker, 5-hour and 7-day utilization, full local reset date and time, relative time to reset, and its cache hit rate.
- Selecting `Use for this session` pins that account to the active Claude Code session immediately. Keep the panel open. The Config tab continues to manage the persistent default account separately.

## Cache rate

A request is a cache hit when the upstream reports more than zero cached-read tokens. Persist the hit as `Option<bool>` in the requests table. `NULL` means the upstream did not report cache-read telemetry. `0` means a reported miss. `1` means a reported hit.

The cache object in stats JSON contains `hit_requests`, `reported_requests`, `rate_percent`, and `coverage_percent`. Calculate hit rate over reported requests. Calculate coverage over all requests in the aggregate. Use `null` for rate when no request reported telemetry. Do not count missing telemetry as a miss.

Add `session` as the initial Usage window. In that window, filter to the active Claude Code session with its `session_id`. Keep `day`, `week`, `month`, and `all` as proxy-wide windows. Show the overall and per-account cache rates on Usage. Show the per-account rate in Accounts.

## Data model choice

Store a nullable request-level Boolean rather than cached-read token counts. The requested measure weights each request once, so a Boolean records exactly what the UI needs. Keep `cache_read: Option<u64>` at the upstream boundary long enough to distinguish a missing field from a reported zero, then convert it to `Option<bool>` before persistence.

The architecture sketch compared storing the upstream token count with storing the request outcome. The request outcome is the base because it matches the requested rate and keeps the database smaller. The design keeps the other candidate's active-session filter and telemetry-coverage counts. It drops a separate all-session scope toggle. The existing Session, Day, Week, Month, and All windows choose that scope.

The UI keeps a native Pane. A focused Client can read raw arrows only after a click, and the built-in `tabs:next` action did not activate custom Mod Buttons in a live test. The native Pane uses Tab and Shift+Tab for its control order. A user-level `f` hotkey moves focus to the filter Input through `$.ui.focus`.

The stats API returns cache counts and percentages with the existing summary, provider, and account rows. The panel calls `/admin/stats?since=session&session_id=...` for the default window. It sends the existing `since` values for proxy-wide day, week, month, and all-time views.

Stats JSON includes a top-level `scope`. The Mod uses it to avoid presenting an older proxy's all-session response as the active-session view. If an older proxy omits the cache or scope fields, show an update message instead of a false rate.

## Data flow

`translate::TokenUsage.cache_read` becomes `Option<u64>` so parsing distinguishes absent telemetry from explicit zero. Non-stream capture and stream capture convert this value to `Option<bool>` and store it on `StatLine`. The stats module adds a nullable SQLite column, migrates old databases without backfilling, and computes hit and coverage counts by time window, session, provider, and account. The existing `/admin/stats` endpoint accepts the session scope and returns cache summaries. The Mod renders those values in Accounts and Usage.

For the Responses upstream path, pass the decoded `TokenUsage` to capture before the response serializer fills absent cached-token fields with zero.

## Verification

- Test missing, zero, and positive cache-read telemetry in parsing and streaming aggregation.
- Test nullable request persistence, idempotent schema migration, session filtering, account grouping, and cache-rate coverage.
- Test `/admin/stats?since=session&session_id=...` and its validation errors.
- Test the Mod's default tab, window request, filter, account grouping, quota formatting, and cache rendering with `claude plugin test`.
- Run the full Rust and Bun checks listed in `AGENTS.md`.
