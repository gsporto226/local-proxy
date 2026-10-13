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

use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::adapters::outbound::paths;
use crate::domain::account::{is_valid_alias, pick_selection, AuthEntry, AuthMap, OAuthTokens};
use crate::domain::oauth::now_ms;
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
    /// The selected account cannot be removed while others remain.
    #[error("selected account")]
    SelectedAccount {
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
/// (`LOCAL_PROXY_CONFIG_DIR`, used by every test) is keyed by its canonical
/// path so isolated stores never share a key with the real one.
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
         );
         CREATE TABLE IF NOT EXISTS selected (
           provider TEXT PRIMARY KEY,
           alias TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS session_accounts (
           session_id TEXT NOT NULL,
           provider TEXT NOT NULL,
           alias TEXT NOT NULL,
           last_used INTEGER NOT NULL,
           PRIMARY KEY(session_id, provider)
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
    bump_revision(&transaction)?;
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
            AuthError::SelectedAccount { provider, alias } => {
                Self::SelectedAccount { provider, alias }
            }
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

/// The process-wide connection, opened once (the key derivation is costly)
/// and keyed by its path.
// ponytail: one global lock around one connection, fine for a local proxy; a
// connection pool if contention ever shows up.
static DB: std::sync::Mutex<Option<(PathBuf, Connection)>> = std::sync::Mutex::new(None);

fn with_db<T>(f: impl FnOnce(&mut Connection) -> Result<T, AuthError>) -> Result<T, AuthError> {
    // A unit test that forgot to point `LOCAL_PROXY_CONFIG_DIR` at an isolated
    // dir must fail loudly instead of migrating and touching the real store.
    #[cfg(test)]
    assert!(
        std::env::var_os("LOCAL_PROXY_CONFIG_DIR").is_some_and(|v| !v.is_empty()),
        "unit tests must set LOCAL_PROXY_CONFIG_DIR to an isolated directory; \
         refusing to touch the real credential store"
    );
    let path = paths::accounts_db();
    // A poisoned lock only means a panic mid-call; SQLite rolled back.
    let mut shared = DB.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    // Tests repoint `LOCAL_PROXY_CONFIG_DIR`, so the connection follows the path.
    if shared.as_ref().is_none_or(|(open, _)| *open != path) {
        *shared = None;
        *shared = Some((path.clone(), open_db(&path)?));
    }
    let (_, conn) = shared.as_mut().expect("connection opened above");
    migrate(conn, &paths::legacy_auth_file())?;
    let result = f(conn);
    drop(shared);
    result
}

/// Count a change to the set of stored accounts in `user_version`. Running
/// proxies rebuild their accounts only when it moves: `data_version` also
/// moves for token refreshes, selections and session snapshots, which need no
/// rebuild.
fn bump_revision(conn: &Connection) -> Result<(), AuthError> {
    let revision: i64 = conn
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .map_err(AuthError::Database)?;
    // An integer read back from SQLite, never user input.
    conn.execute_batch(&format!(
        "PRAGMA user_version = {}",
        revision.wrapping_add(1)
    ))
    .map_err(AuthError::Database)
}

fn revision() -> Result<(i64, i64), AuthError> {
    with_db(|conn| {
        let version = conn
            .query_row("PRAGMA data_version", [], |row| row.get(0))
            .map_err(AuthError::Database)?;
        let revision = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .map_err(AuthError::Database)?;
        Ok((version, revision))
    })
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
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(AuthError::Database)?;
        let inserted = tx
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
        // A provider's first account becomes its selection.
        tx.execute(
            "INSERT OR IGNORE INTO selected(provider, alias) VALUES(?1, ?2)",
            params![provider, alias],
        )
        .map_err(AuthError::Database)?;
        bump_revision(&tx)?;
        tx.commit().map_err(AuthError::Database)
    })
}

