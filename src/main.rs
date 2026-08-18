use std::path::PathBuf;

use clap::{Parser, Subcommand};
use tokio::runtime::Runtime;

use local_proxy::cli;

#[derive(Debug, Parser)]
#[command(
    name = "local-proxy",
    version,
    about = "Local multi-provider translation proxy (OpenAI <-> Anthropic)"
)]
struct Cli {
    /// Path to config file (YAML or JSON); overrides LOCAL_PROXY_CONFIG
    #[arg(long, global = true, value_name = "PATH")]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the proxy server (foreground by default)
    Serve {
        /// Override server host
        #[arg(long)]
        host: Option<String>,
        /// Override server port
        #[arg(long)]
        port: Option<u16>,
        /// Run detached in the background
        #[arg(long)]
        background: bool,
    },
    /// Start the proxy (if needed) and launch an Anthropic-compatible tool
    Launch {
        /// Tool: claude (default) | design
        tool: Option<String>,
        /// Model to route Claude to (sets ANTHROPIC_MODEL / _SMALL_FAST_MODEL)
        #[arg(long)]
        model: Option<String>,
        /// Forward --yes to the tool
        #[arg(long)]
        yes: bool,
        /// Print the env/command without running anything
        #[arg(long)]
        dry_run: bool,
        /// Arguments passed through to the tool (after --)
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Show whether the background proxy is running
    Status,
    /// Stop the background proxy
    Stop,
    /// List routed models
    Models,
}

fn block_on<F: std::future::Future>(fut: F) -> F::Output {
    Runtime::new()
        .expect("failed to build tokio runtime")
        .block_on(fut)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    let config = cli::resolve_config_path(cli.config);
    match cli.command {
        None => block_on(cli::serve(config, None, None, false)),
        Some(Command::Serve {
            host,
            port,
            background,
        }) => block_on(cli::serve(config, host, port, background)),
        Some(Command::Launch {
            tool,
            model,
            yes,
            dry_run,
            args,
        }) => cli::launch(
            config,
            tool.as_deref().unwrap_or("claude"),
            model.as_deref(),
            yes,
            dry_run,
            args,
        ),
        Some(Command::Status) => cli::status(config),
        Some(Command::Stop) => cli::stop(config),
        Some(Command::Models) => cli::models(config),
    }
}
