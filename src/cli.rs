//! CLI launcher: background `serve`, `launch claude|design`, `status`, `stop`,
//! `models`. Pure helpers are unit-testable; process spawns are best-effort.

use std::io;
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use miette::Diagnostic;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::config::{Config, DEFAULT_CONFIG_PATH};
use crate::handlers::{self, AppState};

// ---------------------------------------------------------------------------
// errors
// ---------------------------------------------------------------------------

/// Errors surfaced by the CLI, formatted richly via miette.
#[derive(Debug, Error, Diagnostic)]
pub enum CliError {
    /// Configuration could not be loaded or parsed.
    #[error("failed to load configuration")]
    #[diagnostic(
        code(cli::config),
        help("check that the config file exists and is valid YAML/JSON")
    )]
    Config(#[from] crate::config::ConfigError),

    /// The router could not be built from the configuration.
    #[error("failed to build router")]
    #[diagnostic(
        code(cli::router),
        help("every route must reference a configured provider")
    )]
    Router(#[from] crate::router::RouterError),

    /// The upstream HTTP clients could not be built.
    #[error("failed to build upstream HTTP clients")]
    #[diagnostic(
        code(cli::clients),
        help("check that every provider has a valid base_url and API key env var")
    )]
    Clients(#[from] crate::upstream::UpstreamError),

    /// An operating-system level I/O operation failed.
    #[error("I/O error: {0}")]
    #[diagnostic(code(cli::io))]
    Io(#[from] std::io::Error),

    /// The HTTP server could not start or serve.
    #[error("failed to serve HTTP: {0}")]
    #[diagnostic(code(cli::serve))]
    Serve(#[from] axum::Error),

    /// An external CLI tool could not be spawned.
    #[error("{message}")]
    #[diagnostic(code(cli::tool), help("ensure the CLI tool is installed and on PATH"))]
    Tool {
        /// Human-readable description of the failure.
        message: String,
    },

    /// The proxy could not self-update from GitHub Releases.
    #[error("failed to update local-proxy")]
    #[diagnostic(
        code(cli::update),
        help("check the release exists and the network is reachable")
    )]
    Update(#[from] UpdateError),

    /// A `connect`/`disconnect` operation failed.
    #[error("{message}")]
    #[diagnostic(code(cli::connect))]
    Connect {
        /// Human-readable description of the failure.
        message: String,
    },

    /// The auth store could not be read or written.
    #[error("auth store error: {0}")]
    #[diagnostic(code(cli::auth), help("check that auth.json is valid JSON"))]
    Auth(#[from] crate::auth::AuthError),

    /// The runtime state (config/router/clients) could not be built or reloaded.
    #[error("failed to build runtime state")]
    #[diagnostic(
        code(cli::runtime),
        help("check the config file and the embedded catalog are valid")
    )]
    Runtime(#[from] crate::handlers::RuntimeError),

    /// The local usage statistics could not be read.
    #[error("failed to read usage statistics")]
    #[diagnostic(
        code(cli::stats),
        help("the stats database is created automatically on the first proxied request")
    )]
    Stats(#[from] crate::stats::StatsError),
}

// ---------------------------------------------------------------------------
// runtime files (global per-user config dir)
// ---------------------------------------------------------------------------

/// The runtime directory (the global per-user config dir), shared by config,
/// pid, and log files.
#[must_use]
pub fn config_dir() -> PathBuf {
    crate::config::global_config_dir()
}

/// Path to the file holding the proxy's process ID.
#[must_use]
pub fn pid_file() -> PathBuf {
    config_dir().join("pid")
}

/// Path to the proxy's log file.
#[must_use]
pub fn log_file() -> PathBuf {
    config_dir().join("local-proxy.log")
}

/// Write the given process ID to the pid file, creating the runtime dir if needed.
///
/// # Errors
///
/// Returns an error if the runtime directory cannot be created or the pid file
/// cannot be written.
pub fn write_pid(pid: u32) -> io::Result<()> {
    std::fs::create_dir_all(config_dir())?;
    std::fs::write(pid_file(), pid.to_string())
}

/// Remove the pid file, ignoring errors if it does not exist.
pub fn remove_pid() {
    let _ = std::fs::remove_file(pid_file());
}

/// Read the stored process ID, if any.
#[must_use]
pub fn read_pid() -> Option<u32> {
    std::fs::read_to_string(pid_file())
        .ok()
        .and_then(|s| s.trim().parse().ok())
}

/// Best-effort kill of the given process ID.
pub fn stop_process(pid: u32) {
    #[cfg(windows)]
    let _ = Command::new("taskkill")
        .args(["/F", "/PID", &pid.to_string()])
        .status();
    #[cfg(not(windows))]
    let _ = Command::new("kill").args(["-9", &pid.to_string()]).status();
}

/// Is something accepting TCP connections at host:port (proxy likely up)?
#[must_use]
pub fn is_serving(host: &str, port: u16) -> bool {
    let addr = format!("{host}:{port}");
    if let Ok(mut addrs) = addr.to_socket_addrs() {
        if let Some(sa) = addrs.next() {
            return TcpStream::connect_timeout(&sa, Duration::from_secs(1)).is_ok();
        }
    }
    false
}

/// Spawn this same binary detached (background) with the given args.
///
/// # Errors
///
/// Returns an error if the current executable, runtime directory, or log file
/// cannot be set up, or if the process cannot be spawned.
pub fn spawn_background(args: &[String]) -> io::Result<std::process::Child> {
    let exe = std::env::current_exe()?;
    std::fs::create_dir_all(config_dir())?;
    let log = std::fs::File::create(log_file())?;
    let err = log.try_clone()?;

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let mut cmd = Command::new(exe);
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NO_WINDOW);
        cmd.args(args)
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(err))
            .stdin(Stdio::null());
        cmd.spawn()
    }
    #[cfg(not(windows))]
    {
        let mut cmd = Command::new(exe);
        cmd.args(args)
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(err))
            .stdin(Stdio::null());
        cmd.spawn()
    }
}

// ---------------------------------------------------------------------------
// config loading
// ---------------------------------------------------------------------------

/// Load configuration for the CLI, auto-creating the default global config on
/// first run.
///
/// If `config_path` is the global default and it does not exist yet, a default
/// config is written there and a message is printed so the user can edit it.
/// Explicit flag/env/cwd paths are never auto-created; their load errors are
/// surfaced as-is.
///
/// # Errors
///
/// Returns a [`CliError::Config`] if the config cannot be created or loaded.
#[allow(clippy::result_large_err)]
fn load_config(config_path: &Path) -> Result<Config, CliError> {
    if config_path == crate::config::global_config_path() && !config_path.exists() {
        crate::config::create_default_config(config_path).map_err(CliError::from)?;
        println!(
            "criado config default em {} — edite e rode de novo",
            config_path.display()
        );
    }
    Config::load(config_path).map_err(CliError::from)
}

// ---------------------------------------------------------------------------
// serve
// ---------------------------------------------------------------------------

