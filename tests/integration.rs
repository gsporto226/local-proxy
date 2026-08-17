//! End-to-end integration tests: a real mock upstream (axum + SSE) behind the
//! proxy, exercised through `tower::ServiceExt::oneshot` (no proxy TCP socket).

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::http::{Method, Request, StatusCode};
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use futures_util::stream;
use serde_json::{json, Value};
use tower::ServiceExt;

use local_proxy::config::{Config, Defaults, Provider, ProviderFormat, Route, Server};
use local_proxy::handlers::{self, AppState};
use local_proxy::router::Router as ProxyRouter;

// ---------------------------------------------------------------------------
// mock upstream
// ---------------------------------------------------------------------------

async fn mock_chat(body: Bytes) -> Response {
    let v: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    if v.get("model").and_then(Value::as_str) == Some("gpt-error") {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": {"message": "bad key", "type": "authentication_error", "code": "invalid_api_key"}})),
        )
            .into_response();
    }
    if v.get("stream").and_then(Value::as_bool) == Some(true) {
        let chunks = vec![
            json!({"id":"cmpl_1","object":"chat.completion.chunk","created":1,"model":"gpt-4o","choices":[{"index":0,"delta":{"role":"assistant","content":""},"finish_reason":null}]}),
            json!({"id":"cmpl_1","object":"chat.completion.chunk","created":1,"model":"gpt-4o","choices":[{"index":0,"delta":{"content":"Hel"},"finish_reason":null}]}),
            json!({"id":"cmpl_1","object":"chat.completion.chunk","created":1,"model":"gpt-4o","choices":[{"index":0,"delta":{"content":"lo"},"finish_reason":null}]}),
            json!({"id":"cmpl_1","object":"chat.completion.chunk","created":1,"model":"gpt-4o","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":2,"total_tokens":5}}),
        ];
        let mut events: Vec<Event> = chunks
            .into_iter()
            .map(|c| Event::default().json_data(c).unwrap())
            .collect();
        events.push(Event::default().data("[DONE]"));
        Sse::new(stream::iter(events.into_iter().map(Ok::<_, Infallible>))).into_response()
    } else {
        (
            Json(json!({
                "id": "cmpl_1", "object": "chat.completion", "created": 1, "model": "gpt-4o",
                "choices": [{"index": 0, "message": {"role": "assistant", "content": "hi"}, "finish_reason": "stop", "logprobs": null}],
                "usage": {"prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5}
            })),
        )
            .into_response()
    }
}

async fn mock_messages(body: Bytes) -> Response {
    let v: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    if v.get("stream").and_then(Value::as_bool) == Some(true) {
        let events = vec![
            Event::default().event("message_start").json_data(json!({
                "type": "message_start",
                "message": {"id": "msg_1", "type": "message", "role": "assistant", "model": "claude-sonnet-4-5",
                            "content": [], "stop_reason": null, "stop_sequence": null,
                            "usage": {"input_tokens": 2, "output_tokens": 0}}
            })).unwrap(),
            Event::default().event("content_block_start").json_data(json!({
                "type": "content_block_start", "index": 0,
                "content_block": {"type": "text", "text": ""}
            })).unwrap(),
            Event::default().event("content_block_delta").json_data(json!({
                "type": "content_block_delta", "index": 0,
                "delta": {"type": "text_delta", "text": "oi"}
            })).unwrap(),
            Event::default().event("content_block_stop").json_data(json!({
                "type": "content_block_stop", "index": 0
            })).unwrap(),
            Event::default().event("message_delta").json_data(json!({
                "type": "message_delta",
                "delta": {"stop_reason": "end_turn", "stop_sequence": null},
                "usage": {"output_tokens": 2}
            })).unwrap(),
            Event::default().event("message_stop").json_data(json!({"type": "message_stop"})).unwrap(),
        ];
        Sse::new(stream::iter(events.into_iter().map(Ok::<_, Infallible>))).into_response()
    } else {
        (Json(json!({
            "id": "msg_1", "type": "message", "role": "assistant", "model": "claude-sonnet-4-5",
            "content": [{"type": "text", "text": "hi"}],
            "stop_reason": "end_turn", "stop_sequence": null,
            "usage": {"input_tokens": 2, "output_tokens": 2}
        })),)
            .into_response()
    }
}

