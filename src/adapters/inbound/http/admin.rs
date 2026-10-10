//! Loopback-only admin API (`/admin/*`, see `docs/admin-api.md`): thin JSON
//! wrappers over the application use cases, plus a typed `text/event-stream`.
#![allow(clippy::result_large_err)]

use std::convert::Infallible;
use std::net::SocketAddr;

use axum::extract::{ConnectInfo, Path, Query, Request, State};
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, put};
use axum::{Json, Router};
use futures_util::{Stream, StreamExt as _};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::application::commands::{self, apply_effort, apply_model};
use crate::application::runtime::AppState;
use crate::application::{account_usage, settings, stats_report};
use crate::domain::stats::StatsScope;

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

async fn status(State(app): State<AppState>) -> Json<Value> {
    let mut out = commands::config_json(&app).await;
    out["version"] = json!(env!("CARGO_PKG_VERSION"));
    out["port"] = json!(app.port());
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

async fn provider_list(app: &AppState) -> Result<Vec<settings::ProviderInfo>, Response> {
    let path = app.snapshot().await.config_path;
    let ports = app.ports().clone();
    blocking(move || settings::provider_accounts(&ports, &path)).await
}

async fn accounts(State(app): State<AppState>) -> ApiResult {
    let accounts: Vec<_> = provider_list(&app)
        .await?
        .into_iter()
        .flat_map(|p| p.accounts)
        .collect();
    Ok(Json(json!(accounts)))
}

async fn providers(State(app): State<AppState>) -> ApiResult {
    Ok(Json(json!(provider_list(&app).await?)))
}

async fn models(State(app): State<AppState>) -> ApiResult {
    let path = app.snapshot().await.config_path;
    let ports = app.ports().clone();
    let models = blocking(move || settings::connected_models(&ports, &path)).await?;
    Ok(Json(json!(models)))
}

#[derive(Deserialize)]
struct SinceQuery {
    since: Option<String>,
    session_id: Option<String>,
}

async fn stats(State(app): State<AppState>, Query(q): Query<SinceQuery>) -> ApiResult {
    let since = q.since.unwrap_or_else(|| "day".to_string());
    if q.session_id.as_deref() == Some("") {
        return Err(bad_request("session_id must not be empty"));
    }
    if since == "session" && q.session_id.is_none() {
        return Err(bad_request("session_id is required when since=session"));
    }
    let ports = app.ports().clone();
    let stats = blocking(move || {
        let scope = q
            .session_id
            .as_deref()
            .map_or(StatsScope::All, StatsScope::Session);
        stats_report::json(ports.usage.as_ref(), &since, scope)
    })
    .await?;
    Ok(Json(stats.unwrap_or(Value::Null)))
}

async fn rate_limits(State(app): State<AppState>) -> Json<Value> {
    let (h5, week) = app.ports().usage.rate_limits().unzip();
    Json(json!({ "h5": h5, "week": week }))
}

async fn account_usage(State(app): State<AppState>) -> Json<Value> {
    Json(json!(account_usage::load(&app).await))
}

#[derive(Deserialize)]
struct LinesQuery {
    n: Option<usize>,
}

async fn logs(State(app): State<AppState>, Query(q): Query<LinesQuery>) -> ApiResult {
    let text = app
        .ports()
        .logs
        .tail(q.n.unwrap_or(200))
        .unwrap_or_default();
    Ok(Json(json!({ "lines": text.lines().collect::<Vec<_>>() })))
}

async fn session(State(app): State<AppState>, Path(id): Path<String>) -> ApiResult {
    let usage = &app.ports().usage;
    let stats = usage.session(&id).map_err(internal)?.unwrap_or_default();
    Ok(Json(json!({
        "account": app.session_accounts(&id).await,
        "effort": usage.session_effort(&id),
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
    commands::apply_account(&app, &args)
        .await
        .map(message)
        .map_err(bad_request)
}

async fn disconnect(
    State(app): State<AppState>,
    Path((provider, alias)): Path<(String, String)>,
) -> ApiResult {
    commands::disconnect(&app, &provider, &alias)
        .await
        .map(message)
        .map_err(internal)
}

async fn events(State(app): State<AppState>) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let stream = app
        .ports()
        .events
        .subscribe()
        .map(|(name, data)| Ok(Event::default().event(name).data(data.to_string())));
    Sse::new(stream).keep_alive(KeepAlive::default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::inbound::http::app as http_app;
    use crate::application::runtime::RuntimeState;
    use crate::application::usage::UsageRecorder;
    use crate::domain::account::AuthEntry;
    use crate::domain::config::{Config, Provider};
    use crate::domain::router::Router as ModelRouter;
    use crate::domain::stats::{AccountUsage, StatLine, UsageWindow};
    use crate::ports::{AccountMap, Ports};
    use axum::body::Body;
    use std::collections::HashMap;
    use std::sync::Arc;
    use tower::ServiceExt;

    fn state(ports: Ports, config_path: std::path::PathBuf, accounts: AccountMap) -> AppState {
        let config = Arc::new(Config::default());
        AppState::new(
            RuntimeState {
                config: config.clone(),
                router: Arc::new(ModelRouter::new(config).unwrap()),
                accounts: Arc::new(accounts),
                enforce_active_model: false,
                config_path,
            },
            ports,
        )
    }

    fn app_state(config_path: std::path::PathBuf) -> AppState {
        state(crate::bootstrap::ports(), config_path, HashMap::new())
    }

    /// One `chatgpt` account named `work` holding an API key (not OAuth).
    fn chatgpt_work(ports: &Ports) -> AccountMap {
        let provider = Provider {
            name: "chatgpt".to_string(),
            ..Provider::default()
        };
        let account = ports
            .upstream
            .connect(
                &provider,
                "work",
                false,
                Some(AuthEntry::Api {
                    key: "test-key".to_string(),
                }),
            )
            .unwrap();
        HashMap::from([(
            "chatgpt".to_string(),
            HashMap::from([("work".to_string(), account)]),
        )])
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
        let app = http_app(app_state(std::path::PathBuf::new()));
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
    fn stats_route_filters_cache_rate_to_the_active_session() {
        let _guard = crate::TEST_STATE_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("LOCAL_PROXY_CONFIG_DIR", dir.path());
        let recorder = UsageRecorder::new(&crate::bootstrap::ports());
        for (session_id, cache_hit) in [
            ("sess-1", Some(false)),
            ("sess-1", Some(true)),
            ("sess-1", None),
            ("sess-2", Some(true)),
        ] {
            recorder.record(
                std::time::Instant::now(),
                StatLine {
                    endpoint: "/v1/messages",
                    provider: "anthropic".to_string(),
                    alias: "work".to_string(),
                    model: "claude-test".to_string(),
                    session_id: session_id.to_string(),
                    cache_hit,
                    ..StatLine::default()
                },
            );
        }
        let app = http_app(app_state(dir.path().join("config.yaml")));
        block_on(async {
            let response = app
                .clone()
                .oneshot(request(
                    "GET",
                    "/admin/stats?since=session&session_id=sess-1",
                    "",
                    [127, 0, 0, 1],
                ))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let body = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            let value: Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(value["scope"]["kind"], "session");
            assert_eq!(value["scope"]["session_id"], "sess-1");
            assert_eq!(value["summary"]["requests"], 3);
            assert_eq!(value["summary"]["cache"]["hit_requests"], 1);
            assert_eq!(value["summary"]["cache"]["reported_requests"], 2);
            assert_eq!(value["summary"]["cache"]["rate_percent"], 50.0);
            assert!(
                (value["summary"]["cache"]["coverage_percent"]
                    .as_f64()
                    .unwrap()
                    - 200.0 / 3.0)
                    .abs()
                    < 0.01
            );

            let response = app
                .clone()
                .oneshot(request(
                    "GET",
                    "/admin/stats?since=session",
                    "",
                    [127, 0, 0, 1],
                ))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);

            let response = app
                .oneshot(request(
                    "GET",
                    "/admin/stats?since=session&session_id=",
                    "",
                    [127, 0, 0, 1],
                ))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        });
        std::env::remove_var("LOCAL_PROXY_CONFIG_DIR");
    }

    #[test]
    fn account_usage_route_returns_empty_list_when_no_supported_accounts_exist() {
        let _guard = crate::TEST_STATE_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("LOCAL_PROXY_CONFIG_DIR", dir.path());
        let app = http_app(app_state(std::path::PathBuf::new()));
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
        let ports = crate::bootstrap::ports();
        let accounts = chatgpt_work(&ports);
        let app = http_app(state(ports, dir.path().join("config.yaml"), accounts));

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
        crate::bootstrap::ports()
            .usage
            .save_account_usage(&AccountUsage {
                provider: "chatgpt".to_string(),
                alias: "work".to_string(),
                five_hour: Some(UsageWindow {
                    utilization: 45.0,
                    resets_at: Some("2000000000".to_string()),
                }),
                seven_day: None,
                monthly: None,
                fetched_at,
            })
            .unwrap();
        let ports = crate::bootstrap::ports();
        let accounts = chatgpt_work(&ports);
        let app = http_app(state(ports, dir.path().join("config.yaml"), accounts));

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
        let app = http_app(state.clone());
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
        let state = app_state(config_path.clone());
        let app = http_app(state.clone());
        block_on(async {
            let mut rx = state.ports().events.subscribe();
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
            let saved = state.ports().config.load(&config_path).unwrap();
            assert_eq!(saved.defaults.active_effort.as_deref(), Some("high"));
            loop {
                let (name, data) = rx.next().await.unwrap();
                if name == "config" {
                    assert_eq!(data["effort"], "high");
                    break;
                }
            }
        });
        std::env::remove_var("LOCAL_PROXY_CONFIG_DIR");
    }
}
