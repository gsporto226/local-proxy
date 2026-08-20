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
use crate::streams::{self, parse_energy_comment, StreamCapture, UpstreamStream};
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
    /// The config file this instance was started with (used by `$proxy model`
    /// to persist the selection).
    pub config_path: PathBuf,
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

    /// Set the in-memory `active_model` for this instance without touching any
    /// other running proxy. Persistence is handled separately by the caller.
    pub async fn set_active_model(&self, model: Option<String>) {
        let mut guard = self.inner.write().await;
        let mut new_config = (*guard.config).clone();
        new_config.defaults.active_model = model;
        guard.config = Arc::new(new_config);
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
        config_path: config_path.to_path_buf(),
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
                Ok(mut new_state) => {
                    // Preserve this instance's in-memory active model: a model
                    // write to the shared config must never leak to other
                    // running proxies via hot-reload. Everything else (providers,
                    // routes, auth) still reloads from the file.
                    let current_model = state.read().await.config.defaults.active_model.clone();
                    let mut cfg = (*new_state.config).clone();
                    cfg.defaults.active_model = current_model;
                    new_state.config = Arc::new(cfg);
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
    let mut connected = Vec::new();
    for provider in &config.providers {
        let auth_key = auth.get(&provider.name).map(|e| e.key.clone());
        let client = ProviderClient::new(provider, passthrough, auth_key)?;
        if client.has_key() {
            connected.push(provider.name.clone());
        }
        map.insert(provider.name.clone(), client);
    }
    tracing::info!(
        target: crate::LOG_TARGET,
        providers = config.providers.len(),
        connected = %connected.join(", "),
        "built upstream clients"
    );
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
        .layer(tower_http::trace::TraceLayer::new_for_http())
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
    energy: Option<translate::EnergyCost>,
    cost: Option<translate::EnergyCost>,
    recorded: bool,
}

impl ScannedClientStream {
    fn absorb(&mut self, frame: &crate::sse::SseFrame) {
        if let Some(v) = frame.json() {
            let part = translate::usage_from_frame(&v);
            if part != translate::TokenUsage::default() {
                translate::merge_usage(&mut self.usage, part);
            }
        }
        for comment in &frame.comments {
            if let Some((e, c)) = parse_energy_comment(comment) {
                if e.is_some() {
                    self.energy = e;
                }
                if c.is_some() {
                    self.cost = c;
                }
            }
        }
    }
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
                    this.absorb(&frame);
                }
                std::task::Poll::Ready(Some(Ok(bytes)))
            }
            std::task::Poll::Ready(Some(Err(e))) => std::task::Poll::Ready(Some(Err(e))),
            std::task::Poll::Ready(None) => {
                if let Some(f) = crate::sse::flush_frames(&mut this.buf) {
                    this.absorb(&f);
                }
                if !this.recorded {
                    this.recorded = true;
                    if let Some(c) = &this.capture {
                        c.record(this.usage, this.energy, this.cost);
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
            energy: None,
            cost: None,
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
/// requests are recorded by [`StreamCapture`] once the `SSE` stream completes.
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
    let (energy, cost) = upstream_body.map_or((None, None), |body| {
        if let Some(u) = body.get("usage") {
            tokens = crate::translate::parse_usage(u);
        }
        (
            crate::translate::energy_from_value(body),
            crate::translate::cost_from_value(body),
        )
    });
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
            energy,
            cost,
        },
    );
}

// ---------------------------------------------------------------------------
// $proxy local-command execution
// ---------------------------------------------------------------------------

/// The active model for a response, or `local-proxy` when none is selected.
#[must_use]
fn active_model_or_default(state: &RuntimeState) -> String {
    state
        .config
        .defaults
        .active_model
        .clone()
        .unwrap_or_else(|| "local-proxy".to_string())
}

/// Unix epoch seconds (for synthesized response timestamps).
#[must_use]
fn now_ts() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Run the `$proxy` command carried by `body`, if any, returning its captured
/// output. Returns `None` when exec is disabled or the request is not a
/// `$proxy` command.
async fn maybe_exec(
    app: &AppState,
    state: &RuntimeState,
    body: &Value,
) -> Option<crate::exec::ExecOutput> {
    let exec = &state.config.exec;
    if !exec.enabled {
        return None;
    }
    let text = crate::exec::request_text(body)?;
    let cmd = crate::exec::split_command(&text, &exec.token)?;
    let args = crate::exec::parse_args(cmd);
    if args.first().map(String::as_str) == Some("model") {
        Some(handle_model_exec(app, state, &args).await)
    } else {
        Some(crate::exec::run(&exec.command, &args, Duration::from_secs(exec.timeout_secs)).await)
    }
}

