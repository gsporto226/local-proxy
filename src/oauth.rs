//! OAuth (PKCE) login for subscription-backed providers.
//!
//! Providers such as ChatGPT back their API with an account session rather than
//! a static API key. [`login`] drives a browser PKCE flow against the
//! provider's authorization server and stores the resulting tokens in the auth
//! store (see [`crate::auth`]); [`access_token`] resolves a fresh bearer token
//! at request time, refreshing it when it is close to expiry.
//!
//! Only the ChatGPT backend (`chatgpt`) is registered today; its endpoints come
//! from the public Codex CLI and are overridable through environment variables
//! so a change upstream does not require a release.

use std::collections::HashMap;
use std::time::Duration;

use base64::Engine as _;
use serde::Deserialize;
use thiserror::Error;
use tokio::sync::oneshot;

/// Errors raised by the OAuth login and token-refresh flows.
#[derive(Debug, Error)]
pub enum OAuthError {
    /// The provider has no OAuth configuration.
    #[error("provider '{provider}' nao suporta login OAuth")]
    UnknownProvider {
        /// Provider name that was requested.
        provider: String,
    },
    /// A network request to the authorization or token endpoint failed.
    #[error("falha de rede ao falar com {url}: {source}")]
    Http {
        /// Endpoint that was contacted.
        url: String,
        /// Underlying request error.
        #[source]
        source: reqwest::Error,
    },
    /// The token endpoint returned an error response.
    #[error("o endpoint de token retornou {status}: {body}")]
    Token {
        /// HTTP status code.
        status: u16,
        /// Response body, for diagnosis.
        body: String,
    },
    /// The callback `state` did not match the value we sent.
    #[error("estado OAuth invalido (possivel CSRF)")]
    StateMismatch,
    /// The callback listener could not bind or serve.
    #[error("falha no servidor de callback: {0}")]
    Callback(String),
    /// No callback arrived before the deadline.
    #[error("tempo esgotado esperando o login no navegador")]
    Timeout,
    /// The auth store could not be updated.
    #[error(transparent)]
    Auth(#[from] crate::auth::AuthError),
}

/// Static OAuth configuration for a provider.
#[derive(Debug, Clone)]
pub struct OAuthConfig {
    /// Public client id registered with the provider.
    pub client_id: String,
    /// Authorization endpoint (browser redirect target).
    pub authorize_url: String,
    /// Token endpoint (code exchange and refresh).
    pub token_url: String,
    /// Local redirect, e.g. `http://localhost:1455/auth/callback`.
    pub redirect_uri: String,
    /// Space-separated OAuth scopes.
    pub scope: String,
    /// JWT claim namespace holding the account id, if any.
    pub account_claim: Option<String>,
}

/// Environment override for the OAuth client id.
const ENV_CLIENT_ID: &str = "LOCAL_PROXY_OAUTH_CLIENT_ID";
/// Environment override for the authorization endpoint.
const ENV_AUTHORIZE_URL: &str = "LOCAL_PROXY_OAUTH_AUTHORIZE_URL";
/// Environment override for the token endpoint.
const ENV_TOKEN_URL: &str = "LOCAL_PROXY_OAUTH_TOKEN_URL";
/// Environment override for the callback port.
const ENV_REDIRECT_PORT: &str = "LOCAL_PROXY_OAUTH_REDIRECT_PORT";

/// Return the OAuth configuration for `provider`, honoring environment
/// overrides. Returns `None` for providers without OAuth support.
#[must_use]
pub fn provider_config(provider: &str) -> Option<OAuthConfig> {
    let base = match provider {
        "chatgpt" => OAuthConfig {
            // Public Codex CLI client id.
            client_id: "app_EMoamEEZ73f0CkXaXp7hrann".to_string(),
            authorize_url: "https://auth.openai.com/oauth/authorize".to_string(),
            token_url: "https://auth.openai.com/oauth/token".to_string(),
            redirect_uri: "http://localhost:1455/auth/callback".to_string(),
            scope: "openid profile email offline_access".to_string(),
            account_claim: Some("https://api.openai.com/auth".to_string()),
        },
        _ => return None,
    };
    Some(apply_env_overrides(base))
}

/// Apply `LOCAL_PROXY_OAUTH_*` overrides, if set.
fn apply_env_overrides(mut config: OAuthConfig) -> OAuthConfig {
    if let Some(v) = env_nonempty(ENV_CLIENT_ID) {
        config.client_id = v;
    }
    if let Some(v) = env_nonempty(ENV_AUTHORIZE_URL) {
        config.authorize_url = v;
    }
    if let Some(v) = env_nonempty(ENV_TOKEN_URL) {
        config.token_url = v;
    }
    if let Some(port) = env_nonempty(ENV_REDIRECT_PORT).and_then(|p| p.parse::<u16>().ok()) {
        config.redirect_uri = format!("http://localhost:{port}/auth/callback");
    }
    config
}

fn env_nonempty(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

/// Whether `provider` supports OAuth login.
#[must_use]
pub fn supports(provider: &str) -> bool {
    provider_config(provider).is_some()
}

/// A PKCE verifier and its S256 challenge.
#[derive(Debug, Clone)]
pub struct Pkce {
    /// The high-entropy `code_verifier` sent on the token exchange.
    pub verifier: String,
    /// The `code_challenge` sent on the authorization request.
    pub challenge: String,
}

/// Generate a PKCE verifier/challenge pair (S256).
#[must_use]
pub fn pkce_pair() -> Pkce {
    use rand::RngCore;
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    let verifier = b64url(&bytes);
    let challenge = b64url(&sha256(verifier.as_bytes()));
    Pkce {
        verifier,
        challenge,
    }
}

/// A random `state` value for the OAuth round trip.
#[must_use]
fn random_state() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut bytes);
    b64url(&bytes)
}

