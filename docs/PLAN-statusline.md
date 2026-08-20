# Plan: Template-driven status line + session cost

## Context

We want to display live usage and cost for the *current* Claude Code session in
the Claude Code **status line**. Claude Code's built-in cost badge is computed
client-side from its own model prices, which is wrong when traffic routes
through this multi-provider proxy (OpenRouter, Groq, DeepSeek, …). Instead we
drive the status line from the proxy's own recorded stats, so the numbers are
the real ones.

Empirically verified:

- The policy CLI sends `X-Claude-Code-Session-Id` (a per-session UUID) on every
  `/v1/messages` request. This is what the proxy sees.
- The status line receives the **same UUID** as `session_id` on stdin. I
  confirmed identity by running the real `claude` CLI against a local capture
  server + a logging status line in a real TTY: header value
  `f0f59f0d-…d23bb` == status line `session_id` `f0f59f0d-…d23bb`.
- The status line is configured via `"statusLine": { "type": "command",
  "command": "<script>" }` in `settings.json`. The script gets JSON on stdin,
  prints a line; re-runs per assistant message (300 ms debounce) + optional
  `refreshInterval` (min 1 s).

So the loop is: `X-Claude-Code-Session-Id` header → proxy stores `session_id`
with each request → status line script passes its `session_id` to the proxy →
proxy renders a template line from that session's stats → script prints it.

## Decisions locked

- **No pricing table.** Cost is only shown when the *upstream reports it*. We
  never estimate cost from a token-price table; if a provider returns no cost,
  that field is simply absent from the render.
- **Tagged nullable cost.** `TokenUsage` gains an optional-ish cost that is
  best-effort: it is present only when a recognized cost field is found in the
  upstream usage (some OpenAI-compatible providers, e.g. OpenRouter, Groq) and
  absent otherwise. Never synthesized.
- **Sandboxed script template via an embedded language.** The template is
  evaluated as a **Rhai** script inside the proxy. Rhai is isolated by default
  (no file/network/host access unless explicitly registered) so a template can
  do arithmetic (`+ - * / %`), `floor`/`ceil`/`int` casts, comparisons and full
  boolean algebra (`&&`, `||`, `!`, `==`, `<`…), and `if` expressions — but is
  never arbitrary code. Named params are bound as variables; the proxy decides
  which exist. `evalexpr` is noted as a lighter fallback if we prefer a
  calculator-level expression language over Rhai's scripting subset.
- **OS invariant.** The template is data, evaluated by a single code path in
  the binary. The status line shell (bash / PowerShell / cmd) only invokes the
  binary and prints the result, so nothing is OS-specific.
- **Proxy is the single source of cost/token truth.** The status line supplies
  session-level UI fields (session id, model, cwd) from its stdin; the proxy
  supplies stats/cost. Script = pass session id + template, print rendered

## Design

### 1. Cost capture in `src/translate.rs`

Extend usage parsing to also read a reported cost when present:

- `TokenUsage` keeps `input` / `output` / `reasoning` (token counts).
- Add a separate, nullable cost value next to it (e.g. `cost_usd: Option<f64>`)
  populated only when a recognized provider cost field is found. Field names
  differ by provider; we scan a small allow-list (e.g. OpenAI/OpenRouter-style
  `cost`, `prompt_cost` + `completion_cost`, Groq-style) and total when both
  parts exist. Unknown → `None`.
- `merge_usage` propagates the first non-`None` cost (streaming cumulative).

`parse_usage`, `usage_from_frame` and the SSE merge path all feed this, so both
non-streaming and streaming requests capture cost as the same `TokenUsage`.

### 2. Persist in `src/stats.rs`

- Add nullable-ish columns: `session_id TEXT NOT NULL DEFAULT ''` and
  `cost_usd REAL` (nullable) to `requests`.
- `StatLine` gains `session_id: String` and `cost_usd: Option<f64>`; threaded
  through `INSERT` and the `SELECT`s (summary / by_provider / recent) so
  aggregators can `SUM(cost_usd)`.
- `session_id` is stored per request; `cost_usd` is `NULL` when unknown.
- `ensure_schema` uses `CREATE TABLE IF NOT EXISTS` — existing `stats.db` files
  won't gain new columns automatically. Decide: bump to a fresh schema/side
  table, or run `ALTER TABLE ... ADD COLUMN` guarded by a column-exists check.
  (Prefer guarded `ALTER` so prior data survives.)