/// Handle `$proxy model ...` in-process: report this instance's in-memory
/// model, or validate/persist via the CLI logic and update the in-memory
/// `active_model` (per-instance, no broadcast to other running proxies).
async fn handle_model_exec(
    app: &AppState,
    state: &RuntimeState,
    args: &[String],
) -> crate::exec::ExecOutput {
    let selection = args.get(1).map(String::as_str);
    let stdout = match selection {
        None => state.config.defaults.active_model.as_deref().map_or_else(
            || "nenhum modelo ativo".to_string(),
            |m| format!("modelo ativo: {m}"),
        ),
        Some("clear") => {
            let msg = crate::cli::model_result(&state.config_path, Some("clear"))
                .unwrap_or_else(|e| e.to_string());
            app.set_active_model(None).await;
            msg
        }
        Some(selected) => match crate::cli::model_result(&state.config_path, Some(selected)) {
            Ok(msg) => {
                if msg.starts_with("modelo ativo:") {
                    app.set_active_model(Some(selected.to_string())).await;
                }
                msg
            }
            Err(e) => {
                return crate::exec::ExecOutput {
                    stdout: String::new(),
                    stderr: e.to_string(),
                    code: 1,
                    timed_out: false,
                };
            }
        },
    };
    crate::exec::ExecOutput {
        stdout,
        stderr: String::new(),
        code: 0,
        timed_out: false,
    }
}

/// Synthesize an `Anthropic` Messages response carrying `$proxy` output.
fn exec_messages_response(text: &str, model: &str) -> Response {
    json_response(
        StatusCode::OK,
        json!({
            "id": "msg_local-proxy",
            "type": "message",
            "role": "assistant",
            "model": model,
            "content": [{"type": "text", "text": text}],
            "stop_reason": "end_turn",
            "stop_sequence": null,
            "usage": crate::translate::anthropic_usage(&crate::translate::TokenUsage::default())
        }),
    )
}

/// Synthesize an `OpenAI` chat-completions response carrying `$proxy` output.
fn exec_chat_response(text: &str, model: &str) -> Response {
    json_response(
        StatusCode::OK,
        json!({
            "id": "chatcmpl-local-proxy",
            "object": "chat.completion",
            "created": now_ts(),
            "model": model,
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": text},
                "finish_reason": "stop",
                "logprobs": null
            }],
            "usage": crate::translate::openai_usage(&crate::translate::TokenUsage::default()),
            "system_fingerprint": null
        }),
    )
}

/// Synthesize an `OpenAI` Responses response carrying `$proxy` output.
fn exec_responses_response(text: &str, model: &str) -> Response {
    json_response(
        StatusCode::OK,
        json!({
            "id": "resp_local-proxy",
            "object": "response",
            "created_at": now_ts(),
            "status": "completed",
            "model": model,
            "output": [{
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": text, "annotations": []}]
            }],
            "parallel_tool_calls": true,
            "usage": crate::translate::responses_usage(&crate::translate::TokenUsage::default())
        }),
    )
}

// ---------------------------------------------------------------------------
// /v1/messages (Anthropic client)
// ---------------------------------------------------------------------------

async fn messages_handler(
    State(app): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let state = app.snapshot().await;
    let client_key = match authenticate(&state, &headers) {
        Ok(k) => k,
        Err(e) => return error_response(&e, true),
    };
    match handle_messages(&app, &state, &body, client_key.as_deref()).await {
        Ok(r) => {
            tracing::info!(
                target: crate::LOG_TARGET,
                endpoint = "/v1/messages",
                status = r.status().as_u16(),
                "request completed"
            );
            r
        }
        Err(e) => {
            tracing::warn!(
                target: crate::LOG_TARGET,
                endpoint = "/v1/messages",
                status = e.status,
                kind = %e.kind,
                message = %e.message,
                "request failed"
            );
            error_response(&e, true)
        }
    }
}

