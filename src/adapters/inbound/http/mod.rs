//! The HTTP adapter: provider-compatible endpoints plus the loopback admin API.

use axum::routing::{get, post};
use axum::Router;

use crate::application::runtime::AppState;

pub mod admin;
pub mod proxy;

/// The axum [`Router`] wiring up every route with the given state.
pub fn app(state: AppState) -> Router {
    Router::new()
        .route("/health", get(proxy::health))
        .route("/v1/messages", post(proxy::messages))
        .route("/v1/messages/count_tokens", post(proxy::count_tokens))
        .route("/v1/chat/completions", post(proxy::chat_completions))
        .route("/v1/responses", post(proxy::responses))
        .route("/v1/models", get(proxy::models))
        .layer(tower_http::trace::TraceLayer::new_for_http())
        // Merged after the trace layer: the mod polls /admin, and tracing those
        // calls would feed its own `log` event stream back to it.
        .merge(admin::routes())
        .with_state(state)
}
