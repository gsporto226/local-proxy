//! Global API-key store (`auth.json`), mirroring opencode's `/connect` auth.
//!
//! Keys live in a separate file from the config so credentials are never
//! committed with user configuration. The file lives in the global per-user
//! config dir: `%APPDATA%\local-proxy\auth.json` on Windows, or
//! `~/.config/local-proxy/auth.json` on Unix.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};

use miette::Diagnostic;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// A single auth entry: the API key for a provider, or an OAuth token set.
///
/// For an API-key entry only `kind` (`api`) and `key` are set. For an OAuth
/// entry (`kind` = `oauth`) `key` holds the current access token and the
/// remaining fields carry what a refresh needs.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct AuthEntry {
    /// Auth kind: `api` (API key) or `oauth` (bearer tokens).
    #[serde(rename = "type")]
    pub kind: String,
    /// The provider's API key, or the OAuth access token.
    #[serde(default)]
    pub key: String,
    /// OAuth refresh token, when the entry is an OAuth one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    /// `ChatGPT` account id (`chatgpt_account_id` claim), for backends that
    /// require it as a request header.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    /// Unix timestamp (seconds) at which [`AuthEntry::key`] expires.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
}

impl AuthEntry {
    /// Build a plain API-key entry.
    #[must_use]
    pub fn api(key: impl Into<String>) -> Self {
        Self {
            kind: "api".to_string(),
            key: key.into(),
            ..Self::default()
        }
    }

    /// Whether this entry holds OAuth tokens rather than a static API key.
    #[must_use]
    pub fn is_oauth(&self) -> bool {
        self.kind == "oauth"
    }
}

/// Map of provider name to its stored auth entry.
pub type AuthMap = HashMap<String, AuthEntry>;

/// Errors while reading or writing the auth store.
#[derive(Debug, Error, Diagnostic)]
pub enum AuthError {
    /// The auth file could not be read.
    #[error("failed to read auth file {path}: {source}")]
    #[diagnostic(code(auth::read))]
    Read {
        /// Path that could not be read.
        path: String,
        /// Underlying I/O error.
        #[source]
        source: io::Error,
    },
    /// The auth file could not be parsed.
    #[error("failed to parse auth file {path}: {source}")]
    #[diagnostic(code(auth::parse), help("delete or fix the file and try again"))]
    Parse {
        /// Path that could not be parsed.
        path: String,
        /// Underlying parse error.
        #[source]
        source: serde_json::Error,
    },
    /// The auth file could not be written.
    #[error("failed to write auth file {path}: {source}")]
    #[diagnostic(code(auth::write))]
    Write {
        /// Path that could not be written.
        path: String,
        /// Underlying I/O error.
        #[source]
        source: io::Error,
    },
}

/// Path to the auth store (`<global config dir>/auth.json`).
#[must_use]
pub fn auth_file() -> PathBuf {
    crate::config::global_config_dir().join("auth.json")
}

/// Load the auth store, treating a missing file as an empty map.
fn load(path: &Path) -> Result<AuthMap, AuthError> {
    match std::fs::read_to_string(path) {
        Ok(content) => serde_json::from_str(&content).map_err(|source| AuthError::Parse {
            path: path.display().to_string(),
            source,
        }),
        Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(AuthMap::new()),
        Err(source) => Err(AuthError::Read {
            path: path.display().to_string(),
            source,
        }),
    }
}

/// Persist the auth store atomically (temp file + rename).
fn save(path: &Path, auth: &AuthMap) -> Result<(), AuthError> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir).map_err(|source| AuthError::Write {
        path: path.display().to_string(),
        source,
    })?;
    let tmp = dir.join(".auth.json.tmp");
    let content = serde_json::to_string_pretty(auth).expect("AuthMap serializes to JSON");
    std::fs::write(&tmp, content).map_err(|source| AuthError::Write {
        path: tmp.display().to_string(),
        source,
    })?;
    std::fs::rename(&tmp, path).map_err(|source| AuthError::Write {
        path: path.display().to_string(),
        source,
    })
}

/// Read the current auth store.
///
/// # Errors
///
/// Returns [`AuthError`] if the file cannot be read or parsed.
pub fn read_auth() -> Result<AuthMap, AuthError> {
    load(&auth_file())
}

/// Return the stored entry for `provider`, if any.
#[must_use]
pub fn entry_for(provider: &str) -> Option<AuthEntry> {
    read_auth()
        .ok()
        .and_then(|auth| auth.get(provider).cloned())
}

/// Return the stored API key for `provider`, if any.
#[must_use]
pub fn key_for(provider: &str) -> Option<String> {
    read_auth()
        .ok()
        .and_then(|auth| auth.get(provider).map(|e| e.key.clone()))
}

/// Store the API key for `provider`, creating/updating `auth.json`.
///
/// # Errors
///
/// Returns [`AuthError`] if the file cannot be written.
pub fn set_key(provider: &str, key: &str) -> Result<(), AuthError> {
    let path = auth_file();
    let mut auth = load(&path)?;
    auth.insert(provider.to_string(), AuthEntry::api(key));
    save(&path, &auth)
}

