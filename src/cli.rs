//! CLI launcher: background `serve`, `launch claude|design`, `status`, `stop`,
//! `models`. Pure helpers are unit-testable; process spawns are best-effort.

use std::io;
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use miette::Diagnostic;
use thiserror::Error;

use crate::config::{Config, DEFAULT_CONFIG_PATH};
use crate::handlers::{self, AppState};
use crate::router::Router;

const LOG_TARGET: &str = "local_proxy";

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
    #[diagnostic(
        code(cli::tool),
        help("ensure the CLI tool is installed and on PATH")
    )]
    Tool {
        /// Human-readable description of the failure.
        message: String,
    },
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
        let child = spawn_background(&args).map_err(CliError::from)?;
        println!(
            "started in background (pid {}), log: {}",
            child.id(),
            log_file().display()
        );
        return Ok(());
    }

    init_tracing();
    let config = Arc::new(load_config(&config_path)?);
    tracing::info!(target: LOG_TARGET, path = %config_path.display(), "loaded config");

    let router = Arc::new(Router::new(config.clone()).map_err(CliError::from)?);
    let clients = Arc::new(handlers::build_clients(&config).map_err(CliError::from)?);
    let state = AppState {
        config,
        router,
        clients,
    };

    let host = host_flag.unwrap_or_else(|| state.config.server.host.clone());
    let port = port_flag.unwrap_or(state.config.server.port);
    let addr = format!("{host}:{port}");
    let app = handlers::app(state);

    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .map_err(CliError::from)?;
    write_pid(std::process::id()).map_err(CliError::from)?;
    tracing::info!(target: LOG_TARGET, %addr, "listening");
    let result = axum::serve(listener, app).await;
    remove_pid();
    result.map_err(CliError::from)?;
    Ok(())
}

fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| format!("{LOG_TARGET}=info,tower_http=info").into());
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
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
        message: format!(
            "failed to spawn '{tool_cmd}' (is it installed and on PATH?): {e}"
        ),
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
/// # Errors
///
/// Returns an error if the config cannot be loaded or the router cannot be built.
#[allow(clippy::needless_pass_by_value)]
pub fn models(config_path: PathBuf) -> miette::Result<()> {
    let config = Arc::new(load_config(&config_path)?);
    let router = Router::new(config).map_err(CliError::from)?;
    for m in router.list_models() {
        println!("{m}");
    }
    Ok(())
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
    fn resolve_config_path_uses_cwd_yaml_when_present() {
        let prev = std::env::current_dir().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("config.yaml"), "server: {}\n").unwrap();
        std::env::set_current_dir(tmp.path()).unwrap();
        let result = resolve_config_path(None);
        std::env::set_current_dir(prev).unwrap();
        assert_eq!(result, tmp.path().join("config.yaml"));
    }

    #[test]
    fn resolve_config_path_defaults_to_global_when_no_cwd_file() {
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
}
