# Admin API

Served by the running proxy under `/admin`. **Loopback only**: requests whose
peer address is not loopback get `403`. No API key. JSON in/out.

Port: `LOCAL_PROXY_PORT` env (set by `launch`, honoured by `serve`), default
`8787`. The Claude Code mod finds the proxy through Claude's own
`ANTHROPIC_BASE_URL` (loopback only) and stays inert unless `/admin/status`
answers there.

Stats responses add a `cache` object to `summary`, each provider row, and each
account row. They also include `scope`, either `{ kind: "all" }` or
`{ kind: "session", session_id }`. The cache object contains `hit_requests`,
`reported_requests`, `rate_percent`, and `coverage_percent`. A request is a cache hit when the upstream reports more
than zero cached-read tokens. Requests without cached-read telemetry are
excluded from the rate denominator. `rate_percent` is `null` when no requests
reported cache telemetry; `coverage_percent` is `null` when there are no
requests in the aggregate. Older database rows remain unreported, not misses.

The cache object also includes `read_tokens`, `write_tokens`,
`reported_input_tokens`, `uncached_tokens`, and `read_percent`. Token totals
cover only rows whose cache-read count was reported. `read_percent` is
`100 * read_tokens / reported_input_tokens`, or `null` when that denominator
is zero. `uncached_tokens` subtracts cache reads and writes from that
denominator, saturating at zero. Requests without telemetry cannot dilute
the token reuse percentage.

Recent rows include nullable `cache_read_tokens`, `cache_write_tokens`, and
`cache_diagnostics`, an array of evidence codes: `missing_cache_control`,
`missing_session_key`, `stable_prefix_changed`, `model_changed`,
`account_changed`, `cache_key_changed`, `telemetry_unavailable`,
`cache_miss_unknown`, `cache_warming`, and `upstream_error`. These describe
observations, not a guaranteed upstream explanation. Existing database rows
have an empty diagnostic array. No prompt or credential contents are stored.

`since=session` selects the full Claude Code session and requires `session_id`.
Other windows can also take `session_id` to filter their time range to one
session. Omitting `session_id` keeps the existing all-sessions behavior.

## Reads

| Route | Response |
| --- | --- |
| `GET /admin/status` | `{ version, port, pid, model, effort }` |
| `GET /admin/accounts` | `[{ provider, alias, kind, is_default }]` (`kind`: `api`\|`oauth`) |
| `GET /admin/providers` | `[{ name, format, accounts: [<account as above>] }]` (catalog + config, same data as `local-proxy providers`) |
| `GET /admin/models` | `[string]` |
| `GET /admin/model-context?model=<id>` | `{ provider, model, context_window }` from the running router; the window is a token count or `null` when unknown. Accepts gateway IDs and route aliases. |
| `GET /admin/stats?since=day\|week\|month\|all[&session_id=...]` | same JSON as `local-proxy stats --json`; optional session filter; `null` before any stats exist |
| `GET /admin/stats?since=session&session_id=...` | Stats for the full Claude Code session; `session_id` is required |
| `GET /admin/rate-limits` | `{ h5, week }` (percent, nullable) |
| `GET /admin/account-usage` | Account quota snapshots for ChatGPT and Claude subscriptions. Results are cached for 60 seconds. |
| `GET /admin/logs?n=200` | `{ lines: [string] }` |
| `GET /admin/session/{id}` | `{ account: { provider: alias }, effort, stats }` for that session |

## Writes

| Route | Body | Effect |
| --- | --- | --- |
| `PUT /admin/session/{id}/account` | `{ provider, alias }` | set that session's account snapshot (stored; the selection is unchanged) |
| `DELETE /admin/session/{id}/account/{provider}` | – | drop the snapshot; the next request takes the selection |
| `PUT /admin/model` | `{ model: string \| null }` | same as `local-proxy model` / `model clear` |
| `PUT /admin/effort` | `{ effort: string \| null }` | same as `local-proxy effort` |
| `PUT /admin/account` | `{ provider, alias }` | selected account of the provider (always set; no clear) |
| `DELETE /admin/accounts/{provider}/{alias}` | – | same as `local-proxy disconnect` |

Successful writes return `{ message }` (the CLI's message); session pin
routes return the session's resulting `{ provider: alias }` map.

`GET /admin/account-usage` returns entries tagged `available` or `unavailable`.
Available entries contain the account's 5-hour, 7-day, and optional monthly
extra-usage windows. Each window contains `utilization` and `resets_at`.
Unavailable entries contain the fetch error. A stale available entry includes
the last good snapshot and the latest refresh error.

`connect` is intentionally absent: credentials only via the CLI.

Errors: `{ error: string }` with 4xx/5xx.

## Events

`GET /admin/events` — `text/event-stream`, one stream, typed by `event:`:

| event | data |
| --- | --- |
| `request` | one `requests` row: `{ ts, provider, model, input_tokens, output_tokens, status, latency_ms, cost_usd, session_id, cache_read_tokens, cache_write_tokens, cache_diagnostics }` |
| `log` | `{ line }` |
| `config` | `{ model, effort }` after any write (API or CLI-in-process) |
| `rate_limits` | `{ h5, week }` |

The session id is Claude Code's `X-Claude-Code-Session-Id`, which equals the
mod's session id.