async fn spawn_mock() -> SocketAddr {
    let app = Router::new()
        .route("/v1/chat/completions", post(mock_chat))
        .route("/v1/messages", post(mock_messages));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    addr
}

// ---------------------------------------------------------------------------
// proxy harness
// ---------------------------------------------------------------------------

fn proxy_app(mock: SocketAddr) -> Router {
    let base = format!("http://{mock}");
    let config = Arc::new(Config {
        server: Server::default(),
        providers: vec![
            Provider {
                name: "mock_openai".to_string(),
                base_url: base.clone(),
                api_key_env: None,
                format: ProviderFormat::Openai,
                models: vec!["gpt-4o".to_string()],
            },
            Provider {
                name: "mock_anthropic".to_string(),
                base_url: base,
                api_key_env: None,
                format: ProviderFormat::Anthropic,
                models: vec!["claude-sonnet-4-5".to_string()],
            },
        ],
        routes: vec![
            Route {
                model: "claude-via-openai".to_string(),
                provider: "mock_openai".to_string(),
                prefix: false,
                upstream_model: Some("gpt-4o".to_string()),
            },
            Route {
                model: "gpt-via-anthropic".to_string(),
                provider: "mock_anthropic".to_string(),
                prefix: false,
                upstream_model: Some("claude-sonnet-4-5".to_string()),
            },
            Route {
                model: "err".to_string(),
                provider: "mock_openai".to_string(),
                prefix: false,
                upstream_model: Some("gpt-error".to_string()),
            },
        ],
        defaults: Defaults {
            provider: String::new(),
        },
    });
    let router = Arc::new(ProxyRouter::new(config.clone()).unwrap());
    let clients = Arc::new(handlers::build_clients(&config).unwrap());
    handlers::app(AppState {
        config,
        router,
        clients,
    })
}

async fn send(app: &Router, path: &str, body: Value) -> (StatusCode, Bytes) {
    let request = Request::builder()
        .method(Method::POST)
        .uri(path)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, bytes)
}

