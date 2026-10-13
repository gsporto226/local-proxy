//! Encrypted account credential store, migrated from the legacy `auth.json`.
//!
//! Each credential is identified by provider and account alias. The database
//! key is held by the platform credential vault, never by the config file.
//! After a verified migration the legacy file is renamed to
//! `auth.json.migrated`, never deleted.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::io;
use std::path::{Path, PathBuf};

use miette::Diagnostic;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

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
}

/// Provider names mapped to account aliases and credentials.
pub type AuthMap = HashMap<String, HashMap<String, AuthEntry>>;

/// Credential storage and vault errors.
#[derive(Debug, Error, Diagnostic)]
pub enum AuthError {
    /// Legacy JSON cannot be read.
    #[error("failed to read legacy auth file {path}: {source}")]
    Read {
        /// File path.
        path: String,
        /// I/O cause.
        #[source]
        source: io::Error,
    },
    /// Legacy JSON is invalid.
    #[error("failed to parse legacy auth file {path}: {source}")]
    Parse {
        /// File path.
        path: String,
        /// JSON cause.
        #[source]
        source: serde_json::Error,
    },
    /// An auth file operation failed.
    #[error("failed to write auth file {path}: {source}")]
    Write {
        /// File path.
        path: String,
        /// I/O cause.
        #[source]
        source: io::Error,
    },
    /// OS credential store cannot be accessed.
    #[error("OS credential vault unavailable: {0}")]
    Vault(#[source] keyring::Error),
    /// Database access failed, including an incorrect encryption key.
    #[error("encrypted account database error: {0}")]
    Database(#[source] rusqlite::Error),
    /// Credential could not be serialized.
    #[error("account credential format error: {0}")]
    Format(#[source] serde_json::Error),
    /// A database exists without its vault key.
    #[error("encrypted account database exists but its OS vault key is missing; restore the vault key before retrying")]
    MissingKey,
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
    /// The platform has no supported OS vault integration.
    #[error("an OS credential vault is required on this platform")]
    UnsupportedPlatform,
}

/// Legacy plaintext credential file, only used during migration.
#[must_use]
pub fn auth_file() -> PathBuf {
    crate::config::global_config_dir().join("auth.json")
}

/// Encrypted account database location.
#[must_use]
pub fn auth_db() -> PathBuf {
    crate::config::global_config_dir().join("accounts.db")
}

/// Validate a user-supplied account alias.
///
/// # Errors
/// Returns [`AuthError::InvalidAlias`] for unsafe or empty aliases.
pub fn validate_alias(alias: &str) -> Result<(), AuthError> {
    if alias.is_empty()
        || alias.len() > 64
        || !alias
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    {
        return Err(AuthError::InvalidAlias);
    }
    Ok(())
}

const VAULT_SERVICE: &str = "local-proxy/accounts-db";

fn absolute(path: &Path) -> Result<PathBuf, AuthError> {
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    std::env::current_dir()
        .map(|cwd| cwd.join(path))
        .map_err(|source| AuthError::Read {
            path: path.display().to_string(),
            source,
        })
}

fn path_hash(path: &Path) -> String {
    format!("{:x}", Sha256::digest(path.to_string_lossy().as_bytes()))
}

/// Vault user name for the database at `path`. The real store has a fixed
/// name, so moving the config dir keeps its key; an overridden dir
/// (`LOCAL_PROXY_CONFIG_DIR`, used by every test) is keyed by its
/// canonical path so isolated stores never share a key with the real one.
fn vault_identity(path: &Path, overridden: bool) -> Result<String, AuthError> {
    if !overridden {
        return Ok("default".to_string());
    }
    let path = absolute(path)?;
    // The parent exists (open_db creates it); the file itself may not yet.
    let canonical = match (path.parent().map(std::fs::canonicalize), path.file_name()) {
        (Some(Ok(dir)), Some(name)) => dir.join(name),
        _ => path,
    };
    Ok(path_hash(&canonical))
}

fn db_key(path: &Path) -> Result<String, AuthError> {
    let overridden = std::env::var_os("LOCAL_PROXY_CONFIG_DIR").is_some_and(|v| !v.is_empty());
    let entry = keyring::Entry::new(VAULT_SERVICE, &vault_identity(path, overridden)?)
        .map_err(AuthError::Vault)?;
    match entry.get_password() {
        Ok(key) => return Ok(key),
        Err(keyring::Error::NoEntry) => {}
        Err(source) => return Err(AuthError::Vault(source)),
    }
    // Keys created before the fixed name were stored under the hash of the
    // plain absolute path. Copy, never move: losing the key loses the store.
    let legacy = keyring::Entry::new(VAULT_SERVICE, &path_hash(&absolute(path)?))
        .map_err(AuthError::Vault)?;
    match legacy.get_password() {
        Ok(key) => {
            entry.set_password(&key).map_err(AuthError::Vault)?;
            return Ok(key);
        }
        Err(keyring::Error::NoEntry) => {}
        Err(source) => return Err(AuthError::Vault(source)),
    }
    if path.exists() {
        return Err(AuthError::MissingKey);
    }
    let mut bytes = [0_u8; 32];
    getrandom::getrandom(&mut bytes).map_err(|source| AuthError::Read {
        path: path.display().to_string(),
        source: io::Error::other(source.to_string()),
    })?;
    let key = bytes
        .iter()
        .fold(String::with_capacity(64), |mut key, byte| {
            write!(key, "{byte:02x}").expect("writing to String cannot fail");
            key
        });
    entry.set_password(&key).map_err(AuthError::Vault)?;
    Ok(key)
}

fn open_db(path: &Path) -> Result<Connection, AuthError> {
    #[cfg(not(any(target_os = "windows", target_os = "linux")))]
    return Err(AuthError::UnsupportedPlatform);

    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir).map_err(|source| AuthError::Write {
        path: dir.display().to_string(),
        source,
    })?;
    let key = db_key(path)?;
    let conn = Connection::open(path).map_err(AuthError::Database)?;
    // The CLI and every running proxy share the file; wait out their writes.
    conn.busy_timeout(std::time::Duration::from_secs(5))
        .map_err(AuthError::Database)?;
    // Encryption is the security boundary; owner-only permissions are defense
    // in depth where the OS supports them.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    // `key` is generated as 64 ASCII hex digits, never user-provided SQL.
    conn.execute_batch(&format!("PRAGMA key = \"x'{key}'\";"))
        .map_err(AuthError::Database)?;
    let cipher: String = conn
        .query_row("PRAGMA cipher_version", [], |row| row.get(0))
        .map_err(AuthError::Database)?;
    if cipher.is_empty() {
        return Err(AuthError::Database(rusqlite::Error::InvalidQuery));
    }
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         CREATE TABLE IF NOT EXISTS accounts (
           provider TEXT NOT NULL,
           alias TEXT NOT NULL,
           credential TEXT NOT NULL,
           PRIMARY KEY(provider, alias)
         );",
    )
    .map_err(AuthError::Database)?;
    Ok(conn)
}

fn migrate(conn: &mut Connection, path: &Path) -> Result<(), AuthError> {
    let legacy = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(source) => {
            return Err(AuthError::Read {
                path: path.display().to_string(),
                source,
            })
        }
    };
    let previous: HashMap<String, AuthEntry> =
        serde_json::from_str(&legacy).map_err(|source| AuthError::Parse {
            path: path.display().to_string(),
            source,
        })?;
    let transaction = conn.transaction().map_err(AuthError::Database)?;
    for (provider, entry) in &previous {
        let value = serde_json::to_string(entry).map_err(AuthError::Format)?;
        transaction
            .execute(
                "INSERT OR IGNORE INTO accounts(provider, alias, credential) VALUES(?1, 'default', ?2)",
                params![provider, value],
            )
            .map_err(AuthError::Database)?;
    }
    transaction.commit().map_err(AuthError::Database)?;
    for provider in previous.keys() {
        let serialized: String = conn
            .query_row(
                "SELECT credential FROM accounts WHERE provider=?1 AND alias='default'",
                [provider],
                |row| row.get(0),
            )
            .map_err(AuthError::Database)?;
        // Existing `default` credentials are not replaced by a stale migration.
        let _: AuthEntry = serde_json::from_str(&serialized).map_err(AuthError::Format)?;
    }
    // Keep the plaintext file as `auth.json.migrated`: losing the vault key or
    // the database must never mean losing the credentials themselves.
    let mut backup = path.as_os_str().to_os_string();
    backup.push(".migrated");
    let backup = PathBuf::from(backup);
    // `rename` refuses to replace on Windows; any previous backup is obsolete.
    let _ = std::fs::remove_file(&backup);
    std::fs::rename(path, &backup).map_err(|source| AuthError::Write {
        path: backup.display().to_string(),
        source,
    })?;
    Ok(())
}

