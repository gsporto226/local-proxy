//! The usage report behind `local-proxy stats` and `GET /admin/stats`.
#![allow(clippy::cast_precision_loss)]

use serde_json::{json, Value};

use crate::domain::stats::{
    AccountStats, CacheStats, ProviderStats, RequestRow, RowSummary, StatsFilter, StatsScope,
    TimeWindow,
};
use crate::ports::{StatsError, UsageStore};

/// Everything a report shows for one window and scope.
#[derive(Debug, Clone)]
pub struct StatsReport {
    /// Overall totals.
    pub summary: RowSummary,
    /// Per-provider aggregates.
    pub by_provider: Vec<ProviderStats>,
    /// Per-account aggregates.
    pub by_account: Vec<AccountStats>,
    /// The ten most recent requests.
    pub recent: Vec<RequestRow>,
}

/// The recognized `--since` windows, in seconds.
fn window_seconds(kind: &str) -> Option<i64> {
    match kind {
        "day" => Some(86_400),
        "week" => Some(7 * 86_400),
        "month" => Some(30 * 86_400),
        _ => None,
    }
}

/// The `since` window ending now (`all` or unknown means no filter).
#[must_use]
pub fn window(since: &str) -> TimeWindow {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs().cast_signed());
    TimeWindow {
        since: window_seconds(since).map(|s| now - s),
    }
}

/// Load the report for `since` (`day|week|month|all`) and `scope`, or `None`
/// when nothing was recorded yet.
///
/// # Errors
///
/// Returns [`StatsError`] if the store cannot be read.
pub fn load(
    store: &dyn UsageStore,
    since: &str,
    scope: StatsScope<'_>,
) -> Result<Option<StatsReport>, StatsError> {
    let filter = StatsFilter {
        window: window(since),
        scope,
    };
    let Some(summary) = store.summary(filter)? else {
        return Ok(None);
    };
    Ok(Some(StatsReport {
        summary,
        by_provider: store.by_provider(filter)?.unwrap_or_default(),
        by_account: store.by_account(filter)?.unwrap_or_default(),
        recent: store.recent(filter, 10)?.unwrap_or_default(),
    }))
}

/// The JSON report for `since` and `scope`, or `None` when nothing was
/// recorded yet.
///
/// # Errors
///
/// Returns [`StatsError`] if the store cannot be read.
pub fn json(
    store: &dyn UsageStore,
    since: &str,
    scope: StatsScope<'_>,
) -> Result<Option<Value>, StatsError> {
    Ok(load(store, since, scope)?.map(|report| render_json(scope, &report)))
}

/// Render a report as JSON.
#[must_use]
pub fn render_json(scope: StatsScope<'_>, report: &StatsReport) -> Value {
    let summary = &report.summary;
    let summary_json = json!({
        "requests": summary.requests,
        "input_tokens": summary.input_tokens,
        "output_tokens": summary.output_tokens,
        "total_latency_ms": summary.latency_ms,
        "errors": summary.errors,
        "energy_kwh": summary.energy_kwh_um as f64 / 1_000_000.0,
        "cost_usd": summary.cost_usd_um as f64 / 1_000_000.0,
        "cache": cache_stats_json(summary.cache, summary.requests),
    });
    let providers_json: Vec<Value> = report
        .by_provider
        .iter()
        .map(|p| {
            json!({
                "provider": p.provider,
                "requests": p.requests,
                "input_tokens": p.input_tokens,
                "output_tokens": p.output_tokens,
                "total_latency_ms": p.latency_ms,
                "energy_kwh": p.energy_kwh_um as f64 / 1_000_000.0,
                "cost_usd": p.cost_usd_um as f64 / 1_000_000.0,
                "cache": cache_stats_json(p.cache, p.requests),
            })
        })
        .collect();
    let accounts_json: Vec<Value> = report
        .by_account
        .iter()
        .map(|a| {
            json!({
                "provider": a.provider,
                "alias": a.alias,
                "requests": a.requests,
                "input_tokens": a.input_tokens,
                "output_tokens": a.output_tokens,
                "total_latency_ms": a.latency_ms,
                "energy_kwh": a.energy_kwh_um as f64 / 1_000_000.0,
                "cost_usd": a.cost_usd_um as f64 / 1_000_000.0,
                "cache": cache_stats_json(a.cache, a.requests),
            })
        })
        .collect();
    let recent_json: Vec<Value> = report
        .recent
        .iter()
        .map(|r| {
            let mut v = json!({
                "ts": r.ts,
                "endpoint": r.endpoint,
                "provider": r.provider,
                "alias": r.alias,
                "model": r.model,
                "input_tokens": r.input_tokens,
                "output_tokens": r.output_tokens,
                "streamed": r.streamed,
                "status": r.status,
                "latency_ms": r.latency_ms,
                "error": r.error,
            });
            if let Some(e) = r.energy_kwh_um {
                v["energy_kwh_um"] = json!(e);
            }
            if let Some(c) = r.cost_usd_um {
                v["cost_usd_um"] = json!(c);
            }
            v
        })
        .collect();
    json!({
        "scope": scope_json(scope),
        "summary": summary_json,
        "providers": providers_json,
        "accounts": accounts_json,
        "recent": recent_json,
    })
}

fn scope_json(scope: StatsScope<'_>) -> Value {
    match scope {
        StatsScope::All => json!({ "kind": "all" }),
        StatsScope::Session(session_id) => {
            json!({ "kind": "session", "session_id": session_id })
        }
    }
}

fn cache_stats_json(cache: CacheStats, requests: u64) -> Value {
    let rate_percent = (cache.reported_requests > 0)
        .then(|| cache.hit_requests as f64 / cache.reported_requests as f64 * 100.0);
    let coverage_percent =
        (requests > 0).then(|| cache.reported_requests as f64 / requests as f64 * 100.0);
    json!({
        "hit_requests": cache.hit_requests,
        "reported_requests": cache.reported_requests,
        "rate_percent": rate_percent,
        "coverage_percent": coverage_percent,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_stats_json_reports_hit_rate_and_telemetry_coverage() {
        let reported = cache_stats_json(
            CacheStats {
                hit_requests: 3,
                reported_requests: 4,
            },
            8,
        );
        assert_eq!(reported["hit_requests"], 3);
        assert_eq!(reported["reported_requests"], 4);
        assert_eq!(reported["rate_percent"], 75.0);
        assert_eq!(reported["coverage_percent"], 50.0);

        let unavailable = cache_stats_json(CacheStats::default(), 2);
        assert!(unavailable["rate_percent"].is_null());
        assert_eq!(unavailable["coverage_percent"], 0.0);

        let no_requests = cache_stats_json(CacheStats::default(), 0);
        assert!(no_requests["rate_percent"].is_null());
        assert!(no_requests["coverage_percent"].is_null());
    }

    #[test]
    fn unknown_windows_cover_all_time() {
        assert!(window("all").since.is_none());
        assert!(window("day").since.is_some());
    }
}
