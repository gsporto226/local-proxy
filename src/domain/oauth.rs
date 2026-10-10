//! OAuth 2.0 + PKCE building blocks that need no network: PKCE pairs, the
//! authorize URL, pasted-code parsing, and `id_token` account ids. The HTTP
//! exchange and refresh live in the OAuth adapter.

use std::time::{SystemTime, UNIX_EPOCH};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::domain::config::OAuthProvider;

/// Refresh the access token this long before it actually expires.
pub const REFRESH_LEEWAY_MS: i64 = 60_000;

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
    /// The callback `state` did not match the value we sent.
    #[error("estado OAuth invalido (possivel CSRF)")]
    StateMismatch,
    /// The local callback listener failed or the provider returned an error.
    #[error("falha no servidor de callback: {0}")]
    Callback(String),
    /// No callback arrived before the deadline.
    #[error("tempo esgotado esperando o login no navegador")]
    Timeout,
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

/// Read `chatgpt_account_id` from an `id_token` JWT, under the `claim`
/// namespace or at the root.
///
/// The signature is not verified: the token came over TLS straight from the
/// token endpoint, and the value only fills a request header, never a trust
/// decision. Returns `None` on malformed input.
#[must_use]
pub fn account_id_from_jwt(id_token: &str, claim: &str) -> Option<String> {
    let payload = id_token.split('.').nth(1)?;
    let claims: Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload).ok()?).ok()?;
    claims
        .get(claim)
        .and_then(|ns| ns.get("chatgpt_account_id"))
        .or_else(|| claims.get("chatgpt_account_id"))?
        .as_str()
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn provider() -> OAuthProvider {
        OAuthProvider {
            authorize_url: "https://claude.ai/oauth/authorize".to_string(),
            client_id: "client-1".to_string(),
            scopes: vec!["user:inference".to_string(), "user:profile".to_string()],
            redirect_uri: "https://console.anthropic.com/oauth/code/callback".to_string(),
            ..OAuthProvider::default()
        }
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
        let mut p = provider();
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

    #[test]
    fn account_id_reads_the_namespaced_claim() {
        let b64 = |v: &Value| URL_SAFE_NO_PAD.encode(serde_json::to_vec(v).unwrap());
        let namespaced = format!(
            "{}.{}.sig",
            b64(&json!({"alg": "none"})),
            b64(&json!({
                "https://api.openai.com/auth": { "chatgpt_account_id": "acct-777" },
            }))
        );
        assert_eq!(
            account_id_from_jwt(&namespaced, "https://api.openai.com/auth").as_deref(),
            Some("acct-777")
        );

        // A flat claim works too; a missing claim or a malformed token yields None.
        let flat = format!(
            "{}.{}.sig",
            b64(&json!({})),
            b64(&json!({"chatgpt_account_id": "flat-1"}))
        );
        assert_eq!(
            account_id_from_jwt(&flat, "whatever").as_deref(),
            Some("flat-1")
        );
        assert_eq!(account_id_from_jwt("not-a-jwt", "x"), None);
        assert_eq!(
            account_id_from_jwt(&format!("{}.{}.sig", b64(&json!({})), b64(&json!({}))), "x"),
            None
        );
    }
}
