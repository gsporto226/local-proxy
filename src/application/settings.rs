//! User settings use cases.
//!
//! The active model, effort and account selections, connected providers and
//! their accounts. Shared by the CLI, the `$proxy` commands, and the admin
//! API, which all report the same messages.
#![allow(clippy::result_large_err)]

use std::path::Path;

use miette::Diagnostic;
use thiserror::Error;

use crate::application::runtime::{self, RuntimeError};
use crate::domain::account::{AuthEntry, OAuthTokens};
use crate::domain::config::{qualified_id, Config, ConfigError, OAuthProvider};
use crate::ports::{CredentialError, Ports};

/// Errors from the settings use cases.
#[derive(Debug, Error, Diagnostic)]
pub enum SettingsError {
    /// Configuration could not be loaded, parsed, or written.
    #[error("failed to load configuration")]
    #[diagnostic(
        code(cli::config),
        help("check that the config file exists and is valid YAML/JSON")
    )]
    Config(#[from] ConfigError),

    /// The account store could not be read or written.
    #[error("auth store error: {0}")]
    #[diagnostic(code(cli::auth))]
    Credentials(#[from] CredentialError),

    /// The request is invalid (unknown model, account, provider, level...).
    #[error("{message}")]
    #[diagnostic(code(cli::connect))]
    Invalid {
        /// Human-readable description of the problem.
        message: String,
    },
}

impl From<RuntimeError> for SettingsError {
    fn from(e: RuntimeError) -> Self {
        match e {
            RuntimeError::Config { source, .. } => Self::Config(source),
            RuntimeError::Auth(source) => Self::Credentials(source),
            other => Self::Invalid {
                message: other.to_string(),
            },
        }
    }
}

fn invalid(message: impl Into<String>) -> SettingsError {
    SettingsError::Invalid {
        message: message.into(),
    }
}

/// The effective config (catalog merged with the overlay at `config_path`).
///
/// # Errors
///
/// Returns [`SettingsError::Config`] if the overlay or catalog is invalid.
pub fn effective_config(ports: &Ports, config_path: &Path) -> Result<Config, SettingsError> {
    Ok(runtime::effective_config(ports, config_path)?)
}

/// Models available from providers with a usable credential, in provider
/// config order with duplicates removed.
///
/// # Errors
///
/// Returns a [`SettingsError`] if the config or account store cannot be read.
pub fn connected_models(ports: &Ports, config_path: &Path) -> Result<Vec<String>, SettingsError> {
    let mut config = effective_config(ports, config_path)?;
    let auth = ports.credentials.read_all()?;
    ports.upstream.discover_models(&mut config, &auth);
    let mut models = Vec::new();
    for provider in &config.providers {
        if !auth
            .get(&provider.name)
            .is_some_and(|accounts| accounts.values().any(AuthEntry::usable))
        {
            continue;
        }
        for model in &provider.models {
            let qualified = qualified_id(&provider.name, model);
            if !models.contains(&qualified) {
                models.push(qualified);
            }
        }
        if provider.auto_model.is_some() {
            let qualified = qualified_id(&provider.name, "auto");
            if !models.contains(&qualified) {
                models.push(qualified);
            }
        }
    }
    Ok(models)
}

/// One stored account of a provider, as listed by `providers`/`account` and
/// `GET /admin/accounts`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct AccountInfo {
    /// Provider name.
    pub provider: String,
    /// Account alias.
    pub alias: String,
    /// Credential kind: `api` or `oauth`.
    pub kind: &'static str,
    /// Whether this alias is the persisted default (`defaults.active_accounts`).
    pub is_default: bool,
}

/// One effective provider (catalog and config) with its stored accounts.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ProviderInfo {
    /// Provider name.
    pub name: String,
    /// Wire format of the provider.
    pub format: String,
    /// Stored accounts, sorted by alias.
    pub accounts: Vec<AccountInfo>,
}

/// The effective providers with their stored accounts.
///
/// # Errors
///
/// Returns a [`SettingsError`] if the config or account store cannot be read.
pub fn provider_accounts(
    ports: &Ports,
    config_path: &Path,
) -> Result<Vec<ProviderInfo>, SettingsError> {
    let config = effective_config(ports, config_path)?;
    let auth = ports.credentials.read_all()?;
    Ok(config
        .providers
        .iter()
        .map(|p| {
            let mut accounts: Vec<AccountInfo> = auth
                .get(&p.name)
                .into_iter()
                .flat_map(|entries| entries.iter())
                .map(|(alias, entry)| AccountInfo {
                    provider: p.name.clone(),
                    alias: alias.clone(),
                    kind: entry.kind(),
                    is_default: config.defaults.active_accounts.get(&p.name) == Some(alias),
                })
                .collect();
            accounts.sort_unstable_by(|a, b| a.alias.cmp(&b.alias));
            ProviderInfo {
                name: p.name.clone(),
                format: p.format.to_string(),
                accounts,
            }
        })
        .collect())
}

