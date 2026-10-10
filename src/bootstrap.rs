//! Composition root: the one place that picks an adapter for every port.

use std::sync::Arc;

use crate::adapters::outbound::config_file::FileConfigStore;
use crate::adapters::outbound::credential_store::SqlCipherCredentialStore;
use crate::adapters::outbound::events::BroadcastEventBus;
use crate::adapters::outbound::log_file::FileLogSource;
use crate::adapters::outbound::oauth::HttpOAuthClient;
use crate::adapters::outbound::process::ProcessCommandRunner;
use crate::adapters::outbound::upstream::HttpUpstreamConnector;
use crate::adapters::outbound::usage_store::SqliteUsageStore;
use crate::ports::Ports;

/// The production adapters: files and databases under the config dir, HTTP
/// upstreams, child processes, and an in-process event bus.
#[must_use]
pub fn ports() -> Ports {
    let credentials = Arc::new(SqlCipherCredentialStore);
    let oauth = Arc::new(HttpOAuthClient::default());
    Ports {
        config: Arc::new(FileConfigStore),
        upstream: Arc::new(HttpUpstreamConnector::new(
            credentials.clone(),
            oauth.clone(),
        )),
        credentials,
        usage: Arc::new(SqliteUsageStore),
        events: Arc::new(BroadcastEventBus::default()),
        commands: Arc::new(ProcessCommandRunner),
        logs: Arc::new(FileLogSource),
        oauth,
    }
}