/// Start the proxy server, either detached in the background or in the
/// foreground, binding to the configured host and port.
///
/// # Errors
///
/// Returns an error if the config cannot be loaded, the router or clients
/// cannot be built, the listener cannot bind, or serving fails.
pub async fn serve(
    config_path: PathBuf,
    host_flag: Option<String>,
    port_flag: Option<u16>,
    background: bool,
    check_update: bool,
) -> miette::Result<()> {
    if background {
        let mut args = vec![
            "serve".to_string(),
            "--config".to_string(),
            config_path.display().to_string(),
        ];
        if let Some(h) = host_flag {
            args.push("--host".to_string());
            args.push(h);
        }
        if let Some(p) = port_flag {
            args.push("--port".to_string());
            args.push(p.to_string());
        }
        if check_update {
            args.push("--check-update".to_string());
        }
        let child = spawn_background(&args).map_err(CliError::from)?;
        println!(
            "started in background (pid {}), log: {}",
            child.id(),
            log_file().display()
        );
        return Ok(());
    }

    init_tracing();
    if let Ok(exe) = std::env::current_exe() {
        cleanup_stale_backups(&exe);
    }
    if check_update && std::env::var_os("LOCAL_PROXY_DISABLE_AUTOUPDATE").is_none() {
        if let Some(v) = latest_available_version().await {
            tracing::warn!(
                target: crate::LOG_TARGET,
                %v,
                "uma versão mais recente está disponível; rode `local-proxy update` para atualizar"
            );
        }
    }
    let runtime = handlers::build_runtime_state(&config_path).map_err(CliError::from)?;
    tracing::info!(target: crate::LOG_TARGET, path = %config_path.display(), "loaded config");

    let host = host_flag.unwrap_or_else(|| runtime.config.server.host.clone());
    let port = port_flag.unwrap_or(runtime.config.server.port);
    let addr = format!("{host}:{port}");

    let state = AppState::new(runtime);
    handlers::spawn_watcher(config_path.clone(), &state).map_err(CliError::from)?;
    let app = handlers::app(state);

    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .map_err(CliError::from)?;
    write_pid(std::process::id()).map_err(CliError::from)?;
    tracing::info!(target: crate::LOG_TARGET, %addr, "listening");
    let result = axum::serve(listener, app).await;
    remove_pid();
    result.map_err(CliError::from)?;
    Ok(())
}

fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| format!("{}={},tower_http=debug", crate::LOG_TARGET, "debug").into());

    // Mirror the proxy's logs into the per-user app data dir alongside the pid
    // file, in addition to the console, so issues can be diagnosed from the log
    // file even when the server runs in the background or detached. If the file
    // cannot be opened (e.g. read-only config dir), fall back to stdout only.
    let file_writer = std::fs::create_dir_all(config_dir())
        .and_then(|()| std::fs::File::create(log_file()))
        .map(std::sync::Arc::new);

    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true);
    match file_writer {
        Ok(file) => {
            use tracing_subscriber::fmt::writer::MakeWriterExt;
            let _ = builder.with_writer(std::io::stdout.and(file)).try_init();
        }
        Err(_) => {
            let _ = builder.try_init();
        }
    }
}

// ---------------------------------------------------------------------------
// launch
// ---------------------------------------------------------------------------

/// The env vars that make an Anthropic-compatible tool point at this proxy.
#[must_use]
pub fn launch_environment(config: &Config, model: Option<&str>) -> Vec<(String, String)> {
    let base = format!("http://{}:{}", config.server.host, config.server.port);
    let auth = config
        .server
        .api_keys
        .first()
        .cloned()
        .unwrap_or_else(|| "unused".to_string());
    let mut env = vec![
        ("ANTHROPIC_BASE_URL".to_string(), base),
        ("ANTHROPIC_API_KEY".to_string(), auth.clone()),
        ("ANTHROPIC_AUTH_TOKEN".to_string(), auth),
    ];
    if let Some(m) = model.filter(|m| !m.is_empty()) {
        env.push(("ANTHROPIC_MODEL".to_string(), m.to_string()));
        env.push(("ANTHROPIC_SMALL_FAST_MODEL".to_string(), m.to_string()));
    }
    env
}

fn tool_command_name(tool: &str) -> &'static str {
    match tool {
        "design" | "cd" => "design",
        _ => "claude",
    }
}

