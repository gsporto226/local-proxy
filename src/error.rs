use std::fmt;

use serde_json::{json, Value};

use crate::router::RouterError;
use crate::translate::TranslateError;
use crate::upstream::UpstreamError;

/// An error surfaced to a client, carrying a status code and a kind string
/// compatible with both the `OpenAI` and `Anthropic` error shapes.
#[derive(Debug, Clone)]
pub struct ApiError {
    /// The error kind string, e.g. `invalid_request_error`.
    pub kind: String,
    /// The HTTP status code to return to the client.
    pub status: u16,
    /// Human-readable description of the error.
    pub message: String,
    /// Optional raw upstream error body, preserved for inspection.
    pub upstream_body: Option<Value>,
}

impl ApiError {
    /// Create a new [`ApiError`] with the given HTTP status, kind, and message.
    #[must_use]
    pub fn new(status: u16, kind: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            status,
            message: message.into(),
            upstream_body: None,
        }
    }

    /// Attach a raw upstream error body to this error.
    #[must_use]
    pub fn with_upstream_body(mut self, body: Value) -> Self {
        self.upstream_body = Some(body);
        self
    }

    /// Create a `400` `invalid_request_error`.
    #[must_use]
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(400, "invalid_request_error", message)
    }

    /// Create a `401` `authentication_error`.
    #[must_use]
    pub fn unauthorized(message: impl Into<String>) -> Self {
        Self::new(401, "authentication_error", message)
    }

    /// Create a `404` `not_found_error`.
    #[must_use]
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(404, "not_found_error", message)
    }

    /// Create a `500` `api_error`.
    #[must_use]
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(500, "api_error", message)
    }

    /// Anthropic error shape: `{"type":"error","error":{"type","message"}}`.
    #[must_use]
    pub fn to_anthropic_error(&self) -> Value {
        json!({
            "type": "error",
            "error": { "type": self.kind, "message": self.message }
        })
    }

    /// `OpenAI` error shape: `{"error":{"message","type","code"}}`.
    #[must_use]
    pub fn to_openai_error(&self) -> Value {
        json!({
            "error": {
                "message": self.message,
                "type": self.kind,
                "code": null,
            }
        })
    }

    /// Tolerantly parse an upstream error body in either the Anthropic shape
    /// (`{"type":"error","error":{...}}`) or the `OpenAI` shape (`{"error":{...}}`).
    #[must_use]
    pub fn from_upstream(status: u16, body: Value) -> Self {
        let mut kind = default_kind_for_status(status).to_string();
        let mut message = format!("upstream error (status {status})");

        if let Some(err) = body.get("error") {
            if let Some(t) = err.get("type").and_then(Value::as_str) {
                kind = t.to_string();
            }
            if let Some(m) = err.get("message").and_then(Value::as_str) {
                message = m.to_string();
            }
        }
        if body.get("type").and_then(Value::as_str) == Some("error") {
            if let Some(err) = body.get("error") {
                if let Some(t) = err.get("type").and_then(Value::as_str) {
                    kind = t.to_string();
                }
                if let Some(m) = err.get("message").and_then(Value::as_str) {
                    message = m.to_string();
                }
            }
        }

        Self::new(status, kind, message).with_upstream_body(body)
    }
}

const fn default_kind_for_status(status: u16) -> &'static str {
    match status {
        400 => "invalid_request_error",
        401 => "authentication_error",
        403 => "permission_error",
        404 => "not_found_error",
        429 => "rate_limit_error",
        _ => "api_error",
    }
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}] {}: {}", self.status, self.kind, self.message)
    }
}

impl std::error::Error for ApiError {}

impl From<TranslateError> for ApiError {
    fn from(e: TranslateError) -> Self {
        Self::bad_request(e.to_string())
    }
}

impl From<UpstreamError> for ApiError {
    fn from(e: UpstreamError) -> Self {
        match e {
            UpstreamError::MissingApiKey { provider } => Self::new(
                502,
                "api_error",
                format!(
                    "provider {provider} has no API key; store one via `local-proxy connect {provider}`"
                ),
            ),
            UpstreamError::ClientBuild { source } => {
                Self::internal(format!("failed to build upstream HTTP client: {source}"))
            }
            UpstreamError::InvalidHeader { detail } => {
                Self::internal(format!("invalid upstream header: {detail}"))
            }
            UpstreamError::Request { url, source } => Self::new(
                502,
                "api_error",
                format!("upstream request to {url} failed: {source}"),
            ),
        }
    }
}

impl From<RouterError> for ApiError {
    fn from(e: RouterError) -> Self {
        match e {
            RouterError::ModelNotFound { model } => {
                Self::not_found(format!("model not found: {model}"))
            }
            RouterError::ProviderNotFound { provider } => {
                Self::internal(format!("provider not configured: {provider}"))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anthropic_error_shape() {
        let err = ApiError::new(429, "rate_limit_error", "slow down");
        let v = err.to_anthropic_error();
        assert_eq!(v["type"], "error");
        assert_eq!(v["error"]["type"], "rate_limit_error");
        assert_eq!(v["error"]["message"], "slow down");
    }

    #[test]
    fn openai_error_shape() {
        let err = ApiError::new(400, "invalid_request_error", "bad input");
        let v = err.to_openai_error();
        assert_eq!(v["error"]["message"], "bad input");
        assert_eq!(v["error"]["type"], "invalid_request_error");
        assert_eq!(v["error"]["code"], Value::Null);
    }

    #[test]
    fn parses_openai_upstream_error() {
        let body = json!({"error": {"message": "upstream refused", "type": "invalid_request_error", "code": "oops"}});
        let err = ApiError::from_upstream(400, body);
        assert_eq!(err.message, "upstream refused");
        assert_eq!(err.kind, "invalid_request_error");
        assert_eq!(err.status, 400);
    }

    #[test]
    fn parses_anthropic_upstream_error() {
        let body = json!({"type": "error", "error": {"type": "overloaded_error", "message": "server busy"}});
        let err = ApiError::from_upstream(529, body);
        assert_eq!(err.message, "server busy");
        assert_eq!(err.kind, "overloaded_error");
        assert_eq!(err.status, 529);
    }

    #[test]
    fn falls_back_to_status_kind() {
        let err = ApiError::from_upstream(500, json!({"unexpected": true}));
        assert_eq!(err.kind, "api_error");
        assert_eq!(err.message, "upstream error (status 500)");
        assert!(err.upstream_body.is_some());
    }

    #[test]
    fn router_error_maps_to_not_found() {
        let err = ApiError::from(RouterError::ModelNotFound {
            model: "nope".to_string(),
        });
        assert_eq!(err.status, 404);
        assert_eq!(err.kind, "not_found_error");
    }
}
