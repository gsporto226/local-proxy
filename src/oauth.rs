//! Generic OAuth 2.0 + PKCE engine for subscription providers.
//!
//! Provider-specific details (endpoints, client id, scopes, headers, identity
//! prompt) come from the `oauth:` block of a [`crate::config::Provider`]; this
//! module only knows the standard OAuth shape. The interactive login is driven
//! by [`crate::config::OAuthFlow`]: today only `paste` is implemented, and a
//! new interaction is one flow variant plus one function here.

use std::time::{SystemTime, UNIX_EPOCH};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::auth::OAuthTokens;
use crate::config::OAuthProvider;

/// Refresh the access token this long before it actually expires.
pub const REFRESH_LEEWAY_MS: i64 = 60_000;

/// Fallback token lifetime when the endpoint omits `expires_in`.
const DEFAULT_EXPIRES_SECS: i64 = 3600;

/// Errors from the OAuth login, exchange, and refresh flows.
#[derive(Debug, thiserror::Error)]
pub enum OAuthError {
    /// The pasted authorization code was empty.
    #[error("nenhum codigo de autorizacao fornecido")]
    CodeMissing,
    /// The configured authorize URL is not a valid URL.
    #[error("authorize_url invalida: {detail}")]
    InvalidAuthorizeUrl {
        /// URL parse failure detail.
        detail: String,
    },
    /// The OAuth server request failed or returned an error status.
    #[error("oauth request to {url} falhou: {detail}")]
    Request {
        /// Endpoint that failed.
        url: String,
        /// HTTP status and a short response excerpt.
        detail: String,
    },
    /// A token response could not be parsed.
    #[error("resposta oauth invalida: {source}")]
    Parse {
        /// Underlying JSON error.
        #[source]
        source: serde_json::Error,
    },
    /// The token response carried no access token.
    #[error("resposta oauth sem access_token")]
    MissingToken,
}

/// Current unix time in milliseconds.
#[must_use]
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// Generate a PKCE verifier and its S256 challenge (RFC 7636).
#[must_use]
pub fn generate_pkce() -> (String, String) {
    let mut bytes = [0u8; 32];
    bytes[..16].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    bytes[16..].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    let verifier = URL_SAFE_NO_PAD.encode(bytes);
    let challenge = challenge_for(&verifier);
    (verifier, challenge)
}

/// The S256 challenge for `verifier`: `base64url(SHA-256(verifier))`.
#[must_use]
pub fn challenge_for(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// Build the browser authorization URL for `provider`.
///
/// # Errors
///
/// Returns [`OAuthError::InvalidAuthorizeUrl`] when the configured URL is
/// malformed.
pub fn authorize_url(
    provider: &OAuthProvider,
    challenge: &str,
    state: &str,
) -> Result<String, OAuthError> {
    let mut url =
        url::Url::parse(&provider.authorize_url).map_err(|e| OAuthError::InvalidAuthorizeUrl {
            detail: e.to_string(),
        })?;
    {
        let mut query = url.query_pairs_mut();
        query.append_pair("client_id", &provider.client_id);
        query.append_pair("response_type", "code");
        query.append_pair("redirect_uri", &provider.redirect_uri);
        query.append_pair("scope", &provider.scopes.join(" "));
        query.append_pair("code_challenge", challenge);
        query.append_pair("code_challenge_method", "S256");
        query.append_pair("state", state);
        for (key, value) in &provider.authorize_params {
            query.append_pair(key, value);
        }
    }
    Ok(url.to_string())
}

/// Extract `(code, state)` from what the callback page shows: a bare code, a
/// `code#state` pair, a full callback URL, or a query string.
///
/// # Errors
///
/// Returns [`OAuthError::CodeMissing`] when no code is present.
pub fn parse_code_input(input: &str) -> Result<(String, Option<String>), OAuthError> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(OAuthError::CodeMissing);
    }
    let (raw_code, url_state) = if trimmed.contains("code=") {
        let parsed = url::Url::parse(trimmed)
            .or_else(|_| url::Url::parse(&format!("https://example.invalid/?{trimmed}")))
            .map_err(|_| OAuthError::CodeMissing)?;
        let find = |name: &str| {
            parsed
                .query_pairs()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.to_string())
        };
        (find("code").ok_or(OAuthError::CodeMissing)?, find("state"))
    } else {
        (trimmed.to_string(), None)
    };
    if let Some((code, state)) = raw_code.split_once('#') {
        if code.trim().is_empty() {
            return Err(OAuthError::CodeMissing);
        }
        return Ok((code.to_string(), Some(state.to_string())));
    }
    if raw_code.trim().is_empty() {
        return Err(OAuthError::CodeMissing);
    }
    Ok((raw_code, url_state))
}

