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
use crate::ir::{self, Format};
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
    /// The built upstream clients, keyed by provider name and account alias.
    pub clients: Arc<HashMap<String, HashMap<String, ProviderClient>>>,
    /// When true, requests that carry a client-sent model are forced to use the
    /// proxy's `active_model` instead. Used by `local-proxy launch claude` so the
    /// launched tool's own model selection never overrides the user's choice.
    pub enforce_active_model: bool,
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
    /// The account database or vault could not be read.
    #[error("failed to load accounts: {0}")]
    Auth(#[source] crate::auth::AuthError),
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
    let mut config = crate::catalog::effective_config(base, overlay);
    crate::upstream::discover_models(&mut config).map_err(RuntimeError::Clients)?;
    let config = Arc::new(config);
    let router = Arc::new(Router::new(config.clone()).map_err(RuntimeError::Router)?);
    let clients = Arc::new(build_clients(&config).map_err(|error| match error {
        ClientBuildError::Auth(source) => RuntimeError::Auth(source),
        ClientBuildError::Upstream(source) => RuntimeError::Clients(source),
    })?);
    Ok(RuntimeState {
        config,
        router,
        clients,
        enforce_active_model: false,
        config_path: config_path.to_path_buf(),
    })
}

/// Carry this instance's in-memory state across a hot-reload: the active model
/// (a model write must never leak to other proxies via the shared file) and
/// the `--enforce-active-model` launch flag, which is not config at all.
fn carry_instance_state(old: &RuntimeState, new: &mut RuntimeState) {
    let mut cfg = (*new.config).clone();
    cfg.defaults
        .active_model
        .clone_from(&old.config.defaults.active_model);
    new.config = Arc::new(cfg);
    new.enforce_active_model = old.enforce_active_model;
}

/// Whether `path` is one of the files whose change triggers a hot-reload:
/// the active config file or the auth store.
fn is_reload_path(path: &Path, config_file: &std::ffi::OsStr) -> bool {
    path.file_name()
        .is_some_and(|name| name == config_file || name == "auth.json" || name == "accounts.db")
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
    let config_file = config_path
        .file_name()
        .map(std::ffi::OsStr::to_os_string)
        .unwrap_or_default();
    let (tx, rx) = std::sync::mpsc::channel();
    let mut debouncer = notify_debouncer_full::new_debouncer(
        Duration::from_millis(300),
        None,
        move |result: notify_debouncer_full::DebounceEventResult| {
            let Ok(events) = result else {
                return;
            };
            // The config dir also holds `local-proxy.log`, `stats.db` and `pid`,
            // which change on every request; only the reloadable files matter.
            let relevant = events
                .iter()
                .flat_map(|e| e.event.paths.iter())
                .any(|path| is_reload_path(path, &config_file));
            if relevant {
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
                    // Everything else (providers, routes, auth) reloads from the file.
                    carry_instance_state(&*state.read().await, &mut new_state);
                    *state.write().await = new_state;
                    tracing::info!("config/auth change applied (hot-reload)");
                }
                Err(e) => tracing::warn!("hot-reload rebuild failed: {e}"),
            }
        }
    });
    Ok(())
}