fn with_db<T>(f: impl FnOnce(&mut Connection) -> Result<T, AuthError>) -> Result<T, AuthError> {
    // A unit test that forgot to point `LOCAL_PROXY_CONFIG_DIR` at an isolated
    // dir must fail loudly instead of migrating and touching the real store.
    #[cfg(test)]
    assert!(
        std::env::var_os("LOCAL_PROXY_CONFIG_DIR").is_some_and(|v| !v.is_empty()),
        "unit tests must set LOCAL_PROXY_CONFIG_DIR to an isolated directory; \
         refusing to touch the real credential store"
    );
    let path = auth_db();
    // A poisoned lock only means a panic mid-call; SQLite rolled back.
    let mut shared = DB.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    // Tests repoint `LOCAL_PROXY_CONFIG_DIR`, so the connection follows the path.
    if shared.as_ref().is_none_or(|(open, _)| *open != path) {
        *shared = None;
        *shared = Some((path.clone(), open_db(&path)?));
    }
    let (_, conn) = shared.as_mut().expect("connection opened above");
    migrate(conn, &auth_file())?;
    let result = f(conn);
    drop(shared);
    result
}

/// The process-wide connection, opened once (the key derivation is costly)
/// and keyed by its path.
// ponytail: one global lock, fine for a handful of rare store calls.
static DB: std::sync::Mutex<Option<(PathBuf, Connection)>> = std::sync::Mutex::new(None);

