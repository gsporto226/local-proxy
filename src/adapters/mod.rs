//! Adapters: the edges of the hexagon.
//!
//! [`inbound`] adapters drive the application (HTTP API, CLI, file watcher);
//! [`outbound`] adapters are driven by it through the [`crate::ports`] traits.

pub mod inbound;
pub mod outbound;
