use futures_util::future::BoxFuture;

use crate::domain::account::OAuthTokens;
use crate::domain::config::OAuthProvider;
use crate::domain::oauth::OAuthError;

/// The network half of OAuth: token endpoints and the browser callback login.
pub trait OAuthClient: Send + Sync {
    /// Exchange an authorization code (PKCE) for tokens.
    fn exchange<'a>(
        &'a self,
        provider: &'a OAuthProvider,
        code: &'a str,
        verifier: &'a str,
        state: &'a str,
    ) -> BoxFuture<'a, Result<OAuthTokens, OAuthError>>;

    /// Refresh `current`, keeping its refresh token when the server does not
    /// rotate it.
    fn refresh<'a>(
        &'a self,
        provider: &'a OAuthProvider,
        current: &'a OAuthTokens,
    ) -> BoxFuture<'a, Result<OAuthTokens, OAuthError>>;

    /// Run the browser login, catching the redirect on a local listener.
    fn callback_login<'a>(
        &'a self,
        provider: &'a OAuthProvider,
    ) -> BoxFuture<'a, Result<OAuthTokens, OAuthError>>;
}
