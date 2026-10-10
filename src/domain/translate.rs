use serde_json::{json, Value};

/// Errors that occur while translating requests/responses between provider
/// wire formats.
#[derive(Debug, thiserror::Error)]
pub enum TranslateError {
    /// The request body is not a JSON object.
    #[error("request body is not a JSON object")]
    NotObject,
    /// A required field is missing from the payload.
    #[error("missing required field: {0}")]
    MissingField(&'static str),
    /// A field carries an invalid value.
    #[error("invalid value for {field}: {detail}")]
    Invalid {
        /// The offending field name.
        field: &'static str,
        /// A human-readable description of the invalid value.
        detail: String,
    },
}

fn remove(value: &mut Value, key: &str) {
    if let Some(o) = value.as_object_mut() {
        o.remove(key);
    }
}

fn insert(value: &mut Value, key: &str, val: Value) {
    if let Some(o) = value.as_object_mut() {
        o.insert(key.to_string(), val);
    }
}

// ---------------------------------------------------------------------------
// token usage
// ---------------------------------------------------------------------------

/// Aggregate token counts for a single request/response exchange.
///
/// `cost_usd` is present only when the *upstream reports it* (`cost`,
/// `prompt_cost`+`completion_cost`, …). It is never synthesized from a pricing
/// table; for providers that report nothing it stays `None`.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct TokenUsage {
    /// Number of input (prompt) tokens.
    pub input: u64,
    /// Number of output (completion) tokens.
    pub output: u64,
    /// Number of reasoning tokens, when reported by the upstream.
    pub reasoning: u64,
    /// Input tokens served from the prompt cache, or `None` when unreported.
    pub cache_read: Option<u64>,
    /// Input tokens written to the prompt cache.
    pub cache_write: u64,
    /// The request's monetary cost in USD, when reported by the upstream.
    pub cost_usd: Option<f64>,
}

/// Energy and cost metadata reported by energy-priced providers (`NeuralWatt`).
///
/// Fields are optional because not every request produces them (e.g. when the
/// GPU doesn't expose NVML metrics). Floats from upstream are kept as `f64`.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct EnergyCost {
    /// Total energy in joules, if reported.
    pub energy_joules: Option<f64>,
    /// Total energy in kilowatt-hours, if reported.
    pub energy_kwh: Option<f64>,
    /// Average power draw in watts during the request, if reported.
    pub avg_power_watts: Option<f64>,
    /// The request's monetary cost in USD.
    pub request_cost_usd: Option<f64>,
    /// Cache savings in USD.
    pub cache_savings_usd: Option<f64>,
}

impl TokenUsage {
    /// Whether the upstream reported at least one cached input token.
    #[must_use]
    pub fn cache_hit(&self) -> Option<bool> {
        self.cache_read.map(|tokens| tokens > 0)
    }

    /// The request cost as an [`EnergyCost`], when the upstream reported it via
    /// `usage.cost` / `prompt_cost`+`completion_cost`. This lets the recording
    /// layer persist an OpenAI/OpenRouter-reported cost (which has no energy
    /// payload) through the same `cost_usd_um` column. Returns `None` when no
    /// cost was reported.
    #[must_use]
    pub fn as_cost(&self) -> Option<EnergyCost> {
        self.cost_usd.map(|usd| EnergyCost {
            request_cost_usd: Some(usd),
            ..EnergyCost::default()
        })
    }
}

/// Parse a NeuralWatt-style top-level `energy` object.
#[must_use]
pub fn energy_from_value(value: &Value) -> Option<EnergyCost> {
    let obj = value.get("energy")?.as_object()?;
    let f = |k: &str| obj.get(k).and_then(Value::as_f64);
    Some(EnergyCost {
        energy_joules: f("energy_joules"),
        energy_kwh: f("energy_kwh"),
        avg_power_watts: f("avg_power_watts"),
        request_cost_usd: None,
        cache_savings_usd: None,
    })
}

/// Parse a NeuralWatt-style top-level `cost` object.
#[must_use]
pub fn cost_from_value(value: &Value) -> Option<EnergyCost> {
    let obj = value.get("cost")?.as_object()?;
    let f = |k: &str| obj.get(k).and_then(Value::as_f64);
    Some(EnergyCost {
        energy_joules: None,
        energy_kwh: None,
        avg_power_watts: None,
        request_cost_usd: f("request_cost_usd"),
        cache_savings_usd: f("cache_savings_usd"),
    })
}

