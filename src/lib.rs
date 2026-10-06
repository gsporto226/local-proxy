//! Local multi-provider translation proxy (`OpenAI` <-> `Anthropic`) in Rust.
//!
//! Provides the configuration model ([`config`]), request router ([`router`]),
//! and upstream HTTP client ([`upstream`]) that back the command-line
//! front-end ([`cli`]).

/// The tracing target used by every log line the proxy emits. Kept on a single
/// target so operators can filter the whole application with one
/// `RUST_LOG=local_proxy=...` entry.
pub const LOG_TARGET: &str = "local_proxy";

/// Command-line interface implementation.
pub mod cli;

/// Global API-key store.
pub mod auth;

/// Embedded provider catalog and config-overlay merging.
pub mod catalog;

/// Format-neutral IR and per-format codecs.
pub mod ir;

/// Configuration types and loading.
pub mod config;

/// Shared error types.
pub mod error;

/// `$proxy` local-command execution helpers.
pub mod exec;

/// Axum HTTP request handlers.
pub mod handlers;

/// Model-to-provider routing logic.
pub mod router;

/// Generic OAuth 2.0 + PKCE engine for subscription providers.
pub mod oauth;

/// Server-Sent Events helpers.
pub mod sse;

/// Response streaming helpers.
pub mod streams;

/// Request translation between provider formats.
pub mod translate;

/// Upstream HTTP clients and request helpers.
pub mod upstream;

/// Local usage statistics collected from upstream requests (SQLite store).
pub mod stats;

/// Sandboxed Rhai template rendering for the Claude Code status line.
pub mod statusline;

/// Global lock that serializes unit tests mutating process-global state (the
/// current working directory and environment variables), preventing races
/// between parallel test threads.
#[cfg(test)]
pub(crate) static TEST_STATE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
