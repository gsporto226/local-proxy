//! The provider-compatible endpoints (`/v1/messages`, `/v1/chat/completions`,
//! `/v1/responses`, `/v1/models`, `/v1/messages/count_tokens`): HTTP in,
//! [`crate::application::proxy`] use case, HTTP out.

use std::convert::Infallible;
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::Json;
use futures_util::StreamExt as _;
use serde_json::{json, Value};

use crate::application::proxy::{self, ChatReply, ChatRequest};
use crate::application::runtime::AppState;
use crate::application::streams::EventStream;
use crate::domain::error::ApiError;
use crate::domain::ir::Format;
use crate::domain::request::estimate_tokens;
use crate::ports::ClientHints;

pub(super) async fn health() -> &'static str {
    "ok"
}

/// The key a client presents: `x-api-key`, else a `Bearer` authorization.
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

/// The client session id (`X-Claude-Code-Session-Id`), or `""` when absent
/// (curl, plain `OpenAI` clients, tests).
fn extract_session_id(headers: &HeaderMap) -> String {
    headers
        .get("x-claude-code-session-id")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string()
}

fn extract_account_alias(headers: &HeaderMap) -> Result<Option<&str>, ApiError> {
    headers
        .get("x-local-proxy-account")
        .map(|value| {
            value
                .to_str()
                .map_err(|_| ApiError::bad_request("invalid X-Local-Proxy-Account header"))
        })
        .transpose()
}

fn extract_hints(headers: &HeaderMap) -> ClientHints {
    let get = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    };
    ClientHints {
        user_agent: get(header::USER_AGENT.as_str()),
        anthropic_beta: get("anthropic-beta"),
    }
}

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

fn sse_response(events: EventStream) -> Response {
    let events = events.map(|e| {
        let event = e
            .event
            .map_or_else(Event::default, |name| Event::default().event(name));
        Ok::<_, Infallible>(event.data(e.data))
    });
    Sse::new(events)
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
        .into_response()
}

fn reply_response(reply: ChatReply) -> Response {
    match reply {
        ChatReply::Json(body) => json_response(StatusCode::OK, body),
        ChatReply::Events(events) => sse_response(events),
        ChatReply::Passthrough { status, body } => Response::builder()
            .status(status)
            .header(header::CONTENT_TYPE, "text/event-stream")
            .header(header::CACHE_CONTROL, "no-cache")
            .body(Body::from_stream(body))
            .unwrap_or_else(|_| StatusCode::BAD_GATEWAY.into_response()),
    }
}

/// Serve a chat endpoint of `format`: authenticate, run the use case, and log
/// the outcome.
async fn chat(
    format: Format,
    endpoint: &'static str,
    app: &AppState,
    headers: &HeaderMap,
    body: &Bytes,
) -> Response {
    let anthropic = format == Format::Anthropic;
    let state = app.snapshot().await;
    let client_key = match proxy::authenticate(&state, extract_client_key(headers)) {
        Ok(k) => k,
        Err(e) => return error_response(&e, anthropic),
    };
    let account_alias = match extract_account_alias(headers) {
        Ok(alias) => alias,
        Err(e) => return error_response(&e, anthropic),
    };
    let session_id = extract_session_id(headers);
    let request = ChatRequest {
        format,
        endpoint,
        body,
        client_key: client_key.as_deref(),
        account_alias,
        session_id: &session_id,
        hints: extract_hints(headers),
    };
    match proxy::handle_chat(app, request).await {
        Ok(reply) => {
            let response = reply_response(reply);
            tracing::info!(
                target: crate::LOG_TARGET,
                endpoint,
                status = response.status().as_u16(),
                "request completed"
            );
            response
        }
        Err(e) => {
            tracing::warn!(
                target: crate::LOG_TARGET,
                endpoint,
                status = e.status,
                kind = %e.kind,
                message = %e.message,
                "request failed"
            );
            error_response(&e, anthropic)
        }
    }
}

pub(super) async fn messages(
    State(app): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    chat(Format::Anthropic, "/v1/messages", &app, &headers, &body).await
}

pub(super) async fn chat_completions(
    State(app): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    chat(
        Format::Openai,
        "/v1/chat/completions",
        &app,
        &headers,
        &body,
    )
    .await
}

pub(super) async fn responses(
    State(app): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    chat(Format::Responses, "/v1/responses", &app, &headers, &body).await
}

pub(super) async fn models(State(app): State<AppState>, headers: HeaderMap) -> Response {
    let state = app.snapshot().await;
    let models = state.router.list_models();
    tracing::info!(
        target: crate::LOG_TARGET,
        endpoint = "/v1/models",
        count = models.len(),
        "serving model list"
    );
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

pub(super) async fn count_tokens(
    State(app): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let state = app.snapshot().await;
    if let Err(e) = proxy::authenticate(&state, extract_client_key(&headers)) {
        return error_response(&e, true);
    }
    serde_json::from_slice::<Value>(&body).map_or_else(
        |_| {
            let e = ApiError::bad_request("invalid JSON body");
            tracing::warn!(
                target: crate::LOG_TARGET,
                endpoint = "/v1/messages/count_tokens",
                status = e.status,
                kind = %e.kind,
                message = %e.message,
                "request failed"
            );
            error_response(&e, true)
        },
        |body| {
            let n = estimate_tokens(&body);
            tracing::info!(
                target: crate::LOG_TARGET,
                endpoint = "/v1/messages/count_tokens",
                input_tokens = n,
                "counted tokens"
            );
            json_response(StatusCode::OK, json!({"input_tokens": n}))
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_key_comes_from_x_api_key_or_bearer() {
        let mut headers = HeaderMap::new();
        assert_eq!(extract_client_key(&headers), None);
        headers.insert(header::AUTHORIZATION, "Bearer sk-bearer".parse().unwrap());
        assert_eq!(extract_client_key(&headers).as_deref(), Some("sk-bearer"));
        headers.insert("x-api-key", "sk-proxy".parse().unwrap());
        assert_eq!(extract_client_key(&headers).as_deref(), Some("sk-proxy"));
    }

    #[test]
    fn invalid_account_header_is_rejected() {
        let mut headers = HeaderMap::new();
        assert_eq!(extract_account_alias(&headers).unwrap(), None);
        headers.insert("x-local-proxy-account", "work".parse().unwrap());
        assert_eq!(extract_account_alias(&headers).unwrap(), Some("work"));
        headers.insert(
            "x-local-proxy-account",
            axum::http::HeaderValue::from_bytes(b"\xff").unwrap(),
        );
        assert_eq!(extract_account_alias(&headers).unwrap_err().status, 400);
    }

    #[test]
    fn hints_carry_user_agent_and_betas() {
        let mut headers = HeaderMap::new();
        headers.insert(header::USER_AGENT, "claude-cli/9.9.9".parse().unwrap());
        headers.insert("anthropic-beta", "a,b".parse().unwrap());
        let hints = extract_hints(&headers);
        assert_eq!(hints.user_agent.as_deref(), Some("claude-cli/9.9.9"));
        assert_eq!(hints.anthropic_beta.as_deref(), Some("a,b"));
    }

    #[test]
    fn local_replies_are_ok_json() {
        let reply = ChatReply::Json(crate::domain::exec::local_reply(
            Format::Anthropic,
            "hello\nworld",
            "gpt-4o",
        ));
        assert_eq!(reply_response(reply).status(), StatusCode::OK);
    }
}
