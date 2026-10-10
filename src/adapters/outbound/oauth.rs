//! [`OAuthClient`] over HTTP: token exchange and refresh against the
//! provider's token endpoint, and the browser `callback` login.
//!
//! Provider-specific details (endpoints, client id, scopes) come from the
//! `oauth:` block of a [`crate::domain::config::Provider`]; the PKCE and URL
//! helpers live in [`crate::domain::oauth`].

use std::collections::HashMap;
use std::time::Duration;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use futures_util::future::BoxFuture;
use serde_json::{json, Value};

use crate::domain::account::OAuthTokens;
use crate::domain::config::{OAuthProvider, TokenEncoding};
use crate::domain::oauth::{account_id_from_jwt, authorize_url, generate_pkce, now_ms, OAuthError};
use crate::ports::OAuthClient;

/// OAuth token endpoints reached with a shared HTTP client.
#[derive(Debug, Clone, Default)]
pub struct HttpOAuthClient {
    http: reqwest::Client,
}

impl OAuthClient for HttpOAuthClient {
    fn exchange<'a>(
        &'a self,
        provider: &'a OAuthProvider,
        code: &'a str,
        verifier: &'a str,
        state: &'a str,
    ) -> BoxFuture<'a, Result<OAuthTokens, OAuthError>> {
        Box::pin(exchange(&self.http, provider, code, verifier, state))
    }

    fn refresh<'a>(
        &'a self,
        provider: &'a OAuthProvider,
        current: &'a OAuthTokens,
    ) -> BoxFuture<'a, Result<OAuthTokens, OAuthError>> {
        Box::pin(refresh(&self.http, provider, current))
    }

    fn callback_login<'a>(
        &'a self,
        provider: &'a OAuthProvider,
    ) -> BoxFuture<'a, Result<OAuthTokens, OAuthError>> {
        Box::pin(callback_login(&self.http, provider))
    }
}

/// Fallback token lifetime when the endpoint omits `expires_in`.
const DEFAULT_EXPIRES_SECS: i64 = 3600;

/// Exchange an authorization code for tokens.
///
/// # Errors
///
/// Returns [`OAuthError`] when the request fails or the response is malformed.
async fn exchange(
    http: &reqwest::Client,
    provider: &OAuthProvider,
    code: &str,
    verifier: &str,
    state: &str,
) -> Result<OAuthTokens, OAuthError> {
    let mut params = serde_json::Map::new();
    params.insert("grant_type".into(), json!("authorization_code"));
    params.insert("code".into(), json!(code));
    params.insert("redirect_uri".into(), json!(provider.redirect_uri));
    params.insert("client_id".into(), json!(provider.client_id));
    params.insert("code_verifier".into(), json!(verifier));
    params.insert("state".into(), json!(state));
    for (key, value) in &provider.token_params {
        params.insert(key.clone(), json!(value));
    }
    post_tokens(http, provider, &Value::Object(params), None).await
}

/// Refresh an expired access token, keeping the current refresh token when the
/// server does not rotate it.
///
/// # Errors
///
/// Returns [`OAuthError`] when the request fails or the response is malformed.
async fn refresh(
    http: &reqwest::Client,
    provider: &OAuthProvider,
    current: &OAuthTokens,
) -> Result<OAuthTokens, OAuthError> {
    let mut params = serde_json::Map::new();
    params.insert("grant_type".into(), json!("refresh_token"));
    params.insert("refresh_token".into(), json!(current.refresh));
    params.insert("client_id".into(), json!(provider.client_id));
    for (key, value) in &provider.token_params {
        params.insert(key.clone(), json!(value));
    }
    post_tokens(http, provider, &Value::Object(params), Some(current)).await
}

