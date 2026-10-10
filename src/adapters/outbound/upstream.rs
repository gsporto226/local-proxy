//! [`UpstreamConnector`] over HTTP: per-account `reqwest` clients that know
//! how to authenticate against each provider format, refresh OAuth tokens,
//! and read subscription usage.

use std::sync::Arc;
use std::time::Duration;

use futures_util::future::BoxFuture;
use futures_util::StreamExt as _;
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE, USER_AGENT};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::domain::account::{AuthEntry, AuthMap, OAuthTokens};
use crate::domain::config::{Config, OAuthProvider, Provider, ProviderFormat};
use crate::domain::oauth::{now_ms, REFRESH_LEEWAY_MS};
use crate::domain::sse::StreamError;
use crate::domain::stats::{AccountUsage, UsageWindow};
use crate::ports::{
    Account, ClientHints, CredentialStore, OAuthClient, UpstreamAccount, UpstreamConnector,
    UpstreamError, UpstreamRequest, UpstreamResponse,
};

const ANTHROPIC_VERSION: &str = "2023-06-01";

fn usage_endpoint(provider: &str) -> Option<&'static str> {
    match provider {
        "chatgpt" => Some("https://chatgpt.com/backend-api/wham/usage"),
        "claude" => Some("https://api.anthropic.com/api/oauth/usage"),
        _ => None,
    }
}

/// Builds [`ProviderClient`]s; refreshed OAuth tokens are persisted through
/// the injected credential store.
#[derive(Clone)]
pub struct HttpUpstreamConnector {
    credentials: Arc<dyn CredentialStore>,
    oauth: Arc<dyn OAuthClient>,
}

impl HttpUpstreamConnector {
    /// A connector persisting refreshed tokens to `credentials`.
    #[must_use]
    pub fn new(credentials: Arc<dyn CredentialStore>, oauth: Arc<dyn OAuthClient>) -> Self {
        Self { credentials, oauth }
    }

    /// A concrete client for a named account of `provider`.
    ///
    /// # Errors
    ///
    /// Returns [`UpstreamError::ClientBuild`] if the HTTP client cannot be created.
    pub fn client(
        &self,
        provider: &Provider,
        alias: &str,
        passthrough: bool,
        auth: Option<AuthEntry>,
    ) -> Result<ProviderClient, UpstreamError> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_mins(10))
            .build()
            .map_err(|e| UpstreamError::ClientBuild {
                message: e.to_string(),
            })?;
        let oauth_state = match (&auth, &provider.oauth) {
            (Some(AuthEntry::OAuth(tokens)), Some(config)) => {
                Some(Arc::new(tokio::sync::Mutex::new(OAuthState {
                    tokens: tokens.clone(),
                    config: config.clone(),
                })))
            }
            _ => None,
        };
        let (oauth_headers, identity) = provider.oauth.as_ref().map_or_else(
            || (std::collections::HashMap::new(), None),
            |c| (c.headers.clone(), c.identity.clone()),
        );
        Ok(ProviderClient {
            name: provider.name.clone(),
            alias: alias.to_string(),
            base_url: provider.base_url.trim_end_matches('/').to_string(),
            format: provider.format,
            auth,
            oauth_state,
            oauth_headers,
            identity,
            account_header: provider
                .oauth
                .as_ref()
                .and_then(|c| c.account_id_header.clone()),
            passthrough,
            headers: provider.headers.clone(),
            session_header: provider.session_header.clone(),
            http,
            credentials: self.credentials.clone(),
            oauth: self.oauth.clone(),
        })
    }
}

impl UpstreamConnector for HttpUpstreamConnector {
    fn connect(
        &self,
        provider: &Provider,
        alias: &str,
        passthrough: bool,
        credential: Option<AuthEntry>,
    ) -> Result<Account, UpstreamError> {
        Ok(Arc::new(self.client(
            provider,
            alias,
            passthrough,
            credential,
        )?))
    }

    fn discover_models(&self, config: &mut Config, accounts: &AuthMap) {
        let targets: Vec<(usize, ProviderClient)> = config
            .providers
            .iter()
            .enumerate()
            .filter(|(_, p)| p.models.is_empty())
            .filter_map(|(i, p)| {
                // Any usable account can answer the models query; sort for a
                // deterministic pick across runs.
                let entries = accounts.get(&p.name)?;
                let mut aliases: Vec<_> = entries.iter().collect();
                aliases.sort_unstable_by(|a, b| a.0.cmp(b.0));
                let (alias, entry) = aliases.into_iter().find(|(_, entry)| entry.usable())?;
                let client = self.client(p, alias, false, Some(entry.clone())).ok()?;
                Some((i, client))
            })
            .collect();
        if targets.is_empty() {
            return;
        }
        // Runs on its own thread and runtime, so it is safe to call from
        // inside an async context.
        let found = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .ok()?;
            Some(
                rt.block_on(futures_util::future::join_all(
                    targets
                        .into_iter()
                        .map(|(i, c)| async move { (i, c.list_models().await) }),
                )),
            )
        })
        .join()
        .ok()
        .flatten()
        .unwrap_or_default();
        for (i, models) in found {
            config.providers[i].models = models;
        }
    }
}

/// Mutable OAuth state shared by every clone of a provider's client: the token
/// bundle plus the recipe needed to refresh it.
#[derive(Debug)]
struct OAuthState {
    tokens: OAuthTokens,
    config: OAuthProvider,
}

#[derive(Deserialize)]
struct ChatGptUsageResponse {
    rate_limit: Option<ChatGptRateLimit>,
}