/// Exchange an authorization code for tokens.
///
/// # Errors
///
/// Returns [`OAuthError`] when the request fails or the response is malformed.
pub async fn exchange(
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
pub async fn refresh(
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
    post_tokens(
        http,
        provider,
        &Value::Object(params),
        Some(&current.refresh),
    )
    .await
}

async fn post_tokens(
    http: &reqwest::Client,
    provider: &OAuthProvider,
    body: &Value,
    fallback_refresh: Option<&str>,
) -> Result<OAuthTokens, OAuthError> {
    let resp = http
        .post(&provider.token_url)
        .json(body)
        .send()
        .await
        .map_err(|e| OAuthError::Request {
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
    parse_tokens(&text, fallback_refresh)
}

fn parse_tokens(text: &str, fallback_refresh: Option<&str>) -> Result<OAuthTokens, OAuthError> {
    let value: Value = serde_json::from_str(text).map_err(|source| OAuthError::Parse { source })?;
    let access = value
        .get("access_token")
        .and_then(Value::as_str)
        .ok_or(OAuthError::MissingToken)?;
    let refresh = value
        .get("refresh_token")
        .and_then(Value::as_str)
        .or(fallback_refresh)
        .unwrap_or_default();
    let expires_in = value
        .get("expires_in")
        .and_then(Value::as_i64)
        .unwrap_or(DEFAULT_EXPIRES_SECS);
    Ok(OAuthTokens {
        access: access.to_string(),
        refresh: refresh.to_string(),
        expires: now_ms() + expires_in.saturating_mul(1000),
    })
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

    #[test]
    fn pkce_matches_rfc_vector() {
        assert_eq!(
            challenge_for("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
        let (verifier, challenge) = generate_pkce();
        assert_eq!(verifier.len(), 43);
        assert_eq!(challenge, challenge_for(&verifier));
        assert_ne!(verifier, challenge);
    }

    #[test]
    fn parses_code_input_shapes() {
        assert_eq!(
            parse_code_input("  abc123#state9  ").unwrap(),
            ("abc123".to_string(), Some("state9".to_string()))
        );
        assert_eq!(
            parse_code_input("abc123").unwrap(),
            ("abc123".to_string(), None)
        );
        let url = "https://console.anthropic.com/oauth/code/callback?code=xyz&state=st8";
        assert_eq!(
            parse_code_input(url).unwrap(),
            ("xyz".to_string(), Some("st8".to_string()))
        );
        assert!(matches!(
            parse_code_input("https://x.test/?code=&state=s"),
            Err(OAuthError::CodeMissing)
        ));
        // without `code=` it is treated as a bare code, not a URL
        assert_eq!(
            parse_code_input("https://x.test/?nope=1").unwrap(),
            ("https://x.test/?nope=1".to_string(), None)
        );
        assert!(matches!(
            parse_code_input("#onlystate"),
            Err(OAuthError::CodeMissing)
        ));
    }

    #[test]
    fn authorize_url_carries_pkce_and_extras() {
        let mut p = provider(String::new());
        p.authorize_params =
            std::collections::HashMap::from([("code".to_string(), "true".to_string())]);
        let url = authorize_url(&p, "chal", "ver").unwrap();
        let parsed = url::Url::parse(&url).unwrap();
        let q: std::collections::HashMap<_, _> = parsed.query_pairs().into_owned().collect();
        assert_eq!(q["client_id"], "client-1");
        assert_eq!(q["response_type"], "code");
        assert_eq!(q["code_challenge"], "chal");
        assert_eq!(q["code_challenge_method"], "S256");
        assert_eq!(q["state"], "ver");
        assert_eq!(q["scope"], "user:inference user:profile");
        assert_eq!(q["code"], "true");
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
