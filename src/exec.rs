//! Local command execution for the `$proxy` token.
//!
//! When a request's last user message starts with the configured token (by
//! default `$proxy`), the proxy runs the remainder as a
//! [`crate::config::Exec::command`] invocation instead of forwarding the
//! request upstream. This module provides the pure helpers for token
//! detection, argument parsing, and running the command; the HTTP handlers
//! orchestrate interception and response synthesis.

use std::fmt::Write as _;
use std::time::Duration;

use serde_json::Value;
use tokio::io::AsyncReadExt;

/// The captured result of running a `$proxy` command.
#[derive(Debug, Clone)]
pub struct ExecOutput {
    /// Captured standard output.
    pub stdout: String,
    /// Captured standard error (or a spawn/timeout diagnostic).
    pub stderr: String,
    /// The process exit code, or `127`/`124` for spawn/timeout failures.
    pub code: i32,
    /// Whether the command was killed because it exceeded the timeout.
    pub timed_out: bool,
}

/// Split `text` on a leading `token` followed by whitespace, returning the
/// command remainder (trimmed).
///
/// Returns `Some("")` when `text` is exactly the token, and `None` when the
/// token is not a distinct leading word (so `$proxyz` does not match `$proxy`).
#[must_use]
pub fn split_command<'a>(text: &'a str, token: &str) -> Option<&'a str> {
    let rest = text.strip_prefix(token)?;
    if rest.is_empty() {
        return Some("");
    }
    if rest.starts_with(char::is_whitespace) {
        return Some(rest.trim_start());
    }
    None
}

/// Extract the text of the last `user` message from a message array.
///
/// Handles `Anthropic` and `OpenAI` `messages`/`input` string and content-block
/// forms. Returns `None` if there is no usable user text.
#[must_use]
pub fn last_user_text(messages: &Value) -> Option<String> {
    let arr = messages.as_array()?;
    for msg in arr.iter().rev() {
        if msg.get("role").and_then(Value::as_str) != Some("user") {
            continue;
        }
        if let Some(text) = content_text(msg.get("content")) {
            return Some(text);
        }
    }
    None
}

/// Concatenate the text of a content value that is either a string or an array
/// of `{ "type": "text", "text": ... }` blocks.
#[must_use]
fn content_text(content: Option<&Value>) -> Option<String> {
    match content {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Array(blocks)) => {
            let mut out = String::new();
            for block in blocks {
                if block.get("type").and_then(Value::as_str) == Some("text") {
                    if let Some(t) = block.get("text").and_then(Value::as_str) {
                        out.push_str(t);
                    }
                }
            }
            if out.is_empty() {
                None
            } else {
                Some(out)
            }
        }
        _ => None,
    }
}

/// Extract the last `user` message text from a request body.
///
/// Checks the `Anthropic`/`OpenAI` `messages` array first, then the Responses
/// `input` (a plain string or an array of messages).
#[must_use]
pub fn request_text(body: &Value) -> Option<String> {
    if let Some(messages) = body.get("messages") {
        if let Some(text) = last_user_text(messages) {
            return Some(text);
        }
    }
    if let Some(input) = body.get("input") {
        match input {
            Value::String(s) => return Some(s.clone()),
            Value::Array(_) => return last_user_text(input),
            _ => {}
        }
    }
    None
}

/// Split a command string into arguments, honoring double quotes and treating
/// any other whitespace as a separator. Never invokes a shell.
#[must_use]
pub fn parse_args(input: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut started = false;
    for c in input.chars() {
        match c {
            '"' => {
                in_quotes = !in_quotes;
                started = true;
            }
            c if c.is_whitespace() && !in_quotes => {
                if started {
                    args.push(std::mem::take(&mut current));
                    started = false;
                }
            }
            c => {
                current.push(c);
                started = true;
            }
        }
    }
    if started {
        args.push(current);
    }
    args
}

/// Run `command` with `args`, capturing stdout/stderr and enforcing `timeout`
/// (killing the child on expiry). `stdin` is closed so interactive prompts
/// cannot hang the proxy.
///
/// # Errors
///
/// Never returns an error; failures (spawn, timeout, non-zero exit) are
/// reported through [`ExecOutput`] fields.
pub async fn run(command: &str, args: &[String], timeout: Duration) -> ExecOutput {
    let mut child = match tokio::process::Command::new(command)
        .args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .stdin(std::process::Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(e) => {
            return ExecOutput {
                stdout: String::new(),
                stderr: format!("failed to spawn '{command}': {e}"),
                code: 127,
                timed_out: false,
            };
        }
    };

    let mut out_pipe = child.stdout.take();
    let mut err_pipe = child.stderr.take();

    let wait = async {
        let status = child.wait().await;
        let mut stdout = String::new();
        let mut stderr = String::new();
        if let Some(mut pipe) = out_pipe.take() {
            let _ = pipe.read_to_string(&mut stdout).await;
        }
        if let Some(mut pipe) = err_pipe.take() {
            let _ = pipe.read_to_string(&mut stderr).await;
        }
        (status, stdout, stderr)
    };

    if let Ok((status, stdout, stderr)) = tokio::time::timeout(timeout, wait).await {
        ExecOutput {
            stdout,
            stderr,
            code: status.map_or(1, |s| s.code().unwrap_or(1)),
            timed_out: false,
        }
    } else {
        let _ = child.kill().await;
        let _ = child.wait().await;
        ExecOutput {
            stdout: String::new(),
            stderr: format!("command timed out after {timeout:?}"),
            code: 124,
            timed_out: true,
        }
    }
}

