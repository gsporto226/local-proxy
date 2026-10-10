//! `local-proxy` command-line interface: serve the proxy, launch compatible
//! tools, and manage the background process.

use std::path::PathBuf;

use clap::{Parser, Subcommand};
use tokio::runtime::Runtime;

use local_proxy::adapters::inbound::cli;
use local_proxy::adapters::outbound::paths;
use local_proxy::application::commands::DEFAULT_LOG_LINES;

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
    /// Show the tail of the proxy log file
    Logs {
        /// Number of lines to print
        #[arg(short = 'n', long, default_value_t = DEFAULT_LOG_LINES)]
        lines: usize,
    },
    /// List models available from connected providers
    Models,
    /// Get or set the active model (selected, else first available)
    Model {
        /// Model to set as active; omit to show current; "clear" to unset
        model: Option<String>,
    },
    /// Show or set the reasoning effort the proxy forces on every request
    Effort {
        /// low | medium | high | xhigh | max; omit to show; "clear" to unset
        level: Option<String>,
    },
    /// Store credentials for an existing provider (API key, or OAuth login)
    Connect {
        /// Provider name (must exist in the catalog or config)
        provider: String,
        /// Account alias under this provider (required; e.g. personal or work)
        #[arg(long, required = true, value_name = "ALIAS")]
        account: String,
        /// API key; prompted hidden if omitted
        key: Option<String>,
        /// Run the provider's OAuth login flow instead of storing an API key
        /// (requires an `oauth:` block in the provider config)
        #[arg(long)]
        oauth: bool,
    },
    /// Remove one stored account for a provider
    Disconnect {
        /// Provider name
        provider: String,
        /// Account alias to remove (other accounts are preserved)
        #[arg(long, required = true, value_name = "ALIAS")]
        account: String,
    },
    /// List stored accounts or select the active one (`provider/alias`)
    Account {
        /// `provider/alias` to select, or `clear [provider]` to unset
        #[arg(value_name = "PROVIDER/ALIAS | clear [PROVIDER]")]
        args: Vec<String>,
    },
    /// List effective providers (catalog ∪ config) with account aliases
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
    /// Compare direct and proxy-prepared request behavior (diagnostic only)
    #[command(hide = true)]
    Compare {
        /// Routed model, preferably in provider/model form
        #[arg(long, required = true)]
        model: String,
        /// Native request format expected by the selected provider
        #[arg(long, value_parser = ["anthropic", "openai", "responses"], required = true)]
        format: String,
        /// Path to a JSON request fixture containing `{{compare_tag}}`
        #[arg(long, required = true)]
        request: PathBuf,
        /// Provider account alias; defaults to the configured active account
        #[arg(long)]
        account: Option<String>,
        /// Effort to add when supported by the selected format
        #[arg(long)]
        effort: Option<String>,
        /// Number of requests per arm (each additional run sends two requests)
        #[arg(long, default_value_t = 1)]
        runs: u32,
        /// Confirm live upstream requests using the selected account
        #[arg(long, required = true)]
        confirm_live: bool,
        /// Client `anthropic-beta` header to forward on both arms
        #[arg(long)]
        anthropic_beta: Option<String>,
    },
    /// Install the Claude Code mod (`claude plugin marketplace add` + `install`)
    Setup {
        /// Target tool (only `claude`)
        #[arg(value_parser = ["claude"])]
        tool: String,
        /// Remove the mod and its marketplace instead
        #[arg(long)]
        uninstall: bool,
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
    let config = paths::resolve_config_path(cli.config);
    let ports = local_proxy::bootstrap::ports();
    match cli.command {
        None => block_on(cli::serve(
            ports, config, None, None, false, false, false, None, false,
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
            ports,
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
            &ports,
            config,
            tool.as_deref().unwrap_or("claude"),
            model.as_deref(),
            yes,
            dry_run,
            args,
        ),
        Some(Command::Status) => cli::status(&ports, config),
        Some(Command::Stop) => cli::stop(&ports, config),
        Some(Command::Logs { lines }) => cli::logs(&ports, lines),
        Some(Command::Models) => cli::models(&ports, &config),
        Some(Command::Model { model }) => cli::model(&ports, &config, model.as_deref()),
        Some(Command::Effort { level }) => cli::effort(&ports, &config, level.as_deref()),
        Some(Command::Connect {
            provider,
            account,
            key,
            oauth,
        }) => cli::connect(&ports, &config, &provider, &account, key, oauth),
        Some(Command::Disconnect { provider, account }) => {
            cli::disconnect(&ports, &provider, &account)
        }
        Some(Command::Account { args }) => cli::account(&ports, &config, &args),
        Some(Command::Providers) => cli::providers(&ports, &config),
        Some(Command::Stats { since, json }) => {
            cli::stats(&ports, since.as_deref().unwrap_or("day"), json)
        }
        Some(Command::Compare {
            model,
            format,
            request,
            account,
            effort,
            runs,
            confirm_live,
            anthropic_beta,
        }) => block_on(cli::compare(
            &ports,
            &config,
            model,
            format,
            request,
            account,
            effort,
            runs,
            confirm_live,
            anthropic_beta,
        )),
        Some(Command::Setup { uninstall, .. }) => cli::setup_claude(uninstall),
        Some(Command::Update {
            repo,
            check,
            force,
            no_verify,
        }) => block_on(cli::update(repo, check, force, no_verify)),
        Some(Command::CleanupOld { path }) => cli::cleanup_old_file(&path),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn compare_command_is_hidden_from_main_help_but_has_its_own_help() {
        let main_help = Cli::command().render_long_help().to_string();
        assert!(!main_help.contains("compare"));

        let error = Cli::try_parse_from(["local-proxy", "compare", "--help"]).unwrap_err();
        let compare_help = error.to_string();
        assert!(compare_help.contains("--model"));
        assert!(compare_help.contains("--request"));
        assert!(compare_help.contains("--confirm-live"));
    }
}
