//! CLI adapter: `serve`, `launch`, process management, and self-update.
//!
//! The settings, stats, and compare commands are thin wrappers that print the
//! application use cases. Pure helpers are unit-testable; process spawns are
//! best-effort.

use std::io;
use std::net::{TcpListener, TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use miette::Diagnostic;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::adapters::inbound::{http, watcher};
use crate::adapters::outbound::events::LogTap;
use crate::adapters::outbound::paths::{self, log_file, pid_file};
use crate::application::runtime::{self, AppState};
use crate::application::settings::{self, SettingsError};
use crate::application::{compare as compare_app, stats_report};
use crate::domain::config::{Config, OAuthFlow, OAuthProvider};
use crate::domain::stats::{ProviderStats, RequestRow, RowSummary, StatsScope};
use crate::ports::{ClientHints, Ports};

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
    Config(#[from] crate::domain::config::ConfigError),

    /// A settings use case failed.
    #[error(transparent)]
    #[diagnostic(transparent)]
    Settings(#[from] SettingsError),

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

    /// A `connect` operation failed.
    #[error("{message}")]
    #[diagnostic(code(cli::connect))]
    Connect {
        /// Human-readable description of the failure.
        message: String,
    },

    /// The runtime state (config/router/clients) could not be built.
    #[error("failed to build runtime state")]
    #[diagnostic(
        code(cli::runtime),
        help("check the config file and the embedded catalog are valid")
    )]
    Runtime(#[from] crate::application::runtime::RuntimeError),

    /// The config watcher could not start.
    #[error(transparent)]
    #[diagnostic(transparent)]
    Watch(#[from] watcher::WatchError),

    /// The local usage statistics could not be read.
    #[error("failed to read usage statistics")]
    #[diagnostic(
        code(cli::stats),
        help("the stats database is created automatically on the first proxied request")
    )]
    Stats(#[from] crate::ports::StatsError),
}

// ---------------------------------------------------------------------------
// runtime files (global per-user config dir)
// ---------------------------------------------------------------------------

/// Write the given process ID to the pid file, creating the runtime dir if needed.
///
/// # Errors
///
/// Returns an error if the runtime directory cannot be created or the pid file
/// cannot be written.
pub fn write_pid(pid: u32) -> io::Result<()> {
    std::fs::create_dir_all(paths::config_dir())?;
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
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let _ = Command::new("taskkill")
            .creation_flags(CREATE_NO_WINDOW)
            .args(["/F", "/PID", &pid.to_string()])
            .status();
    }
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
    std::fs::create_dir_all(paths::config_dir())?;
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

/// Reserve an OS-assigned (ephemeral) available port for `host`.
///
/// Binds a listener to port `0`, reads the assigned port, and drops the
/// listener so the caller (or the spawned proxy) can bind it. There is a tiny
/// race between dropping and rebinding, which is acceptable for `launch`.
///
/// # Errors
///
/// Returns an error if the host cannot be bound to an ephemeral port.
pub fn pick_ephemeral_port(host: &str) -> io::Result<u16> {
    let listener = TcpListener::bind(format!("{host}:0"))?;
    let port = listener.local_addr()?.port();
    Ok(port)
}

/// Spawn this same binary as a non-detached child with the given args, keeping
/// the handle so the caller can kill it when the launched tool exits.
///
/// Unlike [`spawn_background`], the child is not detached: its stdout/stderr are
/// still mirrored to the log file, but the returned handle stays attached so the
/// proxy lifetime can be tied to a foreground tool.
///
/// # Errors
///
/// Returns an error if the current executable, runtime directory, or log file
/// cannot be set up, or if the process cannot be spawned.
pub fn spawn_launch_proxy(args: &[String]) -> io::Result<std::process::Child> {
    let exe = std::env::current_exe()?;
    std::fs::create_dir_all(paths::config_dir())?;
    let log = std::fs::File::create(log_file())?;
    let err = log.try_clone()?;

    let mut cmd = Command::new(exe);
    cmd.args(args)
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(err))
        .stdin(Stdio::null());
    cmd.spawn()
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
fn load_config(ports: &Ports, config_path: &Path) -> Result<Config, CliError> {
    if config_path == paths::global_config_path() && !config_path.exists() {
        ports.config.create_default(config_path)?;
        println!(
            "criado config default em {} — edite e rode de novo",
            config_path.display()
        );
    }
    Ok(ports.config.load(config_path)?)
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
#[allow(clippy::too_many_arguments, clippy::fn_params_excessive_bools)]
pub async fn serve(
    ports: Ports,
    config_path: PathBuf,
    host_flag: Option<String>,
    port_flag: Option<u16>,
    background: bool,
    check_update: bool,
    ephemeral: bool,
    model_override: Option<String>,
    enforce_active_model: bool,
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
        if ephemeral {
            args.push("--ephemeral".to_string());
        }
        if let Some(m) = model_override {
            args.push("--model".to_string());
            args.push(m);
        }
        let child = spawn_background(&args).map_err(CliError::from)?;
        println!(
            "started in background (pid {}), log: {}",
            child.id(),
            log_file().display()
        );
        return Ok(());
    }

    init_tracing(&ports);
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
    let mut state = runtime::build_runtime_state(&ports, &config_path).map_err(CliError::from)?;
    tracing::info!(target: crate::LOG_TARGET, path = %config_path.display(), "loaded config");

    // `serve --model <model>` seeds this instance's override model in memory
    // (never persisted). It acts as the fallback used when a client sends no
    // model. `enforce_active_model` makes that override authoritative: a
    // client-sent model is ignored, so the launched tool can never override the
    // user's selection.
    if let Some(m) = model_override.filter(|m| !m.is_empty()) {
        let mut cfg = (*state.config).clone();
        cfg.defaults.active_model = Some(m);
        state.config = Arc::new(cfg);
    }
    if enforce_active_model {
        state.enforce_active_model = true;
    }

    let host = host_flag.unwrap_or_else(|| state.config.server.host.clone());
    let port = port_flag
        .or_else(|| std::env::var("LOCAL_PROXY_PORT").ok()?.parse().ok())
        .unwrap_or(state.config.server.port);
    let addr = format!("{host}:{port}");

    let app_state = AppState::new(state, ports);
    app_state.set_port(port);
    let mut watched = vec![paths::config_dir()];
    watched.extend(config_path.parent().map(Path::to_path_buf));
    watcher::spawn(&config_path, watched, &app_state).map_err(CliError::from)?;
    let app = http::app(app_state);

    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .map_err(CliError::from)?;
    if !ephemeral {
        write_pid(std::process::id()).map_err(CliError::from)?;
    }
    tracing::info!(target: crate::LOG_TARGET, %addr, "listening");
    let result = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .await;
    if !ephemeral {
        remove_pid();
    }
    result.map_err(CliError::from)?;
    Ok(())
}

fn init_tracing(ports: &Ports) {
    use tracing_subscriber::fmt::writer::MakeWriterExt;
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| format!("{}={},tower_http=debug", crate::LOG_TARGET, "debug").into());

    // Mirror the proxy's logs into the per-user app data dir alongside the pid
    // file, in addition to the console, so issues can be diagnosed from the log
    // file even when the server runs in the background or detached. If the file
    // cannot be opened (e.g. read-only config dir), fall back to stdout only.
    let file_writer = std::fs::create_dir_all(paths::config_dir())
        .and_then(|()| std::fs::File::create(log_file()))
        .map(std::sync::Arc::new);

    // One formatter feeds the console, the log file and the `/admin/events` tap,
    // so colour only when a person reads stdout: escape codes in the file or the
    // tap break whatever renders them (the Claude Code mod's Logs tab).
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stdout()))
        .with_target(true);
    let tap = LogTap::new(ports.events.clone());
    let console = std::io::stdout.and(move || tap.clone());
    if let Ok(file) = file_writer {
        let _ = builder.with_writer(console.and(file)).try_init();
    } else {
        let _ = builder.with_writer(console).try_init();
    }
}

