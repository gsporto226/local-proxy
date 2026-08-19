//! MCP server (`local-proxy mcp`) exposing provider/key/model management tools.
//!
//! Runs over stdio and shares the same config/auth files as the proxy, so the
//! running proxy's file watcher applies changes (connect, select) immediately,
//! without a restart. Tools:
//!
//! - `connect(provider, key)` — store an API key for an existing provider.
//! - `disconnect(provider)` — remove a stored API key.
//! - `providers()` — list effective providers (catalog ∪ config) + key status.
//! - `models([select])` — list models from connected providers; `select` persists
//!   the active model in the config (which overrides the harness's model).

use std::path::PathBuf;

use rmcp::handler::server::wrapper::Parameters;
use rmcp::schemars;
use rmcp::schemars::JsonSchema;
use rmcp::transport::stdio;
use rmcp::{tool, tool_router, ErrorData as McpError, ServiceExt};
use serde::Deserialize;

/// Parameters for the `connect` tool.
#[derive(Debug, Deserialize, JsonSchema)]
struct ConnectParams {
    /// Provider name (must exist in the catalog or the config overlay).
    provider: String,
    /// The API key to store in `auth.json`.
    key: String,
}

/// Parameters for the `disconnect` tool.
#[derive(Debug, Deserialize, JsonSchema)]
struct DisconnectParams {
    /// Provider name whose key should be removed.
    provider: String,
}

/// Parameters for the `models` tool.
#[derive(Debug, Deserialize, JsonSchema)]
struct ModelsParams {
    /// Model to select and persist as the active default model; an empty
    /// string clears the selection.
    #[serde(default)]
    #[schemars(description = "Model to select (persisted in config); empty string clears")]
    select: Option<String>,
}

/// The local-proxy MCP server.
#[derive(Clone)]
pub struct LocalProxyServer {
    config_path: PathBuf,
}

#[tool_router(server_handler)]
impl LocalProxyServer {
    /// Create the server for a given config path.
    #[must_use]
    pub const fn new(config_path: PathBuf) -> Self {
        Self { config_path }
    }

    /// Store an API key for an existing provider (catalog or config).
    #[tool(description = "Store the API key for an existing provider (catalog or config)")]
    fn connect(&self, Parameters(params): Parameters<ConnectParams>) -> Result<String, McpError> {
        crate::cli::connect_provider(&self.config_path, &params.provider, Some(params.key))
            .map_err(|e| McpError::internal_error(e.to_string(), None))
    }

    /// Remove the stored API key for a provider.
    #[tool(description = "Remove the stored API key for a provider")]
    fn disconnect(
        &self,
        Parameters(params): Parameters<DisconnectParams>,
    ) -> Result<String, McpError> {
        crate::cli::disconnect_provider(&self.config_path, &params.provider)
            .map_err(|e| McpError::internal_error(e.to_string(), None))
    }

    /// List effective providers (catalog + config) with key status.
    #[tool(description = "List effective providers (catalog plus config) with key status")]
    fn providers(&self) -> Result<String, McpError> {
        crate::cli::list_providers(&self.config_path)
            .map_err(|e| McpError::internal_error(e.to_string(), None))
    }

    /// List models from connected providers, or select one as the active model.
    #[tool(
        description = "List models from connected providers; pass `select` to persist the active model (overrides the harness's model; empty string clears)"
    )]
    fn models(&self, Parameters(params): Parameters<ModelsParams>) -> Result<String, McpError> {
        match params.select.as_deref() {
            Some("") => crate::cli::model_result(&self.config_path, Some("clear"))
                .map_err(|e| McpError::internal_error(e.to_string(), None)),
            Some(selected) => crate::cli::model_result(&self.config_path, Some(selected))
                .map_err(|e| McpError::internal_error(e.to_string(), None)),
            None => {
                let list = crate::cli::models_list(&self.config_path)
                    .map_err(|e| McpError::internal_error(e.to_string(), None))?;
                if list.is_empty() {
                    Ok("no providers are connected".to_string())
                } else {
                    Ok(list.join("\n"))
                }
            }
        }
    }
}

/// Run the MCP stdio server for `config_path`, blocking until it exits.
///
/// # Errors
///
/// Returns a [`crate::cli::CliError`] if the server cannot be started or the
/// runtime fails.
pub async fn run(config_path: PathBuf) -> miette::Result<()> {
    let server = LocalProxyServer::new(config_path);
    let service = server
        .serve(stdio())
        .await
        .map_err(|e| crate::cli::CliError::Connect {
            message: format!("falha ao iniciar MCP server: {e}"),
        })?;
    service
        .waiting()
        .await
        .map_err(|e| crate::cli::CliError::Connect {
            message: format!("MCP server encerrou com erro: {e}"),
        })?;
    Ok(())
}
