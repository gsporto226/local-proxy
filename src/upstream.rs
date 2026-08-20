use std::time::Duration;

use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use serde_json::Value;

use crate::config::{Provider, ProviderFormat};
const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Errors that can occur while building clients or talking to upstreams.
#[derive(Debug, thiserror::Error)]
pub enum UpstreamError {
    /// The provider has no API key in the auth store.
    #[error("provider {provider} has no API key; store one via `local-proxy connect {provider}`")]
    MissingApiKey {
        /// Name of the provider missing a key.
        provider: String,
    },
    /// Failed to build the underlying HTTP client.
    #[error("failed to build HTTP client: {source}")]
    ClientBuild {
        /// Underlying client construction error.
        source: reqwest::Error,
    },
    /// A header value could not be constructed.
    #[error("invalid upstream header: {detail}")]
    InvalidHeader {
        /// Human-readable description of the invalid header.
        detail: String,
    },
    /// An upstream request failed.
    #[error("upstream request to {url} failed: {source}")]
    Request {
        /// URL that was requested.
        url: String,
        /// Underlying request error.
        source: reqwest::Error,
    },
}

/// A per-provider HTTP client that knows how to authenticate against the
/// upstream (Anthropic or `OpenAI`) and honors the passthrough-keys policy.
#[derive(Debug, Clone)]
pub struct ProviderClient {
    name: String,
    base_url: String,
    format: ProviderFormat,
    auth_key: Option<String>,
    passthrough: bool,
    headers: std::collections::HashMap<String, String>,
    http: reqwest::Client,
}

impl ProviderClient {
    /// Build a client for `provider` honoring the given `passthrough` policy.
    /// `auth_key` is the provider's API key resolved from the auth store.
    ///
    /// # Errors
    ///
    /// Returns [`UpstreamError::ClientBuild`] if the HTTP client cannot be
    /// created.
    pub fn new(
        provider: &Provider,
        passthrough: bool,
        auth_key: Option<String>,
    ) -> Result<Self, UpstreamError> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_mins(10))
            .build()
            .map_err(|source| UpstreamError::ClientBuild { source })?;
        Ok(Self {
            name: provider.name.clone(),
            base_url: provider.base_url.trim_end_matches('/').to_string(),
            format: provider.format,
            auth_key,
            passthrough,
            headers: provider.headers.clone(),
            http,
        })
    }

    /// The provider's name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The provider's wire format.
    #[must_use]
    pub const fn format(&self) -> ProviderFormat {
        self.format
    }

    /// Whether this client has an API key available from the auth store.
    #[must_use]
    pub fn has_key(&self) -> bool {
        self.auth_key.as_deref().is_some_and(|k| !k.is_empty())
    }

    /// Default endpoint path for this provider's format.
    #[must_use]
    pub const fn default_path(&self) -> &'static str {
        match self.format {
            ProviderFormat::Anthropic => "/v1/messages",
            ProviderFormat::Openai => "/v1/chat/completions",
        }
    }

    fn configured_key(&self) -> Option<String> {
        self.auth_key
            .as_deref()
            .filter(|k| !k.is_empty())
            .map(str::to_string)
    }

    fn effective_key(&self, client_key: Option<&str>) -> Option<String> {
        if self.passthrough {
            if let Some(key) = client_key.filter(|k| !k.is_empty()) {
                return Some(key.to_string());
            }
        }
        self.configured_key()
    }

    fn header_value(raw: &str) -> Result<HeaderValue, UpstreamError> {
        HeaderValue::from_str(raw).map_err(|e| UpstreamError::InvalidHeader {
            detail: format!("{raw:?}: {e}"),
        })
    }

    /// POST `body` to `path` (defaulting to this provider's endpoint) with the
    /// correct auth headers. Returns the raw response for the caller to read.
    ///
    /// # Errors
    ///
    /// Returns [`UpstreamError::MissingApiKey`] if no API key is available,
    /// [`UpstreamError::InvalidHeader`] if a header cannot be built, or
    /// [`UpstreamError::Request`] if the HTTP request fails.
    pub async fn chat_request(
        &self,
        path: &str,
        body: Value,
        client_key: Option<&str>,
    ) -> Result<reqwest::Response, UpstreamError> {
        let key = self.effective_key(client_key);
        if key.is_none() {
            return Err(UpstreamError::MissingApiKey {
                provider: self.name.clone(),
            });
        }
        let path = if path.is_empty() {
            self.default_path()
        } else {
            path
        };
        let url = format!("{}{}", self.base_url, path);
        tracing::debug!(
            target: crate::LOG_TARGET,
            provider = %self.name,
            url = %url,
            has_key = key.is_some(),
            "sending upstream request"
        );

        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        if let Some(key) = key {
            match self.format {
                ProviderFormat::Anthropic => {
                    headers.insert("x-api-key", Self::header_value(&key)?);
                    headers.insert(
                        "anthropic-version",
                        HeaderValue::from_static(ANTHROPIC_VERSION),
                    );
                }
                ProviderFormat::Openai => {
                    headers.insert(AUTHORIZATION, Self::header_value(&format!("Bearer {key}"))?);
                }
            }
        }
        // Provider-configured static headers override the format/auth defaults.
        for (name, value) in &self.headers {
            let (Ok(name), Ok(value)) = (
                reqwest::header::HeaderName::from_bytes(name.as_bytes()),
                HeaderValue::from_str(value),
            ) else {
                continue;
            };
            headers.insert(name, value);
        }

        let resp = self
            .http
            .post(&url)
            .headers(headers)
            .json(&body)
            .send()
            .await
            .map_err(|source| UpstreamError::Request { url, source })?;
        Ok(resp)
    }
}