/// Store an OAuth token set for `provider`, creating/updating `auth.json`.
///
/// `expires_at` is a Unix timestamp in seconds; `account_id` is optional and
/// only needed by backends that require a per-account header.
///
/// # Errors
///
/// Returns [`AuthError`] if the file cannot be written.
pub fn set_oauth(
    provider: &str,
    access_token: &str,
    refresh_token: Option<String>,
    account_id: Option<String>,
    expires_at: Option<i64>,
) -> Result<(), AuthError> {
    let path = auth_file();
    let mut auth = load(&path)?;
    let previous = auth.get(provider);
    auth.insert(
        provider.to_string(),
        AuthEntry {
            kind: "oauth".to_string(),
            key: access_token.to_string(),
            // A refresh response may omit the refresh token; keep the old one.
            refresh_token: refresh_token.or_else(|| previous.and_then(|e| e.refresh_token.clone())),
            account_id: account_id.or_else(|| previous.and_then(|e| e.account_id.clone())),
            expires_at,
        },
    );
    save(&path, &auth)
}

/// Remove the stored API key for `provider`. Returns whether a key existed.
///
/// # Errors
///
/// Returns [`AuthError`] if the file cannot be written.
pub fn remove_key(provider: &str) -> Result<bool, AuthError> {
    let path = auth_file();
    let mut auth = load(&path)?;
    let removed = auth.remove(provider).is_some();
    if removed {
        save(&path, &auth)?;
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_auth_file(test: &str) -> PathBuf {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock is set")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "local-proxy-auth-{test}-{}-{stamp}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("create test dir");
        dir.join("auth.json")
    }

    #[test]
    fn missing_file_reads_empty() {
        let p = Path::new("C:\\nonexistent\\opencode\\local-proxy\\auth.json");
        let auth = load(p).expect("missing file is empty");
        assert!(auth.is_empty());
    }

    #[test]
    fn set_and_read_key_roundtrip() {
        let path = temp_auth_file("roundtrip");
        std::fs::remove_file(&path).ok();
        let mut auth = AuthMap::new();
        auth.insert("opencode-go".to_string(), AuthEntry::api("sk-test"));
        save(&path, &auth).expect("save");
        let loaded = load(&path).expect("load");
        assert_eq!(loaded["opencode-go"].key, "sk-test");
        assert_eq!(loaded["opencode-go"].kind, "api");
        assert!(!loaded["opencode-go"].is_oauth());
    }

    #[test]
    fn legacy_entry_without_new_fields_still_parses() {
        // An auth.json written before OAuth support: only `type` and `key`.
        // It must keep working, with the new fields defaulting to none.
        let auth: AuthMap =
            serde_json::from_str(r#"{"opencode-go": {"type": "api", "key": "sk-legacy"}}"#)
                .expect("legacy auth parses");
        let entry = &auth["opencode-go"];
        assert_eq!(entry.kind, "api");
        assert_eq!(entry.key, "sk-legacy");
        assert!(!entry.is_oauth());
        assert_eq!(entry.refresh_token, None);
        assert_eq!(entry.account_id, None);
        assert_eq!(entry.expires_at, None);

        // and it round-trips without inventing the optional fields
        let json = serde_json::to_string(entry).expect("serializes");
        assert!(!json.contains("refresh_token"), "{json}");
        assert!(!json.contains("account_id"), "{json}");
        assert!(!json.contains("expires_at"), "{json}");
    }

    #[test]
    fn oauth_entry_roundtrips() {
        let path = temp_auth_file("oauth");
        std::fs::remove_file(&path).ok();
        let mut auth = AuthMap::new();
        auth.insert(
            "chatgpt".to_string(),
            AuthEntry {
                kind: "oauth".to_string(),
                key: "access".to_string(),
                refresh_token: Some("refresh".to_string()),
                account_id: Some("acct-1".to_string()),
                expires_at: Some(1_700_000_000),
            },
        );
        save(&path, &auth).expect("save");
        let loaded = load(&path).expect("load");
        let entry = &loaded["chatgpt"];
        assert!(entry.is_oauth());
        assert_eq!(entry.key, "access");
        assert_eq!(entry.refresh_token.as_deref(), Some("refresh"));
        assert_eq!(entry.account_id.as_deref(), Some("acct-1"));
        assert_eq!(entry.expires_at, Some(1_700_000_000));
    }

    #[test]
    fn save_then_load_and_mutate() {
        let path = temp_auth_file("mutate");
        std::fs::remove_file(&path).ok();
        let mut auth = AuthMap::new();
        auth.insert("zen".to_string(), AuthEntry::api("k"));
        save(&path, &auth).expect("save");
        let mut loaded = load(&path).expect("load");
        loaded.remove("zen");
        save(&path, &loaded).expect("save again");
        let final_auth = load(&path).expect("load final");
        assert!(final_auth.is_empty());
    }

    #[test]
    fn auth_file_lives_in_global_dir() {
        assert!(auth_file().to_string_lossy().contains("local-proxy"));
    }
}
