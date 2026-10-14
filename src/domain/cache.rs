//! Prompt-cache policy and observations. Diagnostics describe evidence, not
//! provider-side causes that the proxy cannot observe.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::domain::config::{PromptCachePolicy, ProviderFormat};
use crate::domain::translate::TokenUsage;

/// Evidence relevant to a request's cache reuse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheDiagnostic {
    /// An Anthropic request has neither explicit nor automatic cache control.
    MissingCacheControl,
    /// A Responses request carries no explicit cache routing key.
    MissingSessionKey,
    /// Tools or system/developer instructions changed within the session.
    StablePrefixChanged,
    /// The session's upstream provider or model changed.
    ModelChanged,
    /// The session's selected account alias changed.
    AccountChanged,
    /// The session's prompt cache routing key changed.
    CacheKeyChanged,
    /// The upstream did not report cached-read tokens.
    TelemetryUnavailable,
    /// Zero cached-read tokens were reported without an observable explanation.
    CacheMissUnknown,
    /// The upstream reported writing new tokens to cache.
    CacheWarming,
    /// An unsuccessful upstream response cannot validate caching.
    UpstreamError,
}

/// Whether any cache-control marker is present, including automatic caching.
#[must_use]
pub fn has_cache_control(body: &Value) -> bool {
    match body {
        Value::Object(fields) => {
            fields.contains_key("cache_control") || fields.values().any(has_cache_control)
        }
        Value::Array(items) => items.iter().any(has_cache_control),
        _ => false,
    }
}

/// Add at most three Anthropic breakpoints without replacing client markers.
pub fn apply_policy(format: ProviderFormat, policy: PromptCachePolicy, body: &mut Value) {
    if format != ProviderFormat::Anthropic || has_cache_control(body) {
        return;
    }
    let ttl = match policy {
        PromptCachePolicy::Preserve => return,
        PromptCachePolicy::Anthropic5m => "5m",
        PromptCachePolicy::Anthropic1h => "1h",
    };
    let marker = json!({"type": "ephemeral", "ttl": ttl});
    if let Some(tool) = body
        .get_mut("tools")
        .and_then(Value::as_array_mut)
        .and_then(|v| v.last_mut())
        .filter(|v| v.is_object())
    {
        tool["cache_control"] = marker.clone();
    }
    if let Some(system) = body.get_mut("system") {
        if let Some(text) = system.as_str().filter(|text| !text.is_empty()) {
            *system = json!([{"type": "text", "text": text, "cache_control": marker}]);
        } else if let Some(block) = system
            .as_array_mut()
            .and_then(|blocks| blocks.iter_mut().rev().find(|b| b["type"] == "text"))
        {
            block["cache_control"] = marker.clone();
        }
    }
    if let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) {
        for message in messages.iter_mut().rev().filter(|m| m["role"] == "user") {
            let Some(content) = message.get_mut("content") else {
                continue;
            };
            if let Some(text) = content.as_str().filter(|text| !text.is_empty()) {
                *content = json!([{"type": "text", "text": text, "cache_control": marker}]);
                break;
            }
            if let Some(block) = content
                .as_array_mut()
                .and_then(|blocks| blocks.iter_mut().rev().find(|b| b["type"] == "text"))
            {
                block["cache_control"] = marker;
                break;
            }
        }
    }
}

/// A content-free observation kept in memory for session change detection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheObservation {
    provider: String,
    model: String,
    account: String,
    prefix_digest: [u8; 32],
    key_digest: Option<[u8; 32]>,
}

impl CacheObservation {
    /// Hash static instructions and tools without retaining the prompt or key.
    #[must_use]
    pub fn new(provider: &str, model: &str, account: &str, body: &Value) -> Self {
        let instructions: Vec<&Value> = ["messages", "input"]
            .iter()
            .filter_map(|key| body.get(*key).and_then(Value::as_array))
            .flatten()
            .filter(|m| matches!(m["role"].as_str(), Some("system" | "developer")))
            .collect();
        let prefix = json!({"system": body.get("system"), "instructions": body.get("instructions"), "tools": body.get("tools"), "developer_messages": instructions});
        Self {
            provider: provider.to_string(),
            model: model.to_string(),
            account: account.to_string(),
            prefix_digest: Sha256::digest(prefix.to_string().as_bytes()).into(),
            key_digest: body
                .get("prompt_cache_key")
                .map(|key| Sha256::digest(key.to_string().as_bytes()).into()),
        }
    }

    /// Describe changes from the previous request in this session.
    #[must_use]
    pub fn changes_from(&self, previous: &Self) -> Vec<CacheDiagnostic> {
        let mut diagnostics = Vec::new();
        if self.provider != previous.provider || self.model != previous.model {
            diagnostics.push(CacheDiagnostic::ModelChanged);
        }
        if self.account != previous.account {
            diagnostics.push(CacheDiagnostic::AccountChanged);
        }
        if self.prefix_digest != previous.prefix_digest {
            diagnostics.push(CacheDiagnostic::StablePrefixChanged);
        }
        if self.key_digest != previous.key_digest {
            diagnostics.push(CacheDiagnostic::CacheKeyChanged);
        }
        diagnostics
    }
}