/// base64url (no padding) encoding.
fn b64url(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// SHA-256 digest.
fn sha256(bytes: &[u8]) -> Vec<u8> {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher.finalize().to_vec()
}

/// Build the browser authorization URL for `config` with the given PKCE
/// challenge and `state`.
#[must_use]
pub fn authorize_url(config: &OAuthConfig, challenge: &str, state: &str) -> String {
    let params = [
        ("response_type", "code"),
        ("client_id", config.client_id.as_str()),
        ("redirect_uri", config.redirect_uri.as_str()),
        ("scope", config.scope.as_str()),
        ("code_challenge", challenge),
        ("code_challenge_method", "S256"),
        ("state", state),
        ("id_token_add_organizations", "true"),
        ("codex_cli_simplified_flow", "true"),
    ];
    let query = params
        .iter()
        .map(|(k, v)| format!("{k}={}", urlencode(v)))
        .collect::<Vec<_>>()
        .join("&");
    format!("{}?{query}", config.authorize_url)
}

/// Percent-encode a query value (unreserved characters pass through).
fn urlencode(value: &str) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            _ => {
                let _ = write!(out, "%{byte:02X}");
            }
        }
    }
    out
}

/// Extract the account id from an `id_token` JWT, reading the configured claim
/// namespace's `chatgpt_account_id` (or `chatgpt_account_id` at the root).
///
/// The signature is not verified: the token arrived over TLS directly from the
/// provider's token endpoint, and the value is only used to populate a request
/// header, never for a trust decision. Returns `None` on any malformed input.
#[must_use]
pub fn account_id_from_jwt(id_token: &str, claim_ns: Option<&str>) -> Option<String> {
    let payload = id_token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    let claims: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    let value = claim_ns
        .and_then(|ns| claims.get(ns))
        .unwrap_or(&claims)
        .get("chatgpt_account_id")?;
    value.as_str().map(str::to_string)
}

/// Response of the token endpoint (exchange and refresh share the shape).
#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    id_token: Option<String>,
    #[serde(default)]
    expires_in: Option<i64>,
}

/// Expiry timestamp `expires_in` seconds from now, defaulting to 1 hour.
fn expiry_from(expires_in: Option<i64>) -> i64 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs().cast_signed());
    now + expires_in.unwrap_or(3600)
}

