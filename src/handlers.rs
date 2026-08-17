use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::sse::{KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Json;
use axum::Router as AxumRouter;
use serde_json::{json, Value};

use crate::config::{Config, ProviderFormat};
use crate::error::ApiError;
use crate::router::Router;
use crate::streams;
use crate::streams::UpstreamStream;
use crate::translate;
use crate::upstream::{send_and_read, ProviderClient, UpstreamError};

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub router: Arc<Router>,
    pub clients: Arc<HashMap<String, ProviderClient>>,
}

pub fn build_clients(config: &Config) -> Result<HashMap<String, ProviderClient>, UpstreamError> {
    let passthrough = config.server.passthrough_keys;
    let mut map = HashMap::new();
    for provider in &config.providers {
        map.insert(
            provider.name.clone(),
            ProviderClient::new(provider, passthrough)?,
        );
    }
    Ok(map)
}

pub fn app(state: AppState) -> AxumRouter {
    AxumRouter::new()
        .route("/health", get(health))
        .route("/v1/messages", post(messages_handler))
        .route("/v1/messages/count_tokens", post(count_tokens_handler))
        .route("/v1/chat/completions", post(chat_completions_handler))
        .route("/v1/responses", post(responses_handler))
        .route("/v1/models", get(models_handler))
        .with_state(state)
}

async fn health() -> &'static str {
    "ok"
}

// ---------------------------------------------------------------------------
// auth
// ---------------------------------------------------------------------------

/// Validate the client's key against the configured API keys and return the
/// presented key (used for passthrough). If no keys are configured, any client
/// is allowed and the presented key (if any) is still captured.
fn authenticate(state: &AppState, headers: &HeaderMap) -> Result<Option<String>, ApiError> {
    let presented = extract_client_key(headers);
    if state.config.server.api_keys.is_empty() {
        return Ok(presented);
    }
    match &presented {
        Some(key) if state.config.server.api_keys.iter().any(|k| k == key) => Ok(presented),
        Some(_) => Err(ApiError::unauthorized("invalid API key")),
        None => Err(ApiError::unauthorized("missing API key")),
    }
}

