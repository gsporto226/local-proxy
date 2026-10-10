#![allow(clippy::result_large_err)]

use std::path::Path;

use crate::domain::config::{Config, ConfigError};

/// Persistence of the user's config overlay (the catalog itself is embedded).
pub trait ConfigStore: Send + Sync {
    /// Load the config file at `path`; a missing file is an error.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] if the file cannot be read or parsed.
    fn load(&self, path: &Path) -> Result<Config, ConfigError>;

    /// Load the overlay at `path`, or the empty default when it does not exist.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] if an existing file cannot be read or parsed.
    fn load_overlay(&self, path: &Path) -> Result<Config, ConfigError> {
        if path.exists() {
            self.load(path)
        } else {
            Ok(Config::default())
        }
    }

    /// Write the embedded default config to `path`, creating its directory.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Create`] if the file cannot be written.
    fn create_default(&self, path: &Path) -> Result<(), ConfigError>;

    /// Apply `change` to the overlay at `path` and save it atomically, creating
    /// the default config first when the file does not exist yet.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] if the file cannot be read, parsed, or written.
    fn update(&self, path: &Path, change: &mut dyn FnMut(&mut Config)) -> Result<(), ConfigError>;
}