/// Launch the given CLI tool against this proxy, starting the proxy in the
/// background first if it is not already serving.
///
/// # Errors
///
/// Returns an error if the config cannot be loaded, the proxy cannot be
/// started, or the tool cannot be spawned.
#[allow(clippy::needless_pass_by_value)]
pub fn launch(
    config_path: PathBuf,
    tool: &str,
    model: Option<&str>,
    yes: bool,
    dry_run: bool,
    args: Vec<String>,
) -> miette::Result<()> {
    let config = load_config(&config_path)?;
    let host = config.server.host.clone();
    let port = config.server.port;
    let tool_cmd = tool_command_name(tool);

    if !is_serving(&host, port) {
        let bg = spawn_background(&[
            "serve".to_string(),
            "--config".to_string(),
            config_path.display().to_string(),
        ])
        .map_err(CliError::from)?;
        println!("proxy started in background (pid {})", bg.id());
        for _ in 0..50 {
            if is_serving(&host, port) {
                break;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    let env = launch_environment(&config, model);

    if dry_run {
        for (k, v) in &env {
            println!("{k}={v}");
        }
        let mut cmdline = String::from(tool_cmd);
        if yes {
            cmdline.push_str(" --yes");
        }
        if !args.is_empty() {
            cmdline.push(' ');
            cmdline.push_str(&args.join(" "));
        }
        println!("command: {cmdline}");
        return Ok(());
    }

    let mut cmd = Command::new(tool_cmd);
    for (k, v) in &env {
        cmd.env(k, v);
    }
    if yes {
        cmd.arg("--yes");
    }
    cmd.args(&args);
    let status = cmd.status().map_err(|e| CliError::Tool {
        message: format!("failed to spawn '{tool_cmd}' (is it installed and on PATH?): {e}"),
    })?;
    std::process::exit(status.code().unwrap_or(1));
}

// ---------------------------------------------------------------------------
// status / stop / models
// ---------------------------------------------------------------------------

/// Print whether the proxy is currently running.
///
/// # Errors
///
/// Returns an error if the config cannot be loaded.
#[allow(clippy::needless_pass_by_value)]
pub fn status(config_path: PathBuf) -> miette::Result<()> {
    let config = load_config(&config_path)?;
    let running = is_serving(&config.server.host, config.server.port);
    match read_pid() {
        Some(pid) if running => println!(
            "running (pid {pid}) at http://{}:{}",
            config.server.host, config.server.port
        ),
        Some(pid) => println!("pid file says {pid}, but not reachable"),
        None if running => println!("reachable but no pid file"),
        None => println!("not running"),
    }
    Ok(())
}

/// Stop the running proxy, killing the recorded process.
///
/// # Errors
///
/// Returns an error only if the config must be loaded to check reachability
/// and that load fails.
#[allow(clippy::needless_pass_by_value)]
pub fn stop(config_path: PathBuf) -> miette::Result<()> {
    if let Some(pid) = read_pid() {
        stop_process(pid);
        remove_pid();
        println!("stopped (pid {pid})");
    } else {
        let config = load_config(&config_path)?;
        if is_serving(&config.server.host, config.server.port) {
            println!("proxy is reachable but no pid file was found; not stopped");
        } else {
            println!("not running");
        }
    }
    Ok(())
}

/// Print the list of models the proxy can route to.
///
/// Models available from providers that have a resolvable key (are "connected"),
/// in provider config order with duplicates removed.
///
/// # Errors
///
/// Returns a [`CliError`] if the catalog or config cannot be loaded.
#[allow(clippy::result_large_err)]
pub fn connected_models(config_path: &Path) -> Result<Vec<String>, CliError> {
    let config = effective_config(config_path)?;
    let auth = crate::auth::read_auth().unwrap_or_default();
    let mut models = Vec::new();
    for provider in &config.providers {
        if !crate::upstream::provider_has_key(
            provider,
            auth.get(&provider.name).map(|e| e.key.as_str()),
        ) {
            continue;
        }
        for model in &provider.models {
            if !models.contains(model) {
                models.push(model.clone());
            }
        }
    }
    Ok(models)
}

/// The first model available from a connected provider, or `None` if no
/// provider has a resolvable key. Used as the default model when the user has
/// not explicitly selected one.
///
/// # Errors
///
/// Returns a [`CliError`] if the catalog or config cannot be loaded.
#[allow(clippy::result_large_err)]
pub fn first_available_model(config_path: &Path) -> Result<Option<String>, CliError> {
    Ok(connected_models(config_path)?.into_iter().next())
}

/// List the models available from connected providers, deduplicated and in
/// provider config order.
///
/// Returns an empty `Vec` when no provider has a resolvable key. This is the
/// core list that both the CLI and MCP use.
///
/// # Errors
///
/// Returns a [`CliError`] if the catalog or config cannot be loaded.
#[allow(clippy::result_large_err)]
pub fn models_list(config_path: &Path) -> Result<Vec<String>, CliError> {
    connected_models(config_path)
}

/// Print the list of models available from connected providers, exiting with a
/// non-zero status if none are connected.
///
/// # Errors
///
/// Returns a [`CliError`] if the catalog or config cannot be loaded.
#[allow(clippy::needless_pass_by_value)]
pub fn models(config_path: PathBuf) -> miette::Result<()> {
    let connected = models_list(&config_path).map_err(miette::Report::from)?;
    if connected.is_empty() {
        println!("no providers are connected");
        std::process::exit(1);
    }
    for m in connected {
        println!("{m}");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// connect / disconnect / providers
// ---------------------------------------------------------------------------

/// Build the effective config for `config_path` by merging the embedded catalog
/// with the user's config overlay.
#[allow(clippy::result_large_err)]
fn effective_config(config_path: &Path) -> Result<Config, CliError> {
    let overlay = if config_path.exists() {
        Config::load(config_path).map_err(CliError::from)?
    } else {
        Config::default()
    };
    let base = crate::catalog::load().map_err(CliError::from)?;
    Ok(crate::catalog::effective_config(base, overlay))
}

/// Validate that `provider` exists in the effective provider set (catalog or
/// config), storing its API key in `auth.json`. Prompts hidden if no key given.
/// Returns the success message.
///
/// # Errors
///
/// Returns a [`CliError`] if the provider is unknown, the prompt fails, or the
/// auth store cannot be written.
#[allow(clippy::result_large_err)]
pub fn connect_provider(
    config_path: &Path,
    provider: &str,
    key: Option<String>,
) -> Result<String, CliError> {
    let effective = effective_config(config_path)?;
    if !effective.providers.iter().any(|p| p.name == provider) {
        return Err(CliError::Connect {
            message: format!(
                "provider '{provider}' nao existe no catalogo nem no config; \
                 use `local-proxy providers` ou adicione-o no config"
            ),
        });
    }
    let key =
        match key.filter(|k| !k.trim().is_empty()) {
            Some(k) => k,
            None => rpassword::prompt_password(format!("chave do provider {provider}: ")).map_err(
                |e| CliError::Connect {
                    message: format!("falha ao ler a chave: {e}"),
                },
            )?,
        };
    crate::auth::set_key(provider, key.trim()).map_err(CliError::from)?;
    Ok(format!("chave do provider '{provider}' salva em auth.json"))
}

/// Remove the stored API key for `provider` from `auth.json`. Returns the
/// result message.
///
/// # Errors
///
/// Returns [`CliError::Auth`] if the auth store cannot be written.
#[allow(clippy::result_large_err)]
pub fn disconnect_provider(config_path: &Path, provider: &str) -> Result<String, CliError> {
    let _ = config_path;
    let removed = crate::auth::remove_key(provider).map_err(CliError::from)?;
    Ok(if removed {
        format!("chave do provider '{provider}' removida")
    } else {
        format!("nenhuma chave salva para o provider '{provider}'")
    })
}

/// Render the effective provider list (catalog ∪ config) with key status.
///
/// # Errors
///
/// Returns a [`CliError`] if the catalog or config cannot be loaded.
#[allow(clippy::result_large_err)]
#[allow(clippy::format_push_string)]
pub fn list_providers(config_path: &Path) -> Result<String, CliError> {
    let effective = effective_config(config_path)?;
    let auth = crate::auth::read_auth().unwrap_or_default();
    let mut out = String::new();
    for p in &effective.providers {
        let has_key = auth.contains_key(&p.name);
        let status = if has_key { "ok" } else { "-" };
        out.push_str(&format!(
            "{:<16} format={:<10} key={status}\n",
            p.name, p.format
        ));
    }
    Ok(out.trim_end().to_string())
}

/// Persist `defaults.active_model` in the config overlay so the model selection
/// survives restarts. `None` clears the selection. Creates a default config
/// file if none exists yet.
///
/// # Errors
///
/// Returns a [`CliError`] if the config cannot be read, serialized, or written.
#[allow(clippy::result_large_err)]
pub fn set_default_model(config_path: &Path, model: Option<&str>) -> Result<(), CliError> {
    if !config_path.exists() {
        if let Some(parent) = config_path.parent() {
            std::fs::create_dir_all(parent).map_err(CliError::from)?;
        }
        crate::config::create_default_config(config_path).map_err(CliError::from)?;
    }
    let mut config = Config::load(config_path).map_err(CliError::from)?;
    config.defaults.active_model = model.map(str::to_string);
    let yaml = serde_yaml::to_string(&config).map_err(|e| CliError::Connect {
        message: format!("falha ao serializar config: {e}"),
    })?;
    let tmp = config_path.with_extension("yaml.tmp");
    std::fs::write(&tmp, yaml).map_err(CliError::from)?;
    std::fs::rename(&tmp, config_path).map_err(CliError::from)?;
    Ok(())
}

/// Compute the message for `local-proxy model`: get, set, or clear the active
/// model, returning the message the CLI prints and that MCP returns verbatim.
///
/// With no `model`, returns the effective active model (the selected one, else
/// the first model available from a connected provider, else "none"). With a
/// model name, validates it is available from a connected provider and persists
/// it as the active model. `clear` clears the selection. When no provider is
/// connected and a model is requested, returns `"no providers are connected"`.
///
/// # Errors
///
/// Returns a [`CliError`] if the config cannot be loaded or written, or the
/// requested model is not available from a connected provider.
#[allow(clippy::result_large_err)]
pub fn model_result(config_path: &Path, model: Option<&str>) -> Result<String, CliError> {
    match model {
        Some("clear") => {
            set_default_model(config_path, None)?;
            Ok("modelo ativo removido".to_string())
        }
        Some(selected) => {
            let connected = connected_models(config_path)?;
            if connected.is_empty() {
                return Ok("no providers are connected".to_string());
            }
            if !connected.iter().any(|m| m == selected) {
                return Err(CliError::Connect {
                    message: format!(
                        "model '{selected}' nao disponivel em um provider conectado; \
                         disponiveis: {}",
                        connected.join(", ")
                    ),
                });
            }
            set_default_model(config_path, Some(selected))?;
            Ok(format!("modelo ativo: {selected}"))
        }
        None => {
            let config = effective_config(config_path)?;
            if let Some(m) = config.defaults.active_model {
                Ok(m)
            } else {
                Ok(first_available_model(config_path)?.map_or_else(
                    || "none".to_string(),
                    |m| format!("{m} (nenhum selecionado; usando o primeiro disponivel)"),
                ))
            }
        }
    }
}

/// CLI entry for `model`: print the result of [`model_result`].
///
/// # Errors
///
/// Returns a [`CliError`] if the config cannot be loaded or written, or the
/// requested model is not available from a connected provider.
#[allow(clippy::needless_pass_by_value)]
pub fn model(config_path: PathBuf, model: Option<String>) -> miette::Result<()> {
    let msg = model_result(&config_path, model.as_deref()).map_err(miette::Report::from)?;
    println!("{msg}");
    Ok(())
}

/// CLI entry for `connect`: store the API key for an existing provider.
///
/// # Errors
///
/// Returns a [`CliError`] if the provider is unknown or the auth store fails.
#[allow(clippy::needless_pass_by_value)]
pub fn connect(config_path: PathBuf, provider: String, key: Option<String>) -> miette::Result<()> {
    let msg = connect_provider(&config_path, &provider, key).map_err(miette::Report::from)?;
    println!("{msg}");
    Ok(())
}

/// CLI entry for `init`: register the local-proxy MCP server into each
/// detected harness (opencode/claude) config file.
///
/// Detected harnesses are listed; unless `yes` is given, each one is confirmed
/// via an interactive prompt (defaulting to accept). Only the MCP server
/// registration is written — model selection and provider setup are untouched.
/// Existing configs are preserved (merged) and backed up to `<path>.bak`.
///
/// # Errors
///
/// Returns a [`CliError`] if a prompt fails or a harness config cannot be read,
/// merged, or written.
#[allow(clippy::needless_pass_by_value)]
pub fn init(config_path: PathBuf, yes: bool) -> miette::Result<()> {
    let _ = config_path;
    let detected = crate::harness::detect();
    println!("=== configurando MCP local-proxy ===");
    if detected.is_empty() {
        println!("nenhum harness detectado (opencode/claude)");
        return Ok(());
    }
    println!("harnesses detectados:");
    for h in &detected {
        println!("  - {}", h.name());
    }

    let mut accepted = Vec::new();
    for h in detected {
        let ok = if yes {
            true
        } else {
            dialoguer::Confirm::new()
                .with_prompt(format!(
                    "Configurar {} para usar o local-proxy via MCP?",
                    h.name()
                ))
                .default(true)
                .interact()
                .map_err(|e| CliError::Connect {
                    message: format!("falha ao ler a resposta: {e}"),
                })?
        };
        if ok {
            accepted.push(h);
        }
    }

    if accepted.is_empty() {
        println!("nenhum harness configurado");
        return Ok(());
    }

    for h in accepted {
        let path = h.config_path();
        let existing = if path.exists() {
            std::fs::read_to_string(&path).map_err(CliError::from)?
        } else {
            String::new()
        };
        let merged =
            crate::harness::merge_mcp_entry(&existing, h).map_err(|e| CliError::Connect {
                message: format!("falha ao mesclar o config do {}: {e}", h.name()),
            })?;
        let written = crate::harness::write_with_backup(&path, &merged).map_err(CliError::from)?;
        println!(
            "{} configurado: {} (backup: {}.bak — para desfazer, restaure o backup)",
            h.name(),
            written.display(),
            written.display()
        );
    }
    Ok(())
}

/// CLI entry for `disconnect`: remove the stored API key for a provider.
///
/// # Errors
///
/// Returns a [`CliError`] if the auth store cannot be written.
#[allow(clippy::needless_pass_by_value)]
pub fn disconnect(config_path: PathBuf, provider: String) -> miette::Result<()> {
    let msg = disconnect_provider(&config_path, &provider).map_err(miette::Report::from)?;
    println!("{msg}");
    Ok(())
}

/// CLI entry for `providers`: print the effective provider list.
///
/// # Errors
///
/// Returns a [`CliError`] if the catalog or config cannot be loaded.
#[allow(clippy::needless_pass_by_value)]
pub fn providers(config_path: PathBuf) -> miette::Result<()> {
    let out = list_providers(&config_path).map_err(miette::Report::from)?;
    println!("{out}");
    Ok(())
}

/// The recognized `--since` windows for `local-proxy stats`, in seconds.
fn window_seconds(kind: &str) -> Option<i64> {
    match kind {
        "day" => Some(86_400),
        "week" => Some(7 * 86_400),
        "month" => Some(30 * 86_400),
        _ => None,
    }
}

/// Print aggregate usage statistics collected from upstream requests.
///
/// Renders a human summary (with a per-provider breakdown) by default, or the
/// same data as JSON when `json` is set. When no stats have been recorded yet,
/// prints a message and returns without error.
///
/// # Errors
///
/// Returns [`CliError::Stats`] if the stats database cannot be read.
#[allow(clippy::needless_pass_by_value, clippy::cast_possible_wrap)]
pub fn stats(config_path: PathBuf, since: String, json: bool) -> miette::Result<()> {
    let _ = config_path;
    // `window_seconds` returning `None` is only reachable for `all`, which the
    // clap value parser guarantees is the sentinel for "no time filter".
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    let window = crate::stats::TimeWindow {
        since: window_seconds(&since).map(|s| now - s),
    };

    let summary = crate::stats::summary(window).map_err(CliError::from)?;
    let by_provider = crate::stats::by_provider(window).map_err(CliError::from)?;
    let recent = crate::stats::recent(window, 10).map_err(CliError::from)?;

    let Some(summary) = summary else {
        println!("nenhuma estatística registrada ainda (a primeira requisição proxy cria o banco)");
        return Ok(());
    };

    let by_provider = by_provider.unwrap_or_default();
    let recent = recent.unwrap_or_default();
    if json {
        render_stats_json(&summary, &by_provider, &recent);
    } else {
        render_stats_text(&summary, &by_provider, &recent);
    }
    Ok(())
}

/// Render the human-readable stats report.
#[allow(clippy::format_push_string, clippy::cast_precision_loss)]
fn render_stats_text(
    summary: &crate::stats::RowSummary,
    by_provider: &[crate::stats::ProviderStats],
    recent: &[crate::stats::RequestRow],
) {
    println!("=== stats local-proxy ===");
    let total_latency = summary.latency_ms as f64 / 1000.0;
    let error_rate = if summary.requests == 0 {
        0.0
    } else {
        summary.errors as f64 / summary.requests as f64 * 100.0
    };
    println!(
        "requisições: {}  |  in: {}  out: {} tokens  |  latency: {total_latency:.1}s  |  erros: {:.1}%",
        summary.requests,
        summary.input_tokens,
        summary.output_tokens,
        error_rate
    );
    let energy_kwh = summary.energy_kwh_um as f64 / 1_000_000.0;
    let cost_usd = summary.cost_usd_um as f64 / 1_000_000.0;
    if energy_kwh > 0.0 || cost_usd > 0.0 {
        println!("energia: {energy_kwh:.6} kWh  |  custo: ${cost_usd:.6}");
    }
    println!("--- por provider ---");
    if by_provider.is_empty() {
        println!("(nenhum)");
    }
    for p in by_provider {
        let ekwh = p.energy_kwh_um as f64 / 1_000_000.0;
        let cusd = p.cost_usd_um as f64 / 1_000_000.0;
        if ekwh > 0.0 || cusd > 0.0 {
            println!(
                "{:<16} reqs={:<5} in={} out={} lat={}ms energia={ekwh:.6}kWh custo=${cusd:.6}",
                p.provider, p.requests, p.input_tokens, p.output_tokens, p.latency_ms
            );
        } else {
            println!(
                "{:<16} reqs={:<5} in={} out={} lat={}ms",
                p.provider, p.requests, p.input_tokens, p.output_tokens, p.latency_ms
            );
        }
    }
    println!("--- recentes ---");
    if recent.is_empty() {
        println!("(nenhum)");
    }
    for r in recent {
        let stream = if r.streamed { "SSE " } else { "    " };
        let err = if r.error { " ERROR" } else { "" };
        let mut extra = String::new();
        if let Some(e) = r.energy_kwh_um {
            extra.push_str(&format!(" {:.3e}kWh", e as f64 / 1_000_000.0));
        }
        if let Some(c) = r.cost_usd_um {
            extra.push_str(&format!(" ${:.3e}", c as f64 / 1_000_000.0));
        }
        println!(
            "{:<12} {stream} {:>3} {:<4} {:<10}{extra}{err}",
            r.endpoint, r.status, r.latency_ms, r.provider
        );
    }
}

/// Render the stats report as JSON.
#[allow(clippy::format_push_string, clippy::cast_precision_loss)]
fn render_stats_json(
    summary: &crate::stats::RowSummary,
    by_provider: &[crate::stats::ProviderStats],
    recent: &[crate::stats::RequestRow],
) {
    let summary_json = serde_json::json!({
        "requests": summary.requests,
        "input_tokens": summary.input_tokens,
        "output_tokens": summary.output_tokens,
        "total_latency_ms": summary.latency_ms,
        "errors": summary.errors,
        "energy_kwh": summary.energy_kwh_um as f64 / 1_000_000.0,
        "cost_usd": summary.cost_usd_um as f64 / 1_000_000.0,
    });
    let providers_json: Vec<serde_json::Value> = by_provider
        .iter()
        .map(|p| {
            serde_json::json!({
                "provider": p.provider,
                "requests": p.requests,
                "input_tokens": p.input_tokens,
                "output_tokens": p.output_tokens,
                "total_latency_ms": p.latency_ms,
                "energy_kwh": p.energy_kwh_um as f64 / 1_000_000.0,
                "cost_usd": p.cost_usd_um as f64 / 1_000_000.0,
            })
        })
        .collect();
    let recent_json: Vec<serde_json::Value> = recent
        .iter()
        .map(|r| {
            let mut v = serde_json::json!({
                "ts": r.ts,
                "endpoint": r.endpoint,
                "provider": r.provider,
                "model": r.model,
                "input_tokens": r.input_tokens,
                "output_tokens": r.output_tokens,
                "streamed": r.streamed,
                "status": r.status,
                "latency_ms": r.latency_ms,
                "error": r.error,
            });
            if let Some(e) = r.energy_kwh_um {
                v["energy_kwh_um"] = serde_json::json!(e);
            }
            if let Some(c) = r.cost_usd_um {
                v["cost_usd_um"] = serde_json::json!(c);
            }
            v
        })
        .collect();
    let out = serde_json::json!({
        "summary": summary_json,
        "providers": providers_json,
        "recent": recent_json,
    });
    println!("{out}");
}

/// Run the MCP stdio server exposing connect/disconnect/models/providers.
///
/// # Errors
///
/// Returns a [`CliError`] if the MCP server cannot be started.
#[allow(clippy::needless_pass_by_value)]
pub async fn mcp(config_path: PathBuf) -> miette::Result<()> {
    crate::mcp::run(config_path).await
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
    if let Some(path) = Config::env_config_path() {
        return path;
    }
    for name in [DEFAULT_CONFIG_PATH, "config.json"] {
        let candidate = PathBuf::from(name);
        if candidate.exists() {
            return std::env::current_dir().map_or(candidate, |dir| dir.join(name));
        }
    }
    crate::config::global_config_path()
}

// ---------------------------------------------------------------------------
// update (self-update from GitHub Releases)
// ---------------------------------------------------------------------------

/// GitHub repository used when no override is given via `--repo` or
/// `LOCAL_PROXY_REPO`.
const DEFAULT_REPO: &str = "gsporto226/local-proxy";

/// Environment variable that overrides the GitHub repository for updates.
const UPDATE_ENV_REPO: &str = "LOCAL_PROXY_REPO";

/// Version of this binary, taken from the crate manifest.
const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(target_os = "windows")]
const CURRENT_OS: &str = "windows";
#[cfg(target_os = "linux")]
const CURRENT_OS: &str = "linux";
#[cfg(target_os = "macos")]
const CURRENT_OS: &str = "darwin";
#[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
const CURRENT_OS: &str = "unknown";

#[cfg(target_arch = "x86_64")]
const CURRENT_ARCH: &str = "x86_64";
#[cfg(target_arch = "aarch64")]
const CURRENT_ARCH: &str = "aarch64";
#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
const CURRENT_ARCH: &str = "unknown";

/// A GitHub release listing (the fields `update` needs).
#[derive(Debug, Deserialize)]
struct Release {
    /// The release tag, e.g. `v1.2.3`.
    tag_name: String,
    /// Assets attached to the release.
    assets: Vec<ReleaseAsset>,
}

/// A single release asset.
#[derive(Debug, Deserialize)]
struct ReleaseAsset {
    /// Asset file name, e.g. `local-proxy.exe`.
    name: String,
    /// Direct download URL for the asset.
    browser_download_url: String,
}

/// Errors that can occur while self-updating.
#[derive(Debug, Error, Diagnostic)]
pub enum UpdateError {
    /// No prebuilt binary is published for the current platform.
    #[error("no prebuilt binary is published for this platform (os={os}, arch={arch})")]
    #[diagnostic(
        code(update::unsupported),
        help("local-proxy publishes x86_64 builds for Linux and Windows")
    )]
    Unsupported {
        /// Detected operating system.
        os: String,
        /// Detected CPU architecture.
        arch: String,
    },
    /// The GitHub Releases API call failed.
    #[error("failed to fetch release info from {repo}: {source}")]
    #[diagnostic(code(update::fetch))]
    Fetch {
        /// Repository queried.
        repo: String,
        /// Underlying HTTP error.
        #[source]
        source: reqwest::Error,
    },
    /// The requested binary is not among the release assets.
    #[error("binary '{bin}' not found in release {tag}")]
    #[diagnostic(code(update::no_asset))]
    NoAsset {
        /// Expected asset name.
        bin: String,
        /// Release tag that was inspected.
        tag: String,
    },
    /// The downloaded binary failed SHA256 verification.
    #[error("SHA256 mismatch for {path}: expected {expected}, got {actual}")]
    #[diagnostic(code(update::verify), help("retry the download or use --no-verify"))]
    Verify {
        /// Path of the staged binary.
        path: String,
        /// Expected digest from the release.
        expected: String,
        /// Actual computed digest.
        actual: String,
    },
    /// The binary could not be downloaded.
    #[error("failed to download {url}: {source}")]
    #[diagnostic(code(update::download))]
    Download {
        /// URL being downloaded.
        url: String,
        /// Underlying HTTP error.
        #[source]
        source: reqwest::Error,
    },
    /// The staged binary could not be written to disk.
    #[error("failed to stage update at {path}: {source}")]
    #[diagnostic(code(update::stage))]
    Stage {
        /// Path that could not be written.
        path: String,
        /// Underlying I/O error.
        #[source]
        source: io::Error,
    },
    /// The running binary could not be replaced within the retry window.
    #[error("could not replace the running binary {exe} with {path}")]
    #[diagnostic(
        code(update::replace),
        help("stop any running local-proxy process, then run: {command}")
    )]
    Replace {
        /// Path of the staged binary.
        path: String,
        /// Path of the running executable.
        exe: String,
        /// Manual command that completes the replacement.
        command: String,
        /// Underlying I/O error.
        #[source]
        source: io::Error,
    },
}