/// SQLite's `data_version`: changes only when another connection (another
/// process) commits, never for this process's own writes.
///
/// # Errors
/// Returns an error if the vault or database is unavailable.
pub fn data_version() -> Result<i64, AuthError> {
    with_db(|conn| {
        conn.query_row("PRAGMA data_version", [], |row| row.get(0))
            .map_err(AuthError::Database)
    })
}

/// Read every saved account and its credential.
///
/// # Errors
/// Returns an error if the vault, database, or migration is unavailable.
pub fn read_auth() -> Result<AuthMap, AuthError> {
    with_db(|conn| {
        let mut stmt = conn
            .prepare("SELECT provider, alias, credential FROM accounts")
            .map_err(AuthError::Database)?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .map_err(AuthError::Database)?;
        let mut accounts: AuthMap = HashMap::new();
        for row in rows {
            let (provider, alias, value) = row.map_err(AuthError::Database)?;
            let entry = serde_json::from_str(&value).map_err(AuthError::Format)?;
            accounts.entry(provider).or_default().insert(alias, entry);
        }
        Ok(accounts)
    })
}

/// Insert a named API-key account. Existing accounts are not overwritten.
///
/// # Errors
/// Returns an error for invalid aliases, duplicates, or storage failures.
pub fn set_key_for(provider: &str, alias: &str, key: &str) -> Result<(), AuthError> {
    insert_account(
        provider,
        alias,
        &AuthEntry::Api {
            key: key.to_string(),
        },
    )
}

