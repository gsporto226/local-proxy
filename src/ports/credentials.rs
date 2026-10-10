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

    /// Replace an existing account's OAuth tokens after a refresh.
    ///
    /// # Errors
    ///
    /// Returns an error if the account is missing or storage is unavailable.
    fn update_oauth(
        &self,
        provider: &str,
        alias: &str,
        tokens: &OAuthTokens,
    ) -> Result<(), CredentialError>;

    /// Remove one account, returning whether it existed.
    ///
    /// # Errors
    ///
    /// Returns [`CredentialError::Backend`] if the store is unavailable.
    fn remove(&self, provider: &str, alias: &str) -> Result<bool, CredentialError>;
}