### 3. Capture the header in `src/handlers.rs`

- In the request handlers that record stats, read
  `X-Claude-Code-Session-Id` from the inbound `HeaderMap` and attach it to the
  `StatLine` sent to `stats::record`.
- Default to `""` when absent (non-policy clients, curl, tests).

### 4. Render a single line via a new CLI command

Add a small command (subcommand of `local-proxy`) that the status line script
calls, e.g. `local-proxy statusline` (or a `--template` flag on a stats query):

- Inputs (stdin or flags): the `session_id`, optionally `model` / `cwd` from
  the status line JSON, and the template string (from config or `--template`).
- Computes parameters from `stats.db` by `session_id`:

  | param | meaning |
  |---|---|
  | `cost_session` | `SUM(cost_usd)` for this session (USD, may be absent) |
  | `cost_month` | `SUM(cost_usd)` this calendar month |
  | `cost_total` | `SUM(cost_usd)` all time |
  | `tokens_in`, `tokens_out` | sums for this session |
  | `requests` | request count for this session |
  | `context_pct` | from status line stdin (context_window.used_percentage) |
  | `model` | from status line stdin (model.display_name) |
  | … | any other status-line field we choose to surface |

  Parameters sourced from the status line stdin are passed through by the
  script; parameters sourced from `stats.db` are computed by the proxy.

- Evaluates the template (a **Rhai** script) with the params bound as
  variables; `floor`/`trunc`-style ops and boolean algebra are native. Params
  that are unknown / non-numeric bind to a "no data" marker. Result is a
  string, printed as the status line.
- Prints the rendered line. The status line script captures stdout and prints
  it (or the command itself *is* the status line, on a single line).

### 5. Template config + sample status line

- Template lives in the proxy config (e.g. a `statusline:` block) **or** is
  passed on the CLI; a `--template` flag **overrides** any config block
  (flag > config). Formatting of the output (currency symbol, rounding, the
  placeholder for unknown cost) is **entirely up to the user's template** —
  the proxy exposes raw numbers/strings and the user formats them.
- Sample: `"{model} · {cost_session} · {context_pct}% ctx"`
- Sample status line script (OS-invariant, any shell):
  `scripts/statusline.sh` / `.ps1` that reads stdin, extracts `session_id`,
  and calls `local-proxy statusline --session "$id" --template "$TPL"`.
- Docs: README section + `docs/` notes; mention wiring via
  `~/.claude/settings.json` `statusLine` (or `/statusline`).

### 6. Where user data is computed

`SUM(cost_usd)` is only ever grouped over rows that carry a non-`NULL` cost; a
provider that reports none contributes 0 to the sum but we can also report e.g.
`cost_session_known` (requests with known cost) so the user can tell an
accurate total from a partial one.

## Notes / risks

- **Cost is partial by design.** A mixed session (some providers report cost,
  some don't) yields a sum that covers only the cost-`Some` rows. We surface the
  number of requests whose cost is known so the figure isn't mistaken for a
  complete bill.
- **Schema migration** of an existing `stats.db` must be guarded (column-exists
  check) to avoid breaking prior stats.
- **Streaming cost** relies on the final usage frame carrying cost; the same
  merge path as tokens handles it.
- **No pricing table** means cost renders blank for Anthropic etc. This is a
  deliberate product decision, not a limitation to fix later.

## Validation

- `cargo test --all-features`
- `cargo clippy --all-targets --all-features -- -D warnings`
- `cargo fmt --all -- --check`
- Manual: configure a `statusLine`, run a real session through the proxy, and
  confirm the rendered line reflects this session's stats.
- e2e: keep `bun test mock.test.ts` green; add a template-evaluation unit test
  (arithmetic, unknown param, division, cost-absent).

## Open questions for implementation

1. **Resolved:** the status line command reads the status line JSON on stdin
   and takes `--template` (flag), which overrides any config `statusline:`
   block. Thin wrapper language (Rhai) inside the proxy — **adopted.**
2. **Resolved:** `--template` flag overrides the config block.
3. **Resolved:** formatting is user-defined in the template; the proxy exposes
   raw values only.

Remaining minor decisions while implementing: error handling for a template
that fails to parse/run (a static fallback string + logged error), and exact
Rhai verse `evalexpr` if total dependency size matters more than scripting
power (I default to Rhai).
