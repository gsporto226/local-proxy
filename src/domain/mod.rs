//! The domain core: the proxy's model and rules, free of I/O.
//!
//! Nothing here touches the network, the file system, or process state; those
//! sit behind the [`crate::ports`] traits.

pub mod account;
pub mod catalog;
pub mod config;
pub mod error;
pub mod exec;
pub mod ir;
pub mod oauth;
pub mod request;

/// Model-to-provider routing logic.
pub mod router;
pub mod sse;
pub mod stats;

/// Request translation between provider formats.
pub mod translate;
