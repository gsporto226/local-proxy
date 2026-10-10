//! The configuration model: providers, routes, defaults, and their parsing.
//! Reading and writing config files is the job of the config-file adapter.

use std::collections::HashMap;
use std::fmt;

use serde::{Deserialize, Serialize};

/// Default config embedded in the binary, written on first run when no config
/// file exists yet.
///
/// Minimal by design: the provider catalog is embedded separately (see
/// [`crate::domain::catalog`]); this file only overrides server settings.
pub const DEFAULT_CONFIG: &str = r"# local-proxy default configuration.
# The provider catalog is embedded in the binary; this file only adds or
# overrides providers/routes/defaults from that catalog (see catalog.yaml).

server:
  host: 127.0.0.1
  port: 8787
  api_keys:
    - sk-proxy
  passthrough_keys: false
";

/// Wire format a provider's API expects.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderFormat {
    /// `Anthropic` Messages API format.
    #[default]
    Anthropic,
    /// `OpenAI` Chat Completions format.
    Openai,
    /// `OpenAI` Responses API format (used by the `ChatGPT` backend).
    #[serde(rename = "openai-responses")]
    OpenaiResponses,
}

/// How token requests are encoded.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TokenEncoding {
    /// `application/json` body.
    #[default]
    Json,
    /// `application/x-www-form-urlencoded` body (RFC 6749 default).
    Form,
}

/// How the interactive OAuth login gets the authorization code.
///
/// The engine in [`crate::domain::oauth`] implements one function per flow; adding a
/// provider with a new interaction is a new variant here plus that function.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum OAuthFlow {
    /// Print the authorize URL, then read the `code#state` the callback page
    /// shows from stdin (`claude setup-token` / Claude Code `/login` style).
    #[default]
    Paste,
    /// Open the authorize URL in the browser and catch the redirect on a
    /// local listener bound to `redirect_uri` (Codex CLI style).
    Callback,
}

/// OAuth 2.0 client recipe for a subscription provider.
///
/// Everything provider-specific lives here (endpoints, client id, scopes,
/// extra headers, identity prompt), so the OAuth engine stays generic and a
/// new provider is a config block, not Rust code.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct OAuthProvider {
    /// Interactive login flow used by `connect --oauth`.
    pub flow: OAuthFlow,
    /// Authorization endpoint opened in the browser.
    pub authorize_url: String,
    /// Token endpoint, used for both code exchange and refresh.
    pub token_url: String,
    /// Public OAuth client id.
    pub client_id: String,
    /// Requested scopes (joined with spaces in the authorize URL).
    pub scopes: Vec<String>,
    /// Redirect URI registered for the client.
    pub redirect_uri: String,
    /// Extra query parameters appended to the authorize URL.
    pub authorize_params: HashMap<String, String>,
    /// Extra fields sent in token requests (e.g. a `client_secret`).
    pub token_params: HashMap<String, String>,
    /// Static headers sent only on OAuth-authenticated requests (beta flags,
    /// app identity). Provider-level `headers` still override these.
    pub headers: HashMap<String, String>,
    /// System prompt block the upstream requires first on every request
    /// (Anthropic rejects non-Haiku OAuth calls without it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<String>,
    /// Encoding of the token endpoint request body.
    pub token_encoding: TokenEncoding,
    /// JWT claim namespace in the `id_token` holding `chatgpt_account_id`;
    /// when set, the account id is stored with the tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id_claim: Option<String>,
    /// Header that carries the stored account id on OAuth requests.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id_header: Option<String>,
}

/// Network and authentication settings for the proxy server.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct Server {
    /// Host interface to bind the server to.
    pub host: String,
    /// TCP port to listen on.
    pub port: u16,
    /// API keys accepted for client authentication.
    pub api_keys: Vec<String>,
    /// Whether to forward the client's key to upstream providers.
    pub passthrough_keys: bool,
}

impl Default for Server {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".to_string(),
            port: 8787,
            api_keys: Vec::new(),
            passthrough_keys: false,
        }
    }
}