/// The stored accounts, one per line, marking each provider's active alias.
///
/// # Errors
///
/// Returns a [`SettingsError`] if the config or account store cannot be read.
pub fn list_accounts(ports: &Ports, config_path: &Path) -> Result<String, SettingsError> {
    let lines: Vec<String> = provider_accounts(ports, config_path)?
        .into_iter()
        .flat_map(|p| p.accounts)
        .map(|a| {
            let marker = if a.is_default { " [ativa]" } else { "" };
            format!("{}/{} ({}){marker}", a.provider, a.alias, a.kind)
        })
        .collect();
    if lines.is_empty() {
        return Ok(
            "nenhuma conta salva; use `local-proxy connect <provider> --account <alias>`"
                .to_string(),
        );
    }
    Ok(lines.join("\n"))
}

/// Persist `defaults.active_model` (`None` clears it) so the selection
/// survives restarts.
///
/// # Errors
///
/// Returns a [`SettingsError`] if the config cannot be written.
pub fn set_default_model(
    ports: &Ports,
    config_path: &Path,
    model: Option<&str>,
) -> Result<(), SettingsError> {
    ports.config.update(config_path, &mut |config| {
        config.defaults.active_model = model.map(str::to_string);
    })?;
    Ok(())
}

/// Persist or clear (`None`) the active account alias for `provider`.
///
/// # Errors
///
/// Returns a [`SettingsError`] if the config cannot be written.
pub fn set_default_account(
    ports: &Ports,
    config_path: &Path,
    provider: &str,
    alias: Option<&str>,
) -> Result<(), SettingsError> {
    ports
        .config
        .update(config_path, &mut |config| match alias {
            Some(alias) => {
                config
                    .defaults
                    .active_accounts
                    .insert(provider.to_string(), alias.to_string());
            }
            None => {
                config.defaults.active_accounts.remove(provider);
            }
        })?;
    Ok(())
}

/// Reasoning effort levels accepted by `local-proxy effort`.
pub const EFFORT_LEVELS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

/// Show (`None`), set, or `clear` the reasoning effort the proxy forces on
/// every request, returning the message to print. Running proxies pick the
/// change up via hot-reload.
///
/// # Errors
///
/// Returns an error if the level is unknown or the config cannot be written.
pub fn effort_result(
    ports: &Ports,
    config_path: &Path,
    level: Option<&str>,
) -> Result<String, SettingsError> {
    let Some(level) = level else {
        return Ok(effective_config(ports, config_path)?
            .defaults
            .active_effort
            .unwrap_or_else(|| "none (o cliente decide)".to_string()));
    };
    let value = if level == "clear" {
        None
    } else if EFFORT_LEVELS.contains(&level) {
        Some(level.to_string())
    } else {
        return Err(invalid(format!(
            "effort '{level}' invalido; use: {} ou clear",
            EFFORT_LEVELS.join(", ")
        )));
    };
    ports.config.update(config_path, &mut |config| {
        config.defaults.active_effort.clone_from(&value);
    })?;
    Ok(value.map_or_else(
        || "effort ativo removido".to_string(),
        |v| format!("effort ativo: {v}"),
    ))
}

