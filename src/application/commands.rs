//! The `$proxy` token: commands typed into a chat that the proxy answers
//! itself, plus the model/effort writes shared with the admin API.

use std::time::Duration;

use serde_json::{json, Value};

use crate::application::runtime::{AppState, RuntimeState};
use crate::application::settings;
use crate::domain::exec::{self, ExecOutput};

/// Default number of lines `local-proxy logs` / `$proxy logs` prints.
pub const DEFAULT_LOG_LINES: usize = 50;

/// Run the `$proxy` command carried by `body`, if any, returning its captured
/// output. Returns `None` when exec is disabled or the request is not a
/// `$proxy` command.
pub async fn maybe_exec(
    app: &AppState,
    state: &RuntimeState,
    body: &Value,
    session_id: &str,
) -> Option<ExecOutput> {
    let config = &state.config.exec;
    if !config.enabled {
        return None;
    }
    let text = exec::request_text(body)?;
    let cmd = exec::split_command(&text, &config.token)?;
    let args = exec::parse_args(cmd);
    Some(match args.first().map(String::as_str) {
        Some("model") => handle_model(app, state, &args).await,
        Some("effort") => handle_effort(app, state, &args).await,
        Some("account") => handle_account(app, state, session_id, &args).await,
        Some("logs") => handle_logs(app, &args),
        _ => {
            app.ports()
                .commands
                .run(
                    &config.command,
                    &args,
                    Duration::from_secs(config.timeout_secs),
                )
                .await
        }
    })
}

/// An [`ExecOutput`] carrying `result` on stdout (code 0) or stderr (code 1).
fn exec_output(result: Result<String, String>) -> ExecOutput {
    let (stdout, stderr, code) = match result {
        Ok(msg) => (msg, String::new(), 0),
        Err(e) => (String::new(), e, 1),
    };
    ExecOutput {
        stdout,
        stderr,
        code,
        timed_out: false,
    }
}

/// The line count for `$proxy logs`: the value after `-n`/`--lines`, or the
/// default when the flag is absent or malformed.
fn logs_lines_arg(args: &[String]) -> usize {
    args.iter()
        .position(|a| a == "-n" || a == "--lines")
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(DEFAULT_LOG_LINES)
}

