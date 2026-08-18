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

/// A single auth entry: the API key for a provider.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AuthEntry {
    /// Auth kind — always `api` for now (future: oauth).
    #[serde(rename = "type")]
    pub kind: String,
    /// The provider's API key.
    pub key: String,
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
    auth.insert(
        provider.to_string(),
        AuthEntry {
            kind: "api".to_string(),
            key: key.to_string(),
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
        auth.insert(
            "opencode-go".to_string(),
            AuthEntry {
                kind: "api".to_string(),
                key: "sk-test".to_string(),
            },
        );
        save(&path, &auth).expect("save");
        let loaded = load(&path).expect("load");
        assert_eq!(loaded["opencode-go"].key, "sk-test");
        assert_eq!(loaded["opencode-go"].kind, "api");
    }

    #[test]
    fn save_then_load_and_mutate() {
        let path = temp_auth_file("mutate");
        std::fs::remove_file(&path).ok();
        let mut auth = AuthMap::new();
        auth.insert(
            "zen".to_string(),
            AuthEntry {
                kind: "api".to_string(),
                key: "k".to_string(),
            },
        );
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