#[derive(Deserialize)]
struct ChatGptRateLimit {
    primary_window: Option<ChatGptWindow>,
    secondary_window: Option<ChatGptWindow>,
}

#[derive(Deserialize)]
struct ChatGptWindow {
    used_percent: Option<f64>,
    limit_window_seconds: Option<u64>,
    reset_at: Option<i64>,
}

#[derive(Deserialize)]
struct ClaudeUsageResponse {
    five_hour: Option<ClaudeWindow>,
    seven_day: Option<ClaudeWindow>,
    extra_usage: Option<ClaudeExtraUsage>,
}

#[derive(Deserialize)]
struct ClaudeWindow {
    utilization: f64,
    resets_at: Option<String>,
}

#[derive(Deserialize)]
struct ClaudeExtraUsage {
    utilization: Option<f64>,
    resets_at: Option<String>,
}

type UsageWindows = (
    Option<UsageWindow>,
    Option<UsageWindow>,
    Option<UsageWindow>,
);

fn parse_chatgpt_usage(body: &Value) -> Option<UsageWindows> {
    let response: ChatGptUsageResponse = serde_json::from_value(body.clone()).ok()?;
    let mut windows = (None, None, None);
    let rate_limit = response.rate_limit?;
    for window in [rate_limit.primary_window, rate_limit.secondary_window]
        .into_iter()
        .flatten()
    {
        let (Some(utilization), Some(seconds)) = (window.used_percent, window.limit_window_seconds)
        else {
            continue;
        };
        let quota = UsageWindow {
            utilization,
            resets_at: window.reset_at.map(|timestamp| timestamp.to_string()),
        };
        match seconds {
            18_000 => windows.0 = Some(quota),
            604_800 => windows.1 = Some(quota),
            2_592_000 => windows.2 = Some(quota),
            _ => {}
        }
    }
    (windows.0.is_some() || windows.1.is_some() || windows.2.is_some()).then_some(windows)
}

fn parse_claude_usage(body: &Value) -> Option<UsageWindows> {
    let response: ClaudeUsageResponse = serde_json::from_value(body.clone()).ok()?;
    let five_hour = response.five_hour.map(|window| UsageWindow {
        utilization: window.utilization,
        resets_at: window.resets_at,
    });
    let seven_day = response.seven_day.map(|window| UsageWindow {
        utilization: window.utilization,
        resets_at: window.resets_at,
    });
    let monthly = response.extra_usage.and_then(|usage| {
        usage.utilization.map(|utilization| UsageWindow {
            utilization,
            resets_at: usage.resets_at,
        })
    });
    (five_hour.is_some() || seven_day.is_some() || monthly.is_some())
        .then_some((five_hour, seven_day, monthly))
}

/// Subscription quota percents `(5h, weekly)` from response headers: `ChatGPT`
/// (Codex) reports percents, Claude subscriptions (OAuth) 0..1 fractions.
fn rate_limits_from(headers: &HeaderMap) -> Option<(f64, f64)> {
    let pct = |h: &str| {
        headers
            .get(h)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<f64>().ok())
    };
    let codex = pct("x-codex-primary-used-percent").zip(pct("x-codex-secondary-used-percent"));
    let claude = pct("anthropic-ratelimit-unified-5h-utilization")
        .zip(pct("anthropic-ratelimit-unified-7d-utilization"))
        .map(|(h5, week)| (h5 * 100.0, week * 100.0));
    codex.or(claude)
}

/// Adapt a `reqwest` response into the port's unread response.
fn into_upstream(resp: reqwest::Response) -> UpstreamResponse {
    let content_type = resp
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    UpstreamResponse {
        status: resp.status().as_u16(),
        content_type,
        rate_limits: rate_limits_from(resp.headers()),
        body: Box::pin(
            resp.bytes_stream()
                .map(|chunk| chunk.map_err(|e| StreamError(e.to_string()))),
        ),
    }
}

/// A per-account HTTP client that knows how to authenticate against the
/// upstream (Anthropic or `OpenAI`) and honors the passthrough-keys policy.
#[derive(Clone)]
pub struct ProviderClient {
    name: String,
    alias: String,
    base_url: String,
    format: ProviderFormat,
    auth: Option<AuthEntry>,
    oauth_state: Option<Arc<tokio::sync::Mutex<OAuthState>>>,
    oauth_headers: std::collections::HashMap<String, String>,
    identity: Option<String>,
    account_header: Option<String>,
    passthrough: bool,
    headers: std::collections::HashMap<String, String>,
    session_header: Option<String>,
    http: reqwest::Client,
    credentials: Arc<dyn CredentialStore>,
    oauth: Arc<dyn OAuthClient>,
}

impl std::fmt::Debug for ProviderClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderClient")
            .field("name", &self.name)
            .field("alias", &self.alias)
            .field("base_url", &self.base_url)
            .field("format", &self.format)
            .finish_non_exhaustive()
    }
}

impl UpstreamAccount for ProviderClient {
    fn provider(&self) -> &str {
        &self.name
    }

    fn alias(&self) -> &str {
        &self.alias
    }

    fn has_credentials(&self) -> bool {
        self.auth.as_ref().is_some_and(AuthEntry::usable)
    }

    fn has_usage_endpoint(&self) -> bool {
        usage_endpoint(&self.name).is_some()
    }

    fn send(
        &self,
        request: UpstreamRequest,
    ) -> BoxFuture<'_, Result<UpstreamResponse, UpstreamError>> {
        Box::pin(self.post(request))
    }

    fn fetch_usage(&self) -> BoxFuture<'_, Result<Option<AccountUsage>, UpstreamError>> {
        Box::pin(async move {
            let Some(url) = usage_endpoint(&self.name) else {
                return Ok(None);
            };
            self.fetch_usage_at(url).await.map(Some)
        })
    }
}