/// Errors while loading credentials or building upstream clients.
#[derive(Debug, thiserror::Error)]
pub enum ClientBuildError {
    /// The encrypted account database or OS vault could not be accessed.
    #[error("account store: {0}")]
    Auth(#[from] crate::auth::AuthError),
    /// An upstream client could not be built.
    #[error("upstream: {0}")]
    Upstream(#[from] UpstreamError),
}

/// Build an upstream [`ProviderClient`] for every configured provider.
///
/// # Errors
///
/// Returns an error if any provider client fails to build.
pub fn build_clients(
    config: &Config,
) -> Result<HashMap<String, HashMap<String, ProviderClient>>, ClientBuildError> {
    let passthrough = config.server.passthrough_keys;
    let auth = crate::auth::read_auth()?;
    let mut map = HashMap::new();
    let mut connected = Vec::new();
    for provider in &config.providers {
        let mut accounts = HashMap::new();
        if let Some(entries) = auth.get(&provider.name) {
            for (alias, entry) in entries {
                let client = ProviderClient::new_for_alias(
                    provider,
                    alias,
                    passthrough,
                    Some(entry.clone()),
                )?;
                if client.has_key() {
                    connected.push(format!("{}:{alias}", provider.name));
                }
                accounts.insert(alias.clone(), client);
            }
        }
        // Preserve passthrough-key support when no credentials are stored.
        if accounts.is_empty() {
            accounts.insert(
                "default".to_string(),
                ProviderClient::new(provider, passthrough, None)?,
            );
        }
        map.insert(provider.name.clone(), accounts);
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

/// Extract the client session id (`X-Claude-Code-Session-Id`) from the inbound
/// headers, defaulting to `""` when absent (curl, plain `OpenAI` clients, tests).
#[must_use]
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
fn prepare_responses_request(
    provider: &crate::config::Provider,
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

/// Read a Responses SSE response to completion and reassemble the single
/// Response object it describes.
///
/// Used for a non-streaming client of a stream-only upstream. A transport error
/// ends the read; whatever was reassembled so far is returned.
async fn aggregate_responses_stream(resp: reqwest::Response) -> ir::Response {
    use futures_util::StreamExt as _;
    let mut frames = Box::pin(crate::sse::sse_frames(resp));
    let mut events = Vec::new();
    while let Some(Ok(frame)) = frames.next().await {
        if frame.is_done() {
            break;
        }
        events.extend(frame.json());
    }
    ir::responses::aggregate(&events)
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

fn is_connected(state: &RuntimeState, provider: &str) -> bool {
    state
        .clients
        .get(provider)
        .is_some_and(|accounts| accounts.values().any(ProviderClient::has_key))
}

fn resolve_model(
    state: &RuntimeState,
    client_model: Option<&str>,
) -> Result<(Arc<crate::config::Provider>, String, Option<String>), ApiError> {
    // Normally the client's requested model wins: it is resolved through the
    // router and must match a configured route/provider/native model. When the
    // client sends no model, the proxy falls back to its own override (active
    // model), then the first model available from a connected provider.
    //
    // When `enforce_active_model` is set (e.g. via `local-proxy launch claude`),
    // a client-sent model is ignored entirely so the user's active model is
    // always the one routed, regardless of what the launched tool selects.
    let client_sent = !state.enforce_active_model && client_model.is_some_and(|m| !m.is_empty());
    let requested = if client_sent {
        client_model.unwrap_or_default().to_string()
    } else if let Some(model) = state.config.defaults.active_model.clone() {
        model
    } else {
        let first = state.config.providers.iter().find_map(|p| {
            is_connected(state, &p.name)
                .then(|| {
                    p.models
                        .first()
                        .map(|m| crate::config::qualified_id(&p.name, m))
                })
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

fn client_for(
    state: &RuntimeState,
    provider: &crate::config::Provider,
    alias: Option<&str>,
) -> Result<ProviderClient, ApiError> {
    let accounts = state.clients.get(&provider.name).ok_or_else(|| {
        ApiError::internal(format!("client not built for provider {}", provider.name))
    })?;
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
        // placeholder client never satisfies it.
        return accounts
            .get(selected)
            .filter(|client| client.has_key())
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

/// Best-effort local statistics capture for a non-streaming request.
///
/// Extracts token usage from the upstream (already consumed) body where
/// possible, records the row against the local stats database, and ignores any
/// failure. The provider/model are the resolved upstream ones. Streaming
/// requests are recorded by [`StreamCapture`] once the `SSE` stream completes.
#[allow(clippy::needless_pass_by_value, clippy::too_many_arguments)]
fn capture(
    endpoint: &'static str,
    provider: &str,
    model: &str,
    streamed: bool,
    status: u16,
    upstream_body: Option<&Value>,
    started: &Instant,
    session_id: &str,
) {
    let mut tokens = crate::translate::TokenUsage::default();
    let (energy, cost) = upstream_body.map_or((None, None), |body| {
        if let Some(u) = body.get("usage") {
            tokens = crate::translate::parse_usage(u);
        }
        (
            crate::translate::energy_from_value(body),
            // A `NeuralWatt`-style top-level `cost` object wins; otherwise fall
            // back to the `usage.cost` reported by OpenAI/OpenRouter-style
            // upstreams (e.g. OpenRouter, Groq) so that cost is still recorded.
            crate::translate::cost_from_value(body).or_else(|| tokens.as_cost()),
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
            session_id: session_id.to_string(),
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
    } else if args.first().map(String::as_str) == Some("effort") {
        Some(handle_effort_exec(app, state, &args).await)
    } else if args.first().map(String::as_str) == Some("logs") {
        Some(handle_logs_exec(&args))
    } else {
        Some(crate::exec::run(&exec.command, &args, Duration::from_secs(exec.timeout_secs)).await)
    }
}

/// The line count for `$proxy logs`: the value after `-n`/`--lines`, or the
/// shared CLI default when the flag is absent or malformed.
fn logs_lines_arg(args: &[String]) -> usize {
    args.iter()
        .position(|a| a == "-n" || a == "--lines")
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(crate::cli::DEFAULT_LOG_LINES)
}

/// Handle `$proxy logs [-n N]` in-process: read the tail of the proxy's own
/// log file, without spawning the CLI binary.
fn handle_logs_exec(args: &[String]) -> crate::exec::ExecOutput {
    let lines = logs_lines_arg(args);
    match crate::cli::logs_text(lines) {
        Ok(stdout) => crate::exec::ExecOutput {
            stdout,
            stderr: String::new(),
            code: 0,
            timed_out: false,
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => crate::exec::ExecOutput {
            stdout: String::new(),
            stderr: format!(
                "nenhum log em {} (proxy ainda não rodou?)",
                crate::cli::log_file().display()
            ),
            code: 1,
            timed_out: false,
        },
        Err(e) => crate::exec::ExecOutput {
            stdout: String::new(),
            stderr: e.to_string(),
            code: 1,
            timed_out: false,
        },
    }
}

/// Handle `$proxy effort [level|clear]` in-process: persist via the CLI logic
/// and apply it to this instance right away (others follow via hot-reload).
async fn handle_effort_exec(
    app: &AppState,
    state: &RuntimeState,
    args: &[String],
) -> crate::exec::ExecOutput {
    let level = args.get(1).map(String::as_str);
    let (stdout, stderr, code) = match crate::cli::effort_result(&state.config_path, level) {
        Ok(msg) => {
            if let Some(l) = level {
                let value = (l != "clear").then(|| l.to_string());
                let mut guard = app.inner.write().await;
                let mut cfg = (*guard.config).clone();
                cfg.defaults.active_effort = value;
                guard.config = Arc::new(cfg);
            }
            (msg, String::new(), 0)
        }
        Err(e) => (String::new(), e.to_string(), 1),
    };
    crate::exec::ExecOutput {
        stdout,
        stderr,
        code,
        timed_out: false,
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

/// The reasoning effort an Anthropic request asks for (`output_config.effort`,
/// as Claude Code sends it), for the status line.
fn request_effort(body: &[u8]) -> Option<String> {
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
    let account_alias = match extract_account_alias(&headers) {
        Ok(alias) => alias,
        Err(e) => return error_response(&e, true),
    };
    let session_id = extract_session_id(&headers);
    if !session_id.is_empty() {
        if let Some(effort) = request_effort(&body) {
            stats::record_effort(&session_id, &effort);
        }
    }
    match handle_chat(
        Format::Anthropic,
        "/v1/messages",
        &app,
        &state,
        &body,
        client_key.as_deref(),
        account_alias,
        &session_id,
    )
    .await
    {
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

/// Serve one chat request from a `client`-format endpoint: resolve the route,
/// translate through the IR when the upstream speaks another format, and
/// translate the response (or stream) back.
#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
async fn handle_chat(
    client_format: Format,
    endpoint: &'static str,
    app: &AppState,
    state: &RuntimeState,
    body: &Bytes,
    client_key: Option<&str>,
    account_alias: Option<&str>,
    session_id: &str,
) -> Result<Response, ApiError> {
    let started = Instant::now();
    let mut body = parse_body(body)?;

    if let Some(out) = maybe_exec(app, state, &body).await {
        let text = crate::exec::format_output(&out);
        let model = active_model_or_default(state);
        tracing::info!(target: crate::LOG_TARGET, endpoint, "handled $proxy command");
        return Ok(match client_format {
            Format::Anthropic => exec_messages_response(&text, &model),
            Format::Openai => exec_chat_response(&text, &model),
            Format::Responses => exec_responses_response(&text, &model),
        });
    }

    let streaming = wants_stream(&body);
    let client_model = body.get("model").and_then(Value::as_str);
    let (provider, upstream_model, reasoning_effort) = resolve_model(state, client_model)?;
    tracing::info!(
        target: crate::LOG_TARGET,
        endpoint,
        provider = %provider.name,
        upstream_model,
        streaming,
        "resolved route"
    );
    body["model"] = json!(upstream_model);
    if client_format == Format::Anthropic {
        if let Some(effort) = &state.config.defaults.active_effort {
            body["output_config"]["effort"] = json!(effort);
        }
    }
    let client = client_for(state, &provider, account_alias)?;
    let upstream_format = Format::from(provider.format);
    let same = upstream_format == client_format;

    let mut upstream_body = if same {
        same_format_request(client_format, body)
    } else {
        ir::translate_request(client_format, upstream_format, body)?
    };
    if streaming && upstream_format == Format::Openai {
        enable_usage(&mut upstream_body);
    }
    prepare_responses_request(
        &provider,
        &mut upstream_body,
        reasoning_effort.as_deref(),
        session_id,
    );

    let resp = client
        .chat_request(client.default_path(), upstream_body, client_key, session_id)
        .await
        .map_err(ApiError::from)?;
    let status = resp.status().as_u16();
    if status >= 400 {
        let (status, rbody) = send_and_read(resp).await;
        capture(
            endpoint,
            &provider.name,
            &upstream_model,
            streaming,
            status,
            Some(&rbody),
            &started,
            session_id,
        );
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
        let cap = StreamCapture::new(
            endpoint,
            &provider.name,
            &upstream_model,
            status,
            started,
            session_id,
        );
        return Ok(if same {
            passthrough_stream(resp, Some(cap))
        } else {
            sse_response(streams::translate(
                resp,
                upstream_format,
                client_format,
                &upstream_model,
                Some(cap),
            ))
        });
    }

    // A Responses upstream only streams; fold its events into one response.
    if upstream_format == Format::Responses {
        let mut folded = aggregate_responses_stream(resp).await;
        let usage = json!({"usage": translate::responses_usage(&folded.usage)});
        capture(
            endpoint,
            &provider.name,
            &upstream_model,
            false,
            status,
            Some(&usage),
            &started,
            session_id,
        );
        folded.model.clone_from(&upstream_model);
        return Ok(json_response(
            StatusCode::OK,
            ir::encode_response(client_format, &folded),
        ));
    }
    let rbody = resp.json::<Value>().await.unwrap_or(Value::Null);
    capture(
        endpoint,
        &provider.name,
        &upstream_model,
        false,
        status,
        Some(&rbody),
        &started,
        session_id,
    );
    if same {
        return Ok(json_response(StatusCode::OK, rbody));
    }
    let mut decoded = ir::decode_response(upstream_format, &rbody);
    decoded.model = upstream_model;
    Ok(json_response(
        StatusCode::OK,
        ir::encode_response(client_format, &decoded),
    ))
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
    let account_alias = match extract_account_alias(&headers) {
        Ok(alias) => alias,
        Err(e) => return error_response(&e, false),
    };
    let session_id = extract_session_id(&headers);
    match handle_chat(
        Format::Openai,
        "/v1/chat/completions",
        &app,
        &state,
        &body,
        client_key.as_deref(),
        account_alias,
        &session_id,
    )
    .await
    {
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
    let account_alias = match extract_account_alias(&headers) {
        Ok(alias) => alias,
        Err(e) => return error_response(&e, false),
    };
    let session_id = extract_session_id(&headers);
    match handle_chat(
        Format::Responses,
        "/v1/responses",
        &app,
        &state,
        &body,
        client_key.as_deref(),
        account_alias,
        &session_id,
    )
    .await
    {
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
    fn hot_reload_keeps_enforce_and_active_model() {
        let make = |model: Option<&str>, enforce: bool| {
            let mut cfg = Config::default();
            cfg.defaults.active_model = model.map(str::to_string);
            RuntimeState {
                config: Arc::new(cfg),
                router: Arc::new(Router::new(Arc::new(Config::default())).unwrap()),
                clients: Arc::new(HashMap::new()),
                enforce_active_model: enforce,
                config_path: PathBuf::new(),
            }
        };
        let old = make(Some("kimi"), true);
        let mut new = make(None, false);
        carry_instance_state(&old, &mut new);
        assert!(new.enforce_active_model);
        assert_eq!(new.config.defaults.active_model.as_deref(), Some("kimi"));
    }

    #[test]
    fn watcher_reacts_only_to_config_and_auth() {
        let config = std::ffi::OsStr::new("config.yaml");
        assert!(is_reload_path(Path::new("C:/cfg/config.yaml"), config));
        assert!(is_reload_path(Path::new("C:/cfg/auth.json"), config));
        assert!(!is_reload_path(Path::new("C:/cfg/other.yaml"), config));
        assert!(!is_reload_path(Path::new("C:/cfg/local-proxy.log"), config));
        assert!(!is_reload_path(Path::new("C:/cfg/stats.db"), config));
        assert!(!is_reload_path(Path::new("C:/cfg/stats.db-wal"), config));
        assert!(!is_reload_path(Path::new("C:/cfg/pid"), config));
    }

    #[test]
    fn request_effort_reads_output_config() {
        let body = br#"{"model":"m","output_config":{"effort":"xhigh"},"messages":[]}"#;
        assert_eq!(request_effort(body).as_deref(), Some("xhigh"));
        assert_eq!(request_effort(br#"{"model":"m"}"#), None);
    }

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
            statusline: crate::config::StatuslineConfig::default(),
        };
        let state = RuntimeState {
            config: Arc::new(cfg),
            router: Arc::new(Router::new(Arc::new(Config::default())).unwrap()),
            clients: Arc::new(HashMap::new()),
            enforce_active_model: false,
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
            enforce_active_model: false,
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
    fn prepare_responses_request_forces_stream_and_sets_effort() {
        let responses = crate::config::Provider {
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
            let provider = crate::config::Provider {
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
    fn rebuild_merges_catalog_with_overlay_and_reapplies() {
        // `build_runtime_state` reads the credential store: keep this test off
        // the real config dir and out of the way of other env-mutating tests.
        let _guard = crate::TEST_STATE_LOCK.lock().unwrap();
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock is set")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "local-proxy-rebuild-{}-{stamp}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("create test dir");
        std::env::set_var("LOCAL_PROXY_CONFIG_DIR", &dir);
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

        std::env::remove_var("LOCAL_PROXY_CONFIG_DIR");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn client_unknown_model_fails_even_with_active_model() {
        let cfg = Arc::new(Config {
            server: crate::config::Server::default(),
            providers: vec![
                crate::config::Provider {
                    name: "openai".to_string(),
                    base_url: "https://api.openai.com/v1".to_string(),
                    format: ProviderFormat::Openai,
                    models: vec!["gpt-4o".to_string()],
                    auto_model: None,
                    headers: std::collections::HashMap::new(),
                    session_header: None,
                    oauth: None,
                },
                crate::config::Provider {
                    name: "anthropic".to_string(),
                    base_url: "https://api.anthropic.com".to_string(),
                    format: ProviderFormat::Anthropic,
                    models: vec!["claude-sonnet-4-5".to_string()],
                    auto_model: None,
                    headers: std::collections::HashMap::new(),
                    session_header: None,
                    oauth: None,
                },
            ],
            routes: vec![crate::config::Route {
                model: "gpt-4o".to_string(),
                provider: "openai".to_string(),
                prefix: false,
                upstream_model: None,
                reasoning_effort: None,
            }],
            defaults: crate::config::Defaults {
                provider: "openai".to_string(),
                active_model: Some("gpt-4o".to_string()),
                active_effort: None,
                active_accounts: HashMap::new(),
            },
            exec: crate::config::Exec::default(),
            statusline: crate::config::StatuslineConfig::default(),
        });
        let mut clients = HashMap::new();
        let openai = cfg
            .providers
            .iter()
            .find(|p| p.name == "openai")
            .expect("openai provider")
            .clone();
        clients.insert(
            "openai".to_string(),
            HashMap::from([(
                "default".to_string(),
                crate::upstream::ProviderClient::new(
                    &openai,
                    false,
                    Some(crate::auth::AuthEntry::Api {
                        key: "sk".to_string(),
                    }),
                )
                .expect("client"),
            )]),
        );
        let state = RuntimeState {
            config: cfg.clone(),
            router: Arc::new(Router::new(cfg).unwrap()),
            clients: Arc::new(clients),
            enforce_active_model: false,
            config_path: PathBuf::new(),
        };

        // A client-sent model that does not resolve fails with `proxy: unknown
        // model`, regardless of the configured active_model fallback.
        let err = resolve_model(&state, Some("totally-unknown")).expect_err("unknown model");
        assert_eq!(err.message, "proxy: unknown model totally-unknown");
    }

    #[test]
    fn client_model_wins_over_active_model() {
        let cfg = Arc::new(Config {
            server: crate::config::Server::default(),
            providers: vec![
                crate::config::Provider {
                    name: "openai".to_string(),
                    base_url: "https://api.openai.com/v1".to_string(),
                    format: ProviderFormat::Openai,
                    models: vec!["gpt-4o".to_string(), "gpt-4o-mini".to_string()],
                    auto_model: None,
                    headers: std::collections::HashMap::new(),
                    session_header: None,
                    oauth: None,
                },
                crate::config::Provider {
                    name: "anthropic".to_string(),
                    base_url: "https://api.anthropic.com".to_string(),
                    format: ProviderFormat::Anthropic,
                    models: vec!["claude-sonnet-4-5".to_string()],
                    auto_model: None,
                    headers: std::collections::HashMap::new(),
                    session_header: None,
                    oauth: None,
                },
            ],
            routes: Vec::new(),
            defaults: crate::config::Defaults {
                provider: "openai".to_string(),
                active_model: Some("gpt-4o".to_string()),
                active_effort: None,
                active_accounts: HashMap::new(),
            },
            exec: crate::config::Exec::default(),
            statusline: crate::config::StatuslineConfig::default(),
        });
        let mut clients = HashMap::new();
        let openai = cfg
            .providers
            .iter()
            .find(|p| p.name == "openai")
            .expect("openai provider")
            .clone();
        clients.insert(
            "openai".to_string(),
            HashMap::from([(
                "default".to_string(),
                crate::upstream::ProviderClient::new(
                    &openai,
                    false,
                    Some(crate::auth::AuthEntry::Api {
                        key: "sk".to_string(),
                    }),
                )
                .expect("client"),
            )]),
        );
        let state = RuntimeState {
            config: cfg.clone(),
            router: Arc::new(Router::new(cfg).unwrap()),
            clients: Arc::new(clients),
            enforce_active_model: false,
            config_path: PathBuf::new(),
        };

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
        let cfg = Arc::new(Config {
            server: crate::config::Server::default(),
            providers: vec![
                crate::config::Provider {
                    name: "openai".to_string(),
                    base_url: "https://api.openai.com/v1".to_string(),
                    format: ProviderFormat::Openai,
                    models: vec!["gpt-4o".to_string(), "gpt-4o-mini".to_string()],
                    auto_model: None,
                    headers: std::collections::HashMap::new(),
                    session_header: None,
                    oauth: None,
                },
                crate::config::Provider {
                    name: "anthropic".to_string(),
                    base_url: "https://api.anthropic.com".to_string(),
                    format: ProviderFormat::Anthropic,
                    models: vec!["claude-sonnet-4-5".to_string()],
                    auto_model: None,
                    headers: std::collections::HashMap::new(),
                    session_header: None,
                    oauth: None,
                },
            ],
            routes: Vec::new(),
            defaults: crate::config::Defaults {
                provider: "openai".to_string(),
                active_model: Some("gpt-4o".to_string()),
                active_effort: None,
                active_accounts: HashMap::new(),
            },
            exec: crate::config::Exec::default(),
            statusline: crate::config::StatuslineConfig::default(),
        });
        let mut clients = HashMap::new();
        let openai = cfg
            .providers
            .iter()
            .find(|p| p.name == "openai")
            .expect("openai provider")
            .clone();
        clients.insert(
            "openai".to_string(),
            HashMap::from([(
                "default".to_string(),
                crate::upstream::ProviderClient::new(
                    &openai,
                    false,
                    Some(crate::auth::AuthEntry::Api {
                        key: "sk".to_string(),
                    }),
                )
                .expect("client"),
            )]),
        );
        let state = RuntimeState {
            config: cfg.clone(),
            router: Arc::new(Router::new(cfg).unwrap()),
            clients: Arc::new(clients),
            enforce_active_model: true,
            config_path: PathBuf::new(),
        };

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
        let cfg = Arc::new(Config {
            server: crate::config::Server::default(),
            providers: vec![crate::config::Provider {
                name: "openai".to_string(),
                base_url: "https://api.openai.com/v1".to_string(),
                format: ProviderFormat::Openai,
                models: vec!["gpt-4o".to_string(), "gpt-4o-mini".to_string()],
                auto_model: None,
                headers: std::collections::HashMap::new(),
                session_header: None,
                oauth: None,
            }],
            routes: Vec::new(),
            defaults: crate::config::Defaults {
                provider: "openai".to_string(),
                active_model: None,
                active_effort: None,
                active_accounts: HashMap::new(),
            },
            exec: crate::config::Exec::default(),
            statusline: crate::config::StatuslineConfig::default(),
        });
        let mut clients = HashMap::new();
        let openai = cfg
            .providers
            .iter()
            .find(|p| p.name == "openai")
            .expect("openai provider")
            .clone();
        clients.insert(
            "openai".to_string(),
            HashMap::from([(
                "default".to_string(),
                crate::upstream::ProviderClient::new(
                    &openai,
                    false,
                    Some(crate::auth::AuthEntry::Api {
                        key: "sk".to_string(),
                    }),
                )
                .expect("client"),
            )]),
        );
        let state = RuntimeState {
            config: cfg.clone(),
            router: Arc::new(Router::new(cfg).unwrap()),
            clients: Arc::new(clients),
            enforce_active_model: false,
            config_path: PathBuf::new(),
        };
        let (provider, upstream, _effort) = resolve_model(&state, None).expect("resolves");
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
                format: ProviderFormat::Openai,
                models: vec!["gpt-4o".to_string()],
                auto_model: None,
                headers: std::collections::HashMap::new(),
                session_header: None,
                oauth: None,
            }],
            routes: Vec::new(),
            defaults: crate::config::Defaults {
                provider: "openai".to_string(),
                active_model: None,
                active_effort: None,
                active_accounts: HashMap::new(),
            },
            exec: crate::config::Exec::default(),
            statusline: crate::config::StatuslineConfig::default(),
        });
        let state = RuntimeState {
            config: cfg.clone(),
            router: Arc::new(Router::new(cfg).unwrap()),
            clients: Arc::new(HashMap::new()),
            enforce_active_model: false,
            config_path: PathBuf::new(),
        };
        assert!(resolve_model(&state, None).is_err());
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
            enforce_active_model: false,
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
            enforce_active_model: false,
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
            enforce_active_model: false,
            config_path: PathBuf::new(),
        });
        let body = json!({"messages": [{"role": "user", "content": "$proxy model"}]});
        block_on(async {
            let state = app.snapshot().await;
            let out = maybe_exec(&app, &state, &body).await.expect("is $proxy");
            assert!(!out.stdout.is_empty(), "stdout should not be empty");
            assert_eq!(out.code, 0);
            assert_eq!(app.snapshot().await.config.defaults.active_model, None);
        });
    }

    #[test]
    fn logs_lines_arg_reads_flag_or_defaults() {
        let parse = crate::exec::parse_args;
        assert_eq!(
            logs_lines_arg(&parse("logs")),
            crate::cli::DEFAULT_LOG_LINES
        );
        assert_eq!(logs_lines_arg(&parse("logs -n 100")), 100);
        assert_eq!(logs_lines_arg(&parse("logs --lines 3")), 3);
        assert_eq!(
            logs_lines_arg(&parse("logs -n nope")),
            crate::cli::DEFAULT_LOG_LINES
        );
    }

    #[test]
    fn exec_messages_response_carries_output() {
        let resp = exec_messages_response("hello\nworld", "gpt-4o");
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[test]
    fn account_selection_requires_alias_only_for_multiple_accounts() {
        let provider = crate::config::Provider {
            name: "openai".to_string(),
            base_url: "http://127.0.0.1:9".to_string(),
            format: ProviderFormat::Openai,
            models: vec!["gpt-test".to_string()],
            auto_model: None,
            headers: HashMap::new(),
            session_header: None,
            oauth: None,
        };
        let account = |alias: &str| {
            ProviderClient::new_for_alias(
                &provider,
                alias,
                false,
                Some(crate::auth::AuthEntry::Api {
                    key: format!("key-{alias}"),
                }),
            )
            .unwrap()
        };
        let cfg = Arc::new(Config {
            providers: vec![provider.clone()],
            ..Config::default()
        });
        let mut accounts = HashMap::from([("one".to_string(), account("one"))]);
        let make_state = |accounts: HashMap<String, ProviderClient>| RuntimeState {
            config: cfg.clone(),
            router: Arc::new(Router::new(cfg.clone()).unwrap()),
            clients: Arc::new(HashMap::from([("openai".to_string(), accounts)])),
            enforce_active_model: false,
            config_path: PathBuf::new(),
        };
        assert!(client_for(&make_state(accounts.clone()), &provider, None).is_ok());
        assert!(client_for(&make_state(accounts.clone()), &provider, Some("one")).is_ok());
        let err = client_for(&make_state(accounts.clone()), &provider, Some("missing"))
            .expect_err("unknown alias must fail even with one account");
        assert_eq!(err.status, 400);
        assert!(err.message.contains("unknown account 'missing'"));
        accounts.insert("two".to_string(), account("two"));
        let state = make_state(accounts);
        let err = client_for(&state, &provider, None).expect_err("ambiguous account");
        assert_eq!(err.status, 400);
        assert!(err.message.contains("X-Local-Proxy-Account"));
        assert!(client_for(&state, &provider, Some("two")).is_ok());
    }

    #[test]
    fn selected_account_wins_over_multiplicity_and_header_wins_over_selection() {
        let provider = crate::config::Provider {
            name: "openai".to_string(),
            base_url: "http://127.0.0.1:9".to_string(),
            format: ProviderFormat::Openai,
            models: vec!["gpt-test".to_string()],
            auto_model: None,
            headers: HashMap::new(),
            session_header: None,
            oauth: None,
        };
        let account = |alias: &str| {
            ProviderClient::new_for_alias(
                &provider,
                alias,
                false,
                Some(crate::auth::AuthEntry::Api {
                    key: format!("key-{alias}"),
                }),
            )
            .unwrap()
        };
        let state_with = |selected: &str| {
            let cfg = Arc::new(Config {
                providers: vec![provider.clone()],
                defaults: crate::config::Defaults {
                    active_accounts: HashMap::from([("openai".to_string(), selected.to_string())]),
                    ..crate::config::Defaults::default()
                },
                ..Config::default()
            });
            RuntimeState {
                config: cfg.clone(),
                router: Arc::new(Router::new(cfg).unwrap()),
                clients: Arc::new(HashMap::from([(
                    "openai".to_string(),
                    HashMap::from([
                        ("personal".to_string(), account("personal")),
                        ("work".to_string(), account("work")),
                    ]),
                )])),
                enforce_active_model: false,
                config_path: PathBuf::new(),
            }
        };

        // No header: the selected account is used without error.
        let state = state_with("work");
        let selected = client_for(&state, &provider, None).unwrap();
        assert_eq!(selected.effective_key(None).as_deref(), Some("key-work"));
        // The header still overrides the selection per request.
        let overridden = client_for(&state, &provider, Some("personal")).unwrap();
        assert_eq!(
            overridden.effective_key(None).as_deref(),
            Some("key-personal")
        );

        // A selection whose account was disconnected is a clear 400, never a
        // silent fallback to another account.
        let disconnected = state_with("ghost");
        let err = client_for(&disconnected, &provider, None).expect_err("stale selection");
        assert_eq!(err.status, 400);
        assert!(
            err.message.contains("no longer stored"),
            "got: {}",
            err.message
        );
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
    fn active_model_to_unconnected_provider_errors_clearly() {
        let cfg = Arc::new(Config {
            server: crate::config::Server::default(),
            providers: vec![crate::config::Provider {
                name: "neuralwatt".to_string(),
                base_url: "https://api.neuralwatt.com/v1".to_string(),
                format: ProviderFormat::Openai,
                models: vec!["glm-5.2".to_string()],
                auto_model: None,
                headers: std::collections::HashMap::new(),
                session_header: None,
                oauth: None,
            }],
            routes: Vec::new(),
            defaults: crate::config::Defaults {
                provider: "neuralwatt".to_string(),
                active_model: Some("glm-5.2".to_string()),
                active_effort: None,
                active_accounts: HashMap::new(),
            },
            exec: crate::config::Exec::default(),
            statusline: crate::config::StatuslineConfig::default(),
        });
        let state = RuntimeState {
            config: cfg.clone(),
            router: Arc::new(Router::new(cfg).unwrap()),
            clients: Arc::new(HashMap::new()),
            enforce_active_model: false,
            config_path: PathBuf::new(),
        };
        let err = resolve_model(&state, None).expect_err("unconnected provider is an error");
        assert!(err.message.contains("no account"), "got: {}", err.message);
    }
}
