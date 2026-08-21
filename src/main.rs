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
        /// Override model used when the client sends none (instance-only)
        #[arg(long)]
        model: Option<String>,
        /// Managed instance: skip the shared pid file (used by `launch`)
        #[arg(long, hide = true)]
        ephemeral: bool,
        /// Ignore any client-sent model and always route through the active
        /// model (set by `launch claude`)
        #[arg(long, hide = true)]
        enforce_active_model: bool,
    },
    /// Start the proxy (if needed) and launch a compatible tool against it
    Launch {
        /// Tool: claude (default) | design | cursor
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
    /// List models available from connected providers
    Models,
    /// Get or set the active model (selected, else first available)
    Model {
        /// Model to set as active; omit to show current; "clear" to unset
        model: Option<String>,
    },
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
    /// Show usage statistics recorded from upstream requests
    Stats {
        /// Time window: day (default) | week | month | all
        #[arg(long)]
        since: Option<String>,
        /// Print the report as JSON
        #[arg(long)]
        json: bool,
    },
    /// Render the Claude Code status line for a session from its recorded stats
    Statusline {
        /// Client session id from the status line JSON (`session_id`)
        #[arg(long)]
        session: Option<String>,
        /// Model name from the status line JSON (`model.display_name`)
        #[arg(long)]
        model: Option<String>,
        /// Context window usage percent from the status line JSON
        #[arg(long)]
        context_pct: Option<f64>,
        /// Rhai template (overrides the config `statusline:` block)
        #[arg(long)]
        template: Option<String>,
        /// Write the status-line script to the config dir and register it in
        /// Claude's settings.json (`statusline setup`)
        #[arg(long)]
        setup: bool,
        /// Claude settings.json to update (with `--setup`; defaults to the
        /// platform `~/.claude/settings.json`)
        #[arg(long)]
        settings: Option<PathBuf>,
    },
    /// Write the status-line script and register it in Claude's settings.
    StatuslineSetup {
        /// Claude settings.json to update (defaults to
        /// `~/.claude/settings.json`; skipped if that file does not exist)
        #[arg(long)]
        settings: Option<PathBuf>,
    },
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
    /// Hidden self-update helper: delete a stale backup after a delay (do not use).
    #[command(name = "__cleanup-old", hide = true)]
    CleanupOld {
        /// Path of the stale `.old` backup to delete after a delay
        path: String,
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
        None => block_on(cli::serve(
            config, None, None, false, false, false, None, false,
        )),
        Some(Command::Serve {
            host,
            port,
            background,
            check_update,
            model,
            ephemeral,
            enforce_active_model,
        }) => block_on(cli::serve(
            config,
            host,
            port,
            background,
            check_update,
            ephemeral,
            model,
            enforce_active_model,
        )),
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
        Some(Command::Model { model }) => cli::model(config, model),
        Some(Command::Connect { provider, key }) => cli::connect(config, provider, key),
        Some(Command::Disconnect { provider }) => cli::disconnect(config, provider),
        Some(Command::Providers) => cli::providers(config),
        Some(Command::Stats { since, json }) => {
            let since = since.unwrap_or_else(|| "day".to_string());
            cli::stats(config, since, json)
        }
        Some(Command::Statusline {
            session,
            model,
            context_pct,
            template,
            setup,
            settings,
        }) => cli::statusline(
            config,
            session,
            model,
            context_pct,
            template,
            setup,
            settings,
        ),
        Some(Command::StatuslineSetup { settings }) => cli::statusline_setup(config, settings),
        Some(Command::Update {
            repo,
            check,
            force,
            no_verify,
        }) => block_on(cli::update(repo, check, force, no_verify)),
        Some(Command::CleanupOld { path }) => cli::cleanup_old_file(&path),
    }
}