/// The asset file name for a published platform, or `None` if no prebuilt
/// binary is released for it.
#[must_use]
pub fn asset_name(os: &str, arch: &str) -> Option<String> {
    if arch != "x86_64" {
        return None;
    }
    match os {
        "windows" => Some("local-proxy.exe".to_string()),
        "linux" => Some("local-proxy".to_string()),
        _ => None,
    }
}

/// Parse a semantic version tag (`v1.2.3`) into a comparable `(major, minor,
/// patch)` tuple, ignoring any pre-release/build suffix. Returns `None` on
/// malformed input.
#[must_use]
pub fn parse_version(tag: &str) -> Option<(u32, u32, u32)> {
    let v = tag.strip_prefix('v').unwrap_or(tag);
    let mut parts = v.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts
        .next()?
        .split(|c: char| !c.is_ascii_digit())
        .next()
        .and_then(|s| s.parse().ok())?;
    Some((major, minor, patch))
}

/// Whether version `a` is strictly newer than version `b`.
#[must_use]
pub fn is_newer(a: &str, b: &str) -> bool {
    match (parse_version(a), parse_version(b)) {
        (Some(x), Some(y)) => x > y,
        _ => false,
    }
}

/// The GitHub repository to query for updates, from `--repo`, the
/// `LOCAL_PROXY_REPO` env var, or the default.
fn resolve_repo(flag: Option<String>) -> String {
    flag.or_else(|| {
        std::env::var(UPDATE_ENV_REPO)
            .ok()
            .filter(|s| !s.is_empty())
    })
    .unwrap_or_else(|| DEFAULT_REPO.to_string())
}

