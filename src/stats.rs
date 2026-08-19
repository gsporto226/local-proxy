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

use std::path::PathBuf;
use std::time::Instant;

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

/// Create the `requests` table if it does not exist yet.
fn ensure_schema(conn: &rusqlite::Connection) -> Result<(), rusqlite::Error> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS requests (
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
        );
        CREATE INDEX IF NOT EXISTS idx_requests_ts ON requests(ts);
        CREATE INDEX IF NOT EXISTS idx_requests_provider ON requests(provider);",
    )
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
    if let Err(e) = write_line(&path, &stat, ts, latency_ms) {
        tracing::warn!(
            target: "local_proxy",
            error = %e,
            "falhou ao registrar stats (dados não persistidos)"
        );
    }
}

fn write_line(path: &PathBuf, stat: &StatLine, ts: i64, latency_ms: u64) -> Result<(), StatsError> {
    let conn = rusqlite::Connection::open(path).map_err(|source| StatsError::Open {
        path: path.clone(),
        source,
    })?;
    ensure_schema(&conn).map_err(|source| StatsError::Query { source })?;
    conn.execute(
        "INSERT INTO requests
            (ts, endpoint, provider, model, input_tokens, output_tokens,
             streamed, status, latency_ms, error)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        rusqlite::params![
            ts,
            stat.endpoint,
            stat.provider,
            stat.model,
            stat.input_tokens,
            stat.output_tokens,
            i64::from(stat.streamed),
            i64::from(stat.status),
            latency_ms,
            i64::from(stat.error),
        ],
    )
    .map_err(|source| StatsError::Query { source })?;
    Ok(())
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
    let conn =
        rusqlite::Connection::open(&path).map_err(|source| StatsError::Open { path, source })?;
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
                    COALESCE(SUM(latency_ms),0), COALESCE(SUM(error),0)
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
    let conn =
        rusqlite::Connection::open(&path).map_err(|source| StatsError::Open { path, source })?;
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
                    COALESCE(SUM(output_tokens),0), COALESCE(SUM(latency_ms),0)
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
    let conn =
        rusqlite::Connection::open(&path).map_err(|source| StatsError::Open { path, source })?;
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
        "SELECT ts, endpoint, provider, model, input_tokens, output_tokens,
                streamed, status, latency_ms, error
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
                model: r.get(3)?,
                input_tokens: r.get::<_, i64>(4)? as u64,
                output_tokens: r.get::<_, i64>(5)? as u64,
                streamed: r.get::<_, i64>(6)? != 0,
                status: r.get::<_, i64>(7)? as u16,
                latency_ms: r.get::<_, i64>(8)? as u64,
                error: r.get::<_, i64>(9)? != 0,
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
}
