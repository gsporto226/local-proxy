//! `OpenAI` Chat Completions codec.

use serde_json::{json, Map, Value};

use super::{
    args_string, args_value, empty_schema, ext_rest, new_id, now_ts, push_message, stops, str_of,
    text_of, usage_from, Block, BlockKind, Effort, Emitter, Event, Image, Part, Request, Response,
    Role, StopReason, StreamDecoder, StreamEncoder, Tool, ToolCall, ToolChoice, ToolResult,
};
use crate::sse::SseFrame;
use crate::translate::{openai_usage, TokenUsage, TranslateError};

const NS: &str = "openai";

// ---------------------------------------------------------------------------
// request
// ---------------------------------------------------------------------------

fn decode_user_content(content: &Value) -> Vec<Block> {
    match content {
        Value::String(s) => vec![Block::new(Part::Text(s.clone()))],
        Value::Array(arr) => arr
            .iter()
            .filter_map(|b| match str_of(b, "type") {
                "text" => Some(Part::Text(str_of(b, "text").to_string())),
                "image_url" => {
                    let url = b
                        .get("image_url")
                        .and_then(|u| u.get("url").or(Some(u)))
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    Some(Part::Image(Image::from_url(url)))
                }
                _ => None,
            })
            .map(Block::new)
            .collect(),
        _ => Vec::new(),
    }
}

fn decode_tool_calls(m: &Value) -> Vec<Block> {
    m.get("tool_calls")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|tc| {
            let f = tc.get("function").unwrap_or(&Value::Null);
            Block::new(Part::ToolCall(ToolCall {
                id: str_of(tc, "id").to_string(),
                name: str_of(f, "name").to_string(),
                arguments: args_value(f.get("arguments")),
            }))
        })
        .collect()
}

/// Decode an `OpenAI` Chat Completions request.
///
/// # Errors
///
/// Returns [`TranslateError`] when the body is not an object or `messages` is
/// not an array.
#[allow(clippy::too_many_lines)]
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
    if let Some(msgs) = obj.get("messages") {
        let arr = msgs.as_array().ok_or_else(|| TranslateError::Invalid {
            field: "messages",
            detail: "expected array".to_string(),
        })?;
        for m in arr {
            let content = m.get("content").unwrap_or(&Value::Null);
            match str_of(m, "role") {
                "system" | "developer" => {
                    let text = text_of(content);
                    if !text.is_empty() {
                        req.system.push(Block::new(Part::Text(text)));
                    }
                }
                "user" => push_message(&mut req.messages, Role::User, decode_user_content(content)),
                "assistant" => {
                    let mut blocks = Vec::new();
                    if let Some(r) = m.get("reasoning_content").and_then(Value::as_str) {
                        if !r.is_empty() {
                            blocks.push(Block::new(Part::Reasoning {
                                text: r.to_string(),
                                signature: None,
                            }));
                        }
                    }
                    let text = text_of(content);
                    if !text.is_empty() {
                        blocks.push(Block::new(Part::Text(text)));
                    }
                    blocks.extend(decode_tool_calls(m));
                    push_message(&mut req.messages, Role::Assistant, blocks);
                }
                "tool" => push_message(
                    &mut req.messages,
                    Role::User,
                    vec![Block::new(Part::ToolResult(ToolResult {
                        call_id: str_of(m, "tool_call_id").to_string(),
                        content: text_of(content),
                        is_error: false,
                    }))],
                ),
                _ => {}
            }
        }
    }
    if let Some(tools) = obj.get("tools").and_then(Value::as_array) {
        req.tools = tools
            .iter()
            .filter(|t| str_of(t, "type") == "function")
            .map(|t| {
                let f = t.get("function").unwrap_or(t);
                Tool::Function {
                    name: str_of(f, "name").to_string(),
                    description: str_of(f, "description").to_string(),
                    schema: f
                        .get("parameters")
                        .filter(|p| p.is_object())
                        .cloned()
                        .unwrap_or_else(empty_schema),
                    cache: None,
                }
            })
            .collect();
    }
    req.tool_choice = obj.get("tool_choice").map(|tc| match tc {
        Value::String(s) if s == "none" => ToolChoice::None,
        Value::String(s) if s == "required" => ToolChoice::Required,
        Value::Object(_) => ToolChoice::Named(
            tc.get("function")
                .map_or("", |f| str_of(f, "name"))
                .to_string(),
        ),
        _ => ToolChoice::Auto,
    });
    req.max_tokens = obj
        .get("max_tokens")
        .or_else(|| obj.get("max_completion_tokens"))
        .and_then(Value::as_u64);
    req.temperature = obj.get("temperature").and_then(Value::as_f64);
    req.top_p = obj.get("top_p").and_then(Value::as_f64);
    req.stop = stops(obj.get("stop"));
    req.stream = obj.get("stream").and_then(Value::as_bool) == Some(true);
    req.effort = ["reasoning_effort", "reasoning"]
        .iter()
        .find_map(|k| obj.get(*k).and_then(Effort::parse));
    req.cache_key = obj
        .get("prompt_cache_key")
        .and_then(Value::as_str)
        .map(str::to_string);
    req.ext = ext_rest(
        obj,
        &[
            "model",
            "messages",
            "tools",
            "tool_choice",
            "max_tokens",
            "max_completion_tokens",
            "temperature",
            "top_p",
            "stop",
            "stream",
            "reasoning_effort",
            "prompt_cache_key",
        ],
        NS,
    );
    Ok(req)
}