/// Compare-and-swap refreshed OAuth tokens under `BEGIN IMMEDIATE`; see
/// [`CredentialStore::swap_oauth`].
fn swap_oauth_for(
    provider: &str,
    alias: &str,
    used_refresh: &str,
    fresh: &OAuthTokens,
) -> Result<Option<OAuthTokens>, AuthError> {
    let value =
        serde_json::to_string(&AuthEntry::OAuth(fresh.clone())).map_err(AuthError::Format)?;
    with_db(|conn| {
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(AuthError::Database)?;
        let stored: Option<String> = tx
            .query_row(
                "SELECT credential FROM accounts WHERE provider=?1 AND alias=?2",
                params![provider, alias],
                |row| row.get(0),
            )
            .optional()
            .map_err(AuthError::Database)?;
        let Some(stored) = stored else {
            return Err(AuthError::NotFound {
                provider: provider.to_string(),
                alias: alias.to_string(),
            });
        };
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

/// Remove one provider/account pair, returning whether it existed. The
/// selected account can only go when it is the provider's last one, so a
/// provider with accounts always has a selection.
fn remove_account(provider: &str, alias: &str) -> Result<bool, AuthError> {
    with_db(|conn| {
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(AuthError::Database)?;
        let siblings: i64 = tx
            .query_row(
                "SELECT COUNT(*) FROM accounts WHERE provider=?1 AND alias<>?2",
                params![provider, alias],
                |row| row.get(0),
            )
            .map_err(AuthError::Database)?;
        if siblings > 0 && selected_in(&tx, provider)?.as_deref() == Some(alias) {
            return Err(AuthError::SelectedAccount {
                provider: provider.to_string(),
                alias: alias.to_string(),
            });
        }
        let removed = tx
            .execute(
                "DELETE FROM accounts WHERE provider=?1 AND alias=?2",
                params![provider, alias],
            )
            .map_err(AuthError::Database)?;
        if removed == 0 {
            return Ok(false);
        }
        if siblings == 0 {
            tx.execute("DELETE FROM selected WHERE provider=?1", [provider])
                .map_err(AuthError::Database)?;
        }
        bump_revision(&tx)?;
        tx.commit().map_err(AuthError::Database)?;
        Ok(true)
    })
}

fn selected_in(conn: &Connection, provider: &str) -> Result<Option<String>, AuthError> {
    conn.query_row(
        "SELECT alias FROM selected WHERE provider=?1",
        [provider],
        |row| row.get(0),
    )
    .optional()
    .map_err(AuthError::Database)
}

/// Make `alias` (which must be stored) the selected account of `provider`.
fn select_in(conn: &Connection, provider: &str, alias: &str) -> Result<(), AuthError> {
    if !account_exists(conn, provider, alias)? {
        return Err(AuthError::NotFound {
            provider: provider.to_string(),
            alias: alias.to_string(),
        });
    }
    conn.execute(
        "INSERT INTO selected(provider, alias) VALUES(?1, ?2)
         ON CONFLICT(provider) DO UPDATE SET alias=excluded.alias",
        params![provider, alias],
    )
    .map_err(AuthError::Database)?;
    Ok(())
}

/// Give every provider with stored accounts but no selection one, seeded from
/// the legacy `defaults.active_accounts` (`provider -> alias`) where possible.
fn seed_selections(legacy: &HashMap<String, String>) -> Result<(), AuthError> {
    with_db(|conn| {
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(AuthError::Database)?;
        let mut unselected: HashMap<String, Vec<String>> = HashMap::new();
        {
            let mut stmt = tx
                .prepare(
                    "SELECT provider, alias FROM accounts
                     WHERE provider NOT IN (SELECT provider FROM selected)",
                )
                .map_err(AuthError::Database)?;
            let rows = stmt
                .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get(1)?)))
                .map_err(AuthError::Database)?;
            for row in rows {
                let (provider, alias) = row.map_err(AuthError::Database)?;
                unselected.entry(provider).or_default().push(alias);
            }
        }
        for (provider, aliases) in &unselected {
            let Some(alias) = pick_selection(aliases, legacy.get(provider).map(String::as_str))
            else {
                continue;
            };
            select_in(&tx, provider, &alias)?;
            tracing::info!(target: crate::LOG_TARGET, %provider, %alias, "selected account");
        }
        tx.commit().map_err(AuthError::Database)
    })
}

fn selections() -> Result<HashMap<String, String>, AuthError> {
    with_db(|conn| {
        let mut stmt = conn
            .prepare("SELECT provider, alias FROM selected")
            .map_err(AuthError::Database)?;
        let rows = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .map_err(AuthError::Database)?;
        rows.collect::<Result<_, _>>().map_err(AuthError::Database)
    })
}

fn select_account(provider: &str, alias: &str) -> Result<(), AuthError> {
    with_db(|conn| {
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(AuthError::Database)?;
        select_in(&tx, provider, alias)?;
        tx.commit().map_err(AuthError::Database)
    })
}

fn snapshot_in(
    conn: &Connection,
    session_id: &str,
    provider: &str,
    alias: &str,
) -> Result<(), AuthError> {
    conn.execute(
        "INSERT INTO session_accounts(session_id, provider, alias, last_used)
         VALUES(?1, ?2, ?3, ?4)
         ON CONFLICT(session_id, provider)
         DO UPDATE SET alias=excluded.alias, last_used=excluded.last_used",
        params![session_id, provider, alias, now_ms()],
    )
    .map_err(AuthError::Database)?;
    Ok(())
}

