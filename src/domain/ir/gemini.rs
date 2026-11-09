//! Gemini `generateContent` codec, as the Code Assist backend
//! (`cloudcode-pa.googleapis.com/v1internal`) behind Google AI Pro/Ultra
//! subscriptions speaks it.
//!
//! Upstream-only: clients never send this format, so there is no request
//! decoder or response/stream encoder. The request is wrapped in the Code
//! Assist envelope `{model, request}`; the upstream adds `project`. Stream
//! frames arrive wrapped as `{response: {...}}` and are unwrapped here.

use std::collections::HashMap;

use serde_json::{json, Map, Value};

use super::{
    fold, new_id, Block, BlockKind, Effort, Emitter, Event, Image, Part, Request, Response, Role,
    StopReason, StreamDecoder, Tool, ToolChoice, DEFAULT_MAX_TOKENS,
};
use crate::domain::sse::SseFrame;
use crate::domain::translate::TokenUsage;

/// Placeholder Gemini 3 accepts in place of a real `thoughtSignature` on
/// replayed function calls.
// ponytail: signatures do not survive the IR stream events, so every replayed
// call gets the bypass value; carry real signatures if Google stops honoring it.
const SKIP_SIGNATURE: &str = "skip_thought_signature_validator";

/// Encode `req` as a Code Assist `streamGenerateContent` body.
#[must_use]
pub fn encode_request(req: &Request) -> Value {
    // functionResponse needs the tool name; IR results only carry the call id.
    let names: HashMap<&str, &str> = req
        .messages
        .iter()
        .flat_map(|m| &m.content)
        .filter_map(|b| match &b.part {
            Part::ToolCall(c) => Some((c.id.as_str(), c.name.as_str())),
            _ => None,
        })
        .collect();
    let contents: Vec<Value> = req
        .messages
        .iter()
        .filter_map(|m| {
            let parts: Vec<Value> = m
                .content
                .iter()
                .filter_map(|b| part(&b.part, &names))
                .collect();
            let role = match m.role {
                Role::User => "user",
                Role::Assistant => "model",
            };
            (!parts.is_empty()).then(|| json!({"role": role, "parts": parts}))
        })
        .collect();

    let mut inner = Map::new();
    inner.insert("contents".into(), json!(contents));
    let system = system_text(&req.system);
    if !system.is_empty() {
        inner.insert(
            "systemInstruction".into(),
            json!({"parts": [{"text": system}]}),
        );
    }
    let functions: Vec<Value> = req
        .tools
        .iter()
        .filter_map(|t| match t {
            Tool::Function {
                name,
                description,
                schema,
                ..
            } => Some(json!({
                "name": name,
                "description": description,
                "parametersJsonSchema": schema,
            })),
            // Google Search cannot be combined with function declarations.
            Tool::WebSearch => None,
        })
        .collect();
    if !functions.is_empty() {
        inner.insert(
            "tools".into(),
            json!([{ "functionDeclarations": functions }]),
        );
        if let Some(choice) = &req.tool_choice {
            let config = match choice {
                ToolChoice::Auto => json!({"mode": "AUTO"}),
                ToolChoice::None => json!({"mode": "NONE"}),
                ToolChoice::Required => json!({"mode": "ANY"}),
                ToolChoice::Named(n) => json!({"mode": "ANY", "allowedFunctionNames": [n]}),
            };
            inner.insert(
                "toolConfig".into(),
                json!({ "functionCallingConfig": config }),
            );
        }
    }
    let mut generation = Map::new();
    generation.insert(
        "maxOutputTokens".into(),
        json!(req.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS)),
    );
    if let Some(t) = req.temperature {
        generation.insert("temperature".into(), json!(t));
    }
    if let Some(p) = req.top_p {
        generation.insert("topP".into(), json!(p));
    }
    if !req.stop.is_empty() {
        generation.insert("stopSequences".into(), json!(req.stop));
    }
    generation.insert(
        "thinkingConfig".into(),
        thinking_config(&req.model, req.effort),
    );
    inner.insert("generationConfig".into(), Value::Object(generation));
    // Coding traffic trips the default filters (CLIProxyAPI sends the same).
    inner.insert(
        "safetySettings".into(),
        json!([
            {"category": "HARM_CATEGORY_HARASSMENT", "threshold": "OFF"},
            {"category": "HARM_CATEGORY_HATE_SPEECH", "threshold": "OFF"},
            {"category": "HARM_CATEGORY_SEXUALLY_EXPLICIT", "threshold": "OFF"},
            {"category": "HARM_CATEGORY_DANGEROUS_CONTENT", "threshold": "OFF"},
            {"category": "HARM_CATEGORY_CIVIC_INTEGRITY", "threshold": "BLOCK_NONE"},
        ]),
    );

    json!({ "model": req.model, "request": inner })
}

