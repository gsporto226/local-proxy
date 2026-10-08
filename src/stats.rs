//! Local usage statistics sampled from upstream requests.
//!
//! Rows are stored in a `SQLite` database (`stats.db` in the global config
//! dir) and queried by `local-proxy stats`. Recording is best-effort: a failure
//! to open or write the database is logged and swallowed so that a stats
//! problem never breaks the proxy.
//!
//! The casts below convert between the Rust numeric types and SQLite INTEGER
//! (`i64`), which is intentional at this boundary.
#![allow(
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap,
    clippy::cast_possible_truncation,
    clippy::cast_lossless,
    clippy::cast_precision_loss
)]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use thiserror::Error;

/// Where the database lives: `<config dir>/stats.db`.
#[must_use]
pub fn stats_db() -> PathBuf {
    crate::config::global_config_dir().join("stats.db")
}

/// One recorded proxy request, the unit appended to the database.
#[derive(Debug, Clone, Default)]
pub struct StatLine {
    /// The proxied endpoint, e.g. `/v1/messages`.
    pub endpoint: &'static str,
    /// The upstream provider the request was routed to.
    pub provider: String,
    /// The account alias the request was routed through, or `"default"`.
    pub alias: String,
    /// The upstream model used.
    pub model: String,
    /// Number of input (prompt) tokens as reported by the upstream, if known.
    pub input_tokens: u64,
    /// Number of output (completion) tokens as reported by the upstream, if known.
    pub output_tokens: u64,
    /// Whether the response was streamed (SSE).
    pub streamed: bool,
    /// The HTTP status returned to the client.
    pub status: u16,
    /// Whether the request failed (non-2xx or upstream error).
    pub error: bool,
    /// Energy metadata reported by the upstream, if any.
    pub energy: Option<crate::translate::EnergyCost>,
    /// Cost metadata reported by the upstream, if any.
    pub cost: Option<crate::translate::EnergyCost>,
    /// The client session (`X-Claude-Code-Session-Id`), or `""` when absent.
    pub session_id: String,
}

/// Errors opening or querying the local statistics database.
#[derive(Debug, Error)]
pub enum StatsError {
    /// The database could not be created or connected.
    #[error("failed to open stats database {path}: {source}")]
    Open {
        /// Path of the database file.
        path: PathBuf,
        /// Underlying `SQLite` error.
        #[source]
        source: rusqlite::Error,
    },
    /// A statement could not be prepared or executed.
    #[error("failed to run stats query: {source}")]
    Query {
        /// Underlying `SQLite` error.
        #[source]
        source: rusqlite::Error,
    },
}

/// Create the `requests` table if it does not exist yet, migrating any older
/// schema in place (adding missing columns).
fn ensure_schema(conn: &rusqlite::Connection) -> Result<(), rusqlite::Error> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS requests (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            ts INTEGER NOT NULL,
            endpoint TEXT NOT NULL,
            provider TEXT NOT NULL,
            alias TEXT NOT NULL DEFAULT 'default',
            model TEXT NOT NULL,
            input_tokens INTEGER NOT NULL DEFAULT 0,
            output_tokens INTEGER NOT NULL DEFAULT 0,
            streamed INTEGER NOT NULL DEFAULT 0,
            status INTEGER NOT NULL,
            latency_ms INTEGER NOT NULL DEFAULT 0,
            error INTEGER NOT NULL DEFAULT 0,
            energy_kwh_um INTEGER,
            cost_usd_um INTEGER,
            session_id TEXT NOT NULL DEFAULT ''
        );
        CREATE INDEX IF NOT EXISTS idx_requests_ts ON requests(ts);
        CREATE INDEX IF NOT EXISTS idx_requests_provider ON requests(provider);",
    )?;
    // Lightweight migration for databases created before energy/cost existed.
    // Guarded by a column-exists check so prior stats survive in place.
    let cols = ["energy_kwh_um", "cost_usd_um", "session_id", "alias"];
    for col in cols {
        let exists: bool = conn
            .query_row(
                "SELECT 1 FROM pragma_table_info('requests') WHERE name = ?1",
                [col],
                |r| r.get(0),
            )
            .unwrap_or(false);
        if !exists {
            let ty = match col {
                "alias" => "TEXT NOT NULL DEFAULT 'default'",
                "session_id" => "TEXT NOT NULL DEFAULT ''",
                _ => "INTEGER",
            };
            conn.execute_batch(&format!("ALTER TABLE requests ADD COLUMN {col} {ty}"))?;
        }
    }
    // Created after the migration so it works on pre-existing databases too.
    conn.execute_batch("CREATE INDEX IF NOT EXISTS idx_requests_session ON requests(session_id);")?;
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_requests_alias ON requests(provider, alias);",
    )?;
    Ok(())
}

/// Open the stats database with settings that tolerate concurrent access.
///
/// Every call opens a fresh connection (writers and readers alike), so WAL
/// journal mode and a busy timeout are applied on each open: WAL lets a
/// background proxy and a `launch`-spawned ephemeral proxy write to the same
/// database concurrently without `SQLITE_BUSY` failures, and the busy timeout
/// makes a writer wait rather than fail when the other process holds the lock.
///
/// # Errors
///
/// Returns a [`StatsError::Open`] if the connection cannot be opened or a
/// [`StatsError::Query`] if a pragma cannot be applied.
fn open(path: &Path) -> Result<rusqlite::Connection, StatsError> {
    let conn = rusqlite::Connection::open(path).map_err(|source| StatsError::Open {
        path: path.to_path_buf(),
        source,
    })?;
    conn.busy_timeout(Duration::from_secs(5))
        .map_err(|source| StatsError::Query { source })?;
    conn.pragma_update(None, "journal_mode", "WAL")
        .map_err(|source| StatsError::Query { source })?;
    conn.pragma_update(None, "synchronous", "NORMAL")
        .map_err(|source| StatsError::Query { source })?;
    Ok(conn)
}