/// Render an [`ExecOutput`] into a single readable text block, folding exit
/// code and diagnostics into the message returned to the model.
#[must_use]
pub fn format_output(out: &ExecOutput) -> String {
    let mut text = String::new();
    if !out.stdout.is_empty() {
        text.push_str(&out.stdout);
    }
    if !out.stderr.is_empty() {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&out.stderr);
    }
    if out.timed_out {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        let _ = write!(text, "(timed out, exit code {})", out.code);
    } else if out.code != 0 {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        let _ = write!(text, "(exit code {})", out.code);
    }
    if text.trim().is_empty() {
        text = "(no output)".to_string();
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_command_matches_token_and_space() {
        assert_eq!(split_command("$proxy status", "$proxy"), Some("status"));
        assert_eq!(
            split_command("$proxy stats --since week", "$proxy"),
            Some("stats --since week")
        );
        assert_eq!(split_command("$proxy", "$proxy"), Some(""));
        assert_eq!(split_command("$proxyz status", "$proxy"), None);
        assert_eq!(split_command("normal message", "$proxy"), None);
    }

    #[test]
    fn split_command_custom_token() {
        assert_eq!(split_command("!exec models", "!exec"), Some("models"));
        assert_eq!(split_command("!exec", "!exec"), Some(""));
        assert_eq!(split_command("!execx", "!exec"), None);
    }

    #[test]
    fn parse_args_handles_quotes_and_spaces() {
        assert_eq!(parse_args("model \"gpt 4o\""), vec!["model", "gpt 4o"]);
        assert_eq!(
            parse_args("stats --since week"),
            vec!["stats", "--since", "week"]
        );
        assert_eq!(parse_args(""), Vec::<String>::new());
        assert_eq!(parse_args("  connect openai  "), vec!["connect", "openai"]);
    }

    #[test]
    fn last_user_text_extracts_string_and_blocks() {
        let v = serde_json::json!([
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": "hello"},
            {"role": "user", "content": [
                {"type": "text", "text": "$proxy "},
                {"type": "text", "text": "models"}
            ]}
        ]);
        assert_eq!(last_user_text(&v).as_deref(), Some("$proxy models"));
    }

    #[test]
    fn last_user_text_none_when_no_user() {
        let v = serde_json::json!([{"role": "assistant", "content": "hello"}]);
        assert_eq!(last_user_text(&v), None);
    }

    #[test]
    fn request_text_prefers_messages_then_input() {
        let body = serde_json::json!({
            "messages": [{"role": "user", "content": "$proxy status"}]
        });
        assert_eq!(request_text(&body).as_deref(), Some("$proxy status"));

        let body = serde_json::json!({
            "input": "$proxy models"
        });
        assert_eq!(request_text(&body).as_deref(), Some("$proxy models"));

        let body = serde_json::json!({
            "input": [{"role": "user", "content": [{"type": "text", "text": "$proxy stats"}]}]
        });
        assert_eq!(request_text(&body).as_deref(), Some("$proxy stats"));
    }

    #[test]
    fn format_output_combines_streams_and_exit_code() {
        let out = ExecOutput {
            stdout: "line1\nline2".to_string(),
            stderr: String::new(),
            code: 0,
            timed_out: false,
        };
        assert_eq!(format_output(&out), "line1\nline2");

        let out = ExecOutput {
            stdout: "ok".to_string(),
            stderr: "boom".to_string(),
            code: 1,
            timed_out: false,
        };
        assert_eq!(format_output(&out), "ok\nboom\n(exit code 1)");

        let out = ExecOutput {
            stdout: String::new(),
            stderr: String::new(),
            code: 124,
            timed_out: true,
        };
        assert_eq!(format_output(&out), "(timed out, exit code 124)");

        let out = ExecOutput {
            stdout: String::new(),
            stderr: String::new(),
            code: 0,
            timed_out: false,
        };
        assert_eq!(format_output(&out), "(no output)");
    }
}
