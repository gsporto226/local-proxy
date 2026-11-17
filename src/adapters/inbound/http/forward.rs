//! Forward-proxy mode (`HTTPS_PROXY`): lets Claude Code reach the proxy without
//! `ANTHROPIC_BASE_URL`, which keeps features gated on the default base URL
//! (Code Mode) turned on.
//!
//! A `CONNECT` to [`INTERCEPTED_HOST`] is terminated with a certificate signed
//! by a local CA ([`ca_cert_path`], trusted by the client through
//! `NODE_EXTRA_CA_CERTS`). Inside that TLS session, `/v1/messages` and
//! `/v1/messages/count_tokens` go to the normal router; every other path is
//! passed through to the real API unchanged. A `CONNECT` to any other host is
//! a plain byte tunnel.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use axum::body::Body;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Router;
use hyper_util::rt::TokioIo;
use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose,
    GeneralSubtree, IsCa, Issuer, KeyPair, KeyUsagePurpose, NameConstraints,
};
use rustls_pki_types::pem::PemObject as _;
use rustls_pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio_rustls::rustls::ServerConfig;
use tokio_rustls::TlsAcceptor;
use tower::ServiceExt as _;

use crate::adapters::outbound::paths;
use crate::application::runtime::AppState;

/// The only host whose TLS the proxy terminates.
pub const INTERCEPTED_HOST: &str = "api.anthropic.com";

/// Paths on [`INTERCEPTED_HOST`] answered by the proxy's own router.
const LOCAL_PATHS: [&str; 2] = ["/v1/messages", "/v1/messages/count_tokens"];

/// Hop-by-hop headers, never forwarded on passthrough.
const HOP_BY_HOP: [&str; 7] = [
    "connection",
    "proxy-connection",
    "keep-alive",
    "transfer-encoding",
    "te",
    "trailer",
    "upgrade",
];

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// The local CA certificate (PEM) a forward-mode client must trust.
#[must_use]
pub fn ca_cert_path() -> PathBuf {
    paths::config_dir().join("forward-ca.pem")
}

fn ca_key_path() -> PathBuf {
    paths::config_dir().join("forward-ca.key.pem")
}

/// The CA's parameters. Deterministic, so a stored key plus these parameters
/// rebuild the issuer without parsing the stored certificate. Name
/// constraints limit the CA to [`INTERCEPTED_HOST`]: a leaked key cannot
/// impersonate any other site.
fn ca_params() -> CertificateParams {
    let mut p = CertificateParams::default();
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, "local-proxy forward CA");
    p.distinguished_name = dn;
    p.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    p.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    p.name_constraints = Some(NameConstraints {
        permitted_subtrees: vec![GeneralSubtree::DnsName(INTERCEPTED_HOST.to_string())],
        excluded_subtrees: vec![],
    });
    p.not_before = rcgen::date_time_ymd(2025, 1, 1);
    p.not_after = rcgen::date_time_ymd(2045, 1, 1);
    p
}

fn write_private(path: &Path, contents: &str) -> std::io::Result<()> {
    use std::io::Write as _;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(path)?.write_all(contents.as_bytes())
}

/// The stored CA (certificate PEM, key), created on first use.
fn load_or_create_ca() -> Result<(String, KeyPair), BoxError> {
    let (cert_path, key_path) = (ca_cert_path(), ca_key_path());
    if let (Ok(cert), Ok(key)) = (
        std::fs::read_to_string(&cert_path),
        std::fs::read_to_string(&key_path),
    ) {
        if let Ok(key) = KeyPair::from_pem(&key) {
            return Ok((cert, key));
        }
    }
    let key = KeyPair::generate()?;
    let cert = ca_params().self_signed(&key)?.pem();
    std::fs::create_dir_all(paths::config_dir())?;
    write_private(&key_path, &key.serialize_pem())?;
    std::fs::write(&cert_path, &cert)?;
    Ok((cert, key))
}

fn build_acceptor() -> Result<TlsAcceptor, BoxError> {
    let (ca_pem, ca_key) = load_or_create_ca()?;
    let issuer = Issuer::new(ca_params(), ca_key);
    let leaf_key = KeyPair::generate()?;
    let mut params = CertificateParams::new(vec![INTERCEPTED_HOST.to_string()])?;
    params
        .distinguished_name
        .push(DnType::CommonName, INTERCEPTED_HOST);
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    params.not_before = rcgen::date_time_ymd(2025, 1, 1);
    params.not_after = rcgen::date_time_ymd(2045, 1, 1);
    let leaf = params.signed_by(&leaf_key, &issuer)?;
    let chain = vec![
        leaf.der().clone(),
        CertificateDer::from_pem_slice(ca_pem.as_bytes())?,
    ];
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(leaf_key.serialize_der()));
    let provider = Arc::new(tokio_rustls::rustls::crypto::ring::default_provider());
    let mut config = ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_single_cert(chain, key)?;
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(TlsAcceptor::from(Arc::new(config)))
}

// ponytail: built once per process from the config dir at first use; a CA
// rotated on disk needs a proxy restart.
static ACCEPTOR: OnceLock<Result<TlsAcceptor, String>> = OnceLock::new();

fn acceptor() -> Result<TlsAcceptor, String> {
    ACCEPTOR
        .get_or_init(|| build_acceptor().map_err(|e| e.to_string()))
        .clone()
}