fn open_stats(path: &Path) -> Result<rusqlite::Connection, StatsError> {
    let conn = open(path)?;
    ensure_schema(&conn).map_err(|source| StatsError::Query { source })?;
    Ok(conn)
}

/// Record one proxy request in the local database, best-effort.
///
/// A failure to open or write the database is logged with `tracing` and ignored
/// so the proxy keeps serving. The `started` instant is used to compute latency
/// from the call-site snapshot until the write happens.
#[allow(clippy::needless_pass_by_value)]
pub fn record(started: Instant, stat: StatLine) {
    let latency_ms = started.elapsed().as_millis() as u64;
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    let path = stats_db();
    match write_line(&path, &stat, ts, latency_ms) {
        Ok(()) => crate::admin::emit(
            "request",
            serde_json::json!({
                "ts": ts,
                "provider": stat.provider,
                "model": stat.model,
                "input_tokens": stat.input_tokens,
                "output_tokens": stat.output_tokens,
                "status": stat.status,
                "latency_ms": latency_ms,
                "cost_usd": stat.cost.and_then(|c| c.request_cost_usd),
                "session_id": stat.session_id,
            }),
        ),
        Err(e) => tracing::warn!(
            target: "local_proxy",
            error = %e,
            "falhou ao registrar stats (dados não persistidos)"
        ),
    }
}

fn write_line(path: &Path, stat: &StatLine, ts: i64, latency_ms: u64) -> Result<(), StatsError> {
    let conn = open_stats(path)?;
    conn.execute(
        "INSERT INTO requests
            (ts, endpoint, provider, alias, model, input_tokens, output_tokens,
             streamed, status, latency_ms, error, energy_kwh_um, cost_usd_um,
             session_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
        rusqlite::params![
            ts,
            stat.endpoint,
            stat.provider,
            stat.alias,
            stat.model,
            stat.input_tokens,
            stat.output_tokens,
            i64::from(stat.streamed),
            i64::from(stat.status),
            latency_ms,
            i64::from(stat.error),
            stat.energy.and_then(|e| e.energy_kwh).map(to_um),
            stat.cost.and_then(|c| c.request_cost_usd).map(to_um),
            stat.session_id,
        ],
    )
    .map_err(|source| StatsError::Query { source })?;
    Ok(())
}

/// Convert a float (e.g. kWh or USD) to fixed-point micro-units for `SQLite`
/// INTEGER storage, rounding to nearest.
fn to_um(v: f64) -> i64 {
    (v * 1_000_000.0).round() as i64
}

/// A time window used to filter `stats` queries. `None` covers all time; a
/// fixed `seconds` covers everything at or after `now - seconds`.
#[derive(Debug, Clone, Copy)]
pub struct TimeWindow {
    /// Coverage start (unix seconds), inclusive, or `None` for all time.
    pub since: Option<i64>,
}

/// Aggregate totals over the matching window.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RowSummary {
    /// Number of recorded requests.
    pub requests: u64,
    /// Sum of input tokens.
    pub input_tokens: u64,
    /// Sum of output tokens.
    pub output_tokens: u64,
    /// Sum of latency, in milliseconds.
    pub latency_ms: u64,
    /// Number of errored requests.
    pub errors: u64,
    /// Sum of energy in micro-kWh.
    pub energy_kwh_um: u64,
    /// Sum of cost in micro-USD.
    pub cost_usd_um: u64,
}

/// Per-account (provider + alias) aggregate over the matching window.
#[derive(Debug, Clone)]
pub struct AccountStats {
    /// Provider name.
    pub provider: String,
    /// Account alias.
    pub alias: String,
    /// Number of requests routed through this account.
    pub requests: u64,
    /// Sum of input tokens.
    pub input_tokens: u64,
    /// Sum of output tokens.
    pub output_tokens: u64,
    /// Sum of latency, in milliseconds.
    pub latency_ms: u64,
    /// Sum of energy in micro-kWh.
    pub energy_kwh_um: u64,
    /// Sum of cost in micro-USD.
    pub cost_usd_um: u64,
}

/// One upstream-reported quota window for a subscription account.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct UsageWindow {
    /// Percentage of the limit consumed.
    pub utilization: f64,
    /// Provider-reported reset time (ISO-8601 or Unix seconds).
    pub resets_at: Option<String>,
}

/// Latest upstream quota snapshot for one provider account.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct AccountUsage {
    /// Provider name.
    pub provider: String,
    /// Account alias.
    pub alias: String,
    /// Five-hour usage window, when the provider reports one.
    pub five_hour: Option<UsageWindow>,
    /// Seven-day usage window, when the provider reports one.
    pub seven_day: Option<UsageWindow>,
    /// Monthly extra-usage window, when the provider reports one.
    pub monthly: Option<UsageWindow>,
    /// Last-observed Unix timestamp in seconds.
    pub fetched_at: i64,
}

/// Per-provider aggregate over the matching window.
#[derive(Debug, Clone)]
pub struct ProviderStats {
    /// Provider name.
    pub provider: String,
    /// Number of requests routed to this provider.
    pub requests: u64,
    /// Sum of input tokens.
    pub input_tokens: u64,
    /// Sum of output tokens.
    pub output_tokens: u64,
    /// Sum of latency, in milliseconds.
    pub latency_ms: u64,
    /// Sum of energy in micro-kWh.
    pub energy_kwh_um: u64,
    /// Sum of cost in micro-USD.
    pub cost_usd_um: u64,
}

