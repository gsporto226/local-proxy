use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Environment variable that overrides the default config file path.
pub const ENV_CONFIG_PATH: &str = "LOCAL_PROXY_CONFIG";

/// Default config file path used when no override is provided.
pub const DEFAULT_CONFIG_PATH: &str = "config.yaml";

/// Default config embedded in the binary, written on first run when no config
/// file exists yet.
///
/// Minimal by design: the provider catalog is embedded separately (see
/// [`crate::catalog`]); this file only overrides server settings.
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

/// Wire format a provider's API expects (`Anthropic` vs `OpenAI`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderFormat {
    /// `Anthropic` Messages API format.
    #[default]
    Anthropic,
    /// `OpenAI` Chat Completions format.
    Openai,
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
    /// Optional static HTTP headers sent with every request to this provider.
    /// Headers with the same name override the format/auth defaults.
    #[serde(default)]
    pub headers: HashMap<String, String>,
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
}

/// Configuration for the `$proxy` local-command-execution feature.
///
/// When the last user message of a request starts with the `token` prefix,
/// the proxy runs the remainder as a [`crate::config::Exec::command`]
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
}

/// Template for the Claude Code status line, rendered by `local-proxy
/// statusline`. The script is a sandboxed Rhai expression evaluated inside the
/// proxy against the current session's recorded stats.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct StatuslineConfig {
    /// The Rhai template. A `--template` CLI flag overrides this value.
    pub template: Option<String>,
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
    /// Status line template for the Claude Code status line.
    #[serde(default)]
    pub statusline: StatuslineConfig,
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
    /// Load configuration from the file at `path`, inferring the format from
    /// its extension.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] if the file cannot be read or parsed.
    #[allow(clippy::result_large_err)]
    pub fn load(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let path = path.as_ref();
        let content = std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
            path: path.display().to_string(),
            source,
        })?;
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase)
            .unwrap_or_default();
        Self::parse(&content, &ext, &path.display().to_string())
    }

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
    /// error with `name`.
    #[allow(clippy::result_large_err)]
    fn parse(content: &str, ext: &str, name: &str) -> Result<Self, ConfigError> {
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

    /// Resolve the config path from the [`ENV_CONFIG_PATH`] environment
    /// variable, if set and non-empty.
    #[must_use]
    pub fn env_config_path() -> Option<PathBuf> {
        std::env::var(ENV_CONFIG_PATH)
            .ok()
            .filter(|p| !p.is_empty())
            .map(PathBuf::from)
    }
}

/// Returns the per-user config directory for local-proxy.
///
/// Overridden by the `LOCAL_PROXY_CONFIG_DIR` environment variable (so tests
/// and tooling can isolate the config dir and the `stats.db` it contains).
/// Otherwise prefers `directories::ProjectDirs` (`.config_dir()`): on Windows
/// this is `%APPDATA%\local-proxy`, on Unix `~/.config/local-proxy`. Falls back
/// to the `APPDATA` (Windows) or `HOME`/`USERPROFILE` environment variables,
/// and finally to a local `.config/local-proxy` directory. Never panics.
#[must_use]
pub fn global_config_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("LOCAL_PROXY_CONFIG_DIR").filter(|v| !v.is_empty()) {
        return PathBuf::from(dir);
    }
    if let Some(dirs) = directories::ProjectDirs::from("", "", "local-proxy") {
        return dirs.config_dir().to_path_buf();
    }

    #[cfg(windows)]
    {
        if let Some(appdata) = std::env::var_os("APPDATA") {
            return PathBuf::from(appdata).join("local-proxy");
        }
    }

    if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
        return PathBuf::from(home).join(".config").join("local-proxy");
    }

    PathBuf::from(".").join(".config").join("local-proxy")
}

/// Returns the default global config file path, i.e.
/// `global_config_dir()/config.yaml`.
#[must_use]
pub fn global_config_path() -> PathBuf {
    global_config_dir().join("config.yaml")
}