/// Get, set, or `clear` the active model, returning the message to print.
///
/// With no `model`, returns the effective active model (the selected one, else
/// the first model available from a connected provider, else "none"). With a
/// model name, validates it is available from a connected provider and
/// persists it. When no provider is connected and a model is requested,
/// returns `"no providers are connected"`.
///
/// # Errors
///
/// Returns a [`SettingsError`] if the config cannot be loaded or written, or
/// the requested model is not available from a connected provider.
pub fn model_result(
    ports: &Ports,
    config_path: &Path,
    model: Option<&str>,
) -> Result<String, SettingsError> {
    match model {
        Some("clear") => {
            set_default_model(ports, config_path, None)?;
            Ok("modelo ativo removido".to_string())
        }
        Some(selected) => {
            let connected = connected_models(ports, config_path)?;
            if connected.is_empty() {
                return Ok("no providers are connected".to_string());
            }
            if !connected.iter().any(|m| m == selected) {
                return Err(invalid(format!(
                    "model '{selected}' nao disponivel em um provider conectado; \
                     disponiveis: {}",
                    connected.join(", ")
                )));
            }
            set_default_model(ports, config_path, Some(selected))?;
            Ok(format!("modelo ativo: {selected}"))
        }
        None => {
            let config = effective_config(ports, config_path)?;
            if let Some(m) = config.defaults.active_model {
                return Ok(m);
            }
            Ok(connected_models(ports, config_path)?
                .into_iter()
                .next()
                .map_or_else(
                    || "none".to_string(),
                    |m| format!("{m} (nenhum selecionado; usando o primeiro disponivel)"),
                ))
        }
    }
}

/// List accounts (no args), select one (`provider/alias`), or clear the
/// selection (`clear [provider]`), returning the message to print. Running
/// proxies pick the change up via hot-reload.
///
/// # Errors
///
/// Returns a [`SettingsError`] if the target is malformed, the account does
/// not exist, or the config cannot be written.
pub fn account_result(
    ports: &Ports,
    config_path: &Path,
    args: &[String],
) -> Result<String, SettingsError> {
    match args {
        [] => list_accounts(ports, config_path),
        [command, provider] if command == "clear" => {
            set_default_account(ports, config_path, provider, None)?;
            Ok(format!("conta ativa do provider '{provider}' limpa"))
        }
        [command] if command == "clear" => {
            let config = effective_config(ports, config_path)?;
            if config.defaults.active_accounts.is_empty() {
                return Ok("nenhuma conta ativa para limpar".to_string());
            }
            ports.config.update(config_path, &mut |config| {
                config.defaults.active_accounts.clear();
            })?;
            Ok("contas ativas limpas".to_string())
        }
        [target] => {
            let Some((provider, alias)) = target.split_once('/') else {
                return Err(invalid(format!(
                    "alvo '{target}' invalido; use provider/alias (ex.: chatgpt/work)"
                )));
            };
            let auth = ports.credentials.read_all()?;
            if !auth
                .get(provider)
                .is_some_and(|accounts| accounts.contains_key(alias))
            {
                return Err(invalid(format!(
                    "conta '{alias}' nao encontrada para o provider '{provider}'; \
                     veja `local-proxy account`"
                )));
            }
            set_default_account(ports, config_path, provider, Some(alias))?;
            Ok(format!("conta ativa do provider '{provider}': {alias}"))
        }
        _ => Err(invalid(
            "uso: `local-proxy account [provider/alias | clear [provider]]`",
        )),
    }
}

/// Remove one stored account, returning the message to print.
///
/// # Errors
///
/// Returns [`SettingsError::Credentials`] if the store cannot be written.
pub fn disconnect_provider(
    ports: &Ports,
    provider: &str,
    account: &str,
) -> Result<String, SettingsError> {
    let removed = ports.credentials.remove(provider, account)?;
    Ok(if removed {
        format!("conta '{account}' do provider '{provider}' removida")
    } else {
        format!("nenhuma conta '{account}' salva para o provider '{provider}'")
    })
}

fn check_connect_target(
    ports: &Ports,
    config_path: &Path,
    provider: &str,
    account: &str,
) -> Result<crate::domain::config::Provider, SettingsError> {
    if account.trim().is_empty() {
        return Err(invalid("account alias must not be empty"));
    }
    effective_config(ports, config_path)?
        .providers
        .into_iter()
        .find(|p| p.name == provider)
        .ok_or_else(|| {
            invalid(format!(
                "provider '{provider}' nao existe no catalogo nem no config; \
                 use `local-proxy providers` ou adicione-o no config"
            ))
        })
}

/// The OAuth recipe for connecting `account` of `provider`, after checking
/// the provider exists and the alias is not empty.
///
/// # Errors
///
/// Returns [`SettingsError::Invalid`] if the provider is unknown or has no
/// `oauth:` block.
pub fn oauth_recipe(
    ports: &Ports,
    config_path: &Path,
    provider: &str,
    account: &str,
) -> Result<OAuthProvider, SettingsError> {
    check_connect_target(ports, config_path, provider, account)?
        .oauth
        .ok_or_else(|| {
            invalid(format!(
                "provider '{provider}' nao tem bloco `oauth:` no config; \
                 adicione um ou use `connect {provider} <chave>`"
            ))
        })
}