/// Fetch the latest release metadata for `repo` from the GitHub Releases API.
async fn fetch_release(client: &reqwest::Client, repo: &str) -> Result<Release, UpdateError> {
    let api_url = format!("https://api.github.com/repos/{repo}/releases/latest");
    client
        .get(&api_url)
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", "local-proxy-updater")
        .send()
        .await
        .map_err(|source| UpdateError::Fetch {
            repo: repo.to_string(),
            source,
        })?
        .error_for_status()
        .map_err(|source| UpdateError::Fetch {
            repo: repo.to_string(),
            source,
        })?
        .json()
        .await
        .map_err(|source| UpdateError::Fetch {
            repo: repo.to_string(),
            source,
        })
}

/// Download the binary at `asset_url` and, unless `no_verify`, check its SHA256
/// against the sibling `.sha256` file.
async fn download_and_verify(
    client: &reqwest::Client,
    asset_url: &str,
    staged: &Path,
    no_verify: bool,
) -> Result<Vec<u8>, UpdateError> {
    let body = client
        .get(asset_url)
        .send()
        .await
        .map_err(|source| UpdateError::Download {
            url: asset_url.to_string(),
            source,
        })?
        .error_for_status()
        .map_err(|source| UpdateError::Download {
            url: asset_url.to_string(),
            source,
        })?
        .bytes()
        .await
        .map_err(|source| UpdateError::Download {
            url: asset_url.to_string(),
            source,
        })?;

    if no_verify {
        println!("> verificação SHA256 pulada");
        return Ok(body.to_vec());
    }

    let sha_url = format!("{asset_url}.sha256");
    let sha_text = client
        .get(&sha_url)
        .send()
        .await
        .map_err(|source| UpdateError::Download {
            url: sha_url.clone(),
            source,
        })?
        .error_for_status()
        .map_err(|source| UpdateError::Download {
            url: sha_url.clone(),
            source,
        })?
        .text()
        .await
        .map_err(|source| UpdateError::Download {
            url: sha_url.clone(),
            source,
        })?;
    let expected = sha_text
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_lowercase();
    let mut hasher = Sha256::new();
    hasher.update(&body);
    let actual = format!("{:x}", hasher.finalize());
    if expected.is_empty() || actual != expected {
        return Err(UpdateError::Verify {
            path: staged.display().to_string(),
            expected,
            actual,
        });
    }
    println!("> SHA256 OK ({actual})");
    Ok(body.to_vec())
}