/// POST to the token endpoint; `current` supplies the refresh token and account
/// id to keep when the response omits them.
async fn post_tokens(
    http: &reqwest::Client,
    provider: &OAuthProvider,
    body: &Value,
    current: Option<&OAuthTokens>,
) -> Result<OAuthTokens, OAuthError> {
    let request = http.post(&provider.token_url);
    let request = match provider.token_encoding {
        TokenEncoding::Json => request.json(body),
        TokenEncoding::Form => {
            let pairs: Vec<(&str, &str)> = body
                .as_object()
                .into_iter()
                .flatten()
                .filter_map(|(k, v)| v.as_str().map(|v| (k.as_str(), v)))
                .collect();
            request.form(&pairs)
        }
    };
    let resp = request.send().await.map_err(|e| OAuthError::Request {
        url: provider.token_url.clone(),
        detail: e.to_string(),
    })?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        let excerpt: String = text.chars().take(300).collect();
        return Err(OAuthError::Request {
            url: provider.token_url.clone(),
            detail: format!("HTTP {status}: {excerpt}"),
        });
    }
    parse_tokens(&text, provider, current)
}

fn parse_tokens(
    text: &str,
    provider: &OAuthProvider,
    current: Option<&OAuthTokens>,
) -> Result<OAuthTokens, OAuthError> {
    let value: Value = serde_json::from_str(text).map_err(|source| OAuthError::Parse { source })?;
    let access = value
        .get("access_token")
        .and_then(Value::as_str)
        .ok_or(OAuthError::MissingToken)?;
    let refresh = value
        .get("refresh_token")
        .and_then(Value::as_str)
        .or_else(|| current.map(|c| c.refresh.as_str()))
        .unwrap_or_default();
    let expires_in = value
        .get("expires_in")
        .and_then(Value::as_i64)
        .unwrap_or(DEFAULT_EXPIRES_SECS);
    let account_id = value
        .get("id_token")
        .and_then(Value::as_str)
        .zip(provider.account_id_claim.as_deref())
        .and_then(|(token, claim)| account_id_from_jwt(token, claim))
        .or_else(|| current.and_then(|c| c.account_id.clone()));
    Ok(OAuthTokens {
        access: access.to_string(),
        refresh: refresh.to_string(),
        expires: now_ms() + expires_in.saturating_mul(1000),
        account_id,
    })
}

/// Run the `callback` login: open the authorize URL in the browser and catch
/// the redirect on a local listener bound to `redirect_uri`'s host and port.
///
/// # Errors
///
/// Returns [`OAuthError`] when the listener cannot bind, the state does not
/// match, no callback arrives within 5 minutes, or the exchange fails.
async fn callback_login(
    http: &reqwest::Client,
    provider: &OAuthProvider,
) -> Result<OAuthTokens, OAuthError> {
    let redirect = url::Url::parse(&provider.redirect_uri)
        .map_err(|e| OAuthError::Callback(format!("redirect_uri invalida: {e}")))?;
    let host = redirect.host_str().unwrap_or("localhost");
    let port = redirect.port_or_known_default().unwrap_or(80);
    let listener = tokio::net::TcpListener::bind((host, port))
        .await
        .map_err(|e| OAuthError::Callback(format!("nao consegui abrir {host}:{port}: {e}")))?;

    let (verifier, challenge) = generate_pkce();
    let state = URL_SAFE_NO_PAD.encode(uuid::Uuid::new_v4().as_bytes());
    let url = authorize_url(provider, &challenge, &state)?;
    println!("abra esta URL no navegador se ela nao abrir sozinha:\n  {url}");
    if let Err(e) = open_browser(&url) {
        eprintln!("nao consegui abrir o navegador automaticamente ({e}); abra a URL acima.");
    }

    let (tx, rx) = tokio::sync::oneshot::channel::<Result<String, OAuthError>>();
    let sender = std::sync::Arc::new(std::sync::Mutex::new(Some(tx)));
    let expected = state.clone();
    let app = axum::Router::new().route(
        redirect.path(),
        axum::routing::get(
            move |axum::extract::Query(params): axum::extract::Query<HashMap<String, String>>| {
                let outcome = callback_outcome(&params, &expected);
                let page = if outcome.is_ok() {
                    "<h1>Login concluido</h1><p>Pode fechar esta aba e voltar ao terminal.</p>"
                } else {
                    "<h1>Login falhou</h1><p>Volte ao terminal.</p>"
                };
                if let Some(tx) = sender.lock().ok().and_then(|mut s| s.take()) {
                    let _ = tx.send(outcome);
                }
                async move { axum::response::Html(page) }
            },
        ),
    );
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let code = tokio::time::timeout(Duration::from_mins(5), rx).await;
    server.abort();
    let code = code
        .map_err(|_| OAuthError::Timeout)?
        .map_err(|_| OAuthError::Callback("canal de callback fechou".to_string()))??;
    exchange(http, provider, &code, &verifier, &state).await
}

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

