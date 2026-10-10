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
/// # Errors
///
/// Returns an [`ApiError`] for unknown models, no available model, or a
/// model whose provider has no account.
pub fn resolve_model(
    state: &RuntimeState,
    client_model: Option<&str>,
) -> Result<(Arc<Provider>, String, Option<String>), ApiError> {
    let client_sent = !state.enforce_active_model && client_model.is_some_and(|m| !m.is_empty());
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
    let is_connected = |name: &str| is_connected(state, name);
    // A client-provided model is resolved strictly (no default-provider
    // fallback), so an unknown model fails loudly instead of silently routing
    // elsewhere. The proxy-override path keeps the default fallback.
    let resolved = if client_sent {
        state
            .router
            .resolve_client_model(&requested, &is_connected)
            .map_err(ApiError::from)?
    } else {
        state
            .router
            .resolve_model(&requested, &is_connected)
            .map_err(ApiError::from)?
    };
    if !is_connected(&resolved.provider.name) {
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

/// Pick the account a request goes through: the session pin, then the
/// per-request header, then the persisted selection, then the only account.
///
/// # Errors
///
/// Returns a `400` for stale pins/selections, unknown aliases, or an
/// ambiguous provider with several accounts.
pub fn account_for(
    state: &RuntimeState,
    provider: &Provider,
    session_alias: Option<&str>,
    alias: Option<&str>,
) -> Result<Account, ApiError> {
    let accounts = state.accounts.get(&provider.name).ok_or_else(|| {
        ApiError::internal(format!("client not built for provider {}", provider.name))
    })?;
    // A `$proxy account` pin outranks everything for this session.
    if let Some(alias) = session_alias {
        return accounts
            .get(alias)
            .filter(|account| account.has_credentials())
            .cloned()
            .ok_or_else(|| {
                ApiError::bad_request(format!(
                    "session account '{alias}' for provider '{}' is no longer stored; \
                     select another with `$proxy account {}/<alias>`",
                    provider.name, provider.name
                ))
            });
    }
    if let Some(alias) = alias {
        return accounts.get(alias).cloned().ok_or_else(|| {
            ApiError::bad_request(format!(
                "unknown account '{alias}' for provider '{}'",
                provider.name
            ))
        });
    }
    if let Some(selected) = state.config.defaults.active_accounts.get(&provider.name) {
        // The selection must resolve to a stored credential; the unauthenticated
        // placeholder account never satisfies it.
        return accounts
            .get(selected)
            .filter(|account| account.has_credentials())
            .cloned()
            .ok_or_else(|| {
                ApiError::bad_request(format!(
                    "selected account '{selected}' for provider '{}' is no longer stored; \
                     select another with `local-proxy account {}/<alias>`",
                    provider.name, provider.name
                ))
            });
    }
    if accounts.len() > 1 {
        return Err(ApiError::bad_request(format!(
            "provider '{}' has multiple accounts; select one with \
             `local-proxy account {}/<alias>` or specify X-Local-Proxy-Account",
            provider.name, provider.name
        )));
    }
    accounts.values().next().cloned().ok_or_else(|| {
        ApiError::internal(format!("client not built for provider {}", provider.name))
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
    let session_alias = app.session_account(session_id, &provider.name).await;
    let account = account_for(
        &state,
        &provider,
        session_alias.as_deref(),
        req.account_alias,
    )?;
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
    fn account_selection_requires_alias_only_for_multiple_accounts() {
        let provider = openai_provider(&["gpt-test"]);
        let cfg = Config {
            providers: vec![provider.clone()],
            ..Config::default()
        };
        let one = state_with(cfg.clone(), openai_accounts(&["one"]));
        assert!(account_for(&one, &provider, None, None).is_ok());
        assert!(account_for(&one, &provider, None, Some("one")).is_ok());
        let err = account_for(&one, &provider, None, Some("missing"))
            .expect_err("unknown alias must fail even with one account");
        assert_eq!(err.status, 400);
        assert!(err.message.contains("unknown account 'missing'"));

        let two = state_with(cfg, openai_accounts(&["one", "two"]));
        let err = account_for(&two, &provider, None, None).expect_err("ambiguous account");
        assert_eq!(err.status, 400);
        assert!(err.message.contains("X-Local-Proxy-Account"));
        assert!(account_for(&two, &provider, None, Some("two")).is_ok());
    }

    #[test]
    fn account_resolution_is_session_then_header_then_selection() {
        let provider = openai_provider(&["gpt-test"]);
        let state_with_selected = |selected: &str| {
            let cfg = Config {
                providers: vec![provider.clone()],
                defaults: Defaults {
                    active_accounts: HashMap::from([("openai".to_string(), selected.to_string())]),
                    ..Defaults::default()
                },
                ..Config::default()
            };
            state_with(cfg, openai_accounts(&["personal", "work"]))
        };

        // No header: the selected account is used without error.
        let state = state_with_selected("work");
        let selected = account_for(&state, &provider, None, None).unwrap();
        assert_eq!(selected.alias(), "work");
        // The header overrides the persisted selection per request.
        let overridden = account_for(&state, &provider, None, Some("personal")).unwrap();
        assert_eq!(overridden.alias(), "personal");

        // A session pin (from `$proxy account`) outranks both the persisted
        // selection and the per-request header.
        let pinned = account_for(&state, &provider, Some("personal"), None).unwrap();
        assert_eq!(pinned.alias(), "personal");
        let pinned_over_header =
            account_for(&state, &provider, Some("personal"), Some("work")).unwrap();
        assert_eq!(pinned_over_header.alias(), "personal");

        // A selection whose account was disconnected is a clear 400, never a
        // silent fallback to another account.
        let disconnected = state_with_selected("ghost");
        let err = account_for(&disconnected, &provider, None, None).expect_err("stale selection");
        assert_eq!(err.status, 400);
        assert!(
            err.message.contains("no longer stored"),
            "got: {}",
            err.message
        );
        // Same for a stale session pin.
        let err = account_for(&state, &provider, Some("ghost"), None).expect_err("stale pin");
        assert_eq!(err.status, 400);
        assert!(
            err.message.contains("session account"),
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
