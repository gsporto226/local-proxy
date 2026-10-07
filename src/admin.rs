//! Loopback-only admin API (`/admin/*`, see `docs/admin-api.md`): thin JSON
//! wrappers over the CLI functions, plus a typed `text/event-stream`.
#![allow(clippy::result_large_err)]

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::{LazyLock, OnceLock};

use axum::extract::{ConnectInfo, Path, Query, Request, State};
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, put};
use axum::{Json, Router};
use futures_util::Stream;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::broadcast;

use crate::handlers::{apply_effort, apply_model, AppState};

// ponytail: one process-global channel; the emitters (stats, rate limits,
// tracing) have no AppState handle, and there is one proxy per process.
static EVENTS: LazyLock<broadcast::Sender<(&'static str, Value)>> =
    LazyLock::new(|| broadcast::channel(256).0);
static PORT: OnceLock<u16> = OnceLock::new();

/// Publish an event to every `GET /admin/events` subscriber (no-op without any).
pub fn emit(event: &'static str, data: Value) {
    let _ = EVENTS.send((event, data));
}

/// Publish this instance's `{ model, effort }` as a `config` event.
pub async fn emit_config(app: &AppState) {
    emit("config", config_json(app).await);
}

/// Record the port `serve` bound, reported by `GET /admin/status`.
pub fn set_port(port: u16) {
    let _ = PORT.set(port);
}

async fn config_json(app: &AppState) -> Value {
    let defaults = app.snapshot().await.config.defaults.clone();
    json!({ "model": defaults.active_model, "effort": defaults.active_effort })
}

/// A tracing writer that forwards each formatted log line as a `log` event.
#[derive(Default)]
pub struct LogTap;

impl std::io::Write for LogTap {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if EVENTS.receiver_count() > 0 {
            for line in String::from_utf8_lossy(buf).lines() {
                emit("log", json!({ "line": line }));
            }
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// The `/admin/*` routes, guarded to loopback peers.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/admin/status", get(status))
        .route("/admin/accounts", get(accounts))
        .route("/admin/providers", get(providers))
        .route("/admin/models", get(models))
        .route("/admin/stats", get(stats))
        .route("/admin/rate-limits", get(rate_limits))
        .route("/admin/logs", get(logs))
        .route("/admin/session/{id}", get(session))
        .route("/admin/session/{id}/account", put(pin_account))
        .route(
            "/admin/session/{id}/account/{provider}",
            delete(unpin_account),
        )
        .route("/admin/model", put(set_model))
        .route("/admin/effort", put(set_effort))
        .route("/admin/account", put(set_account))
        .route("/admin/accounts/{provider}/{alias}", delete(disconnect))
        .route("/admin/events", get(events))
        .layer(middleware::from_fn(loopback_only))
}

/// Reject any peer that is not loopback (or unknown) with `403`.
async fn loopback_only(req: Request, next: Next) -> Response {
    let loopback = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .is_some_and(|ConnectInfo(addr)| addr.ip().is_loopback());
    if loopback {
        next.run(req).await
    } else {
        error(StatusCode::FORBIDDEN, "admin API is loopback-only")
    }
}

fn error(status: StatusCode, message: impl std::fmt::Display) -> Response {
    (status, Json(json!({ "error": message.to_string() }))).into_response()
}

type ApiResult = Result<Json<Value>, Response>;

fn internal(e: impl std::fmt::Display) -> Response {
    error(StatusCode::INTERNAL_SERVER_ERROR, e)
}

fn bad_request(e: impl std::fmt::Display) -> Response {
    error(StatusCode::BAD_REQUEST, e)
}

#[allow(clippy::needless_pass_by_value)]
fn message(msg: String) -> Json<Value> {
    Json(json!({ "message": msg }))
}

async fn config_path(app: &AppState) -> std::path::PathBuf {
    app.snapshot().await.config_path
}

async fn status(State(app): State<AppState>) -> Json<Value> {
    let mut out = config_json(&app).await;
    out["version"] = json!(env!("CARGO_PKG_VERSION"));
    out["port"] = json!(PORT.get());
    out["pid"] = json!(std::process::id());
    Json(out)
}

async fn accounts(State(app): State<AppState>) -> ApiResult {
    let providers = crate::cli::provider_accounts(&config_path(&app).await).map_err(internal)?;
    let accounts: Vec<_> = providers.into_iter().flat_map(|p| p.accounts).collect();
    Ok(Json(json!(accounts)))
}

async fn providers(State(app): State<AppState>) -> ApiResult {
    let providers = crate::cli::provider_accounts(&config_path(&app).await).map_err(internal)?;
    Ok(Json(json!(providers)))
}

async fn models(State(app): State<AppState>) -> ApiResult {
    let models = crate::cli::models_list(&config_path(&app).await).map_err(internal)?;
    Ok(Json(json!(models)))
}

#[derive(Deserialize)]
struct SinceQuery {
    since: Option<String>,
}

async fn stats(Query(q): Query<SinceQuery>) -> ApiResult {
    let since = q.since.as_deref().unwrap_or("day");
    Ok(Json(
        crate::cli::stats_json(since)
            .map_err(internal)?
            .unwrap_or(Value::Null),
    ))
}

async fn rate_limits() -> Json<Value> {
    let (h5, week) = crate::stats::rate_limits().unzip();
    Json(json!({ "h5": h5, "week": week }))
}

#[derive(Deserialize)]
struct LinesQuery {
    n: Option<usize>,
}

async fn logs(Query(q): Query<LinesQuery>) -> ApiResult {
    let text = crate::cli::logs_text(q.n.unwrap_or(200)).unwrap_or_default();
    Ok(Json(json!({ "lines": text.lines().collect::<Vec<_>>() })))
}

async fn session(State(app): State<AppState>, Path(id): Path<String>) -> ApiResult {
    let stats = crate::stats::session(&id)
        .map_err(internal)?
        .unwrap_or_default();
    Ok(Json(json!({
        "account": app.session_accounts(&id).await,
        "effort": crate::stats::session_effort(&id),
        "stats": stats,
    })))
}

#[derive(Deserialize)]
struct AccountBody {
    provider: String,
    alias: Option<String>,
    #[serde(default)]
    clear: bool,
}

async fn pin_account(
    State(app): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<AccountBody>,
) -> ApiResult {
    let alias = body.alias.ok_or_else(|| bad_request("alias is required"))?;
    app.set_session_account(&id, &body.provider, &alias).await;
    Ok(Json(json!(app.session_accounts(&id).await)))
}

async fn unpin_account(
    State(app): State<AppState>,
    Path((id, provider)): Path<(String, String)>,
) -> Json<Value> {
    app.clear_session_accounts(&id, Some(&provider)).await;
    Json(json!(app.session_accounts(&id).await))
}

#[derive(Deserialize)]
struct ModelBody {
    model: Option<String>,
}

async fn set_model(State(app): State<AppState>, Json(body): Json<ModelBody>) -> ApiResult {
    let model = body.model.as_deref().unwrap_or("clear");
    apply_model(&app, model)
        .await
        .map(message)
        .map_err(bad_request)
}

#[derive(Deserialize)]
struct EffortBody {
    effort: Option<String>,
}

async fn set_effort(State(app): State<AppState>, Json(body): Json<EffortBody>) -> ApiResult {
    let level = body.effort.as_deref().unwrap_or("clear");
    apply_effort(&app, level)
        .await
        .map(message)
        .map_err(bad_request)
}

async fn set_account(State(app): State<AppState>, Json(body): Json<AccountBody>) -> ApiResult {
    let args = match (body.clear, body.alias) {
        (true, _) => vec!["clear".to_string(), body.provider],
        (false, Some(alias)) => vec![format!("{}/{alias}", body.provider)],
        (false, None) => return Err(bad_request("alias or clear is required")),
    };
    let msg = crate::cli::account_result(&config_path(&app).await, &args).map_err(bad_request)?;
    emit_config(&app).await;
    Ok(message(msg))
}

async fn disconnect(
    State(app): State<AppState>,
    Path((provider, alias)): Path<(String, String)>,
) -> ApiResult {
    let msg = crate::cli::disconnect_provider(&config_path(&app).await, &provider, &alias)
        .map_err(internal)?;
    emit_config(&app).await;
    Ok(message(msg))
}

async fn events() -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let stream = futures_util::stream::unfold(EVENTS.subscribe(), |mut rx| async move {
        loop {
            match rx.recv().await {
                Ok((name, data)) => {
                    let event = Event::default().event(name).data(data.to_string());
                    return Some((Ok(event), rx));
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use std::collections::HashMap;
    use std::sync::Arc;
    use tower::ServiceExt;

    fn app_state(config_path: std::path::PathBuf) -> AppState {
        let config = Arc::new(crate::config::Config::default());
        AppState::new(crate::handlers::RuntimeState {
            config: config.clone(),
            router: Arc::new(crate::router::Router::new(config).unwrap()),
            clients: Arc::new(HashMap::new()),
            enforce_active_model: false,
            config_path,
        })
    }

    fn request(method: &str, uri: &str, body: &str, peer: [u8; 4]) -> Request {
        let mut req = Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        req.extensions_mut()
            .insert(ConnectInfo(SocketAddr::from((peer, 1234))));
        req
    }

    fn block_on<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Runtime::new().unwrap().block_on(f)
    }

    #[test]
    fn non_loopback_peer_is_forbidden() {
        let app = crate::handlers::app(app_state(std::path::PathBuf::new()));
        block_on(async {
            let res = app
                .clone()
                .oneshot(request("GET", "/admin/status", "", [10, 0, 0, 2]))
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::FORBIDDEN);
            let res = app
                .oneshot(request("GET", "/admin/status", "", [127, 0, 0, 1]))
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::OK);
        });
    }

    #[test]
    fn session_pin_round_trips() {
        let state = app_state(std::path::PathBuf::new());
        let app = crate::handlers::app(state.clone());
        block_on(async {
            let body = r#"{"provider":"mock","alias":"work"}"#;
            let res = app
                .oneshot(request(
                    "PUT",
                    "/admin/session/s1/account",
                    body,
                    [127, 0, 0, 1],
                ))
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::OK);
            assert_eq!(
                state.session_account("s1", "mock").await.as_deref(),
                Some("work")
            );
        });
    }

    #[test]
    fn effort_write_persists_and_emits_config() {
        let _guard = crate::TEST_STATE_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("LOCAL_PROXY_CONFIG_DIR", dir.path());
        let config_path = dir.path().join("config.yaml");
        let app = crate::handlers::app(app_state(config_path.clone()));
        block_on(async {
            let mut rx = EVENTS.subscribe();
            let res = app
                .oneshot(request(
                    "PUT",
                    "/admin/effort",
                    r#"{"effort":"high"}"#,
                    [127, 0, 0, 1],
                ))
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::OK);
            let saved = crate::config::Config::load(&config_path).unwrap();
            assert_eq!(saved.defaults.active_effort.as_deref(), Some("high"));
            loop {
                let (name, data) = rx.recv().await.unwrap();
                if name == "config" {
                    assert_eq!(data["effort"], "high");
                    break;
                }
            }
        });
        std::env::remove_var("LOCAL_PROXY_CONFIG_DIR");
    }
}