/// `$proxy logs [-n N]`: the tail of the proxy's own log.
fn handle_logs(app: &AppState, args: &[String]) -> ExecOutput {
    let logs = &app.ports().logs;
    exec_output(match logs.tail(logs_lines_arg(args)) {
        Ok(text) => Ok(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(format!(
            "nenhum log em {} (proxy ainda não rodou?)",
            logs.location().display()
        )),
        Err(e) => Err(e.to_string()),
    })
}

/// Publish this instance's `{ model, effort }` as a `config` event.
pub async fn emit_config(app: &AppState) {
    app.ports().events.publish("config", config_json(app).await);
}

/// This instance's `{ model, effort }`.
pub async fn config_json(app: &AppState) -> Value {
    let defaults = app.snapshot().await.config.defaults.clone();
    json!({ "model": defaults.active_model, "effort": defaults.active_effort })
}

/// Persist the reasoning effort (`level` or `clear`) and apply it to this
/// instance right away (others follow via hot-reload). Shared by
/// `$proxy effort` and `PUT /admin/effort`.
///
/// # Errors
///
/// Returns the message of an invalid level or a failed write.
pub async fn apply_effort(app: &AppState, level: &str) -> Result<String, String> {
    let config_path = app.snapshot().await.config_path;
    let msg = settings::effort_result(app.ports(), &config_path, Some(level))
        .map_err(|e| e.to_string())?;
    app.set_active_effort((level != "clear").then(|| level.to_string()))
        .await;
    emit_config(app).await;
    Ok(msg)
}

/// Validate and persist the active model (`clear` unsets it).
///
/// Also updates this instance's in-memory `active_model` (per-instance, no
/// broadcast to other running proxies). Shared by `$proxy model` and
/// `PUT /admin/model`.
///
/// # Errors
///
/// Returns the message of an unavailable model or a failed write.
pub async fn apply_model(app: &AppState, model: &str) -> Result<String, String> {
    let config_path = app.snapshot().await.config_path;
    let msg = settings::model_result(app.ports(), &config_path, Some(model))
        .map_err(|e| e.to_string())?;
    if model == "clear" {
        app.set_active_model(None).await;
    } else if msg.starts_with("modelo ativo:") {
        app.set_active_model(Some(model.to_string())).await;
    }
    emit_config(app).await;
    Ok(msg)
}

/// Select (`provider/alias`) or clear (`clear [provider]`) the persisted
/// default account, then announce the config. Shared by `PUT /admin/account`.
///
/// # Errors
///
/// Returns the message of a malformed target, unknown account, or failed write.
pub async fn apply_account(app: &AppState, args: &[String]) -> Result<String, String> {
    let config_path = app.snapshot().await.config_path;
    let msg =
        settings::account_result(app.ports(), &config_path, args).map_err(|e| e.to_string())?;
    emit_config(app).await;
    Ok(msg)
}

/// Remove one stored account, then announce the config. Shared by
/// `DELETE /admin/accounts/{provider}/{alias}`.
///
/// # Errors
///
/// Returns the message of a failed credential-store write.
pub async fn disconnect(app: &AppState, provider: &str, alias: &str) -> Result<String, String> {
    let msg =
        settings::disconnect_provider(app.ports(), provider, alias).map_err(|e| e.to_string())?;
    emit_config(app).await;
    Ok(msg)
}

/// `$proxy effort [level|clear]`.
async fn handle_effort(app: &AppState, state: &RuntimeState, args: &[String]) -> ExecOutput {
    exec_output(match args.get(1) {
        Some(level) => apply_effort(app, level).await,
        None => settings::effort_result(app.ports(), &state.config_path, None)
            .map_err(|e| e.to_string()),
    })
}

/// `$proxy model [model|clear]`: report this instance's in-memory model, or
/// set/clear it via [`apply_model`].
async fn handle_model(app: &AppState, state: &RuntimeState, args: &[String]) -> ExecOutput {
    exec_output(match args.get(1) {
        None => Ok(state.config.defaults.active_model.as_deref().map_or_else(
            || "nenhum modelo ativo".to_string(),
            |m| format!("modelo ativo: {m}"),
        )),
        Some(model) => apply_model(app, model).await,
    })
}

/// `$proxy account ...`: list accounts, switch this session to
/// `provider/alias`, or clear the selection.
///
/// Selecting pins the account to the session id that made the request and
/// persists it as the last-selected default; clearing drops the session pin
/// too. Without a session id the command only changes the persisted default.
async fn handle_account(
    app: &AppState,
    state: &RuntimeState,
    session_id: &str,
    args: &[String],
) -> ExecOutput {
    let rest = args.get(1..).unwrap_or_default();
    let result = settings::account_result(app.ports(), &state.config_path, rest);
    if result.is_ok() {
        match rest {
            [command] if command == "clear" => {
                app.clear_session_accounts(session_id, None).await;
            }
            [command, provider] if command == "clear" => {
                app.clear_session_accounts(session_id, Some(provider)).await;
            }
            [target] => {
                if let Some((provider, alias)) = target.split_once('/') {
                    app.set_session_account(session_id, provider, alias).await;
                }
            }
            _ => {}
        }
        emit_config(app).await;
    }
    exec_output(result.map_err(|e| e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::testing::empty_state;
    use crate::domain::account::AuthEntry;
    use crate::domain::config::Config;

    #[tokio::test]
    async fn maybe_exec_returns_none_when_disabled() {
        let mut cfg = Config::default();
        cfg.exec.enabled = false;
        let app = AppState::new(empty_state(cfg), crate::bootstrap::ports());
        let body = json!({"messages": [{"role": "user", "content": "$proxy status"}]});
        let state = app.snapshot().await;
        assert!(maybe_exec(&app, &state, &body, "").await.is_none());
    }

    #[tokio::test]
    async fn maybe_exec_model_get_runs_in_process() {
        // The `model` get path answers from this instance's in-memory config
        // without spawning any binary, and must not mutate the selection.
        let app = AppState::new(empty_state(Config::default()), crate::bootstrap::ports());
        let body = json!({"messages": [{"role": "user", "content": "$proxy model"}]});
        let state = app.snapshot().await;
        let out = maybe_exec(&app, &state, &body, "")
            .await
            .expect("is $proxy");
        assert!(!out.stdout.is_empty(), "stdout should not be empty");
        assert_eq!(out.code, 0);
        assert_eq!(app.snapshot().await.config.defaults.active_model, None);
    }

    #[test]
    fn maybe_exec_account_pins_the_session_and_persists_the_default() {
        let _guard = crate::TEST_STATE_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("LOCAL_PROXY_CONFIG_DIR", dir.path());
        let ports = crate::bootstrap::ports();
        ports
            .credentials
            .insert(
                "mock",
                "work",
                &AuthEntry::Api {
                    key: "key-work".to_string(),
                },
            )
            .unwrap();
        let config_path = dir.path().join("config.yaml");
        let mut state = empty_state(Config::default());
        state.config_path.clone_from(&config_path);
        let app = AppState::new(state, ports.clone());

        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let state = app.snapshot().await;

                let body =
                    json!({"messages": [{"role": "user", "content": "$proxy account mock/work"}]});
                let out = maybe_exec(&app, &state, &body, "sess-1")
                    .await
                    .expect("is $proxy");
                assert_eq!(out.code, 0, "stderr: {}", out.stderr);
                assert_eq!(
                    app.session_account("sess-1", "mock").await.as_deref(),
                    Some("work")
                );
                // The selection is also the persisted last-selected default.
                let saved = ports.config.load(&config_path).unwrap();
                assert_eq!(saved.defaults.active_accounts["mock"], "work");

                // `clear` drops the session pin and the persisted default.
                let body =
                    json!({"messages": [{"role": "user", "content": "$proxy account clear mock"}]});
                let out = maybe_exec(&app, &state, &body, "sess-1")
                    .await
                    .expect("is $proxy");
                assert_eq!(out.code, 0, "stderr: {}", out.stderr);
                assert_eq!(app.session_account("sess-1", "mock").await, None);
                let saved = ports.config.load(&config_path).unwrap();
                assert!(!saved.defaults.active_accounts.contains_key("mock"));
            });

        std::env::remove_var("LOCAL_PROXY_CONFIG_DIR");
    }

    #[test]
    fn logs_lines_arg_reads_flag_or_defaults() {
        let parse = exec::parse_args;
        assert_eq!(logs_lines_arg(&parse("logs")), DEFAULT_LOG_LINES);
        assert_eq!(logs_lines_arg(&parse("logs -n 100")), 100);
        assert_eq!(logs_lines_arg(&parse("logs --lines 3")), 3);
        assert_eq!(logs_lines_arg(&parse("logs -n nope")), DEFAULT_LOG_LINES);
    }
}
