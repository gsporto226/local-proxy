//! Local multi-provider translation proxy (`OpenAI` <-> `Anthropic`) in Rust.
//!
//! Provides the configuration model ([`config`]), request router ([`router`]),
//! and upstream HTTP client ([`upstream`]) that back the command-line
//! front-end ([`cli`]).

/// Command-line interface implementation.
pub mod cli;

/// Configuration types and loading.
pub mod config;

/// Shared error types.
pub mod error;

/// Axum HTTP request handlers.
pub mod handlers;

/// Model-to-provider routing logic.
pub mod router;

/// Server-Sent Events helpers.
pub mod sse;

/// Response streaming helpers.
pub mod streams;

/// Request translation between provider formats.
pub mod translate;

/// Upstream HTTP clients and request helpers.
pub mod upstream;
