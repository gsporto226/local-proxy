//! Loopback-only admin API (`/admin/*`, see `docs/admin-api.md`): thin JSON
//! wrappers over the CLI functions, plus a typed `text/event-stream`.
#![allow(clippy::result_large_err)]

use std::collections::HashMap;
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
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::broadcast;

use crate::handlers::{apply_effort, apply_model, AppState};

// ponytail: one process-global channel; the emitters (stats, rate limits,
// tracing) have no AppState handle, and there is one proxy per process.
static EVENTS: LazyLock<broadcast::Sender<(&'static str, Value)>> =
    LazyLock::new(|| broadcast::channel(256).0);
static PORT: OnceLock<u16> = OnceLock::new();
// ponytail: one global lock avoids duplicate refreshes; use per-account locks if unrelated endpoint latency becomes a bottleneck.
static USAGE_REFRESH: LazyLock<tokio::sync::Mutex<()>> =
    LazyLock::new(|| tokio::sync::Mutex::new(()));
const ACCOUNT_USAGE_CACHE_SECS: i64 = 60;

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
        .route("/admin/account-usage", get(account_usage))
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

/// Runs a synchronous read (config file, credential vault, SQLite) on the
/// blocking pool so admin polling never stalls proxied requests.
async fn blocking<T, E>(f: impl FnOnce() -> Result<T, E> + Send + 'static) -> Result<T, Response>
where
    T: Send + 'static,
    E: std::fmt::Display + Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(internal)?
        .map_err(internal)
}

async fn accounts(State(app): State<AppState>) -> ApiResult {
    let path = config_path(&app).await;
    let providers = blocking(move || crate::cli::provider_accounts(&path)).await?;
    let accounts: Vec<_> = providers.into_iter().flat_map(|p| p.accounts).collect();
    Ok(Json(json!(accounts)))
}

async fn providers(State(app): State<AppState>) -> ApiResult {
    let path = config_path(&app).await;
    let providers = blocking(move || crate::cli::provider_accounts(&path)).await?;
    Ok(Json(json!(providers)))
}

async fn models(State(app): State<AppState>) -> ApiResult {
    let path = config_path(&app).await;
    let models = blocking(move || crate::cli::models_list(&path)).await?;
    Ok(Json(json!(models)))
}

#[derive(Deserialize)]
struct SinceQuery {
    since: Option<String>,
}

async fn stats(Query(q): Query<SinceQuery>) -> ApiResult {
    let since = q.since.unwrap_or_else(|| "day".to_string());
    let stats = blocking(move || crate::cli::stats_json(&since)).await?;
    Ok(Json(stats.unwrap_or(Value::Null)))
}

async fn rate_limits() -> Json<Value> {
    let (h5, week) = crate::stats::rate_limits().unzip();
    Json(json!({ "h5": h5, "week": week }))
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum AccountUsageResult {
    Available {
        usage: crate::stats::AccountUsage,
        stale: bool,
        error: Option<String>,
    },
    Unavailable {
        provider: String,
        alias: String,
        error: String,
    },
}

async fn account_usage(State(app): State<AppState>) -> Json<Value> {
    // The panel polls this route; serialize refreshes and reuse snapshots for a minute.
    let _refresh = USAGE_REFRESH.lock().await;
    Json(json!(load_account_usage(&app).await))
}

async fn load_account_usage(app: &AppState) -> Vec<AccountUsageResult> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs().cast_signed());
    let cached = match tokio::task::spawn_blocking(crate::stats::account_usage_cache).await {
        Ok(Ok(cached)) => cached,
        Ok(Err(error)) => {
            tracing::warn!(target: crate::LOG_TARGET, error = %error, "failed to read account usage cache");
            Vec::new()
        }
        Err(error) => {
            tracing::warn!(target: crate::LOG_TARGET, error = %error, "account usage cache task failed");
            Vec::new()
        }
    };
    let mut cached: HashMap<_, _> = cached
        .into_iter()
        .map(|usage| ((usage.provider.clone(), usage.alias.clone()), usage))
        .collect();
    let state = app.snapshot().await;
    let mut clients: Vec<_> = state
        .clients
        .iter()
        .flat_map(|(provider, aliases)| {
            aliases
                .iter()
                .filter(|(_, client)| client.has_usage_endpoint())
                .map(|(alias, client)| (provider.clone(), alias.clone(), client.clone()))
                .collect::<Vec<_>>()
        })
        .collect();
    clients.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));

    let refreshes = clients.into_iter().map(|(provider, alias, client)| {
        let previous = cached.remove(&(provider.clone(), alias.clone()));
        refresh_account_usage(provider, alias, client, previous, now)
    });
    futures_util::future::join_all(refreshes).await
}