// ---------------------------------------------------------------------------
// launch
// ---------------------------------------------------------------------------

/// The env vars that make an Anthropic-compatible tool point at this proxy.
#[must_use]
pub fn launch_environment(
    config: &Config,
    port: u16,
    model: Option<&str>,
) -> Vec<(String, String)> {
    let base = format!("http://{}:{}", config.server.host, port);
    let auth = config
        .server
        .api_keys
        .first()
        .cloned()
        .unwrap_or_else(|| "unused".to_string());
    let mut env = vec![
        ("LOCAL_PROXY_PORT".to_string(), port.to_string()),
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
        "cursor" | "cr" => "cursor",
        _ => "claude",
    }
}

/// The env vars that make a tool point at this proxy, per tool.
///
/// Anthropic-compatible tools (Claude Code, Design) read `ANTHROPIC_BASE_URL`.
/// Cursor respects the same override, which sends it to the proxy's
/// `/v1/messages` endpoint and leaves the proxy to route and translate.
/// Returns `(env, oai_base)` where the second element is the base URL with a
/// trailing `/v1` for OpenAI-compatible tools that expect it, or `None`.
fn tool_launch_env(
    config: &Config,
    port: u16,
    model: Option<&str>,
    tool: &str,
) -> (Vec<(String, String)>, Option<String>) {
    let base = format!("http://{}:{}", config.server.host, port);
    let mut env = launch_environment(config, port, model);
    // Cursor has no `--model` flag of its own; pinning the proxy's active model
    // happens through `local-proxy model <provider>/<model>` instead.
    if tool == "cursor" || tool == "cr" {
        env.retain(|(k, _)| k != "ANTHROPIC_MODEL" && k != "ANTHROPIC_SMALL_FAST_MODEL");
        env.push(("OPENAI_API_BASE".to_string(), format!("{base}/v1")));
        env.push(("OPENAI_API_KEY".to_string(), "unused".to_string()));
        return (env, Some(format!("{base}/v1")));
    }
    // Claude Code warns when both are set; the proxy accepts the Bearer token.
    if tool == "claude" {
        env.retain(|(k, _)| k != "ANTHROPIC_API_KEY");
    }
    (env, None)
}