/// Path of the staged (downloaded) binary, in the same directory as `exe` so
/// the final swap is a same-filesystem rename. The `.exe` suffix on Windows is
/// required for the detached helper to be launched.
fn staged_path(exe: &Path, pid: u32) -> PathBuf {
    let dir = exe.parent().unwrap_or_else(|| Path::new("."));
    let stem = exe
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("local-proxy");
    #[cfg(target_os = "windows")]
    {
        dir.join(format!("{stem}.new.{pid}.exe"))
    }
    #[cfg(not(target_os = "windows"))]
    {
        dir.join(format!("{stem}.new.{pid}"))
    }
}

/// Manual command that completes the binary swap (used as a fallback when the
/// binary swap cannot be applied).
fn manual_replace_command(staged: &Path, exe: &Path) -> String {
    if CURRENT_OS == "windows" {
        format!("move /y \"{}\" \"{}\"", staged.display(), exe.display())
    } else {
        format!("mv \"{}\" \"{}\"", staged.display(), exe.display())
    }
}

/// How the running binary was installed, which determines how updates apply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InstallMethod {
    /// Installed by the install script into `~/.local/bin` (or `%USERPROFILE%\.local\bin`).
    Standalone,
    /// Installed via cargo into the cargo bin dir (`$CARGO_HOME/bin` or `~/.cargo/bin`).
    Cargo,
    /// Installed at any other custom location.
    Custom,
}

