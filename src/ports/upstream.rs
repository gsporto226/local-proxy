use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use futures_util::future::BoxFuture;
use serde_json::Value;

use crate::domain::account::{AuthEntry, AuthMap};
use crate::domain::config::{Config, Provider};
use crate::domain::error::ApiError;
use crate::domain::sse::ByteStream;
use crate::domain::stats::AccountUsage;
use crate::ports::CredentialError;

/// Errors from building upstream connections or talking to upstreams.
#[derive(Debug, thiserror::Error, miette::Diagnostic)]
pub enum UpstreamError {
    /// The account store could not be read.
    #[error("account store: {0}")]
    Credentials(#[from] CredentialError),
    /// The provider has no credential for this request.
    #[error("provider {provider} has no credentials; connect with `local-proxy connect {provider} --account <alias>`")]
    MissingApiKey {
        /// Name of the provider missing a key.
        provider: String,
    },
    /// The HTTP client could not be built.
    #[error("failed to build HTTP client: {message}")]
    ClientBuild {
        /// Underlying client construction error.
        message: String,
    },
    /// A header value could not be constructed.
    #[error("invalid upstream header: {detail}")]
    InvalidHeader {
        /// Human-readable description of the invalid header.
        detail: String,
    },
    /// An upstream request failed in transport.
    #[error("upstream request to {url} failed: {message}")]
    Request {
        /// URL that was requested.
        url: String,
        /// Underlying transport error.
        message: String,
    },
    /// The provider's usage endpoint requires an OAuth subscription account.
    #[error("provider {provider} usage endpoint requires an OAuth account")]
    UsageRequiresOAuth {
        /// Provider name.
        provider: String,
    },
    /// The provider's usage endpoint rejected the request.
    #[error("provider {provider} usage endpoint returned HTTP {status}")]
    UsageStatus {
        /// Provider name.
        provider: String,
        /// HTTP response status.
        status: u16,
    },
    /// The provider returned a successful response without recognized usage windows.
    #[error("provider {provider} usage response contained no supported quota windows")]
    UsageData {
        /// Provider name.
        provider: String,
    },
}

impl From<UpstreamError> for ApiError {
    fn from(e: UpstreamError) -> Self {
        match e {
            UpstreamError::Credentials(source) => {
                Self::internal(format!("account store: {source}"))
            }
            UpstreamError::MissingApiKey { provider } => Self::new(
                502,
                "api_error",
                format!(
                    "provider {provider} has no account; connect via `local-proxy connect {provider} --account <alias>`"
                ),
            ),
            UpstreamError::ClientBuild { message } => {
                Self::internal(format!("failed to build upstream HTTP client: {message}"))
            }
            UpstreamError::InvalidHeader { detail } => {
                Self::internal(format!("invalid upstream header: {detail}"))
            }
            UpstreamError::Request { url, message } => Self::new(
                502,
                "api_error",
                format!("upstream request to {url} failed: {message}"),
            ),
            UpstreamError::UsageRequiresOAuth { provider }
            | UpstreamError::UsageData { provider } => Self::new(
                502,
                "api_error",
                format!("provider {provider} usage is unavailable"),
            ),
            UpstreamError::UsageStatus { provider, status } => Self::new(
                502,
                "api_error",
                format!("provider {provider} usage endpoint returned HTTP {status}"),
            ),
        }
    }
}

/// The client headers an upstream request may forward.
#[derive(Debug, Clone, Default)]
pub struct ClientHints {
    /// The client's `User-Agent` (Anthropic gates models on Claude Code's).
    pub user_agent: Option<String>,
    /// The client's `anthropic-beta` list (prompt-cache betas and the like).
    pub anthropic_beta: Option<String>,
}

/// One request to send through an upstream account.
#[derive(Debug, Clone, Default)]
pub struct UpstreamRequest {
    /// The body, already in the provider's format.
    pub body: Value,
    /// The key the client presented (used when passthrough is enabled).
    pub client_key: Option<String>,
    /// The client session id, or `""`.
    pub session_id: String,
    /// Client headers worth forwarding.
    pub hints: ClientHints,
}

/// An upstream response whose body has not been read yet.
pub struct UpstreamResponse {
    /// HTTP status.
    pub status: u16,
    /// The `Content-Type` header, if any.
    pub content_type: Option<String>,
    /// Subscription quota percents `(5h, weekly)` reported in the headers.
    pub rate_limits: Option<(f64, f64)>,
    /// The raw body.
    pub body: ByteStream,
}

impl fmt::Debug for UpstreamResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UpstreamResponse")
            .field("status", &self.status)
            .field("content_type", &self.content_type)
            .field("rate_limits", &self.rate_limits)
            .finish_non_exhaustive()
    }
}

impl UpstreamResponse {
    /// Whether the body is a server-sent event stream.
    #[must_use]
    pub fn is_event_stream(&self) -> bool {
        self.content_type
            .as_deref()
            .is_some_and(|c| c.contains("text/event-stream"))
    }
}

/// One authenticated account of an upstream provider.
pub trait UpstreamAccount: Send + Sync + fmt::Debug {
    /// The provider name.
    fn provider(&self) -> &str;

    /// The account alias.
    fn alias(&self) -> &str;

    /// Whether a usable credential (API key or OAuth tokens) is stored.
    fn has_credentials(&self) -> bool;

    /// Whether the provider has a subscription usage endpoint.
    fn has_usage_endpoint(&self) -> bool;

    /// POST `request` to the provider's chat endpoint.
    fn send(
        &self,
        request: UpstreamRequest,
    ) -> BoxFuture<'_, Result<UpstreamResponse, UpstreamError>>;

    /// Fetch this account's quota windows, or `None` without a usage endpoint.
    fn fetch_usage(&self) -> BoxFuture<'_, Result<Option<AccountUsage>, UpstreamError>>;
}

/// A shared upstream account.
pub type Account = Arc<dyn UpstreamAccount>;

/// Accounts keyed by provider name, then alias.
pub type AccountMap = HashMap<String, HashMap<String, Account>>;

/// Builds upstream accounts and asks upstreams for their model lists.
pub trait UpstreamConnector: Send + Sync {
    /// An account of `provider` named `alias`, authenticated with `credential`
    /// (`None` for a passthrough-only placeholder).
    ///
    /// # Errors
    ///
    /// Returns [`UpstreamError::ClientBuild`] if the HTTP client cannot be built.
    fn connect(
        &self,
        provider: &Provider,
        alias: &str,
        passthrough: bool,
        credential: Option<AuthEntry>,
    ) -> Result<Account, UpstreamError>;

    /// Fill `models` for providers that list none, asking each upstream with
    /// any usable account from `accounts`. Failures leave the list empty.
    /// Blocks the caller.
    fn discover_models(&self, config: &mut Config, accounts: &AuthMap);
}