/// Request-side observations independent of the provider's response.
#[must_use]
pub fn request_diagnostics(format: ProviderFormat, body: &Value) -> Vec<CacheDiagnostic> {
    match format {
        ProviderFormat::Anthropic if !has_cache_control(body) => {
            vec![CacheDiagnostic::MissingCacheControl]
        }
        ProviderFormat::OpenaiResponses
            if body
                .get("prompt_cache_key")
                .and_then(Value::as_str)
                .is_none_or(str::is_empty) =>
        {
            vec![CacheDiagnostic::MissingSessionKey]
        }
        _ => Vec::new(),
    }
}

/// Add reported cache outcome to the request observations.
#[must_use]
pub fn outcome_diagnostics(
    observed: &[CacheDiagnostic],
    status: u16,
    usage: &TokenUsage,
) -> Vec<CacheDiagnostic> {
    let mut diagnostics = observed.to_vec();
    if (200..300).contains(&status) {
        match usage.cache_read {
            None => diagnostics.push(CacheDiagnostic::TelemetryUnavailable),
            Some(0) if usage.cache_write == 0 => {
                diagnostics.push(CacheDiagnostic::CacheMissUnknown);
            }
            Some(_) => {}
        }
        if usage.cache_write > 0 {
            diagnostics.push(CacheDiagnostic::CacheWarming);
        }
    } else {
        diagnostics.push(CacheDiagnostic::UpstreamError);
    }
    diagnostics
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_preserves_content_and_client_markers_and_is_idempotent() {
        let mut body = json!({"system": "instructions", "tools": [{"name":"read", "input_schema":{}}], "messages":[{"role":"user","content":"hello"}]});
        apply_policy(
            ProviderFormat::Anthropic,
            PromptCachePolicy::Anthropic1h,
            &mut body,
        );
        assert_eq!(body["system"][0]["text"], "instructions");
        assert_eq!(body["messages"][0]["content"][0]["text"], "hello");
        assert_eq!(body["tools"][0]["cache_control"]["ttl"], "1h");
        let once = body.clone();
        apply_policy(
            ProviderFormat::Anthropic,
            PromptCachePolicy::Anthropic5m,
            &mut body,
        );
        assert_eq!(body, once);
        let mut explicit = json!({"cache_control":{"type":"ephemeral"}, "system":"s"});
        let before = explicit.clone();
        apply_policy(
            ProviderFormat::Anthropic,
            PromptCachePolicy::Anthropic1h,
            &mut explicit,
        );
        assert_eq!(explicit, before);
    }

    #[test]
    fn policy_is_opt_in_and_does_not_mark_other_formats_or_thinking() {
        let original = json!({"messages":[{"role":"assistant","content":[{"type":"thinking","thinking":"secret","signature":"s"}]}]});
        for (format, policy) in [
            (ProviderFormat::Openai, PromptCachePolicy::Anthropic1h),
            (ProviderFormat::Anthropic, PromptCachePolicy::Preserve),
            (ProviderFormat::Anthropic, PromptCachePolicy::Anthropic5m),
        ] {
            let mut body = original.clone();
            apply_policy(format, policy, &mut body);
            assert_eq!(body, original);
        }
    }

    #[test]
    fn observations_ignore_appended_turns_but_detect_static_and_route_changes() {
        let original = json!({"instructions":"stable", "input": [{"role":"user","content":"first"}], "prompt_cache_key":"session"});
        let first = CacheObservation::new("chatgpt", "gpt", "work", &original);
        let mut next = original;
        next["input"]
            .as_array_mut()
            .unwrap()
            .push(json!({"role":"user","content":"second"}));
        assert_eq!(
            CacheObservation::new("chatgpt", "gpt", "work", &next).changes_from(&first),
            Vec::<CacheDiagnostic>::new()
        );
        next["instructions"] = json!("changed");
        next["prompt_cache_key"] = json!("different");
        assert_eq!(
            CacheObservation::new("chatgpt", "other", "personal", &next).changes_from(&first),
            vec![
                CacheDiagnostic::ModelChanged,
                CacheDiagnostic::AccountChanged,
                CacheDiagnostic::StablePrefixChanged,
                CacheDiagnostic::CacheKeyChanged
            ]
        );
    }

    #[test]
    fn outcomes_distinguish_unknown_missing_writing_and_hits() {
        assert_eq!(
            outcome_diagnostics(&[], 200, &TokenUsage::default()),
            vec![CacheDiagnostic::TelemetryUnavailable]
        );
        let mut usage = TokenUsage {
            cache_read: Some(0),
            ..TokenUsage::default()
        };
        assert_eq!(
            outcome_diagnostics(&[], 200, &usage),
            vec![CacheDiagnostic::CacheMissUnknown]
        );
        usage.cache_write = 50;
        assert_eq!(
            outcome_diagnostics(&[], 200, &usage),
            vec![CacheDiagnostic::CacheWarming]
        );
        usage.cache_read = Some(40);
        usage.cache_write = 0;
        assert_eq!(
            outcome_diagnostics(&[], 200, &usage),
            Vec::<CacheDiagnostic>::new()
        );
        assert_eq!(
            outcome_diagnostics(&[], 429, &usage),
            vec![CacheDiagnostic::UpstreamError]
        );
    }
}