#[allow(clippy::too_many_lines)]
async fn handle_messages(
    app: &AppState,
    state: &RuntimeState,
    body: &Bytes,
    client_key: Option<&str>,
) -> Result<Response, ApiError> {
    let started = Instant::now();
    let mut body = parse_body(body)?;

    if let Some(out) = maybe_exec(app, state, &body).await {
        let text = crate::exec::format_output(&out);
        let model = active_model_or_default(state);
        tracing::info!(
            target: crate::LOG_TARGET,
            endpoint = "/v1/messages",
            "handled $proxy command"
        );
        return Ok(exec_messages_response(&text, &model));
    }

    let streaming = wants_stream(&body);
    let (provider, upstream_model) = resolve_model(state)?;
    tracing::info!(
        target: crate::LOG_TARGET,
        endpoint = "/v1/messages",
        provider = %provider.name,
        upstream_model,
        streaming,
        "resolved route"
    );
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
        tracing::warn!(
            target: crate::LOG_TARGET,
            endpoint = "/v1/messages",
            provider = %provider.name,
            status,
            body = %rbody,
            "upstream returned error"
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
    State(app): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let state = app.snapshot().await;
    let client_key = match authenticate(&state, &headers) {
        Ok(k) => k,
        Err(e) => return error_response(&e, false),
    };
    match handle_chat_completions(&app, &state, &body, client_key.as_deref()).await {
        Ok(r) => {
            tracing::info!(
                target: crate::LOG_TARGET,
                endpoint = "/v1/chat/completions",
                status = r.status().as_u16(),
                "request completed"
            );
            r
        }
        Err(e) => {
            tracing::warn!(
                target: crate::LOG_TARGET,
                endpoint = "/v1/chat/completions",
                status = e.status,
                kind = %e.kind,
                message = %e.message,
                "request failed"
            );
            error_response(&e, false)
        }
    }
}

#[allow(clippy::too_many_lines)]
async fn handle_chat_completions(
    app: &AppState,
    state: &RuntimeState,
    body: &Bytes,
    client_key: Option<&str>,
) -> Result<Response, ApiError> {
    let started = Instant::now();
    let mut body = parse_body(body)?;

    if let Some(out) = maybe_exec(app, state, &body).await {
        let text = crate::exec::format_output(&out);
        let model = active_model_or_default(state);
        tracing::info!(
            target: crate::LOG_TARGET,
            endpoint = "/v1/chat/completions",
            "handled $proxy command"
        );
        return Ok(exec_chat_response(&text, &model));
    }

    let streaming = wants_stream(&body);
    let (provider, upstream_model) = resolve_model(state)?;
    tracing::info!(
        target: crate::LOG_TARGET,
        endpoint = "/v1/chat/completions",
        provider = %provider.name,
        upstream_model,
        streaming,
        "resolved route"
    );
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
        tracing::warn!(
            target: crate::LOG_TARGET,
            endpoint = "/v1/chat/completions",
            provider = %provider.name,
            status,
            body = %rbody,
            "upstream returned error"
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
    State(app): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let state = app.snapshot().await;
    let client_key = match authenticate(&state, &headers) {
        Ok(k) => k,
        Err(e) => return error_response(&e, false),
    };
    match handle_responses(&app, &state, &body, client_key.as_deref()).await {
        Ok(r) => {
            tracing::info!(
                target: crate::LOG_TARGET,
                endpoint = "/v1/responses",
                status = r.status().as_u16(),
                "request completed"
            );
            r
        }
        Err(e) => {
            tracing::warn!(
                target: crate::LOG_TARGET,
                endpoint = "/v1/responses",
                status = e.status,
                kind = %e.kind,
                message = %e.message,
                "request failed"
            );
            error_response(&e, false)
        }
    }
}

#[allow(clippy::too_many_lines)]
async fn handle_responses(
    app: &AppState,
    state: &RuntimeState,
    body: &Bytes,
    client_key: Option<&str>,
) -> Result<Response, ApiError> {
    let started = Instant::now();
    let mut body = parse_body(body)?;

    if let Some(out) = maybe_exec(app, state, &body).await {
        let text = crate::exec::format_output(&out);
        let model = active_model_or_default(state);
        tracing::info!(
            target: crate::LOG_TARGET,
            endpoint = "/v1/responses",
            "handled $proxy command"
        );
        return Ok(exec_responses_response(&text, &model));
    }

    let streaming = wants_stream(&body);
    let (provider, upstream_model) = resolve_model(state)?;
    tracing::info!(
        target: crate::LOG_TARGET,
        endpoint = "/v1/responses",
        provider = %provider.name,
        upstream_model,
        streaming,
        "resolved route"
    );
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
        tracing::warn!(
            target: crate::LOG_TARGET,
            endpoint = "/v1/responses",
            provider = %provider.name,
            status,
            body = %rbody,
            "upstream returned error"
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
            tracing::info!(
                target: crate::LOG_TARGET,
                endpoint = "/v1/messages/count_tokens",
                input_tokens = n,
                "counted tokens"
            );
            json_response(StatusCode::OK, json!({"input_tokens": n}))
        }
        Err(e) => {
            tracing::warn!(
                target: crate::LOG_TARGET,
                endpoint = "/v1/messages/count_tokens",
                status = e.status,
                kind = %e.kind,
                message = %e.message,
                "request failed"
            );
            error_response(&e, true)
        }
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
            exec: crate::config::Exec::default(),
        };
        let state = RuntimeState {
            config: Arc::new(cfg),
            router: Arc::new(Router::new(Arc::new(Config::default())).unwrap()),
            clients: Arc::new(HashMap::new()),
            config_path: PathBuf::new(),
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
            config_path: PathBuf::new(),
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
                    headers: std::collections::HashMap::new(),
                },
                crate::config::Provider {
                    name: "anthropic".to_string(),
                    base_url: "https://api.anthropic.com".to_string(),
                    api_key_env: None,
                    api_key: None,
                    format: ProviderFormat::Anthropic,
                    models: vec!["claude-sonnet-4-5".to_string()],
                    headers: std::collections::HashMap::new(),
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
            exec: crate::config::Exec::default(),
        });
        let state = RuntimeState {
            config: cfg.clone(),
            router: Arc::new(Router::new(cfg).unwrap()),
            clients: Arc::new(HashMap::new()),
            config_path: PathBuf::new(),
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
                headers: std::collections::HashMap::new(),
            }],
            routes: Vec::new(),
            defaults: crate::config::Defaults {
                provider: "openai".to_string(),
                active_model: None,
            },
            exec: crate::config::Exec::default(),
        });
        let state = RuntimeState {
            config: cfg.clone(),
            router: Arc::new(Router::new(cfg.clone()).unwrap()),
            clients: Arc::new(build_clients(&cfg).expect("clients")),
            config_path: PathBuf::new(),
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
                headers: std::collections::HashMap::new(),
            }],
            routes: Vec::new(),
            defaults: crate::config::Defaults {
                provider: "openai".to_string(),
                active_model: None,
            },
            exec: crate::config::Exec::default(),
        });
        let state = RuntimeState {
            config: cfg.clone(),
            router: Arc::new(Router::new(cfg).unwrap()),
            clients: Arc::new(HashMap::new()),
            config_path: PathBuf::new(),
        };
        assert!(resolve_model(&state).is_err());
    }

    fn block_on<F: std::future::Future>(fut: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(fut)
    }

    #[test]
    fn set_active_model_is_per_instance() {
        let app = AppState::new(RuntimeState {
            config: Arc::new(Config::default()),
            router: Arc::new(Router::new(Arc::new(Config::default())).unwrap()),
            clients: Arc::new(HashMap::new()),
            config_path: PathBuf::new(),
        });
        block_on(async {
            assert_eq!(app.snapshot().await.config.defaults.active_model, None);
            app.set_active_model(Some("gpt-4o".to_string())).await;
            assert_eq!(
                app.snapshot().await.config.defaults.active_model.as_deref(),
                Some("gpt-4o")
            );
        });
    }

    #[test]
    fn maybe_exec_returns_none_when_disabled() {
        let mut cfg = Config::default();
        cfg.exec.enabled = false;
        let app = AppState::new(RuntimeState {
            config: Arc::new(cfg),
            router: Arc::new(Router::new(Arc::new(Config::default())).unwrap()),
            clients: Arc::new(HashMap::new()),
            config_path: PathBuf::new(),
        });
        let body = json!({"messages": [{"role": "user", "content": "$proxy status"}]});
        block_on(async {
            let state = app.snapshot().await;
            assert!(maybe_exec(&app, &state, &body).await.is_none());
        });
    }

    #[test]
    fn maybe_exec_model_get_runs_in_process() {
        // The `model` get path resolves in-process (from the effective config /
        // catalog) without spawning any binary, and must not mutate the
        // in-memory selection. The exact model reported is environment-dependent
        // (depends on connected providers), so only structure is asserted.
        let app = AppState::new(RuntimeState {
            config: Arc::new(Config::default()),
            router: Arc::new(Router::new(Arc::new(Config::default())).unwrap()),
            clients: Arc::new(HashMap::new()),
            config_path: PathBuf::new(),
        });
        let body = json!({"messages": [{"role": "user", "content": "$proxy model"}]});
        block_on(async {
            let state = app.snapshot().await;
            let out = maybe_exec(&app, &state, &body).await.expect("is $proxy");
            assert!(!out.stdout.is_empty());
            assert_eq!(out.code, 0);
            assert_eq!(app.snapshot().await.config.defaults.active_model, None);
        });
    }

    #[test]
    fn exec_messages_response_carries_output() {
        let resp = exec_messages_response("hello\nworld", "gpt-4o");
        assert_eq!(resp.status(), StatusCode::OK);
    }
}