/// Create `path` (and its parent directories) and write the embedded
/// [`DEFAULT_CONFIG`] contents to it.
///
/// # Errors
///
/// Returns [`ConfigError::Create`] if the parent directories cannot be created
/// or the file cannot be written.
#[allow(clippy::result_large_err)]
pub fn create_default_config(path: impl AsRef<Path>) -> Result<(), ConfigError> {
    let path = path.as_ref();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| ConfigError::Create {
            path: path.display().to_string(),
            source,
        })?;
    }
    std::fs::write(path, DEFAULT_CONFIG).map_err(|source| ConfigError::Create {
        path: path.display().to_string(),
        source,
    })
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
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");

    #[test]
    fn loads_yaml_fixture() {
        let config = Config::load(Path::new(FIXTURES).join("config.yaml")).expect("yaml loads");

        assert_eq!(config.server.host, "127.0.0.1");
        assert_eq!(config.server.port, 8787);
        assert_eq!(config.server.api_keys, vec!["sk-proxy".to_string()]);
        assert!(!config.server.passthrough_keys);

        assert_eq!(config.providers.len(), 2);
        let anthropic = &config.providers[0];
        assert_eq!(anthropic.name, "anthropic");
        assert_eq!(anthropic.base_url, "https://api.anthropic.com");
        assert_eq!(anthropic.format, ProviderFormat::Anthropic);
        assert_eq!(
            anthropic.models,
            vec![
                "claude-sonnet-4-5".to_string(),
                "claude-opus-4-1".to_string()
            ]
        );

        let openai = &config.providers[1];
        assert_eq!(openai.name, "openai");
        assert_eq!(openai.format, ProviderFormat::Openai);
        assert_eq!(openai.models, vec!["gpt-4o".to_string(), "o3".to_string()]);

        assert_eq!(config.routes.len(), 2);
        let prefix_route = &config.routes[0];
        assert_eq!(prefix_route.model, "claude-sonnet");
        assert_eq!(prefix_route.provider, "anthropic");
        assert!(prefix_route.prefix);
        assert_eq!(
            prefix_route.upstream_model.as_deref(),
            Some("claude-sonnet-4-5")
        );

        let exact_route = &config.routes[1];
        assert_eq!(exact_route.model, "gpt-4o");
        assert_eq!(exact_route.provider, "openai");
        assert!(!exact_route.prefix);
        assert_eq!(exact_route.upstream_model, None);

        assert_eq!(config.defaults.provider, "anthropic");
    }

    #[test]
    fn loads_json_fixture() {
        let config = Config::load(Path::new(FIXTURES).join("config.json")).expect("json loads");

        assert_eq!(config.server.host, "127.0.0.1");
        assert_eq!(config.server.port, 8787);
        assert_eq!(config.server.api_keys, vec!["sk-proxy".to_string()]);
        assert!(!config.server.passthrough_keys);

        assert_eq!(config.providers.len(), 2);
        let anthropic = &config.providers[0];
        assert_eq!(anthropic.name, "anthropic");
        assert_eq!(anthropic.format, ProviderFormat::Anthropic);

        assert_eq!(config.routes.len(), 2);
        assert!(config.routes[0].prefix);
        assert_eq!(config.defaults.provider, "anthropic");
    }

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
    fn env_config_path_override() {
        let _guard = crate::TEST_STATE_LOCK.lock().unwrap();
        std::env::remove_var(ENV_CONFIG_PATH);
        assert!(Config::env_config_path().is_none());

        std::env::set_var(ENV_CONFIG_PATH, "custom.yaml");
        assert_eq!(
            Config::env_config_path().as_deref(),
            Some(Path::new("custom.yaml"))
        );
        std::env::remove_var(ENV_CONFIG_PATH);
    }

    #[test]
    fn global_config_dir_env_override() {
        let _guard = crate::TEST_STATE_LOCK.lock().unwrap();
        std::env::remove_var("LOCAL_PROXY_CONFIG_DIR");
        let default = global_config_dir();
        assert_ne!(default, PathBuf::new());

        std::env::set_var("LOCAL_PROXY_CONFIG_DIR", "/tmp/lp-e2e");
        assert_eq!(global_config_dir(), PathBuf::from("/tmp/lp-e2e"));
        std::env::remove_var("LOCAL_PROXY_CONFIG_DIR");
    }

    #[test]
    fn global_config_path_ends_with_config_yaml() {
        let path = global_config_path();
        assert_eq!(
            path.file_name().and_then(|n| n.to_str()),
            Some("config.yaml")
        );
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
        assert!(config.defaults.provider.is_empty());
        assert_eq!(config.defaults.active_model, None);
    }

    #[test]
    fn create_default_config_writes_parseable_file() {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock is set")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "local-proxy-create-default-{}-{stamp}",
            std::process::id()
        ));
        let path = dir.join("config.yaml");
        create_default_config(&path).expect("create default config");

        let config = Config::load(&path).expect("loaded default config");
        assert_eq!(config.server.port, 8787);
        assert!(config.providers.is_empty());

        let _ = std::fs::remove_dir_all(&dir);
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