/// Create the CA (if missing) and the TLS acceptor ahead of the first
/// `CONNECT`, so [`ca_cert_path`] exists before a client is launched.
///
/// # Errors
///
/// Returns the reason the CA could not be read, created or used.
pub fn prepare() -> Result<PathBuf, String> {
    acceptor().map(|_| ca_cert_path())
}

/// The router's fallback: handles `CONNECT` (forward-proxy mode), 404 for
/// everything else. Only loopback peers may open a tunnel, so a proxy bound to
/// a public address is not an open relay.
pub(super) async fn fallback(State(app): State<AppState>, req: Request) -> Response {
    if req.method() != Method::CONNECT {
        return StatusCode::NOT_FOUND.into_response();
    }
    let peer = req.extensions().get::<ConnectInfo<SocketAddr>>();
    if peer.is_some_and(|ConnectInfo(p)| !p.ip().is_loopback()) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let Some(authority) = req.uri().authority().map(ToString::to_string) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let acceptor = if req.uri().host() == Some(INTERCEPTED_HOST) {
        match acceptor() {
            Ok(a) => Some(a),
            Err(e) => {
                tracing::warn!(target: crate::LOG_TARGET, error = %e, "forward CA unavailable");
                return StatusCode::BAD_GATEWAY.into_response();
            }
        }
    } else {
        None
    };
    tokio::spawn(async move {
        let io = match hyper::upgrade::on(req).await {
            Ok(upgraded) => TokioIo::new(upgraded),
            Err(e) => {
                tracing::debug!(target: crate::LOG_TARGET, error = %e, "CONNECT upgrade failed");
                return;
            }
        };
        let result = match acceptor {
            Some(acceptor) => intercept(acceptor, io, app).await,
            None => tunnel(io, &authority).await,
        };
        if let Err(e) = result {
            tracing::debug!(target: crate::LOG_TARGET, %authority, error = %e, "CONNECT session ended");
        }
    });
    StatusCode::OK.into_response()
}

async fn tunnel(
    mut io: TokioIo<hyper::upgrade::Upgraded>,
    authority: &str,
) -> Result<(), BoxError> {
    let mut upstream = tokio::net::TcpStream::connect(authority).await?;
    tokio::io::copy_bidirectional(&mut io, &mut upstream).await?;
    Ok(())
}

async fn intercept(
    acceptor: TlsAcceptor,
    io: TokioIo<hyper::upgrade::Upgraded>,
    app: AppState,
) -> Result<(), BoxError> {
    let tls = acceptor.accept(io).await?;
    let router = super::app(app.clone());
    let service = hyper::service::service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
        let (router, app) = (router.clone(), app.clone());
        async move { Ok::<_, Infallible>(route(router, app, req.map(Body::new)).await) }
    });
    hyper::server::conn::http1::Builder::new()
        .serve_connection(TokioIo::new(tls), service)
        .await?;
    Ok(())
}

async fn route(router: Router, app: AppState, mut req: Request) -> Response {
    if !LOCAL_PATHS.contains(&req.uri().path()) {
        return passthrough(req).await;
    }
    // The client carries its claude.ai credentials, not a proxy key: swap them
    // for the proxy's own key so `server.api_keys` still authorizes the call
    // and the subscription token never leaves for another provider.
    let headers = req.headers_mut();
    headers.remove(header::AUTHORIZATION);
    headers.remove("x-api-key");
    let snapshot = app.snapshot().await;
    if let Some(key) = snapshot.config.server.api_keys.first() {
        if let Ok(value) = HeaderValue::from_str(key) {
            headers.insert("x-api-key", value);
        }
    }
    match router.oneshot(req).await {
        Ok(response) => response,
        Err(never) => match never {},
    }
}

/// Where non-local paths go. `LOCAL_PROXY_PASSTHROUGH_BASE` overrides it for
/// tests.
fn passthrough_base() -> String {
    std::env::var("LOCAL_PROXY_PASSTHROUGH_BASE")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| format!("https://{INTERCEPTED_HOST}"))
}

fn strip_hop_by_hop(headers: &mut HeaderMap) {
    for name in HOP_BY_HOP {
        headers.remove(name);
    }
}

async fn passthrough(req: Request) -> Response {
    // Never through a proxy: the proxy's own env may carry `HTTPS_PROXY`
    // pointing back at itself.
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    let client = CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .no_proxy()
            .build()
            .unwrap_or_default()
    });
    let (mut parts, body) = req.into_parts();
    let url = format!(
        "{}{}",
        passthrough_base(),
        parts.uri.path_and_query().map_or("/", |p| p.as_str())
    );
    parts.headers.remove(header::HOST);
    strip_hop_by_hop(&mut parts.headers);
    let sent = client
        .request(parts.method, url)
        .headers(parts.headers)
        .body(reqwest::Body::wrap_stream(body.into_data_stream()))
        .send()
        .await;
    match sent {
        Ok(upstream) => {
            let mut response = Response::new(Body::empty());
            *response.status_mut() = upstream.status();
            *response.headers_mut() = upstream.headers().clone();
            strip_hop_by_hop(response.headers_mut());
            *response.body_mut() = Body::from_stream(upstream.bytes_stream());
            response
        }
        Err(e) => {
            tracing::warn!(target: crate::LOG_TARGET, error = %e, "passthrough failed");
            StatusCode::BAD_GATEWAY.into_response()
        }
    }
}
