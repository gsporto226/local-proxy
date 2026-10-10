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

use rusqlite::{params, Connection, OptionalExtension};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::adapters::outbound::paths;
use crate::domain::account::{is_valid_alias, AuthEntry, AuthMap, OAuthTokens};
use crate::ports::{CredentialError, CredentialStore};

/// [`CredentialStore`] over a `SQLCipher` database whose key lives in the OS
/// credential vault. Paths are resolved per call from [`paths::config_dir`].
#[derive(Debug, Clone, Copy, Default)]
pub struct SqlCipherCredentialStore;

/// Credential storage and vault errors.
#[derive(Debug, Error)]
enum AuthError {
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
    /// The account to update does not exist.
    #[error("account '{alias}' not found for provider '{provider}'")]
    NotFound {
        /// Provider name.
        provider: String,
        /// Account alias.
        alias: String,
    },
    /// The platform has no supported OS vault integration.
    #[cfg_attr(any(target_os = "windows", target_os = "linux"), allow(dead_code))]
    #[error("an OS credential vault is required on this platform")]
    UnsupportedPlatform,
}

fn vault_entry(path: &Path) -> Result<keyring::Entry, AuthError> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|source| AuthError::Read {
                path: path.display().to_string(),
                source,
            })?
            .join(path)
    };
    let hash = Sha256::digest(absolute.to_string_lossy().as_bytes());
    let identity = format!("{hash:x}");
    keyring::Entry::new("local-proxy/accounts-db", &identity).map_err(AuthError::Vault)
}

fn db_key(path: &Path) -> Result<String, AuthError> {
    let entry = vault_entry(path)?;
    match entry.get_password() {
        Ok(key) => Ok(key),
        Err(keyring::Error::NoEntry) if path.exists() => Err(AuthError::MissingKey),
        Err(keyring::Error::NoEntry) => {
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
        Err(source) => Err(AuthError::Vault(source)),
    }
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
        "PRAGMA journal_mode = DELETE;
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

impl From<AuthError> for CredentialError {
    fn from(e: AuthError) -> Self {
        match e {
            AuthError::InvalidAlias => Self::InvalidAlias,
            AuthError::Exists { provider, alias } => Self::Exists { provider, alias },
            AuthError::NotFound { provider, alias } => Self::NotFound { provider, alias },
            other => Self::Backend(other.to_string()),
        }
    }
}

fn validate_alias(alias: &str) -> Result<(), AuthError> {
    if is_valid_alias(alias) {
        Ok(())
    } else {
        Err(AuthError::InvalidAlias)
    }
}

fn with_db<T>(f: impl FnOnce(&Connection) -> Result<T, AuthError>) -> Result<T, AuthError> {
    // A unit test that forgot to point `LOCAL_PROXY_CONFIG_DIR` at an isolated
    // dir must fail loudly instead of migrating and touching the real store.
    #[cfg(test)]
    assert!(
        std::env::var_os("LOCAL_PROXY_CONFIG_DIR").is_some_and(|v| !v.is_empty()),
        "unit tests must set LOCAL_PROXY_CONFIG_DIR to an isolated directory; \
         refusing to touch the real credential store"
    );
    let mut conn = open_db(&paths::accounts_db())?;
    migrate(&mut conn, &paths::legacy_auth_file())?;
    f(&conn)
}

/// Read every saved account and its credential.
///
/// # Errors
/// Returns an error if the vault, database, or migration is unavailable.
fn read_auth() -> Result<AuthMap, AuthError> {
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

/// Replace an existing account's OAuth tokens after a refresh.
///
/// # Errors
/// Returns an error if the account is missing or storage is unavailable.
fn update_oauth_for(provider: &str, alias: &str, tokens: &OAuthTokens) -> Result<(), AuthError> {
    let value =
        serde_json::to_string(&AuthEntry::OAuth(tokens.clone())).map_err(AuthError::Format)?;
    with_db(|conn| {
        let updated = conn
            .execute(
                "UPDATE accounts SET credential=?3 WHERE provider=?1 AND alias=?2",
                params![provider, alias, value],
            )
            .map_err(AuthError::Database)?;
        if updated == 0 {
            return Err(AuthError::NotFound {
                provider: provider.to_string(),
                alias: alias.to_string(),
            });
        }
        Ok(())
    })
}

/// Remove one provider/account pair, returning whether it existed.
///
/// # Errors
/// Returns an error if the vault or database is unavailable.
fn remove_account(provider: &str, alias: &str) -> Result<bool, AuthError> {
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
fn account_for(provider: &str, alias: &str) -> Result<Option<AuthEntry>, AuthError> {
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

impl CredentialStore for SqlCipherCredentialStore {
    fn read_all(&self) -> Result<AuthMap, CredentialError> {
        Ok(read_auth()?)
    }

    fn get(&self, provider: &str, alias: &str) -> Result<Option<AuthEntry>, CredentialError> {
        Ok(account_for(provider, alias)?)
    }

    fn insert(
        &self,
        provider: &str,
        alias: &str,
        entry: &AuthEntry,
    ) -> Result<(), CredentialError> {
        Ok(insert_account(provider, alias, entry)?)
    }

    fn update_oauth(
        &self,
        provider: &str,
        alias: &str,
        tokens: &OAuthTokens,
    ) -> Result<(), CredentialError> {
        Ok(update_oauth_for(provider, alias, tokens)?)
    }

    fn remove(&self, provider: &str, alias: &str) -> Result<bool, CredentialError> {
        Ok(remove_account(provider, alias)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

        let auth = SqlCipherCredentialStore.read_all().unwrap();
        assert_eq!(auth["p-api"]["default"].api_key(), Some("k1"));
        assert_eq!(auth["p-oauth"]["default"].oauth().unwrap().refresh, "r");
        assert!(!dir.join("auth.json").exists(), "legacy file consumed");
        assert!(
            dir.join("auth.json.migrated").exists(),
            "legacy file kept as a backup"
        );

        // A restart reads the same entries and leaves the backup alone.
        let again = SqlCipherCredentialStore.read_all().unwrap();
        assert_eq!(again.len(), 2);
        assert!(dir.join("auth.json.migrated").exists());

        std::env::remove_var("LOCAL_PROXY_CONFIG_DIR");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