/// Detect how the running binary was installed, so the update can choose the
/// right way to apply itself (in-place swap for standalone/custom, delegation
/// to cargo for cargo installs).
#[must_use]
fn install_method(exe: &Path) -> InstallMethod {
    let home = directories::BaseDirs::new().map(|d| d.home_dir().to_path_buf());
    let local_bin = home.as_ref().map(|h| h.join(".local").join("bin"));
    let cargo_bin = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| home.as_ref().map(|h| h.join(".cargo")))
        .map(|p| p.join("bin"));
    if local_bin.as_deref().is_some_and(|d| exe.starts_with(d)) {
        InstallMethod::Standalone
    } else if cargo_bin.as_deref().is_some_and(|d| exe.starts_with(d)) {
        InstallMethod::Cargo
    } else {
        InstallMethod::Custom
    }
}

/// Atomically swap the staged binary into the running executable's path.
///
/// On Unix, renaming over a running executable is allowed: the old process
/// keeps its inode and the path atomically becomes the new binary. On Windows
/// the running image cannot be overwritten, so the current executable is first
/// renamed aside to a `.old` sibling (renames are allowed) and the new binary
/// is moved into place; the stale `.old` is then deleted by a detached helper
/// and cleaned up again on the next startup.
fn swap_binary(staged: &Path, exe: &Path) -> Result<(), UpdateError> {
    #[cfg(not(target_os = "windows"))]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::rename(staged, exe).map_err(|source| UpdateError::Replace {
            path: staged.display().to_string(),
            exe: exe.display().to_string(),
            command: manual_replace_command(staged, exe),
            source,
        })?;
        let _ = std::fs::set_permissions(exe, std::fs::Permissions::from_mode(0o755));
        Ok(())
    }
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let old = exe.with_extension("old");
        std::fs::rename(exe, &old).map_err(|source| UpdateError::Replace {
            path: staged.display().to_string(),
            exe: exe.display().to_string(),
            command: manual_replace_command(staged, exe),
            source,
        })?;
        std::fs::rename(staged, exe).map_err(|source| UpdateError::Replace {
            path: staged.display().to_string(),
            exe: exe.display().to_string(),
            command: manual_replace_command(staged, exe),
            source,
        })?;
        let script = format!(
            "timeout /t 2 /nobreak >nul & del /f /q \"{}\"",
            old.display()
        );
        let mut cmd = Command::new("cmd");
        cmd.args(["/c", &script]);
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NO_WINDOW);
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let _ = cmd.spawn();
        Ok(())
    }
}

