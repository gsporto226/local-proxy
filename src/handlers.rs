use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::sse::{KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Json;
use axum::Router as AxumRouter;
use futures_util::Stream;
use miette::Diagnostic;
use notify::RecursiveMode;
use serde_json::{json, Value};
use thiserror::Error;
use tokio::sync::RwLock;

use crate::config::{Config, ConfigError, ProviderFormat};
use crate::error::ApiError;
use crate::router::{Router, RouterError};
use crate::stats::{self, StatLine};
use crate::streams::{self, StreamCapture, UpstreamStream};
use crate::translate;
use crate::upstream::{send_and_read, ProviderClient, UpstreamError};

/// The current, hot-reloadable runtime state shared by the HTTP handlers.
#[derive(Clone)]
pub struct RuntimeState {
    /// The effective (catalog-merged) configuration.
    pub config: Arc<Config>,
    /// The model/router resolution logic.
    pub router: Arc<Router>,
    /// The built upstream clients, keyed by provider name.
    pub clients: Arc<HashMap<String, ProviderClient>>,
}

/// Shared application state threaded through the axum handlers.
#[derive(Clone)]
pub struct AppState {
    inner: Arc<RwLock<RuntimeState>>,
}

impl AppState {
    /// Wrap a [`RuntimeState`] in shared, lockable application state.
    #[must_use]
    pub fn new(state: RuntimeState) -> Self {
        Self {
            inner: Arc::new(RwLock::new(state)),
        }
    }

    /// Snapshot the current runtime state (cheap Arc clones).
    pub async fn snapshot(&self) -> RuntimeState {
        self.inner.read().await.clone()
    }
}

/// Errors while (re)building the runtime state.
#[derive(Debug, Error, Diagnostic)]
pub enum RuntimeError {
    /// The config overlay could not be loaded.
    #[error("failed to load config {path}: {source}")]
    #[diagnostic(code(runtime::config))]
    Config {
        /// Path of the config file.
        path: String,
        /// Underlying config error.
        #[source]
        source: ConfigError,
    },
    /// The router could not be built.
    #[error("failed to build router: {0}")]
    #[diagnostic(code(runtime::router))]
    Router(#[source] RouterError),
    /// The upstream clients could not be built.
    #[error("failed to build upstream clients: {0}")]
    #[diagnostic(code(runtime::clients))]
    Clients(#[source] UpstreamError),
    /// The file watcher could not be created or started.
    #[error("failed to start config watcher: {message}")]
    #[diagnostic(code(runtime::watcher))]
    Watcher {
        /// Underlying watcher error message.
        message: String,
    },
}

/// Build the effective runtime state for `config_path` by loading the overlay,
/// merging it with the embedded catalog, and building the router and clients.
///
/// # Errors
///
/// Returns a [`RuntimeError`] if the config, router, or clients fail to build.
#[allow(clippy::result_large_err)]
pub fn build_runtime_state(config_path: &Path) -> Result<RuntimeState, RuntimeError> {
    let overlay = if config_path.exists() {
        Config::load(config_path).map_err(|source| RuntimeError::Config {
            path: config_path.display().to_string(),
            source,
        })?
    } else {
        Config::default()
    };
    let base = crate::catalog::load().map_err(|source| RuntimeError::Config {
        path: "<catalog>".to_string(),
        source,
    })?;
    let config = Arc::new(crate::catalog::effective_config(base, overlay));
    let router = Arc::new(Router::new(config.clone()).map_err(RuntimeError::Router)?);
    let clients = Arc::new(build_clients(&config).map_err(RuntimeError::Clients)?);
    Ok(RuntimeState {
        config,
        router,
        clients,
    })
}

/// Spawn a file-watcher task that rebuilds `state` when the config or auth file
/// changes (hot-reload without restart). Watches the global config dir and the
/// parent of `config_path` when they differ.
///
/// # Errors
///
/// Returns an error if the file watcher cannot be created or started.
#[allow(clippy::result_large_err)]
pub fn spawn_watcher(config_path: PathBuf, app_state: &AppState) -> Result<(), RuntimeError> {
    let (tx, rx) = std::sync::mpsc::channel();
    let mut debouncer = notify_debouncer_full::new_debouncer(
        Duration::from_millis(300),
        None,
        move |result: notify_debouncer_full::DebounceEventResult| {
            if result.is_ok() {
                let _ = tx.send(());
            }
        },
    )
    .map_err(|e| RuntimeError::Watcher {
        message: format!("{e}"),
    })?;

    let mut dirs = vec![crate::config::global_config_dir()];
    if let Some(parent) = config_path.parent() {
        let parent = parent.to_path_buf();
        if !dirs.contains(&parent) {
            dirs.push(parent);
        }
    }
    for dir in dirs {
        if dir.exists() {
            debouncer
                .watch(&dir, RecursiveMode::NonRecursive)
                .map_err(|e| RuntimeError::Watcher {
                    message: format!("failed to watch {}: {e}", dir.display()),
                })?;
        }
    }

    let state = app_state.inner.clone();
    let (btx, mut brx) = tokio::sync::mpsc::unbounded_channel::<()>();
    tokio::task::spawn_blocking(move || {
        while rx.recv().is_ok() {
            let _ = btx.send(());
        }
    });
    tokio::spawn(async move {
        let _debouncer = debouncer;
        while brx.recv().await.is_some() {
            match build_runtime_state(&config_path) {
                Ok(new_state) => {
                    *state.write().await = new_state;
                    tracing::info!("config/auth change applied (hot-reload)");
                }
                Err(e) => tracing::warn!("hot-reload rebuild failed: {e}"),
            }
        }
    });
    Ok(())
}

/// Build an upstream [`ProviderClient`] for every configured provider.
///
/// # Errors
///
/// Returns an error if any provider client fails to build.
pub fn build_clients(config: &Config) -> Result<HashMap<String, ProviderClient>, UpstreamError> {
    let passthrough = config.server.passthrough_keys;
    let auth = crate::auth::read_auth().unwrap_or_default();
    let mut map = HashMap::new();
    for provider in &config.providers {
        let auth_key = auth.get(&provider.name).map(|e| e.key.clone());
        map.insert(
            provider.name.clone(),
            ProviderClient::new(provider, passthrough, auth_key)?,
        );
    }
    Ok(map)
}

/// Build the axum [`AxumRouter`] wiring up all routes with the given state.
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
fn authenticate(state: &RuntimeState, headers: &HeaderMap) -> Result<Option<String>, ApiError> {
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

/// A same-format SSE body stream that tees raw bytes to the client while
/// scanning them for cumulative token usage, recording stats when the stream
/// ends. The client bytes are forwarded unchanged.
struct ScannedClientStream {
    inner: Pin<Box<dyn Stream<Item = Result<axum::body::Bytes, reqwest::Error>> + Send>>,
    capture: Option<StreamCapture>,
    buf: String,
    usage: translate::TokenUsage,
    recorded: bool,
}

impl Stream for ScannedClientStream {
    type Item = Result<axum::body::Bytes, reqwest::Error>;

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let this = &mut *self;
        match this.inner.as_mut().poll_next(cx) {
            std::task::Poll::Ready(Some(Ok(bytes))) => {
                for frame in crate::sse::feed_frames(&mut this.buf, &bytes) {
                    if let Some(v) = frame.json() {
                        let part = translate::usage_from_frame(&v);
                        if part != translate::TokenUsage::default() {
                            translate::merge_usage(&mut this.usage, part);
                        }
                    }
                }
                std::task::Poll::Ready(Some(Ok(bytes)))
            }
            std::task::Poll::Ready(Some(Err(e))) => std::task::Poll::Ready(Some(Err(e))),
            std::task::Poll::Ready(None) => {
                if let Some(f) = crate::sse::flush_frames(&mut this.buf) {
                    if let Some(v) = f.json() {
                        let part = translate::usage_from_frame(&v);
                        translate::merge_usage(&mut this.usage, part);
                    }
                }
                if !this.recorded {
                    this.recorded = true;
                    if let Some(c) = &this.capture {
                        c.record(this.usage);
                    }
                }
                std::task::Poll::Ready(None)
            }
            std::task::Poll::Pending => std::task::Poll::Pending,
        }
    }
}

/// Forward a same-format upstream SSE response verbatim, scanning for usage.
fn passthrough_stream(resp: reqwest::Response, capture: Option<StreamCapture>) -> Response {
    let status = resp.status();
    let inner: Pin<Box<dyn Stream<Item = Result<axum::body::Bytes, reqwest::Error>> + Send>> =
        Box::pin(resp.bytes_stream());
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .body(Body::from_stream(ScannedClientStream {
            inner,
            capture,
            buf: String::new(),
            usage: translate::TokenUsage::default(),
            recorded: false,
        }))
        .unwrap_or_else(|_| StatusCode::BAD_GATEWAY.into_response())
}

fn resolve_model(state: &RuntimeState) -> Result<(Arc<crate::config::Provider>, String), ApiError> {
    // The proxy never uses the model requested by the harness. It routes through
    // the explicitly selected active model, or the first model available from a
    // connected provider, or errors if no model is available.
    let requested = if let Some(model) = state.config.defaults.active_model.clone() {
        model
    } else {
        let first = state.config.providers.iter().find_map(|p| {
            let connected = state
                .clients
                .get(&p.name)
                .is_some_and(crate::upstream::ProviderClient::has_key);
            connected.then(|| p.models.first().cloned()).flatten()
        });
        first.ok_or_else(|| {
            ApiError::bad_request(
                "no model available; connect a provider or run `local-proxy model <model>`",
            )
        })?
    };
    let resolved = state
        .router
        .resolve_model(&requested)
        .map_err(ApiError::from)?;
    Ok((resolved.provider, resolved.upstream_model))
}

fn client_for(
    state: &RuntimeState,
    provider: &crate::config::Provider,
) -> Result<ProviderClient, ApiError> {
    state.clients.get(&provider.name).cloned().ok_or_else(|| {
        ApiError::internal(format!("client not built for provider {}", provider.name))
    })
}

/// Best-effort local statistics capture for a non-streaming request.
///
/// Extracts token usage from the upstream (already consumed) body where
/// possible, records the row against the local stats database, and ignores any
/// failure. The provider/model are the resolved upstream ones. Streaming
/// requests are recorded by [`StreamCapture`] once the SSE stream completes.
#[allow(clippy::needless_pass_by_value)]
fn capture(
    endpoint: &'static str,
    provider: &str,
    model: &str,
    streamed: bool,
    status: u16,
    upstream_body: Option<&Value>,
    started: &Instant,
) {
    let mut tokens = crate::translate::TokenUsage::default();
    if let Some(body) = upstream_body {
        if let Some(u) = body.get("usage") {
            tokens = crate::translate::parse_usage(u);
        }
    }
    stats::record(
        *started,
        StatLine {
            endpoint,
            provider: provider.to_string(),
            model: model.to_string(),
            input_tokens: tokens.input,
            output_tokens: tokens.output,
            streamed,
            status,
            error: status >= 400,
        },
    );
}

// ---------------------------------------------------------------------------
// /v1/messages (Anthropic client)
// ---------------------------------------------------------------------------

async fn messages_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let state = state.snapshot().await;
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
    state: &RuntimeState,
    body: &Bytes,
    client_key: Option<&str>,
) -> Result<Response, ApiError> {
    let started = Instant::now();
    let mut body = parse_body(body)?;
    let streaming = wants_stream(&body);
    let (provider, upstream_model) = resolve_model(state)?;
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
        capture(
            "/v1/messages",
            &provider.name,
            &upstream_model,
            streaming,
            status,
            Some(&rbody),
            &started,
        );
        return Err(ApiError::from_upstream(status, rbody));
    }
    if streaming {
        let cap = StreamCapture::new(
            "/v1/messages",
            &provider.name,
            &upstream_model,
            status,
            started,
        );
        return match provider.format {
            ProviderFormat::Anthropic => Ok(passthrough_stream(resp, Some(cap))),
            ProviderFormat::Openai => Ok(sse_response(streams::anthropic_from_openai(
                resp,
                upstream_model,
                Some(cap),
            ))),
        };
    }

    let rbody = resp.json::<Value>().await.unwrap_or(Value::Null);
    match provider.format {
        ProviderFormat::Anthropic => {
            capture(
                "/v1/messages",
                &provider.name,
                &upstream_model,
                false,
                status,
                Some(&rbody),
                &started,
            );
            Ok(json_response(StatusCode::OK, rbody))
        }
        ProviderFormat::Openai => {
            capture(
                "/v1/messages",
                &provider.name,
                &upstream_model,
                false,
                status,
                Some(&rbody),
                &started,
            );
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
    let state = state.snapshot().await;
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
    state: &RuntimeState,
    body: &Bytes,
    client_key: Option<&str>,
) -> Result<Response, ApiError> {
    let started = Instant::now();
    let mut body = parse_body(body)?;
    let streaming = wants_stream(&body);
    let (provider, upstream_model) = resolve_model(state)?;
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
        capture(
            "/v1/chat/completions",
            &provider.name,
            &upstream_model,
            streaming,
            status,
            Some(&rbody),
            &started,
        );
        return Err(ApiError::from_upstream(status, rbody));
    }
    if streaming {
        let cap = StreamCapture::new(
            "/v1/chat/completions",
            &provider.name,
            &upstream_model,
            status,
            started,
        );
        return match provider.format {
            ProviderFormat::Openai => Ok(passthrough_stream(resp, Some(cap))),
            ProviderFormat::Anthropic => Ok(sse_response(streams::openai_from_anthropic(
                resp,
                upstream_model,
                Some(cap),
            ))),
        };
    }

    let rbody = resp.json::<Value>().await.unwrap_or(Value::Null);
    match provider.format {
        ProviderFormat::Openai => {
            capture(
                "/v1/chat/completions",
                &provider.name,
                &upstream_model,
                false,
                status,
                Some(&rbody),
                &started,
            );
            Ok(json_response(StatusCode::OK, rbody))
        }
        ProviderFormat::Anthropic => {
            capture(
                "/v1/chat/completions",
                &provider.name,
                &upstream_model,
                false,
                status,
                Some(&rbody),
                &started,
            );
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
    let state = state.snapshot().await;
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
    state: &RuntimeState,
    body: &Bytes,
    client_key: Option<&str>,
) -> Result<Response, ApiError> {
    let started = Instant::now();
    let mut body = parse_body(body)?;
    let streaming = wants_stream(&body);
    let (provider, upstream_model) = resolve_model(state)?;
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
        capture(
            "/v1/responses",
            &provider.name,
            &upstream_model,
            streaming,
            status,
            Some(&rbody),
            &started,
        );
        return Err(ApiError::from_upstream(status, rbody));
    }
    if streaming {
        let cap = StreamCapture::new(
            "/v1/responses",
            &provider.name,
            &upstream_model,
            status,
            started,
        );
        return match provider.format {
            ProviderFormat::Openai => Ok(sse_response(streams::responses_from_openai(
                resp,
                upstream_model,
                Some(cap),
            ))),
            ProviderFormat::Anthropic => Ok(sse_response(streams::responses_from_anthropic(
                resp,
                upstream_model,
                Some(cap),
            ))),
        };
    }

    let rbody = resp.json::<Value>().await.unwrap_or(Value::Null);
    capture(
        "/v1/responses",
        &provider.name,
        &upstream_model,
        false,
        status,
        Some(&rbody),
        &started,
    );
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
    let state = state.snapshot().await;
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
    let state = state.snapshot().await;
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
        let state = RuntimeState {
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
        let state = RuntimeState {
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

    #[test]
    fn rebuild_merges_catalog_with_overlay_and_reapplies() {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock is set")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "local-proxy-rebuild-{}-{stamp}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("create test dir");
        let path = dir.join("config.yaml");
        std::fs::write(
            &path,
            "providers:\n  - name: mylocal\n    base_url: http://127.0.0.1:9/v1\n    format: openai\n    models: [m]\n",
        )
        .expect("write config");

        let first = build_runtime_state(&path).expect("first build");
        assert!(first.config.providers.iter().any(|p| p.name == "mylocal"));
        assert!(first.config.providers.iter().any(|p| p.name == "anthropic"));

        // Simulate a hot-reload: the user edits the config file, adding a provider.
        std::fs::write(
            &path,
            "providers:\n  - name: mylocal\n    base_url: http://127.0.0.1:9/v1\n    format: openai\n    models: [m]\n  - name: second\n    base_url: http://127.0.0.1:9/v1\n    format: openai\n    models: [m]\n",
        )
        .expect("rewrite config");
        let second = build_runtime_state(&path).expect("rebuild");
        assert!(second.config.providers.iter().any(|p| p.name == "second"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn active_model_overrides_harness_model_in_routing() {
        let cfg = Arc::new(Config {
            server: crate::config::Server::default(),
            providers: vec![
                crate::config::Provider {
                    name: "openai".to_string(),
                    base_url: "https://api.openai.com/v1".to_string(),
                    api_key_env: None,
                    api_key: Some("sk".to_string()),
                    format: ProviderFormat::Openai,
                    models: vec!["gpt-4o".to_string()],
                },
                crate::config::Provider {
                    name: "anthropic".to_string(),
                    base_url: "https://api.anthropic.com".to_string(),
                    api_key_env: None,
                    api_key: None,
                    format: ProviderFormat::Anthropic,
                    models: vec!["claude-sonnet-4-5".to_string()],
                },
            ],
            routes: vec![crate::config::Route {
                model: "gpt-4o".to_string(),
                provider: "openai".to_string(),
                prefix: false,
                upstream_model: None,
            }],
            defaults: crate::config::Defaults {
                provider: "openai".to_string(),
                active_model: Some("gpt-4o".to_string()),
            },
        });
        let state = RuntimeState {
            config: cfg.clone(),
            router: Arc::new(Router::new(cfg).unwrap()),
            clients: Arc::new(HashMap::new()),
        };

        // The harness asks for an unknown model, but active_model forces gpt-4o.
        let (provider, upstream) = resolve_model(&state).expect("resolves");
        assert_eq!(provider.name, "openai");
        assert_eq!(upstream, "gpt-4o");
    }

    #[test]
    fn no_active_model_uses_first_connected_model() {
        let cfg = Arc::new(Config {
            server: crate::config::Server::default(),
            providers: vec![crate::config::Provider {
                name: "openai".to_string(),
                base_url: "https://api.openai.com/v1".to_string(),
                api_key_env: None,
                api_key: Some("sk".to_string()),
                format: ProviderFormat::Openai,
                models: vec!["gpt-4o".to_string(), "gpt-4o-mini".to_string()],
            }],
            routes: Vec::new(),
            defaults: crate::config::Defaults {
                provider: "openai".to_string(),
                active_model: None,
            },
        });
        let state = RuntimeState {
            config: cfg.clone(),
            router: Arc::new(Router::new(cfg.clone()).unwrap()),
            clients: Arc::new(build_clients(&cfg).expect("clients")),
        };
        let (provider, upstream) = resolve_model(&state).expect("resolves");
        assert_eq!(provider.name, "openai");
        assert_eq!(upstream, "gpt-4o");
    }

    #[test]
    fn no_model_available_errors() {
        let cfg = Arc::new(Config {
            server: crate::config::Server::default(),
            providers: vec![crate::config::Provider {
                name: "openai".to_string(),
                base_url: "https://api.openai.com/v1".to_string(),
                api_key_env: None,
                api_key: None,
                format: ProviderFormat::Openai,
                models: vec!["gpt-4o".to_string()],
            }],
            routes: Vec::new(),
            defaults: crate::config::Defaults {
                provider: "openai".to_string(),
                active_model: None,
            },
        });
        let state = RuntimeState {
            config: cfg.clone(),
            router: Arc::new(Router::new(cfg).unwrap()),
            clients: Arc::new(HashMap::new()),
        };
        assert!(resolve_model(&state).is_err());
    }
}