/// Recursively drop JSON Schema `pattern`/`patternProperties`. They are
/// advisory for tool calls, and some upstream validators reject regexes they
/// cannot compile (`DeepSeek` 400s on a `\0` escape), failing the request.
fn strip_schema_patterns(mut value: Value) -> Value {
    match &mut value {
        Value::Object(map) => {
            map.remove("pattern");
            map.remove("patternProperties");
            for v in map.values_mut() {
                *v = strip_schema_patterns(std::mem::take(v));
            }
        }
        Value::Array(arr) => {
            for v in arr {
                *v = strip_schema_patterns(std::mem::take(v));
            }
        }
        _ => {}
    }
    value
}

/// Text-only user content collapses to a string (most compatible); anything
/// with images stays an array.
fn user_content(parts: &[&Part]) -> Value {
    if parts.iter().all(|p| matches!(p, Part::Text(_))) {
        return json!(parts
            .iter()
            .filter_map(|p| match p {
                Part::Text(t) => Some(t.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"));
    }
    Value::Array(
        parts
            .iter()
            .filter_map(|p| match p {
                Part::Text(t) => Some(json!({"type": "text", "text": t})),
                Part::Image(i) => {
                    Some(json!({"type": "image_url", "image_url": {"url": i.to_url()}}))
                }
                _ => None,
            })
            .collect(),
    )
}

fn encode_messages(req: &Request) -> Vec<Value> {
    let mut out = Vec::new();
    if !req.system.is_empty() {
        let text: Vec<String> = req
            .system
            .iter()
            .filter_map(|b| match &b.part {
                Part::Text(t) => Some(t.clone()),
                _ => None,
            })
            .collect();
        out.push(json!({"role": "system", "content": text.join("\n")}));
    }
    for m in &req.messages {
        if m.role == Role::Assistant {
            let text: Vec<&str> = m
                .content
                .iter()
                .filter_map(|b| match &b.part {
                    Part::Text(t) => Some(t.as_str()),
                    _ => None,
                })
                .collect();
            let calls: Vec<Value> = m
                .content
                .iter()
                .filter_map(|b| match &b.part {
                    Part::ToolCall(c) => Some(json!({
                        "id": c.id,
                        "type": "function",
                        "function": {"name": c.name, "arguments": args_string(&c.arguments)}
                    })),
                    _ => None,
                })
                .collect();
            if text.is_empty() && calls.is_empty() {
                continue;
            }
            let mut msg = json!({"role": "assistant"});
            if !text.is_empty() {
                msg["content"] = json!(text.join("\n"));
            }
            if !calls.is_empty() {
                msg["tool_calls"] = Value::Array(calls);
            }
            out.push(msg);
            continue;
        }
        // User turn: tool results become `tool` messages, in order; other
        // content between them is flushed as a user message.
        let mut pending: Vec<&Part> = Vec::new();
        for b in &m.content {
            match &b.part {
                Part::ToolResult(r) => {
                    if !pending.is_empty() {
                        out.push(json!({"role": "user", "content": user_content(&pending)}));
                        pending.clear();
                    }
                    out.push(
                        json!({"role": "tool", "tool_call_id": r.call_id, "content": r.content}),
                    );
                }
                p @ (Part::Text(_) | Part::Image(_)) => pending.push(p),
                _ => {}
            }
        }
        if !pending.is_empty() {
            out.push(json!({"role": "user", "content": user_content(&pending)}));
        }
    }
    out
}

/// Encode an `OpenAI` Chat Completions request.
#[must_use]
pub fn encode_request(req: &Request) -> Value {
    let mut out = Map::new();
    out.insert("model".into(), json!(req.model));
    out.insert("messages".into(), Value::Array(encode_messages(req)));
    let tools: Vec<Value> = req
        .tools
        .iter()
        .filter_map(|t| match t {
            Tool::Function {
                name,
                description,
                schema,
                ..
            } => Some(json!({
                "type": "function",
                "function": {
                    "name": name,
                    "description": description,
                    "parameters": strip_schema_patterns(schema.clone())
                }
            })),
            Tool::WebSearch => None,
        })
        .collect();
    if !tools.is_empty() {
        out.insert("tools".into(), Value::Array(tools));
    }
    if let Some(tc) = &req.tool_choice {
        out.insert(
            "tool_choice".into(),
            match tc {
                ToolChoice::Auto => json!("auto"),
                ToolChoice::None => json!("none"),
                ToolChoice::Required => json!("required"),
                ToolChoice::Named(n) => json!({"type": "function", "function": {"name": n}}),
            },
        );
    }
    if let Some(m) = req.max_tokens {
        out.insert("max_tokens".into(), json!(m));
    }
    if let Some(t) = req.temperature {
        out.insert("temperature".into(), json!(t));
    }
    if let Some(p) = req.top_p {
        out.insert("top_p".into(), json!(p));
    }
    if !req.stop.is_empty() {
        out.insert("stop".into(), json!(req.stop));
    }
    if let Some(e) = req.effort {
        out.insert("reasoning_effort".into(), json!(e.as_lmh()));
    }
    if req.stream {
        out.insert("stream".into(), json!(true));
    }
    Value::Object(out)
}

// ---------------------------------------------------------------------------
// response
// ---------------------------------------------------------------------------

fn stop_from(s: &str) -> Option<StopReason> {
    match s {
        "tool_calls" | "tool_call" | "function_call" => Some(StopReason::ToolUse),
        "length" | "max_tokens" => Some(StopReason::MaxTokens),
        "stop" | "" => None,
        _ => Some(StopReason::EndTurn),
    }
}

const fn finish_str(s: StopReason) -> &'static str {
    match s {
        StopReason::EndTurn | StopReason::StopSequence => "stop",
        StopReason::MaxTokens => "length",
        StopReason::ToolUse => "tool_calls",
    }
}

/// Decode an `OpenAI` Chat Completions response.
#[must_use]
pub fn decode_response(body: &Value) -> Response {
    let choice = body.pointer("/choices/0").unwrap_or(&Value::Null);
    let msg = choice.get("message").unwrap_or(&Value::Null);
    let mut content = Vec::new();
    if let Some(r) = msg.get("reasoning_content").and_then(Value::as_str) {
        if !r.is_empty() {
            content.push(Part::Reasoning {
                text: r.to_string(),
                signature: None,
            });
        }
    }
    let text = text_of(msg.get("content").unwrap_or(&Value::Null));
    if !text.is_empty() {
        content.push(Part::Text(text));
    }
    content.extend(decode_tool_calls(msg).into_iter().map(|b| b.part));
    let saw_tool = content.iter().any(|p| matches!(p, Part::ToolCall(_)));
    Response {
        id: str_of(body, "id").to_string(),
        model: str_of(body, "model").to_string(),
        content,
        stop: stop_from(str_of(choice, "finish_reason")).unwrap_or(if saw_tool {
            StopReason::ToolUse
        } else {
            StopReason::EndTurn
        }),
        created: body.get("created").and_then(Value::as_u64),
        usage: usage_from(body.get("usage").unwrap_or(&Value::Null)),
        ext: super::Ext::default(),
    }
}

/// Encode an `OpenAI` Chat Completions response.
#[must_use]
pub fn encode_response(resp: &Response) -> Value {
    let mut message = json!({"role": "assistant"});
    let text: String = resp
        .content
        .iter()
        .filter_map(|p| match p {
            Part::Text(t) => Some(t.as_str()),
            _ => None,
        })
        .collect();
    message["content"] = if text.is_empty() {
        Value::Null
    } else {
        json!(text)
    };
    let reasoning: String = resp
        .content
        .iter()
        .filter_map(|p| match p {
            Part::Reasoning { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    if !reasoning.is_empty() {
        message["reasoning_content"] = json!(reasoning);
    }
    let calls: Vec<Value> = resp
        .content
        .iter()
        .filter_map(|p| match p {
            Part::ToolCall(c) => Some(json!({
                "id": c.id,
                "type": "function",
                "function": {"name": c.name, "arguments": args_string(&c.arguments)}
            })),
            _ => None,
        })
        .collect();
    if !calls.is_empty() {
        message["tool_calls"] = Value::Array(calls);
    }
    json!({
        "id": if resp.id.is_empty() { new_id("chatcmpl") } else { resp.id.clone() },
        "object": "chat.completion",
        "created": resp.created.unwrap_or_else(now_ts),
        "model": resp.model,
        "choices": [{
            "index": 0,
            "message": message,
            "finish_reason": finish_str(resp.stop),
            "logprobs": null
        }],
        "usage": openai_usage(&resp.usage)
    })
}

// ---------------------------------------------------------------------------
// stream
// ---------------------------------------------------------------------------

/// `OpenAI` chunks -> IR events.
///
/// Blocks are implicit in this format, so they are synthesized: text and reasoning blocks close when another kind starts;
/// tool calls stay open until the stream ends (their deltas may interleave).
#[derive(Debug, Default)]
pub struct Decoder {
    em: Emitter,
    text: Option<u32>,
    reasoning: Option<u32>,
    /// upstream tool-call index -> IR index
    tools: Vec<(u64, u32)>,
    stop: Option<StopReason>,
}

impl Decoder {
    fn close_prose(&mut self, out: &mut Vec<Event>) {
        if let Some(i) = self.text.take() {
            self.em.close(out, i);
        }
        if let Some(i) = self.reasoning.take() {
            self.em.close(out, i);
        }
    }
}

impl StreamDecoder for Decoder {
    fn frame(&mut self, frame: &SseFrame) -> Vec<Event> {
        let mut out = Vec::new();
        if frame.is_done() {
            return out;
        }
        let Some(v) = frame.json() else {
            return out;
        };
        self.em
            .start(&mut out, str_of(&v, "id"), str_of(&v, "model"));
        if let Some(u) = v.get("usage").filter(|u| !u.is_null()) {
            self.em.usage(usage_from(u));
        }
        let Some(choice) = v.pointer("/choices/0") else {
            return out;
        };
        let delta = choice.get("delta").unwrap_or(&Value::Null);

        let reasoning = delta
            .get("reasoning_content")
            .or_else(|| delta.get("reasoning"))
            .and_then(Value::as_str)
            .unwrap_or("");
        if !reasoning.is_empty() {
            let index = if let Some(i) = self.reasoning {
                i
            } else {
                if let Some(t) = self.text.take() {
                    self.em.close(&mut out, t);
                }
                let i = self.em.open(&mut out, BlockKind::Reasoning);
                self.reasoning = Some(i);
                i
            };
            Emitter::delta(&mut out, index, reasoning);
        }

        let text = str_of(delta, "content");
        if !text.is_empty() {
            let index = if let Some(i) = self.text {
                i
            } else {
                if let Some(r) = self.reasoning.take() {
                    self.em.close(&mut out, r);
                }
                let i = self.em.open(&mut out, BlockKind::Text);
                self.text = Some(i);
                i
            };
            Emitter::delta(&mut out, index, text);
        }

        for tc in delta
            .get("tool_calls")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let upstream = tc.get("index").and_then(Value::as_u64).unwrap_or(0);
            let f = tc.get("function").unwrap_or(&Value::Null);
            let index = if let Some((_, i)) = self.tools.iter().find(|(u, _)| *u == upstream) {
                *i
            } else {
                self.close_prose(&mut out);
                let id = match str_of(tc, "id") {
                    "" => format!("toolu_{upstream}"),
                    id => id.to_string(),
                };
                let kind = BlockKind::ToolCall {
                    id,
                    name: str_of(f, "name").to_string(),
                };
                let i = self.em.open(&mut out, kind);
                self.tools.push((upstream, i));
                i
            };
            Emitter::delta(&mut out, index, str_of(f, "arguments"));
        }

        if let Some(fr) = choice.get("finish_reason").and_then(Value::as_str) {
            self.stop = stop_from(fr);
        }
        out
    }

    fn finish(&mut self) -> Vec<Event> {
        self.em.finish(self.stop)
    }
}

/// IR events -> `OpenAI` chunks.
#[derive(Debug)]
pub struct Encoder {
    id: String,
    model: String,
    created: u64,
    started: bool,
    usage: TokenUsage,
    kinds: Vec<(u32, BlockKind)>,
    /// IR index -> `tool_calls[].index`
    tools: Vec<u32>,
}

impl Encoder {
    /// Encoder reporting `model` to the client.
    #[must_use]
    pub fn new(model: &str) -> Self {
        Self {
            id: new_id("chatcmpl"),
            model: model.to_string(),
            created: now_ts(),
            started: false,
            usage: TokenUsage::default(),
            kinds: Vec::new(),
            tools: Vec::new(),
        }
    }

    #[allow(clippy::needless_pass_by_value)]
    fn chunk(&self, delta: Value, finish: Option<&str>) -> Value {
        json!({
            "id": self.id,
            "object": "chat.completion.chunk",
            "created": self.created,
            "model": self.model,
            "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]
        })
    }

    fn tool_ordinal(&self, index: u32) -> usize {
        self.tools.iter().position(|i| *i == index).unwrap_or(0)
    }
}

impl StreamEncoder for Encoder {
    fn event(&mut self, event: &Event) -> Vec<Value> {
        let mut out = Vec::new();
        if !self.started {
            self.started = true;
            if let Event::Start { id, .. } = event {
                if !id.is_empty() {
                    self.id.clone_from(id);
                }
            }
            out.push(self.chunk(json!({"role": "assistant"}), None));
        }
        match event {
            Event::BlockStart { index, kind } => {
                self.kinds.push((*index, kind.clone()));
                if let BlockKind::ToolCall { id, name } = kind {
                    self.tools.push(*index);
                    out.push(self.chunk(
                        json!({"tool_calls": [{
                            "index": self.tool_ordinal(*index),
                            "id": id,
                            "type": "function",
                            "function": {"name": name, "arguments": ""}
                        }]}),
                        None,
                    ));
                }
            }
            Event::Delta { index, text } => {
                let kind = self.kinds.iter().find(|(i, _)| i == index).map(|(_, k)| k);
                let delta = match kind {
                    Some(BlockKind::Reasoning) => json!({"reasoning_content": text}),
                    Some(BlockKind::ToolCall { .. }) => json!({"tool_calls": [{
                        "index": self.tool_ordinal(*index),
                        "function": {"arguments": text}
                    }]}),
                    _ => json!({"content": text}),
                };
                out.push(self.chunk(delta, None));
            }
            Event::Usage(u) => self.usage = *u,
            Event::Finish(stop) => {
                let mut last = self.chunk(json!({}), Some(finish_str(*stop)));
                if self.usage.input > 0 || self.usage.output > 0 {
                    last["usage"] = openai_usage(&self.usage);
                }
                out.push(last);
                out.push(json!("[DONE]"));
            }
            Event::Start { .. } | Event::BlockStop { .. } => {}
        }
        out
    }
}
