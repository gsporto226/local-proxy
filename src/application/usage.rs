//! Best-effort usage recording: a stats problem is logged, never surfaced, so
//! it can never break a proxied request.

use std::sync::Arc;
use std::time::Instant;

use serde_json::{json, Value};

use crate::domain::stats::StatLine;
use crate::domain::translate::{self, EnergyCost, TokenUsage};
use crate::ports::{EventBus, Ports, UsageStore};

/// Records proxied requests to the usage store and announces them as live
/// events.
#[derive(Clone)]
pub struct UsageRecorder {
    store: Arc<dyn UsageStore>,
    events: Arc<dyn EventBus>,
}

impl UsageRecorder {
    /// A recorder writing through `ports`.
    #[must_use]
    pub fn new(ports: &Ports) -> Self {
        Self {
            store: ports.usage.clone(),
            events: ports.events.clone(),
        }
    }

    /// Record one request; latency runs from `started` until now.
    #[allow(clippy::needless_pass_by_value, clippy::cast_possible_truncation)]
    pub fn record(&self, started: Instant, stat: StatLine) {
        let latency_ms = started.elapsed().as_millis() as u64;
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs().cast_signed());
        tracing::info!(
            target: crate::LOG_TARGET,
            provider = %stat.provider,
            alias = %stat.alias,
            model = %stat.model,
            session = %stat.session_id,
            status = stat.status,
            input = stat.input_tokens,
            cache_read = ?stat.cache_read_tokens,
            cache_write = stat.cache_write_tokens,
            latency_ms,
            "request usage"
        );
        match self.store.insert(&stat, ts, latency_ms) {
            Ok(()) => self.events.publish(
                "request",
                json!({
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
                target: crate::LOG_TARGET,
                error = %e,
                "falhou ao registrar stats (dados não persistidos)"
            ),
        }
    }

    /// Record a completed non-streaming response, reading token usage, energy
    /// and cost from the (already consumed) upstream body when present.
    pub fn record_response(&self, request: &RequestMeta, status: u16, body: Option<&Value>) {
        let tokens = body
            .and_then(|body| body.get("usage"))
            .map(translate::parse_usage)
            .unwrap_or_default();
        self.record_parsed(request, status, body, tokens);
    }

    /// Like [`Self::record_response`] with the token usage already known.
    pub fn record_parsed(
        &self,
        request: &RequestMeta,
        status: u16,
        body: Option<&Value>,
        tokens: TokenUsage,
    ) {
        let (energy, cost) = body.map_or((None, tokens.as_cost()), |body| {
            (
                translate::energy_from_value(body),
                // A `NeuralWatt`-style top-level `cost` object wins; otherwise fall
                // back to the `usage.cost` reported by OpenAI/OpenRouter-style
                // upstreams (e.g. OpenRouter, Groq) so that cost is still recorded.
                translate::cost_from_value(body).or_else(|| tokens.as_cost()),
            )
        });
        self.record(request.started, request.line(status, tokens, energy, cost));
    }

    /// Remember the reasoning effort a session last asked for.
    pub fn record_effort(&self, session_id: &str, effort: &str) {
        if let Err(e) = self.store.record_effort(session_id, effort) {
            tracing::warn!(target: crate::LOG_TARGET, error = %e, "falhou ao registrar effort");
        }
    }

    /// Remember subscription quota percents `(5h, weekly)` reported by an
    /// account's response, and announce them.
    pub fn record_rate_limits(&self, provider: &str, alias: &str, (h5, week): (f64, f64)) {
        if let Err(e) = self.store.record_rate_limits(h5, week) {
            tracing::warn!(target: crate::LOG_TARGET, error = %e, "falhou ao registrar rate limits");
        }
        self.events
            .publish("rate_limits", json!({ "h5": h5, "week": week }));
        if let Err(e) = self
            .store
            .record_account_rate_limits(provider, alias, h5, week)
        {
            tracing::warn!(target: crate::LOG_TARGET, error = %e, "failed to record account quota headers");
        }
    }
}

/// What every stats row of one proxied request shares.
#[derive(Debug, Clone)]
pub struct RequestMeta {
    /// The proxied endpoint, e.g. `/v1/messages`.
    pub endpoint: &'static str,
    /// The upstream provider.
    pub provider: String,
    /// The account alias routed through.
    pub alias: String,
    /// The upstream model.
    pub model: String,
    /// Whether the client asked for a streamed response.
    pub streamed: bool,
    /// When the request arrived.
    pub started: Instant,
    /// The client session id, or `""`.
    pub session_id: String,
}

impl RequestMeta {
    fn line(
        &self,
        status: u16,
        usage: TokenUsage,
        energy: Option<EnergyCost>,
        cost: Option<EnergyCost>,
    ) -> StatLine {
        StatLine {
            endpoint: self.endpoint,
            provider: self.provider.clone(),
            alias: self.alias.clone(),
            model: self.model.clone(),
            input_tokens: usage.input,
            output_tokens: usage.output,
            streamed: self.streamed,
            status,
            error: status >= 400,
            energy,
            cost,
            session_id: self.session_id.clone(),
            cache_hit: usage.cache_hit(),
            cache_read_tokens: usage.cache_read,
            cache_write_tokens: usage.cache_write,
        }
    }
}

/// Deferred best-effort stats capture for a streaming request.
///
/// A streaming request cannot be recorded up-front: its token usage is only
/// known once the SSE stream has been fully read. This carries the request
/// metadata and records the row (with the cumulative usage) when the stream
/// completes.
pub struct StreamCapture {
    recorder: UsageRecorder,
    request: RequestMeta,
    status: u16,
}

impl StreamCapture {
    /// A capture handle for a proxied streaming request.
    #[must_use]
    pub const fn new(recorder: UsageRecorder, request: RequestMeta, status: u16) -> Self {
        Self {
            recorder,
            request,
            status,
        }
    }