/// Remove stale `<stem>*.old` backup files left next to the executable by a
/// Windows update swap (best-effort; called at server startup as a safety net).
fn cleanup_stale_backups(exe: &Path) {
    let Some(dir) = exe.parent().map(Path::to_path_buf) else {
        return;
    };
    let stem = exe
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("local-proxy");
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with(stem) && name.ends_with(".old") {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
}

/// The latest published version tag if it is newer than the running one, or
/// `None` if already up to date or the release cannot be fetched.
async fn latest_available_version() -> Option<String> {
    let repo = resolve_repo(None);
    let client = reqwest::Client::new();
    match fetch_release(&client, &repo).await {
        Ok(r) if is_newer(&r.tag_name, CURRENT_VERSION) => Some(r.tag_name),
        _ => None,
    }
}

/// Check for a newer release and, unless `check`, download and apply it.
/// Applies in place for standalone/custom installs, delegates to cargo for
/// cargo installs, and never blocks on the running process.
///
/// # Errors
///
/// Returns [`CliError::Update`] if the release cannot be fetched, verified, or
/// staged.
pub async fn update(
    repo_flag: Option<String>,
    check: bool,
    force: bool,
    no_verify: bool,
) -> miette::Result<()> {
    let repo = resolve_repo(repo_flag);
    let bin = asset_name(CURRENT_OS, CURRENT_ARCH).ok_or_else(|| UpdateError::Unsupported {
        os: CURRENT_OS.to_string(),
        arch: CURRENT_ARCH.to_string(),
    })?;
    let client = reqwest::Client::new();
    let release = fetch_release(&client, &repo).await?;

    let latest = release.tag_name.clone();
    if check {
        if is_newer(&latest, CURRENT_VERSION) {
            println!("versão mais recente disponível: {latest} (atual: v{CURRENT_VERSION})");
        } else {
            println!("já está na versão mais recente: v{CURRENT_VERSION}");
        }
        return Ok(());
    }
    if !force && !is_newer(&latest, CURRENT_VERSION) {
        println!("já está na versão mais recente (v{CURRENT_VERSION})");
        return Ok(());
    }

    let asset = release
        .assets
        .iter()
        .find(|a| a.name == bin)
        .ok_or_else(|| UpdateError::NoAsset {
            bin: bin.clone(),
            tag: latest.clone(),
        })?;
    let asset_url = asset.browser_download_url.clone();

    let exe = std::env::current_exe().map_err(|source| UpdateError::Stage {
        path: bin.clone(),
        source,
    })?;

    if install_method(&exe) == InstallMethod::Cargo {
        println!("instalado via cargo — atualize pelo cargo:");
        println!("  cargo install --force local-proxy");
        println!("(com cargo-update:  cargo install-update local-proxy)");
        return Ok(());
    }

    let staged = staged_path(&exe, std::process::id());

    let body = download_and_verify(&client, &asset_url, &staged, no_verify).await?;
    std::fs::write(&staged, body).map_err(|source| UpdateError::Stage {
        path: staged.display().to_string(),
        source,
    })?;
    #[cfg(not(target_os = "windows"))]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755));
    }

    match swap_binary(&staged, &exe) {
        Ok(()) => {
            println!("atualizado para {latest} (de v{CURRENT_VERSION})");
            #[cfg(target_os = "windows")]
            println!("a troca será concluída ao sair; reinicie o proxy para usar a nova versão.");
            Ok(())
        }
        Err(err) => {
            println!("o binário novo está em: {}", staged.display());
            Err(err.into())
        }
    }
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Defaults, Server};

    fn config_with(keys: Vec<String>) -> Config {
        Config {
            server: Server {
                host: "127.0.0.1".to_string(),
                port: 8787,
                api_keys: keys,
                passthrough_keys: false,
            },
            providers: Vec::new(),
            routes: Vec::new(),
            defaults: Defaults::default(),
        }
    }

    #[test]
    fn launch_env_uses_configured_key_and_model() {
        let cfg = config_with(vec!["sk-proxy".to_string()]);
        let env = launch_environment(&cfg, Some("kimi-k2.6"));
        let map: std::collections::HashMap<_, _> = env.into_iter().collect();
        assert_eq!(map["ANTHROPIC_BASE_URL"], "http://127.0.0.1:8787");
        assert_eq!(map["ANTHROPIC_API_KEY"], "sk-proxy");
        assert_eq!(map["ANTHROPIC_AUTH_TOKEN"], "sk-proxy");
        assert_eq!(map["ANTHROPIC_MODEL"], "kimi-k2.6");
        assert_eq!(map["ANTHROPIC_SMALL_FAST_MODEL"], "kimi-k2.6");
    }

    #[test]
    fn launch_env_without_keys_uses_unused_and_no_model() {
        let cfg = config_with(Vec::new());
        let env = launch_environment(&cfg, None);
        let map: std::collections::HashMap<_, _> = env.into_iter().collect();
        assert_eq!(map["ANTHROPIC_API_KEY"], "unused");
        assert_eq!(map["ANTHROPIC_AUTH_TOKEN"], "unused");
        assert!(!map.contains_key("ANTHROPIC_MODEL"));
    }

    #[test]
    fn tool_command_maps_aliases() {
        assert_eq!(tool_command_name("claude"), "claude");
        assert_eq!(tool_command_name("cc"), "claude");
        assert_eq!(tool_command_name("design"), "design");
        assert_eq!(tool_command_name("cd"), "design");
    }

    #[test]
    fn runtime_paths_live_under_config_dir() {
        assert!(pid_file().to_string_lossy().contains("local-proxy"));
        assert!(log_file().to_string_lossy().contains("local-proxy"));
    }

    #[test]
    fn config_dir_is_global() {
        assert_eq!(config_dir(), crate::config::global_config_dir());
    }

    #[test]
    fn set_default_model_persists_and_clears() {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock is set")
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("local-proxy-select-{}-{stamp}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create test dir");
        let path = dir.join("config.yaml");

        set_default_model(&path, Some("deepseek-v4-flash")).expect("persist model");
        let config = Config::load(&path).expect("reload");
        assert_eq!(
            config.defaults.active_model.as_deref(),
            Some("deepseek-v4-flash")
        );

        set_default_model(&path, None).expect("clear model");
        let config = Config::load(&path).expect("reload");
        assert_eq!(config.defaults.active_model, None);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_config_path_uses_cwd_yaml_when_present() {
        let _guard = crate::TEST_STATE_LOCK.lock().unwrap();
        let prev = std::env::current_dir().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("config.yaml"), "server: {}\n").unwrap();
        std::env::remove_var("LOCAL_PROXY_CONFIG");
        std::env::set_current_dir(tmp.path()).unwrap();
        let result = resolve_config_path(None);
        std::env::set_current_dir(prev).unwrap();
        assert_eq!(result, tmp.path().join("config.yaml"));
    }

    #[test]
    fn resolve_config_path_defaults_to_global_when_no_cwd_file() {
        let _guard = crate::TEST_STATE_LOCK.lock().unwrap();
        let prev = std::env::current_dir().unwrap();
        std::env::remove_var("LOCAL_PROXY_CONFIG");
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_current_dir(tmp.path()).unwrap();
        let result = resolve_config_path(None);
        std::env::set_current_dir(prev).unwrap();
        assert_eq!(result, crate::config::global_config_path());
    }

    #[test]
    fn resolve_config_path_explicit_flag_wins() {
        let explicit = PathBuf::from("/explicit/custom.yaml");
        assert_eq!(resolve_config_path(Some(explicit.clone())), explicit);
    }

    #[test]
    fn asset_name_matches_published_platforms() {
        assert_eq!(
            asset_name("windows", "x86_64"),
            Some("local-proxy.exe".to_string())
        );
        assert_eq!(
            asset_name("linux", "x86_64"),
            Some("local-proxy".to_string())
        );
        assert_eq!(asset_name("darwin", "x86_64"), None);
        assert_eq!(asset_name("windows", "aarch64"), None);
        assert_eq!(asset_name("linux", "aarch64"), None);
    }

    #[test]
    fn install_method_detects_standalone_cargo_and_custom() {
        let home = directories::BaseDirs::new()
            .expect("base dirs")
            .home_dir()
            .to_path_buf();
        let standalone = home.join(".local").join("bin").join("local-proxy");
        assert_eq!(install_method(&standalone), InstallMethod::Standalone);

        let cargo_dir = std::env::var_os("CARGO_HOME")
            .map_or_else(|| home.join(".cargo"), PathBuf::from)
            .join("bin");
        assert_eq!(
            install_method(&cargo_dir.join("local-proxy")),
            InstallMethod::Cargo
        );

        let custom = Path::new("/opt/tools").join("local-proxy");
        assert_eq!(install_method(&custom), InstallMethod::Custom);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn staged_path_uses_pid_and_exe_suffix_on_windows() {
        let exe = Path::new("C:\\tools\\local-proxy.exe");
        assert_eq!(
            staged_path(exe, 1234),
            Path::new("C:\\tools\\local-proxy.new.1234.exe")
        );
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn staged_path_uses_pid_suffix_on_unix() {
        let exe = Path::new("/usr/local/bin/local-proxy");
        assert_eq!(
            staged_path(exe, 1234),
            Path::new("/usr/local/bin/local-proxy.new.1234")
        );
    }

    #[test]
    fn manual_command_matches_platform() {
        let staged = Path::new(if cfg!(target_os = "windows") {
            "C:\\tools\\local-proxy.new.1.exe"
        } else {
            "/tools/local-proxy.new.1"
        });
        let exe = Path::new(if cfg!(target_os = "windows") {
            "C:\\tools\\local-proxy.exe"
        } else {
            "/tools/local-proxy"
        });
        let command = manual_replace_command(staged, exe);
        if cfg!(target_os = "windows") {
            assert_eq!(
                command,
                "move /y \"C:\\tools\\local-proxy.new.1.exe\" \"C:\\tools\\local-proxy.exe\""
            );
        } else {
            assert_eq!(
                command,
                "mv \"/tools/local-proxy.new.1\" \"/tools/local-proxy\""
            );
        }
    }

    #[test]
    fn cleanup_stale_backups_removes_only_old_siblings() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("local-proxy.exe");
        std::fs::write(&exe, b"x").unwrap();
        std::fs::write(dir.path().join("local-proxy.old"), b"old").unwrap();
        std::fs::write(dir.path().join("local-proxy.new.1.exe"), b"new").unwrap();
        std::fs::write(dir.path().join("unrelated.txt"), b"keep").unwrap();

        cleanup_stale_backups(&exe);

        assert!(!dir.path().join("local-proxy.old").exists());
        assert!(dir.path().join("local-proxy.new.1.exe").exists());
        assert!(dir.path().join("unrelated.txt").exists());
    }

    #[test]
    fn parse_version_handles_v_prefix_and_suffix() {
        assert_eq!(parse_version("v1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse_version("1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse_version("v1.2.3-beta"), Some((1, 2, 3)));
        assert_eq!(parse_version("v1.2.3+build.7"), Some((1, 2, 3)));
        assert_eq!(parse_version("nope"), None);
        assert_eq!(parse_version("v1"), None);
    }

    #[test]
    fn is_newer_compares_semver_versions() {
        assert!(is_newer("v1.2.4", "v1.2.3"));
        assert!(is_newer("v2.0.0", "v1.9.9"));
        assert!(!is_newer("v1.2.3", "v1.2.3"));
        assert!(!is_newer("v1.2.2", "v1.2.3"));
        assert!(!is_newer("garbage", "v1.2.3"));
    }

    #[test]
    fn resolve_repo_prefers_flag_over_env_and_default() {
        let _guard = crate::TEST_STATE_LOCK.lock().unwrap();
        std::env::remove_var(UPDATE_ENV_REPO);
        assert_eq!(resolve_repo(None), DEFAULT_REPO);
        assert_eq!(resolve_repo(Some("other/repo".to_string())), "other/repo");
        std::env::set_var(UPDATE_ENV_REPO, "env/repo");
        assert_eq!(resolve_repo(None), "env/repo");
        assert_eq!(resolve_repo(Some("flag/repo".to_string())), "flag/repo");
        std::env::remove_var(UPDATE_ENV_REPO);
    }
}
