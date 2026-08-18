//! CLI launcher: background `serve`, `launch claude|design`, `status`, `stop`,
//! `models`. Pure helpers are unit-testable; process spawns are best-effort.

use std::io;
use std::net::{TcpStream, ToSocketAddrs};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use crate::config::{Config, DEFAULT_CONFIG_PATH};
use crate::handlers::{self, AppState};
use crate::router::Router;

const LOG_TARGET: &str = "local_proxy";

// ---------------------------------------------------------------------------
// runtime files (~/.config/local-proxy/)
// ---------------------------------------------------------------------------

pub fn config_dir() -> PathBuf {
    let base = std::env::var_os("LOCAL_PROXY_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("USERPROFILE").map(PathBuf::from))
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join(".config").join("local-proxy")
}

pub fn pid_file() -> PathBuf {
    config_dir().join("pid")
}

pub fn log_file() -> PathBuf {
    config_dir().join("local-proxy.log")
}

pub fn write_pid(pid: u32) -> io::Result<()> {
    std::fs::create_dir_all(config_dir())?;
    std::fs::write(pid_file(), pid.to_string())
}

pub fn remove_pid() {
    let _ = std::fs::remove_file(pid_file());
}

pub fn read_pid() -> Option<u32> {
    std::fs::read_to_string(pid_file())
        .ok()
        .and_then(|s| s.trim().parse().ok())
}

pub fn stop_process(pid: u32) {
    #[cfg(windows)]
    let _ = Command::new("taskkill")
        .args(["/F", "/PID", &pid.to_string()])
        .status();
    #[cfg(not(windows))]
    let _ = Command::new("kill").args(["-9", &pid.to_string()]).status();
}

/// Is something accepting TCP connections at host:port (proxy likely up)?
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
// serve
// ---------------------------------------------------------------------------

pub async fn serve(
    config_path: PathBuf,
    host_flag: Option<String>,
    port_flag: Option<u16>,
    background: bool,
) -> Result<(), Box<dyn std::error::Error>> {
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
        let child = spawn_background(&args)?;
        println!(
            "started in background (pid {}), log: {}",
            child.id(),
            log_file().display()
        );
        return Ok(());
    }

    init_tracing();
    let config = Arc::new(Config::load(&config_path)?);
    tracing::info!(target: LOG_TARGET, path = %config_path.display(), "loaded config");

    let router = Arc::new(Router::new(config.clone())?);
    let clients = Arc::new(handlers::build_clients(&config)?);
    let state = AppState {
        config,
        router,
        clients,
    };

    let host = host_flag.unwrap_or_else(|| state.config.server.host.clone());
    let port = port_flag.unwrap_or(state.config.server.port);
    let addr = format!("{host}:{port}");
    let app = handlers::app(state);

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    write_pid(std::process::id())?;
    tracing::info!(target: LOG_TARGET, %addr, "listening");
    let result = axum::serve(listener, app).await;
    remove_pid();
    result?;
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

pub fn launch(
    config_path: PathBuf,
    tool: &str,
    model: Option<&str>,
    yes: bool,
    dry_run: bool,
    args: Vec<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::load(&config_path)?;
    let host = config.server.host.clone();
    let port = config.server.port;
    let tool_cmd = tool_command_name(tool);

    if !is_serving(&host, port) {
        let bg = spawn_background(&[
            "serve".to_string(),
            "--config".to_string(),
            config_path.display().to_string(),
        ])?;
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
    let status = cmd.status().map_err(|e| {
        Box::<dyn std::error::Error>::from(format!(
            "failed to spawn '{tool_cmd}' (is it installed and on PATH?): {e}"
        ))
    })?;
    std::process::exit(status.code().unwrap_or(1));
}

// ---------------------------------------------------------------------------
// status / stop / models
// ---------------------------------------------------------------------------

pub fn status(config_path: PathBuf) -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::load(&config_path)?;
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

pub fn stop(config_path: PathBuf) -> Result<(), Box<dyn std::error::Error>> {
    match read_pid() {
        Some(pid) => {
            stop_process(pid);
            remove_pid();
            println!("stopped (pid {pid})");
        }
        None => {
            let config = Config::load(&config_path)?;
            if is_serving(&config.server.host, config.server.port) {
                println!("proxy is reachable but no pid file was found; not stopped");
            } else {
                println!("not running");
            }
        }
    }
    Ok(())
}

pub fn models(config_path: PathBuf) -> Result<(), Box<dyn std::error::Error>> {
    let config = Arc::new(Config::load(&config_path)?);
    let router = Router::new(config)?;
    for m in router.list_models() {
        println!("{m}");
    }
    Ok(())
}

pub fn resolve_config_path(flag: Option<PathBuf>) -> PathBuf {
    flag.or_else(Config::env_config_path)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_CONFIG_PATH))
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Server;

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
            defaults: Default::default(),
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
}