/// One raw request row, used for the recent/detail view.
#[derive(Debug, Clone)]
pub struct RequestRow {
    /// Unix timestamp (seconds).
    pub ts: i64,
    /// Proxied endpoint.
    pub endpoint: String,
    /// Upstream provider.
    pub provider: String,
    /// Account alias.
    pub alias: String,
    /// Upstream model.
    pub model: String,
    /// Input tokens.
    pub input_tokens: u64,
    /// Output tokens.
    pub output_tokens: u64,
    /// Whether the response was streamed.
    pub streamed: bool,
    /// HTTP status.
    pub status: u16,
    /// Latency in milliseconds.
    pub latency_ms: u64,
    /// Whether the request errored.
    pub error: bool,
    /// Energy in micro-kWh, if reported.
    pub energy_kwh_um: Option<u64>,
    /// Cost in micro-USD, if reported.
    pub cost_usd_um: Option<u64>,
    /// The client session (`X-Claude-Code-Session-Id`), or `""` when absent.
    pub session_id: String,
}

/// Overall totals for the window, or `None` if the database does not exist yet.
///
/// # Errors
///
/// Returns a [`StatsError::Open`] if the database cannot be opened or a
/// [`StatsError::Query`] if the query fails.
pub fn summary(window: TimeWindow) -> Result<Option<RowSummary>, StatsError> {
    let path = stats_db();
    if !path.exists() {
        return Ok(None);
    }
    let conn = open_stats(&path)?;
    summary_on(&conn, window).map(Some)
}

/// Compute the overall totals for `window` over a live connection.
///
/// # Errors
///
/// Returns a [`StatsError::Query`] if the query fails.
pub fn summary_on(
    conn: &rusqlite::Connection,
    window: TimeWindow,
) -> Result<RowSummary, StatsError> {
    let (wsql, params) = where_clause(window);
    let mut stmt = conn
        .prepare(&format!(
            "SELECT COUNT(*), COALESCE(SUM(input_tokens),0), COALESCE(SUM(output_tokens),0),
                    COALESCE(SUM(latency_ms),0), COALESCE(SUM(error),0),
                    COALESCE(SUM(energy_kwh_um),0), COALESCE(SUM(cost_usd_um),0)
             FROM requests {wsql}"
        ))
        .map_err(|source| StatsError::Query { source })?;
    stmt.query_row(rusqlite::params_from_iter(params), |r| {
        Ok(RowSummary {
            requests: r.get::<_, i64>(0)? as u64,
            input_tokens: r.get::<_, i64>(1)? as u64,
            output_tokens: r.get::<_, i64>(2)? as u64,
            latency_ms: r.get::<_, i64>(3)? as u64,
            errors: r.get::<_, i64>(4)? as u64,
            energy_kwh_um: r.get::<_, i64>(5)? as u64,
            cost_usd_um: r.get::<_, i64>(6)? as u64,
        })
    })
    .map_err(|source| StatsError::Query { source })
}

/// Per-provider aggregates for the window, or `None` if the database does not
/// exist yet. Rows are ordered by provider name.
///
/// # Errors
///
/// Returns a [`StatsError::Open`] if the database cannot be opened or a
/// [`StatsError::Query`] if the query fails.
pub fn by_provider(window: TimeWindow) -> Result<Option<Vec<ProviderStats>>, StatsError> {
    let path = stats_db();
    if !path.exists() {
        return Ok(None);
    }
    let conn = open_stats(&path)?;
    by_provider_on(&conn, window).map(Some)
}

/// Per-provider aggregates for `window` over a live connection.
///
/// # Errors
///
/// Returns a [`StatsError::Query`] if the query fails.
pub fn by_provider_on(
    conn: &rusqlite::Connection,
    window: TimeWindow,
) -> Result<Vec<ProviderStats>, StatsError> {
    let (wsql, params) = where_clause(window);
    let mut stmt = conn
        .prepare(&format!(
            "SELECT provider, COUNT(*), COALESCE(SUM(input_tokens),0),
                    COALESCE(SUM(output_tokens),0), COALESCE(SUM(latency_ms),0),
                    COALESCE(SUM(energy_kwh_um),0), COALESCE(SUM(cost_usd_um),0)
             FROM requests {wsql}
             GROUP BY provider ORDER BY provider"
        ))
        .map_err(|source| StatsError::Query { source })?;
    let rows = stmt
        .query_map(rusqlite::params_from_iter(params), |r| {
            Ok(ProviderStats {
                provider: r.get(0)?,
                requests: r.get::<_, i64>(1)? as u64,
                input_tokens: r.get::<_, i64>(2)? as u64,
                output_tokens: r.get::<_, i64>(3)? as u64,
                latency_ms: r.get::<_, i64>(4)? as u64,
                energy_kwh_um: r.get::<_, i64>(5)? as u64,
                cost_usd_um: r.get::<_, i64>(6)? as u64,
            })
        })
        .map_err(|source| StatsError::Query { source })?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|source| StatsError::Query { source })?;
    Ok(rows)
}

/// Per-account aggregates for the window, or `None` if the database does not
/// exist yet.
///
/// # Errors
///
/// Returns a [`StatsError::Open`] if the database cannot be opened or a
/// [`StatsError::Query`] if the query fails.
pub fn by_account(window: TimeWindow) -> Result<Option<Vec<AccountStats>>, StatsError> {
    let path = stats_db();
    if !path.exists() {
        return Ok(None);
    }
    let conn = open_stats(&path)?;
    by_account_on(&conn, window).map(Some)
}