impl ProviderClient {
    async fn fetch_usage_at(&self, url: &str) -> Result<AccountUsage, UpstreamError> {
        let Some(access) = self.oauth_access().await else {
            return Err(UpstreamError::UsageRequiresOAuth {
                provider: self.name.clone(),
            });
        };

        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            Self::header_value(&format!("Bearer {access}"))?,
        );
        self.insert_oauth_headers(&mut headers);
        if let (Some(name), Some(account)) = (&self.account_header, self.oauth_account().await) {
            let name =
                reqwest::header::HeaderName::from_bytes(name.as_bytes()).map_err(|error| {
                    UpstreamError::InvalidHeader {
                        detail: error.to_string(),
                    }
                })?;
            headers.insert(name, Self::header_value(&account)?);
        }
        for (name, value) in &self.headers {
            let (Ok(name), Ok(value)) = (
                reqwest::header::HeaderName::from_bytes(name.as_bytes()),
                HeaderValue::from_str(value),
            ) else {
                continue;
            };
            headers.insert(name, value);
        }

        let response = self
            .http
            .get(url)
            .headers(headers)
            .timeout(Duration::from_secs(10))
            .send()
            .await
            .map_err(|source| UpstreamError::Request {
                url: url.to_string(),
                message: source.to_string(),
            })?;
        if !response.status().is_success() {
            return Err(UpstreamError::UsageStatus {
                provider: self.name.clone(),
                status: response.status().as_u16(),
            });
        }
        let body = response
            .json::<Value>()
            .await
            .map_err(|source| UpstreamError::Request {
                url: url.to_string(),
                message: source.to_string(),
            })?;
        let windows = match self.name.as_str() {
            "chatgpt" => parse_chatgpt_usage(&body),
            "claude" => parse_claude_usage(&body),
            _ => None,
        }
        .ok_or_else(|| UpstreamError::UsageData {
            provider: self.name.clone(),
        })?;
        let fetched_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |duration| duration.as_secs().cast_signed());
        Ok(AccountUsage {
            provider: self.name.clone(),
            alias: self.alias.clone(),
            five_hour: windows.0,
            seven_day: windows.1,
            monthly: windows.2,
            fetched_at,
        })
    }

    /// Default endpoint path for this provider's format.
    #[must_use]
    pub const fn default_path(&self) -> &'static str {
        match self.format {
            ProviderFormat::Anthropic => "/v1/messages",
            ProviderFormat::Openai => "/v1/chat/completions",
            ProviderFormat::OpenaiResponses => "/responses",
        }
    }

    fn configured_key(&self) -> Option<String> {
        self.auth
            .as_ref()
            .and_then(AuthEntry::api_key)
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

    /// Current OAuth access token, refreshing first when it is at or inside
    /// [`REFRESH_LEEWAY_MS`] of expiry. Refreshes are serialized per provider
    /// by the shared state mutex, so concurrent requests refresh once. A
    /// failed refresh keeps the current token; the upstream answers 401 if it
    /// is truly expired.
    async fn oauth_access(&self) -> Option<String> {
        let Some(state) = &self.oauth_state else {
            return self
                .auth
                .as_ref()
                .and_then(AuthEntry::oauth)
                .map(|t| t.access.clone())
                .filter(|a| !a.is_empty());
        };
        let mut guard = state.lock().await;
        if guard.tokens.expires <= now_ms() + REFRESH_LEEWAY_MS {
            tracing::info!(target: crate::LOG_TARGET, provider = %self.name, "refreshing oauth token");
            match self.oauth.refresh(&guard.config, &guard.tokens).await {
                Ok(fresh) => {
                    if let Err(e) = self
                        .credentials
                        .update_oauth(&self.name, &self.alias, &fresh)
                    {
                        tracing::warn!(
                            target: crate::LOG_TARGET,
                            provider = %self.name,
                            error = %e,
                            "failed to persist refreshed oauth token"
                        );
                    }
                    guard.tokens = fresh;
                }
                Err(e) => {
                    tracing::warn!(
                        target: crate::LOG_TARGET,
                        provider = %self.name,
                        error = %e,
                        "oauth refresh failed; using current token"
                    );
                }
            }
        }
        let access = guard.tokens.access.clone();
        drop(guard);
        (!access.is_empty()).then_some(access)
    }

    /// Account id stored with the OAuth tokens, for `oauth.account_id_header`.
    async fn oauth_account(&self) -> Option<String> {
        match &self.oauth_state {
            Some(state) => state.lock().await.tokens.account_id.clone(),
            None => self
                .auth
                .as_ref()
                .and_then(AuthEntry::oauth)?
                .account_id
                .clone(),
        }
    }

    /// Insert the provider's OAuth-only headers (beta flags, app identity).
    fn insert_oauth_headers(&self, headers: &mut HeaderMap) {
        for (name, value) in &self.oauth_headers {
            let (Ok(name), Ok(value)) = (
                reqwest::header::HeaderName::from_bytes(name.as_bytes()),
                HeaderValue::from_str(value),
            ) else {
                continue;
            };
            headers.insert(name, value);
        }
    }

    fn header_value(raw: &str) -> Result<HeaderValue, UpstreamError> {
        HeaderValue::from_str(raw).map_err(|e| UpstreamError::InvalidHeader {
            detail: format!("{raw:?}: {e}"),
        })
    }

    /// POST the request body to the provider's chat endpoint with this
    /// account's credentials and headers.
    #[allow(clippy::too_many_lines)]
    async fn post(&self, request: UpstreamRequest) -> Result<UpstreamResponse, UpstreamError> {
        let UpstreamRequest {
            mut body,
            client_key,
            session_id,
            hints,
        } = request;
        let is_oauth = matches!(self.auth, Some(AuthEntry::OAuth(_)));
        let oauth_access = if is_oauth {
            self.oauth_access().await
        } else {
            None
        };
        let key = self.effective_key(client_key.as_deref());
        if oauth_access.is_none() && key.is_none() {
            return Err(UpstreamError::MissingApiKey {
                provider: self.name.clone(),
            });
        }
        let url = format!("{}{}", self.base_url, self.default_path());
        tracing::debug!(
            target: crate::LOG_TARGET,
            provider = %self.name,
            url = %url,
            has_key = key.is_some() || oauth_access.is_some(),
            oauth = oauth_access.is_some(),
            "sending upstream request"
        );

        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        if let Some(access) = &oauth_access {
            headers.insert(
                AUTHORIZATION,
                Self::header_value(&format!("Bearer {access}"))?,
            );
            if self.format == ProviderFormat::Anthropic {
                headers.insert(
                    "anthropic-version",
                    HeaderValue::from_static(ANTHROPIC_VERSION),
                );
            }
            self.insert_oauth_headers(&mut headers);
            if let (Some(name), Some(account)) = (&self.account_header, self.oauth_account().await)
            {
                headers.insert(
                    reqwest::header::HeaderName::from_bytes(name.as_bytes()).map_err(|e| {
                        UpstreamError::InvalidHeader {
                            detail: e.to_string(),
                        }
                    })?,
                    Self::header_value(&account)?,
                );
            }
            if self.format == ProviderFormat::Anthropic {
                if let Some(identity) = &self.identity {
                    inject_identity(&mut body, identity);
                }
            }
        } else if let Some(key) = key {
            match self.format {
                ProviderFormat::Anthropic => {
                    headers.insert("x-api-key", Self::header_value(&key)?);
                    headers.insert(
                        "anthropic-version",
                        HeaderValue::from_static(ANTHROPIC_VERSION),
                    );
                }
                ProviderFormat::Openai | ProviderFormat::OpenaiResponses => {
                    headers.insert(AUTHORIZATION, Self::header_value(&format!("Bearer {key}"))?);
                }
            }
        }
        if let Some(name) = &self.session_header {
            // Clients without a session id share one stable id per process.
            let session = if session_id.is_empty() {
                format!("local-proxy-{}", std::process::id())
            } else {
                session_id.clone()
            };
            if let (Ok(name), Ok(value)) = (
                reqwest::header::HeaderName::from_bytes(name.as_bytes()),
                HeaderValue::from_str(&session),
            ) {
                headers.insert(name, value);
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
        apply_hints(&mut headers, &hints, self.format)?;

        tracing::debug!(
            target: crate::LOG_TARGET,
            provider = %self.name,
            body_bytes = body.to_string().len(),
            "sending upstream request body"
        );

        let resp = self
            .http
            .post(&url)
            .headers(headers)
            .json(&body)
            .send()
            .await
            .map_err(|source| UpstreamError::Request {
                url,
                message: source.to_string(),
            })?;
        Ok(into_upstream(resp))
    }

    /// Model IDs from the upstream's `GET /v1/models` (both `OpenAI` and
    /// Anthropic return `{"data": [{"id": ...}]}`). Empty on any failure.
    async fn list_models(&self) -> Vec<String> {
        let is_oauth = matches!(self.auth, Some(AuthEntry::OAuth(_)));
        let oauth_access = if is_oauth {
            self.oauth_access().await
        } else {
            None
        };
        let api_key = self.configured_key();
        if oauth_access.is_none() && api_key.is_none() {
            return Vec::new();
        }
        let url = format!("{}/v1/models", self.base_url);
        let mut req = self.http.get(&url);
        if let Some(access) = oauth_access {
            req = req.header(AUTHORIZATION, format!("Bearer {access}"));
        } else if let Some(key) = api_key {
            req = match self.format {
                ProviderFormat::Anthropic => req.header("x-api-key", key),
                ProviderFormat::Openai | ProviderFormat::OpenaiResponses => req.bearer_auth(key),
            };
        }
        if self.format == ProviderFormat::Anthropic {
            req = req
                .header("anthropic-version", ANTHROPIC_VERSION)
                .query(&[("limit", "1000")]);
        }
        for (k, v) in &self.oauth_headers {
            req = req.header(k.as_str(), v.as_str());
        }
        let req = self
            .headers
            .iter()
            .fold(req, |r, (k, v)| r.header(k.as_str(), v.as_str()));
        let body = match req.timeout(Duration::from_secs(10)).send().await {
            Ok(resp) if resp.status().is_success() => {
                resp.json::<Value>().await.unwrap_or(Value::Null)
            }
            Ok(resp) => {
                tracing::warn!(target: crate::LOG_TARGET, provider = %self.name, status = %resp.status(), "model discovery failed");
                return Vec::new();
            }
            Err(e) => {
                tracing::warn!(target: crate::LOG_TARGET, provider = %self.name, error = %e, "model discovery failed");
                return Vec::new();
            }
        };
        body["data"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|m| m["id"].as_str().map(str::to_string))
            .collect()
    }
}

/// Forward the client headers worth forwarding.
///
/// Anthropic gates models on the Claude Code client version it reads from
/// `User-Agent`, so the real client's replaces a stale catalog one.
/// Prompt-cache betas (e.g. `extended-cache-ttl` for `ttl: "1h"` blocks) live
/// in the client's `anthropic-beta` and are merged into ours.
fn apply_hints(
    headers: &mut HeaderMap,
    hints: &ClientHints,
    format: ProviderFormat,
) -> Result<(), UpstreamError> {
    if let Some(ua) = hints
        .user_agent
        .as_deref()
        .filter(|u| u.starts_with("claude-cli/"))
    {
        if let Ok(value) = HeaderValue::from_str(ua) {
            headers.insert(USER_AGENT, value);
        }
    }
    if format == ProviderFormat::Anthropic {
        if let Some(merged) = merge_betas(
            headers.get("anthropic-beta").and_then(|v| v.to_str().ok()),
            hints.anthropic_beta.as_deref(),
        ) {
            headers.insert("anthropic-beta", ProviderClient::header_value(&merged)?);
        }
    }
    Ok(())
}

/// Union of two comma-separated `anthropic-beta` lists, ours first, deduplicated.
fn merge_betas(ours: Option<&str>, client: Option<&str>) -> Option<String> {
    let client = client?;
    let mut out: Vec<&str> = Vec::new();
    for beta in ours.unwrap_or("").split(',').chain(client.split(',')) {
        let beta = beta.trim();
        if !beta.is_empty() && !out.contains(&beta) {
            out.push(beta);
        }
    }
    Some(out.join(","))
}

/// Ensure `identity` is the first `system` block, preserving the caller's
/// prompt as the next block. Anthropic rejects OAuth-authenticated requests on
/// all models except Haiku when the request does not lead with this exact
/// block, so it is prepended verbatim and never merged with other text.
fn inject_identity(body: &mut Value, identity: &str) {
    let block = json!({"type": "text", "text": identity});
    match body.get_mut("system") {
        None => body["system"] = Value::Array(vec![block]),
        Some(Value::String(text)) if text == identity => {}
        Some(Value::String(text)) => {
            let original = json!({"type": "text", "text": text.clone()});
            body["system"] = Value::Array(vec![block, original]);
        }
        Some(Value::Array(blocks)) => {
            let first_matches = blocks
                .first()
                .and_then(|b| b.get("text"))
                .and_then(Value::as_str)
                == Some(identity);
            if !first_matches {
                blocks.insert(0, block);
            }
        }
        Some(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::outbound::credential_store::SqlCipherCredentialStore;
    use crate::adapters::outbound::oauth::HttpOAuthClient;

    fn connector() -> HttpUpstreamConnector {
        HttpUpstreamConnector::new(
            Arc::new(SqlCipherCredentialStore),
            Arc::new(HttpOAuthClient::default()),
        )
    }

    fn request(body: Value, client_key: Option<&str>, session_id: &str) -> UpstreamRequest {
        UpstreamRequest {
            body,
            client_key: client_key.map(str::to_string),
            session_id: session_id.to_string(),
            hints: ClientHints::default(),
        }
    }

    fn provider(format: ProviderFormat) -> Provider {
        Provider {
            name: "test".to_string(),
            base_url: "http://127.0.0.1:9".to_string(),
            format,
            models: Vec::new(),
            auto_model: None,
            headers: std::collections::HashMap::new(),
            session_header: None,
            oauth: None,
        }
    }

    fn api_key(key: &str) -> AuthEntry {
        AuthEntry::Api {
            key: key.to_string(),
        }
    }

    #[test]
    fn default_paths_per_format() {
        assert_eq!(
            connector()
                .client(&provider(ProviderFormat::Anthropic), "default", false, None)
                .unwrap()
                .default_path(),
            "/v1/messages"
        );
        assert_eq!(
            connector()
                .client(&provider(ProviderFormat::Openai), "default", false, None)
                .unwrap()
                .default_path(),
            "/v1/chat/completions"
        );
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn missing_key_is_an_error() {
        let _guard = crate::TEST_STATE_LOCK.lock().unwrap();
        let client = connector()
            .client(&provider(ProviderFormat::Anthropic), "default", false, None)
            .unwrap();
        let err = client.send(request(json!({}), None, "")).await.unwrap_err();
        assert!(matches!(err, UpstreamError::MissingApiKey { .. }));
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn effective_key_passthrough_prefers_client_key() {
        let _guard = crate::TEST_STATE_LOCK.lock().unwrap();
        let p = provider(ProviderFormat::Anthropic);
        let client = connector()
            .client(&p, "default", true, Some(api_key("configured-key")))
            .unwrap();
        let key = client.effective_key(Some("client-key"));
        assert_eq!(key.as_deref(), Some("client-key"));
        // without a client key, falls back to the configured key
        let key = client.effective_key(None);
        assert_eq!(key.as_deref(), Some("configured-key"));
        // without passthrough, client key is ignored
        let client = connector()
            .client(&p, "default", false, Some(api_key("configured-key")))
            .unwrap();
        let key = client.effective_key(Some("client-key"));
        assert_eq!(key.as_deref(), Some("configured-key"));
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn key_resolution_uses_auth_only() {
        let _guard = crate::TEST_STATE_LOCK.lock().unwrap();

        // auth key is used
        let p = provider(ProviderFormat::Anthropic);
        let client = connector()
            .client(&p, "default", false, Some(api_key("auth-key")))
            .unwrap();
        assert_eq!(client.configured_key().as_deref(), Some("auth-key"));

        // no key at all
        let p = provider(ProviderFormat::Anthropic);
        let client = connector().client(&p, "default", false, None).unwrap();
        assert_eq!(client.configured_key(), None);
    }

    /// Spin up a one-shot HTTP server that records the request headers it
    /// receives and returns them in the response body as JSON. Returns
    /// `(base_url, received_headers)`.
    async fn header_capture_server() -> (String, tokio::sync::oneshot::Receiver<HeaderMap>) {
        json_capture_server("{}").await
    }

    async fn json_capture_server(
        body: &'static str,
    ) -> (String, tokio::sync::oneshot::Receiver<HeaderMap>) {
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
            let body = body.as_bytes();
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

    fn usage_oauth_entry(account_id: Option<&str>) -> AuthEntry {
        AuthEntry::OAuth(OAuthTokens {
            access: "test-access".to_string(),
            refresh: "test-refresh".to_string(),
            expires: i64::MAX,
            account_id: account_id.map(str::to_string),
        })
    }

    #[tokio::test]
    async fn chatgpt_usage_uses_the_account_oauth_and_reads_quota_windows() {
        let (url, received) = json_capture_server(
            r#"{"rate_limit":{"primary_window":{"used_percent":20,"limit_window_seconds":18000,"reset_at":2000000000},"secondary_window":{"used_percent":35,"limit_window_seconds":604800,"reset_at":2000000100}}}"#,
        )
        .await;
        let mut provider = provider(ProviderFormat::OpenaiResponses);
        provider.name = "chatgpt".to_string();
        provider.oauth = Some(crate::domain::config::OAuthProvider {
            account_id_header: Some("chatgpt-account-id".to_string()),
            ..crate::domain::config::OAuthProvider::default()
        });
        let client = connector()
            .client(
                &provider,
                "personal",
                false,
                Some(usage_oauth_entry(Some("account-123"))),
            )
            .unwrap();

        let usage = client.fetch_usage_at(&url).await.unwrap();
        let headers = received.await.unwrap();

        assert_eq!(
            headers.get("authorization").and_then(|v| v.to_str().ok()),
            Some("Bearer test-access")
        );
        assert_eq!(
            headers
                .get("chatgpt-account-id")
                .and_then(|v| v.to_str().ok()),
            Some("account-123")
        );
        assert_eq!(usage.provider, "chatgpt");
        assert_eq!(usage.alias, "personal");
        assert!((usage.five_hour.as_ref().unwrap().utilization - 20.0).abs() < f64::EPSILON);
        assert!((usage.seven_day.as_ref().unwrap().utilization - 35.0).abs() < f64::EPSILON);
        assert_eq!(
            usage.five_hour.as_ref().unwrap().resets_at.as_deref(),
            Some("2000000000")
        );
    }

    #[tokio::test]
    async fn claude_usage_uses_oauth_headers_and_reads_extra_usage() {
        let (url, received) = json_capture_server(
            r#"{"five_hour":{"utilization":12.5,"resets_at":"2026-10-08T01:00:00Z"},"seven_day":{"utilization":45.0,"resets_at":"2026-10-12T00:00:00Z"},"extra_usage":{"is_enabled":true,"utilization":30.0}}"#,
        )
        .await;
        let mut provider = provider(ProviderFormat::Anthropic);
        provider.name = "claude".to_string();
        provider.oauth = Some(crate::domain::config::OAuthProvider {
            headers: std::collections::HashMap::from([(
                "anthropic-beta".to_string(),
                "oauth-2025-04-20".to_string(),
            )]),
            ..crate::domain::config::OAuthProvider::default()
        });
        let client = connector()
            .client(&provider, "max", false, Some(usage_oauth_entry(None)))
            .unwrap();

        let usage = client.fetch_usage_at(&url).await.unwrap();
        let headers = received.await.unwrap();

        assert_eq!(
            headers.get("anthropic-beta").and_then(|v| v.to_str().ok()),
            Some("oauth-2025-04-20")
        );
        assert!((usage.five_hour.as_ref().unwrap().utilization - 12.5).abs() < f64::EPSILON);
        assert!((usage.seven_day.as_ref().unwrap().utilization - 45.0).abs() < f64::EPSILON);
        assert!((usage.monthly.as_ref().unwrap().utilization - 30.0).abs() < f64::EPSILON);
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
        let client = connector()
            .client(&p, "default", false, Some(api_key("key")))
            .unwrap();
        client.send(request(json!({}), None, "")).await.unwrap();

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
    async fn session_header_carries_client_session() {
        let _guard = crate::TEST_STATE_LOCK.lock().unwrap();

        let (base, rx) = header_capture_server().await;
        let mut p = provider(ProviderFormat::Openai);
        p.base_url = base.clone();
        p.session_header = Some("x-opencode-session".to_string());
        let client = connector()
            .client(&p, "default", false, Some(api_key("key")))
            .unwrap();
        client
            .send(request(json!({}), None, "sess-1"))
            .await
            .unwrap();

        let received = rx.await.unwrap();
        assert_eq!(
            received
                .get("x-opencode-session")
                .and_then(|v| v.to_str().ok()),
            Some("sess-1")
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
        let client = connector()
            .client(&p, "default", false, Some(api_key("key")))
            .unwrap();
        client.send(request(json!({}), None, "")).await.unwrap();

        let received = rx.await.unwrap();
        assert_eq!(
            received.get("authorization").and_then(|v| v.to_str().ok()),
            Some("Bearer custom")
        );
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn claude_client_user_agent_replaces_catalog_fallback() {
        let _guard = crate::TEST_STATE_LOCK.lock().unwrap();

        let (base, rx) = header_capture_server().await;
        let mut p = provider(ProviderFormat::Anthropic);
        p.base_url = base;
        p.headers = std::collections::HashMap::from([(
            "user-agent".to_string(),
            "claude-cli/2.1.0 (external, cli)".to_string(),
        )]);
        let client = connector()
            .client(&p, "default", false, Some(api_key("key")))
            .unwrap();
        client
            .send(UpstreamRequest {
                hints: ClientHints {
                    user_agent: Some("claude-cli/9.9.9 (external, cli)".to_string()),
                    anthropic_beta: Some(
                        "extended-cache-ttl-2025-04-11,oauth-2025-04-20".to_string(),
                    ),
                },
                ..request(json!({}), None, "")
            })
            .await
            .unwrap();
        let received = rx.await.unwrap();
        assert_eq!(
            received.get("user-agent").and_then(|v| v.to_str().ok()),
            Some("claude-cli/9.9.9 (external, cli)")
        );
        assert_eq!(
            received.get("anthropic-beta").and_then(|v| v.to_str().ok()),
            Some("extended-cache-ttl-2025-04-11,oauth-2025-04-20")
        );

        // Anything that is not Claude Code keeps the catalog's value.
        let (base2, rx2) = header_capture_server().await;
        p.base_url = base2;
        let client = connector()
            .client(&p, "default", false, Some(api_key("key")))
            .unwrap();
        client
            .send(UpstreamRequest {
                hints: ClientHints {
                    user_agent: Some("curl/8.5.0".to_string()),
                    anthropic_beta: None,
                },
                ..request(json!({}), None, "")
            })
            .await
            .unwrap();
        let received = rx2.await.unwrap();
        assert_eq!(
            received.get("user-agent").and_then(|v| v.to_str().ok()),
            Some("claude-cli/2.1.0 (external, cli)")
        );
    }

    #[tokio::test]
    async fn list_models_reads_upstream_ids() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 4096];
            let n = sock.read(&mut buf).await.unwrap();
            assert!(String::from_utf8_lossy(&buf[..n]).starts_with("GET /v1/models"));
            let body = br#"{"data":[{"id":"new-model-1"},{"id":"new-model-2"}]}"#;
            let head = format!(
                "HTTP/1.1 200 OK
content-length: {}
connection: close

",
                body.len()
            );
            sock.write_all(head.as_bytes()).await.unwrap();
            sock.write_all(body).await.unwrap();
        });
        let mut p = provider(ProviderFormat::Openai);
        p.base_url = format!("http://{addr}");
        let client = connector()
            .client(&p, "default", false, Some(api_key("key")))
            .unwrap();
        assert_eq!(
            client.list_models().await,
            vec!["new-model-1", "new-model-2"]
        );
    }

    /// One-shot server answering a request with `body` and `content_type`.
    async fn body_server(body: &'static str, content_type: &'static str) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 1024];
            let _ = sock.read(&mut buf).await;
            let head = format!(
                "HTTP/1.1 400 Bad Request\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            let _ = sock.write_all(head.as_bytes()).await;
            let _ = sock.write_all(body.as_bytes()).await;
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn error_responses_keep_status_content_type_and_body() {
        let base = body_server("upstream exploded", "text/plain").await;
        let mut p = provider(ProviderFormat::Openai);
        p.base_url = base;
        let client = connector()
            .client(&p, "default", false, Some(api_key("key")))
            .unwrap();
        let resp = client.send(request(json!({}), None, "")).await.unwrap();
        assert_eq!(resp.status, 400);
        assert_eq!(resp.content_type.as_deref(), Some("text/plain"));
        assert!(!resp.is_event_stream());
        let body = crate::domain::sse::collect(resp.body).await;
        assert_eq!(body, b"upstream exploded");
    }

    #[test]
    fn rate_limit_headers_from_codex_and_claude() {
        let mut headers = HeaderMap::new();
        assert_eq!(rate_limits_from(&headers), None);
        headers.insert(
            "anthropic-ratelimit-unified-5h-utilization",
            HeaderValue::from_static("0.25"),
        );
        headers.insert(
            "anthropic-ratelimit-unified-7d-utilization",
            HeaderValue::from_static("0.5"),
        );
        assert_eq!(rate_limits_from(&headers), Some((25.0, 50.0)));
        headers.insert(
            "x-codex-primary-used-percent",
            HeaderValue::from_static("10"),
        );
        headers.insert(
            "x-codex-secondary-used-percent",
            HeaderValue::from_static("20"),
        );
        assert_eq!(rate_limits_from(&headers), Some((10.0, 20.0)));
    }

    fn oauth_provider(token_url: String) -> Provider {
        let mut p = provider(ProviderFormat::Anthropic);
        p.oauth = Some(OAuthProvider {
            authorize_url: "https://claude.ai/oauth/authorize".to_string(),
            token_url,
            client_id: "client-1".to_string(),
            scopes: vec!["user:inference".to_string()],
            redirect_uri: "https://console.anthropic.com/oauth/code/callback".to_string(),
            headers: std::collections::HashMap::from([(
                "anthropic-beta".to_string(),
                "oauth-2025-04-20".to_string(),
            )]),
            identity: Some("You are Claude Code, Anthropic's official CLI for Claude.".to_string()),
            ..OAuthProvider::default()
        });
        p
    }

    fn oauth_entry(access: &str, refresh: &str, expires: i64) -> AuthEntry {
        AuthEntry::OAuth(OAuthTokens {
            access: access.to_string(),
            refresh: refresh.to_string(),
            expires,
            account_id: None,
        })
    }

    #[test]
    fn identity_injection_shapes() {
        let id = "You are Claude Code, Anthropic's official CLI for Claude.";
        let mut body = json!({"model": "m"});
        inject_identity(&mut body, id);
        assert_eq!(body["system"][0]["text"], id);

        let mut body = json!({"system": "client prompt"});
        inject_identity(&mut body, id);
        assert_eq!(body["system"][0]["text"], id);
        assert_eq!(body["system"][1]["text"], "client prompt");

        let mut body = json!({"system": [{"type": "text", "text": "other"}]});
        inject_identity(&mut body, id);
        assert_eq!(body["system"][0]["text"], id);
        assert_eq!(body["system"][1]["text"], "other");

        // already first: not duplicated
        let mut body = json!({"system": [{"type": "text", "text": id}]});
        inject_identity(&mut body, id);
        assert_eq!(body["system"].as_array().unwrap().len(), 1);
    }

    /// One-shot server that records the full request head and body.
    async fn full_capture_server() -> (String, tokio::sync::oneshot::Receiver<(String, String)>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                let n = sock.read(&mut chunk).await.unwrap_or(0);
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
                if let Some(head_end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&buf[..head_end]).to_lowercase();
                    let len = head
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length:"))
                        .and_then(|v| v.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    if buf.len() >= head_end + 4 + len {
                        break;
                    }
                }
            }
            let head_end = buf
                .windows(4)
                .position(|w| w == b"\r\n\r\n")
                .map_or(buf.len(), |p| p + 4);
            let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
            let body = String::from_utf8_lossy(&buf[head_end..]).to_string();
            let _ = tx.send((head, body));
            let payload = b"{}";
            let _ = sock
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                        payload.len()
                    )
                    .as_bytes(),
                )
                .await;
            let _ = sock.write_all(payload).await;
        });
        (format!("http://{addr}"), rx)
    }

    /// One-shot token endpoint answering with `response_body`.
    async fn token_server(
        response_body: &'static str,
    ) -> (String, tokio::sync::oneshot::Receiver<String>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = Vec::new();
            let mut chunk = [0u8; 1024];
            loop {
                let n = sock.read(&mut chunk).await.unwrap_or(0);
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
                if let Some(head_end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&buf[..head_end]).to_lowercase();
                    let len = head
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length:"))
                        .and_then(|v| v.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    if buf.len() >= head_end + 4 + len {
                        break;
                    }
                }
            }
            let head_end = buf
                .windows(4)
                .position(|w| w == b"\r\n\r\n")
                .map_or(buf.len(), |p| p + 4);
            let _ = tx.send(String::from_utf8_lossy(&buf[head_end..]).to_string());
            let head = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                response_body.len()
            );
            let _ = sock.write_all(head.as_bytes()).await;
            let _ = sock.write_all(response_body.as_bytes()).await;
        });
        (format!("http://{addr}"), rx)
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn oauth_request_uses_bearer_beta_and_identity() {
        let _guard = crate::TEST_STATE_LOCK.lock().unwrap();

        let (base, rx) = full_capture_server().await;
        let mut p = oauth_provider(String::new());
        p.base_url = base;
        let tokens = now_ms() + 3_600_000;
        let client = connector()
            .client(
                &p,
                "default",
                true,
                Some(oauth_entry("sk-ant-oat01-live", "ref", tokens)),
            )
            .unwrap();
        let body = json!({
            "model": "m",
            "max_tokens": 1,
            "system": "client prompt",
            "messages": []
        });
        client
            .send(request(body, Some("client-key"), ""))
            .await
            .unwrap();

        let (head, body) = rx.await.unwrap();
        let head = head.to_lowercase();
        assert!(
            head.contains("authorization: bearer sk-ant-oat01-live"),
            "bearer auth missing: {head}"
        );
        assert!(head.contains("anthropic-beta: oauth-2025-04-20"));
        assert!(head.contains("anthropic-version: 2023-06-01"));
        assert!(!head.contains("x-api-key"));
        assert!(!head.contains("client-key"));

        let body: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(
            body["system"][0]["text"],
            "You are Claude Code, Anthropic's official CLI for Claude."
        );
        assert_eq!(body["system"][1]["text"], "client prompt");
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn expired_oauth_refreshes_and_persists() {
        let _guard = crate::TEST_STATE_LOCK.lock().unwrap();

        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "local-proxy-oauth-upstream-{}-{stamp}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("LOCAL_PROXY_CONFIG_DIR", &dir);

        let (token_base, tokens_rx) = token_server(
            r#"{"access_token":"fresh-acc","refresh_token":"fresh-ref","expires_in":3600}"#,
        )
        .await;
        let (base, rx) = header_capture_server().await;
        let mut p = oauth_provider(token_base);
        p.base_url = base;
        SqlCipherCredentialStore
            .insert(
                &p.name,
                "default",
                &AuthEntry::OAuth(OAuthTokens {
                    access: "stale-acc".to_string(),
                    refresh: "old-ref".to_string(),
                    expires: 0,
                    account_id: None,
                }),
            )
            .unwrap();
        let client = connector()
            .client(
                &p,
                "default",
                false,
                Some(oauth_entry("stale-acc", "old-ref", 0)),
            )
            .unwrap();
        client.send(request(json!({}), None, "")).await.unwrap();

        let received = rx.await.unwrap();
        assert_eq!(
            received.get("authorization").and_then(|v| v.to_str().ok()),
            Some("Bearer fresh-acc")
        );
        let refresh_body: Value = serde_json::from_str(&tokens_rx.await.unwrap()).unwrap();
        assert_eq!(refresh_body["grant_type"], "refresh_token");
        assert_eq!(refresh_body["refresh_token"], "old-ref");

        let persisted = SqlCipherCredentialStore
            .get(&p.name, "default")
            .unwrap()
            .unwrap();
        let refreshed = persisted.oauth().unwrap();
        assert_eq!(refreshed.access, "fresh-acc");
        assert_eq!(refreshed.refresh, "fresh-ref");

        std::env::remove_var("LOCAL_PROXY_CONFIG_DIR");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
