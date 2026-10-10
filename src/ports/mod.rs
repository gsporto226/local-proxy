//! Ports: the interfaces the application core needs from the outside world.
//!
//! The application layer only talks to these traits; the adapters in
//! [`crate::adapters::outbound`] implement them and [`crate::bootstrap`] wires
//! one implementation of each into a [`Ports`] bundle.

use std::sync::Arc;

/// Reading and writing the config overlay file.
pub mod config;

/// The encrypted per-provider account store.
pub mod credentials;

/// Recording and querying usage statistics.
pub mod usage;

/// The live event stream (`/admin/events`).
pub mod events;

/// Talking to upstream LLM providers.
pub mod upstream;

/// Running local `$proxy` commands.
pub mod commands;

/// Reading the proxy's own log.
pub mod logs;

/// OAuth token exchange and refresh.
pub mod oauth;

pub use self::commands::CommandRunner;
pub use self::config::ConfigStore;
pub use self::credentials::{CredentialError, CredentialStore};
pub use self::events::EventBus;
pub use self::logs::LogSource;
pub use self::oauth::OAuthClient;
pub use self::upstream::{
    Account, AccountMap, ClientHints, UpstreamAccount, UpstreamConnector, UpstreamError,
    UpstreamRequest, UpstreamResponse,
};
pub use self::usage::{StatsError, UsageStore};

/// One implementation of every port, shared by the application services.
#[derive(Clone)]
pub struct Ports {
    /// Config overlay persistence.
    pub config: Arc<dyn ConfigStore>,
    /// Account credential storage.
    pub credentials: Arc<dyn CredentialStore>,
    /// Usage statistics storage.
    pub usage: Arc<dyn UsageStore>,
    /// Live event publishing and subscription.
    pub events: Arc<dyn EventBus>,
    /// Upstream provider connections.
    pub upstream: Arc<dyn UpstreamConnector>,
    /// Local command execution.
    pub commands: Arc<dyn CommandRunner>,
    /// The proxy's log file.
    pub logs: Arc<dyn LogSource>,
    /// OAuth token endpoints.
    pub oauth: Arc<dyn OAuthClient>,
}