/// Run the browser PKCE login for `provider` using the `flavor` flow.
///
/// Tokens are stored on success, keyed by `provider` (the configured provider
/// name), so several providers may share one OAuth flow.
///
/// # Errors
///
/// Returns [`OAuthError`] if the flow is unknown, the callback cannot be
/// served, no code arrives before the timeout, or the token exchange/auth store
/// fails.
pub async fn login(provider: &str, flavor: &str) -> Result<(), OAuthError> {
    let config = provider_config(flavor).ok_or_else(|| OAuthError::UnknownProvider {
        provider: flavor.to_string(),
    })?;
    let pkce = pkce_pair();
    let state = random_state();

    let (tx, rx) = oneshot::channel::<Result<String, OAuthError>>();
    let listener = serve_callback(&config, &state, tx)?;
    let url = authorize_url(&config, &pkce.challenge, &state);
    println!("abra esta URL no navegador se ela nao abrir sozinha:\n  {url}");
    if let Err(e) = open_browser(&url) {
        eprintln!("nao consegui abrir o navegador automaticamente ({e}); abra a URL acima.");
    }

    let code = tokio::select! {
        res = rx => match res {
            Ok(Ok(code)) => code,
            Ok(Err(e)) => return Err(e),
            Err(_) => return Err(OAuthError::Callback("canal de callback fechou".to_string())),
        },
        () = tokio::time::sleep(Duration::from_mins(5)) => return Err(OAuthError::Timeout),
    };
    listener.abort();

    let tokens = exchange_code(&config, &code, &pkce.verifier).await?;
    let account_id = tokens.id_token.as_deref().and_then(|t| {
        account_id_from_jwt(t, config.account_claim.as_deref())
            .or_else(|| account_id_from_jwt(t, None))
    });
    crate::auth::set_oauth(
        provider,
        &tokens.access_token,
        tokens.refresh_token,
        account_id,
        Some(expiry_from(tokens.expires_in)),
    )?;
    Ok(())
}

/// Bind the callback listener and spawn a one-shot axum server that delivers
/// the authorization `code` (or an error) through `tx`.
fn serve_callback(
    config: &OAuthConfig,
    state: &str,
    tx: oneshot::Sender<Result<String, OAuthError>>,
) -> Result<tokio::task::JoinHandle<()>, OAuthError> {
    use std::net::{Ipv4Addr, SocketAddr, ToSocketAddrs};

    let addr: SocketAddr = config
        .redirect_uri
        .trim_start_matches("http://")
        .trim_end_matches("/auth/callback")
        .to_socket_addrs()
        .map_err(|e| OAuthError::Callback(format!("redirect_uri invalida: {e}")))?
        .next()
        .unwrap_or_else(|| SocketAddr::from((Ipv4Addr::LOCALHOST, 1455)));

    let expected: std::sync::Arc<str> = std::sync::Arc::from(state);
    let sender = std::sync::Arc::new(tokio::sync::Mutex::new(Some(tx)));

    let handle = tokio::spawn(async move {
        let listener = match tokio::net::TcpListener::bind(addr).await {
            Ok(l) => l,
            Err(e) => {
                let taken = sender.lock().await.take();
                if let Some(tx) = taken {
                    let _ = tx.send(Err(OAuthError::Callback(format!(
                        "nao consegui abrir {addr}: {e}"
                    ))));
                }
                return;
            }
        };
        let app = axum::Router::new().route(
            "/auth/callback",
            axum::routing::get(
                move |axum::extract::Query(params): axum::extract::Query<
                    HashMap<String, String>,
                >| {
                    let expected = expected.clone();
                    let sender = sender.clone();
                    async move {
                        let outcome = callback_outcome(&params, &expected);
                        let (status, html) = match &outcome {
                            Ok(_) => (
                                axum::http::StatusCode::OK,
                                "<h1>Login concluido</h1>\
                                 <p>Pode fechar esta aba e voltar ao terminal.</p>"
                                    .to_string(),
                            ),
                            Err(_) => (
                                axum::http::StatusCode::BAD_REQUEST,
                                "<h1>Login falhou</h1><p>Volte ao terminal.</p>".to_string(),
                            ),
                        };
                        let taken = sender.lock().await.take();
                        if let Some(tx) = taken {
                            let _ = tx.send(outcome);
                        }
                        (status, axum::response::Html(html))
                    }
                },
            ),
        );
        let _ = axum::serve(listener, app).await;
    });
    Ok(handle)
}

