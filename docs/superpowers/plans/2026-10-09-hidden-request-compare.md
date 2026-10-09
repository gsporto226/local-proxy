# Hidden Request Comparison Implementation Plan

> **For agentic workers:** Execute inline task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a hidden generic CLI command that compares direct and proxy-prepared requests and reports cache usage without persisting prompts.

**Architecture:** Factor the outbound request preparation into a shared function used by both `handlers::handle_chat` and the comparer. A new comparison module loads a fixture, validates provider/model/format and live confirmation, sends direct and proxy-prepared variants using the same provider client, then prints only a redacted structural diff and response usage. Every live comparison requires a fixture placeholder so each arm receives a distinct cache prefix.

**Tech Stack:** Rust 2021, clap, serde_json, reqwest, existing upstream client and IR/stream parsers.

**Spec:** `docs/superpowers/specs/2026-10-09-hidden-compare-command-design.md`

## Global Constraints

- Run `cargo fmt --all -- --check`.
- Run `cargo clippy --all-targets --all-features -- -D warnings`.
- Run `cargo test --all-features`.
- Run e2e checks: `cargo build --manifest-path ../Cargo.toml`, `bun install`, and `bun test mock.test.ts` from `e2e/`.
- Every public item requires `///` documentation (`missing_docs = "deny"`).
- Comparison requests must not write to the real stats database or print prompt/response bodies.

---

### Task 1: Shared proxy request preparation

**Files:**
- Modify: `src/handlers.rs`
- Test: `src/handlers.rs` unit tests

**Interfaces:**
- Produce `pub(crate) fn prepare_upstream_body(client_format: Format, provider: &crate::config::Provider, upstream_model: &str, body: Value, effort: Option<&str>, session_id: &str, streaming: bool) -> Result<Value, crate::translate::TranslateError>`.
- `handle_chat` must call this function for all forwarded requests, preserving current model rewriting, same-format normalization/translation, streaming usage setup, and Responses preparation.

- [ ] Add tests for same-format Anthropic preserving cache markers while removing unsupported knobs, and Responses setting cache key/stream defaults.
- [ ] Run `cargo test --all-features prepare_upstream_body` and confirm the new API is missing.
- [ ] Extract the existing transformations into the shared function and update `handle_chat` to call it.
- [ ] Re-run the targeted test and verify both new cases pass.

### Task 2: Safe response metrics and redacted request comparison

**Files:**
- Create: `src/compare.rs`
- Modify: `src/lib.rs`
- Modify: `src/upstream.rs`
- Test: `src/compare.rs` unit tests

**Interfaces:**
- Produce pure helpers that summarize two JSON request bodies without emitting prompt text and calculate cache-read percentage from `translate::TokenUsage`.
- Produce an async runner that accepts a resolved `ProviderClient`, native `Format`, upstream model, fixture body, repeat count, and compare tag, and returns structured per-arm response summaries.
- Use existing `sse_frames`, `usage_from_frame`, and `merge_usage` to collect usage from streamed responses; parse JSON usage for non-streaming responses.

- [ ] Add failing tests proving the request summary detects model/field/cache-control differences without including text values.
- [ ] Run `cargo test --all-features compare::tests` and confirm the assertions fail before the helper exists.
- [ ] Implement the structural summary and token-weighted cache percentage; represent missing usage as `None`.
- [ ] Add a local HTTP mock test for response usage parsing and direct/proxy arm selection.
- [ ] Run targeted comparer tests and verify no comparison rows are written through stats capture.
- [ ] Remove upstream debug logging of full JSON request bodies; retain safe provider/model/body-size metadata only.

### Task 3: Hidden generic CLI command

**Files:**
- Modify: `src/main.rs`
- Modify: `src/cli.rs`
- Modify: `src/compare.rs`
- Test: CLI tests in `src/main.rs` or `src/compare.rs`

**Interfaces:**
- Add hidden `compare` subcommand options: `--model <provider/model>`, `--format <anthropic|openai|responses>`, `--request <path>`, optional `--account`, `--effort`, `--runs` (default 1), and mandatory `--confirm-live`.
- Resolve the model through the configured router and reject a format mismatch before network requests.
- Require and replace `{{compare_tag}}` in string values with distinct direct/proxy tags that remain stable within each arm.
- Use the same selected account and provider client for both variants. The direct arm skips proxy body preparation; the proxy arm uses Task 1's shared function.
- Print the planned call count before making requests and output only redacted JSON summaries after confirmation.

- [ ] Add tests for hidden clap visibility, missing confirmation, invalid fixture, bad model/format, and repeat-without-placeholder; assert each rejects before the runner is called.
- [ ] Run targeted CLI tests and confirm they fail for missing command/validation.
- [ ] Implement the CLI wiring and input/route validation.
- [ ] Run targeted tests and verify successful validation produces a generic report shape independent of model name.

### Task 4: Verification and usage documentation

**Files:**
- Modify: `docs/superpowers/specs/2026-10-09-hidden-compare-command-design.md`
- Modify: `docs/superpowers/plans/2026-10-09-hidden-request-compare.md`

- [ ] Document a minimal fixture format and sample hidden command invocation without embedding credentials or real prompts.
- [ ] Run all required Rust and e2e checks from `AGENTS.md`.
- [ ] Review `git diff --check`, `git status --short`, and the full diff; preserve unrelated `.claude/` work.
