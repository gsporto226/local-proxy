use std::time::Duration;

use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use serde_json::Value;

use crate::config::{Provider, ProviderFormat};

const ANTHROPIC_VERSION: &str = "2023-06-01";

#[derive(Debug, thiserror::Error)]
pub enum UpstreamError {
    #[error("missing API key for provider {provider}: set env var {env}")]
    MissingApiKey { provider: String, env: String },
    #[error("failed to build HTTP client: {source}")]
    ClientBuild { source: reqwest::Error },
    #[error("invalid upstream header: {detail}")]
    InvalidHeader { detail: String },
    #[error("upstream request to {url} failed: {source}")]
    Request { url: String, source: reqwest::Error },
}

/// A per-provider HTTP client that knows how to authenticate against the
/// upstream (Anthropic or OpenAI) and honors the passthrough-keys policy.
#[derive(Debug, Clone)]
pub struct ProviderClient {
    name: String,
    base_url: String,
    format: ProviderFormat,
    api_key_env: Option<String>,
    passthrough: bool,
    http: reqwest::Client,
}

impl ProviderClient {
    pub fn new(provider: &Provider, passthrough: bool) -> Result<Self, UpstreamError> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(600))
            .build()
            .map_err(|source| UpstreamError::ClientBuild { source })?;
        Ok(Self {
            name: provider.name.clone(),
            base_url: provider.base_url.trim_end_matches('/').to_string(),
            format: provider.format,
            api_key_env: provider.api_key_env.clone(),
            passthrough,
            http,
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn format(&self) -> ProviderFormat {
        self.format
    }

    /// Default endpoint path for this provider's format.
    pub fn default_path(&self) -> &'static str {
        match self.format {
            ProviderFormat::Anthropic => "/v1/messages",
            ProviderFormat::Openai => "/v1/chat/completions",
        }
    }

    fn configured_key(&self) -> Result<Option<String>, UpstreamError> {
        match &self.api_key_env {
            Some(env) => match std::env::var(env) {
                Ok(key) if !key.is_empty() => Ok(Some(key)),
                _ => Err(UpstreamError::MissingApiKey {
                    provider: self.name.clone(),
                    env: env.clone(),
                }),
            },
            None => Ok(None),
        }
    }

    fn effective_key(&self, client_key: Option<&str>) -> Result<Option<String>, UpstreamError> {
        if self.passthrough {
            if let Some(key) = client_key.filter(|k| !k.is_empty()) {
                return Ok(Some(key.to_string()));
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
    pub async fn chat_request(
        &self,
        path: &str,
        body: Value,
        client_key: Option<&str>,
    ) -> Result<reqwest::Response, UpstreamError> {
        let key = self.effective_key(client_key)?;
        let path = if path.is_empty() {
            self.default_path()
        } else {
            path
        };
        let url = format!("{}{}", self.base_url, path);

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
            api_key_env: Some("LOCAL_PROXY_TEST_KEY".to_string()),
            format,
            models: Vec::new(),
        }
    }

    #[test]
    fn default_paths_per_format() {
        assert_eq!(
            ProviderClient::new(&provider(ProviderFormat::Anthropic), false)
                .unwrap()
                .default_path(),
            "/v1/messages"
        );
        assert_eq!(
            ProviderClient::new(&provider(ProviderFormat::Openai), false)
                .unwrap()
                .default_path(),
            "/v1/chat/completions"
        );
    }

    #[tokio::test]
    async fn missing_env_key_is_an_error() {
        std::env::remove_var("LOCAL_PROXY_TEST_MISSING_KEY");
        let mut p = provider(ProviderFormat::Anthropic);
        p.api_key_env = Some("LOCAL_PROXY_TEST_MISSING_KEY".to_string());
        let client = ProviderClient::new(&p, false).unwrap();
        let err = client
            .chat_request("/v1/messages", json!({}), None)
            .await
            .unwrap_err();
        assert!(matches!(err, UpstreamError::MissingApiKey { .. }));
    }

    #[tokio::test]
    async fn effective_key_passthrough_prefers_client_key() {
        std::env::set_var("LOCAL_PROXY_TEST_KEY", "configured-key");
        let client = ProviderClient::new(&provider(ProviderFormat::Anthropic), true).unwrap();
        let key = client.effective_key(Some("client-key")).unwrap();
        assert_eq!(key.as_deref(), Some("client-key"));
        // without a client key, falls back to the configured key
        let key = client.effective_key(None).unwrap();
        assert_eq!(key.as_deref(), Some("configured-key"));
        // without passthrough, client key is ignored
        let client = ProviderClient::new(&provider(ProviderFormat::Anthropic), false).unwrap();
        let key = client.effective_key(Some("client-key")).unwrap();
        assert_eq!(key.as_deref(), Some("configured-key"));
        std::env::remove_var("LOCAL_PROXY_TEST_KEY");
    }
}
