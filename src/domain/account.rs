//! Provider accounts: the credentials stored per provider and alias.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// An API key or OAuth token bundle for one account.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum AuthEntry {
    /// API-key entry (`{"type":"api","key":"..."}`).
    Api {
        /// The provider's API key.
        key: String,
    },
    /// OAuth token bundle.
    OAuth(OAuthTokens),
}

/// OAuth tokens for a single account.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct OAuthTokens {
    /// Current bearer token.
    pub access: String,
    /// Token used to refresh access.
    pub refresh: String,
    /// Access token expiry as Unix milliseconds.
    pub expires: i64,
    /// Account identifier needed by some providers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
}

impl AuthEntry {
    /// The key for an API-key entry.
    #[must_use]
    pub fn api_key(&self) -> Option<&str> {
        match self {
            Self::Api { key } => Some(key),
            Self::OAuth(_) => None,
        }
    }

    /// The OAuth bundle for an OAuth entry.
    #[must_use]
    pub const fn oauth(&self) -> Option<&OAuthTokens> {
        match self {
            Self::Api { .. } => None,
            Self::OAuth(tokens) => Some(tokens),
        }
    }

    /// Whether this entry contains a usable credential.
    #[must_use]
    pub const fn usable(&self) -> bool {
        match self {
            Self::Api { key } => !key.is_empty(),
            Self::OAuth(tokens) => !tokens.access.is_empty() || !tokens.refresh.is_empty(),
        }
    }

    /// The credential kind shown to users: `api` or `oauth`.
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Api { .. } => "api",
            Self::OAuth(_) => "oauth",
        }
    }
}

/// Provider names mapped to account aliases and credentials.
pub type AuthMap = HashMap<String, HashMap<String, AuthEntry>>;

/// Whether `alias` is a safe account alias: 1..=64 ASCII letters, digits,
/// `.`, `_` or `-` (it travels in headers and file names).
#[must_use]
pub fn is_valid_alias(alias: &str) -> bool {
    !alias.is_empty()
        && alias.len() <= 64
        && alias
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// The alias a provider without a selection gets: the legacy config choice if
/// it is still stored, then the only account, then `default`, then the first
/// by name.
#[must_use]
pub fn pick_selection(aliases: &[String], legacy: Option<&str>) -> Option<String> {
    if let Some(legacy) = legacy.filter(|l| aliases.iter().any(|a| a == l)) {
        return Some(legacy.to_string());
    }
    if aliases.len() > 1 && aliases.iter().any(|a| a == "default") {
        return Some("default".to_string());
    }
    aliases.iter().min().cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aliases_reject_header_unsafe_values() {
        for name in ["", "a b", "a/b", "é", "\na", "a:b"] {
            assert!(!is_valid_alias(name), "{name:?}");
        }
        for name in ["default", "work-2", "a.b_c"] {
            assert!(is_valid_alias(name), "{name:?}");
        }
    }

    #[test]
    fn selection_seed_prefers_legacy_then_only_then_default_then_name() {
        let v = |names: &[&str]| names.iter().map(ToString::to_string).collect::<Vec<_>>();
        let pick = |names: &[&str], legacy| pick_selection(&v(names), legacy);
        assert_eq!(pick(&["b", "a"], Some("b")).as_deref(), Some("b"));
        assert_eq!(pick(&["b", "a"], Some("gone")).as_deref(), Some("a"));
        assert_eq!(pick(&["work"], None).as_deref(), Some("work"));
        assert_eq!(
            pick(&["z", "default", "a"], None).as_deref(),
            Some("default")
        );
        assert_eq!(pick(&["z", "a"], None).as_deref(), Some("a"));
        assert_eq!(pick(&[], None), None);
    }

    #[test]
    fn legacy_api_entry_parses() {
        let entry: AuthEntry = serde_json::from_str(r#"{"type":"api","key":"sk-legacy"}"#).unwrap();
        assert_eq!(entry.api_key(), Some("sk-legacy"));
        assert_eq!(entry.kind(), "api");
    }
}
