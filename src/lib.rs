//! Local multi-provider translation proxy (`OpenAI` <-> `Anthropic`) in Rust.
//!
//! Laid out as a hexagon:
//!
//! - [`domain`]: the model and rules (config, routing, format translation),
//!   free of I/O;
//! - [`ports`]: the traits the application needs from the outside world;
//! - [`application`]: the use cases, written against ports and domain only;
//! - [`adapters`]: HTTP/CLI/watcher on the driving side, storage, upstream
//!   HTTP, processes and events on the driven side;
//! - [`bootstrap`]: the composition root wiring adapters into ports.

/// The tracing target used by every log line the proxy emits. Kept on a single
/// target so operators can filter the whole application with one
/// `RUST_LOG=local_proxy=...` entry.
pub const LOG_TARGET: &str = "local_proxy";

pub mod adapters;
pub mod application;
pub mod bootstrap;
pub mod domain;
pub mod ports;

/// Global lock that serializes unit tests mutating process-global state (the
/// current working directory and environment variables), preventing races
/// between parallel test threads.
#[cfg(test)]
pub(crate) static TEST_STATE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
