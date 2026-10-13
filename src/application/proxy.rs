//! The proxy use case: serve one chat request from any supported client
//! format through the routed upstream account, translating both ways.

use std::sync::Arc;
use std::time::Instant;

use serde_json::{json, Value};

use crate::application::commands;
use crate::application::runtime::{AppState, RuntimeState};
use crate::application::streams::{self, EventStream};
use crate::application::usage::{RequestMeta, StreamCapture};
use crate::domain::config::{qualified_id, Provider};
use crate::domain::error::ApiError;
use crate::domain::exec;
use crate::domain::ir::{self, Format};
use crate::domain::request;
use crate::domain::sse::{self, ByteStream};
use crate::domain::translate;
use crate::ports::{Account, ClientHints, UpstreamRequest, UpstreamResponse};

/// One chat request as received by an inbound adapter.
#[derive(Debug, Clone)]
pub struct ChatRequest<'a> {
    /// The client's wire format (from the endpoint it called).
    pub format: Format,
    /// The endpoint path, for logs and stats.
    pub endpoint: &'static str,
    /// The raw request body.
    pub body: &'a [u8],
    /// The key the client presented, after [`authenticate`].
    pub client_key: Option<&'a str>,
    /// The `X-Local-Proxy-Account` override, if any.
    pub account_alias: Option<&'a str>,
    /// The client session id, or `""`.
    pub session_id: &'a str,
    /// Client headers worth forwarding upstream.
    pub hints: ClientHints,
}

/// The successful outcome of a chat request.
pub enum ChatReply {
    /// A complete JSON response (status 200).
    Json(Value),
    /// A stream translated into the client's format.
    Events(EventStream),
    /// A same-format stream forwarded byte for byte.
    Passthrough {
        /// The upstream status.
        status: u16,
        /// The upstream body.
        body: ByteStream,
    },
}

/// Validate the client's key against the configured API keys.
///
/// Returns the presented key (used for passthrough). If no keys are
/// configured, any client is allowed and the presented key (if any) is still
/// captured.
///
/// # Errors
///
/// Returns a `401` when keys are configured and the presented one is missing
/// or unknown.
pub fn authenticate(
    state: &RuntimeState,
    presented: Option<String>,
) -> Result<Option<String>, ApiError> {
    if state.config.server.api_keys.is_empty() {
        return Ok(presented);
    }
    match &presented {
        Some(key) if state.config.server.api_keys.iter().any(|k| k == key) => Ok(presented),
        Some(_) => {
            let e = ApiError::unauthorized("invalid API key");
            tracing::warn!(target: crate::LOG_TARGET, status = e.status, kind = %e.kind, "auth rejected: invalid API key");
            Err(e)
        }
        None => {
            let e = ApiError::unauthorized("missing API key");
            tracing::warn!(target: crate::LOG_TARGET, status = e.status, kind = %e.kind, "auth rejected: missing API key");
            Err(e)
        }
    }
}

fn is_connected(state: &RuntimeState, provider: &str) -> bool {
    state
        .accounts
        .get(provider)
        .is_some_and(|accounts| accounts.values().any(|a| a.has_credentials()))
}

/// Prefix that keeps a model ID visible in Claude Code's `/model`.
///
/// Claude Code's gateway discovery keeps only IDs matching
/// `/(claude|anthropic)/i`, so the Anthropic-shaped `/v1/models` prefixes the
/// rest and [`resolve_model`] strips it back off. Not `/`, which would collide
/// with the real `anthropic` provider.
pub const GATEWAY_ID_MARKER: &str = "anthropic:";

/// Whether Claude Code's gateway model discovery keeps this ID as-is.
#[must_use]
pub fn passes_gateway_filter(id: &str) -> bool {
    let id = id.to_ascii_lowercase();
    id.contains("claude") || id.contains("anthropic")
}

