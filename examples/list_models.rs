//! Diagnostic helper: list the models the `ChatGPT` Codex backend exposes for
//! the account in the auth store.
//!
//! Not part of the product surface. Use it when the embedded catalog's slugs
//! stop working: the Codex backend validates model ids against its own catalog,
//! which is distinct from the chat web UI's, and this prints that catalog for
//! the signed-in account.
//!
//! Run with the config dir holding the `chatgpt` OAuth entry:
//!
//! ```text
//! LOCAL_PROXY_CONFIG_DIR=~/.config/local-proxy cargo run --example list_models
//! ```

use std::time::Duration;

use local_proxy::domain::account::AuthEntry;

/// The Codex model manifest endpoint (the one the Codex CLI itself uses).
/// Distinct from `chatgpt.com/backend-api/models`, which is the *chat* web UI
/// catalog and lists slugs the Codex backend rejects.
const MODELS_URL: &str = "https://chatgpt.com/backend-api/codex/models";

/// The Codex CLI version to report. The endpoint validates this and returns a
/// manifest tailored to it.
const CLIENT_VERSION: &str = "0.157.1";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let provider = "chatgpt";
    // Uses the stored access token as is; run any proxied request first if it
    // has expired (the proxy refreshes and persists it).
    let account_alias = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "default".to_string());
    let tokens = local_proxy::bootstrap::ports()
        .credentials
        .get(provider, &account_alias)?
        .as_ref()
        .and_then(AuthEntry::oauth)
        .cloned()
        .ok_or("no chatgpt oauth account; connect with --account <alias> or pass an alias")?;
    let (token, account) = (tokens.access, tokens.account_id);

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()?;
    let mut req = client
        .get(MODELS_URL)
        .query(&[("client_version", CLIENT_VERSION)])
        .bearer_auth(&token)
        .header("originator", "codex_cli_rs")
        .header("User-Agent", "local-proxy");
    // Without the account header the endpoint answers 200 with an empty model
    // list, which reads as "no models" rather than "not authorized".
    if let Some(account) = account {
        req = req.header("chatgpt-account-id", account);
    }

    let resp = req.send().await?;
    let status = resp.status();
    let body: serde_json::Value = resp.json().await?;
    eprintln!("status {status}");
    if let Some(models) = body.get("models").and_then(serde_json::Value::as_array) {
        for model in models {
            let slug = model
                .get("slug")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            let visibility = model
                .get("visibility")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("-");
            let efforts: Vec<&str> = model
                .get("supported_reasoning_levels")
                .and_then(serde_json::Value::as_array)
                .map(|levels| {
                    levels
                        .iter()
                        .filter_map(|l| l.get("effort").and_then(serde_json::Value::as_str))
                        .collect()
                })
                .unwrap_or_default();
            println!(
                "{slug}\tvisibility={visibility}\tefforts={}",
                efforts.join(",")
            );
        }
    } else {
        println!("{body}");
    }
    Ok(())
}