/// Insert an OAuth account. Existing accounts are not overwritten.
///
/// # Errors
/// Returns an error for invalid aliases, duplicates, or storage failures.
pub fn set_oauth_for(provider: &str, alias: &str, tokens: &OAuthTokens) -> Result<(), AuthError> {
    insert_account(provider, alias, &AuthEntry::OAuth(tokens.clone()))
}

fn insert_account(provider: &str, alias: &str, entry: &AuthEntry) -> Result<(), AuthError> {
    validate_alias(alias)?;
    let value = serde_json::to_string(entry).map_err(AuthError::Format)?;
    with_db(|conn| {
        let inserted = conn
            .execute(
                "INSERT OR IGNORE INTO accounts(provider, alias, credential) VALUES(?1, ?2, ?3)",
                params![provider, alias, value],
            )
            .map_err(AuthError::Database)?;
        if inserted == 0 {
            return Err(AuthError::Exists {
                provider: provider.to_string(),
                alias: alias.to_string(),
            });
        }
        Ok(())
    })
}

/// Store refreshed OAuth tokens unless another process refreshed first.
///
/// A compare-and-swap under `BEGIN IMMEDIATE` on the refresh token `fresh`
/// was obtained with. Returns `None` when `fresh` was written, or the stored tokens when another
/// process refreshed first: with rotating refresh tokens there is one chain,
/// and the stored one wins.
///
/// # Errors
/// Returns an error if the account is missing or storage is unavailable.
pub fn swap_oauth_for(
    provider: &str,
    alias: &str,
    used_refresh: &str,
    fresh: &OAuthTokens,
) -> Result<Option<OAuthTokens>, AuthError> {
    let value =
        serde_json::to_string(&AuthEntry::OAuth(fresh.clone())).map_err(AuthError::Format)?;
    with_db(|conn| {
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(AuthError::Database)?;
        let stored: String = tx
            .query_row(
                "SELECT credential FROM accounts WHERE provider=?1 AND alias=?2",
                params![provider, alias],
                |row| row.get(0),
            )
            .map_err(AuthError::Database)?;
        if let AuthEntry::OAuth(current) =
            serde_json::from_str(&stored).map_err(AuthError::Format)?
        {
            if current.refresh != used_refresh {
                return Ok(Some(current));
            }
        }
        tx.execute(
            "UPDATE accounts SET credential=?3 WHERE provider=?1 AND alias=?2",
            params![provider, alias, value],
        )
        .map_err(AuthError::Database)?;
        tx.commit().map_err(AuthError::Database)?;
        Ok(None)
    })
}

/// Remove one provider/account pair, returning whether it existed.
///
/// # Errors
/// Returns an error if the vault or database is unavailable.
pub fn remove_account(provider: &str, alias: &str) -> Result<bool, AuthError> {
    with_db(|conn| {
        conn.execute(
            "DELETE FROM accounts WHERE provider=?1 AND alias=?2",
            params![provider, alias],
        )
        .map(|count| count != 0)
        .map_err(AuthError::Database)
    })
}