/// Per-account aggregates for `window` over a live connection.
///
/// # Errors
///
/// Returns a [`StatsError::Query`] if the query fails.
pub fn by_account_on(
    conn: &rusqlite::Connection,
    window: TimeWindow,
) -> Result<Vec<AccountStats>, StatsError> {
    let (wsql, params) = where_clause(window);
    let mut stmt = conn
        .prepare(&format!(
            "SELECT provider, COALESCE(CAST(alias AS TEXT), 'default'), COUNT(*), COALESCE(SUM(input_tokens),0),
                    COALESCE(SUM(output_tokens),0), COALESCE(SUM(latency_ms),0),
                    COALESCE(SUM(energy_kwh_um),0), COALESCE(SUM(cost_usd_um),0)
             FROM requests {wsql}
             GROUP BY provider, 2 ORDER BY provider, 2"
        ))
        .map_err(|source| StatsError::Query { source })?;
    let rows = stmt
        .query_map(rusqlite::params_from_iter(params), |r| {
            Ok(AccountStats {
                provider: r.get(0)?,
                alias: r.get(1)?,
                requests: r.get::<_, i64>(2)? as u64,
                input_tokens: r.get::<_, i64>(3)? as u64,
                output_tokens: r.get::<_, i64>(4)? as u64,
                latency_ms: r.get::<_, i64>(5)? as u64,
                energy_kwh_um: r.get::<_, i64>(6)? as u64,
                cost_usd_um: r.get::<_, i64>(7)? as u64,
            })
        })
        .map_err(|source| StatsError::Query { source })?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|source| StatsError::Query { source })?;
    Ok(rows)
}

/// The most recent request rows in the window, newest first, or `None` if the
/// database does not exist yet.
///
/// # Errors
///
/// Returns a [`StatsError::Open`] if the database cannot be opened or a
/// [`StatsError::Query`] if the query fails.
pub fn recent(window: TimeWindow, limit: u32) -> Result<Option<Vec<RequestRow>>, StatsError> {
    let path = stats_db();
    if !path.exists() {
        return Ok(None);
    }
    let conn = open_stats(&path)?;
    recent_on(&conn, window, limit).map(Some)
}

/// The most recent request rows in `window`, newest first, over a live
/// connection.
///
/// # Errors
///
/// Returns a [`StatsError::Query`] if the query fails.
pub fn recent_on(
    conn: &rusqlite::Connection,
    window: TimeWindow,
    limit: u32,
) -> Result<Vec<RequestRow>, StatsError> {
    let (wsql, params) = where_clause(window);
    let sql = format!(
        "SELECT ts, endpoint, provider, COALESCE(CAST(alias AS TEXT), 'default'), model, input_tokens, output_tokens,
                streamed, status, latency_ms, error, energy_kwh_um, cost_usd_um,
                session_id
         FROM requests {wsql}
         ORDER BY ts DESC, id DESC LIMIT {limit}"
    );
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|source| StatsError::Query { source })?;
    let rows = stmt
        .query_map(rusqlite::params_from_iter(params), |r| {
            Ok(RequestRow {
                ts: r.get(0)?,
                endpoint: r.get(1)?,
                provider: r.get(2)?,
                alias: r.get(3)?,
                model: r.get(4)?,
                input_tokens: r.get::<_, i64>(5)? as u64,
                output_tokens: r.get::<_, i64>(6)? as u64,
                streamed: r.get::<_, i64>(7)? != 0,
                status: r.get::<_, i64>(8)? as u16,
                latency_ms: r.get::<_, i64>(9)? as u64,
                error: r.get::<_, i64>(10)? != 0,
                energy_kwh_um: r.get::<_, Option<i64>>(11)?.map(|v| v as u64),
                cost_usd_um: r.get::<_, Option<i64>>(12)?.map(|v| v as u64),
                session_id: r.get(13)?,
            })
        })
        .map_err(|source| StatsError::Query { source })?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|source| StatsError::Query { source })?;
    Ok(rows)
}

/// Build the SQL `WHERE` clause and its parameters for a time window.
fn where_clause(window: TimeWindow) -> (String, Vec<rusqlite::types::Value>) {
    window.since.map_or_else(
        || (String::new(), Vec::new()),
        |since| ("WHERE ts >= ?1".to_string(), vec![since.into()]),
    )
}

/// Aggregated figures for a single client session (served by `/admin`).
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct SessionStats {
    /// Number of recorded requests in the session.
    pub requests: u64,
    /// Sum of input tokens.
    pub tokens_in: u64,
    /// Sum of output tokens.
    pub tokens_out: u64,
    /// Sum of reported cost in USD (only over rows carrying a known cost).
    pub cost_usd: f64,
    /// Number of requests whose cost was reported (unknown-cost rows excluded).
    pub cost_known_requests: u64,
    /// The most recent model used in the session, if any.
    pub last_model: Option<String>,
}

/// Aggregate a single session's requests, or `None` when the database does not
/// exist or the session has no rows.
///
/// # Errors
///
/// Returns a [`StatsError::Open`] if the database cannot be opened or a
/// [`StatsError::Query`] if the query fails.
pub fn session(session_id: &str) -> Result<Option<SessionStats>, StatsError> {
    let path = stats_db();
    if !path.exists() {
        return Ok(None);
    }
    if session_id.is_empty() {
        return Ok(None);
    }
    let conn = open_stats(&path)?;
    session_on(&conn, session_id)
}