/// Gemini 3 takes a `thinkingLevel`; 2.5 takes a token budget.
fn thinking_config(model: &str, effort: Option<Effort>) -> Value {
    let mut config = json!({"includeThoughts": true});
    let Some(effort) = effort else {
        return config;
    };
    if model.starts_with("gemini-2") {
        let budget = match effort {
            Effort::Minimal => 128,
            Effort::Low => 1024,
            Effort::Medium => 8192,
            Effort::High | Effort::Max => 24576,
        };
        config["thinkingBudget"] = json!(budget);
    } else {
        let level = match effort {
            Effort::Minimal | Effort::Low => "low",
            Effort::Medium | Effort::High | Effort::Max => "high",
        };
        config["thinkingLevel"] = json!(level);
    }
    config
}

fn system_text(system: &[Block]) -> String {
    system
        .iter()
        .filter_map(|b| match &b.part {
            Part::Text(t) => Some(t.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn part(p: &Part, names: &HashMap<&str, &str>) -> Option<Value> {
    Some(match p {
        Part::Text(t) if t.is_empty() => return None,
        Part::Text(t) => json!({"text": t}),
        Part::Image(Image::Base64 { media_type, data }) => {
            json!({"inlineData": {"mimeType": media_type, "data": data}})
        }
        Part::Image(Image::Url(url)) => json!({"fileData": {"fileUri": url}}),
        Part::ToolCall(c) => json!({
            "functionCall": {"name": c.name, "args": c.arguments, "id": c.id},
            "thoughtSignature": SKIP_SIGNATURE,
        }),
        Part::ToolResult(r) => {
            let name = names.get(r.call_id.as_str()).copied().unwrap_or("tool");
            let key = if r.is_error { "error" } else { "result" };
            json!({"functionResponse": {
                "name": name,
                "response": {key: r.content},
                "id": r.call_id,
            }})
        }
        // Other providers' reasoning cannot be replayed to Gemini.
        Part::Reasoning { .. } => return None,
    })
}

/// Decode a non-streamed (or wrapped) `generateContent` body.
#[must_use]
pub fn decode_response(body: &Value) -> Response {
    let mut decoder = Decoder::default();
    let mut events = decoder.chunk(body);
    events.extend(decoder.finish());
    fold(&events)
}

/// Stream decoder for `streamGenerateContent?alt=sse`.
#[derive(Debug, Default)]
pub struct Decoder {
    emitter: Emitter,
    /// The open text/reasoning block: (index, is reasoning).
    current: Option<(u32, bool)>,
    stop: Option<StopReason>,
}

impl Decoder {
    fn chunk(&mut self, value: &Value) -> Vec<Event> {
        let value = value.get("response").unwrap_or(value);
        let mut out = Vec::new();
        let id = value
            .get("responseId")
            .and_then(Value::as_str)
            .unwrap_or("");
        let model = value
            .get("modelVersion")
            .and_then(Value::as_str)
            .unwrap_or("");
        self.emitter.start(&mut out, id, model);
        let candidate = value.pointer("/candidates/0");
        let parts = candidate
            .and_then(|c| c.pointer("/content/parts"))
            .and_then(Value::as_array);
        for p in parts.into_iter().flatten() {
            if let Some(call) = p.get("functionCall") {
                self.close_current(&mut out);
                let id = call
                    .get("id")
                    .and_then(Value::as_str)
                    .map_or_else(|| new_id("call"), str::to_string);
                let name = super::str_of(call, "name").to_string();
                let index = self
                    .emitter
                    .open(&mut out, BlockKind::ToolCall { id, name });
                let args = call.get("args").cloned().unwrap_or_else(|| json!({}));
                Emitter::delta(&mut out, index, &args.to_string());
                self.emitter.close(&mut out, index);
            } else if let Some(text) = p.get("text").and_then(Value::as_str) {
                let thought = p.get("thought").and_then(Value::as_bool) == Some(true);
                let index = match self.current {
                    Some((index, kind)) if kind == thought => index,
                    _ => {
                        self.close_current(&mut out);
                        let kind = if thought {
                            BlockKind::Reasoning
                        } else {
                            BlockKind::Text
                        };
                        let index = self.emitter.open(&mut out, kind);
                        self.current = Some((index, thought));
                        index
                    }
                };
                Emitter::delta(&mut out, index, text);
            }
        }
        // STOP also ends tool-call turns; the emitter picks ToolUse then.
        if candidate
            .and_then(|c| c.get("finishReason"))
            .and_then(Value::as_str)
            == Some("MAX_TOKENS")
        {
            self.stop = Some(StopReason::MaxTokens);
        }
        if let Some(meta) = value.get("usageMetadata") {
            let n = |k: &str| meta.get(k).and_then(Value::as_u64).unwrap_or(0);
            let reasoning = n("thoughtsTokenCount");
            self.emitter.usage(TokenUsage {
                input: n("promptTokenCount"),
                output: n("candidatesTokenCount") + reasoning,
                reasoning,
                cache_read: meta.get("cachedContentTokenCount").and_then(Value::as_u64),
                ..TokenUsage::default()
            });
        }
        out
    }

    fn close_current(&mut self, out: &mut Vec<Event>) {
        if let Some((index, _)) = self.current.take() {
            self.emitter.close(out, index);
        }
    }
}

impl StreamDecoder for Decoder {
    fn frame(&mut self, frame: &SseFrame) -> Vec<Event> {
        frame.json().map_or_else(Vec::new, |v| self.chunk(&v))
    }

    fn finish(&mut self) -> Vec<Event> {
        self.emitter.finish(self.stop)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::ir::{Message, ToolCall, ToolResult};

    #[test]
    fn encodes_envelope_tools_and_tool_results() {
        let req = Request {
            model: "gemini-3-pro-preview".into(),
            system: vec![Block::new(Part::Text("be brief".into()))],
            messages: vec![
                Message {
                    role: Role::User,
                    content: vec![Block::new(Part::Text("hi".into()))],
                },
                Message {
                    role: Role::Assistant,
                    content: vec![Block::new(Part::ToolCall(ToolCall {
                        id: "c1".into(),
                        name: "ls".into(),
                        arguments: json!({"p": "."}),
                    }))],
                },
                Message {
                    role: Role::User,
                    content: vec![Block::new(Part::ToolResult(ToolResult {
                        call_id: "c1".into(),
                        content: "a.txt".into(),
                        is_error: false,
                    }))],
                },
            ],
            tools: vec![Tool::Function {
                name: "ls".into(),
                description: "list".into(),
                schema: json!({"type": "object"}),
                cache: None,
            }],
            effort: Some(Effort::Low),
            ..Request::default()
        };
        let body = encode_request(&req);
        assert_eq!(body["model"], "gemini-3-pro-preview");
        let r = &body["request"];
        assert_eq!(r["systemInstruction"]["parts"][0]["text"], "be brief");
        assert_eq!(r["contents"][1]["role"], "model");
        assert_eq!(r["contents"][1]["parts"][0]["functionCall"]["name"], "ls");
        assert_eq!(
            r["contents"][2]["parts"][0]["functionResponse"],
            json!({"name": "ls", "response": {"result": "a.txt"}, "id": "c1"})
        );
        assert_eq!(r["tools"][0]["functionDeclarations"][0]["name"], "ls");
        assert_eq!(
            r["generationConfig"]["thinkingConfig"]["thinkingLevel"],
            "low"
        );
    }

    #[test]
    fn decodes_wrapped_stream_with_thoughts_text_and_calls() {
        let frames = [
            json!({"response": {"responseId": "r1", "modelVersion": "gemini-3-pro",
                "candidates": [{"content": {"parts": [{"text": "hmm", "thought": true}]}}]}}),
            json!({"response": {"candidates": [{"content": {"parts": [{"text": "ok"}]}}]}}),
            json!({"response": {"candidates": [{"content": {"parts": [
                {"functionCall": {"name": "ls", "args": {"p": "."}}}]}, "finishReason": "STOP"}],
                "usageMetadata": {"promptTokenCount": 10, "candidatesTokenCount": 3,
                    "thoughtsTokenCount": 2}}}),
        ];
        let mut d = Decoder::default();
        let mut events: Vec<Event> = frames.iter().flat_map(|f| d.chunk(f)).collect();
        events.extend(d.finish());
        let resp = fold(&events);
        assert_eq!(resp.id, "r1");
        assert_eq!(resp.stop, StopReason::ToolUse);
        assert_eq!(resp.usage.input, 10);
        assert_eq!(resp.usage.output, 5);
        assert!(matches!(&resp.content[0], Part::Reasoning { text, .. } if text == "hmm"));
        assert_eq!(resp.content[1], Part::Text("ok".into()));
        assert!(
            matches!(&resp.content[2], Part::ToolCall(c) if c.name == "ls" && c.arguments == json!({"p": "."}))
        );
    }
}
