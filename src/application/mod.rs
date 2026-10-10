//! Application services: the use cases, written against [`crate::ports`] and
//! the [`crate::domain`] model only.

pub mod account_usage;
pub mod commands;
pub mod compare;
pub mod proxy;
pub mod runtime;
pub mod settings;
pub mod stats_report;
pub mod streams;
pub mod usage;

/// Fakes and fixtures shared by the application tests.
#[cfg(test)]
pub(crate) mod testing {
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::Arc;

    use futures_util::future::BoxFuture;

    use crate::application::runtime::RuntimeState;
    use crate::domain::config::{Config, Provider, ProviderFormat};
    use crate::domain::router::Router;
    use crate::domain::stats::AccountUsage;
    use crate::ports::{
        Account, AccountMap, UpstreamAccount, UpstreamError, UpstreamRequest, UpstreamResponse,
    };

    /// An upstream account that never reaches the network.
    #[derive(Debug)]
    pub struct FakeAccount {
        provider: String,
        alias: String,
        credentials: bool,
    }

    impl UpstreamAccount for FakeAccount {
        fn provider(&self) -> &str {
            &self.provider
        }

        fn alias(&self) -> &str {
            &self.alias
        }

        fn has_credentials(&self) -> bool {
            self.credentials
        }

        fn has_usage_endpoint(&self) -> bool {
            false
        }

        fn send(
            &self,
            _request: UpstreamRequest,
        ) -> BoxFuture<'_, Result<UpstreamResponse, UpstreamError>> {
            Box::pin(async {
                Err(UpstreamError::MissingApiKey {
                    provider: self.provider.clone(),
                })
            })
        }

        fn fetch_usage(&self) -> BoxFuture<'_, Result<Option<AccountUsage>, UpstreamError>> {
            Box::pin(async { Ok(None) })
        }
    }

    /// A fake account of `provider` named `alias`.
    pub fn account(provider: &Provider, alias: &str, credentials: bool) -> Account {
        Arc::new(FakeAccount {
            provider: provider.name.clone(),
            alias: alias.to_string(),
            credentials,
        })
    }

    /// An `openai`-format provider named `openai` serving `models`.
    pub fn openai_provider(models: &[&str]) -> Provider {
        Provider {
            name: "openai".to_string(),
            base_url: "https://api.openai.com/v1".to_string(),
            format: ProviderFormat::Openai,
            models: models.iter().map(ToString::to_string).collect(),
            ..Provider::default()
        }
    }

    /// Runtime state over `config` with no upstream accounts.
    pub fn empty_state(config: Config) -> RuntimeState {
        state_with(config, HashMap::new())
    }

    /// Runtime state over `config` and `accounts`, routed by `config` itself.
    pub fn state_with(config: Config, accounts: AccountMap) -> RuntimeState {
        let config = Arc::new(config);
        RuntimeState {
            router: Arc::new(Router::new(config.clone()).unwrap()),
            config,
            accounts: Arc::new(accounts),
            enforce_active_model: false,
            model_override: None,
            config_path: PathBuf::new(),
        }
    }
}