/// Aggregate `session_id`'s requests over a live connection. Returns `None`
/// when there are no rows for the session.
///
/// # Errors
///
/// Returns a [`StatsError::Query`] if the query fails.
pub fn session_on(
    conn: &rusqlite::Connection,
    session_id: &str,
) -> Result<Option<SessionStats>, StatsError> {
    let mut stmt = conn
        .prepare(
            "SELECT COUNT(*),
                    COALESCE(SUM(input_tokens),0),
                    COALESCE(SUM(output_tokens),0),
                    COALESCE(SUM(cost_usd_um),0),
                    COUNT(cost_usd_um),
                    (SELECT model FROM requests r2
                      WHERE r2.session_id = ?1 ORDER BY ts DESC, id DESC LIMIT 1)
             FROM requests WHERE session_id = ?1",
        )
        .map_err(|source| StatsError::Query { source })?;
    let first = stmt
        .query_row(rusqlite::params![session_id], |r| {
            let requests = r.get::<_, i64>(0)?;
            let last_model = r.get::<_, Option<String>>(5)?;
            Ok(if requests == 0 {
                None
            } else {
                Some(SessionStats {
                    requests: requests as u64,
                    tokens_in: r.get::<_, i64>(1)? as u64,
                    tokens_out: r.get::<_, i64>(2)? as u64,
                    cost_usd: r.get::<_, i64>(3)? as f64 / 1_000_000.0,
                    cost_known_requests: r.get::<_, i64>(4)? as u64,
                    last_model,
                })
            })
        })
        .map_err(|source| StatsError::Query { source })?;
    Ok(first)
}

/// Remember the reasoning effort `session_id` last requested, best-effort.
///
/// Served by `/admin` per session; failures are logged and ignored.
pub fn record_effort(session_id: &str, effort: &str) {
    let res = open(&stats_db()).and_then(|conn| {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS session_effort (
                session_id TEXT PRIMARY KEY, effort TEXT NOT NULL)",
        )
        .and_then(|()| {
            conn.execute(
                "INSERT OR REPLACE INTO session_effort VALUES (?1, ?2)",
                [session_id, effort],
            )
        })
        .map_err(|source| StatsError::Query { source })
    });
    if let Err(e) = res {
        tracing::warn!(target: "local_proxy", error = %e, "falhou ao registrar effort");
    }
}

fn ensure_account_usage_schema(conn: &rusqlite::Connection) -> Result<(), rusqlite::Error> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS account_usage (
            provider TEXT NOT NULL,
            alias TEXT NOT NULL,
            five_hour REAL,
            five_hour_resets_at TEXT,
            seven_day REAL,
            seven_day_resets_at TEXT,
            monthly REAL,
            monthly_resets_at TEXT,
            fetched_at INTEGER NOT NULL,
            PRIMARY KEY (provider, alias)
        );",
    )
}

/// Store a complete provider usage response for one account.
///
/// # Errors
///
/// Returns a [`StatsError`] if the database cannot be opened or written.
pub fn save_account_usage(usage: &AccountUsage) -> Result<(), StatsError> {
    let conn = open(&stats_db())?;
    ensure_account_usage_schema(&conn).map_err(|source| StatsError::Query { source })?;
    conn.execute(
        "INSERT INTO account_usage
            (provider, alias, five_hour, five_hour_resets_at, seven_day,
             seven_day_resets_at, monthly, monthly_resets_at, fetched_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
         ON CONFLICT(provider, alias) DO UPDATE SET
            five_hour = excluded.five_hour,
            five_hour_resets_at = excluded.five_hour_resets_at,
            seven_day = excluded.seven_day,
            seven_day_resets_at = excluded.seven_day_resets_at,
            monthly = excluded.monthly,
            monthly_resets_at = excluded.monthly_resets_at,
            fetched_at = excluded.fetched_at",
        rusqlite::params![
            usage.provider,
            usage.alias,
            usage.five_hour.as_ref().map(|window| window.utilization),
            usage
                .five_hour
                .as_ref()
                .and_then(|window| window.resets_at.as_deref()),
            usage.seven_day.as_ref().map(|window| window.utilization),
            usage
                .seven_day
                .as_ref()
                .and_then(|window| window.resets_at.as_deref()),
            usage.monthly.as_ref().map(|window| window.utilization),
            usage
                .monthly
                .as_ref()
                .and_then(|window| window.resets_at.as_deref()),
            usage.fetched_at,
        ],
    )
    .map_err(|source| StatsError::Query { source })?;
    Ok(())
}

/// Load the latest cached usage snapshots for all supported accounts.
///
/// # Errors
///
/// Returns a [`StatsError`] if the database cannot be read.
pub fn account_usage_cache() -> Result<Vec<AccountUsage>, StatsError> {
    let path = stats_db();
    if !path.exists() {
        return Ok(Vec::new());
    }
    let conn = open(&path)?;
    ensure_account_usage_schema(&conn).map_err(|source| StatsError::Query { source })?;
    let mut stmt = conn
        .prepare(
            "SELECT provider, alias, five_hour, five_hour_resets_at, seven_day,
                    seven_day_resets_at, monthly, monthly_resets_at, fetched_at
             FROM account_usage ORDER BY provider, alias",
        )
        .map_err(|source| StatsError::Query { source })?;
    let rows = stmt
        .query_map([], |row| {
            let window = |utilization: Option<f64>, resets_at: Option<String>| {
                utilization.map(|utilization| UsageWindow {
                    utilization,
                    resets_at,
                })
            };
            Ok(AccountUsage {
                provider: row.get(0)?,
                alias: row.get(1)?,
                five_hour: window(row.get(2)?, row.get(3)?),
                seven_day: window(row.get(4)?, row.get(5)?),
                monthly: window(row.get(6)?, row.get(7)?),
                fetched_at: row.get(8)?,
            })
        })
        .map_err(|source| StatsError::Query { source })?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|source| StatsError::Query { source })?;
    Ok(rows)
}

