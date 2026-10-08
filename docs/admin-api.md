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
| `GET /admin/stats?since=day\|week\|month\|all[&session_id=...]` | same JSON as `local-proxy stats --json`; optional session filter; `null` before any stats exist |
| `GET /admin/stats?since=session&session_id=...` | Stats for the full Claude Code session; `session_id` is required |
| `GET /admin/rate-limits` | `{ h5, week }` (percent, nullable) |
| `GET /admin/account-usage` | Account quota snapshots for ChatGPT and Claude subscriptions. Results are cached for 60 seconds. |
| `GET /admin/logs?n=200` | `{ lines: [string] }` |
| `GET /admin/session/{id}` | `{ account: { provider: alias }, effort, stats }` for that session |

## Writes

| Route | Body | Effect |
| --- | --- | --- |
| `PUT /admin/session/{id}/account` | `{ provider, alias }` | pin account for that session (in-memory) |
| `DELETE /admin/session/{id}/account/{provider}` | – | unpin |
| `PUT /admin/model` | `{ model: string \| null }` | same as `local-proxy model` / `model clear` |
| `PUT /admin/effort` | `{ effort: string \| null }` | same as `local-proxy effort` |
| `PUT /admin/account` | `{ provider, alias } \| { provider, clear: true }` | persisted default account |
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
| `request` | one `requests` row: `{ ts, provider, model, input_tokens, output_tokens, status, latency_ms, cost_usd, session_id }` |
| `log` | `{ line }` |
| `config` | `{ model, effort }` after any write (API or CLI-in-process) |
| `rate_limits` | `{ h5, week }` |

The session id is Claude Code's `X-Claude-Code-Session-Id`, which equals the
mod's session id.