/// Check that `provider` exists and `account` is a non-empty alias before
/// asking the user for a key.
///
/// # Errors
///
/// Returns [`SettingsError::Invalid`] for an unknown provider or empty alias.
pub fn check_api_key_target(
    ports: &Ports,
    config_path: &Path,
    provider: &str,
    account: &str,
) -> Result<(), SettingsError> {
    check_connect_target(ports, config_path, provider, account).map(|_| ())
}

/// Store an API key as `account` of `provider`, returning the message to print.
///
/// # Errors
///
/// Returns a [`SettingsError`] for invalid aliases, duplicates, or storage
/// failures.
pub fn save_api_key(
    ports: &Ports,
    provider: &str,
    account: &str,
    key: &str,
) -> Result<String, SettingsError> {
    ports.credentials.insert(
        provider,
        account,
        &AuthEntry::Api {
            key: key.trim().to_string(),
        },
    )?;
    Ok(format!(
        "chave da conta '{account}' do provider '{provider}' salva no banco criptografado"
    ))
}

/// Store OAuth tokens as `account` of `provider`, returning the message to print.
///
/// # Errors
///
/// Returns a [`SettingsError`] for invalid aliases, duplicates, or storage
/// failures.
pub fn save_oauth(
    ports: &Ports,
    provider: &str,
    account: &str,
    tokens: OAuthTokens,
) -> Result<String, SettingsError> {
    let expires = tokens.expires;
    ports
        .credentials
        .insert(provider, account, &AuthEntry::OAuth(tokens))?;
    Ok(format!(
        "oauth da conta '{account}' do provider '{provider}' conectado no banco criptografado (access token expira {})",
        remaining_lifetime(expires)
    ))
}

/// Render a token's remaining lifetime in coarse units (`~7h`, `~12d`).
fn remaining_lifetime(expires_ms: i64) -> String {
    let remaining = expires_ms.saturating_sub(crate::domain::oauth::now_ms()) / 1000;
    if remaining >= 86_400 {
        format!("em ~{}d", remaining / 86_400)
    } else if remaining >= 3_600 {
        format!("em ~{}h", remaining / 3_600)
    } else {
        format!("em ~{}min", (remaining / 60).max(1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_default_model_persists_and_clears() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        let ports = crate::bootstrap::ports();

        set_default_model(&ports, &path, Some("deepseek-v4-flash")).expect("persist model");
        let config = ports.config.load(&path).expect("reload");
        assert_eq!(
            config.defaults.active_model.as_deref(),
            Some("deepseek-v4-flash")
        );

        set_default_model(&ports, &path, None).expect("clear model");
        let config = ports.config.load(&path).expect("reload");
        assert_eq!(config.defaults.active_model, None);
    }

    #[test]
    fn set_default_account_persists_and_clears() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        let ports = crate::bootstrap::ports();

        set_default_account(&ports, &path, "chatgpt", Some("work")).expect("persist account");
        set_default_account(&ports, &path, "opencode-go", Some("personal"))
            .expect("persist second");
        let config = ports.config.load(&path).expect("reload");
        assert_eq!(config.defaults.active_accounts["chatgpt"], "work");
        assert_eq!(config.defaults.active_accounts["opencode-go"], "personal");

        set_default_account(&ports, &path, "chatgpt", None).expect("clear one");
        let config = ports.config.load(&path).expect("reload");
        assert!(!config.defaults.active_accounts.contains_key("chatgpt"));
        assert_eq!(config.defaults.active_accounts["opencode-go"], "personal");
    }

    #[test]
    fn effort_rejects_unknown_levels() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        let ports = crate::bootstrap::ports();
        let err = effort_result(&ports, &path, Some("extreme")).unwrap_err();
        assert!(err.to_string().contains("invalido"), "{err}");
        assert!(!path.exists(), "an invalid level must not write the config");
        assert_eq!(
            effort_result(&ports, &path, Some("high")).unwrap(),
            "effort ativo: high"
        );
    }

    #[test]
    fn remaining_lifetime_uses_coarse_units() {
        let now = crate::domain::oauth::now_ms();
        assert_eq!(remaining_lifetime(now + 3 * 86_400_000 + 5_000), "em ~3d");
        assert_eq!(remaining_lifetime(now + 2 * 3_600_000 + 5_000), "em ~2h");
        assert_eq!(remaining_lifetime(now), "em ~1min");
    }
}