/// Record subscription quota percentages observed on a model response.
/// Existing monthly usage remains intact when only the 5h/week headers arrive.
pub fn record_account_rate_limits(provider: &str, alias: &str, h5: f64, week: f64) {
    let res = open(&stats_db()).and_then(|conn| {
        ensure_account_usage_schema(&conn).map_err(|source| StatsError::Query { source })?;
        conn.execute(
            "INSERT INTO account_usage
                (provider, alias, five_hour, seven_day, fetched_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(provider, alias) DO UPDATE SET
                five_hour = excluded.five_hour,
                seven_day = excluded.seven_day,
                fetched_at = excluded.fetched_at",
            rusqlite::params![provider, alias, h5, week, unix_seconds()],
        )
        .map_err(|source| StatsError::Query { source })
    });
    if let Err(error) = res {
        tracing::warn!(target: "local_proxy", error = %error, "failed to record account quota headers");
    }
}

fn unix_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs() as i64)
}

/// Remember the latest account-wide subscription usage percents (5h, weekly), best-effort.
///
/// Account-wide, so a single row; served by `/admin`. Failures are logged and ignored.
pub fn record_rate_limits(h5: f64, week: f64) {
    let res = open(&stats_db()).and_then(|conn| {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS rate_limits (
                id INTEGER PRIMARY KEY CHECK (id = 0), h5 REAL NOT NULL, week REAL NOT NULL)",
        )
        .and_then(|()| {
            conn.execute(
                "INSERT OR REPLACE INTO rate_limits VALUES (0, ?1, ?2)",
                [h5, week],
            )
        })
        .map_err(|source| StatsError::Query { source })
    });
    if let Err(e) = res {
        tracing::warn!(target: "local_proxy", error = %e, "falhou ao registrar rate limits");
    }
    crate::admin::emit("rate_limits", serde_json::json!({ "h5": h5, "week": week }));
}

