use crate::domain::stats::{
    AccountStats, AccountUsage, ProviderStats, RequestRow, RowSummary, SessionStats, StatLine,
    StatsFilter,
};

/// An error opening or querying the statistics store.
#[derive(Debug, thiserror::Error, miette::Diagnostic)]
#[error("{0}")]
pub struct StatsError(pub String);

/// Storage of recorded requests and the aggregates read back from them.
///
/// Query methods return `Ok(None)` when nothing was ever recorded (the store
/// does not exist yet).
pub trait UsageStore: Send + Sync {
    /// Append one request row stamped `ts` (unix seconds) with its latency.
    ///
    /// # Errors
    ///
    /// Returns [`StatsError`] if the row cannot be written.
    fn insert(&self, stat: &StatLine, ts: i64, latency_ms: u64) -> Result<(), StatsError>;

    /// Overall totals for the filter.
    ///
    /// # Errors
    ///
    /// Returns [`StatsError`] if the store cannot be read.
    fn summary(&self, filter: StatsFilter<'_>) -> Result<Option<RowSummary>, StatsError>;

    /// Per-provider aggregates, ordered by provider name.
    ///
    /// # Errors
    ///
    /// Returns [`StatsError`] if the store cannot be read.
    fn by_provider(
        &self,
        filter: StatsFilter<'_>,
    ) -> Result<Option<Vec<ProviderStats>>, StatsError>;

    /// Per-account aggregates, ordered by provider then alias.
    ///
    /// # Errors
    ///
    /// Returns [`StatsError`] if the store cannot be read.
    fn by_account(&self, filter: StatsFilter<'_>) -> Result<Option<Vec<AccountStats>>, StatsError>;

    /// The most recent rows, newest first.
    ///
    /// # Errors
    ///
    /// Returns [`StatsError`] if the store cannot be read.
    fn recent(
        &self,
        filter: StatsFilter<'_>,
        limit: u32,
    ) -> Result<Option<Vec<RequestRow>>, StatsError>;

    /// One session's aggregate, or `None` when it has no rows.
    ///
    /// # Errors
    ///
    /// Returns [`StatsError`] if the store cannot be read.
    fn session(&self, session_id: &str) -> Result<Option<SessionStats>, StatsError>;

    /// Remember the reasoning effort `session_id` last requested.
    ///
    /// # Errors
    ///
    /// Returns [`StatsError`] if the value cannot be written.
    fn record_effort(&self, session_id: &str, effort: &str) -> Result<(), StatsError>;

    /// The reasoning effort `session_id` last requested, if recorded.
    fn session_effort(&self, session_id: &str) -> Option<String>;

    /// Remember the latest subscription usage percents `(5h, weekly)`.
    ///
    /// # Errors
    ///
    /// Returns [`StatsError`] if the value cannot be written.
    fn record_rate_limits(&self, h5: f64, week: f64) -> Result<(), StatsError>;

    /// The latest recorded subscription usage percents `(5h, weekly)`.
    fn rate_limits(&self) -> Option<(f64, f64)>;

    /// Record quota percents observed for one account, keeping its monthly
    /// window intact.
    ///
    /// # Errors
    ///
    /// Returns [`StatsError`] if the value cannot be written.
    fn record_account_rate_limits(
        &self,
        provider: &str,
        alias: &str,
        h5: f64,
        week: f64,
    ) -> Result<(), StatsError>;

    /// Store a complete usage snapshot for one account.
    ///
    /// # Errors
    ///
    /// Returns [`StatsError`] if the snapshot cannot be written.
    fn save_account_usage(&self, usage: &AccountUsage) -> Result<(), StatsError>;

    /// The cached usage snapshots of every account.
    ///
    /// # Errors
    ///
    /// Returns [`StatsError`] if the store cannot be read.
    fn account_usage_cache(&self) -> Result<Vec<AccountUsage>, StatsError>;
}