/// An upstream provider (`Anthropic`, `OpenAI`, or another `OpenAI`-compatible host).
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Provider {
    /// Unique provider name used in routes and the CLI.
    pub name: String,
    /// Base URL for the provider's API.
    pub base_url: String,
    /// Wire format the provider expects.
    pub format: ProviderFormat,
    /// Native model IDs the provider can serve.
    pub models: Vec<String>,
    /// Model sent upstream when a request asks for `auto` (bare, or as
    /// `<provider>/auto`). When unset, `auto` is not available for this
    /// provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_model: Option<String>,
    /// Optional static HTTP headers sent with every request to this provider.
    /// Headers with the same name override the format/auth defaults.
    #[serde(default)]
    pub headers: HashMap<String, String>,
    /// Header that carries the client session id upstream, for providers that
    /// require one to route requests (e.g. `OpenCode`'s `x-opencode-session`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_header: Option<String>,
    /// OAuth 2.0 subscription recipe. Providers with this block accept
    /// `connect <name> --oauth` and refresh their tokens automatically.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oauth: Option<OAuthProvider>,
}

/// Maps a requested model to a provider (exact match or prefix).
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Route {
    /// Requested model name (or prefix when `prefix` is set).
    pub model: String,
    /// Provider to route matching requests to.
    pub provider: String,
    /// Treat `model` as a prefix rather than an exact match.
    pub prefix: bool,
    /// Optional model name to send upstream instead of the requested one.
    pub upstream_model: Option<String>,
    /// Reasoning effort to request upstream (`low`, `medium`, `high`, …).
    ///
    /// Only meaningful for providers whose reasoning depth is a request field
    /// rather than part of the model name — the `ChatGPT` Codex backend, where
    /// `gpt-6-sol` at `low` and at `medium` share one model id. The proxy adds
    /// it to the request body; providers that ignore the field are unaffected.
    pub reasoning_effort: Option<String>,
}

/// Configuration for the `$proxy` local-command-execution feature.
///
/// When the last user message of a request starts with the `token` prefix,
/// the proxy runs the remainder as a [`crate::domain::config::Exec::command`]
/// invocation instead of forwarding the request upstream, returning the
/// terminal output as the model's reply.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct Exec {
    /// Whether `$proxy` execution is active. On by default.
    pub enabled: bool,
    /// The magic prefix that triggers local execution, e.g. `$proxy`.
    pub token: String,
    /// The binary invoked for `$proxy` commands.
    pub command: String,
    /// Maximum seconds a `$proxy` command may run before it is killed.
    pub timeout_secs: u64,
}

impl Default for Exec {
    fn default() -> Self {
        Self {
            enabled: true,
            token: "$proxy".to_string(),
            command: "local-proxy".to_string(),
            timeout_secs: 30,
        }
    }
}

/// Fallback values used when a request doesn't match any route.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Defaults {
    /// Provider used when no route matches.
    pub provider: String,
    /// Active model that the proxy routes all traffic through, ignoring the
    /// model requested by the harness. Set via `local-proxy model` or
    /// `$proxy model`; persists across restarts.
    pub active_model: Option<String>,
    /// Reasoning effort the proxy forces on every Anthropic request
    /// (`output_config.effort`), ignoring the harness's. Set via
    /// `local-proxy effort`; persists across restarts.
    pub active_effort: Option<String>,
    /// Active account alias per provider (`provider -> alias`), used when a
    /// request carries no `X-Local-Proxy-Account` header. Set via
    /// `local-proxy account <provider>/<alias>`; persists across restarts.
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    pub active_accounts: HashMap<String, String>,
}

/// Top-level parsed configuration.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Config {
    /// Server settings.
    pub server: Server,
    /// Configured upstream providers.
    pub providers: Vec<Provider>,
    /// Model-to-provider routing rules.
    pub routes: Vec<Route>,
    /// Fallback defaults.
    pub defaults: Defaults,
    /// `$proxy` local-command-execution settings.
    pub exec: Exec,
}