/// Parse a `NeuralWatt` energy comment payload (a bare `energy` object, not
/// wrapped in a top-level `energy` key), e.g. from an `SSE` comment.
#[must_use]
pub fn energy_from_payload(value: &Value) -> Option<EnergyCost> {
    let obj = value.as_object()?;
    let f = |k: &str| obj.get(k).and_then(Value::as_f64);
    Some(EnergyCost {
        energy_joules: f("energy_joules"),
        energy_kwh: f("energy_kwh"),
        avg_power_watts: f("avg_power_watts"),
        request_cost_usd: None,
        cache_savings_usd: None,
    })
}

/// Parse a `NeuralWatt` cost comment payload (a bare `cost` object), e.g. from
/// an `SSE` comment.
#[must_use]
pub fn cost_from_payload(value: &Value) -> Option<EnergyCost> {
    let obj = value.as_object()?;
    let f = |k: &str| obj.get(k).and_then(Value::as_f64);
    Some(EnergyCost {
        energy_joules: None,
        energy_kwh: None,
        avg_power_watts: None,
        request_cost_usd: f("request_cost_usd"),
        cache_savings_usd: f("cache_savings_usd"),
    })
}

/// Tolerantly parse usage from any upstream shape (Anthropic, `OpenAI`, Responses).
#[must_use]
pub fn parse_usage(value: &Value) -> TokenUsage {
    let mut usage = TokenUsage::default();
    if let Some(obj) = value.as_object() {
        usage.input = num(obj.get("input_tokens"))
            .or_else(|| num(obj.get("prompt_tokens")))
            .unwrap_or(0);
        usage.output = num(obj.get("output_tokens"))
            .or_else(|| num(obj.get("completion_tokens")))
            .unwrap_or(0);
        if let Some(details) = obj.get("completion_tokens_details") {
            usage.reasoning = num(details.get("reasoning_tokens")).unwrap_or(0);
        }
        if let Some(details) = obj.get("output_tokens_details") {
            usage.reasoning = num(details.get("reasoning_tokens")).unwrap_or(usage.reasoning);
        }
        // IR `input` is the whole prompt. Anthropic reports cached tokens
        // apart from `input_tokens`; OpenAI and Responses include them.
        let anthropic_read = num(obj.get("cache_read_input_tokens"));
        let anthropic_write = num(obj.get("cache_creation_input_tokens"));
        if anthropic_read.is_some() || anthropic_write.is_some() {
            usage.cache_read = anthropic_read;
            usage.cache_write = anthropic_write.unwrap_or(0);
            usage.input += usage.cache_read.unwrap_or(0) + usage.cache_write;
        } else {
            usage.cache_read = ["prompt_tokens_details", "input_tokens_details"]
                .iter()
                .find_map(|k| num(obj.get(*k).and_then(|d| d.get("cached_tokens"))));
        }
        usage.cost_usd = cost_from_usage(obj);
    }
    usage
}

/// Extract a reported monetary cost from a usage object, if one is present.
///
/// Cost field names differ by provider; we scan a small allow-list and total
/// when both parts exist. Returns `None` when no recognized field carries a
/// cost — the proxy never estimates cost from a token-price table.
fn cost_from_usage(obj: &serde_json::Map<String, Value>) -> Option<f64> {
    // OpenAI-compatible: a single `cost` field (e.g. OpenRouter, Groq).
    let single = obj.get("cost").and_then(Value::as_f64);
    // OpenRouter-style split prompt/completion cost.
    let both = match (
        obj.get("prompt_cost").and_then(Value::as_f64),
        obj.get("completion_cost").and_then(Value::as_f64),
    ) {
        (Some(p), Some(c)) => Some(p + c),
        _ => None,
    };
    single.or(both)
}

/// Merge `part` into `acc`, keeping the highest value per field.
///
/// Used to accumulate cumulative usage across SSE frames: some events carry
/// only a partial usage (e.g. an Anthropic `message_delta` reports just
/// `output_tokens`), so later frames may revisit earlier fields at their final
/// (larger) value.
pub fn merge_usage(acc: &mut TokenUsage, part: TokenUsage) {
    acc.input = acc.input.max(part.input);
    acc.output = acc.output.max(part.output);
    acc.reasoning = acc.reasoning.max(part.reasoning);
    acc.cache_read = acc.cache_read.max(part.cache_read);
    acc.cache_write = acc.cache_write.max(part.cache_write);
    if part.cost_usd.is_some() {
        acc.cost_usd = part.cost_usd;
    }
}

