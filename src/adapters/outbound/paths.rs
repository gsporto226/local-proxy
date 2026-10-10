//! Where local-proxy keeps its files: the per-user config dir and the
//! config, credential, stats, pid, and log files inside it.

use std::path::PathBuf;

/// Environment variable that overrides the default config file path.
pub const ENV_CONFIG_PATH: &str = "LOCAL_PROXY_CONFIG";

/// Default config file name looked up in the working directory.
pub const DEFAULT_CONFIG_PATH: &str = "config.yaml";

/// Returns the per-user config directory for local-proxy.
///
/// Overridden by the `LOCAL_PROXY_CONFIG_DIR` environment variable (so tests
/// and tooling can isolate the config dir and the `stats.db` it contains).
/// Otherwise prefers `directories::ProjectDirs` (`.config_dir()`): on Windows
/// this is `%APPDATA%\local-proxy`, on Unix `~/.config/local-proxy`. Falls back
/// to the `APPDATA` (Windows) or `HOME`/`USERPROFILE` environment variables,
/// and finally to a local `.config/local-proxy` directory. Never panics.
#[must_use]
pub fn config_dir() -> PathBuf {
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

/// The default global config file, `config_dir()/config.yaml`.
#[must_use]
pub fn global_config_path() -> PathBuf {
    config_dir().join("config.yaml")
}

/// The encrypted account database.
#[must_use]
pub fn accounts_db() -> PathBuf {
    config_dir().join("accounts.db")
}

/// The legacy plaintext credential file, only read to migrate it.
#[must_use]
pub fn legacy_auth_file() -> PathBuf {
    config_dir().join("auth.json")
}

/// The usage statistics database.
#[must_use]
pub fn stats_db() -> PathBuf {
    config_dir().join("stats.db")
}

/// The file holding the background proxy's process ID.
#[must_use]
pub fn pid_file() -> PathBuf {
    config_dir().join("pid")
}

/// The proxy's log file.
#[must_use]
pub fn log_file() -> PathBuf {
    config_dir().join("local-proxy.log")
}

/// The config path from [`ENV_CONFIG_PATH`], if set and non-empty.
#[must_use]
pub fn env_config_path() -> Option<PathBuf> {
    std::env::var(ENV_CONFIG_PATH)
        .ok()
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
}

/// Resolve the config path from an explicit flag, the environment, the current
/// working directory, or the global default.
///
/// Precedence: an explicit `--config` flag, then `LOCAL_PROXY_CONFIG`, then a
/// `config.yaml`/`config.json` in the current working directory (only if one
/// exists), then the global per-user default. Flag/env paths are returned as-is
/// without checking existence; only the working-directory candidates are
/// existence-checked.
#[must_use]
pub fn resolve_config_path(flag: Option<PathBuf>) -> PathBuf {
    if let Some(path) = flag {
        return path;
    }
    if let Some(path) = env_config_path() {
        return path;
    }
    for name in [DEFAULT_CONFIG_PATH, "config.json"] {
        let candidate = PathBuf::from(name);
        if candidate.exists() {
            return std::env::current_dir().map_or(candidate, |dir| dir.join(name));
        }
    }
    global_config_path()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn env_config_path_override() {
        let _guard = crate::TEST_STATE_LOCK.lock().unwrap();
        std::env::remove_var(ENV_CONFIG_PATH);
        assert!(env_config_path().is_none());

        std::env::set_var(ENV_CONFIG_PATH, "custom.yaml");
        assert_eq!(env_config_path().as_deref(), Some(Path::new("custom.yaml")));
        std::env::remove_var(ENV_CONFIG_PATH);
    }

    #[test]
    fn config_dir_env_override() {
        let _guard = crate::TEST_STATE_LOCK.lock().unwrap();
        std::env::remove_var("LOCAL_PROXY_CONFIG_DIR");
        let default = config_dir();
        assert_ne!(default, PathBuf::new());

        std::env::set_var("LOCAL_PROXY_CONFIG_DIR", "/tmp/lp-e2e");
        assert_eq!(config_dir(), PathBuf::from("/tmp/lp-e2e"));
        std::env::remove_var("LOCAL_PROXY_CONFIG_DIR");
    }

    #[test]
    fn global_config_path_ends_with_config_yaml() {
        let _guard = crate::TEST_STATE_LOCK.lock().unwrap();
        let path = global_config_path();
        assert_eq!(
            path.file_name().and_then(|n| n.to_str()),
            Some("config.yaml")
        );
    }

    #[test]
    fn runtime_paths_live_under_config_dir() {
        let _guard = crate::TEST_STATE_LOCK.lock().unwrap();
        assert!(pid_file().to_string_lossy().contains("local-proxy"));
        assert!(log_file().to_string_lossy().contains("local-proxy"));
        assert!(pid_file().starts_with(config_dir()));
    }

    #[test]
    fn resolve_config_path_uses_cwd_yaml_when_present() {
        let _guard = crate::TEST_STATE_LOCK.lock().unwrap();
        let prev = std::env::current_dir().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("config.yaml"), "server: {}\n").unwrap();
        std::env::remove_var(ENV_CONFIG_PATH);
        std::env::set_current_dir(tmp.path()).unwrap();
        let result = resolve_config_path(None);
        std::env::set_current_dir(prev).unwrap();
        assert_eq!(result, tmp.path().join("config.yaml"));
    }

    #[test]
    fn resolve_config_path_defaults_to_global_when_no_cwd_file() {
        let _guard = crate::TEST_STATE_LOCK.lock().unwrap();
        let prev = std::env::current_dir().unwrap();
        std::env::remove_var(ENV_CONFIG_PATH);
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_current_dir(tmp.path()).unwrap();
        let result = resolve_config_path(None);
        std::env::set_current_dir(prev).unwrap();
        assert_eq!(result, global_config_path());
    }

    #[test]
    fn resolve_config_path_explicit_flag_wins() {
        let explicit = PathBuf::from("/explicit/custom.yaml");
        assert_eq!(resolve_config_path(Some(explicit.clone())), explicit);
    }
}
