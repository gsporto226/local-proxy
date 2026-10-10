//! Subscription quota per account (`GET /admin/account-usage`): cached
//! snapshots refreshed from each provider's usage endpoint at most once a
//! minute.

use std::collections::HashMap;

use serde::Serialize;

use crate::application::runtime::AppState;
use crate::domain::stats::AccountUsage;
use crate::ports::{Account, Ports};

const ACCOUNT_USAGE_CACHE_SECS: i64 = 60;

/// One account's usage, or why it is unavailable.
#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AccountUsageResult {
    /// A snapshot, possibly stale when the latest refresh failed.
    Available {
        /// The snapshot.
        usage: AccountUsage,
        /// Whether the snapshot is older than the latest failed refresh.
        stale: bool,
        /// The refresh error, when stale.
        error: Option<String>,
    },
    /// No snapshot could be produced.
    Unavailable {
        /// Provider name.
        provider: String,
        /// Account alias.
        alias: String,
        /// Why.
        error: String,
    },
}

/// The usage of every account whose provider has a usage endpoint, sorted by
/// provider and alias. Refreshes are serialized across callers.
pub async fn load(app: &AppState) -> Vec<AccountUsageResult> {
    // The panel polls this route; serialize refreshes and reuse snapshots for a minute.
    let _refresh = app.usage_refresh_lock().await;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs().cast_signed());
    let ports = app.ports().clone();
    let cached = match tokio::task::spawn_blocking(move || ports.usage.account_usage_cache()).await
    {
        Ok(Ok(cached)) => cached,
        Ok(Err(error)) => {
            tracing::warn!(target: crate::LOG_TARGET, error = %error, "failed to read account usage cache");
            Vec::new()
        }
        Err(error) => {
            tracing::warn!(target: crate::LOG_TARGET, error = %error, "account usage cache task failed");
            Vec::new()
        }
    };
    let mut cached: HashMap<_, _> = cached
        .into_iter()
        .map(|usage| ((usage.provider.clone(), usage.alias.clone()), usage))
        .collect();
    let state = app.snapshot().await;
    let mut accounts: Vec<_> = state
        .accounts
        .iter()
        .flat_map(|(provider, aliases)| {
            aliases
                .iter()
                .filter(|(_, account)| account.has_usage_endpoint())
                .map(|(alias, account)| (provider.clone(), alias.clone(), account.clone()))
                .collect::<Vec<_>>()
        })
        .collect();
    accounts.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));

    let refreshes = accounts.into_iter().map(|(provider, alias, account)| {
        let previous = cached.remove(&(provider.clone(), alias.clone()));
        refresh(app.ports(), provider, alias, account, previous, now)
    });
    futures_util::future::join_all(refreshes).await
}

async fn refresh(
    ports: &Ports,
    provider: String,
    alias: String,
    account: Account,
    previous: Option<AccountUsage>,
    now: i64,
) -> AccountUsageResult {
    if let Some(usage) = previous
        .as_ref()
        .filter(|usage| now.saturating_sub(usage.fetched_at) < ACCOUNT_USAGE_CACHE_SECS)
    {
        return AccountUsageResult::Available {
            usage: usage.clone(),
            stale: false,
            error: None,
        };
    }
    match account.fetch_usage().await {
        Ok(Some(usage)) => {
            persist(ports, usage.clone()).await;
            AccountUsageResult::Available {
                usage,
                stale: false,
                error: None,
            }
        }
        Ok(None) => AccountUsageResult::Unavailable {
            provider,
            alias,
            error: "no upstream usage endpoint is configured".to_string(),
        },
        Err(fetch_error) => {
            let message = fetch_error.to_string();
            if let Some(usage) = previous {
                AccountUsageResult::Available {
                    usage,
                    stale: true,
                    error: Some(message),
                }
            } else {
                AccountUsageResult::Unavailable {
                    provider,
                    alias,
                    error: message,
                }
            }
        }
    }
}

async fn persist(ports: &Ports, usage: AccountUsage) {
    let ports = ports.clone();
    match tokio::task::spawn_blocking(move || ports.usage.save_account_usage(&usage)).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => tracing::warn!(
            target: crate::LOG_TARGET,
            error = %error,
            "failed to save account usage"
        ),
        Err(error) => tracing::warn!(
            target: crate::LOG_TARGET,
            error = %error,
            "account usage save task failed"
        ),
    }
}
