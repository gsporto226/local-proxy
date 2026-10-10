//! The proxy's live state: the effective config, router and upstream accounts
//! (rebuilt on hot-reload), plus per-instance state that survives reloads.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use miette::Diagnostic;
use thiserror::Error;
use tokio::sync::RwLock;

use crate::application::usage::UsageRecorder;
use crate::domain::config::{Config, ConfigError, Defaults};
use crate::domain::router::{Router, RouterError};
use crate::ports::{AccountMap, CredentialError, Ports, UpstreamError};

/// The current, hot-reloadable runtime state shared by the request handlers.
#[derive(Clone)]
pub struct RuntimeState {
    /// The effective (catalog-merged) configuration.
    pub config: Arc<Config>,
    /// The model/router resolution logic.
    pub router: Arc<Router>,
    /// The upstream accounts, keyed by provider name and account alias.
    pub accounts: Arc<AccountMap>,
    /// When true, requests that carry a client-sent model are forced to use the
    /// proxy's `active_model` instead. Used by `local-proxy launch claude` so the
    /// launched tool's own model selection never overrides the user's choice.
    pub enforce_active_model: bool,
    /// The `--model` this instance was launched with: kept as its default
    /// model across hot-reloads instead of the persisted one.
    pub model_override: Option<String>,
    /// The config file this instance was started with (used by `$proxy model`
    /// to persist the selection).
    pub config_path: PathBuf,
}