/// Validate the callback query: reject a provider error, a `state` mismatch, or
/// a missing `code`; otherwise yield the authorization code.
fn callback_outcome(
    params: &HashMap<String, String>,
    expected: &str,
) -> Result<String, OAuthError> {
    if let Some(err) = params.get("error") {
        return Err(OAuthError::Callback(err.clone()));
    }
    if params.get("state").map(String::as_str) != Some(expected) {
        return Err(OAuthError::StateMismatch);
    }
    params
        .get("code")
        .cloned()
        .ok_or_else(|| OAuthError::Callback("callback sem codigo de autorizacao".to_string()))
}

/// Exchange an authorization code for tokens.
async fn exchange_code(
    config: &OAuthConfig,
    code: &str,
    verifier: &str,
) -> Result<TokenResponse, OAuthError> {
    let params = [
        ("grant_type", "authorization_code"),
        ("code", code),
        ("client_id", config.client_id.as_str()),
        ("redirect_uri", config.redirect_uri.as_str()),
        ("code_verifier", verifier),
    ];
    post_token(config, &params).await
}

/// Refresh an access token.
async fn refresh_tokens(
    config: &OAuthConfig,
    refresh_token: &str,
) -> Result<TokenResponse, OAuthError> {
    let params = [
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
        ("client_id", config.client_id.as_str()),
        ("scope", config.scope.as_str()),
    ];
    post_token(config, &params).await
}

/// POST form parameters to the token endpoint and parse the response.
async fn post_token(
    config: &OAuthConfig,
    params: &[(&str, &str)],
) -> Result<TokenResponse, OAuthError> {
    let client = reqwest::Client::new();
    let resp = client
        .post(&config.token_url)
        .form(params)
        .send()
        .await
        .map_err(|source| OAuthError::Http {
            url: config.token_url.clone(),
            source,
        })?;
    let status = resp.status().as_u16();
    if status >= 400 {
        let body = resp.text().await.unwrap_or_default();
        return Err(OAuthError::Token { status, body });
    }
    resp.json::<TokenResponse>()
        .await
        .map_err(|source| OAuthError::Http {
            url: config.token_url.clone(),
            source,
        })
}

/// Return a valid access token for `provider`, refreshing it when the stored
/// one is expired or expires within 60 seconds.
///
/// `flavor` names the OAuth flow (see [`provider_config`]); the auth entry is
/// keyed by `provider` (the configured provider name), which is also what a
/// refresh writes back to.
///
/// # Errors
///
/// Returns [`OAuthError`] if the flow is unknown, `provider` has no stored OAuth
/// entry, or the refresh fails.
pub async fn access_token(provider: &str, flavor: &str) -> Result<String, OAuthError> {
    let entry = crate::auth::entry_for(provider).ok_or_else(|| OAuthError::UnknownProvider {
        provider: provider.to_string(),
    })?;
    if !entry.is_oauth() {
        return Ok(entry.key);
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs().cast_signed());
    let fresh = entry.expires_at.is_some_and(|expires| expires - now > 60);
    if fresh {
        return Ok(entry.key);
    }
    let Some(refresh) = entry.refresh_token.clone() else {
        // No refresh token: use the access token as-is and let the upstream
        // reject it if it is expired.
        return Ok(entry.key);
    };
    let config = provider_config(flavor).ok_or_else(|| OAuthError::UnknownProvider {
        provider: flavor.to_string(),
    })?;
    let tokens = refresh_tokens(&config, &refresh).await?;
    let account_id = tokens
        .id_token
        .as_deref()
        .and_then(|t| account_id_from_jwt(t, config.account_claim.as_deref()))
        .or_else(|| entry.account_id.clone());
    crate::auth::set_oauth(
        provider,
        &tokens.access_token,
        tokens.refresh_token,
        account_id,
        Some(expiry_from(tokens.expires_in)),
    )?;
    Ok(tokens.access_token)
}