/// Whether `provider` has an API key available from the auth store.
///
/// Mirrors [`ProviderClient::configured_key`] without building an HTTP client,
/// so it can be used to decide "connected" providers.
#[must_use]
pub fn provider_has_key(provider: &Provider, auth_key: Option<&str>) -> bool {
    let _ = provider;
    auth_key.is_some_and(|k| !k.is_empty())
}

/// Read a completed upstream response into `(status, json_body)`, tolerating a
/// non-JSON body (yields `Value::Null`).
pub async fn send_and_read(resp: reqwest::Response) -> (u16, Value) {
    let status = resp.status().as_u16();
    let body = resp.json::<Value>().await.unwrap_or(Value::Null);
    (status, body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn provider(format: ProviderFormat) -> Provider {
        Provider {
            name: "test".to_string(),
            base_url: "http://127.0.0.1:9".to_string(),
            format,
            models: Vec::new(),
            headers: std::collections::HashMap::new(),
        }
    }

    #[test]
    fn default_paths_per_format() {
        assert_eq!(
            ProviderClient::new(&provider(ProviderFormat::Anthropic), false, None)
                .unwrap()
                .default_path(),
            "/v1/messages"
        );
        assert_eq!(
            ProviderClient::new(&provider(ProviderFormat::Openai), false, None)
                .unwrap()
                .default_path(),
            "/v1/chat/completions"
        );
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn missing_key_is_an_error() {
        let _guard = crate::TEST_STATE_LOCK.lock().unwrap();
        let client =
            ProviderClient::new(&provider(ProviderFormat::Anthropic), false, None).unwrap();
        let err = client
            .chat_request("/v1/messages", json!({}), None)
            .await
            .unwrap_err();
        assert!(matches!(err, UpstreamError::MissingApiKey { .. }));
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn effective_key_passthrough_prefers_client_key() {
        let _guard = crate::TEST_STATE_LOCK.lock().unwrap();
        let p = provider(ProviderFormat::Anthropic);
        let client = ProviderClient::new(&p, true, Some("configured-key".to_string())).unwrap();
        let key = client.effective_key(Some("client-key"));
        assert_eq!(key.as_deref(), Some("client-key"));
        // without a client key, falls back to the configured key
        let key = client.effective_key(None);
        assert_eq!(key.as_deref(), Some("configured-key"));
        // without passthrough, client key is ignored
        let client = ProviderClient::new(&p, false, Some("configured-key".to_string())).unwrap();
        let key = client.effective_key(Some("client-key"));
        assert_eq!(key.as_deref(), Some("configured-key"));
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn key_resolution_uses_auth_only() {
        let _guard = crate::TEST_STATE_LOCK.lock().unwrap();

        // auth key is used
        let p = provider(ProviderFormat::Anthropic);
        let client = ProviderClient::new(&p, false, Some("auth-key".to_string())).unwrap();
        assert_eq!(client.configured_key().as_deref(), Some("auth-key"));

        // no key at all
        let p = provider(ProviderFormat::Anthropic);
        let client = ProviderClient::new(&p, false, None).unwrap();
        assert_eq!(client.configured_key(), None);
    }

    /// Spin up a one-shot HTTP server that records the request headers it
    /// receives and returns them in the response body as JSON. Returns
    /// `(base_url, received_headers)`.
    async fn header_capture_server() -> (String, tokio::sync::oneshot::Receiver<HeaderMap>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 4096];
            let _ = sock.read(&mut buf).await;
            // parse the head: split headers from body on \r\n\r\n
            let head_end = buf
                .windows(4)
                .position(|w| w == b"\r\n\r\n")
                .unwrap_or(buf.len());
            let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
            let mut headers = HeaderMap::new();
            for line in head.lines().skip(1) {
                if let Some((k, v)) = line.split_once(':') {
                    if let (Ok(k), Ok(v)) = (
                        reqwest::header::HeaderName::from_bytes(k.trim().as_bytes()),
                        HeaderValue::from_str(v.trim()),
                    ) {
                        headers.append(k, v);
                    }
                }
            }
            let _ = tx.send(headers);
            let body = b"{}";
            let _ = sock
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await;
            let _ = sock.write_all(body).await;
        });
        (format!("http://{addr}"), rx)
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn provider_headers_are_attached_to_request() {
        let _guard = crate::TEST_STATE_LOCK.lock().unwrap();

        let (base, rx) = header_capture_server().await;
        let mut p = provider(ProviderFormat::Openai);
        p.base_url = base.clone();
        p.headers = std::collections::HashMap::from([
            (
                "HTTP-Referer".to_string(),
                "https://example.com".to_string(),
            ),
            ("X-Title".to_string(), "local-proxy".to_string()),
        ]);
        let client = ProviderClient::new(&p, false, Some("key".to_string())).unwrap();
        client
            .chat_request("/v1/chat/completions", json!({}), None)
            .await
            .unwrap();

        let received = rx.await.unwrap();
        assert_eq!(
            received.get("http-referer").and_then(|v| v.to_str().ok()),
            Some("https://example.com")
        );
        assert_eq!(
            received.get("x-title").and_then(|v| v.to_str().ok()),
            Some("local-proxy")
        );
        assert_eq!(
            received.get("authorization").and_then(|v| v.to_str().ok()),
            Some("Bearer key")
        );
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn provider_headers_override_auth_default() {
        let _guard = crate::TEST_STATE_LOCK.lock().unwrap();

        let (base, rx) = header_capture_server().await;
        let mut p = provider(ProviderFormat::Openai);
        p.base_url = base.clone();
        p.headers = std::collections::HashMap::from([(
            "Authorization".to_string(),
            "Bearer custom".to_string(),
        )]);
        let client = ProviderClient::new(&p, false, Some("key".to_string())).unwrap();
        client
            .chat_request("/v1/chat/completions", json!({}), None)
            .await
            .unwrap();

        let received = rx.await.unwrap();
        assert_eq!(
            received.get("authorization").and_then(|v| v.to_str().ok()),
            Some("Bearer custom")
        );
    }
}