/// Extract cumulative token usage from a generic SSE frame payload.
///
/// Covers the shapes that carry usage mid-stream: an `OpenAI` chat-completions
/// chunk (`usage`), an Anthropic `message_start`/`message_delta`
/// (`message.usage` / `usage`) and a Responses `response.completed`
/// (`response.usage`). Returns a zeroed [`TokenUsage`] when the frame carries
/// none.
#[must_use]
pub fn usage_from_frame(value: &Value) -> TokenUsage {
    if let Some(u) = value.get("usage") {
        return parse_usage(u);
    }
    if let Some(msg) = value.get("message") {
        if let Some(u) = msg.get("usage") {
            return parse_usage(u);
        }
    }
    if let Some(resp) = value.get("response") {
        if let Some(u) = resp.get("usage") {
            return parse_usage(u);
        }
    }
    TokenUsage::default()
}

fn num(v: Option<&Value>) -> Option<u64> {
    v.and_then(Value::as_u64)
}

/// Serialize [`TokenUsage`] into an Anthropic-style usage object.
#[must_use]
pub fn anthropic_usage(u: &TokenUsage) -> Value {
    json!({
        "input_tokens": u.input.saturating_sub(
            u.cache_read.unwrap_or(0).saturating_add(u.cache_write)
        ),
        "output_tokens": u.output,
        "cache_read_input_tokens": u.cache_read.unwrap_or(0),
        "cache_creation_input_tokens": u.cache_write
    })
}

/// Serialize [`TokenUsage`] into an `OpenAI`-style usage object.
#[must_use]
pub fn openai_usage(u: &TokenUsage) -> Value {
    let mut details = json!({});
    if u.reasoning > 0 {
        insert(&mut details, "reasoning_tokens", json!(u.reasoning));
    }
    json!({
        "prompt_tokens": u.input,
        "completion_tokens": u.output,
        "total_tokens": u.input + u.output,
        "prompt_tokens_details": {"cached_tokens": u.cache_read.unwrap_or(0)},
        "completion_tokens_details": details
    })
}

/// Serialize [`TokenUsage`] into a Responses-API-style usage object.
#[must_use]
pub fn responses_usage(u: &TokenUsage) -> Value {
    let mut out_details = json!({});
    if u.reasoning > 0 {
        insert(&mut out_details, "reasoning_tokens", json!(u.reasoning));
    }
    json!({
        "input_tokens": u.input,
        "output_tokens": u.output,
        "total_tokens": u.input + u.output,
        "input_tokens_details": {"cached_tokens": u.cache_read.unwrap_or(0)},
        "output_tokens_details": out_details
    })
}

// ---------------------------------------------------------------------------
// anthropic -> anthropic normalization (passthrough hygiene)
// ---------------------------------------------------------------------------

