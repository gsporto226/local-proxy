use std::path::PathBuf;

use clap::{Parser, Subcommand};

use local_proxy::config::{Config, DEFAULT_CONFIG_PATH};
use local_proxy::router::Router;

const LOG_TARGET: &str = "local_proxy";

#[derive(Debug, Parser)]
#[command(
    name = "local-proxy",
    version,
    about = "Local multi-provider translation proxy (OpenAI <-> Anthropic)"
)]
struct Cli {
    /// Path to config file (YAML or JSON); overrides LOCAL_PROXY_CONFIG
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,
    /// Override server host
    #[arg(long)]
    host: Option<String>,
    /// Override server port
    #[arg(long)]
    port: Option<u16>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the proxy server
    Serve {
        /// Path to config file (YAML or JSON); overrides LOCAL_PROXY_CONFIG
        #[arg(long, value_name = "PATH")]
        config: Option<PathBuf>,
        /// Override server host
        #[arg(long)]
        host: Option<String>,
        /// Override server port
        #[arg(long)]
        port: Option<u16>,
    },
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    match cli.command {
        Some(Command::Serve { config, host, port }) => run_serve(config, host, port),
        None => run_serve(cli.config, cli.host, cli.port),
    }
}

#[tokio::main]
async fn run_serve(
    config_flag: Option<PathBuf>,
    host_flag: Option<String>,
    port_flag: Option<u16>,
) -> Result<(), Box<dyn std::error::Error>> {
    init_tracing();

    let path = config_flag
        .or_else(Config::env_config_path)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_CONFIG_PATH));
    let config = Config::load(&path)?;
    tracing::info!(target: LOG_TARGET, path = %path.display(), "loaded config");

    let host = host_flag.unwrap_or_else(|| config.server.host.clone());
    let port = port_flag.unwrap_or(config.server.port);

    let _registry = Router::new(&config)?;
    tracing::info!(target: LOG_TARGET, routes = config.routes.len(), providers = config.providers.len(), "router ready");

    let addr = format!("{host}:{port}");
    let app = axum::Router::new().route("/health", axum::routing::get(health));

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!(target: LOG_TARGET, %addr, "listening");
    axum::serve(listener, app).await?;
    Ok(())
}

async fn health() -> &'static str {
    "ok"
}

fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| format!("{LOG_TARGET}=info,tower_http=info").into());
    tracing_subscriber::fmt().with_env_filter(filter).init();
}