fn extract_client_key(headers: &HeaderMap) -> Option<String> {
    if let Some(v) = headers.get("x-api-key") {
        if let Ok(s) = v.to_str() {
            return Some(s.to_string());
        }
    }
    if let Some(v) = headers.get(header::AUTHORIZATION) {
        if let Ok(s) = v.to_str() {
            if let Some(rest) = s.strip_prefix("Bearer ") {
                return Some(rest.to_string());
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// small helpers
// ---------------------------------------------------------------------------

fn json_response(status: StatusCode, value: Value) -> Response {
    (status, Json(value)).into_response()
}

fn error_response(err: &ApiError, anthropic: bool) -> Response {
    let status = StatusCode::from_u16(err.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let body = if anthropic {
        err.to_anthropic_error()
    } else {
        err.to_openai_error()
    };
    json_response(status, body)
}

fn parse_body(body: &Bytes) -> Result<Value, ApiError> {
    serde_json::from_slice(body).map_err(|_| ApiError::bad_request("invalid JSON body"))
}

/// Seam replaced by task 003: streaming is now fully supported. A request is
/// streaming when it carries `stream: true`.
fn wants_stream(body: &Value) -> bool {
    body.get("stream").and_then(Value::as_bool) == Some(true)
}

/// Ask the upstream OpenAI-style server to include usage in the final chunk.
fn enable_usage(body: &mut Value) {
    if body.get("stream_options").is_none() {
        body["stream_options"] = json!({"include_usage": true});
    }
}

fn sse_response(stream: UpstreamStream) -> Response {
    Sse::new(stream)
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
        .into_response()
}

/// Forward a same-format upstream SSE response verbatim.
fn passthrough_stream(resp: reqwest::Response) -> Response {
    Response::builder()
        .status(resp.status())
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .body(Body::from_stream(resp.bytes_stream()))
        .unwrap_or_else(|_| StatusCode::BAD_GATEWAY.into_response())
}

fn resolve_model(
    state: &AppState,
    body: &Value,
) -> Result<(Arc<crate::config::Provider>, String), ApiError> {
    let model = body
        .get("model")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::bad_request("missing required field 'model'"))?;
    let resolved = state.router.resolve_model(model).map_err(ApiError::from)?;
    Ok((resolved.provider, resolved.upstream_model))
}

fn client_for(
    state: &AppState,
    provider: &crate::config::Provider,
) -> Result<ProviderClient, ApiError> {
    state.clients.get(&provider.name).cloned().ok_or_else(|| {
        ApiError::internal(format!("client not built for provider {}", provider.name))
    })
}

// ---------------------------------------------------------------------------
// /v1/messages (Anthropic client)
// ---------------------------------------------------------------------------

async fn messages_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let client_key = match authenticate(&state, &headers) {
        Ok(k) => k,
        Err(e) => return error_response(&e, true),
    };
    match handle_messages(&state, &body, client_key.as_deref()).await {
        Ok(r) => r,
        Err(e) => error_response(&e, true),
    }
}

async fn handle_messages(
    state: &AppState,
    body: &Bytes,
    client_key: Option<&str>,
) -> Result<Response, ApiError> {
    let mut body = parse_body(body)?;
    let streaming = wants_stream(&body);
    let (provider, upstream_model) = resolve_model(state, &body)?;
    body["model"] = json!(upstream_model);
    let client = client_for(state, &provider)?;

    let mut upstream_body = match provider.format {
        ProviderFormat::Anthropic => translate::normalize_anthropic_request(&body),
        ProviderFormat::Openai => translate::anthropic_to_openai_request(body)?,
    };
    if streaming && provider.format == ProviderFormat::Openai {
        enable_usage(&mut upstream_body);
    }

    let resp = client
        .chat_request(client.default_path(), upstream_body, client_key)
        .await
        .map_err(ApiError::from)?;
    let status = resp.status().as_u16();
    if status >= 400 {
        let (status, rbody) = send_and_read(resp).await;
        return Err(ApiError::from_upstream(status, rbody));
    }
    if streaming {
        return match provider.format {
            ProviderFormat::Anthropic => Ok(passthrough_stream(resp)),
            ProviderFormat::Openai => Ok(sse_response(streams::anthropic_from_openai(
                resp,
                upstream_model,
            ))),
        };
    }

    let rbody = resp.json::<Value>().await.unwrap_or(Value::Null);
    match provider.format {
        ProviderFormat::Anthropic => Ok(json_response(StatusCode::OK, rbody)),
        ProviderFormat::Openai => {
            let translated = translate::openai_to_anthropic_response(rbody, &upstream_model)
                .map_err(ApiError::from)?;
            Ok(json_response(StatusCode::OK, translated))
        }
    }
}

// ---------------------------------------------------------------------------
// /v1/chat/completions (OpenAI client)
// ---------------------------------------------------------------------------

async fn chat_completions_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let client_key = match authenticate(&state, &headers) {
        Ok(k) => k,
        Err(e) => return error_response(&e, false),
    };
    match handle_chat_completions(&state, &body, client_key.as_deref()).await {
        Ok(r) => r,
        Err(e) => error_response(&e, false),
    }
}

async fn handle_chat_completions(
    state: &AppState,
    body: &Bytes,
    client_key: Option<&str>,
) -> Result<Response, ApiError> {
    let mut body = parse_body(body)?;
    let streaming = wants_stream(&body);
    let (provider, upstream_model) = resolve_model(state, &body)?;
    body["model"] = json!(upstream_model);
    let client = client_for(state, &provider)?;

    let mut upstream_body = match provider.format {
        ProviderFormat::Openai => body,
        ProviderFormat::Anthropic => translate::openai_to_anthropic_request(body)?,
    };
    if streaming && provider.format == ProviderFormat::Openai {
        enable_usage(&mut upstream_body);
    }

    let resp = client
        .chat_request(client.default_path(), upstream_body, client_key)
        .await
        .map_err(ApiError::from)?;
    let status = resp.status().as_u16();
    if status >= 400 {
        let (status, rbody) = send_and_read(resp).await;
        return Err(ApiError::from_upstream(status, rbody));
    }
    if streaming {
        return match provider.format {
            ProviderFormat::Openai => Ok(passthrough_stream(resp)),
            ProviderFormat::Anthropic => Ok(sse_response(streams::openai_from_anthropic(
                resp,
                upstream_model,
            ))),
        };
    }

    let rbody = resp.json::<Value>().await.unwrap_or(Value::Null);
    match provider.format {
        ProviderFormat::Openai => Ok(json_response(StatusCode::OK, rbody)),
        ProviderFormat::Anthropic => {
            let translated = translate::anthropic_to_openai_response(rbody, &upstream_model)
                .map_err(ApiError::from)?;
            Ok(json_response(StatusCode::OK, translated))
        }
    }
}

// ---------------------------------------------------------------------------
// /v1/responses (OpenAI Responses client)
// ---------------------------------------------------------------------------

async fn responses_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let client_key = match authenticate(&state, &headers) {
        Ok(k) => k,
        Err(e) => return error_response(&e, false),
    };
    match handle_responses(&state, &body, client_key.as_deref()).await {
        Ok(r) => r,
        Err(e) => error_response(&e, false),
    }
}

async fn handle_responses(
    state: &AppState,
    body: &Bytes,
    client_key: Option<&str>,
) -> Result<Response, ApiError> {
    let mut body = parse_body(body)?;
    let streaming = wants_stream(&body);
    let (provider, upstream_model) = resolve_model(state, &body)?;
    body["model"] = json!(upstream_model);
    let client = client_for(state, &provider)?;

    let mut upstream_body = match provider.format {
        ProviderFormat::Openai => translate::responses_to_openai_request(body)?,
        ProviderFormat::Anthropic => translate::responses_to_anthropic_request(body)?,
    };
    if streaming && provider.format == ProviderFormat::Openai {
        enable_usage(&mut upstream_body);
    }

    let resp = client
        .chat_request(client.default_path(), upstream_body, client_key)
        .await
        .map_err(ApiError::from)?;
    let status = resp.status().as_u16();
    if status >= 400 {
        let (status, rbody) = send_and_read(resp).await;
        return Err(ApiError::from_upstream(status, rbody));
    }
    if streaming {
        return match provider.format {
            ProviderFormat::Openai => Ok(sse_response(streams::responses_from_openai(
                resp,
                upstream_model,
            ))),
            ProviderFormat::Anthropic => Ok(sse_response(streams::responses_from_anthropic(
                resp,
                upstream_model,
            ))),
        };
    }

    let rbody = resp.json::<Value>().await.unwrap_or(Value::Null);
    let translated = match provider.format {
        ProviderFormat::Openai => translate::openai_to_responses_response(rbody, &upstream_model)
            .map_err(ApiError::from)?,
        ProviderFormat::Anthropic => {
            translate::anthropic_to_responses_response(rbody, &upstream_model)
                .map_err(ApiError::from)?
        }
    };
    Ok(json_response(StatusCode::OK, translated))
}

// ---------------------------------------------------------------------------
// /v1/models
// ---------------------------------------------------------------------------

async fn models_handler(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let models = state.router.list_models();
    if headers.contains_key("anthropic-version") {
        let data: Vec<Value> = models
            .iter()
            .map(|m| json!({"type": "model", "id": m}))
            .collect();
        let first = models.first();
        let last = models.last();
        json_response(
            StatusCode::OK,
            json!({"data": data, "has_more": false, "first_id": first, "last_id": last}),
        )
    } else {
        let data: Vec<Value> = models
            .iter()
            .map(|m| json!({"id": m, "object": "model", "created": 0, "owned_by": "local-proxy"}))
            .collect();
        json_response(StatusCode::OK, json!({"object": "list", "data": data}))
    }
}

// ---------------------------------------------------------------------------
// /v1/messages/count_tokens
// ---------------------------------------------------------------------------

async fn count_tokens_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Err(e) = authenticate(&state, &headers) {
        return error_response(&e, true);
    }
    match parse_body(&body) {
        Ok(body) => {
            let n = estimate_tokens(&body);
            json_response(StatusCode::OK, json!({"input_tokens": n}))
        }
        Err(e) => error_response(&e, true),
    }
}

