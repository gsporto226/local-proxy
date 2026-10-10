//! Hot-reload: a file watcher that drives [`AppState::reload`] when the config
//! or the credential store changes.

use std::path::{Path, PathBuf};
use std::time::Duration;

use notify::RecursiveMode;

use crate::application::runtime::AppState;

/// The file watcher could not be created or started.
#[derive(Debug, thiserror::Error, miette::Diagnostic)]
#[error("failed to start config watcher: {message}")]
#[diagnostic(code(runtime::watcher))]
pub struct WatchError {
    /// Underlying watcher error message.
    pub message: String,
}

/// Whether `path` is one of the files whose change triggers a hot-reload:
/// the active config file or the credential store.
fn is_reload_path(path: &Path, config_file: &std::ffi::OsStr) -> bool {
    path.file_name()
        .is_some_and(|name| name == config_file || name == "auth.json" || name == "accounts.db")
}

/// Watch `dirs` (non-recursively) and reload `app` when `config_path` or the
/// credential store inside them changes.
///
/// # Errors
///
/// Returns a [`WatchError`] if the watcher cannot be created or a directory
/// cannot be watched.
pub fn spawn(config_path: &Path, dirs: Vec<PathBuf>, app: &AppState) -> Result<(), WatchError> {
    let config_file = config_path
        .file_name()
        .map(std::ffi::OsStr::to_os_string)
        .unwrap_or_default();
    let (tx, rx) = std::sync::mpsc::channel();
    let mut debouncer = notify_debouncer_full::new_debouncer(
        Duration::from_millis(300),
        None,
        move |result: notify_debouncer_full::DebounceEventResult| {
            let Ok(events) = result else {
                return;
            };
            // The config dir also holds `local-proxy.log`, `stats.db` and `pid`,
            // which change on every request; only the reloadable files matter.
            let relevant = events
                .iter()
                .flat_map(|e| e.event.paths.iter())
                .any(|path| is_reload_path(path, &config_file));
            if relevant {
                let _ = tx.send(());
            }
        },
    )
    .map_err(|e| WatchError {
        message: format!("{e}"),
    })?;

    let mut unique: Vec<PathBuf> = Vec::new();
    for dir in dirs {
        if !unique.contains(&dir) {
            unique.push(dir);
        }
    }
    for dir in unique.iter().filter(|d| d.exists()) {
        debouncer
            .watch(dir, RecursiveMode::NonRecursive)
            .map_err(|e| WatchError {
                message: format!("failed to watch {}: {e}", dir.display()),
            })?;
    }

    let (btx, mut brx) = tokio::sync::mpsc::unbounded_channel::<()>();
    tokio::task::spawn_blocking(move || {
        while rx.recv().is_ok() {
            let _ = btx.send(());
        }
    });
    let app = app.clone();
    tokio::spawn(async move {
        let _debouncer = debouncer;
        while brx.recv().await.is_some() {
            match app.reload().await {
                // Everything else (providers, routes, auth) reloads from the file.
                Ok(()) => tracing::info!("config/auth change applied (hot-reload)"),
                Err(e) => tracing::warn!("hot-reload rebuild failed: {e}"),
            }
        }
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn watcher_reacts_only_to_config_and_auth() {
        let config = std::ffi::OsStr::new("config.yaml");
        assert!(is_reload_path(Path::new("C:/cfg/config.yaml"), config));
        assert!(is_reload_path(Path::new("C:/cfg/auth.json"), config));
        assert!(is_reload_path(Path::new("C:/cfg/accounts.db"), config));
        assert!(!is_reload_path(Path::new("C:/cfg/other.yaml"), config));
        assert!(!is_reload_path(Path::new("C:/cfg/local-proxy.log"), config));
        assert!(!is_reload_path(Path::new("C:/cfg/stats.db"), config));
        assert!(!is_reload_path(Path::new("C:/cfg/stats.db-wal"), config));
        assert!(!is_reload_path(Path::new("C:/cfg/pid"), config));
    }
}