/// Errors while (re)building the runtime state.
#[derive(Debug, Error, Diagnostic)]
pub enum RuntimeError {
    /// The config overlay could not be loaded.
    #[error("failed to load config {path}: {source}")]
    #[diagnostic(code(runtime::config))]
    Config {
        /// Path of the config file.
        path: String,
        /// Underlying config error.
        #[source]
        source: ConfigError,
    },
    /// The router could not be built.
    #[error("failed to build router: {0}")]
    #[diagnostic(code(runtime::router))]
    Router(#[source] RouterError),
    /// The account database or vault could not be read.
    #[error("failed to load accounts: {0}")]
    Auth(#[source] CredentialError),
    /// The upstream clients could not be built.
    #[error("failed to build upstream clients: {0}")]
    #[diagnostic(code(runtime::clients))]
    Clients(#[source] UpstreamError),
}

/// The effective config for `config_path`: the embedded catalog merged with
/// the user's overlay.
///
/// # Errors
///
/// Returns a [`RuntimeError::Config`] naming the overlay or the catalog.
#[allow(clippy::result_large_err)]
pub fn effective_config(ports: &Ports, config_path: &Path) -> Result<Config, RuntimeError> {
    let overlay =
        ports
            .config
            .load_overlay(config_path)
            .map_err(|source| RuntimeError::Config {
                path: config_path.display().to_string(),
                source,
            })?;
    let base = crate::domain::catalog::load().map_err(|source| RuntimeError::Config {
        path: "<catalog>".to_string(),
        source,
    })?;
    Ok(crate::domain::catalog::effective_config(base, overlay))
}

/// Build the effective runtime state for `config_path`: load the overlay,
/// merge it with the catalog, discover models, and build the router and
/// upstream accounts.
///
/// # Errors
///
/// Returns a [`RuntimeError`] if the config, router, or accounts fail to build.
#[allow(clippy::result_large_err)]
pub fn build_runtime_state(
    ports: &Ports,
    config_path: &Path,
) -> Result<RuntimeState, RuntimeError> {
    let mut config = effective_config(ports, config_path)?;
    let auth = ports.credentials.read_all().map_err(RuntimeError::Auth)?;
    ports.upstream.discover_models(&mut config, &auth);
    let config = Arc::new(config);
    let router = Arc::new(Router::new(config.clone()).map_err(RuntimeError::Router)?);
    let accounts = Arc::new(build_accounts(ports, &config)?);
    Ok(RuntimeState {
        config,
        router,
        accounts,
        enforce_active_model: false,
        model_override: None,
        config_path: config_path.to_path_buf(),
    })
}

/// Build an upstream account for every stored credential of every provider; a
/// provider without credentials gets one `default` placeholder so
/// passthrough keys keep working.
///
/// # Errors
///
/// Returns an error if the credential store cannot be read or an account
/// cannot be built.
#[allow(clippy::result_large_err)]
pub fn build_accounts(ports: &Ports, config: &Config) -> Result<AccountMap, RuntimeError> {
    let passthrough = config.server.passthrough_keys;
    let auth = ports.credentials.read_all().map_err(RuntimeError::Auth)?;
    let mut map = HashMap::new();
    let mut connected = Vec::new();
    for provider in &config.providers {
        let mut accounts = HashMap::new();
        if let Some(entries) = auth.get(&provider.name) {
            for (alias, entry) in entries {
                let account = ports
                    .upstream
                    .connect(provider, alias, passthrough, Some(entry.clone()))
                    .map_err(RuntimeError::Clients)?;
                if account.has_credentials() {
                    connected.push(format!("{}:{alias}", provider.name));
                }
                accounts.insert(alias.clone(), account);
            }
        }
        if accounts.is_empty() {
            accounts.insert(
                "default".to_string(),
                ports
                    .upstream
                    .connect(provider, "default", passthrough, None)
                    .map_err(RuntimeError::Clients)?,
            );
        }
        map.insert(provider.name.clone(), accounts);
    }
    tracing::info!(
        target: crate::LOG_TARGET,
        providers = config.providers.len(),
        connected = %connected.join(", "),
        "built upstream clients"
    );
    Ok(map)
}

/// Carry this instance's launch flags across a hot-reload: `--model` (which
/// outranks the persisted model) and `--enforce-active-model`. Everything
/// else follows the file, so new sessions start from the last saved settings;
/// existing sessions keep their own frozen defaults (see
/// [`AppState::session_state`]).
fn carry_instance_state(old: &RuntimeState, new: &mut RuntimeState) {
    if let Some(model) = &old.model_override {
        let mut cfg = (*new.config).clone();
        cfg.defaults.active_model = Some(model.clone());
        new.config = Arc::new(cfg);
    }
    new.model_override.clone_from(&old.model_override);
    new.enforce_active_model = old.enforce_active_model;
}

/// One session's own settings: its defaults, frozen from the instance's when
/// the session was first seen, and its `$proxy account` pins.
#[derive(Clone)]
struct Session {
    defaults: Defaults,
    pins: HashMap<String, String>,
}

impl Session {
    fn new(defaults: &Defaults) -> Self {
        Self {
            defaults: defaults.clone(),
            pins: HashMap::new(),
        }
    }
}

/// Shared application state: the runtime state behind a lock, the session
/// account pins, and the ports every use case runs through.
#[derive(Clone)]
pub struct AppState {
    inner: Arc<RwLock<RuntimeState>>,
    /// Per-session settings, keyed by session id. In-memory only: a restart
    /// drops them and every session starts from the persisted defaults.
    // ponytail: never evicted, one small entry per session id; add an LRU cap
    // if a long-lived `serve` sees unbounded session ids.
    sessions: Arc<RwLock<HashMap<String, Session>>>,
    ports: Ports,
    recorder: UsageRecorder,
    port: Arc<OnceLock<u16>>,
    // ponytail: one global lock avoids duplicate refreshes; use per-account
    // locks if unrelated endpoint latency becomes a bottleneck.
    usage_refresh: Arc<tokio::sync::Mutex<()>>,
}

impl AppState {
    /// Wrap a [`RuntimeState`] in shared, lockable application state.
    #[must_use]
    pub fn new(state: RuntimeState, ports: Ports) -> Self {
        Self {
            inner: Arc::new(RwLock::new(state)),
            sessions: Arc::new(RwLock::new(HashMap::new())),
            recorder: UsageRecorder::new(&ports),
            ports,
            port: Arc::new(OnceLock::new()),
            usage_refresh: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    /// The ports this instance runs through.
    #[must_use]
    pub const fn ports(&self) -> &Ports {
        &self.ports
    }

    /// The usage recorder.
    #[must_use]
    pub const fn recorder(&self) -> &UsageRecorder {
        &self.recorder
    }

    /// Record the port the server bound (reported by the status endpoint).
    pub fn set_port(&self, port: u16) {
        let _ = self.port.set(port);
    }

    /// The port the server bound, once known.
    #[must_use]
    pub fn port(&self) -> Option<u16> {
        self.port.get().copied()
    }

    /// Serializes account-usage refreshes across concurrent pollers.
    pub(crate) async fn usage_refresh_lock(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.usage_refresh.lock().await
    }

    /// The account pinned to `session_id` for `provider`, if any.
    pub async fn session_account(&self, session_id: &str, provider: &str) -> Option<String> {
        self.sessions
            .read()
            .await
            .get(session_id)
            .and_then(|session| session.pins.get(provider))
            .cloned()
    }

    /// All account pins of `session_id` (`provider -> alias`).
    pub async fn session_accounts(&self, session_id: &str) -> HashMap<String, String> {
        self.sessions
            .read()
            .await
            .get(session_id)
            .map(|session| session.pins.clone())
            .unwrap_or_default()
    }

    /// Pin `alias` as the account of `session_id` for `provider`. A request
    /// without a session id cannot pin anything (there is no session).
    pub async fn set_session_account(&self, session_id: &str, provider: &str, alias: &str) {
        self.with_session(session_id, |session| {
            session.pins.insert(provider.to_string(), alias.to_string());
        })
        .await;
    }

    /// Drop the session's account pins and selected defaults: one provider,
    /// or all of them.
    pub async fn clear_session_accounts(&self, session_id: &str, provider: Option<&str>) {
        self.with_session(session_id, |session| {
            if let Some(provider) = provider {
                session.pins.remove(provider);
                session.defaults.active_accounts.remove(provider);
            } else {
                session.pins.clear();
                session.defaults.active_accounts.clear();
            }
        })
        .await;
    }

    /// Snapshot the current runtime state (cheap Arc clones).
    pub async fn snapshot(&self) -> RuntimeState {
        self.inner.read().await.clone()
    }

    /// The runtime state as `session_id` sees it: this instance's state with
    /// the session's own defaults (model, effort, accounts). The first call
    /// for a session freezes the instance's current defaults for it, so later
    /// global changes only reach new sessions. Without a session id this is
    /// [`Self::snapshot`].
    pub async fn session_state(&self, session_id: &str) -> RuntimeState {
        let mut state = self.snapshot().await;
        if session_id.is_empty() {
            return state;
        }
        let defaults = self
            .sessions
            .write()
            .await
            .entry(session_id.to_string())
            .or_insert_with(|| Session::new(&state.config.defaults))
            .defaults
            .clone();
        let mut config = (*state.config).clone();
        config.defaults = defaults;
        state.config = Arc::new(config);
        state
    }

    /// Change the defaults of `session_id` only, or this instance's defaults
    /// (seen by requests without a session) when the id is empty. Never
    /// touches other sessions or other running proxies; persistence is
    /// handled separately by the caller.
    pub async fn update_defaults(&self, session_id: &str, change: impl FnOnce(&mut Defaults)) {
        if session_id.is_empty() {
            let mut guard = self.inner.write().await;
            let mut config = (*guard.config).clone();
            change(&mut config.defaults);
            guard.config = Arc::new(config);
        } else {
            self.with_session(session_id, |session| change(&mut session.defaults))
                .await;
        }
    }

    /// Run `f` on the session, freezing the instance defaults for it first if
    /// it is new. A no-op without a session id.
    async fn with_session(&self, session_id: &str, f: impl FnOnce(&mut Session)) {
        if session_id.is_empty() {
            return;
        }
        let defaults = self.snapshot().await.config.defaults.clone();
        let mut sessions = self.sessions.write().await;
        f(sessions
            .entry(session_id.to_string())
            .or_insert_with(|| Session::new(&defaults)));
    }

    /// Rebuild the runtime state from the config file and credential store,
    /// keeping this instance's own active model and launch flags.
    ///
    /// # Errors
    ///
    /// Returns a [`RuntimeError`] when the rebuild fails; the current state is
    /// kept in that case.
    #[allow(clippy::result_large_err)]
    pub async fn reload(&self) -> Result<(), RuntimeError> {
        let config_path = self.snapshot().await.config_path;
        let ports = self.ports.clone();
        let mut new_state =
            tokio::task::spawn_blocking(move || build_runtime_state(&ports, &config_path))
                .await
                .unwrap_or_else(|e| std::panic::resume_unwind(e.into_panic()))?;
        let mut guard = self.inner.write().await;
        carry_instance_state(&guard, &mut new_state);
        *guard = new_state;
        drop(guard);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::testing::empty_state;

    #[test]
    fn hot_reload_keeps_launch_flags_and_follows_the_file() {
        let make = |model: Option<&str>, effort: Option<&str>| {
            let mut cfg = Config::default();
            cfg.defaults.active_model = model.map(str::to_string);
            cfg.defaults.active_effort = effort.map(str::to_string);
            empty_state(cfg)
        };
        // Without `--model` the reloaded file wins.
        let old = make(Some("old"), Some("low"));
        let mut new = make(Some("saved"), Some("high"));
        carry_instance_state(&old, &mut new);
        assert_eq!(new.config.defaults.active_model.as_deref(), Some("saved"));
        assert_eq!(new.config.defaults.active_effort.as_deref(), Some("high"));

        // `--model` and `--enforce-active-model` survive the reload.
        let old = RuntimeState {
            enforce_active_model: true,
            model_override: Some("kimi".to_string()),
            ..make(Some("kimi"), None)
        };
        let mut new = make(Some("saved"), None);
        carry_instance_state(&old, &mut new);
        assert!(new.enforce_active_model);
        assert_eq!(new.config.defaults.active_model.as_deref(), Some("kimi"));
    }

    #[tokio::test]
    async fn sessions_freeze_defaults_and_change_independently() {
        let mut cfg = Config::default();
        cfg.defaults.active_model = Some("global".to_string());
        let app = AppState::new(empty_state(cfg), crate::bootstrap::ports());
        let model = |state: RuntimeState| state.config.defaults.active_model.clone();

        // `a` is seen first and freezes the global default.
        assert_eq!(
            model(app.session_state("a").await).as_deref(),
            Some("global")
        );
        // A session's own change touches only that session.
        app.update_defaults("b", |d| d.active_model = Some("mine".to_string()))
            .await;
        assert_eq!(model(app.session_state("b").await).as_deref(), Some("mine"));
        assert_eq!(
            model(app.session_state("a").await).as_deref(),
            Some("global")
        );
        assert_eq!(model(app.snapshot().await).as_deref(), Some("global"));

        // A new global default (hot-reload) reaches new sessions only.
        app.update_defaults("", |d| d.active_model = Some("new".to_string()))
            .await;
        assert_eq!(
            model(app.session_state("a").await).as_deref(),
            Some("global")
        );
        assert_eq!(model(app.session_state("c").await).as_deref(), Some("new"));
    }

    #[test]
    fn rebuild_merges_catalog_with_overlay_and_reapplies() {
        // `build_runtime_state` reads the credential store: keep this test off
        // the real config dir and out of the way of other env-mutating tests.
        let _guard = crate::TEST_STATE_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("LOCAL_PROXY_CONFIG_DIR", dir.path());
        let ports = crate::bootstrap::ports();
        let path = dir.path().join("config.yaml");
        std::fs::write(
            &path,
            "providers:\n  - name: mylocal\n    base_url: http://127.0.0.1:9/v1\n    format: openai\n    models: [m]\n",
        )
        .expect("write config");

        let first = build_runtime_state(&ports, &path).expect("first build");
        assert!(first.config.providers.iter().any(|p| p.name == "mylocal"));
        assert!(first.config.providers.iter().any(|p| p.name == "anthropic"));

        // Simulate a hot-reload: the user edits the config file, adding a provider.
        std::fs::write(
            &path,
            "providers:\n  - name: mylocal\n    base_url: http://127.0.0.1:9/v1\n    format: openai\n    models: [m]\n  - name: second\n    base_url: http://127.0.0.1:9/v1\n    format: openai\n    models: [m]\n",
        )
        .expect("rewrite config");
        let second = build_runtime_state(&ports, &path).expect("rebuild");
        assert!(second.config.providers.iter().any(|p| p.name == "second"));

        std::env::remove_var("LOCAL_PROXY_CONFIG_DIR");
    }
}