    /// Write the stats row with the final cumulative `usage`, plus any energy
    /// and cost metadata observed in the stream (`NeuralWatt`-style). When no
    /// `NeuralWatt` cost was observed, falls back to the `usage.cost` reported
    /// by OpenAI/OpenRouter-style upstreams.
    pub fn record(&self, usage: TokenUsage, energy: Option<EnergyCost>, cost: Option<EnergyCost>) {
        self.recorder.record(
            self.request.started,
            self.request
                .line(self.status, usage, energy, cost.or_else(|| usage.as_cost())),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::stats::{StatsFilter, StatsScope, TimeWindow};

    fn meta(endpoint: &'static str, provider: &str, model: &str) -> RequestMeta {
        RequestMeta {
            endpoint,
            provider: provider.to_string(),
            alias: "work".to_string(),
            model: model.to_string(),
            streamed: false,
            started: Instant::now(),
            session_id: "sess-1".to_string(),
        }
    }

    #[test]
    fn capture_keeps_missing_zero_and_positive_cache_reports_distinct() {
        let _guard = crate::TEST_STATE_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("LOCAL_PROXY_CONFIG_DIR", dir.path());
        let ports = crate::bootstrap::ports();
        let recorder = UsageRecorder::new(&ports);
        for usage in [
            json!({"input_tokens": 10}),
            json!({"input_tokens": 10, "cache_read_input_tokens": 0}),
            json!({"input_tokens": 10, "cache_read_input_tokens": 4}),
        ] {
            recorder.record_response(
                &meta("/v1/messages", "anthropic", "claude-test"),
                200,
                Some(&json!({"usage": usage})),
            );
        }
        let response_usage = json!({"usage": translate::responses_usage(&TokenUsage::default())});
        recorder.record_parsed(
            &meta("/v1/responses", "openai", "gpt-test"),
            200,
            Some(&response_usage),
            TokenUsage::default(),
        );
        let summary = ports
            .usage
            .summary(StatsFilter {
                window: TimeWindow { since: None },
                scope: StatsScope::Session("sess-1"),
            })
            .unwrap()
            .unwrap();
        assert_eq!(summary.requests, 4);
        assert_eq!(summary.cache.hit_requests, 1);
        assert_eq!(summary.cache.reported_requests, 2);
        std::env::remove_var("LOCAL_PROXY_CONFIG_DIR");
    }

    #[test]
    fn stream_capture_persists_reported_cache_hits_and_misses() {
        let _guard = crate::TEST_STATE_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("LOCAL_PROXY_CONFIG_DIR", dir.path());
        let ports = crate::bootstrap::ports();
        for cache_read in [None, Some(0), Some(5)] {
            let mut request = meta("/v1/messages", "anthropic", "claude-test");
            request.session_id = "sess-stream".to_string();
            request.streamed = true;
            StreamCapture::new(UsageRecorder::new(&ports), request, 200).record(
                TokenUsage {
                    cache_read,
                    ..TokenUsage::default()
                },
                None,
                None,
            );
        }
        let summary = ports
            .usage
            .summary(StatsFilter {
                window: TimeWindow { since: None },
                scope: StatsScope::Session("sess-stream"),
            })
            .unwrap()
            .unwrap();
        assert_eq!(summary.cache.hit_requests, 1);
        assert_eq!(summary.cache.reported_requests, 2);
        std::env::remove_var("LOCAL_PROXY_CONFIG_DIR");
    }
}
