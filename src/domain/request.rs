//! Pure request handling: preparing a client request for its upstream and
//! reading the bits of a request the proxy cares about.

use serde_json::{json, Value};

use crate::domain::config::{Provider, ProviderFormat};
use crate::domain::ir::{self, Format};
use crate::domain::translate::{self, TranslateError};

/// A request is streaming when it carries `stream: true`.
pub fn wants_stream(body: &Value) -> bool {
    body.get("stream").and_then(Value::as_bool) == Some(true)
}

/// Ask the upstream OpenAI-style server to include usage in the final chunk.
fn enable_usage(body: &mut Value) {
    if body.get("stream_options").is_none() {
        body["stream_options"] = json!({"include_usage": true});
    }
}

/// Apply the fields a Responses-API upstream needs before sending.
///
/// - the configured reasoning effort, when the resolved route set one and the
///   client did not ask for its own (the Codex backend takes reasoning depth as
///   a request field, so `gpt-6-sol` at `low` and at `medium` are the same model
///   id — the route pin is a default, not an override, mirroring how a
///   client-sent model wins over `active_model`);
/// - a `prompt_cache_key` from the client session id, when the client sent none;
/// - forced streaming: that backend rejects non-streaming requests
///   (`"Stream must be set to true"`). A client that asked for a single
///   response still gets one — the caller reassembles the stream.
///
/// No-op for other formats.
pub fn prepare_responses_request(
    provider: &Provider,
    body: &mut Value,
    effort: Option<&str>,
    session_id: &str,
) {
    if provider.format != ProviderFormat::OpenaiResponses {
        return;
    }
    // Route the prompt cache by conversation (what Codex itself sends), so
    // turns of one session hit the same cache.
    if body.get("prompt_cache_key").is_none() && !session_id.is_empty() {
        body["prompt_cache_key"] = json!(session_id);
    }
    if let Some(effort) = effort.filter(|e| !e.is_empty()) {
        if body.get("reasoning").is_none() {
            body["reasoning"] = json!({ "effort": effort });
        }
    }
    body["stream"] = json!(true);
}

/// Prepare a client request for its upstream provider: the upstream model,
/// the forced effort, format translation, and per-format request hygiene.
///
/// # Errors
///
/// Returns [`TranslateError`] when the body cannot be translated.
#[allow(clippy::too_many_arguments)]
pub fn prepare_upstream_body(
    client_format: Format,
    provider: &Provider,
    upstream_model: &str,
    mut body: Value,
    active_effort: Option<&str>,
    reasoning_effort: Option<&str>,
    session_id: &str,
    streaming: bool,
) -> Result<Value, TranslateError> {
    body["model"] = json!(upstream_model);
    if client_format == Format::Anthropic {
        if let Some(effort) = active_effort {
            body["output_config"]["effort"] = json!(effort);
        }
    }

    let upstream_format = Format::from(provider.format);
    let mut upstream_body = if upstream_format == client_format {
        same_format_request(client_format, body)
    } else {
        ir::translate_request(client_format, upstream_format, body)?
    };
    if streaming && upstream_format == Format::Openai {
        enable_usage(&mut upstream_body);
    }
    prepare_responses_request(provider, &mut upstream_body, reasoning_effort, session_id);
    Ok(upstream_body)
}

/// The reasoning effort an Anthropic request asks for (`output_config.effort`,
/// as Claude Code sends it), for the `/admin` session stats.
#[must_use]
pub fn request_effort(body: &[u8]) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct OutputConfig {
        effort: Option<String>,
    }
    #[derive(serde::Deserialize)]
    struct Req {
        output_config: Option<OutputConfig>,
    }
    serde_json::from_slice::<Req>(body)
        .ok()?
        .output_config?
        .effort
        .filter(|e| !e.is_empty())
}

/// Body for an upstream of the client's own format: no translation, only the
/// per-format hygiene the passthrough has always applied.
fn same_format_request(format: Format, body: Value) -> Value {
    match format {
        Format::Anthropic => translate::normalize_anthropic_request(&body),
        Format::Openai => body,
        // The ChatGPT backend rejects stored responses; make `store` explicit
        // when the client omitted it.
        Format::Responses => {
            let mut body = body;
            if body.get("store").is_none() {
                body["store"] = json!(false);
            }
            body
        }
    }
}

/// Heuristic token estimate: ceil(total chars of system + messages / 4).
#[must_use]
pub fn estimate_tokens(body: &Value) -> u64 {
    let mut chars = 0usize;
    if let Some(system) = body.get("system") {
        chars += text_len(system);
    }
    if let Some(msgs) = body.get("messages").and_then(Value::as_array) {
        for m in msgs {
            if let Some(content) = m.get("content") {
                chars += text_len(content);
            }
        }
    }
    chars.div_ceil(4) as u64
}

