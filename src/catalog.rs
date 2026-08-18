//! Embedded provider catalog and config-overlay merging.
//!
//! The binary ships a default catalog (`catalog.yaml`, embedded via
//! [`include_str!`]) with every provider the proxy supports. The user's
//! `config.yaml` acts as an *overlay*: providers it defines are added when new
//! or replace same-named catalog entries; routes and defaults from the overlay
//! take precedence. [`effective_config`] combines both into the runtime
//! configuration the proxy actually uses.

use crate::config::{Config, ConfigError};

/// The embedded catalog, included verbatim from `catalog.yaml`.
pub const CATALOG_YAML: &str = include_str!("catalog.yaml");

/// Parse the embedded [`CATALOG_YAML`] into a [`Config`] (the base layer).
///
/// # Errors
///
/// Returns [`ConfigError::Parse`] if the embedded catalog is invalid.
#[allow(clippy::result_large_err)]
pub fn load() -> Result<Config, ConfigError> {
    Config::from_str(CATALOG_YAML, "yaml")
}

/// Merge the catalog `base` with a user config `overlay`, producing the
/// effective configuration used at runtime.
///
/// Rules:
/// - Providers: catalog entries first; overlay providers with a new name are
///   appended, and same-named entries replace the catalog's.
/// - Routes: overlay routes win over catalog routes with the same `model`;
///   remaining catalog routes are appended (deduplicated by `model`).
/// - Defaults: the overlay wins when `provider`/`model` are set, otherwise the
///   catalog value is kept.
/// - Server: entirely owned by the overlay (falls back to defaults).
#[must_use]
pub fn effective_config(base: Config, overlay: Config) -> Config {
    let mut providers = base.providers.clone();
    for provider in overlay.providers {
        match providers.iter_mut().find(|p| p.name == provider.name) {
            Some(existing) => *existing = provider,
            None => providers.push(provider),
        }
    }

    let mut routes = overlay.routes.clone();
    for route in base.routes {
        if !routes.iter().any(|r| r.model == route.model) {
            routes.push(route);
        }
    }

    let defaults = crate::config::Defaults {
        provider: if overlay.defaults.provider.is_empty() {
            base.defaults.provider.clone()
        } else {
            overlay.defaults.provider.clone()
        },
        model: overlay.defaults.model.or(base.defaults.model),
    };

    Config {
        server: overlay.server,
        providers,
        routes,
        defaults,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Provider, ProviderFormat, Route};

    fn catalog() -> Config {
        Config::from_str(
            r"providers:
  - name: anthropic
    base_url: https://api.anthropic.com
    api_key_env: ANTHROPIC_API_KEY
    format: anthropic
    models: [claude-sonnet-4-5]
  - name: openai
    base_url: https://api.openai.com/v1
    api_key_env: OPENAI_API_KEY
    format: openai
    models: [gpt-4o]
routes:
  - model: claude-sonnet
    provider: anthropic
    prefix: true
    upstream_model: claude-sonnet-4-5
defaults:
  provider: anthropic
",
            "yaml",
        )
        .expect("catalog parses")
    }

    #[test]
    fn embedded_catalog_loads() {
        let config = load().expect("embedded catalog parses");
        assert!(!config.providers.is_empty());
        assert!(config.providers.iter().any(|p| p.name == "opencode-go"));
        assert!(config.providers.iter().any(|p| p.name == "anthropic"));
        assert!(!config.routes.is_empty());
        assert!(config.defaults.provider.is_empty());
    }

    #[test]
    fn overlay_adds_new_provider() {
        let overlay = Config::from_str(
            r"providers:
  - name: mylocal
    base_url: http://127.0.0.1:8080/v1
    format: openai
    models: [local-model]
",
            "yaml",
        )
        .expect("overlay parses");
        let merged = effective_config(catalog(), overlay);
        assert_eq!(merged.providers.len(), 3);
        assert!(merged.providers.iter().any(|p| p.name == "mylocal"));
    }

    #[test]
    fn overlay_overrides_same_named_provider() {
        let overlay = Config::from_str(
            r"providers:
  - name: anthropic
    base_url: http://127.0.0.1:9999/v1
    format: anthropic
    models: [custom-model]
",
            "yaml",
        )
        .expect("overlay parses");
        let merged = effective_config(catalog(), overlay);
        let anthropic = merged
            .providers
            .iter()
            .find(|p| p.name == "anthropic")
            .expect("provider present");
        assert_eq!(anthropic.base_url, "http://127.0.0.1:9999/v1");
        assert_eq!(anthropic.models, vec!["custom-model".to_string()]);
        assert_eq!(merged.providers.len(), 2);
    }

    #[test]
    fn overlay_route_wins_and_rest_kept() {
        let overlay = Config::from_str(
            r"routes:
  - model: claude-sonnet
    provider: openai
defaults:
  provider: openai
",
            "yaml",
        )
        .expect("overlay parses");
        let merged = effective_config(catalog(), overlay);
        assert_eq!(merged.defaults.provider, "openai");
        let route = merged
            .routes
            .iter()
            .find(|r| r.model == "claude-sonnet")
            .expect("route present");
        assert_eq!(route.provider, "openai");
        assert!(!route.prefix);
        assert!(!merged.routes.is_empty());
    }

    #[test]
    fn overlay_default_model_kept_and_base_passthrough() {
        let base = catalog();
        assert_eq!(base.defaults.model, None);

        let overlay = Config::from_str("defaults:\n  model: gpt-4o\n", "yaml").expect("parses");
        let merged = effective_config(base, overlay);
        assert_eq!(merged.defaults.model.as_deref(), Some("gpt-4o"));

        let empty = Config::from_str("", "yaml").expect("parses");
        let merged = effective_config(catalog(), empty);
        assert_eq!(merged.defaults.model, None);
    }

    #[test]
    fn provider_format_roundtrip() {
        let p: Provider = serde_yaml::from_str("name: x\nformat: openai\n").expect("parses");
        assert_eq!(p.format, ProviderFormat::Openai);
        let yaml = serde_yaml::to_string(&Route {
            model: "m".to_string(),
            provider: "p".to_string(),
            prefix: true,
            upstream_model: None,
        })
        .expect("serializes");
        assert!(yaml.contains("prefix: true"));
    }
}
