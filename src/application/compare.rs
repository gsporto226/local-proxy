//! `local-proxy compare`: direct versus proxy-prepared requests.
//!
//! Sends one request directly and after the proxy's request preparation
//! through the same account, and reports how the two differ (structure and
//! cache usage only, never prompt contents).

use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use serde::Serialize;
use serde_json::Value;
use thiserror::Error;

use crate::application::{runtime, streams};
use crate::domain::config::Provider;
use crate::domain::ir::Format;
use crate::domain::router::Router;
use crate::domain::sse;
use crate::domain::translate::{self, TokenUsage};
use crate::ports::{Account, ClientHints, Ports, UpstreamError, UpstreamRequest, UpstreamResponse};

const SAFE_PATH_KEYS: &[&str] = &[
    "model",
    "messages",
    "input",
    "content",
    "text",
    "role",
    "system",
    "instructions",
    "tools",
    "tool_choice",
    "name",
    "arguments",
    "parameters",
    "description",
    "type",
    "cache_control",
    "ttl",
    "prompt_cache_key",
    "output_config",
    "stream",
    "store",
    "max_tokens",
    "max_output_tokens",
    "temperature",
    "top_p",
    "reasoning",
    "effort",
    "stop_sequences",
];

/// List paths whose values differ without returning any request values.
#[must_use]
pub fn request_diff(direct: &Value, proxied: &Value) -> Vec<String> {
    fn visit(path: &str, left: &Value, right: &Value, changed: &mut Vec<String>) {
        if left == right {
            return;
        }
        match (left, right) {
            (Value::Object(left), Value::Object(right)) => {
                let keys: std::collections::BTreeSet<_> = left.keys().chain(right.keys()).collect();
                for key in keys {
                    let display = if SAFE_PATH_KEYS.contains(&key.as_str()) {
                        key.as_str()
                    } else {
                        "<field>"
                    };
                    let child = if path.is_empty() {
                        display.to_string()
                    } else {
                        format!("{path}.{display}")
                    };
                    visit(
                        &child,
                        left.get(key).unwrap_or(&Value::Null),
                        right.get(key).unwrap_or(&Value::Null),
                        changed,
                    );
                }
            }
            (Value::Array(left), Value::Array(right)) => {
                for index in 0..left.len().max(right.len()) {
                    visit(
                        &format!("{path}[{index}]"),
                        left.get(index).unwrap_or(&Value::Null),
                        right.get(index).unwrap_or(&Value::Null),
                        changed,
                    );
                }
            }
            _ => changed.push(if path.is_empty() {
                "$".to_string()
            } else {
                path.to_string()
            }),
        }
    }

    let mut changed = Vec::new();
    visit("", direct, proxied, &mut changed);
    changed
}

/// Cache-read input tokens as a percentage of total input tokens, if reported.
#[must_use]
#[allow(clippy::cast_precision_loss)]
pub fn cache_read_percent(usage: &TokenUsage) -> Option<f64> {
    let cached = usage.cache_read?;
    (usage.input > 0).then(|| cached as f64 / usage.input as f64 * 100.0)
}

/// A safe structural overview of a request body.
#[derive(Debug, Serialize)]
pub struct RequestOverview {
    body_bytes: usize,
    top_level_fields: usize,
    cache_control_markers: usize,
    prompt_cache_key_present: bool,
    stream: Option<bool>,
}

/// Usage returned for one live comparison request.
#[derive(Debug, Serialize)]
pub struct UsageSummary {
    input_tokens: u64,
    output_tokens: u64,
    cache_read_tokens: Option<u64>,
    cache_write_tokens: u64,
    cache_hit: Option<bool>,
    cache_read_percent: Option<f64>,
}

/// Status, timing, and reported usage for one live request.
#[derive(Debug, Serialize)]
pub struct RunSummary {
    status: u16,
    latency_ms: u64,
    usage: Option<UsageSummary>,
}

/// Redacted request diff and usage results for direct and proxy-prepared arms.
#[derive(Debug, Serialize)]
pub struct ComparisonReport {
    provider: String,
    model: String,
    request_diff: Vec<String>,
    direct_request: RequestOverview,
    proxy_request: RequestOverview,
    direct: Vec<RunSummary>,
    proxied: Vec<RunSummary>,
}

