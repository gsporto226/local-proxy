//! [`ConfigStore`] over YAML/JSON files on disk.

use std::path::Path;

use crate::domain::config::{Config, ConfigError, DEFAULT_CONFIG};
use crate::ports::ConfigStore;

/// Config overlays stored as YAML (or JSON, by extension) files.
#[derive(Debug, Clone, Copy, Default)]
pub struct FileConfigStore;

/// Load the config file at `path`, inferring the format from its extension.
///
/// # Errors
///
/// Returns [`ConfigError`] if the file cannot be read or parsed.
#[allow(clippy::result_large_err)]
pub fn load(path: &Path) -> Result<Config, ConfigError> {
    let content = std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
        path: path.display().to_string(),
        source,
    })?;
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    Config::parse(&content, &ext, &path.display().to_string())
}

/// Create `path` (and its parent directories) holding [`DEFAULT_CONFIG`].
///
/// # Errors
///
/// Returns [`ConfigError::Create`] if the directory or file cannot be written.
#[allow(clippy::result_large_err)]
pub fn create_default(path: &Path) -> Result<(), ConfigError> {
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

/// Serialize `config` and replace `path` atomically (temp file + rename).
#[allow(clippy::result_large_err)]
fn save(path: &Path, config: &Config) -> Result<(), ConfigError> {
    let yaml = serde_yaml::to_string(config).map_err(|e| ConfigError::Serialize {
        message: e.to_string(),
    })?;
    let tmp = path.with_extension("yaml.tmp");
    let write_err = |source| ConfigError::Write {
        path: path.display().to_string(),
        source,
    };
    std::fs::write(&tmp, yaml).map_err(write_err)?;
    std::fs::rename(&tmp, path).map_err(write_err)
}

impl ConfigStore for FileConfigStore {
    fn load(&self, path: &Path) -> Result<Config, ConfigError> {
        load(path)
    }

    fn create_default(&self, path: &Path) -> Result<(), ConfigError> {
        create_default(path)
    }

    fn update(&self, path: &Path, change: &mut dyn FnMut(&mut Config)) -> Result<(), ConfigError> {
        if !path.exists() {
            create_default(path)?;
        }
        let mut config = load(path)?;
        change(&mut config);
        save(path, &config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::config::ProviderFormat;

    const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");

    #[test]
    fn loads_yaml_fixture() {
        let config = load(&Path::new(FIXTURES).join("config.yaml")).expect("yaml loads");

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
        let config = load(&Path::new(FIXTURES).join("config.json")).expect("json loads");

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
    fn create_default_writes_parseable_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("config.yaml");
        create_default(&path).expect("create default config");

        let config = load(&path).expect("loaded default config");
        assert_eq!(config.server.port, 8787);
        assert!(config.providers.is_empty());
    }

    #[test]
    fn update_creates_then_persists_changes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        let store = FileConfigStore;

        store
            .update(&path, &mut |c| {
                c.defaults.active_model = Some("deepseek-v4-flash".to_string());
            })
            .unwrap();
        let config = load(&path).unwrap();
        assert_eq!(
            config.defaults.active_model.as_deref(),
            Some("deepseek-v4-flash")
        );
        // the default server block came along with the created file
        assert_eq!(config.server.api_keys, vec!["sk-proxy".to_string()]);

        store
            .update(&path, &mut |c| c.defaults.active_model = None)
            .unwrap();
        assert_eq!(load(&path).unwrap().defaults.active_model, None);
    }

    #[test]
    fn load_overlay_defaults_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        let overlay = FileConfigStore
            .load_overlay(&dir.path().join("absent.yaml"))
            .unwrap();
        assert!(overlay.providers.is_empty());
    }
}