/// Errors that can occur while loading, parsing, or creating configuration.
#[derive(Debug, thiserror::Error, miette::Diagnostic)]
pub enum ConfigError {
    /// Failed to read the config file from disk.
    #[error("failed to read config file {path}: {source}")]
    #[diagnostic(code(config::io))]
    Io {
        /// Path of the file that could not be read.
        path: String,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// Failed to parse the config contents.
    #[error("failed to parse config as {kind}: {source}")]
    #[diagnostic(code(config::parse))]
    #[diagnostic(help("fix the highlighted portion of the config and try again"))]
    Parse {
        /// Format that was attempted ("JSON" or "YAML").
        kind: &'static str,
        /// Name used to label the source (file path, or `<config>`).
        name: String,
        /// Full config contents, attached so the diagnostic can render context.
        #[source_code]
        content: miette::NamedSource<String>,
        /// Byte span of the offending location in `content`.
        #[label("parse error here")]
        span: miette::SourceSpan,
        /// Underlying parse error.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    /// Failed to create the default config file.
    #[error("failed to write default config to {path}: {source}")]
    #[diagnostic(code(config::create))]
    Create {
        /// Path of the file that could not be written.
        path: String,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// Failed to serialize the config for saving.
    #[error("falha ao serializar config: {message}")]
    #[diagnostic(code(config::serialize))]
    Serialize {
        /// Serializer error message.
        message: String,
    },
    /// Failed to write the config file.
    #[error("failed to write config file {path}: {source}")]
    #[diagnostic(code(config::write))]
    Write {
        /// Path of the file that could not be written.
        path: String,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },
}

impl ConfigError {
    /// Build a [`ConfigError::Parse`], computing the [`miette::SourceSpan`] from
    /// a 1-indexed line/column location in `content`.
    fn parse_error(
        kind: &'static str,
        name: &str,
        content: &str,
        line: usize,
        column: usize,
        source: Box<dyn std::error::Error + Send + Sync>,
    ) -> Self {
        let offset = byte_offset(content, line, column);
        Self::Parse {
            kind,
            name: name.to_string(),
            content: miette::NamedSource::new(name, content.to_string()),
            span: miette::SourceSpan::new(offset.into(), 0),
            source,
        }
    }
}

/// Compute the byte offset in `content` of the given 1-indexed `line`/`column`,
/// clamping out-of-range values to the nearest valid position.
#[must_use]
fn byte_offset(content: &str, line: usize, column: usize) -> usize {
    let line = line.saturating_sub(1);
    let column = column.saturating_sub(1);
    let mut start = 0usize;
    for _ in 0..line {
        match content[start..].find('\n') {
            Some(i) => start += i + 1,
            None => return content.len(),
        }
    }
    let line_len = content[start..].find('\n').unwrap_or(content.len() - start);
    start + column.min(line_len)
}

impl Config {
    /// Parse configuration from `content`, using `ext` to select the parser
    /// (`"json"` for JSON, anything else for YAML).
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Parse`] if `content` is not valid for the
    /// detected format.
    #[allow(clippy::result_large_err)]
    pub fn from_str(content: &str, ext: &str) -> Result<Self, ConfigError> {
        Self::parse(content, ext, "<config>")
    }

    /// Parse `content` using the parser selected by `ext`, labelling any parse
    /// error with `name` (usually the file path).
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Parse`] if `content` is not valid for the
    /// detected format.
    #[allow(clippy::result_large_err)]
    pub fn parse(content: &str, ext: &str, name: &str) -> Result<Self, ConfigError> {
        match ext {
            "json" => serde_json::from_str(content).map_err(|source| {
                ConfigError::parse_error(
                    "JSON",
                    name,
                    content,
                    source.line(),
                    source.column(),
                    Box::new(source),
                )
            }),
            _ => serde_yaml::from_str(content).map_err(|source| {
                let (line, column) = source
                    .location()
                    .map_or((1, 1), |loc| (loc.line(), loc.column()));
                ConfigError::parse_error("YAML", name, content, line, column, Box::new(source))
            }),
        }
    }
}

/// Build the provider-qualified model id (`provider/model`) for `model` served
/// by `provider`.
///
/// Models that already carry the `provider/` prefix are returned unchanged
/// (e.g. `opencode-go/deepseek-v4-flash`); bare models get the prefix
/// (e.g. `neuralwatt/glm-5.2`).
#[must_use]
pub fn qualified_id(provider: &str, model: &str) -> String {
    if model.starts_with(&format!("{provider}/")) {
        model.to_string()
    } else {
        format!("{provider}/{model}")
    }
}

impl fmt::Display for ProviderFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Anthropic => write!(f, "anthropic"),
            Self::Openai => write!(f, "openai"),
            Self::OpenaiResponses => write!(f, "openai-responses"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_format_by_extension() {
        let yaml = Config::from_str("server:\n  port: 9999\n", "yaml").expect("yaml ext");
        assert_eq!(yaml.server.port, 9999);
        assert_eq!(yaml.server.host, "127.0.0.1");

        let json = Config::from_str(r#"{"server":{"port":1234}}"#, "json").expect("json ext");
        assert_eq!(json.server.port, 1234);

        let no_ext = Config::from_str("server:\n  port: 5555\n", "").expect("defaults to yaml");
        assert_eq!(no_ext.server.port, 5555);
    }

    #[test]
    fn missing_sections_use_defaults() {
        let config = Config::from_str("", "yaml").expect("empty yaml");
        assert_eq!(config.server.host, "127.0.0.1");
        assert_eq!(config.server.port, 8787);
        assert!(config.providers.is_empty());
        assert!(config.routes.is_empty());
        assert_eq!(config.defaults.provider, "");
    }

    #[test]
    fn default_config_parses() {
        let config = Config::from_str(DEFAULT_CONFIG, "yaml").expect("default config parses");
        assert_eq!(config.server.host, "127.0.0.1");
        assert_eq!(config.server.port, 8787);
        assert_eq!(config.server.api_keys, vec!["sk-proxy".to_string()]);
        assert!(!config.server.passthrough_keys);
        assert!(config.providers.is_empty());
        assert!(config.routes.is_empty());
        assert!(
            config.defaults.provider.is_empty(),
            "defaults.provider should be empty"
        );
        assert_eq!(config.defaults.active_model, None);
    }

    #[test]
    fn provider_format_parses_openai_responses() {
        let config = Config::from_str(
            "providers:\n  - name: chatgpt\n    base_url: http://x\n    format: openai-responses\n",
            "yaml",
        )
        .expect("yaml parses");
        assert_eq!(config.providers[0].format, ProviderFormat::OpenaiResponses);
        assert_eq!(config.providers[0].format.to_string(), "openai-responses");

        // round-trips through YAML with the same spelling
        let yaml = serde_yaml::to_string(&config.providers[0]).expect("serializes");
        assert!(yaml.contains("openai-responses"), "{yaml}");
    }

    #[test]
    fn route_reasoning_effort_parses_and_defaults_to_none() {
        let config = Config::from_str(
            r"routes:
  - model: sol-low
    provider: chatgpt
    upstream_model: gpt-6-sol
    reasoning_effort: low
  - model: plain
    provider: chatgpt
",
            "yaml",
        )
        .expect("yaml parses");
        assert_eq!(config.routes[0].reasoning_effort.as_deref(), Some("low"));
        assert_eq!(
            config.routes[0].upstream_model.as_deref(),
            Some("gpt-6-sol")
        );
        assert_eq!(config.routes[1].reasoning_effort, None);

        // A route without an effort still serializes the field (as null), which
        // matches how `upstream_model` already behaves; the value round-trips.
        let yaml = serde_yaml::to_string(&config.routes[1]).expect("serializes");
        assert!(yaml.contains("reasoning_effort: null"), "{yaml}");
        let back: crate::domain::config::Route = serde_yaml::from_str(&yaml).expect("parses back");
        assert_eq!(back.reasoning_effort, None);
    }

    #[test]
    fn chatgpt_catalog_recipe_uses_callback_and_form() {
        let catalog = crate::domain::catalog::load().expect("catalog parses");
        let p = catalog
            .providers
            .iter()
            .find(|p| p.name == "chatgpt")
            .expect("chatgpt in catalog");
        let oauth = p.oauth.as_ref().expect("chatgpt has oauth");
        assert_eq!(oauth.flow, OAuthFlow::Callback);
        assert_eq!(oauth.token_encoding, TokenEncoding::Form);
        assert_eq!(
            oauth.account_id_header.as_deref(),
            Some("chatgpt-account-id")
        );
    }

    #[test]
    fn provider_headers_deserialize_from_yaml_and_round_trip() {
        let config = Config::from_str(
            r"providers:
  - name: openrouter
    base_url: https://openrouter.ai/api/v1
    format: openai
    headers:
      HTTP-Referer: https://github.com/gsporto226/local-proxy
      X-Title: local-proxy
    models: [openrouter/auto]
",
            "yaml",
        )
        .expect("yaml parses");
        let p = &config.providers[0];
        assert_eq!(
            p.headers.get("HTTP-Referer").map(String::as_str),
            Some("https://github.com/gsporto226/local-proxy")
        );
        assert_eq!(
            p.headers.get("X-Title").map(String::as_str),
            Some("local-proxy")
        );

        // a provider without headers defaults to an empty map
        let bare = Config::from_str(
            "providers:\n  - name: x\n    base_url: http://x\n    format: openai\n",
            "yaml",
        )
        .expect("yaml parses");
        assert!(bare.providers[0].headers.is_empty());
    }
}