/// Resolve the model a request asks for to `(provider, upstream model,
/// reasoning effort)`.
///
/// Normally the client's requested model wins: it is resolved through the
/// router and must match a configured route/provider/native model. When the
/// client sends no model, the proxy falls back to its own override (active
/// model), then the first model available from a connected provider.
///
/// When `enforce_active_model` is set (e.g. via `local-proxy launch claude`),
/// a client-sent model is ignored entirely so the user's active model is
/// always the one routed, regardless of what the launched tool selects.
///
/// A Claude-family name the router can't resolve (Claude Code's own default,
/// sent before the user picks anything in `/model`) also falls back to the
/// active model instead of failing. IDs carrying [`GATEWAY_ID_MARKER`] (picked
/// from `/model`) have it stripped first.
///
/// # Errors
///
/// Returns an [`ApiError`] for unknown models, no available model, or a
/// model whose provider has no account.
pub fn resolve_model(
    state: &RuntimeState,
    client_model: Option<&str>,
) -> Result<(Arc<Provider>, String, Option<String>), ApiError> {
    let client_model = client_model
        .map(|m| m.strip_prefix(GATEWAY_ID_MARKER).unwrap_or(m))
        .filter(|m| !m.is_empty());
    let connected = |name: &str| is_connected(state, name);
    let client_sent = !state.enforce_active_model
        && client_model.is_some_and(|m| {
            state.config.defaults.active_model.is_none()
                || !passes_gateway_filter(m)
                || state.router.resolve_client_model(m, &connected).is_ok()
        });
    let requested = if client_sent {
        client_model.unwrap_or_default().to_string()
    } else if let Some(model) = state.config.defaults.active_model.clone() {
        model
    } else {
        let first = state.config.providers.iter().find_map(|p| {
            is_connected(state, &p.name)
                .then(|| p.models.first().map(|m| qualified_id(&p.name, m)))
                .flatten()
        });
        first.ok_or_else(|| {
            ApiError::bad_request(
                "no model available; connect a provider or run `local-proxy model <model>`",
            )
        })?
    };
    // A client-provided model is resolved strictly (no default-provider
    // fallback), so an unknown model fails loudly instead of silently routing
    // elsewhere. The proxy-override path keeps the default fallback.
    let resolved = if client_sent {
        state
            .router
            .resolve_client_model(&requested, &connected)
            .map_err(ApiError::from)?
    } else {
        state
            .router
            .resolve_model(&requested, &connected)
            .map_err(ApiError::from)?
    };
    if !connected(&resolved.provider.name) {
        return Err(ApiError::bad_request(format!(
            "model '{requested}' resolves to provider '{}' which has no account; \
             connect it via `local-proxy connect {} --account <alias>` or select a connected model",
            resolved.provider.name, resolved.provider.name
        )));
    }
    Ok((
        resolved.provider,
        resolved.upstream_model,
        resolved.reasoning_effort,
    ))
}

/// Pick the account a request goes through.
///
/// The per-request `header` alias wins, then `chosen` (the session's account,
/// else the provider's selection). Both must name a stored credential; the
/// unauthenticated placeholder of a provider without accounts is only used
/// when neither is given.
///
/// # Errors
///
/// Returns a `400` for an unknown or stale alias.
pub fn account_for(
    state: &RuntimeState,
    provider: &Provider,
    header: Option<&str>,
    chosen: Option<&str>,
) -> Result<Account, ApiError> {
    let accounts = state.accounts.get(&provider.name).ok_or_else(|| {
        ApiError::internal(format!("client not built for provider {}", provider.name))
    })?;
    let stored = |alias: &str| {
        accounts
            .get(alias)
            .filter(|account| account.has_credentials())
            .cloned()
    };
    if let Some(alias) = header {
        return stored(alias).ok_or_else(|| {
            ApiError::bad_request(format!(
                "unknown account '{alias}' for provider '{}'",
                provider.name
            ))
        });
    }
    if let Some(alias) = chosen {
        return stored(alias).ok_or_else(|| {
            ApiError::bad_request(format!(
                "account '{alias}' for provider '{}' is no longer stored; \
                 select another with `local-proxy account {}/<alias>`",
                provider.name, provider.name
            ))
        });
    }
    // Only a provider without stored accounts has no selection.
    accounts.get("default").cloned().ok_or_else(|| {
        ApiError::internal(format!(
            "no account selected for provider {}",
            provider.name
        ))
    })
}

/// Read a completed upstream response into `(status, body)`. The raw body is
/// read as text first, so a non-JSON error page is kept as a
/// [`Value::String`] instead of being dropped.
pub async fn read_body(resp: UpstreamResponse) -> (u16, Value) {
    let status = resp.status;
    let content_type = resp.content_type.unwrap_or_default();
    let raw = sse::collect(resp.body).await;
    let text = String::from_utf8_lossy(&raw).into_owned();
    serde_json::from_str::<Value>(&text).map_or_else(
        |_| {
            tracing::warn!(
                target: crate::LOG_TARGET,
                status,
                content_type = %content_type,
                body = %text,
                "upstream error body is not JSON"
            );
            (status, Value::String(text))
        },
        |body| (status, body),
    )
}

