//! Anthropic Messages API codec.

use serde_json::{json, Map, Value};

use super::{
    args_value, empty_schema, ext_rest, new_id, stops, str_of, text_of, usage_from, Block,
    BlockKind, Cache, Effort, Emitter, Event, Image, Message, Part, Request, Response, Role,
    StopReason, StreamDecoder, StreamEncoder, Tool, ToolCall, ToolChoice, ToolResult,
    DEFAULT_MAX_TOKENS,
};
use crate::domain::sse::SseFrame;
use crate::domain::translate::{anthropic_usage, TokenUsage, TranslateError};

const NS: &str = "anthropic";

// ---------------------------------------------------------------------------
// request
// ---------------------------------------------------------------------------

fn cache_of(v: &Value) -> Option<Cache> {
    v.get("cache_control").map(|c| Cache {
        ttl: c.get("ttl").and_then(Value::as_str).map(str::to_string),
    })
}

fn cache_json(cache: &Cache) -> Value {
    cache.ttl.as_ref().map_or_else(
        || json!({"type": "ephemeral"}),
        |ttl| json!({"type": "ephemeral", "ttl": ttl}),
    )
}

fn decode_block(b: &Value) -> Option<Block> {
    let part = match str_of(b, "type") {
        "text" => Part::Text(str_of(b, "text").to_string()),
        "image" => {
            let src = b.get("source")?;
            Part::Image(match str_of(src, "type") {
                "base64" => Image::Base64 {
                    media_type: src
                        .get("media_type")
                        .and_then(Value::as_str)
                        .unwrap_or("image/png")
                        .to_string(),
                    data: str_of(src, "data").to_string(),
                },
                "url" => Image::Url(str_of(src, "url").to_string()),
                _ => return None,
            })
        }
        "tool_use" => Part::ToolCall(ToolCall {
            id: str_of(b, "id").to_string(),
            name: str_of(b, "name").to_string(),
            arguments: args_value(b.get("input")),
        }),
        "tool_result" => Part::ToolResult(ToolResult {
            call_id: b
                .get("tool_use_id")
                .or_else(|| b.get("tool_call_id"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            content: text_of(b.get("content").unwrap_or(&Value::Null)),
            is_error: b.get("is_error").and_then(Value::as_bool).unwrap_or(false),
        }),
        "thinking" => Part::Reasoning {
            text: str_of(b, "thinking").to_string(),
            signature: b
                .get("signature")
                .and_then(Value::as_str)
                .map(str::to_string),
        },
        _ => return None,
    };
    Some(Block {
        part,
        cache: cache_of(b),
    })
}

fn decode_blocks(content: &Value) -> Vec<Block> {
    match content {
        Value::String(s) => vec![Block::new(Part::Text(s.clone()))],
        Value::Array(arr) => arr.iter().filter_map(decode_block).collect(),
        _ => Vec::new(),
    }
}

fn decode_tool(t: &Value) -> Option<Tool> {
    match t.get("type").and_then(Value::as_str) {
        None | Some("custom") => Some(Tool::Function {
            name: str_of(t, "name").to_string(),
            description: str_of(t, "description").to_string(),
            schema: t.get("input_schema").cloned().unwrap_or_else(empty_schema),
            cache: cache_of(t),
        }),
        Some(ty) if ty.starts_with("web_search") => Some(Tool::WebSearch),
        Some(_) => None,
    }
}

/// Decode an Anthropic Messages request.
///
/// # Errors
///
/// Returns [`TranslateError`] when the body is not an object or `messages` is
/// not an array.
pub fn decode_request(body: Value) -> Result<Request, TranslateError> {
    let Value::Object(obj) = body else {
        return Err(TranslateError::NotObject);
    };
    let mut req = Request {
        model: obj
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        ..Request::default()
    };
    if let Some(sys) = obj.get("system") {
        req.system = decode_blocks(sys)
            .into_iter()
            .filter(|b| matches!(b.part, Part::Text(_)))
            .collect();
    }
    if let Some(msgs) = obj.get("messages") {
        let arr = msgs.as_array().ok_or_else(|| TranslateError::Invalid {
            field: "messages",
            detail: "expected array".to_string(),
        })?;
        for m in arr {
            let role = match str_of(m, "role") {
                "assistant" => Role::Assistant,
                "user" => Role::User,
                // lenient clients that already speak the OpenAI tool role
                "tool" => {
                    req.messages.push(Message {
                        role: Role::User,
                        content: vec![Block::new(Part::ToolResult(ToolResult {
                            call_id: str_of(m, "tool_call_id").to_string(),
                            content: text_of(m.get("content").unwrap_or(&Value::Null)),
                            is_error: false,
                        }))],
                    });
                    continue;
                }
                _ => continue,
            };
            req.messages.push(Message {
                role,
                content: decode_blocks(m.get("content").unwrap_or(&Value::Null)),
            });
        }
    }
    if let Some(tools) = obj.get("tools").and_then(Value::as_array) {
        req.tools = tools.iter().filter_map(decode_tool).collect();
    }
    req.tool_choice = obj.get("tool_choice").map(|tc| match str_of(tc, "type") {
        "none" => ToolChoice::None,
        "any" => ToolChoice::Required,
        "tool" => ToolChoice::Named(str_of(tc, "name").to_string()),
        _ => ToolChoice::Auto,
    });
    // The API requires it; fill its usual default for lenient clients.
    req.max_tokens = Some(
        obj.get("max_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_MAX_TOKENS),
    );
    req.temperature = obj.get("temperature").and_then(Value::as_f64);
    req.top_p = obj.get("top_p").and_then(Value::as_f64);
    req.stop = stops(obj.get("stop_sequences"));
    req.stream = obj.get("stream").and_then(Value::as_bool) == Some(true);
    // `output_config.effort` is Anthropic's; the rest are spellings non-Claude
    // clients put on an Anthropic-shaped request.
    req.effort = [
        "output_config",
        "thinking",
        "reasoning",
        "reasoning_effort",
        "effort",
        "level",
        "depth",
    ]
    .iter()
    .find_map(|k| obj.get(*k).and_then(Effort::parse));
    req.ext = ext_rest(
        obj,
        &[
            "model",
            "system",
            "messages",
            "tools",
            "tool_choice",
            "max_tokens",
            "temperature",
            "top_p",
            "stop_sequences",
            "stream",
        ],
        NS,
    );
    Ok(req)
}

fn encode_block(b: &Block) -> Option<Value> {
    let mut v = match &b.part {
        Part::Text(t) if t.is_empty() => return None,
        Part::Text(t) => json!({"type": "text", "text": t}),
        Part::Image(Image::Base64 { media_type, data }) => json!({
            "type": "image",
            "source": {"type": "base64", "media_type": media_type, "data": data}
        }),
        Part::Image(Image::Url(url)) => {
            json!({"type": "image", "source": {"type": "url", "url": url}})
        }
        Part::ToolCall(c) => json!({
            "type": "tool_use",
            "id": c.id,
            "name": c.name,
            "input": if c.arguments.is_object() { c.arguments.clone() } else { json!({}) }
        }),
        Part::ToolResult(r) => {
            let mut v = json!({
                "type": "tool_result",
                "tool_use_id": r.call_id,
                "content": r.content
            });
            if r.is_error {
                v["is_error"] = json!(true);
            }
            v
        }
        // Anthropic rejects thinking blocks without their signature.
        Part::Reasoning {
            text,
            signature: Some(sig),
        } => json!({"type": "thinking", "thinking": text, "signature": sig}),
        Part::Reasoning { .. } => return None,
    };
    if let Some(c) = b.cache.as_ref().map(cache_json) {
        v["cache_control"] = c;
    }
    Some(v)
}

/// Encode an Anthropic Messages request.
#[must_use]
pub fn encode_request(req: &Request) -> Value {
    let mut out = Map::new();
    out.insert("model".into(), json!(req.model));
    // A plain string unless a block carries a cache breakpoint.
    if req.system.iter().any(|b| b.cache.is_some()) {
        let system: Vec<Value> = req.system.iter().filter_map(encode_block).collect();
        out.insert("system".into(), Value::Array(system));
    } else {
        let text: Vec<&str> = req
            .system
            .iter()
            .filter_map(|b| match &b.part {
                Part::Text(t) if !t.is_empty() => Some(t.as_str()),
                _ => None,
            })
            .collect();
        if !text.is_empty() {
            out.insert(
                "system".into(),
                json!(text.join(
                    "

"
                )),
            );
        }
    }
    let messages: Vec<Value> = req
        .messages
        .iter()
        .filter_map(|m| {
            let content: Vec<Value> = m.content.iter().filter_map(encode_block).collect();
            (!content.is_empty()).then(|| {
                json!({
                    "role": if m.role == Role::Assistant { "assistant" } else { "user" },
                    "content": content
                })
            })
        })
        .collect();
    out.insert("messages".into(), Value::Array(messages));
    let tools: Vec<Value> = req
        .tools
        .iter()
        .map(|t| match t {
            Tool::Function {
                name,
                description,
                schema,
                cache,
            } => {
                let mut v = json!({
                    "name": name,
                    "description": description,
                    "input_schema": if schema.is_object() { schema.clone() } else { empty_schema() }
                });
                if let Some(c) = cache.as_ref().map(cache_json) {
                    v["cache_control"] = c;
                }
                v
            }
            // Stays a client tool: the client (not Anthropic) runs the search.
            Tool::WebSearch => json!({
                "name": "web_search",
                "description": "Search the web",
                "input_schema": {
                    "type": "object",
                    "properties": {"query": {"type": "string"}},
                    "required": ["query"]
                }
            }),
        })
        .collect();
    if !tools.is_empty() {
        out.insert("tools".into(), Value::Array(tools));
    }
    if let Some(tc) = &req.tool_choice {
        out.insert(
            "tool_choice".into(),
            match tc {
                ToolChoice::Auto => json!({"type": "auto"}),
                ToolChoice::None => json!({"type": "none"}),
                ToolChoice::Required => json!({"type": "any"}),
                ToolChoice::Named(n) => json!({"type": "tool", "name": n}),
            },
        );
    }
    out.insert(
        "max_tokens".into(),
        json!(req.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS)),
    );
    if let Some(t) = req.temperature {
        out.insert("temperature".into(), json!(t));
    }
    if let Some(p) = req.top_p {
        out.insert("top_p".into(), json!(p));
    }
    if !req.stop.is_empty() {
        out.insert("stop_sequences".into(), json!(req.stop));
    }
    if req.stream {
        out.insert("stream".into(), json!(true));
    }
    Value::Object(out)
}

// ---------------------------------------------------------------------------
// response
// ---------------------------------------------------------------------------

fn stop_from(s: &str) -> StopReason {
    match s {
        "tool_use" => StopReason::ToolUse,
        "max_tokens" | "max_tokens_reached" => StopReason::MaxTokens,
        "stop_sequence" => StopReason::StopSequence,
        _ => StopReason::EndTurn,
    }
}

const fn stop_str(s: StopReason) -> &'static str {
    match s {
        StopReason::EndTurn => "end_turn",
        StopReason::MaxTokens => "max_tokens",
        StopReason::ToolUse => "tool_use",
        StopReason::StopSequence => "stop_sequence",
    }
}

/// Decode an Anthropic Messages response.
#[must_use]
pub fn decode_response(body: &Value) -> Response {
    Response {
        id: str_of(body, "id").to_string(),
        model: str_of(body, "model").to_string(),
        content: decode_blocks(body.get("content").unwrap_or(&Value::Null))
            .into_iter()
            .map(|b| b.part)
            .collect(),
        stop: stop_from(str_of(body, "stop_reason")),
        created: None,
        usage: usage_from(body.get("usage").unwrap_or(&Value::Null)),
        ext: super::Ext::default(),
    }
}

/// Encode an Anthropic Messages response.
#[must_use]
pub fn encode_response(resp: &Response) -> Value {
    let content: Vec<Value> = resp
        .content
        .iter()
        .filter_map(|p| encode_block(&Block::new(p.clone())))
        .collect();
    json!({
        "id": if resp.id.is_empty() { new_id("msg") } else { resp.id.clone() },
        "type": "message",
        "role": "assistant",
        "model": resp.model,
        "content": content,
        "stop_reason": stop_str(resp.stop),
        "stop_sequence": null,
        "usage": anthropic_usage(&resp.usage)
    })
}

// ---------------------------------------------------------------------------
// stream
// ---------------------------------------------------------------------------

/// Anthropic SSE -> IR events.
#[derive(Debug, Default)]
pub struct Decoder {
    em: Emitter,
    /// upstream block index -> IR index
    blocks: Vec<(u64, u32)>,
    stop: Option<StopReason>,
}

impl Decoder {
    fn ir_index(&self, upstream: u64) -> Option<u32> {
        self.blocks
            .iter()
            .find(|(u, _)| *u == upstream)
            .map(|(_, i)| *i)
    }
}

impl StreamDecoder for Decoder {
    fn frame(&mut self, frame: &SseFrame) -> Vec<Event> {
        let mut out = Vec::new();
        let Some(v) = frame.json() else {
            return out;
        };
        let upstream = v.get("index").and_then(Value::as_u64).unwrap_or(0);
        match str_of(&v, "type") {
            "message_start" => {
                let msg = v.get("message").unwrap_or(&Value::Null);
                self.em
                    .start(&mut out, str_of(msg, "id"), str_of(msg, "model"));
                if let Some(u) = msg.get("usage") {
                    self.em.usage(usage_from(u));
                }
            }
            "content_block_start" => {
                let b = v.get("content_block").unwrap_or(&Value::Null);
                let kind = match str_of(b, "type") {
                    "text" => BlockKind::Text,
                    "thinking" => BlockKind::Reasoning,
                    "tool_use" => BlockKind::ToolCall {
                        id: str_of(b, "id").to_string(),
                        name: str_of(b, "name").to_string(),
                    },
                    _ => return out,
                };
                let index = self.em.open(&mut out, kind);
                self.blocks.push((upstream, index));
            }
            "content_block_delta" => {
                let d = v.get("delta").unwrap_or(&Value::Null);
                let text = match str_of(d, "type") {
                    "text_delta" => str_of(d, "text"),
                    "thinking_delta" => str_of(d, "thinking"),
                    "input_json_delta" => str_of(d, "partial_json"),
                    _ => return out,
                };
                let index = match self.ir_index(upstream) {
                    Some(i) => i,
                    // tolerate a text delta without its content_block_start
                    None if str_of(d, "type") == "text_delta" => {
                        let i = self.em.open(&mut out, BlockKind::Text);
                        self.blocks.push((upstream, i));
                        i
                    }
                    None => return out,
                };
                Emitter::delta(&mut out, index, text);
            }
            "content_block_stop" => {
                if let Some(i) = self.ir_index(upstream) {
                    self.em.close(&mut out, i);
                }
            }
            "message_delta" => {
                if let Some(sr) = v
                    .get("delta")
                    .and_then(|d| d.get("stop_reason"))
                    .and_then(Value::as_str)
                {
                    self.stop = Some(stop_from(sr));
                }
                if let Some(u) = v.get("usage") {
                    self.em.usage(usage_from(u));
                }
            }
            _ => {}
        }
        out
    }

    fn finish(&mut self) -> Vec<Event> {
        self.em.finish(self.stop)
    }
}

/// IR events -> Anthropic SSE.
#[derive(Debug)]
pub struct Encoder {
    model: String,
    usage: TokenUsage,
    started: bool,
    kinds: Vec<(u32, BlockKind)>,
}

impl Encoder {
    /// Encoder reporting `model` to the client.
    #[must_use]
    pub fn new(model: &str) -> Self {
        Self {
            model: model.to_string(),
            usage: TokenUsage::default(),
            started: false,
            kinds: Vec::new(),
        }
    }

    fn kind(&self, index: u32) -> Option<&BlockKind> {
        self.kinds.iter().find(|(i, _)| *i == index).map(|(_, k)| k)
    }

    fn start(&mut self, id: &str, out: &mut Vec<Value>) {
        if self.started {
            return;
        }
        self.started = true;
        let id = if id.is_empty() {
            new_id("msg")
        } else {
            id.to_string()
        };
        out.push(json!({
            "type": "message_start",
            "message": {
                "id": id,
                "type": "message",
                "role": "assistant",
                "model": self.model,
                "content": [],
                "stop_reason": null,
                "stop_sequence": null,
                "usage": anthropic_usage(&self.usage)
            }
        }));
    }
}

impl StreamEncoder for Encoder {
    fn event(&mut self, event: &Event) -> Vec<Value> {
        let mut out = Vec::new();
        if let Event::Start { id, .. } = event {
            self.start(id, &mut out);
            return out;
        }
        self.start("", &mut out);
        match event {
            Event::BlockStart { index, kind } => {
                let block = match kind {
                    BlockKind::Text => json!({"type": "text", "text": ""}),
                    BlockKind::Reasoning => json!({"type": "thinking", "thinking": ""}),
                    BlockKind::ToolCall { id, name } => {
                        json!({"type": "tool_use", "id": id, "name": name, "input": {}})
                    }
                };
                out.push(
                    json!({"type": "content_block_start", "index": index, "content_block": block}),
                );
                self.kinds.push((*index, kind.clone()));
            }
            Event::Delta { index, text } => {
                let delta = match self.kind(*index) {
                    Some(BlockKind::Reasoning) => {
                        json!({"type": "thinking_delta", "thinking": text})
                    }
                    Some(BlockKind::ToolCall { .. }) => {
                        json!({"type": "input_json_delta", "partial_json": text})
                    }
                    _ => json!({"type": "text_delta", "text": text}),
                };
                out.push(json!({"type": "content_block_delta", "index": index, "delta": delta}));
            }
            Event::BlockStop { index } => {
                out.push(json!({"type": "content_block_stop", "index": index}));
            }
            Event::Usage(u) => self.usage = *u,
            Event::Finish(stop) => {
                out.push(json!({
                    "type": "message_delta",
                    "delta": {"stop_reason": stop_str(*stop), "stop_sequence": null},
                    "usage": anthropic_usage(&self.usage)
                }));
                out.push(json!({"type": "message_stop"}));
            }
            Event::Start { .. } => {}
        }
        out
    }
}
