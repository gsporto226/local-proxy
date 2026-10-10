//! Usage statistics: the request rows the proxy records and the aggregates
//! read back from them.

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
    pub energy: Option<crate::domain::translate::EnergyCost>,
    /// Cost metadata reported by the upstream, if any.
    pub cost: Option<crate::domain::translate::EnergyCost>,
    /// The client session (`X-Claude-Code-Session-Id`), or `""` when absent.
    pub session_id: String,
    /// Whether the upstream reported a cache hit for this request.
    /// `None` means cache-read telemetry was not reported.
    pub cache_hit: Option<bool>,
    /// Prompt tokens read from cache, or `None` when not reported.
    pub cache_read_tokens: Option<u64>,
    /// Prompt tokens written to cache (Anthropic only; `0` otherwise).
    pub cache_write_tokens: u64,
}

/// A time window used to filter `stats` queries. `None` covers all time; a
/// fixed `seconds` covers everything at or after `now - seconds`.
#[derive(Debug, Clone, Copy)]
pub struct TimeWindow {
    /// Coverage start (unix seconds), inclusive, or `None` for all time.
    pub since: Option<i64>,
}

/// Which request set a statistics query covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatsScope<'a> {
    /// Requests across all Claude Code sessions.
    All,
    /// Requests carrying this Claude Code session ID.
    Session(&'a str),
}

/// Time and session filters shared by stats aggregates and recent rows.
#[derive(Debug, Clone, Copy)]
pub struct StatsFilter<'a> {
    /// The timestamp lower bound, if any.
    pub window: TimeWindow,
    /// All sessions or one specific session.
    pub scope: StatsScope<'a>,
}

/// Request-level cache hit counts in an aggregate.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CacheStats {
    /// Requests with a positive upstream-reported cached-read count.
    pub hit_requests: u64,
    /// Requests whose upstream reported cached-read telemetry, including zero.
    pub reported_requests: u64,
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
    /// Request-level prompt-cache hits in this aggregate.
    pub cache: CacheStats,
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
    /// Request-level prompt-cache hits in this aggregate.
    pub cache: CacheStats,
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
    /// Request-level prompt-cache hits in this aggregate.
    pub cache: CacheStats,
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