/// Parse an SSE body into `(event_name, data)` frames.
fn frames_from(bytes: &[u8]) -> Vec<(Option<String>, String)> {
    let text = String::from_utf8_lossy(bytes);
    let mut frames = Vec::new();
    for chunk in text.split("\n\n") {
        let mut event = None;
        let mut data = String::new();
        for line in chunk.lines() {
            if let Some(r) = line.strip_prefix("event:") {
                event = Some(r.trim().to_string());
            } else if let Some(r) = line.strip_prefix("data:") {
                data = r.trim_start().to_string();
            }
        }
        if !data.is_empty() {
            frames.push((event, data));
        }
    }
    frames
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn messages_stream_via_openai_upstream_translates_to_anthropic_events() {
    let mock = spawn_mock().await;
    let app = proxy_app(mock);
    let (status, bytes) = send(
        &app,
        "/v1/messages",
        json!({"model": "claude-via-openai", "max_tokens": 10, "messages": [{"role": "user", "content": "hi"}], "stream": true}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let frames = frames_from(&bytes);
    let events: Vec<&str> = frames.iter().filter_map(|(e, _)| e.as_deref()).collect();
    assert_eq!(
        events,
        [
            "message_start",
            "content_block_start",
            "content_block_delta",
            "content_block_delta",
            "content_block_stop",
            "message_delta",
            "message_stop",
        ]
    );
    let deltas: Vec<String> = frames
        .iter()
        .filter(|(e, _)| e.as_deref() == Some("content_block_delta"))
        .filter_map(|(_, d)| {
            serde_json::from_str::<Value>(d)
                .ok()
                .and_then(|v| v["delta"]["text"].as_str().map(str::to_string))
        })
        .collect();
    assert_eq!(deltas, ["Hel", "lo"]);
    let md = frames
        .iter()
        .find(|(e, _)| e.as_deref() == Some("message_delta"))
        .unwrap();
    let md: Value = serde_json::from_str(&md.1).unwrap();
    assert_eq!(md["delta"]["stop_reason"], "end_turn");
    assert_eq!(md["usage"]["input_tokens"], 3);
    assert_eq!(md["usage"]["output_tokens"], 2);
}

#[tokio::test]
async fn chat_completions_stream_via_anthropic_upstream_translates_to_openai_chunks() {
    let mock = spawn_mock().await;
    let app = proxy_app(mock);
    let (status, bytes) = send(
        &app,
        "/v1/chat/completions",
        json!({"model": "gpt-via-anthropic", "messages": [{"role": "user", "content": "hi"}], "stream": true}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let frames = frames_from(&bytes);
    assert!(frames.iter().any(|(e, d)| e.is_none() && d == "[DONE]"));

    let chunks: Vec<Value> = frames
        .iter()
        .filter(|(e, _)| e.is_none())
        .filter_map(|(_, d)| serde_json::from_str(d).ok())
        .collect();
    let first = &chunks[0];
    assert_eq!(first["choices"][0]["delta"]["role"], "assistant");
    let contents: Vec<&str> = chunks
        .iter()
        .filter_map(|c| c["choices"][0]["delta"]["content"].as_str())
        .collect();
    assert_eq!(contents, ["oi"]);
    let finish = chunks
        .iter()
        .find(|c| c["choices"][0]["finish_reason"].is_string())
        .unwrap();
    assert_eq!(finish["choices"][0]["finish_reason"], "stop");
    assert_eq!(finish["usage"]["prompt_tokens"], 2);
    assert_eq!(finish["usage"]["completion_tokens"], 2);
}

#[tokio::test]
async fn responses_stream_via_openai_upstream_emits_full_event_sequence() {
    let mock = spawn_mock().await;
    let app = proxy_app(mock);
    let (status, bytes) = send(
        &app,
        "/v1/responses",
        json!({"model": "claude-via-openai", "input": [{"role": "user", "content": [{"type": "input_text", "text": "hi"}]}], "stream": true}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let frames = frames_from(&bytes);
    let events: Vec<&str> = frames.iter().filter_map(|(e, _)| e.as_deref()).collect();
    assert_eq!(
        events,
        [
            "response.created",
            "response.output_item.added",
            "response.content_part.added",
            "response.output_text.delta",
            "response.output_text.delta",
            "response.output_text.done",
            "response.output_item.done",
            "response.completed",
        ]
    );
    let completed: Value = serde_json::from_str(
        &frames
            .iter()
            .find(|(e, _)| e.as_deref() == Some("response.completed"))
            .unwrap()
            .1,
    )
    .unwrap();
    assert_eq!(completed["response"]["status"], "completed");
    assert_eq!(
        completed["response"]["output"][0]["content"][0]["text"],
        "Hello"
    );
}

#[tokio::test]
async fn responses_stream_via_anthropic_upstream_translates_anthropic_events() {
    let mock = spawn_mock().await;
    let app = proxy_app(mock);
    let (status, bytes) = send(
        &app,
        "/v1/responses",
        json!({"model": "gpt-via-anthropic", "input": [{"role": "user", "content": [{"type": "input_text", "text": "hi"}]}], "stream": true}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let frames = frames_from(&bytes);
    let events: Vec<&str> = frames.iter().filter_map(|(e, _)| e.as_deref()).collect();
    assert_eq!(
        events,
        [
            "response.created",
            "response.output_item.added",
            "response.content_part.added",
            "response.output_text.delta",
            "response.output_text.done",
            "response.output_item.done",
            "response.completed",
        ]
    );
}

#[tokio::test]
async fn messages_stream_to_anthropic_upstream_passes_through() {
    let mock = spawn_mock().await;
    let app = proxy_app(mock);
    let (status, bytes) = send(
        &app,
        "/v1/messages",
        json!({"model": "gpt-via-anthropic", "max_tokens": 5, "messages": [{"role": "user", "content": "hi"}], "stream": true}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let frames = frames_from(&bytes);
    let events: Vec<&str> = frames.iter().filter_map(|(e, _)| e.as_deref()).collect();
    assert!(events.contains(&"message_start"));
    assert!(events.contains(&"content_block_delta"));
    assert!(events.contains(&"message_stop"));
    // text must survive untouched
    let body = String::from_utf8_lossy(&bytes);
    assert!(body.contains("oi"));
}

#[tokio::test]
async fn non_streaming_messages_translates_openai_response() {
    let mock = spawn_mock().await;
    let app = proxy_app(mock);
    let (status, bytes) = send(
        &app,
        "/v1/messages",
        json!({"model": "claude-via-openai", "max_tokens": 10, "messages": [{"role": "user", "content": "hi"}]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["type"], "message");
    assert_eq!(v["content"][0]["text"], "hi");
    assert_eq!(v["stop_reason"], "end_turn");
}

#[tokio::test]
async fn non_streaming_chat_completions_translates_anthropic_response() {
    let mock = spawn_mock().await;
    let app = proxy_app(mock);
    let (status, bytes) = send(
        &app,
        "/v1/chat/completions",
        json!({"model": "gpt-via-anthropic", "messages": [{"role": "user", "content": "hi"}]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["choices"][0]["message"]["content"], "hi");
    assert_eq!(v["choices"][0]["finish_reason"], "stop");
}

#[tokio::test]
async fn upstream_error_is_reformatted_to_client_format() {
    let mock = spawn_mock().await;
    let app = proxy_app(mock);

    // Anthropic client /v1/messages -> OpenAI-shape upstream error -> Anthropic error shape
    let (status, bytes) = send(
        &app,
        "/v1/messages",
        json!({"model": "err", "max_tokens": 5, "messages": [{"role": "user", "content": "hi"}]}),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["type"], "error");
    assert_eq!(v["error"]["message"], "bad key");

    // OpenAI client /v1/chat/completions -> same upstream error -> OpenAI error shape
    let (status, bytes) = send(
        &app,
        "/v1/chat/completions",
        json!({"model": "err", "messages": [{"role": "user", "content": "hi"}]}),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    assert!(v.get("error").is_some());
    assert_eq!(v["error"]["message"], "bad key");
}

#[tokio::test]
async fn unknown_model_returns_not_found_in_client_format() {
    let mock = spawn_mock().await;
    let app = proxy_app(mock);

    let (status, bytes) = send(
        &app,
        "/v1/chat/completions",
        json!({"model": "nope", "messages": [{"role": "user", "content": "hi"}]}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["error"]["type"], "not_found_error");

    let (status, bytes) = send(
        &app,
        "/v1/messages",
        json!({"model": "nope", "max_tokens": 5, "messages": [{"role": "user", "content": "hi"}]}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["type"], "error");
    assert_eq!(v["error"]["type"], "not_found_error");
}

#[tokio::test]
async fn count_tokens_returns_estimate() {
    let mock = spawn_mock().await;
    let app = proxy_app(mock);
    let (status, bytes) = send(
        &app,
        "/v1/messages/count_tokens",
        json!({"model": "claude-via-openai", "messages": [{"role": "user", "content": "hello world how are you"}]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    assert!(v["input_tokens"].as_u64().unwrap() > 0);
}

#[tokio::test]
async fn models_endpoint_returns_catalog_per_format() {
    let mock = spawn_mock().await;
    let app = proxy_app(mock);
    let request = Request::builder()
        .method(Method::GET)
        .uri("/v1/models")
        .header("anthropic-version", "2023-06-01")
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    let ids: Vec<&str> = v["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|m| m["id"].as_str())
        .collect();
    assert!(ids.contains(&"claude-via-openai"));
    assert!(ids.contains(&"gpt-via-anthropic"));
    assert!(v["data"][0]["type"].as_str().is_some());

    let request = Request::builder()
        .method(Method::GET)
        .uri("/v1/models")
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["object"], "list");
    let ids: Vec<&str> = v["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|m| m["id"].as_str())
        .collect();
    assert!(ids.contains(&"claude-via-openai"));
}