/// Strip the fields some Anthropic-format upstreams reject.
///
/// `cache_control` markers are kept: stripping them turns prompt caching off.
#[must_use]
pub fn normalize_anthropic_request(body: &Value) -> Value {
    let mut out = body.clone();
    for key in [
        "thinking",
        "reasoning",
        "reasoning_effort",
        "effort",
        "level",
        "depth",
        "output_config",
        // Claude Code sends this on every request; upstreams that do not
        // implement context editing reject the whole body with
        // "400 context_management: Extra inputs are not permitted".
        "context_management",
    ] {
        remove(&mut out, key);
    }
    out
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_usage_tolerant_shapes() {
        let a = json!({"input_tokens": 1, "output_tokens": 2});
        assert_eq!(
            parse_usage(&a),
            TokenUsage {
                input: 1,
                output: 2,
                reasoning: 0,
                cost_usd: None,
                ..TokenUsage::default()
            }
        );

        let o = json!({"prompt_tokens": 3, "completion_tokens": 4, "total_tokens": 7, "completion_tokens_details": {"reasoning_tokens": 1}});
        let u = parse_usage(&o);
        assert_eq!(u.input, 3);
        assert_eq!(u.output, 4);
        assert_eq!(u.reasoning, 1);

        let r = json!({"input_tokens": 9, "output_tokens": 8, "total_tokens": 17, "output_tokens_details": {"reasoning_tokens": 5}});
        let u = parse_usage(&r);
        assert_eq!(u.input, 9);
        assert_eq!(u.output, 8);
        assert_eq!(u.reasoning, 5);

        assert_eq!(parse_usage(&json!({})), TokenUsage::default());
    }

    #[test]
    fn parse_usage_captures_reported_cost_when_present() {
        // OpenAI/OpenRouter single `cost` field.
        let c = 0.000_123;
        let single = json!({"prompt_tokens": 1, "completion_tokens": 1, "cost": c});
        assert_eq!(parse_usage(&single).cost_usd, Some(c));

        // OpenRouter split prompt/completion cost totals both.
        let split = json!({"prompt_tokens": 1, "completion_tokens": 1, "prompt_cost": 0.01, "completion_cost": 0.02});
        assert_eq!(parse_usage(&split).cost_usd, Some(0.03));

        // Anthropic reports no cost -> None (never synthesized).
        let anthro = json!({"input_tokens": 1, "output_tokens": 1});
        assert_eq!(parse_usage(&anthro).cost_usd, None);
    }

    #[test]
    fn cache_read_telemetry_distinguishes_missing_zero_and_positive() {
        assert_eq!(parse_usage(&json!({"input_tokens": 10})).cache_read, None);
        assert_eq!(
            parse_usage(&json!({"input_tokens": 10, "cache_read_input_tokens": 0})).cache_read,
            Some(0)
        );
        assert_eq!(
            parse_usage(&json!({
                "prompt_tokens": 10,
                "prompt_tokens_details": {"cached_tokens": 4}
            }))
            .cache_read,
            Some(4)
        );
        assert_eq!(
            parse_usage(&json!({
                "input_tokens": 10,
                "input_tokens_details": {"cached_tokens": 0}
            }))
            .cache_read,
            Some(0)
        );
        assert_eq!(
            parse_usage(&json!({
                "input_tokens": 10,
                "cache_creation_input_tokens": 5
            }))
            .cache_read,
            None
        );
    }

    #[test]
    fn merge_usage_keeps_reported_zero_when_later_frames_omit_cache_data() {
        let mut usage = TokenUsage {
            cache_read: Some(0),
            ..TokenUsage::default()
        };
        merge_usage(&mut usage, TokenUsage::default());
        assert_eq!(usage.cache_read, Some(0));
        merge_usage(
            &mut usage,
            TokenUsage {
                cache_read: Some(7),
                ..TokenUsage::default()
            },
        );
        assert_eq!(usage.cache_read, Some(7));
    }

    #[test]
    fn merge_usage_propagates_first_reported_cost() {
        let mut acc = TokenUsage::default();
        merge_usage(
            &mut acc,
            TokenUsage {
                input: 5,
                output: 3,
                reasoning: 0,
                cost_usd: Some(0.007),
                ..TokenUsage::default()
            },
        );
        assert_eq!(acc.cost_usd, Some(0.007));
        // a later frame without cost does not clear it
        merge_usage(
            &mut acc,
            TokenUsage {
                input: 6,
                output: 4,
                reasoning: 0,
                cost_usd: None,
                ..TokenUsage::default()
            },
        );
        assert_eq!(acc.cost_usd, Some(0.007));
    }

    #[test]
    fn as_cost_roundtrips_reported_usage_cost() {
        // A reported usage cost becomes an EnergyCost carrying request_cost_usd,
        // so the recording layer can persist it through cost_usd_um.
        let used = TokenUsage {
            cost_usd: Some(0.0042),
            ..TokenUsage::default()
        };
        let cost = used.as_cost().expect("cost present");
        assert_eq!(cost.request_cost_usd, Some(0.0042));

        // No reported cost -> None (nothing to record).
        assert_eq!(TokenUsage::default().as_cost(), None);
    }

    #[test]
    fn normalize_anthropic_request_strips_knobs_but_keeps_cache_control() {
        let body = json!({
            "model": "m",
            "thinking": {"type": "enabled"},
            "effort": "high",
            "context_management": {"edits": []},
            "messages": [{"role": "user", "content": [{"type": "text", "text": "x", "cache_control": {"type": "ephemeral"}}]}]
        });
        let out = normalize_anthropic_request(&body);
        assert!(out.get("thinking").is_none());
        assert!(out.get("effort").is_none());
        // a backend without context editing 400s the whole body on this
        assert!(out.get("context_management").is_none());
        let text = serde_json::to_string(&out).unwrap();
        // prompt caching stays on for the passthrough
        assert!(text.contains("cache_control"));
        assert_eq!(out["messages"][0]["content"][0]["text"], "x");
    }

    #[test]
    fn merge_usage_keeps_max_per_field() {
        let mut acc = TokenUsage::default();
        // first a partial Anthropic message_delta (output only)
        merge_usage(
            &mut acc,
            TokenUsage {
                input: 0,
                output: 2,
                reasoning: 0,
                cost_usd: None,
                ..TokenUsage::default()
            },
        );
        // then a full picture with larger input
        merge_usage(
            &mut acc,
            TokenUsage {
                input: 5,
                output: 3,
                reasoning: 1,
                cost_usd: None,
                ..TokenUsage::default()
            },
        );
        assert_eq!(acc.input, 5);
        assert_eq!(acc.output, 3);
        assert_eq!(acc.reasoning, 1);
        // a smaller value never shrinks an accumulated field
        merge_usage(
            &mut acc,
            TokenUsage {
                input: 1,
                output: 1,
                reasoning: 0,
                cost_usd: None,
                ..TokenUsage::default()
            },
        );
        assert_eq!(acc.input, 5);
        assert_eq!(acc.output, 3);
    }

    #[test]
    fn usage_from_frame_covers_event_shapes() {
        // OpenAI chat chunk
        let oai = json!({"usage": {"prompt_tokens": 3, "completion_tokens": 2}});
        let u = usage_from_frame(&oai);
        assert_eq!((u.input, u.output), (3, 2));

        // Anthropic message_start (message.usage)
        let astart = json!({"type": "message_start", "message": {"usage": {"input_tokens": 4, "output_tokens": 0}}});
        let u = usage_from_frame(&astart);
        assert_eq!((u.input, u.output), (4, 0));

        // Anthropic message_delta (top-level usage)
        let adelta = json!({"type": "message_delta", "usage": {"output_tokens": 7}});
        let u = usage_from_frame(&adelta);
        assert_eq!((u.input, u.output), (0, 7));

        // Responses completed (response.usage)
        let resp = json!({"type": "response.completed", "response": {"usage": {"input_tokens": 8, "output_tokens": 9}}});
        let u = usage_from_frame(&resp);
        assert_eq!((u.input, u.output), (8, 9));

        // A frame with no usage yields default
        assert_eq!(
            usage_from_frame(&json!({"type": "message_stop"})),
            TokenUsage::default()
        );
    }

    #[test]
    fn parses_top_level_energy_and_cost() {
        // non-streaming: top-level `energy` and `cost` fields
        let body = json!({
            "choices": [],
            "energy": {"energy_kwh": 1.5e-5, "energy_joules": 54.0, "avg_power_watts": 55.3},
            "cost": {"request_cost_usd": 1.04e-5, "cache_savings_usd": 0.0}
        });
        let e = energy_from_value(&body).unwrap();
        assert_eq!(e.energy_kwh, Some(1.5e-5));
        assert_eq!(e.energy_joules, Some(54.0));
        assert_eq!(e.avg_power_watts, Some(55.3));
        let c = cost_from_value(&body).unwrap();
        assert_eq!(c.request_cost_usd, Some(1.04e-5));
        assert_eq!(c.cache_savings_usd, Some(0.0));
    }

    #[test]
    fn parses_energy_cost_comment_payloads() {
        // streaming: bare objects from SSE comments
        let e = energy_from_payload(
            &json!({"energy_joules": 4.99, "energy_kwh": 1.385e-6, "avg_power_watts": 55.3}),
        )
        .unwrap();
        assert_eq!(e.energy_kwh, Some(1.385e-6));
        assert_eq!(e.energy_joules, Some(4.99));
        let c = cost_from_payload(&json!({"request_cost_usd": 1.04e-5, "cache_savings_usd": 0.0}))
            .unwrap();
        assert_eq!(c.request_cost_usd, Some(1.04e-5));

        // no energy/cost key -> None
        assert!(energy_from_value(&json!({"choices": []})).is_none());
        assert!(cost_from_value(&json!({})).is_none());
    }
}