/// Heuristic token estimate: ceil(total chars of system + messages / 4).
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

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_accepts_configured_keys() {
        let cfg = Config {
            server: crate::config::Server {
                host: "127.0.0.1".to_string(),
                port: 0,
                api_keys: vec!["sk-proxy".to_string()],
                passthrough_keys: false,
            },
            providers: Vec::new(),
            routes: Vec::new(),
            defaults: crate::config::Defaults::default(),
        };
        let state = AppState {
            config: Arc::new(cfg),
            router: Arc::new(Router::new(Arc::new(Config::default())).unwrap()),
            clients: Arc::new(HashMap::new()),
        };
        let mut headers = HeaderMap::new();
        assert!(authenticate(&state, &headers).is_err());
        headers.insert("x-api-key", "sk-proxy".parse().unwrap());
        assert_eq!(
            authenticate(&state, &headers).unwrap().as_deref(),
            Some("sk-proxy")
        );
        headers.insert(header::AUTHORIZATION, "Bearer sk-proxy".parse().unwrap());
        assert_eq!(
            authenticate(&state, &headers).unwrap().as_deref(),
            Some("sk-proxy")
        );
    }

    #[test]
    fn no_keys_means_open_access() {
        let cfg = Config::default();
        let state = AppState {
            config: Arc::new(cfg),
            router: Arc::new(Router::new(Arc::new(Config::default())).unwrap()),
            clients: Arc::new(HashMap::new()),
        };
        assert_eq!(authenticate(&state, &HeaderMap::new()).unwrap(), None);
    }

    #[test]
    fn estimate_tokens_nonzero() {
        let body = json!({
            "system": "hello world",
            "messages": [{"role": "user", "content": "how are you today"}]
        });
        assert!(estimate_tokens(&body) > 0);
    }
}
