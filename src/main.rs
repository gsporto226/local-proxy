//! `local-proxy` command-line interface: serve the proxy, launch compatible
//! tools, and manage the background process.

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
    /// Path to config file (YAML or JSON); overrides `LOCAL_PROXY_CONFIG`
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
        /// Check once at startup for a newer release and warn in the log
        #[arg(long)]
        check_update: bool,
    },
    /// Start the proxy (if needed) and launch an Anthropic-compatible tool
    Launch {
        /// Tool: claude (default) | design
        tool: Option<String>,
        /// Model to route Claude to (sets `ANTHROPIC_MODEL` / `_SMALL_FAST_MODEL`)
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
    /// Store the API key for an existing provider (catalog or config)
    Connect {
        /// Provider name (must exist in the catalog or config)
        provider: String,
        /// API key; prompted hidden if omitted
        key: Option<String>,
    },
    /// Remove the stored API key for a provider
    Disconnect {
        /// Provider name
        provider: String,
    },
    /// List effective providers (catalog ∪ config) with key status
    Providers,
    /// Run the MCP stdio server (connect/disconnect/models/providers)
    Mcp,
    /// Check for a newer release and stage a manual update from GitHub Releases
    Update {
        /// GitHub owner/repo (overrides `LOCAL_PROXY_REPO`)
        #[arg(long)]
        repo: Option<String>,
        /// Only report the latest version, without downloading
        #[arg(long)]
        check: bool,
        /// Update even if already on the latest version
        #[arg(long)]
        force: bool,
        /// Skip SHA256 verification
        #[arg(long)]
        no_verify: bool,
    },
}

fn block_on<F: std::future::Future>(fut: F) -> F::Output {
    Runtime::new()
        .expect("failed to build tokio runtime")
        .block_on(fut)
}

fn main() -> miette::Result<()> {
    miette::set_hook(Box::new(
        |_| Box::new(miette::GraphicalReportHandler::new()),
    ))?;
    let cli = Cli::parse();
    let config = cli::resolve_config_path(cli.config);
    match cli.command {
        None => block_on(cli::serve(config, None, None, false, false)),
        Some(Command::Serve {
            host,
            port,
            background,
            check_update,
        }) => block_on(cli::serve(config, host, port, background, check_update)),
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
        Some(Command::Connect { provider, key }) => cli::connect(config, provider, key),
        Some(Command::Disconnect { provider }) => cli::disconnect(config, provider),
        Some(Command::Providers) => cli::providers(config),
        Some(Command::Mcp) => block_on(cli::mcp(config)),
        Some(Command::Update {
            repo,
            check,
            force,
            no_verify,
        }) => block_on(cli::update(repo, check, force, no_verify)),
    }
}
