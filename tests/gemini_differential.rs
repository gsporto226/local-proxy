//! Differential test of the `gemini` codec against `CLIProxyAPI`.
//!
//! Each `tests/fixtures/gemini/<name>.anthropic.json` request and
//! `<name>.gemini.json` response has a golden written by `CLIProxyAPI`'s
//! translator (`tests/gemini-reference/generate.sh`). Both sides are reduced
//! to the fields the upstream or the client acts on, so formatting, ids and
//! field order don't count as differences.

use std::path::{Path, PathBuf};

use local_proxy::domain::ir::{self, Format};
use serde_json::{json, Value};

fn fixtures(suffix: &str) -> Vec<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/gemini");
    let mut out: Vec<PathBuf> = std::fs::read_dir(dir)
        .expect("fixture dir")
        .map(|e| e.expect("entry").path())
        .filter(|p| p.to_string_lossy().ends_with(suffix))
        .collect();
    out.sort();
    assert!(!out.is_empty(), "no *{suffix} fixtures");
    out
}

fn read(path: &Path) -> Value {
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| {
        panic!(
            "{}: {e}; run tests/gemini-reference/generate.sh",
            path.display()
        )
    });
    serde_json::from_str(&text).expect("json")
}

fn golden(path: &Path, from: &str, to: &str) -> Value {
    read(&PathBuf::from(path.to_string_lossy().replace(from, to)))
}

/// What the Gemini upstream acts on in a request body.
fn request_view(r: &Value) -> Value {
    let parts = |c: &Value| -> Vec<Value> {
        c["parts"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|p| {
                p["text"]
                    .as_str()
                    .map(|t| json!(["text", t]))
                    .or_else(|| {
                        p.get("inlineData")
                            .map(|d| json!(["image", d["mimeType"], d["data"]]))
                    })
                    .or_else(|| {
                        p.get("functionCall")
                            .map(|f| json!(["call", f["name"], f["args"], f["id"]]))
                    })
                    .or_else(|| {
                        p.get("functionResponse")
                            .map(|f| json!(["result", f["name"], f["response"], f["id"]]))
                    })
            })
            .collect()
    };
    let contents: Vec<Value> = r["contents"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|c| json!({"role": c["role"], "parts": parts(c)}))
        .collect();
    let system: Vec<Value> = parts(&r["systemInstruction"]);
    let tools: Vec<Value> = r["tools"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|t| {
            t["functionDeclarations"]
                .as_array()
                .cloned()
                .unwrap_or_default()
        })
        .map(|f| {
            let schema = f
                .get("parametersJsonSchema")
                .or_else(|| f.get("parameters"));
            json!([f["name"], f["description"], schema])
        })
        .collect();
    let g = &r["generationConfig"];
    json!({
        "system": system,
        "contents": contents,
        "tools": tools,
        "tool_config": r["toolConfig"]["functionCallingConfig"],
        "max_tokens": g["maxOutputTokens"],
        "temperature": g["temperature"],
        "stop": g["stopSequences"],
        "safety": r["safetySettings"],
    })
}

/// Fields we send on purpose that `CLIProxyAPI` drops: images, the output cap
/// and stop sequences. Removed from our side before comparing.
fn without_reference_gaps(mut view: Value) -> Value {
    view["max_tokens"] = Value::Null;
    view["stop"] = Value::Null;
    for content in view["contents"].as_array_mut().into_iter().flatten() {
        if let Some(parts) = content["parts"].as_array_mut() {
            parts.retain(|p| p[0] != "image");
        }
    }
    view
}

/// What the Anthropic client acts on in a response body.
fn response_view(r: &Value) -> Value {
    let content: Vec<Value> = r["content"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|b| match b["type"].as_str() {
            Some("tool_use") => json!(["tool_use", b["name"], b["input"]]),
            Some("thinking") => json!(["thinking", b["thinking"]]),
            other => json!([other, b["text"]]),
        })
        .collect();
    json!({
        "content": content,
        "stop_reason": r["stop_reason"],
        "input_tokens": r["usage"]["input_tokens"],
        "output_tokens": r["usage"]["output_tokens"],
    })
}

fn without_thinking(mut view: Value) -> Value {
    if let Some(content) = view["content"].as_array_mut() {
        content.retain(|b| b[0] != "thinking");
    }
    view
}

#[test]
fn requests_match_cliproxyapi() {
    for path in fixtures(".anthropic.json") {
        let mut input = read(&path);
        input["model"] = json!("gemini-2.5-pro");
        let ours = ir::translate_request(Format::Anthropic, Format::Gemini, input).expect("ours");
        let reference = golden(&path, ".anthropic.json", ".reference-request.json");
        assert_eq!(
            without_reference_gaps(request_view(&ours["request"])),
            request_view(&reference),
            "{}",
            path.display()
        );
    }
}

#[test]
fn responses_match_cliproxyapi() {
    for path in fixtures(".gemini.json") {
        let decoded = ir::decode_response(Format::Gemini, &read(&path));
        let ours = ir::encode_response(Format::Anthropic, &decoded);
        let reference = golden(&path, ".gemini.json", ".reference-response.json");
        assert_eq!(
            response_view(&ours),
            // Our non-streamed Anthropic encoder drops unsigned thinking on
            // purpose (`openai_response_to_anthropic`); the reference keeps it.
            without_thinking(response_view(&reference)),
            "{}",
            path.display()
        );
    }
}
