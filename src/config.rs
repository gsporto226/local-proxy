use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Environment variable that overrides the default config file path.
pub const ENV_CONFIG_PATH: &str = "LOCAL_PROXY_CONFIG";

/// Default config file path used when no override is provided.
pub const DEFAULT_CONFIG_PATH: &str = "config.yaml";

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
    /// Environment variable that holds the provider's API key.
    pub api_key_env: Option<String>,
    /// Wire format the provider expects.
    pub format: ProviderFormat,
    /// Native model IDs the provider can serve.
    pub models: Vec<String>,
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

/// Fallback values used when a request doesn't match any route.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Defaults {
    /// Provider used when no route matches.
    pub provider: String,
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
}

/// Errors that can occur while loading or parsing configuration.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// Failed to read the config file from disk.
    #[error("failed to read config file {path}: {source}")]
    Io {
        /// Path of the file that could not be read.
        path: String,
        /// Underlying I/O error.
        source: std::io::Error,
    },
    /// Failed to parse the config contents.
    #[error("failed to parse config as {kind}: {source}")]
    Parse {
        /// Format that was attempted ("JSON" or "YAML").
        kind: &'static str,
        /// Underlying parse error.
        source: Box<dyn std::error::Error + Send + Sync>,
    },
}

impl Config {
    /// Load configuration from the file at `path`, inferring the format from
    /// its extension.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] if the file cannot be read or parsed.
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
        Self::from_str(&content, &ext)
    }

    /// Parse configuration from `content`, using `ext` to select the parser
    /// (`"json"` for JSON, anything else for YAML).
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Parse`] if `content` is not valid for the
    /// detected format.
    pub fn from_str(content: &str, ext: &str) -> Result<Self, ConfigError> {
        match ext {
            "json" => serde_json::from_str(content).map_err(|source| ConfigError::Parse {
                kind: "JSON",
                source: Box::new(source),
            }),
            _ => serde_yaml::from_str(content).map_err(|source| ConfigError::Parse {
                kind: "YAML",
                source: Box::new(source),
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
        assert_eq!(anthropic.api_key_env.as_deref(), Some("ANTHROPIC_API_KEY"));
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
        std::env::remove_var(ENV_CONFIG_PATH);
        assert!(Config::env_config_path().is_none());

        std::env::set_var(ENV_CONFIG_PATH, "custom.yaml");
        assert_eq!(
            Config::env_config_path().as_deref(),
            Some(Path::new("custom.yaml"))
        );
        std::env::remove_var(ENV_CONFIG_PATH);
    }
}