/// Environment guard that suppresses the automatic browser launch, so the flow
/// can run headless (tests, remote shells); the URL is still printed.
const ENV_NO_BROWSER: &str = "LOCAL_PROXY_OAUTH_NO_BROWSER";

/// Open `url` in the platform's default browser.
///
/// On Windows the URL is routed through `rundll32 url.dll,FileProtocolHandler`
/// rather than `cmd /c start`, so the `&`/`?` characters in an authorization
/// query string are never re-parsed as shell syntax.
fn open_browser(url: &str) -> std::io::Result<()> {
    if env_nonempty(ENV_NO_BROWSER).is_some() {
        return Ok(());
    }
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        std::process::Command::new("rundll32")
            .args(["url.dll,FileProtocolHandler", url])
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()?;
    }
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open").arg(url).spawn()?;
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        std::process::Command::new("xdg-open").arg(url).spawn()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_challenge_is_s256_of_verifier() {
        let pair = pkce_pair();
        assert!(pair.verifier.len() >= 43);
        assert!(
            pair.verifier
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
            "verifier is base64url"
        );
        assert_eq!(pair.challenge, b64url(&sha256(pair.verifier.as_bytes())));
    }

    #[test]
    fn pkce_pair_is_random() {
        assert_ne!(pkce_pair().verifier, pkce_pair().verifier);
    }

    #[test]
    fn chatgpt_is_known_and_others_are_not() {
        assert!(supports("chatgpt"));
        assert!(!supports("anthropic"));
        assert!(provider_config("nope").is_none());
    }

    #[test]
    fn authorize_url_carries_pkce_and_scope() {
        let config = provider_config("chatgpt").expect("chatgpt config");
        let url = authorize_url(&config, "challenge-value", "state-value");
        assert!(url.starts_with("https://auth.openai.com/oauth/authorize?"));
        assert!(url.contains("code_challenge=challenge-value"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("state=state-value"));
        assert!(url.contains("client_id=app_EMoamEEZ73f0CkXaXp7hrann"));
        assert!(url.contains("redirect_uri=http%3A%2F%2Flocalhost%3A1455%2Fauth%2Fcallback"));
    }

    #[test]
    fn env_overrides_client_id_and_redirect_port() {
        let _guard = crate::TEST_STATE_LOCK.lock().unwrap();
        std::env::set_var(ENV_CLIENT_ID, "custom-client");
        std::env::set_var(ENV_REDIRECT_PORT, "9999");
        let config = provider_config("chatgpt").expect("config");
        assert_eq!(config.client_id, "custom-client");
        assert_eq!(config.redirect_uri, "http://localhost:9999/auth/callback");
        std::env::remove_var(ENV_CLIENT_ID);
        std::env::remove_var(ENV_REDIRECT_PORT);
    }

    #[test]
    fn account_id_reads_nested_claim() {
        // header.payload.signature with the Codex claim namespace.
        let claims = serde_json::json!({
            "https://api.openai.com/auth": { "chatgpt_account_id": "acct-42" }
        });
        let payload = b64url(serde_json::to_string(&claims).unwrap().as_bytes());
        let jwt = format!("header.{payload}.sig");
        assert_eq!(
            account_id_from_jwt(&jwt, Some("https://api.openai.com/auth")).as_deref(),
            Some("acct-42")
        );
        // unknown namespace falls back to the root
        assert_eq!(account_id_from_jwt(&jwt, Some("nope")), None);
        assert_eq!(account_id_from_jwt("garbage", None), None);
    }

    #[test]
    fn account_id_reads_root_claim() {
        let claims = serde_json::json!({ "chatgpt_account_id": "root-acct" });
        let payload = b64url(serde_json::to_string(&claims).unwrap().as_bytes());
        let jwt = format!("h.{payload}.s");
        assert_eq!(
            account_id_from_jwt(&jwt, Some("https://api.openai.com/auth")).as_deref(),
            Some("root-acct")
        );
    }

    #[test]
    fn urlencode_escapes_reserved() {
        assert_eq!(urlencode("a b/c"), "a%20b%2Fc");
        assert_eq!(urlencode("safe-._~"), "safe-._~");
    }
}