async fn refresh_account_usage(
    provider: String,
    alias: String,
    client: crate::upstream::ProviderClient,
    previous: Option<crate::stats::AccountUsage>,
    now: i64,
) -> AccountUsageResult {
    if let Some(usage) = previous
        .as_ref()
        .filter(|usage| now.saturating_sub(usage.fetched_at) < ACCOUNT_USAGE_CACHE_SECS)
    {
        return AccountUsageResult::Available {
            usage: usage.clone(),
            stale: false,
            error: None,
        };
    }
    match client.fetch_usage().await {
        Ok(Some(usage)) => {
            persist_account_usage(usage.clone()).await;
            AccountUsageResult::Available {
                usage,
                stale: false,
                error: None,
            }
        }
        Ok(None) => AccountUsageResult::Unavailable {
            provider,
            alias,
            error: "no upstream usage endpoint is configured".to_string(),
        },
        Err(fetch_error) => {
            let message = fetch_error.to_string();
            if let Some(usage) = previous {
                AccountUsageResult::Available {
                    usage,
                    stale: true,
                    error: Some(message),
                }
            } else {
                AccountUsageResult::Unavailable {
                    provider,
                    alias,
                    error: message,
                }
            }
        }
    }
}

async fn persist_account_usage(usage: crate::stats::AccountUsage) {
    match tokio::task::spawn_blocking(move || crate::stats::save_account_usage(&usage)).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => tracing::warn!(
            target: crate::LOG_TARGET,
            error = %error,
            "failed to save account usage"
        ),
        Err(error) => tracing::warn!(
            target: crate::LOG_TARGET,
            error = %error,
            "account usage save task failed"
        ),
    }
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
    fn account_usage_route_returns_empty_list_when_no_supported_accounts_exist() {
        let _guard = crate::TEST_STATE_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("LOCAL_PROXY_CONFIG_DIR", dir.path());
        let app = crate::handlers::app(app_state(std::path::PathBuf::new()));
        block_on(async {
            let response = app
                .oneshot(request("GET", "/admin/account-usage", "", [127, 0, 0, 1]))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let body = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            assert_eq!(serde_json::from_slice::<Value>(&body).unwrap(), json!([]));
        });
        std::env::remove_var("LOCAL_PROXY_CONFIG_DIR");
    }

    #[test]
    fn account_usage_route_reports_supported_accounts_without_subscription_auth() {
        let _guard = crate::TEST_STATE_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("LOCAL_PROXY_CONFIG_DIR", dir.path());
        let config = Arc::new(crate::config::Config::default());
        let provider = crate::config::Provider {
            name: "chatgpt".to_string(),
            ..crate::config::Provider::default()
        };
        let client = crate::upstream::ProviderClient::new_for_alias(
            &provider,
            "work",
            false,
            Some(crate::auth::AuthEntry::Api {
                key: "test-key".to_string(),
            }),
        )
        .unwrap();
        let app = crate::handlers::app(AppState::new(crate::handlers::RuntimeState {
            config: config.clone(),
            router: Arc::new(crate::router::Router::new(config).unwrap()),
            clients: Arc::new(HashMap::from([(
                "chatgpt".to_string(),
                HashMap::from([("work".to_string(), client)]),
            )])),
            enforce_active_model: false,
            config_path: dir.path().join("config.yaml"),
        }));

        block_on(async {
            let response = app
                .oneshot(request("GET", "/admin/account-usage", "", [127, 0, 0, 1]))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let body = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            assert_eq!(
                serde_json::from_slice::<Value>(&body).unwrap(),
                json!([{
                    "kind": "unavailable",
                    "provider": "chatgpt",
                    "alias": "work",
                    "error": "provider chatgpt usage endpoint requires an OAuth account"
                }])
            );
        });
        std::env::remove_var("LOCAL_PROXY_CONFIG_DIR");
    }

    #[test]
    fn account_usage_route_returns_cached_snapshot_by_provider_and_alias() {
        let _guard = crate::TEST_STATE_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("LOCAL_PROXY_CONFIG_DIR", dir.path());
        let fetched_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            .cast_signed();
        crate::stats::save_account_usage(&crate::stats::AccountUsage {
            provider: "chatgpt".to_string(),
            alias: "work".to_string(),
            five_hour: Some(crate::stats::UsageWindow {
                utilization: 45.0,
                resets_at: Some("2000000000".to_string()),
            }),
            seven_day: None,
            monthly: None,
            fetched_at,
        })
        .unwrap();
        let config = Arc::new(crate::config::Config::default());
        let provider = crate::config::Provider {
            name: "chatgpt".to_string(),
            ..crate::config::Provider::default()
        };
        let client = crate::upstream::ProviderClient::new_for_alias(
            &provider,
            "work",
            false,
            Some(crate::auth::AuthEntry::Api {
                key: "test-key".to_string(),
            }),
        )
        .unwrap();
        let app = crate::handlers::app(AppState::new(crate::handlers::RuntimeState {
            config: config.clone(),
            router: Arc::new(crate::router::Router::new(config).unwrap()),
            clients: Arc::new(HashMap::from([(
                "chatgpt".to_string(),
                HashMap::from([("work".to_string(), client)]),
            )])),
            enforce_active_model: false,
            config_path: dir.path().join("config.yaml"),
        }));

        block_on(async {
            let response = app
                .oneshot(request("GET", "/admin/account-usage", "", [127, 0, 0, 1]))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let body = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            let value: Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(value[0]["kind"], "available");
            assert_eq!(value[0]["usage"]["provider"], "chatgpt");
            assert_eq!(value[0]["usage"]["alias"], "work");
            assert_eq!(value[0]["usage"]["five_hour"]["utilization"], 45.0);
            assert_eq!(value[0]["stale"], false);
        });
        std::env::remove_var("LOCAL_PROXY_CONFIG_DIR");
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