/// Spawn a dedicated, ephemeral proxy instance on `port` and wait until it
/// accepts connections, returning the child handle so its lifetime can be tied
/// to the launched tool.
///
/// # Errors
///
/// Returns an error if the proxy cannot be spawned or does not come up within
/// the wait window (the child is killed in that case).
#[allow(clippy::result_large_err)]
fn start_ephemeral_proxy(
    host: &str,
    port: u16,
    config_path: &Path,
    model: Option<&str>,
    enforce_active_model: bool,
) -> Result<std::process::Child, CliError> {
    let mut args = vec![
        "serve".to_string(),
        "--config".to_string(),
        config_path.display().to_string(),
        "--port".to_string(),
        port.to_string(),
        "--ephemeral".to_string(),
    ];
    if let Some(m) = model.filter(|m| !m.is_empty()) {
        args.push("--model".to_string());
        args.push(m.to_string());
    }
    if enforce_active_model {
        args.push("--enforce-active-model".to_string());
    }
    let mut child = spawn_launch_proxy(&args).map_err(CliError::from)?;
    println!("proxy started in background (pid {})", child.id());

    let mut up = false;
    for _ in 0..50 {
        if is_serving(host, port) {
            up = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    if !up {
        let _ = child.kill();
        let _ = child.wait();
        return Err(CliError::Tool {
            message: format!(
                "proxy did not come up on http://{host}:{port}; check the log at {}",
                log_file().display()
            ),
        });
    }
    Ok(child)
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
    ports: &Ports,
    config_path: PathBuf,
    tool: &str,
    model: Option<&str>,
    yes: bool,
    dry_run: bool,
    args: Vec<String>,
) -> miette::Result<()> {
    let config = load_config(ports, &config_path)?;
    let host = config.server.host.clone();
    let tool_cmd = tool_command_name(tool);

    // Always spin up a dedicated proxy instance on a random available port so
    // its lifetime can be tied to the launched tool, leaving any pre-existing
    // background proxy (and the shared pid file) untouched.
    let port = pick_ephemeral_port(&host).map_err(CliError::from)?;

    let proxy = if dry_run {
        None
    } else {
        // Only Anthropic-compatible tools (claude, design) send their own model
        // and must be pinned to the active model. Cursor is pinned via the
        // `OPENAI_API_BASE` override and the shared `local-proxy model` setting,
        // so it keeps client-driven selection.
        let enforce = tool_cmd == "claude" || tool_cmd == "design";
        Some(start_ephemeral_proxy(
            &host,
            port,
            &config_path,
            model,
            enforce,
        )?)
    };

    let (env, oai_base) = tool_launch_env(&config, port, model, tool_cmd);

    if dry_run {
        for (k, v) in &env {
            println!("{k}={v}");
        }
        let mut cmdline = String::from(tool_cmd);
        if yes && tool_cmd != "cursor" {
            cmdline.push_str(" --yes");
        }
        if !args.is_empty() {
            cmdline.push(' ');
            cmdline.push_str(&args.join(" "));
        }
        println!("command: {cmdline}");
        if tool_cmd == "cursor" {
            println!(
                "also set in Cursor: Settings → Models → Override OpenAI Base URL → {}",
                oai_base.as_deref().unwrap_or("")
            );
        }
        return Ok(());
    }

    let mut cmd = Command::new(tool_cmd);
    for (k, v) in &env {
        cmd.env(k, v);
    }
    if yes && tool_cmd != "cursor" {
        cmd.arg("--yes");
    }
    cmd.args(&args);
    let status = cmd.status().map_err(|e| CliError::Tool {
        message: format!("failed to spawn '{tool_cmd}' (is it installed and on PATH?): {e}"),
    });

    // The proxy's lifetime is tied to the launched tool: kill it on exit, in
    // all cases (including tool spawn failure or non-zero exit code).
    if let Some(mut child) = proxy {
        let _ = child.kill();
        let _ = child.wait();
    }

    let status = status?;
    std::process::exit(status.code().unwrap_or(1));
}

/// Marketplace (and plugin) name declared in `.claude-plugin/marketplace.json`.
const CLAUDE_MARKETPLACE: &str = "local-proxy";

/// `owner/repo` GitHub slug derived from the `repository` field in Cargo.toml.
fn repo_slug() -> &'static str {
    env!("CARGO_PKG_REPOSITORY").trim_start_matches("https://github.com/")
}

/// The `claude` invocations `setup claude` runs, in order.
fn claude_setup_argv(uninstall: bool) -> Vec<Vec<String>> {
    let plugin = format!("{CLAUDE_MARKETPLACE}@{CLAUDE_MARKETPLACE}");
    let v = |a: &[&str]| a.iter().map(ToString::to_string).collect::<Vec<_>>();
    if uninstall {
        vec![
            v(&["plugin", "uninstall", &plugin]),
            v(&["plugin", "marketplace", "remove", CLAUDE_MARKETPLACE]),
        ]
    } else {
        vec![
            v(&["plugin", "marketplace", "add", repo_slug()]),
            v(&["plugin", "install", &plugin]),
        ]
    }
}

/// The `claude` invocations that refresh the GitHub marketplace and plugin.
fn claude_plugin_update_argv() -> Vec<Vec<String>> {
    let plugin = format!("{CLAUDE_MARKETPLACE}@{CLAUDE_MARKETPLACE}");
    let v = |a: &[&str]| a.iter().map(ToString::to_string).collect::<Vec<_>>();
    vec![
        v(&["plugin", "marketplace", "update", CLAUDE_MARKETPLACE]),
        v(&["plugin", "update", &plugin]),
    ]
}

/// Refresh the remote Claude Code marketplace and plugin; report failures but
/// let the binary update continue independently.
fn update_claude_plugin() {
    for argv in claude_plugin_update_argv() {
        println!("$ claude {}", argv.join(" "));
        match Command::new("claude").args(&argv).status() {
            Ok(status) if !status.success() => {
                eprintln!("(passo terminou com {status}; seguindo)");
            }
            Err(error) => eprintln!("falha ao executar claude: {error}"),
            Ok(_) => {}
        }
    }
}

/// Install (or with `uninstall`, remove) the Claude Code mod via `claude plugin`.
///
/// Re-running is harmless: each step is attempted and a
/// failure (e.g. "already installed") is reported without aborting.
///
/// # Errors
///
/// Returns an error if `claude` cannot be spawned.
pub fn setup_claude(uninstall: bool) -> miette::Result<()> {
    for argv in claude_setup_argv(uninstall) {
        println!("$ claude {}", argv.join(" "));
        let status = Command::new("claude")
            .args(&argv)
            .status()
            .map_err(|e| CliError::Tool {
                message: format!("failed to spawn 'claude' (is it installed and on PATH?): {e}"),
            })?;
        if !status.success() {
            eprintln!("(passo terminou com {status}; seguindo)");
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// status / stop / logs / models
// ---------------------------------------------------------------------------

/// Print whether the proxy is currently running.
///
/// # Errors
///
/// Returns an error if the config cannot be loaded.
#[allow(clippy::needless_pass_by_value)]
pub fn status(ports: &Ports, config_path: PathBuf) -> miette::Result<()> {
    let config = load_config(ports, &config_path)?;
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
pub fn stop(ports: &Ports, config_path: PathBuf) -> miette::Result<()> {
    if let Some(pid) = read_pid() {
        stop_process(pid);
        remove_pid();
        println!("stopped (pid {pid})");
    } else {
        let config = load_config(ports, &config_path)?;
        if is_serving(&config.server.host, config.server.port) {
            println!("proxy is reachable but no pid file was found; not stopped");
        } else {
            println!("not running");
        }
    }
    Ok(())
}

/// CLI entry for `logs`: print the tail of the proxy's log file.
///
/// # Errors
///
/// Returns [`CliError::Io`] if the log file exists but cannot be read.
pub fn logs(ports: &Ports, lines: usize) -> miette::Result<()> {
    match ports.logs.tail(lines) {
        Ok(text) => println!("{text}"),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            println!(
                "nenhum log em {} (proxy ainda não rodou?)",
                ports.logs.location().display()
            );
        }
        Err(e) => return Err(CliError::Io(e).into()),
    }
    Ok(())
}

/// Print the models available from connected providers, exiting with a
/// non-zero status if none are connected.
///
/// # Errors
///
/// Returns a [`CliError`] if the catalog or config cannot be loaded.
pub fn models(ports: &Ports, config_path: &Path) -> miette::Result<()> {
    let connected = settings::connected_models(ports, config_path).map_err(CliError::from)?;
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
// settings: model / effort / connect / disconnect / account / providers
// ---------------------------------------------------------------------------

/// CLI entry for `model`: get, set, or clear the active model.
///
/// # Errors
///
/// Returns an error if the config cannot be loaded or written, or the
/// requested model is not available from a connected provider.
pub fn model(ports: &Ports, config_path: &Path, model: Option<&str>) -> miette::Result<()> {
    let msg = settings::model_result(ports, config_path, model).map_err(CliError::from)?;
    println!("{msg}");
    Ok(())
}

/// CLI entry for `effort`: show, set, or clear the reasoning effort.
///
/// # Errors
///
/// Returns an error if the level is unknown or the config cannot be written.
pub fn effort(ports: &Ports, config_path: &Path, level: Option<&str>) -> miette::Result<()> {
    let msg = settings::effort_result(ports, config_path, level).map_err(CliError::from)?;
    println!("{msg}");
    Ok(())
}

/// Validate that `provider` exists and store its credential: the API key
/// given (or prompted hidden), or, with `oauth`, the result of the provider's
/// OAuth login flow. Returns the success message.
///
/// # Errors
///
/// Returns a [`CliError`] if the provider is unknown, lacks an `oauth:` block
/// when `--oauth` is used, the prompt fails, or the store cannot be written.
#[allow(clippy::result_large_err)]
pub fn connect_provider(
    ports: &Ports,
    config_path: &Path,
    provider: &str,
    account: &str,
    key: Option<String>,
    oauth: bool,
) -> Result<String, CliError> {
    if oauth {
        let recipe = settings::oauth_recipe(ports, config_path, provider, account)?;
        let tokens = match recipe.flow {
            OAuthFlow::Paste => oauth_paste_login(ports, provider, &recipe)?,
            OAuthFlow::Callback => oauth_callback_login(ports, provider, &recipe)?,
        };
        return Ok(settings::save_oauth(ports, provider, account, tokens)?);
    }
    settings::check_api_key_target(ports, config_path, provider, account)?;
    let key =
        match key.filter(|k| !k.trim().is_empty()) {
            Some(k) => k,
            None => rpassword::prompt_password(format!("chave do provider {provider}: ")).map_err(
                |e| CliError::Connect {
                    message: format!("falha ao ler a chave: {e}"),
                },
            )?,
        };
    Ok(settings::save_api_key(ports, provider, account, &key)?)
}

/// A single-threaded runtime for the blocking OAuth flows.
#[allow(clippy::result_large_err)]
fn oauth_runtime() -> Result<tokio::runtime::Runtime, CliError> {
    Ok(tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?)
}

/// Run the `callback` OAuth flow for `provider`: browser login with the
/// redirect caught on a local listener.
#[allow(clippy::result_large_err)]
fn oauth_callback_login(
    ports: &Ports,
    provider: &str,
    recipe: &OAuthProvider,
) -> Result<crate::domain::account::OAuthTokens, CliError> {
    oauth_runtime()?
        .block_on(ports.oauth.callback_login(recipe))
        .map_err(|e| CliError::Connect {
            message: format!("provider '{provider}': falha no login OAuth: {e}"),
        })
}

/// Run the interactive `paste` OAuth flow for `provider`: print the authorize
/// URL, read the `code#state` from stdin, and exchange it for tokens. The state
/// carries the PKCE verifier, matching the reference Claude Code flow.
#[allow(clippy::result_large_err)]
fn oauth_paste_login(
    ports: &Ports,
    provider: &str,
    recipe: &OAuthProvider,
) -> Result<crate::domain::account::OAuthTokens, CliError> {
    use std::io::Write as _;

    let (verifier, challenge) = crate::domain::oauth::generate_pkce();
    let url = crate::domain::oauth::authorize_url(recipe, &challenge, &verifier).map_err(|e| {
        CliError::Connect {
            message: format!("provider '{provider}': {e}"),
        }
    })?;
    println!("abra esta URL no navegador e autorize o acesso:\n\n{url}\n");
    print!("cole o codigo (code#state) mostrado na pagina: ");
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let (code, pasted_state) =
        crate::domain::oauth::parse_code_input(&input).map_err(|e| CliError::Connect {
            message: format!("codigo invalido: {e}"),
        })?;
    let state = pasted_state.unwrap_or_else(|| verifier.clone());
    oauth_runtime()?
        .block_on(ports.oauth.exchange(recipe, &code, &verifier, &state))
        .map_err(|e| CliError::Connect {
            message: format!("falha ao trocar o codigo por tokens: {e}"),
        })
}

/// CLI entry for `connect`: store the API key (or OAuth login) for a provider.
///
/// # Errors
///
/// Returns a [`CliError`] if the provider is unknown or the store fails.
pub fn connect(
    ports: &Ports,
    config_path: &Path,
    provider: &str,
    account: &str,
    key: Option<String>,
    oauth: bool,
) -> miette::Result<()> {
    let msg = connect_provider(ports, config_path, provider, account, key, oauth)?;
    println!("{msg}");
    Ok(())
}

/// CLI entry for `disconnect`: remove one stored provider account.
///
/// # Errors
///
/// Returns a [`CliError`] if the store cannot be written.
pub fn disconnect(ports: &Ports, provider: &str, account: &str) -> miette::Result<()> {
    let msg = settings::disconnect_provider(ports, provider, account).map_err(CliError::from)?;
    println!("{msg}");
    Ok(())
}

/// Render the effective provider list (catalog and config) with account aliases.
///
/// # Errors
///
/// Returns a [`CliError`] if the catalog, config, or store cannot be loaded.
#[allow(clippy::result_large_err)]
pub fn list_providers(ports: &Ports, config_path: &Path) -> Result<String, CliError> {
    let lines: Vec<String> = settings::provider_accounts(ports, config_path)?
        .into_iter()
        .map(|p| {
            let status = if p.accounts.is_empty() {
                "-".to_string()
            } else {
                p.accounts
                    .iter()
                    .map(|a| format!("{} ({})", a.alias, a.kind))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            format!("{:<16} format={:<16} accounts={status}", p.name, p.format)
        })
        .collect();
    Ok(lines.join("\n"))
}

/// CLI entry for `providers`: print the effective provider list.
///
/// # Errors
///
/// Returns a [`CliError`] if the catalog or config cannot be loaded.
pub fn providers(ports: &Ports, config_path: &Path) -> miette::Result<()> {
    println!("{}", list_providers(ports, config_path)?);
    Ok(())
}

/// CLI entry for `account`: list, select, or clear the active account.
///
/// # Errors
///
/// Returns an error if the target is malformed or the config cannot be written.
pub fn account(ports: &Ports, config_path: &Path, args: &[String]) -> miette::Result<()> {
    let out = settings::account_result(ports, config_path, args).map_err(CliError::from)?;
    println!("{out}");
    Ok(())
}

// ---------------------------------------------------------------------------
// compare
// ---------------------------------------------------------------------------

/// Compare one request sent directly and after the proxy's request preparation.
///
/// The fixture must use the selected provider's native request format and
/// contain `{{compare_tag}}` in a cacheable prompt prefix. The command sends
/// `2 * runs` live requests and never stores the fixture or results in stats.
///
/// # Errors
///
/// Returns an error for invalid inputs, routing/account failures, or upstream
/// request failures.
#[allow(clippy::too_many_arguments)]
pub async fn compare(
    ports: &Ports,
    config_path: &Path,
    model: String,
    format: String,
    request_path: PathBuf,
    account: Option<String>,
    effort: Option<String>,
    runs: u32,
    confirm_live: bool,
    anthropic_beta: Option<String>,
) -> miette::Result<()> {
    let fixture_text = std::fs::read_to_string(request_path).map_err(CliError::from)?;
    let fixture: serde_json::Value = serde_json::from_str(&fixture_text)
        .map_err(|error| miette::miette!("invalid request fixture JSON: {error}"))?;
    if !fixture.is_object() {
        return Err(miette::miette!("request fixture must be a JSON object"));
    }
    compare_app::validate_live_request(&fixture, runs, confirm_live)
        .map_err(|error| miette::miette!("request comparison failed: {error}"))?;

    let client_format = match format.as_str() {
        "anthropic" => crate::domain::ir::Format::Anthropic,
        "openai" => crate::domain::ir::Format::Openai,
        "responses" => crate::domain::ir::Format::Responses,
        _ => return Err(miette::miette!("unsupported request format")),
    };
    let target = compare_app::resolve_target(ports, config_path, &model, client_format, account)
        .map_err(|error| miette::miette!("{error}"))?;

    let run_id = uuid::Uuid::new_v4();
    let direct_tag = format!("local-proxy-compare-{run_id}-direct");
    let proxy_tag = format!("local-proxy-compare-{run_id}-proxy");
    let session_id = format!("local-proxy-compare-{run_id}");
    let calls = runs.saturating_mul(2);
    eprintln!(
        "sending {calls} live requests to {}/{} with account {}",
        target.provider.name,
        target.upstream_model,
        target.account.alias()
    );
    let active_effort = effort.as_deref().or(target.active_effort.as_deref());
    let reasoning_effort = effort.as_deref().or(target.reasoning_effort.as_deref());
    let hints = ClientHints {
        user_agent: None,
        anthropic_beta,
    };
    let report = compare_app::run_comparison(
        &target.account,
        client_format,
        &target.provider,
        &target.upstream_model,
        &fixture,
        active_effort,
        reasoning_effort,
        &session_id,
        runs,
        &direct_tag,
        &proxy_tag,
        &hints,
    )
    .await
    .map_err(|error| miette::miette!("request comparison failed: {error}"))?;
    println!(
        "{}",
        serde_json::to_string_pretty(&report)
            .map_err(|error| miette::miette!("failed to render comparison report: {error}"))?
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// stats
// ---------------------------------------------------------------------------

/// Print aggregate usage statistics collected from upstream requests.
///
/// Renders a human summary (with a per-provider breakdown) by default, or the
/// same data as JSON when `json` is set. When no stats have been recorded yet,
/// prints a message and returns without error.
///
/// # Errors
///
/// Returns [`CliError::Stats`] if the stats database cannot be read.
pub fn stats(ports: &Ports, since: &str, json: bool) -> miette::Result<()> {
    let report =
        stats_report::load(ports.usage.as_ref(), since, StatsScope::All).map_err(CliError::from)?;
    let Some(report) = report else {
        println!("nenhuma estatística registrada ainda (a primeira requisição proxy cria o banco)");
        return Ok(());
    };
    if json {
        println!("{}", stats_report::render_json(StatsScope::All, &report));
    } else {
        render_stats_text(&report.summary, &report.by_provider, &report.recent);
    }
    Ok(())
}

/// Render the human-readable stats report.
#[allow(clippy::format_push_string, clippy::cast_precision_loss)]
fn render_stats_text(summary: &RowSummary, by_provider: &[ProviderStats], recent: &[RequestRow]) {
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

/// Env guard that must be set for the hidden `__cleanup-old` mode to delete a
/// file, so it can never be used to remove arbitrary paths by accident.
const CLEANUP_ENV: &str = "LOCAL_PROXY_CLEANUP";

/// Hidden self-update helper: wait briefly (so the just-exited process releases
/// the lock on its swapped `.old` image) then delete the given stale backup.
///
/// Only runs when [`CLEANUP_ENV`] is set, and only ever deletes the single
/// `path` argument. Replaces the previous `cmd /c timeout & del` helper so no
/// console window can flash during an update.
///
/// # Errors
///
/// Returns an error if [`CLEANUP_ENV`] is not set (defense against misuse).
pub fn cleanup_old_file(path: &str) -> miette::Result<()> {
    if std::env::var_os(CLEANUP_ENV).is_none() {
        miette::bail!("refusing to delete '{path}' without {CLEANUP_ENV} set");
    }
    std::thread::sleep(Duration::from_secs(2));
    let _ = std::fs::remove_file(path);
    Ok(())
}

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
        let helper = std::env::current_exe().map_err(|source| UpdateError::Replace {
            path: staged.display().to_string(),
            exe: exe.display().to_string(),
            command: manual_replace_command(staged, exe),
            source,
        })?;
        let mut cmd = Command::new(helper);
        cmd.arg("__cleanup-old").arg(&old);
        cmd.env(CLEANUP_ENV, "1");
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
    if !check {
        update_claude_plugin();
    }
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
    use crate::domain::config::{Defaults, Server};

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
            exec: crate::domain::config::Exec::default(),
        }
    }

    #[test]
    fn claude_setup_argv_install_and_uninstall() {
        assert_eq!(
            claude_setup_argv(false),
            vec![
                vec!["plugin", "marketplace", "add", "gsporto226/local-proxy"],
                vec!["plugin", "install", "local-proxy@local-proxy"],
            ]
        );
        assert_eq!(
            claude_setup_argv(true),
            vec![
                vec!["plugin", "uninstall", "local-proxy@local-proxy"],
                vec!["plugin", "marketplace", "remove", "local-proxy"],
            ]
        );
    }

    #[test]
    fn claude_plugin_update_argv_refreshes_marketplace_then_plugin() {
        assert_eq!(
            claude_plugin_update_argv(),
            vec![
                vec!["plugin", "marketplace", "update", "local-proxy"],
                vec!["plugin", "update", "local-proxy@local-proxy"],
            ]
        );
    }

    #[test]
    fn launch_env_uses_configured_key_and_model() {
        let cfg = config_with(vec!["sk-proxy".to_string()]);
        let env = launch_environment(&cfg, 8787, Some("kimi-k2.6"));
        let map: std::collections::HashMap<_, _> = env.into_iter().collect();
        assert_eq!(map["LOCAL_PROXY_PORT"], "8787");
        assert_eq!(map["ANTHROPIC_BASE_URL"], "http://127.0.0.1:8787");
        assert_eq!(map["ANTHROPIC_API_KEY"], "sk-proxy");
        assert_eq!(map["ANTHROPIC_AUTH_TOKEN"], "sk-proxy");
        assert_eq!(map["ANTHROPIC_MODEL"], "kimi-k2.6");
        assert_eq!(map["ANTHROPIC_SMALL_FAST_MODEL"], "kimi-k2.6");
    }

    #[test]
    fn launch_env_without_keys_uses_unused_and_no_model() {
        let cfg = config_with(Vec::new());
        let env = launch_environment(&cfg, 8787, None);
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
        assert_eq!(tool_command_name("cursor"), "cursor");
        assert_eq!(tool_command_name("cr"), "cursor");
    }

    #[test]
    fn cursor_env_sets_openai_and_strips_anthropic_model() {
        let cfg = config_with(vec!["sk-proxy".to_string()]);
        let (env, oai_base) = tool_launch_env(&cfg, 8787, Some("kimi-k2.6"), "cursor");
        let map: std::collections::HashMap<_, _> = env.into_iter().collect();
        assert_eq!(oai_base.as_deref(), Some("http://127.0.0.1:8787/v1"));
        assert_eq!(map["ANTHROPIC_BASE_URL"], "http://127.0.0.1:8787");
        assert_eq!(map["OPENAI_API_BASE"], "http://127.0.0.1:8787/v1");
        assert_eq!(map["OPENAI_API_KEY"], "unused");
        assert_eq!(map["ANTHROPIC_API_KEY"], "sk-proxy");
        assert!(!map.contains_key("ANTHROPIC_MODEL"));
        assert!(!map.contains_key("ANTHROPIC_SMALL_FAST_MODEL"));
    }

    #[test]
    fn claude_env_has_no_openai_overrides() {
        let cfg = config_with(vec![]);
        let (env, oai_base) = tool_launch_env(&cfg, 8787, None, "claude");
        let map: std::collections::HashMap<_, _> = env.into_iter().collect();
        assert!(oai_base.is_none());
        assert!(!map.contains_key("OPENAI_API_BASE"));
        assert!(!map.contains_key("OPENAI_API_KEY"));
        assert!(!map.contains_key("ANTHROPIC_API_KEY"));
        assert_eq!(map["ANTHROPIC_AUTH_TOKEN"], "unused");
    }

    #[test]
    fn pick_ephemeral_port_returns_an_open_port() {
        let port = pick_ephemeral_port("127.0.0.1").expect("pick a port");
        assert_ne!(port, 0);
        let listener = TcpListener::bind(("127.0.0.1", port)).expect("port is rebindable");
        assert_eq!(listener.local_addr().unwrap().port(), port);
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