fn text_len(value: &Value) -> usize {
    match value {
        Value::String(s) => s.chars().count(),
        Value::Array(arr) => arr.iter().map(text_len).sum(),
        Value::Null => 0,
        other => other.to_string().chars().count(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_effort_reads_output_config() {
        let body = br#"{"model":"m","output_config":{"effort":"xhigh"},"messages":[]}"#;
        assert_eq!(request_effort(body).as_deref(), Some("xhigh"));
        assert_eq!(request_effort(br#"{"model":"m"}"#), None);
    }

    #[test]
    fn estimate_tokens_nonzero() {
        let body = json!({
            "system": "hello world",
            "messages": [{"role": "user", "content": "how are you today"}]
        });
        assert!(estimate_tokens(&body) > 0);
    }

    #[test]
    fn prepare_responses_request_forces_stream_and_sets_effort() {
        let responses = crate::domain::config::Provider {
            name: "chatgpt".to_string(),
            base_url: "http://x".to_string(),
            format: ProviderFormat::OpenaiResponses,
            models: Vec::new(),
            auto_model: None,
            headers: std::collections::HashMap::new(),
            session_header: None,
            oauth: None,
        };
        let mut body = json!({"model": "gpt-6-sol", "stream": false});
        prepare_responses_request(&responses, &mut body, Some("medium"), "sess-1");
        // the session routes the prompt cache
        assert_eq!(body["prompt_cache_key"], "sess-1");
        // the Codex backend rejects non-streaming, so upstream always streams
        assert_eq!(body["stream"], true);
        assert_eq!(body["reasoning"]["effort"], "medium");

        // without a configured effort the field is not injected
        let mut plain = json!({"model": "gpt-6-sol"});
        prepare_responses_request(&responses, &mut plain, None, "");
        assert_eq!(plain["stream"], true);
        assert!(plain.get("reasoning").is_none());

        // an empty effort string is treated as unset
        let mut empty = json!({});
        prepare_responses_request(&responses, &mut empty, Some(""), "");
        assert!(empty.get("reasoning").is_none());

        // the route pin is a default, not an override: a client that asked for
        // its own effort keeps it
        let mut client = json!({"reasoning": {"effort": "xhigh"}});
        prepare_responses_request(&responses, &mut client, Some("low"), "");
        assert_eq!(client["reasoning"]["effort"], "xhigh");
    }

    #[test]
    fn prepare_responses_request_leaves_other_formats_alone() {
        for format in [ProviderFormat::Openai, ProviderFormat::Anthropic] {
            let provider = crate::domain::config::Provider {
                name: "p".to_string(),
                base_url: "http://x".to_string(),
                format,
                models: Vec::new(),
                auto_model: None,
                headers: std::collections::HashMap::new(),
                session_header: None,
                oauth: None,
            };
            let mut body = json!({"stream": false});
            prepare_responses_request(&provider, &mut body, Some("medium"), "");
            // untouched: no forced streaming, no reasoning field
            assert_eq!(body["stream"], false);
            assert!(body.get("reasoning").is_none());
        }
    }

    #[test]
    fn prepare_upstream_body_normalizes_anthropic_and_preserves_cache_markers() {
        let provider = crate::domain::config::Provider {
            name: "claude".to_string(),
            base_url: "http://x".to_string(),
            format: ProviderFormat::Anthropic,
            models: Vec::new(),
            auto_model: None,
            headers: std::collections::HashMap::new(),
            session_header: None,
            oauth: None,
        };
        let body = json!({
            "model": "claude/haiku-5.5",
            "output_config": {"effort": "low"},
            "context_management": {"edits": []},
            "system": [{
                "type": "text",
                "text": "stable prefix",
                "cache_control": {"type": "ephemeral"}
            }],
            "messages": [{"role": "user", "content": "{{compare_tag}}"}]
        });

        let prepared = prepare_upstream_body(
            crate::domain::ir::Format::Anthropic,
            &provider,
            "haiku-5.5",
            body,
            Some("low"),
            None,
            "session-1",
            false,
        )
        .unwrap();

        assert_eq!(prepared["model"], "haiku-5.5");
        assert_eq!(prepared["system"][0]["cache_control"]["type"], "ephemeral");
        assert!(prepared.get("context_management").is_none());
        assert!(prepared.get("output_config").is_none());
    }

    #[test]
    fn prepare_upstream_body_sets_responses_stream_and_session_cache_key() {
        let provider = crate::domain::config::Provider {
            name: "chatgpt".to_string(),
            base_url: "http://x".to_string(),
            format: ProviderFormat::OpenaiResponses,
            models: Vec::new(),
            auto_model: None,
            headers: std::collections::HashMap::new(),
            session_header: None,
            oauth: None,
        };

        let prepared = prepare_upstream_body(
            crate::domain::ir::Format::Responses,
            &provider,
            "gpt-6-luna",
            json!({"model": "chatgpt/gpt-6-luna", "stream": false}),
            None,
            Some("low"),
            "session-1",
            false,
        )
        .unwrap();

        assert_eq!(prepared["model"], "gpt-6-luna");
        assert_eq!(prepared["stream"], true);
        assert_eq!(prepared["prompt_cache_key"], "session-1");
        assert_eq!(prepared["reasoning"]["effort"], "low");
        assert_eq!(prepared["store"], false);
    }
}