/// The active model for a synthesized reply, or `local-proxy` when none is
/// selected.
fn active_model_or_default(state: &RuntimeState) -> String {
    state
        .config
        .defaults
        .active_model
        .clone()
        .unwrap_or_else(|| "local-proxy".to_string())
}

/// Serve one chat request: answer a `$proxy` command locally, or resolve the
/// route, translate through the IR when the upstream speaks another format,
/// and translate the response (or stream) back.
///
/// # Errors
///
/// Returns an [`ApiError`] for bad requests, routing and account problems,
/// transport failures, and upstream error statuses.
#[allow(clippy::too_many_lines)]
pub async fn handle_chat(app: &AppState, req: ChatRequest<'_>) -> Result<ChatReply, ApiError> {
    let started = Instant::now();
    let state = app.session_state(req.session_id).await;
    let ChatRequest {
        format: client_format,
        endpoint,
        session_id,
        ..
    } = req;
    if client_format == Format::Anthropic && !session_id.is_empty() {
        if let Some(effort) = request::request_effort(req.body) {
            app.recorder().record_effort(session_id, &effort);
        }
    }
    let body: Value =
        serde_json::from_slice(req.body).map_err(|_| ApiError::bad_request("invalid JSON body"))?;

    if let Some(out) = commands::maybe_exec(app, &state, &body, session_id).await {
        let text = exec::format_output(&out);
        tracing::info!(target: crate::LOG_TARGET, endpoint, "handled $proxy command");
        return Ok(ChatReply::Json(exec::local_reply(
            client_format,
            &text,
            &active_model_or_default(&state),
        )));
    }

    let streaming = request::wants_stream(&body);
    let client_model = body.get("model").and_then(Value::as_str);
    let (provider, upstream_model, reasoning_effort) = resolve_model(&state, client_model)?;
    tracing::info!(
        target: crate::LOG_TARGET,
        endpoint,
        provider = %provider.name,
        upstream_model,
        streaming,
        "resolved route"
    );
    // A provider without stored accounts has nothing to choose (passthrough).
    let has_accounts = state
        .accounts
        .get(&provider.name)
        .is_some_and(|accounts| accounts.values().any(|a| a.has_credentials()));
    let chosen = if req.account_alias.is_none() && has_accounts {
        app.request_account(session_id, &provider.name)
            .await
            .map_err(|e| ApiError::internal(e.to_string()))?
    } else {
        None
    };
    let account = account_for(&state, &provider, req.account_alias, chosen.as_deref())?;
    let upstream_format = Format::from(provider.format);
    let same = upstream_format == client_format;
    let upstream_body = request::prepare_upstream_body(
        client_format,
        &provider,
        &upstream_model,
        body,
        if client_format == Format::Anthropic {
            state.config.defaults.active_effort.as_deref()
        } else {
            None
        },
        reasoning_effort.as_deref(),
        session_id,
        streaming,
    )?;

    let resp = account
        .send(UpstreamRequest {
            body: upstream_body,
            client_key: req.client_key.map(str::to_string),
            session_id: session_id.to_string(),
            hints: req.hints,
        })
        .await
        .map_err(ApiError::from)?;
    if let Some(limits) = resp.rate_limits {
        app.recorder()
            .record_rate_limits(&provider.name, account.alias(), limits);
    }
    let meta = RequestMeta {
        endpoint,
        provider: provider.name.clone(),
        alias: account.alias().to_string(),
        model: upstream_model.clone(),
        streamed: streaming,
        started,
        session_id: session_id.to_string(),
    };
    let recorder = app.recorder();
    let status = resp.status;
    if status >= 400 {
        let (status, rbody) = read_body(resp).await;
        recorder.record_response(&meta, status, Some(&rbody));
        tracing::warn!(
            target: crate::LOG_TARGET,
            endpoint,
            provider = %provider.name,
            status,
            body = %rbody,
            "upstream returned error"
        );
        return Err(ApiError::from_upstream(status, rbody));
    }
    if streaming {
        let capture = StreamCapture::new(recorder.clone(), meta, status);
        return Ok(if same {
            ChatReply::Passthrough {
                status,
                body: streams::scan_passthrough(resp.body, Some(capture)),
            }
        } else {
            ChatReply::Events(streams::translate(
                resp.body,
                upstream_format,
                client_format,
                &upstream_model,
                Some(capture),
            ))
        });
    }

    // A Responses upstream only streams; fold its events into one response.
    if upstream_format == Format::Responses {
        let mut folded = streams::aggregate_responses(resp.body).await;
        let usage = json!({"usage": translate::responses_usage(&folded.usage)});
        recorder.record_parsed(&meta, status, Some(&usage), folded.usage);
        folded.model.clone_from(&upstream_model);
        return Ok(ChatReply::Json(ir::encode_response(client_format, &folded)));
    }
    let rbody =
        serde_json::from_slice::<Value>(&sse::collect(resp.body).await).unwrap_or(Value::Null);
    recorder.record_response(&meta, status, Some(&rbody));
    if same {
        return Ok(ChatReply::Json(rbody));
    }
    let mut decoded = ir::decode_response(upstream_format, &rbody);
    decoded.model = upstream_model;
    Ok(ChatReply::Json(ir::encode_response(
        client_format,
        &decoded,
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::testing::{account, empty_state, openai_provider, state_with};
    use crate::domain::config::{Config, Defaults, ProviderFormat, Route, Server};
    use std::collections::HashMap;

    fn two_provider_config(active_model: Option<&str>, routes: Vec<Route>) -> Config {
        Config {
            server: Server::default(),
            providers: vec![
                openai_provider(&["gpt-4o", "gpt-4o-mini"]),
                Provider {
                    name: "anthropic".to_string(),
                    base_url: "https://api.anthropic.com".to_string(),
                    format: ProviderFormat::Anthropic,
                    models: vec!["claude-sonnet-4-5".to_string()],
                    ..Provider::default()
                },
            ],
            routes,
            defaults: Defaults {
                provider: "openai".to_string(),
                active_model: active_model.map(str::to_string),
                ..Defaults::default()
            },
            exec: crate::domain::config::Exec::default(),
        }
    }

    fn openai_accounts(aliases: &[&str]) -> crate::ports::AccountMap {
        let provider = openai_provider(&["gpt-test"]);
        HashMap::from([(
            "openai".to_string(),
            aliases
                .iter()
                .map(|a| ((*a).to_string(), account(&provider, a, true)))
                .collect(),
        )])
    }

    #[test]
    fn auth_accepts_configured_keys() {
        let mut cfg = Config::default();
        cfg.server.api_keys = vec!["sk-proxy".to_string()];
        let state = empty_state(cfg);
        assert!(authenticate(&state, None).is_err());
        assert!(authenticate(&state, Some("nope".to_string())).is_err());
        assert_eq!(
            authenticate(&state, Some("sk-proxy".to_string()))
                .unwrap()
                .as_deref(),
            Some("sk-proxy")
        );
    }

    #[test]
    fn no_keys_means_open_access() {
        let state = empty_state(Config::default());
        assert_eq!(authenticate(&state, None).unwrap(), None);
    }

    #[test]
    fn client_unknown_model_fails_even_with_active_model() {
        let cfg = two_provider_config(
            Some("gpt-4o"),
            vec![Route {
                model: "gpt-4o".to_string(),
                provider: "openai".to_string(),
                ..Route::default()
            }],
        );
        let state = state_with(cfg, openai_accounts(&["default"]));
        // A client-sent model that does not resolve fails with `proxy: unknown
        // model`, regardless of the configured active_model fallback.
        let err = resolve_model(&state, Some("totally-unknown")).expect_err("unknown model");
        assert_eq!(err.message, "proxy: unknown model totally-unknown");

        // Claude Code's own default (a Claude name the router can't resolve)
        // falls back to the active model instead.
        let (provider, upstream, _) =
            resolve_model(&state, Some("claude-opus-9")).expect("falls back");
        assert_eq!(
            (provider.name.as_str(), upstream.as_str()),
            ("openai", "gpt-4o")
        );

        // An ID picked from `/model` arrives with the discovery marker.
        let (provider, upstream, _) =
            resolve_model(&state, Some("anthropic:openai/gpt-4o")).expect("marker stripped");
        assert_eq!(
            (provider.name.as_str(), upstream.as_str()),
            ("openai", "gpt-4o")
        );
    }

    #[test]
    fn gateway_filter_matches_claude_code_regex() {
        assert!(passes_gateway_filter("claude-sonnet-4-5"));
        assert!(passes_gateway_filter("anthropic/x"));
        assert!(passes_gateway_filter("Claude"));
        assert!(!passes_gateway_filter("openai/gpt-4o"));
    }

    #[test]
    fn client_model_wins_over_active_model() {
        let state = state_with(
            two_provider_config(Some("gpt-4o"), Vec::new()),
            openai_accounts(&["default"]),
        );
        // Active model is gpt-4o, but the client asks for gpt-4o-mini: client wins.
        let (provider, upstream, _effort) =
            resolve_model(&state, Some("gpt-4o-mini")).expect("resolves");
        assert_eq!(provider.name, "openai");
        assert_eq!(upstream, "gpt-4o-mini");

        // Client sends no model: the active_model fallback applies.
        let (provider, upstream, _effort) = resolve_model(&state, None).expect("resolves");
        assert_eq!(provider.name, "openai");
        assert_eq!(upstream, "gpt-4o");
    }

    #[test]
    fn enforce_active_model_ignores_client_model() {
        let mut state = state_with(
            two_provider_config(Some("gpt-4o"), Vec::new()),
            openai_accounts(&["default"]),
        );
        state.enforce_active_model = true;
        // Even though the client asks for gpt-4o-mini, the enforced active model
        // (gpt-4o) wins and the client-sent model is ignored.
        let (provider, upstream, _effort) =
            resolve_model(&state, Some("gpt-4o-mini")).expect("resolves");
        assert_eq!(provider.name, "openai");
        assert_eq!(upstream, "gpt-4o");

        // With no client model it naturally uses the active model too.
        let (provider, upstream, _effort) = resolve_model(&state, None).expect("resolves");
        assert_eq!(provider.name, "openai");
        assert_eq!(upstream, "gpt-4o");
    }

    #[test]
    fn no_active_model_uses_first_connected_model() {
        let cfg = Config {
            providers: vec![openai_provider(&["gpt-4o", "gpt-4o-mini"])],
            ..Config::default()
        };
        let state = state_with(cfg, openai_accounts(&["default"]));
        let (provider, upstream, _effort) = resolve_model(&state, None).expect("resolves");
        assert_eq!(provider.name, "openai");
        assert_eq!(upstream, "gpt-4o");
    }

    #[test]
    fn no_model_available_errors() {
        let cfg = Config {
            providers: vec![openai_provider(&["gpt-4o"])],
            ..Config::default()
        };
        let state = state_with(cfg, HashMap::new());
        assert!(resolve_model(&state, None).is_err());
    }

    #[test]
    fn active_model_to_unconnected_provider_errors_clearly() {
        let cfg = Config {
            providers: vec![Provider {
                name: "neuralwatt".to_string(),
                base_url: "https://api.neuralwatt.com/v1".to_string(),
                format: ProviderFormat::Openai,
                models: vec!["glm-5.2".to_string()],
                ..Provider::default()
            }],
            defaults: Defaults {
                provider: "neuralwatt".to_string(),
                active_model: Some("glm-5.2".to_string()),
                ..Defaults::default()
            },
            ..Config::default()
        };
        let state = state_with(cfg, HashMap::new());
        let err = resolve_model(&state, None).expect_err("unconnected provider is an error");
        assert!(err.message.contains("no account"), "got: {}", err.message);
    }

    #[test]
    fn account_resolution_is_header_then_chosen() {
        let provider = openai_provider(&["gpt-test"]);
        let cfg = Config {
            providers: vec![provider.clone()],
            ..Config::default()
        };
        let state = state_with(cfg, openai_accounts(&["personal", "work"]));

        // The chosen account (session, else selection) is used.
        let chosen = account_for(&state, &provider, None, Some("work")).unwrap();
        assert_eq!(chosen.alias(), "work");
        // The header overrides it per request.
        let header = account_for(&state, &provider, Some("personal"), Some("work")).unwrap();
        assert_eq!(header.alias(), "personal");

        // An unknown header alias is a 400.
        let err = account_for(&state, &provider, Some("nope"), None).expect_err("unknown");
        assert_eq!(err.status, 400);
        assert!(err.message.contains("unknown account 'nope'"));
        // A chosen account that was disconnected is a clear 400, never a
        // silent fallback to another account.
        let err = account_for(&state, &provider, None, Some("ghost")).expect_err("stale");
        assert_eq!(err.status, 400);
        assert!(
            err.message.contains("no longer stored"),
            "got: {}",
            err.message
        );
    }

    #[tokio::test]
    async fn read_body_keeps_non_json_and_empty_bodies() {
        let resp = |body: &'static str| UpstreamResponse {
            status: 400,
            content_type: Some("text/plain".to_string()),
            rate_limits: None,
            body: sse::once(body),
        };
        assert_eq!(
            read_body(resp("upstream exploded")).await,
            (400, Value::String("upstream exploded".to_string()))
        );
        assert_eq!(
            read_body(resp(r#"{"model":"m"}"#)).await,
            (400, json!({"model": "m"}))
        );
        assert_eq!(
            read_body(resp("")).await,
            (400, Value::String(String::new()))
        );
    }
}