/// The latest recorded subscription usage percents `(5h, weekly)`, if any.
#[must_use]
pub fn rate_limits() -> Option<(f64, f64)> {
    let path = stats_db();
    if !path.exists() {
        return None;
    }
    open(&path)
        .ok()?
        .query_row("SELECT h5, week FROM rate_limits WHERE id = 0", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .ok()
}

/// The reasoning effort `session_id` last requested, if recorded.
#[must_use]
pub fn session_effort(session_id: &str) -> Option<String> {
    let path = stats_db();
    if session_id.is_empty() || !path.exists() {
        return None;
    }
    open(&path)
        .ok()?
        .query_row(
            "SELECT effort FROM session_effort WHERE session_id = ?1",
            [session_id],
            |r| r.get(0),
        )
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn in_memory() -> rusqlite::Connection {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        ensure_schema(&conn).unwrap();
        conn
    }

    fn insert(conn: &rusqlite::Connection, ts: i64, provider: &str, inp: u64, out: u64, err: bool) {
        conn.execute(
            "INSERT INTO requests
                (ts, endpoint, provider, model, input_tokens, output_tokens,
                 streamed, status, latency_ms, error)
             VALUES (?1,'/v1/messages',?2,'m',?3,?4,0,200,50,?5)",
            rusqlite::params![ts, provider, inp as i64, out as i64, i64::from(err)],
        )
        .unwrap();
    }

    fn insert_line(conn: &rusqlite::Connection, line: &StatLine) {
        conn.execute(
            "INSERT INTO requests
                (ts, endpoint, provider, alias, model, input_tokens, output_tokens,
                 streamed, status, latency_ms, error, energy_kwh_um, cost_usd_um,
                 session_id)
             VALUES (1,?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
            rusqlite::params![
                line.endpoint,
                line.provider,
                line.alias,
                line.model,
                line.input_tokens as i64,
                line.output_tokens as i64,
                i64::from(line.streamed),
                i64::from(line.status),
                50i64,
                i64::from(line.error),
                line.energy.and_then(|e| e.energy_kwh).map(to_um),
                line.cost.and_then(|c| c.request_cost_usd).map(to_um),
                line.session_id,
            ],
        )
        .unwrap();
    }

    fn all() -> TimeWindow {
        TimeWindow { since: None }
    }

    #[test]
    fn schema_created_on_first_insert() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        // ensure_schema creates the table
        ensure_schema(&conn).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM requests", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn migration_adds_default_text_alias_for_existing_requests() {
        let db = tempfile::NamedTempFile::new().unwrap();
        {
            let conn = rusqlite::Connection::open(db.path()).unwrap();
            conn.execute_batch(
                "CREATE TABLE requests (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    ts INTEGER NOT NULL,
                    endpoint TEXT NOT NULL,
                    provider TEXT NOT NULL,
                    model TEXT NOT NULL,
                    input_tokens INTEGER NOT NULL DEFAULT 0,
                    output_tokens INTEGER NOT NULL DEFAULT 0,
                    streamed INTEGER NOT NULL DEFAULT 0,
                    status INTEGER NOT NULL,
                    latency_ms INTEGER NOT NULL DEFAULT 0,
                    error INTEGER NOT NULL DEFAULT 0,
                    energy_kwh_um INTEGER,
                    cost_usd_um INTEGER,
                    session_id TEXT NOT NULL DEFAULT ''
                );
                INSERT INTO requests (ts, endpoint, provider, model, status)
                VALUES (1, '/v1/messages', 'openai', 'gpt-x', 200);",
            )
            .unwrap();
        }
        let conn = open_stats(db.path()).unwrap();

        let accounts = by_account_on(&conn, all()).unwrap();
        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0].provider, "openai");
        assert_eq!(accounts[0].alias, "default");
    }

    #[test]
    fn migration_reads_aliases_from_an_earlier_integer_column() {
        let db = tempfile::NamedTempFile::new().unwrap();
        {
            let conn = rusqlite::Connection::open(db.path()).unwrap();
            conn.execute_batch(
                "CREATE TABLE requests (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    ts INTEGER NOT NULL,
                    endpoint TEXT NOT NULL,
                    provider TEXT NOT NULL,
                    alias INTEGER,
                    model TEXT NOT NULL,
                    input_tokens INTEGER NOT NULL DEFAULT 0,
                    output_tokens INTEGER NOT NULL DEFAULT 0,
                    streamed INTEGER NOT NULL DEFAULT 0,
                    status INTEGER NOT NULL,
                    latency_ms INTEGER NOT NULL DEFAULT 0,
                    error INTEGER NOT NULL DEFAULT 0,
                    energy_kwh_um INTEGER,
                    cost_usd_um INTEGER,
                    session_id TEXT NOT NULL DEFAULT ''
                );
                INSERT INTO requests (ts, endpoint, provider, model, status)
                VALUES (1, '/v1/messages', 'openai', 'gpt-x', 200);
                INSERT INTO requests (ts, endpoint, provider, alias, model, status)
                VALUES (2, '/v1/messages', 'openai', 123, 'gpt-x', 200);",
            )
            .unwrap();
        }

        let conn = open_stats(db.path()).unwrap();
        let accounts = by_account_on(&conn, all()).unwrap();
        assert_eq!(accounts.len(), 2);
        assert_eq!(accounts[0].alias, "123");
        assert_eq!(accounts[1].alias, "default");
        let recent = recent_on(&conn, all(), 2).unwrap();
        assert_eq!(recent[0].alias, "123");
        assert_eq!(recent[1].alias, "default");
    }

    #[test]
    fn writes_preserve_the_routed_account_alias() {
        let db = tempfile::NamedTempFile::new().unwrap();
        let line = StatLine {
            endpoint: "/v1/messages",
            provider: "openai".to_string(),
            alias: "work".to_string(),
            model: "gpt-x".to_string(),
            status: 200,
            ..StatLine::default()
        };

        write_line(db.path(), &line, 1, 0).unwrap();

        let conn = open_stats(db.path()).unwrap();
        let accounts = by_account_on(&conn, all()).unwrap();
        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0].alias, "work");
    }

    #[test]
    fn account_usage_cache_isolated_by_provider_and_alias() {
        let _guard = crate::TEST_STATE_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("LOCAL_PROXY_CONFIG_DIR", dir.path());
        let chatgpt = AccountUsage {
            provider: "chatgpt".to_string(),
            alias: "personal".to_string(),
            five_hour: Some(UsageWindow {
                utilization: 25.0,
                resets_at: Some("2000000000".to_string()),
            }),
            seven_day: None,
            monthly: None,
            fetched_at: 100,
        };
        let claude = AccountUsage {
            provider: "claude".to_string(),
            alias: "max".to_string(),
            five_hour: Some(UsageWindow {
                utilization: 60.0,
                resets_at: None,
            }),
            seven_day: Some(UsageWindow {
                utilization: 35.0,
                resets_at: Some("2026-10-12T00:00:00Z".to_string()),
            }),
            monthly: Some(UsageWindow {
                utilization: 10.0,
                resets_at: None,
            }),
            fetched_at: 200,
        };

        save_account_usage(&chatgpt).unwrap();
        save_account_usage(&claude).unwrap();

        let cached = account_usage_cache().unwrap();
        assert_eq!(cached, [chatgpt, claude]);
        std::env::remove_var("LOCAL_PROXY_CONFIG_DIR");
    }

    #[test]
    fn account_quota_headers_update_only_the_matching_account() {
        let _guard = crate::TEST_STATE_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("LOCAL_PROXY_CONFIG_DIR", dir.path());
        save_account_usage(&AccountUsage {
            provider: "chatgpt".to_string(),
            alias: "work".to_string(),
            five_hour: None,
            seven_day: None,
            monthly: Some(UsageWindow {
                utilization: 15.0,
                resets_at: None,
            }),
            fetched_at: 10,
        })
        .unwrap();

        record_account_rate_limits("chatgpt", "work", 25.0, 40.0);
        record_account_rate_limits("chatgpt", "personal", 5.0, 10.0);

        let cached = account_usage_cache().unwrap();
        assert_eq!(cached.len(), 2);
        let work = cached.iter().find(|item| item.alias == "work").unwrap();
        assert!((work.five_hour.as_ref().unwrap().utilization - 25.0).abs() < f64::EPSILON);
        assert!((work.seven_day.as_ref().unwrap().utilization - 40.0).abs() < f64::EPSILON);
        assert!((work.monthly.as_ref().unwrap().utilization - 15.0).abs() < f64::EPSILON);
        let personal = cached.iter().find(|item| item.alias == "personal").unwrap();
        assert!((personal.five_hour.as_ref().unwrap().utilization - 5.0).abs() < f64::EPSILON);
        assert!((personal.seven_day.as_ref().unwrap().utilization - 10.0).abs() < f64::EPSILON);
        std::env::remove_var("LOCAL_PROXY_CONFIG_DIR");
    }

    #[test]
    fn summary_aggregates_and_tracks_errors() {
        let conn = in_memory();
        insert(&conn, 1, "openai", 10, 5, false);
        insert(&conn, 2, "anthropic", 20, 8, false);
        insert(&conn, 3, "openai", 30, 2, true);

        let s = summary_on(&conn, all()).unwrap();
        assert_eq!(s.requests, 3);
        assert_eq!(s.input_tokens, 60);
        assert_eq!(s.output_tokens, 15);
        assert_eq!(s.latency_ms, 150);
        assert_eq!(s.errors, 1);
    }

    #[test]
    fn by_provider_groups_and_orders() {
        let conn = in_memory();
        insert(&conn, 1, "openai", 10, 5, false);
        insert(&conn, 2, "anthropic", 20, 8, false);
        insert(&conn, 3, "openai", 30, 2, true);

        let rows = by_provider_on(&conn, all()).unwrap();
        let names: Vec<&str> = rows.iter().map(|r| r.provider.as_str()).collect();
        assert_eq!(names, ["anthropic", "openai"]);
        let openai = rows.iter().find(|r| r.provider == "openai").unwrap();
        assert_eq!(openai.requests, 2);
        assert_eq!(openai.input_tokens, 40);
    }

    #[test]
    fn time_window_filters_rows() {
        let conn = in_memory();
        insert(&conn, 100, "openai", 10, 5, false);
        insert(&conn, 200, "anthropic", 20, 8, false);
        insert(&conn, 300, "openai", 30, 2, true);

        // everything at or after ts=200
        let window = TimeWindow { since: Some(200) };
        let s = summary_on(&conn, window).unwrap();
        assert_eq!(s.requests, 2);
        let rows = by_provider_on(&conn, window).unwrap();
        assert_eq!(rows.iter().map(|r| r.requests).sum::<u64>(), 2);

        // nothing in the window
        let empty = TimeWindow { since: Some(9999) };
        assert_eq!(summary_on(&conn, empty).unwrap().requests, 0);
    }

    #[test]
    fn recent_returns_newest_first() {
        let conn = in_memory();
        insert(&conn, 100, "openai", 10, 5, false);
        insert(&conn, 300, "anthropic", 20, 8, false);
        insert(&conn, 200, "openai", 30, 2, true);

        let rows = recent_on(&conn, all(), 2).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].ts, 300); // newest first
        assert_eq!(rows[1].ts, 200);
        assert_eq!(rows[0].provider, "anthropic");
    }

    #[test]
    fn energy_and_cost_persist_and_aggregate() {
        let conn = in_memory();
        // a NeuralWatt-style row with energy and cost (micro-units)
        let e = crate::translate::EnergyCost {
            energy_joules: Some(54.0),
            energy_kwh: Some(1.5e-5),
            avg_power_watts: Some(55.3),
            request_cost_usd: None,
            cache_savings_usd: None,
        };
        let c = crate::translate::EnergyCost {
            energy_joules: None,
            energy_kwh: None,
            avg_power_watts: None,
            request_cost_usd: Some(1.04e-5),
            cache_savings_usd: Some(0.0),
        };
        let line = StatLine {
            endpoint: "/v1/messages",
            provider: "neuralwatt".to_string(),
            alias: "default".to_string(),
            model: "glm-5.2".to_string(),
            input_tokens: 10,
            output_tokens: 5,
            streamed: false,
            status: 200,
            error: false,
            energy: Some(e),
            cost: Some(c),
            session_id: String::new(),
        };
        insert_line(&conn, &line);

        let s = summary_on(&conn, all()).unwrap();
        // 1.5e-5 kWh -> 15 um; 1.04e-5 USD -> 10.4 -> 10 um (round)
        assert_eq!(s.energy_kwh_um, 15);
        assert_eq!(s.cost_usd_um, 10);

        let rows = recent_on(&conn, all(), 1).unwrap();
        assert_eq!(rows[0].energy_kwh_um, Some(15));
        assert_eq!(rows[0].cost_usd_um, Some(10));
    }

    #[test]
    fn legacy_schema_migrates_in_place() {
        // a database created before energy/cost columns
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE requests (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                ts INTEGER NOT NULL,
                endpoint TEXT NOT NULL,
                provider TEXT NOT NULL,
                model TEXT NOT NULL,
                input_tokens INTEGER NOT NULL DEFAULT 0,
                output_tokens INTEGER NOT NULL DEFAULT 0,
                streamed INTEGER NOT NULL DEFAULT 0,
                status INTEGER NOT NULL,
                latency_ms INTEGER NOT NULL DEFAULT 0,
                error INTEGER NOT NULL DEFAULT 0
            );",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO requests
                (ts, endpoint, provider, model, input_tokens, output_tokens,
                 streamed, status, latency_ms, error)
             VALUES (1,'/v1/messages','anthropic','m',1,1,0,200,5,0)",
            [],
        )
        .unwrap();

        // ensure_schema should add the missing columns without losing rows
        ensure_schema(&conn).unwrap();
        let s = summary_on(&conn, all()).unwrap();
        assert_eq!(s.requests, 1);
        assert_eq!(s.input_tokens, 1);
        assert_eq!(s.energy_kwh_um, 0);
    }

    #[test]
    fn session_aggregates_per_session_with_cost() {
        let conn = in_memory();
        // a row with reported cost, one without, for the same session
        let mut line = StatLine {
            endpoint: "/v1/messages",
            provider: "openrouter".to_string(),
            alias: "default".to_string(),
            model: "model-a".to_string(),
            input_tokens: 10,
            output_tokens: 5,
            streamed: false,
            status: 200,
            error: false,
            energy: None,
            cost: Some(crate::translate::EnergyCost {
                energy_joules: None,
                energy_kwh: None,
                avg_power_watts: None,
                request_cost_usd: Some(0.001),
                cache_savings_usd: None,
            }),
            session_id: "sess-1".to_string(),
        };
        insert_line(&conn, &line);
        line.cost = None;
        line.model = "model-b".to_string();
        insert_line(&conn, &line);
        // a different session must be excluded
        line.session_id = "sess-2".to_string();
        line.input_tokens = 999;
        insert_line(&conn, &line);

        let s = session_on(&conn, "sess-1").unwrap().unwrap();
        assert_eq!(s.requests, 2);
        assert_eq!(s.tokens_in, 20);
        assert_eq!(s.tokens_out, 10);
        // 0.001 USD -> 1000 um -> back to 0.001
        assert!((s.cost_usd - 0.001).abs() < 1e-9);
        assert_eq!(s.cost_known_requests, 1);
        assert_eq!(s.last_model.as_deref(), Some("model-b"));

        // unknown session -> None
        assert!(session_on(&conn, "nope").unwrap().is_none());
    }
}