/// Retrieve one named account.
///
/// # Errors
/// Returns an error if the vault or database is unavailable.
pub fn account_for(provider: &str, alias: &str) -> Result<Option<AuthEntry>, AuthError> {
    with_db(|conn| {
        let value: Option<String> = conn
            .query_row(
                "SELECT credential FROM accounts WHERE provider=?1 AND alias=?2",
                params![provider, alias],
                |row| row.get(0),
            )
            .optional()
            .map_err(AuthError::Database)?;
        value
            .map(|v| serde_json::from_str(&v).map_err(AuthError::Format))
            .transpose()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aliases_reject_header_unsafe_values() {
        for name in ["", "a b", "a/b", "é", "\na", "a:b"] {
            assert!(validate_alias(name).is_err(), "{name:?}");
        }
        for name in ["default", "work-2", "a.b_c"] {
            assert!(validate_alias(name).is_ok(), "{name:?}");
        }
    }

    #[test]
    fn vault_identity_is_fixed_for_the_real_store_only() {
        let dir = std::env::temp_dir();
        let db = dir.join("accounts.db");
        assert_eq!(vault_identity(&db, false).unwrap(), "default");
        let isolated = vault_identity(&db, true).unwrap();
        assert_eq!(isolated.len(), 64, "overrides are keyed by path hash");
        // Equivalent spellings of the same dir share one key.
        let dotted = dir.join(".").join("accounts.db");
        assert_eq!(vault_identity(&dotted, true).unwrap(), isolated);
    }

    #[test]
    fn data_version_moves_only_for_other_connections() {
        let _guard = crate::TEST_STATE_LOCK.lock().unwrap();
        let dir = std::env::temp_dir().join(format!(
            "local-proxy-auth-version-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("LOCAL_PROXY_CONFIG_DIR", &dir);

        let start = data_version().unwrap();
        set_key_for("p", "own", "k").unwrap();
        assert_eq!(data_version().unwrap(), start, "own writes are not changes");

        // A second connection stands in for the CLI process.
        let other = open_db(&auth_db()).unwrap();
        other
            .execute(
                "INSERT INTO accounts(provider, alias, credential) VALUES('p', 'cli', ?1)",
                [r#"{"type":"api","key":"k2"}"#],
            )
            .unwrap();
        drop(other);
        assert_ne!(data_version().unwrap(), start);
        assert!(read_auth().unwrap()["p"].contains_key("cli"));

        std::env::remove_var("LOCAL_PROXY_CONFIG_DIR");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn oauth_swap_keeps_the_first_refresh() {
        let _guard = crate::TEST_STATE_LOCK.lock().unwrap();
        let dir = std::env::temp_dir().join(format!(
            "local-proxy-auth-swap-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("LOCAL_PROXY_CONFIG_DIR", &dir);
        let tokens = |refresh: &str| OAuthTokens {
            access: format!("a-{refresh}"),
            refresh: refresh.to_string(),
            expires: 1,
            account_id: None,
        };
        set_oauth_for("p", "a", &tokens("r0")).unwrap();

        // First refresher (holding r0) wins and writes r1.
        assert_eq!(swap_oauth_for("p", "a", "r0", &tokens("r1")).unwrap(), None);
        // A second refresher that also started from r0 must adopt r1.
        let adopted = swap_oauth_for("p", "a", "r0", &tokens("r2")).unwrap();
        assert_eq!(adopted, Some(tokens("r1")));
        let stored = account_for("p", "a").unwrap().unwrap();
        assert_eq!(stored.oauth().unwrap().refresh, "r1");

        std::env::remove_var("LOCAL_PROXY_CONFIG_DIR");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn legacy_api_entry_parses() {
        let entry: AuthEntry = serde_json::from_str(r#"{"type":"api","key":"sk-legacy"}"#).unwrap();
        assert_eq!(entry.api_key(), Some("sk-legacy"));
    }

    #[test]
    fn legacy_file_migrates_once_and_is_kept_as_backup() {
        let _guard = crate::TEST_STATE_LOCK.lock().unwrap();
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "local-proxy-auth-migrate-{}-{stamp}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("LOCAL_PROXY_CONFIG_DIR", &dir);
        std::fs::write(
            dir.join("auth.json"),
            r#"{"p-api":{"type":"api","key":"k1"},
                "p-oauth":{"type":"oauth","access":"a","refresh":"r","expires":1}}"#,
        )
        .unwrap();

        let auth = read_auth().unwrap();
        assert_eq!(auth["p-api"]["default"].api_key(), Some("k1"));
        assert_eq!(auth["p-oauth"]["default"].oauth().unwrap().refresh, "r");
        assert!(!dir.join("auth.json").exists(), "legacy file consumed");
        assert!(
            dir.join("auth.json.migrated").exists(),
            "legacy file kept as a backup"
        );

        // A restart reads the same entries and leaves the backup alone.
        let again = read_auth().unwrap();
        assert_eq!(again.len(), 2);
        assert!(dir.join("auth.json.migrated").exists());

        std::env::remove_var("LOCAL_PROXY_CONFIG_DIR");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