/// Why a comparison could not run.
#[derive(Debug, Error)]
pub enum CompareError {
    /// `--confirm-live` was not given.
    #[error("live comparison requires --confirm-live")]
    ConfirmationRequired,
    /// `--runs 0`.
    #[error("comparison runs must be greater than zero")]
    InvalidRuns,
    /// The fixture has no `{{compare_tag}}` to isolate the cache arms.
    #[error("request fixture must contain {{compare_tag}} in a prompt prefix")]
    MissingCompareTag,
    /// The fixture's format is not the provider's native one.
    #[error("request format must match the selected provider's native format")]
    FormatMismatch,
    /// The proxy-side preparation failed.
    #[error("failed to prepare proxy request: {0}")]
    Translate(#[from] translate::TranslateError),
    /// An upstream request failed in transport.
    #[error("upstream comparison request failed: {0}")]
    Upstream(#[from] UpstreamError),
    /// The model, provider, or account could not be resolved.
    #[error("{0}")]
    Setup(String),
}

/// The provider account and model a comparison runs against.
pub struct CompareTarget {
    /// The account both arms are sent through.
    pub account: Account,
    /// The routed provider.
    pub provider: Arc<Provider>,
    /// The model sent upstream.
    pub upstream_model: String,
    /// The reasoning effort the route pins, if any.
    pub reasoning_effort: Option<String>,
    /// The persisted active effort, if any.
    pub active_effort: Option<String>,
}

/// Resolve `model` to a provider account: `account`, else the persisted
/// selection, else the provider's only account. The provider must speak
/// `format` natively.
///
/// # Errors
///
/// Returns [`CompareError::Setup`] or [`CompareError::FormatMismatch`] when
/// the model, provider, or account cannot be used.
pub fn resolve_target(
    ports: &Ports,
    config_path: &Path,
    model: &str,
    format: Format,
    account: Option<String>,
) -> Result<CompareTarget, CompareError> {
    let setup = |message: String| CompareError::Setup(message);
    let config = runtime::effective_config(ports, config_path).map_err(|e| setup(e.to_string()))?;
    let router = Router::new(Arc::new(config.clone()))
        .map_err(|error| setup(format!("failed to build model router: {error}")))?;
    let resolved = router
        .resolve_model(model, &|_| false)
        .map_err(|error| setup(format!("failed to resolve model: {error}")))?;
    if Format::from(resolved.provider.format) != format {
        return Err(setup(
            "--format must match the selected provider's native request format".to_string(),
        ));
    }
    let accounts = runtime::build_accounts(ports, &config)
        .map_err(|error| setup(format!("failed to build upstream clients: {error}")))?;
    let name = &resolved.provider.name;
    let accounts = accounts
        .get(name)
        .ok_or_else(|| setup(format!("no clients for provider {name}")))?;
    let alias = account
        .or_else(|| config.defaults.active_accounts.get(name).cloned())
        .or_else(|| {
            (accounts.len() == 1)
                .then(|| accounts.keys().next().cloned())
                .flatten()
        })
        .ok_or_else(|| {
            setup(format!(
                "provider {name} has multiple accounts; pass --account"
            ))
        })?;
    let account = accounts
        .get(&alias)
        .ok_or_else(|| setup(format!("account {alias} not found for {name}")))?;
    if !account.has_credentials() {
        return Err(setup(format!(
            "account {alias} has no credentials for {name}"
        )));
    }
    Ok(CompareTarget {
        account: account.clone(),
        provider: resolved.provider.clone(),
        upstream_model: resolved.upstream_model,
        reasoning_effort: resolved.reasoning_effort,
        active_effort: config.defaults.active_effort,
    })
}

/// Validate confirmation, repeat count, and cache-arm isolation before setup.
///
/// # Errors
///
/// Returns the [`CompareError`] naming the first failed check.
pub fn validate_live_request(
    fixture: &Value,
    runs: u32,
    confirm_live: bool,
) -> Result<(), CompareError> {
    if !confirm_live {
        return Err(CompareError::ConfirmationRequired);
    }
    if runs == 0 {
        return Err(CompareError::InvalidRuns);
    }
    let mut probe = fixture.clone();
    if !replace_compare_tag(&mut probe, "validation") {
        return Err(CompareError::MissingCompareTag);
    }
    Ok(())
}

/// Replace the required fixture placeholder in every string value.
fn replace_compare_tag(value: &mut Value, tag: &str) -> bool {
    match value {
        Value::String(text) if text.contains("{{compare_tag}}") => {
            *text = text.replace("{{compare_tag}}", tag);
            true
        }
        Value::Array(items) => {
            let mut found = false;
            for item in items {
                found |= replace_compare_tag(item, tag);
            }
            found
        }
        Value::Object(items) => {
            let mut found = false;
            for item in items.values_mut() {
                found |= replace_compare_tag(item, tag);
            }
            found
        }
        _ => false,
    }
}

fn request_overview(body: &Value) -> RequestOverview {
    fn count_markers(value: &Value) -> usize {
        match value {
            Value::Object(fields) => {
                usize::from(fields.contains_key("cache_control"))
                    + fields.values().map(count_markers).sum::<usize>()
            }
            Value::Array(items) => items.iter().map(count_markers).sum(),
            _ => 0,
        }
    }

    RequestOverview {
        body_bytes: serde_json::to_vec(body).map_or(0, |bytes| bytes.len()),
        top_level_fields: body.as_object().map_or(0, serde_json::Map::len),
        cache_control_markers: count_markers(body),
        prompt_cache_key_present: body.get("prompt_cache_key").is_some(),
        stream: body.get("stream").and_then(Value::as_bool),
    }
}

fn usage_summary(usage: TokenUsage) -> UsageSummary {
    UsageSummary {
        input_tokens: usage.input,
        output_tokens: usage.output,
        cache_read_tokens: usage.cache_read,
        cache_write_tokens: usage.cache_write,
        cache_hit: usage.cache_hit(),
        cache_read_percent: cache_read_percent(&usage),
    }
}

async fn response_usage(response: UpstreamResponse) -> Option<TokenUsage> {
    if !(200..300).contains(&response.status) {
        return None;
    }
    if !response.is_event_stream() {
        let body = serde_json::from_slice::<Value>(&sse::collect(response.body).await).ok()?;
        return body.get("usage").map(translate::parse_usage);
    }
    streams::stream_usage(response.body).await
}

/// Send one arm. Quota headers are deliberately not recorded: diagnostics
/// must not touch the stats store.
async fn send_one(
    account: &Account,
    body: Value,
    session_id: &str,
    hints: &ClientHints,
) -> Result<RunSummary, CompareError> {
    let started = Instant::now();
    let response = account
        .send(UpstreamRequest {
            body,
            client_key: None,
            session_id: session_id.to_string(),
            hints: hints.clone(),
        })
        .await?;
    let status = response.status;
    let usage = response_usage(response).await.map(usage_summary);
    let latency_ms = started.elapsed().as_millis().try_into().unwrap_or(u64::MAX);
    Ok(RunSummary {
        status,
        latency_ms,
        usage,
    })
}

/// Send direct and proxy-prepared variants through the same configured account.
///
/// # Errors
///
/// Returns a [`CompareError`] for invalid fixtures or failed requests.
#[allow(clippy::too_many_arguments)]
pub async fn run_comparison(
    account: &Account,
    client_format: Format,
    provider: &Provider,
    upstream_model: &str,
    fixture: &Value,
    active_effort: Option<&str>,
    reasoning_effort: Option<&str>,
    session_id: &str,
    runs: u32,
    direct_tag: &str,
    proxy_tag: &str,
    hints: &ClientHints,
) -> Result<ComparisonReport, CompareError> {
    validate_live_request(fixture, runs, true)?;
    if client_format != Format::from(provider.format) {
        return Err(CompareError::FormatMismatch);
    }
    let mut direct = fixture.clone();
    let mut proxied = fixture.clone();
    if !replace_compare_tag(&mut direct, direct_tag)
        || !replace_compare_tag(&mut proxied, proxy_tag)
    {
        return Err(CompareError::MissingCompareTag);
    }
    direct["model"] = serde_json::json!(upstream_model);
    let streaming = fixture.get("stream").and_then(Value::as_bool) == Some(true);
    let proxied = crate::domain::request::prepare_upstream_body(
        client_format,
        provider,
        upstream_model,
        proxied,
        active_effort,
        reasoning_effort,
        session_id,
        streaming,
    )?;
    let direct_request = request_overview(&direct);
    let proxy_request = request_overview(&proxied);
    let request_diff = request_diff(&direct, &proxied);
    let mut direct_runs = Vec::with_capacity(runs as usize);
    let mut proxy_runs = Vec::with_capacity(runs as usize);
    for _ in 0..runs {
        direct_runs.push(send_one(account, direct.clone(), session_id, hints).await?);
    }
    for _ in 0..runs {
        proxy_runs.push(send_one(account, proxied.clone(), session_id, hints).await?);
    }
    Ok(ComparisonReport {
        provider: provider.name.clone(),
        model: upstream_model.to_string(),
        request_diff,
        direct_request,
        proxy_request,
        direct: direct_runs,
        proxied: proxy_runs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{self, Write};
    use std::sync::{Arc, Mutex};

    use serde_json::json;
    use tracing_subscriber::fmt::writer::MakeWriter;

    #[derive(Clone)]
    struct SharedLogWriter(Arc<Mutex<Vec<u8>>>);

    impl<'a> MakeWriter<'a> for SharedLogWriter {
        type Writer = Self;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    impl Write for SharedLogWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn request_diff_reports_changed_paths_without_prompt_values() {
        let direct = json!({
            "model": "claude-haiku",
            "messages": [{
                "role": "user",
                "content": [{"type": "text", "text": "private direct prompt"}]
            }]
        });
        let proxied = json!({
            "model": "claude-haiku",
            "messages": [{
                "role": "user",
                "content": [{"type": "text", "text": "private proxy prompt"}]
            }]
        });

        let changed = request_diff(&direct, &proxied);
        let output = changed.join("\n");

        assert_eq!(changed, ["messages[0].content[0].text"]);
        assert!(!output.contains("private"));
    }

    #[test]
    fn cache_read_percent_uses_reported_cached_and_total_input_tokens() {
        let mut usage = TokenUsage {
            input: 80,
            cache_read: Some(10),
            ..Default::default()
        };
        assert_eq!(cache_read_percent(&usage), Some(12.5));

        usage.cache_read = Some(0);
        assert_eq!(cache_read_percent(&usage), Some(0.0));

        usage.cache_read = None;
        assert_eq!(cache_read_percent(&usage), None);

        usage.cache_read = Some(10);
        usage.input = 0;
        assert_eq!(cache_read_percent(&usage), None);
    }

    #[test]
    fn live_validation_rejects_unconfirmed_or_unisolated_fixtures() {
        let tagged = json!({"messages": [{"content": "{{compare_tag}}"}]});
        let untagged = json!({"messages": [{"content": "prompt"}]});

        assert!(validate_live_request(&tagged, 1, false).is_err());
        assert!(validate_live_request(&untagged, 1, true).is_err());
        assert!(validate_live_request(&tagged, 0, true).is_err());
        assert!(validate_live_request(&tagged, 1, true).is_ok());
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn comparison_uses_same_provider_and_keeps_prompts_out_of_report() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let config_dir = tempfile::tempdir().unwrap();
        let lock_guard = crate::TEST_STATE_LOCK.lock().unwrap();
        std::env::set_var("LOCAL_PROXY_CONFIG_DIR", config_dir.path());
        drop(lock_guard);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let mut bodies = Vec::new();
            for _ in 0..2 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                let mut buffer = [0_u8; 2048];
                let (body_start, content_length) = loop {
                    let count = socket.read(&mut buffer).await.unwrap();
                    request.extend_from_slice(&buffer[..count]);
                    if let Some(head_end) =
                        request.windows(4).position(|bytes| bytes == b"\r\n\r\n")
                    {
                        let headers = String::from_utf8_lossy(&request[..head_end]);
                        let length = headers
                            .lines()
                            .find_map(|line| {
                                let (name, value) = line.split_once(':')?;
                                name.eq_ignore_ascii_case("content-length")
                                    .then(|| value.trim().parse::<usize>().unwrap())
                            })
                            .unwrap();
                        let start = head_end + 4;
                        if request.len() >= start + length {
                            break (start, length);
                        }
                    }
                };
                bodies.push(
                    serde_json::from_slice::<Value>(
                        &request[body_start..body_start + content_length],
                    )
                    .unwrap(),
                );
                let direct_arm = bodies.last().unwrap()["system"][0]["text"]
                    .as_str()
                    .unwrap()
                    .contains("direct-isolated");
                let (content_type, response) = if direct_arm {
                    (
                        "text/event-stream",
                        format!(
                            "data: {}\n\n",
                            json!({"message": {"usage": {
                                "input_tokens": 80,
                                "output_tokens": 3,
                                "cache_read_input_tokens": 20,
                                "cache_creation_input_tokens": 0
                            }}})
                        )
                        .into_bytes(),
                    )
                } else {
                    (
                        "application/json",
                        br#"{"usage":{"input_tokens":80,"output_tokens":3,"cache_read_input_tokens":20,"cache_creation_input_tokens":0}}"#.to_vec(),
                    )
                };
                let headers = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: {content_type}\r\nanthropic-ratelimit-unified-5h-utilization: 0.5\r\nanthropic-ratelimit-unified-7d-utilization: 0.4\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    response.len()
                );
                socket.write_all(headers.as_bytes()).await.unwrap();
                socket.write_all(&response).await.unwrap();
            }
            bodies
        });
        let provider = crate::domain::config::Provider {
            name: "mock".to_string(),
            base_url: format!("http://{address}"),
            format: crate::domain::config::ProviderFormat::Anthropic,
            models: vec!["mock-model".to_string()],
            auto_model: None,
            headers: std::collections::HashMap::default(),
            session_header: None,
            oauth: None,
        };
        let client = crate::bootstrap::ports()
            .upstream
            .connect(
                &provider,
                "default",
                false,
                Some(crate::domain::account::AuthEntry::Api {
                    key: "test-only".to_string(),
                }),
            )
            .unwrap();
        let fixture = json!({
            "model": "mock/mock-model",
            "output_config": {"effort": "low"},
            "system": [{
                "type": "text",
                "text": "private prefix {{compare_tag}}",
                "cache_control": {"type": "ephemeral"}
            }],
            "messages": [{"role": "user", "content": "private message"}]
        });

        let log_bytes = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::DEBUG)
            .with_writer(SharedLogWriter(log_bytes.clone()))
            .finish();
        let subscriber_guard = tracing::subscriber::set_default(subscriber);
        let report = run_comparison(
            &client,
            crate::domain::ir::Format::Anthropic,
            &provider,
            "mock-model",
            &fixture,
            Some("low"),
            None,
            "compare-session",
            1,
            "direct-isolated",
            "proxy-isolated",
            &ClientHints::default(),
        )
        .await
        .unwrap();
        drop(subscriber_guard);
        let bodies = server.await.unwrap();
        let output = serde_json::to_string(&report).unwrap();
        let logs = String::from_utf8(log_bytes.lock().unwrap().clone()).unwrap();

        assert_eq!(bodies.len(), 2);
        assert_eq!(bodies[0]["model"], "mock-model");
        assert!(bodies[0]["system"][0]["text"]
            .as_str()
            .unwrap()
            .contains("direct-isolated"));
        assert!(bodies[1]["system"][0]["text"]
            .as_str()
            .unwrap()
            .contains("proxy-isolated"));
        assert_eq!(bodies[0]["output_config"]["effort"], "low");
        assert!(bodies[1].get("output_config").is_none());
        assert_eq!(
            report.direct[0].usage.as_ref().unwrap().cache_read_percent,
            Some(20.0)
        );
        assert_eq!(
            report.proxied[0].usage.as_ref().unwrap().cache_read_percent,
            Some(20.0)
        );
        assert!(report
            .request_diff
            .iter()
            .any(|path| path.ends_with("output_config")));
        assert!(!output.contains("private prefix"));
        assert!(!output.contains("private message"));
        assert!(!output.contains("test-only"));
        assert!(!logs.contains("private prefix"));
        assert!(!logs.contains("private message"));
        assert!(!logs.contains("test-only"));
        assert!(!config_dir.path().join("stats.db").exists());
        let lock_guard = crate::TEST_STATE_LOCK.lock().unwrap();
        std::env::remove_var("LOCAL_PROXY_CONFIG_DIR");
        drop(lock_guard);
    }
}
