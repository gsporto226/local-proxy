# Proxy panel UX implementation plan

> **For agentic workers:** Follow this plan task by task. Keep each test and implementation change in a verified state before starting the next task.

**Goal:** Make the Claude Code Mod panel a focused account and usage view with session cache hit rates.

**Architecture:** Preserve nullable cache-hit telemetry per request in SQLite. Aggregate it through the existing stats JSON and admin endpoint, with optional session scope. Rework the existing Mod Pane to render provider groups, fuzzy account search, quotas, and cache rates.

**Tech stack:** Rust, SQLite, Axum, TypeScript TSX, Claude Code Mods API, `claude plugin test`, Bun e2e.

**Spec:** `docs/superpowers/specs/2026-10-08-proxy-panel-design.md`

## Global constraints

- Every public Rust item needs `///` documentation because `missing_docs = "deny"`.
- Cache telemetry must distinguish missing fields from reported zero.
- Session and time filters must use SQLite parameters.
- Tests that change `LOCAL_PROXY_CONFIG_DIR` must hold `TEST_STATE_LOCK` and use a temporary directory.
- Do not commit or push without the user asking.

---

### Task 1: Preserve cache-read telemetry

**Files:** `src/translate.rs`, `src/ir/mod.rs`, `src/ir/tests.rs`, `src/handlers.rs`, `src/streams.rs`.

**Interfaces:** `TokenUsage.cache_read: Option<u64>` is `None` when no cached-read field was reported. `TokenUsage::cache_hit()` returns `None` for missing telemetry, `Some(false)` for zero, and `Some(true)` for positive counts.

- [x] Add parser coverage for absent, zero, positive, and cache-creation-only upstream usage.
- [x] Verify the parser test fails before changing `TokenUsage`.
- [x] Change `parse_usage`, `merge_usage`, `Emitter::usage`, and protocol serializers to preserve optional telemetry.
- [x] Pass decoded Responses usage directly to capture before response serialization.
- [x] Run `cargo test --all-features cache`.

### Task 2: Persist and aggregate request hit state

**Files:** `src/stats.rs`, `src/cli.rs`, `src/admin.rs`, `src/handlers.rs`, `src/streams.rs`, `docs/admin-api.md`.

**Interfaces:** `StatLine.cache_hit: Option<bool>`. `StatsFilter` combines `TimeWindow` with `StatsScope::{All, Session(&str)}`. `CacheStats` carries hit and telemetry request counts.

- [x] Write a failing stats test for missing, miss, and hit rows grouped by session and account.
- [x] Add the nullable `cache_hit` column with an idempotent legacy-schema migration.
- [x] Persist `Option<bool>` and calculate counts in the existing summary, provider, and account queries.
- [x] Add cache rates and coverage to stats JSON without exposing storage details.
- [x] Add `session_id` to the admin stats query. Require it for `since=session` and reject empty IDs.
- [x] Test cache JSON, the session endpoint filter, and missing-telemetry semantics.
- [x] Run the focused Rust tests.

### Task 3: Rebuild the Claude Code panel

**Files:** `claude-mod/hooks/register.tsx`, `claude-mod/hooks/register.test.tsx`, `claude-mod/types/index.d.ts`.

**Interfaces:** `ProxyCacheStats` carries hit count, reported count, rate, and coverage. The Mod's window type is `session | day | week | month | all`.

- [x] Add failing tests for the Accounts default, 90-percent pane request, fuzzy matching, quota reset formatting, and cache rate display.
- [x] Update the panel to request 90 percent of viewport dimensions and open on Accounts.
- [x] Add provider grouping, the `f`-focused fuzzy Input, compact quota rows, and per-account cache rates.
- [x] Add overall and per-account cache rates to Usage. Make Session the initial window.
- [x] Keep the panel open after session account changes and leave persistent default changes in Config.
- [x] Run `claude plugin test`.

### Task 4: Verify and review

**Files:** `src/cli.rs`, `src/admin.rs`, `claude-mod/hooks/register.test.tsx`, and `e2e/mock.test.ts` for verification.

- [x] Run `cargo fmt --all -- --check`.
- [x] Run `cargo clippy --all-targets --all-features -- -D warnings`.
- [x] Run `cargo test --all-features`.
- [x] In `e2e/`, run `cargo build --manifest-path ../Cargo.toml`, `bun install`, and `bun test mock.test.ts`.
- [x] Run `claude plugin validate` on `claude-mod` and `claude plugin test`.
- [x] Inspect the complete diff and confirm the original `.claude/` worktree remains untouched.
