use std::collections::HashMap;

use crate::domain::account::{AuthEntry, AuthMap, OAuthTokens};

/// Errors from the credential store.
#[derive(Debug, thiserror::Error, miette::Diagnostic)]
pub enum CredentialError {
    /// The chosen alias is invalid.
    #[error("account alias must contain only letters, digits, '.', '_', or '-' and be at most 64 characters")]
    InvalidAlias,
    /// An account already exists.
    #[error("account '{alias}' already exists for provider '{provider}'")]
    Exists {
        /// Provider name.
        provider: String,
        /// Account alias.
        alias: String,
    },
    /// The account to update does not exist.
    #[error("account '{alias}' not found for provider '{provider}'")]
    NotFound {
        /// Provider name.
        provider: String,
        /// Account alias.
        alias: String,
    },
    /// The selected account cannot be removed while the provider has others.
    #[error(
        "conta '{alias}' e a selecionada do provider '{provider}'; selecione outra com \
         `local-proxy account {provider}/<alias>` antes de remove-la"
    )]
    SelectedAccount {
        /// Provider name.
        provider: String,
        /// Account alias.
        alias: String,
    },
    /// The backing store (vault, database, legacy file) failed.
    #[error("{0}")]
    Backend(String),
}

/// Storage of provider account credentials, keyed by provider and alias.
pub trait CredentialStore: Send + Sync {
    /// Every saved account and its credential.
    ///
    /// # Errors
    ///
    /// Returns [`CredentialError::Backend`] if the store is unavailable.
    fn read_all(&self) -> Result<AuthMap, CredentialError>;

    /// One named account, if stored.
    ///
    /// # Errors
    ///
    /// Returns [`CredentialError::Backend`] if the store is unavailable.
    fn get(&self, provider: &str, alias: &str) -> Result<Option<AuthEntry>, CredentialError>;

    /// Insert a new account; an existing one is never overwritten.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid aliases, duplicates, or storage failures.
    fn insert(&self, provider: &str, alias: &str, entry: &AuthEntry)
        -> Result<(), CredentialError>;

    /// Store refreshed OAuth tokens unless another process refreshed first: a
    /// compare-and-swap on `used_refresh`, the refresh token `fresh` was
    /// obtained with. Returns `None` when `fresh` was written, or the stored
    /// tokens when another process won (rotating refresh tokens form one
    /// chain, and the stored one is it).
    ///
    /// # Errors
    ///
    /// Returns an error if the account is missing or storage is unavailable.
    fn swap_oauth(
        &self,
        provider: &str,
        alias: &str,
        used_refresh: &str,
        fresh: &OAuthTokens,
    ) -> Result<Option<OAuthTokens>, CredentialError>;

    /// Remove one account, returning whether it existed.
    ///
    /// # Errors
    ///
    /// Returns [`CredentialError::Backend`] if the store is unavailable.
    fn remove(&self, provider: &str, alias: &str) -> Result<bool, CredentialError>;

    /// `(data_version, account revision)`: the first moves whenever another
    /// process commits (cheap to poll), the second only when an account was
    /// added or removed.
    ///
    /// # Errors
    ///
    /// Returns [`CredentialError::Backend`] if the store is unavailable.
    fn revision(&self) -> Result<(i64, i64), CredentialError>;

    /// Give every provider with stored accounts but no selection one, seeded
    /// from the legacy config selections (`provider -> alias`); see
    /// [`crate::domain::account::pick_selection`]. Existing selections stay.
    ///
    /// # Errors
    ///
    /// Returns [`CredentialError::Backend`] if the store is unavailable.
    fn seed_selections(&self, legacy: &HashMap<String, String>) -> Result<(), CredentialError>;

    /// The selected alias of every provider that has stored accounts.
    ///
    /// # Errors
    ///
    /// Returns [`CredentialError::Backend`] if the store is unavailable.
    fn selections(&self) -> Result<HashMap<String, String>, CredentialError>;

    /// Make `alias` the selected account of `provider`.
    ///
    /// # Errors
    ///
    /// Returns [`CredentialError::NotFound`] if the account is not stored.
    fn select(&self, provider: &str, alias: &str) -> Result<(), CredentialError>;

    /// The account `session_id` uses for `provider`: snapshotted from the
    /// provider's selection on the session's first request and kept when the
    /// selection later changes. A snapshot whose account was disconnected is
    /// retaken from the current selection. `None` without stored accounts.
    ///
    /// # Errors
    ///
    /// Returns [`CredentialError::Backend`] if the store is unavailable.
    fn session_account(
        &self,
        session_id: &str,
        provider: &str,
    ) -> Result<Option<String>, CredentialError>;

    /// Set the snapshot of `session_id` for `provider` to `alias`, and with
    /// `select` also make it the provider's selection, in one transaction.
    ///
    /// # Errors
    ///
    /// Returns [`CredentialError::NotFound`] if the account is not stored.
    fn pin_session(
        &self,
        session_id: &str,
        provider: &str,
        alias: &str,
        select: bool,
    ) -> Result<(), CredentialError>;

    /// Drop the snapshot of `session_id` for `provider`; its next request
    /// snapshots the current selection again.
    ///
    /// # Errors
    ///
    /// Returns [`CredentialError::Backend`] if the store is unavailable.
    fn unpin_session(&self, session_id: &str, provider: &str) -> Result<(), CredentialError>;

    /// Every live snapshot of `session_id` (`provider -> alias`).
    ///
    /// # Errors
    ///
    /// Returns [`CredentialError::Backend`] if the store is unavailable.
    fn session_accounts(
        &self,
        session_id: &str,
    ) -> Result<HashMap<String, String>, CredentialError>;

    /// Delete snapshots unused since `cutoff_ms`, returning how many.
    ///
    /// # Errors
    ///
    /// Returns [`CredentialError::Backend`] if the store is unavailable.
    fn prune_sessions(&self, cutoff_ms: i64) -> Result<usize, CredentialError>;
}