/// Open `url` in the default browser; a no-op when
/// `LOCAL_PROXY_OAUTH_NO_BROWSER` is set (tests play the browser themselves).
fn open_browser(url: &str) -> std::io::Result<()> {
    if std::env::var_os("LOCAL_PROXY_OAUTH_NO_BROWSER").is_some() {
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
    std::process::Command::new("open").arg(url).spawn()?;
    #[cfg(all(unix, not(target_os = "macos")))]
    std::process::Command::new("xdg-open").arg(url).spawn()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider(token_url: String) -> OAuthProvider {
        OAuthProvider {
            authorize_url: "https://claude.ai/oauth/authorize".to_string(),
            token_url,
            client_id: "client-1".to_string(),
            scopes: vec!["user:inference".to_string(), "user:profile".to_string()],
            redirect_uri: "https://console.anthropic.com/oauth/code/callback".to_string(),
            token_params: std::collections::HashMap::from([(
                "client_secret".to_string(),
                "shh".to_string(),
            )]),
            ..OAuthProvider::default()
        }
    }

    /// One-shot token endpoint that captures the request body and answers with
    /// `response_body`.
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
    async fn exchange_sends_pkce_fields_and_parses_tokens() {
        let (base, rx) =
            token_server(r#"{"access_token":"acc-1","refresh_token":"ref-1","expires_in":120}"#)
                .await;
        let p = provider(base);
        let before = now_ms();
        let tokens = exchange(&reqwest::Client::new(), &p, "code-1", "ver-1", "state-1")
            .await
            .unwrap();
        assert_eq!(tokens.access, "acc-1");
        assert_eq!(tokens.refresh, "ref-1");
        assert!(tokens.expires >= before + 119_000);
        assert!(tokens.expires <= now_ms() + 121_000);

        let body: Value = serde_json::from_str(&rx.await.unwrap()).unwrap();
        assert_eq!(body["grant_type"], "authorization_code");
        assert_eq!(body["code"], "code-1");
        assert_eq!(body["code_verifier"], "ver-1");
        assert_eq!(body["state"], "state-1");
        assert_eq!(body["client_id"], "client-1");
        assert_eq!(
            body["redirect_uri"],
            "https://console.anthropic.com/oauth/code/callback"
        );
        assert_eq!(body["client_secret"], "shh");
    }

    #[tokio::test]
    async fn refresh_rotates_and_keeps_refresh_token() {
        let (base, rx) = token_server(r#"{"access_token":"new-acc","expires_in":60}"#).await;
        let p = provider(base);
        let current = OAuthTokens {
            access: "old-acc".to_string(),
            refresh: "old-ref".to_string(),
            expires: 0,
            account_id: None,
        };
        let tokens = refresh(&reqwest::Client::new(), &p, &current)
            .await
            .unwrap();
        assert_eq!(tokens.access, "new-acc");
        assert_eq!(tokens.refresh, "old-ref");

        let body: Value = serde_json::from_str(&rx.await.unwrap()).unwrap();
        assert_eq!(body["grant_type"], "refresh_token");
        assert_eq!(body["refresh_token"], "old-ref");
        assert_eq!(body["client_id"], "client-1");
    }

    #[tokio::test]
    async fn token_error_status_is_reported() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 2048];
            let _ = sock.read(&mut buf).await;
            let body = r#"{"error":"invalid_grant"}"#;
            let head = format!(
                "HTTP/1.1 400 Bad Request\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            let _ = sock.write_all(head.as_bytes()).await;
            let _ = sock.write_all(body.as_bytes()).await;
        });
        let p = provider(format!("http://{addr}"));
        let err = exchange(&reqwest::Client::new(), &p, "c", "v", "s")
            .await
            .unwrap_err();
        let detail = err.to_string();
        assert!(detail.contains("400"));
        assert!(detail.contains("invalid_grant"));
    }
}