/// The account `session_id` uses for `provider`: snapshotted from the
/// selection on the session's first request and kept when the selection
/// later changes. A snapshot whose account was disconnected is retaken from
/// the current selection.
fn session_account(session_id: &str, provider: &str) -> Result<Option<String>, AuthError> {
    with_db(|conn| {
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(AuthError::Database)?;
        // The join drops snapshots of disconnected accounts.
        let snapshot: Option<(String, i64)> = tx
            .query_row(
                "SELECT s.alias, s.last_used FROM session_accounts s
                 JOIN accounts a ON a.provider = s.provider AND a.alias = s.alias
                 WHERE s.session_id = ?1 AND s.provider = ?2",
                params![session_id, provider],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(AuthError::Database)?;
        let alias = if let Some((alias, last_used)) = snapshot {
            // Only touched to age out idle sessions; a minute is plenty.
            if now_ms() - last_used > 60_000 {
                snapshot_in(&tx, session_id, provider, &alias)?;
            }
            Some(alias)
        } else {
            let selected = selected_in(&tx, provider)?;
            if let Some(alias) = &selected {
                snapshot_in(&tx, session_id, provider, alias)?;
            }
            selected
        };
        tx.commit().map_err(AuthError::Database)?;
        Ok(alias)
    })
}

fn pin_session(
    session_id: &str,
    provider: &str,
    alias: &str,
    select: bool,
) -> Result<(), AuthError> {
    with_db(|conn| {
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(AuthError::Database)?;
        if select {
            select_in(&tx, provider, alias)?;
        } else if !account_exists(&tx, provider, alias)? {
            return Err(AuthError::NotFound {
                provider: provider.to_string(),
                alias: alias.to_string(),
            });
        }
        snapshot_in(&tx, session_id, provider, alias)?;
        tx.commit().map_err(AuthError::Database)
    })
}

fn account_exists(conn: &Connection, provider: &str, alias: &str) -> Result<bool, AuthError> {
    conn.query_row(
        "SELECT 1 FROM accounts WHERE provider=?1 AND alias=?2",
        params![provider, alias],
        |_| Ok(()),
    )
    .optional()
    .map(|found| found.is_some())
    .map_err(AuthError::Database)
}

fn unpin_session(session_id: &str, provider: &str) -> Result<(), AuthError> {
    with_db(|conn| {
        conn.execute(
            "DELETE FROM session_accounts WHERE session_id=?1 AND provider=?2",
            params![session_id, provider],
        )
        .map(|_| ())
        .map_err(AuthError::Database)
    })
}

fn session_accounts(session_id: &str) -> Result<HashMap<String, String>, AuthError> {
    with_db(|conn| {
        let mut stmt = conn
            .prepare(
                "SELECT s.provider, s.alias FROM session_accounts s
                 JOIN accounts a ON a.provider = s.provider AND a.alias = s.alias
                 WHERE s.session_id = ?1",
            )
            .map_err(AuthError::Database)?;
        let rows = stmt
            .query_map([session_id], |row| Ok((row.get(0)?, row.get(1)?)))
            .map_err(AuthError::Database)?;
        rows.collect::<Result<_, _>>().map_err(AuthError::Database)
    })
}

fn prune_sessions(cutoff_ms: i64) -> Result<usize, AuthError> {
    with_db(|conn| {
        conn.execute(
            "DELETE FROM session_accounts WHERE last_used < ?1",
            [cutoff_ms],
        )
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

    fn swap_oauth(
        &self,
        provider: &str,
        alias: &str,
        used_refresh: &str,
        fresh: &OAuthTokens,
    ) -> Result<Option<OAuthTokens>, CredentialError> {
        Ok(swap_oauth_for(provider, alias, used_refresh, fresh)?)
    }

    fn remove(&self, provider: &str, alias: &str) -> Result<bool, CredentialError> {
        Ok(remove_account(provider, alias)?)
    }

    fn revision(&self) -> Result<(i64, i64), CredentialError> {
        Ok(revision()?)
    }

    fn seed_selections(&self, legacy: &HashMap<String, String>) -> Result<(), CredentialError> {
        Ok(seed_selections(legacy)?)
    }

    fn selections(&self) -> Result<HashMap<String, String>, CredentialError> {
        Ok(selections()?)
    }

    fn select(&self, provider: &str, alias: &str) -> Result<(), CredentialError> {
        Ok(select_account(provider, alias)?)
    }

    fn session_account(
        &self,
        session_id: &str,
        provider: &str,
    ) -> Result<Option<String>, CredentialError> {
        Ok(session_account(session_id, provider)?)
    }

    fn pin_session(
        &self,
        session_id: &str,
        provider: &str,
        alias: &str,
        select: bool,
    ) -> Result<(), CredentialError> {
        Ok(pin_session(session_id, provider, alias, select)?)
    }

    fn unpin_session(&self, session_id: &str, provider: &str) -> Result<(), CredentialError> {
        Ok(unpin_session(session_id, provider)?)
    }

    fn session_accounts(
        &self,
        session_id: &str,
    ) -> Result<HashMap<String, String>, CredentialError> {
        Ok(session_accounts(session_id)?)
    }

    fn prune_sessions(&self, cutoff_ms: i64) -> Result<usize, CredentialError> {
        Ok(prune_sessions(cutoff_ms)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn session_snapshot_survives_selection_changes_but_not_disconnects() {
        let _guard = crate::TEST_STATE_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("LOCAL_PROXY_CONFIG_DIR", dir.path());
        let store = SqlCipherCredentialStore;
        let api = |key: &str| AuthEntry::Api {
            key: key.to_string(),
        };
        store.insert("p", "work", &api("k1")).unwrap();
        store.insert("p", "personal", &api("k2")).unwrap();
        let session = |id: &str| store.session_account(id, "p").unwrap();

        assert_eq!(session("a").as_deref(), Some("work"));
        store.select("p", "personal").unwrap();
        assert_eq!(session("a").as_deref(), Some("work"), "snapshot kept");
        assert_eq!(session("b").as_deref(), Some("personal"));

        // Disconnecting the snapshot's account retakes it from the selection.
        store.remove("p", "work").unwrap();
        assert_eq!(session("a").as_deref(), Some("personal"));

        // Idle snapshots are pruned.
        assert_eq!(store.prune_sessions(now_ms() + 1).unwrap(), 2);
        assert!(store.session_accounts("a").unwrap().is_empty());

        std::env::remove_var("LOCAL_PROXY_CONFIG_DIR");
    }

    #[test]
    fn oauth_swap_keeps_the_first_refresh() {
        let _guard = crate::TEST_STATE_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("LOCAL_PROXY_CONFIG_DIR", dir.path());
        let store = SqlCipherCredentialStore;
        let tokens = |refresh: &str| OAuthTokens {
            access: format!("a-{refresh}"),
            refresh: refresh.to_string(),
            expires: 1,
            account_id: None,
        };
        store
            .insert("p", "a", &AuthEntry::OAuth(tokens("r0")))
            .unwrap();

        // The first refresher (holding r0) wins and writes r1.
        assert_eq!(
            store.swap_oauth("p", "a", "r0", &tokens("r1")).unwrap(),
            None
        );
        // A second refresher that also started from r0 adopts r1.
        let adopted = store.swap_oauth("p", "a", "r0", &tokens("r2")).unwrap();
        assert_eq!(adopted, Some(tokens("r1")));
        let stored = store.get("p", "a").unwrap().unwrap();
        assert_eq!(stored.oauth().unwrap().refresh, "r1");

        std::env::remove_var("LOCAL_PROXY_CONFIG_DIR");
    }

    #[test]
    fn revision_moves_only_for_other_connections_account_changes() {
        let _guard = crate::TEST_STATE_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("LOCAL_PROXY_CONFIG_DIR", dir.path());
        let store = SqlCipherCredentialStore;
        let entry = |key: &str| AuthEntry::Api {
            key: key.to_string(),
        };

        let start = store.revision().unwrap();
        store.insert("p", "own", &entry("k")).unwrap();
        let own = store.revision().unwrap();
        assert_eq!(own.0, start.0, "own writes are not foreign changes");
        assert_ne!(own.1, start.1, "an added account moves the revision");

        // A second connection stands in for the CLI process.
        let other = open_db(&paths::accounts_db()).unwrap();
        other
            .execute(
                "UPDATE accounts SET credential=?1 WHERE alias='own'",
                [r#"{"type":"api","key":"k2"}"#],
            )
            .unwrap();
        let touched = store.revision().unwrap();
        assert_ne!(touched.0, own.0, "a foreign commit moves data_version");
        assert_eq!(
            touched.1, own.1,
            "a credential update is not an account change"
        );
        bump_revision(&other).unwrap();
        drop(other);
        assert_ne!(store.revision().unwrap().1, own.1);

        std::env::remove_var("LOCAL_PROXY_CONFIG_DIR");
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
