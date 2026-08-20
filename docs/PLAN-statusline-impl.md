# Implementation Plan: status line + session cost (from docs/PLAN-statusline.md)

Goal: render the **current Claude Code session's** real usage/cost in the status
line, driven by stats the proxy itself records (`stats.db`), not Claude's
built-in client-side pricing.

Loop: `X-Claude-Code-Session-Id` header on each `/v1/messages` request → proxy
stores `session_id` per request → status line script passes its `session_id` +
a template → `local-proxy statusline` renders a Rhai template from that
session's aggregated stats.

## 1. `src/translate.rs` — cost in `TokenUsage`

- Add `cost_usd: Option<f64>` to `TokenUsage`.
  - `Copy` is preserved (`Option<f64>` is `Copy`); drop `Eq` (f64 has no `Eq`),
    keep `PartialEq`. Update the derives only; `== TokenUsage::default()` sites
    use `PartialEq` and stay valid.
- In `parse_usage`, after reading tokens, scan a small allow-list for a reported
  cost and store the first match found:
  - `usage.cost` (OpenAI/OpenRouter-style) → `as_f64`.
  - `prompt_cost` + `completion_cost` both present (OpenRouter) → their sum.
  - else `None`. **Never synthesized** (no pricing table).
- In `merge_usage` (translate.rs), propagate the first non-`None` cost
  (streaming cumulative), mirroring the token `max` logic.
- `streams.rs`'s own `const fn merge_usage` (line 290) must also propagate cost
  (const-compatible match/assignment) so translated streaming merges it too.

## 2. `src/stats.rs` — schema + session queries

- Schema (`ensure_schema` + CREATE TABLE): add
  - `session_id TEXT NOT NULL DEFAULT ''`
  - `cost_usd REAL` (nullable)
- Migration: extend the existing guarded `pragma_table_info` column loop (the
  pattern already used for `energy_kwh_um`/`cost_usd_um`) so prior `stats.db`
  files gain both columns without losing rows. Keep micro-unit `cost_usd_um`
  for existing energy/cost reporting; for the status line we read the REAl cost.
  (Simplest: store `cost_usd` as the same `to_um` micro-unit INTEGER to avoid two
  cost flows. **Decision: reuse `cost_usd_um` micro-units** for session cost, and
  keep the existing cost reporting untouched. Add only `session_id` column;
  the existing `cost_usd_um` already holds cost.)
  - Revisit: the plan asks for `cost_usd REAL` + `SUM(cost_usd)`. I'll add
    `session_id` only and query `SUM(cost_usd_um)` on it — one canonical cost
    column, micro-units, consistent with current stats.
- `StatLine` gains `session_id: String` (default `""`); `write_line` inserts it.
- `RequestRow` gains `session_id`; `recent_on` selects it.
- New query `session(session_id)` returning a struct with:
  `requests`, `tokens_in`, `tokens_out`, `cost_usd` (sum of `cost_usd_um`
  divided-back to f64), `cost_known_requests` (count of rows with non-NULL cost),
  and `last_model` (most recent model for the session). Returns `None` when the
  db doesn't exist / no rows.
- Month/total cost params (for the template) query the same aggregates over
  windows; a small `sum_cost(window) -> Option<f64>` + `cost_known(window)`.
  Reuse existing `summary_on`.

## 3. `src/handlers.rs` — capture the header

- Read `X-Claude-Code-Session-Id` from the inbound `HeaderMap` (case-insensitive,
  via a `headers.get("x-claude-code-session-id")`).
- Thread `session_id: &str` into:
  - `capture(...)` → `StatLine.session_id`
  - `StreamCapture::new(...)` (and `StreamCapture.record` → `StatLine.session_id`)
  - the three non-stream `capture()` calls for `/v1/messages`,
    `/v1/chat/completions`, `/v1/responses`.
- Default to `""` when absent (curl/tests/other clients).

## 4. New `src/statusline.rs` — Rhai template renderer

- New module declared in `lib.rs`.
- `statusline::render(template: &str, params: &HashMap<String,String>) -> String`
  - Compile the template as a Rhai script; bind each param as a variable
    (numbers as `f64`, strings as `String`).
  - Evaluate; convert result to a string.
  - **On any parse/run error**: log via `tracing` and return a static fallback
    (e.g. `"statusline: erro"`). Never panic, never exit non-zero (so the
    status line keeps rendering).
- Unit tests: arithmetic, unknown/no-data param, division, cost-absent.

## 5. `src/cli.rs` — `statusline` command + config block

- Add `Config` field `statusline: Option<StatuslineConfig>` in `config.rs`
  (`template: Option<String>`). `#[serde(default)]` so existing configs parse.
- New CLI function `statusline(config_path, session, model, context_pct, template)`:
  - `--session` (required-ish; empty → render anyway with no-data params),
    `--model`, `--context-pct` optional flags, `--template` overrides config.
  - Load effective config for `statusline.template` (flag > config).
  - Query `stats::session(session_id)` + `sum_cost` month/total.
  - Build params map (raw values only — formatting is the user's job):
    `cost_session`, `cost_month`, `cost_total`, `cost_known`, `tokens_in`,
    `tokens_out`, `requests`, `model`, `context_pct`.
    Unknown → a no-data marker (e.g. `"?"` / `"-"`), decided by template.
  - Call `statusline::render` and print the line.
- Add `CliError` variant if needed (reuse `Stats` for db errors; new `Statusline`
  only if a distinct message helps).
- `main.rs`: add `Statusline` subcommand (flags: `--session`, `--model`,
  `--context-pct`, `--template`).

## 6. Config + docs + scripts

- `config.example.yaml`: add commented `statusline:` block example.
- README / docs: section on wiring via `~/.claude/settings.json` `statusLine`,
  with a sample script (`scripts/statusline.ps1` + `.sh`) that reads the status
  line JSON on stdin, extracts `session_id`, and calls
  `local-proxy statusline --session "$id" --template "$TPL"`.
- Add a `statusline:` sample to `PENDING.md` / release notes.

## 7. Dependencies

- Add `rhai = { version = "1", default-features = false }` (scripting-free,
  sandboxed by default; no file/network/host access). The plan's default.

## Validation

- `cargo test --all-features` — add unit tests for:
  - cost parsing (`parse_usage` with `cost`, `prompt_cost`+`completion_cost`),
  - stats session query + schema migration for `session_id`,
  - statusline render (arithmetic, unknown param, division, cost-absent),
  - header extraction in handlers (existing test infra).
- `cargo clippy --all-targets --all-features -- -D warnings` clean.
- `cargo fmt --all -- --check` clean.
- `bun test e2e/mock.test.ts` still green.
- Manual: configure a `statusLine`, run a session through the proxy, confirm the
  line reflects the session's real stats.
