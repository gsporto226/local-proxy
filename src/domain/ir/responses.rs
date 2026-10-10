//! `OpenAI` Responses API codec.

use serde_json::{json, Map, Value};

use super::{
    args_string, args_value, empty_schema, ext_rest, new_id, now_ts, push_message, str_of, text_of,
    usage_from, Block, BlockKind, Effort, Emitter, Event, Image, Part, Request, Response, Role,
    StopReason, StreamDecoder, StreamEncoder, Tool, ToolCall, ToolChoice, ToolResult,
};
use crate::domain::sse::SseFrame;
use crate::domain::translate::{responses_usage, TokenUsage, TranslateError};

const NS: &str = "responses";

// ---------------------------------------------------------------------------
// request
// ---------------------------------------------------------------------------

fn decode_content(content: &Value) -> Vec<Block> {
    match content {
        Value::String(s) => vec![Block::new(Part::Text(s.clone()))],
        Value::Array(arr) => arr
            .iter()
            .filter_map(|b| match str_of(b, "type") {
                "input_text" | "output_text" | "text" => {
                    Some(Part::Text(str_of(b, "text").to_string()))
                }
                "input_image" => {
                    let url = b
                        .get("image_url")
                        .and_then(|u| u.as_str().or_else(|| u.get("url")?.as_str()))
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

fn call_id(item: &Value) -> String {
    item.get("call_id")
        .or_else(|| item.get("id"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

/// Decode a Responses API request.
///
/// # Errors
///
/// Returns [`TranslateError`] when the body is not an object or `input` has
/// the wrong shape.
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
    if let Some(s) = obj.get("instructions").and_then(Value::as_str) {
        if !s.is_empty() {
            req.system.push(Block::new(Part::Text(s.to_string())));
        }
    }
    let items = match obj.get("input") {
        Some(Value::String(s)) => vec![json!({"role": "user", "content": s})],
        Some(Value::Array(a)) => a.clone(),
        None | Some(Value::Null) => Vec::new(),
        Some(_) => {
            return Err(TranslateError::Invalid {
                field: "input",
                detail: "expected string or array".to_string(),
            })
        }
    };
    for item in &items {
        match item
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("message")
        {
            "message" => {
                let blocks = decode_content(item.get("content").unwrap_or(&Value::Null));
                match str_of(item, "role") {
                    "system" | "developer" => req.system.extend(blocks),
                    "assistant" => push_message(&mut req.messages, Role::Assistant, blocks),
                    _ => push_message(&mut req.messages, Role::User, blocks),
                }
            }
            "function_call" => push_message(
                &mut req.messages,
                Role::Assistant,
                vec![Block::new(Part::ToolCall(ToolCall {
                    id: call_id(item),
                    name: str_of(item, "name").to_string(),
                    arguments: args_value(item.get("arguments")),
                }))],
            ),
            "function_call_output" => push_message(
                &mut req.messages,
                Role::User,
                vec![Block::new(Part::ToolResult(ToolResult {
                    call_id: call_id(item),
                    content: text_of(item.get("output").unwrap_or(&Value::Null)),
                    is_error: false,
                }))],
            ),
            _ => {}
        }
    }
    if let Some(tools) = obj.get("tools").and_then(Value::as_array) {
        req.tools = tools
            .iter()
            .filter_map(|t| match str_of(t, "type") {
                "function" => Some(Tool::Function {
                    name: str_of(t, "name").to_string(),
                    description: str_of(t, "description").to_string(),
                    schema: t
                        .get("parameters")
                        .filter(|p| p.is_object())
                        .cloned()
                        .unwrap_or_else(empty_schema),
                    cache: None,
                }),
                ty if ty.starts_with("web_search") || ty == "web_fetch" => Some(Tool::WebSearch),
                _ => None,
            })
            .collect();
    }
    req.tool_choice = obj.get("tool_choice").map(|tc| match tc {
        Value::String(s) if s == "none" => ToolChoice::None,
        Value::String(s) if s == "required" => ToolChoice::Required,
        Value::Object(_) if str_of(tc, "type") == "function" => {
            ToolChoice::Named(str_of(tc, "name").to_string())
        }
        _ => ToolChoice::Auto,
    });
    req.max_tokens = obj.get("max_output_tokens").and_then(Value::as_u64);
    req.temperature = obj.get("temperature").and_then(Value::as_f64);
    req.top_p = obj.get("top_p").and_then(Value::as_f64);
    req.stream = obj.get("stream").and_then(Value::as_bool) == Some(true);
    req.effort = obj.get("reasoning").and_then(Effort::parse);
    req.cache_key = obj
        .get("prompt_cache_key")
        .and_then(Value::as_str)
        .map(str::to_string);
    req.ext = ext_rest(
        obj,
        &[
            "model",
            "instructions",
            "input",
            "tools",
            "tool_choice",
            "max_output_tokens",
            "temperature",
            "top_p",
            "stream",
            "prompt_cache_key",
        ],
        NS,
    );
    Ok(req)
}

fn content_part(part: &Part, assistant: bool) -> Option<Value> {
    match part {
        Part::Text(t) if assistant => {
            Some(json!({"type": "output_text", "text": t, "annotations": []}))
        }
        Part::Text(t) => Some(json!({"type": "input_text", "text": t})),
        Part::Image(i) if !assistant => {
            Some(json!({"type": "input_image", "image_url": i.to_url()}))
        }
        _ => None,
    }
}

fn encode_input(req: &Request) -> Vec<Value> {
    let mut out = Vec::new();
    for m in &req.messages {
        let assistant = m.role == Role::Assistant;
        let role = if assistant { "assistant" } else { "user" };
        let mut pending: Vec<Value> = Vec::new();
        let flush = |pending: &mut Vec<Value>, out: &mut Vec<Value>| {
            if !pending.is_empty() {
                out.push(
                    json!({"type": "message", "role": role, "content": std::mem::take(pending)}),
                );
            }
        };
        for b in &m.content {
            match &b.part {
                Part::ToolCall(c) => {
                    flush(&mut pending, &mut out);
                    out.push(json!({
                        "type": "function_call",
                        "call_id": c.id,
                        "name": c.name,
                        "arguments": args_string(&c.arguments)
                    }));
                }
                Part::ToolResult(r) => {
                    flush(&mut pending, &mut out);
                    out.push(json!({
                        "type": "function_call_output",
                        "call_id": r.call_id,
                        "output": r.content
                    }));
                }
                other => pending.extend(content_part(other, assistant)),
            }
        }
        flush(&mut pending, &mut out);
    }
    out
}

/// Encode a Responses API request.
#[must_use]
pub fn encode_request(req: &Request) -> Value {
    let mut out = Map::new();
    out.insert("model".into(), json!(req.model));
    let instructions: Vec<&str> = req
        .system
        .iter()
        .filter_map(|b| match &b.part {
            Part::Text(t) => Some(t.as_str()),
            _ => None,
        })
        .collect();
    if !instructions.is_empty() {
        out.insert("instructions".into(), json!(instructions.join("\n\n")));
    }
    out.insert("input".into(), Value::Array(encode_input(req)));
    let tools: Vec<Value> = req
        .tools
        .iter()
        .map(|t| match t {
            Tool::Function {
                name,
                description,
                schema,
                ..
            } => json!({
                "type": "function",
                "name": name,
                "description": description,
                "parameters": schema
            }),
            Tool::WebSearch => json!({"type": "web_search"}),
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
                ToolChoice::Named(n) => json!({"type": "function", "name": n}),
            },
        );
    }
    if let Some(m) = req.max_tokens {
        out.insert("max_output_tokens".into(), json!(m));
    }
    if let Some(t) = req.temperature {
        out.insert("temperature".into(), json!(t));
    }
    if let Some(p) = req.top_p {
        out.insert("top_p".into(), json!(p));
    }
    if let Some(e) = req.effort {
        let mut reasoning = json!({"effort": e.as_responses()});
        // Keep a summary the client asked for (raw `reasoning` stays in ext).
        if let Some(summary) = ["openai:reasoning", "responses:reasoning"]
            .iter()
            .find_map(|k| req.ext.get(k)?.get("summary"))
        {
            reasoning["summary"] = summary.clone();
        }
        out.insert("reasoning".into(), reasoning);
    }
    if let Some(k) = &req.cache_key {
        out.insert("prompt_cache_key".into(), json!(k));
    }
    // The ChatGPT backend rejects stored responses: default to false, but
    // honor a client that asked for storage explicitly.
    let store = ["openai:store", "responses:store"]
        .iter()
        .find_map(|k| req.ext.get(k).and_then(Value::as_bool))
        .unwrap_or(false);
    out.insert("store".into(), json!(store));
    if req.stream {
        out.insert("stream".into(), json!(true));
    }
    Value::Object(out)
}

// ---------------------------------------------------------------------------
// response
// ---------------------------------------------------------------------------

/// Decode a Responses API response.
#[must_use]
pub fn decode_response(body: &Value) -> Response {
    let mut content = Vec::new();
    for item in body
        .get("output")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        match str_of(item, "type") {
            "message" => {
                let text = item
                    .get("content")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter(|p| str_of(p, "type") == "output_text")
                    .map(|p| str_of(p, "text"))
                    .collect::<String>();
                if !text.is_empty() {
                    content.push(Part::Text(text));
                }
            }
            "function_call" => content.push(Part::ToolCall(ToolCall {
                id: call_id(item),
                name: str_of(item, "name").to_string(),
                arguments: args_value(item.get("arguments")),
            })),
            "reasoning" => {
                let text = text_of(item.get("summary").unwrap_or(&Value::Null));
                if !text.is_empty() {
                    content.push(Part::Reasoning {
                        text,
                        signature: None,
                    });
                }
            }
            _ => {}
        }
    }
    let stop = if str_of(body, "status") == "incomplete" {
        StopReason::MaxTokens
    } else if content.iter().any(|p| matches!(p, Part::ToolCall(_))) {
        StopReason::ToolUse
    } else {
        StopReason::EndTurn
    };
    let mut ext = super::Ext::default();
    for key in ["status", "error"] {
        if let Some(v) = body.get(key).filter(|v| !v.is_null()) {
            ext.insert(format!("{NS}:{key}"), v.clone());
        }
    }
    Response {
        id: str_of(body, "id").to_string(),
        model: str_of(body, "model").to_string(),
        content,
        stop,
        usage: usage_from(body.get("usage").unwrap_or(&Value::Null)),
        created: body.get("created_at").and_then(Value::as_u64),
        ext,
    }
}

fn message_item(id: &str, text: &str) -> Value {
    json!({
        "type": "message",
        "id": id,
        "status": "completed",
        "role": "assistant",
        "content": [{"type": "output_text", "text": text, "annotations": []}]
    })
}

fn function_item(id: &str, name: &str, args: &str) -> Value {
    json!({
        "type": "function_call",
        "id": id,
        "call_id": id,
        "name": name,
        "arguments": args,
        "status": "completed"
    })
}

#[allow(clippy::needless_pass_by_value)]
fn envelope(
    id: &str,
    model: &str,
    created_at: u64,
    status: &str,
    output: &[Value],
    usage: Value,
) -> Value {
    json!({
        "id": id,
        "object": "response",
        "created_at": created_at,
        "status": status,
        "model": model,
        "output": output,
        "parallel_tool_calls": true,
        "usage": usage
    })
}

/// Encode a Responses API response.
#[must_use]
pub fn encode_response(resp: &Response) -> Value {
    let mut output = Vec::new();
    let text: String = resp
        .content
        .iter()
        .filter_map(|p| match p {
            Part::Text(t) => Some(t.as_str()),
            _ => None,
        })
        .collect();
    if !text.is_empty() {
        output.push(message_item("msg_0", &text));
    }
    for p in &resp.content {
        if let Part::ToolCall(c) = p {
            output.push(function_item(&c.id, &c.name, &args_string(&c.arguments)));
        }
    }
    let id = if resp.id.is_empty() {
        new_id("resp")
    } else {
        resp.id.clone()
    };
    let status = resp
        .ext
        .get("responses:status")
        .and_then(Value::as_str)
        .unwrap_or("completed");
    let mut body = envelope(
        &id,
        &resp.model,
        resp.created.unwrap_or_else(now_ts),
        status,
        &output,
        responses_usage(&resp.usage),
    );
    if let Some(err) = resp.ext.get("responses:error") {
        body["error"] = err.clone();
    }
    body
}

// ---------------------------------------------------------------------------
// stream
// ---------------------------------------------------------------------------

/// Responses SSE -> IR events.
#[derive(Debug, Default)]
pub struct Decoder {
    em: Emitter,
    text: Option<u32>,
    reasoning: Option<u32>,
    /// upstream item id -> IR index
    tools: Vec<(String, u32)>,
    incomplete: bool,
}

impl Decoder {
    /// Key of an output item: its id, else its output index (some upstreams
    /// send deltas with only `output_index`).
    fn item_key(v: &Value, id: &str) -> String {
        match id {
            "" => format!(
                "#{}",
                v.get("output_index").and_then(Value::as_u64).unwrap_or(0)
            ),
            id => id.to_string(),
        }
    }

    fn tool_index(&self, key: &str) -> Option<u32> {
        self.tools.iter().find(|(k, _)| k == key).map(|(_, i)| *i)
    }

    fn open_tool(&mut self, out: &mut Vec<Event>, key: String, item: &Value) -> u32 {
        self.close_prose(out);
        let kind = BlockKind::ToolCall {
            id: call_id(item),
            name: str_of(item, "name").to_string(),
        };
        let i = self.em.open(out, kind);
        self.tools.push((key, i));
        i
    }

    fn close_prose(&mut self, out: &mut Vec<Event>) {
        if let Some(i) = self.text.take() {
            self.em.close(out, i);
        }
        if let Some(i) = self.reasoning.take() {
            self.em.close(out, i);
        }
    }

    fn prose(&mut self, out: &mut Vec<Event>, reasoning: bool) -> u32 {
        let (mine, other) = if reasoning {
            (self.reasoning, self.text)
        } else {
            (self.text, self.reasoning)
        };
        if let Some(i) = mine {
            return i;
        }
        if let Some(o) = other {
            self.em.close(out, o);
        }
        let kind = if reasoning {
            BlockKind::Reasoning
        } else {
            BlockKind::Text
        };
        let i = self.em.open(out, kind);
        if reasoning {
            self.reasoning = Some(i);
            self.text = None;
        } else {
            self.text = Some(i);
            self.reasoning = None;
        }
        i
    }
}

impl StreamDecoder for Decoder {
    fn frame(&mut self, frame: &SseFrame) -> Vec<Event> {
        if frame.is_done() {
            return Vec::new();
        }
        frame.json().map_or_else(Vec::new, |v| self.value(&v))
    }

    fn finish(&mut self) -> Vec<Event> {
        let stop = self.incomplete.then_some(StopReason::MaxTokens);
        self.em.finish(stop)
    }
}

impl Decoder {
    /// Decode one event payload.
    pub fn value(&mut self, v: &Value) -> Vec<Event> {
        let mut out = Vec::new();
        if let Some(r) = v.get("response") {
            self.em.start(&mut out, str_of(r, "id"), str_of(r, "model"));
            if let Some(u) = r.get("usage").filter(|u| !u.is_null()) {
                self.em.usage(usage_from(u));
            }
            if str_of(r, "status") == "incomplete" {
                self.incomplete = true;
            }
        }
        let item = v.get("item").unwrap_or(&Value::Null);
        match str_of(v, "type") {
            "response.output_item.added" if str_of(item, "type") == "function_call" => {
                let key = Self::item_key(v, str_of(item, "id"));
                self.open_tool(&mut out, key, item);
            }
            "response.output_text.delta" => {
                let i = self.prose(&mut out, false);
                Emitter::delta(&mut out, i, str_of(v, "delta"));
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                let i = self.prose(&mut out, true);
                Emitter::delta(&mut out, i, str_of(v, "delta"));
            }
            "response.function_call_arguments.delta" => {
                if let Some(i) = self.tool_index(&Self::item_key(v, str_of(v, "item_id"))) {
                    Emitter::delta(&mut out, i, str_of(v, "delta"));
                }
            }
            "response.output_item.done" => match str_of(item, "type") {
                "function_call" => {
                    let key = Self::item_key(v, str_of(item, "id"));
                    // A call that arrives only as a finished item carries its
                    // arguments here.
                    let i = self.tool_index(&key).unwrap_or_else(|| {
                        let i = self.open_tool(&mut out, key, item);
                        Emitter::delta(&mut out, i, str_of(item, "arguments"));
                        i
                    });
                    self.em.close(&mut out, i);
                }
                "message" => {
                    if let Some(i) = self.text.take() {
                        self.em.close(&mut out, i);
                    }
                }
                "reasoning" => {
                    if let Some(i) = self.reasoning.take() {
                        self.em.close(&mut out, i);
                    }
                }
                _ => {}
            },
            _ => {}
        }
        out
    }
}

/// Reassemble one [`Response`] from a Responses SSE event sequence (a
/// non-streaming client of a stream-only upstream).
///
/// The incremental events are folded through the IR, then the terminal
/// envelope (`response.completed` / `failed` / `incomplete`) wins wherever it
/// carries data: its finished `output`, status, error, and usage are
/// authoritative over what the deltas built up.
#[must_use]
pub fn aggregate(events: &[Value]) -> Response {
    let mut decoder = Decoder::default();
    let mut ir_events = Vec::new();
    let mut envelope = Map::new();
    for event in events {
        ir_events.extend(decoder.value(event));
        if let Some(Value::Object(r)) = event.get("response") {
            for (k, v) in r {
                if !v.is_null() {
                    envelope.insert(k.clone(), v.clone());
                }
            }
        }
    }
    ir_events.extend(decoder.finish());
    let mut resp = super::fold(&ir_events);
    let env = decode_response(&Value::Object(envelope));
    if !env.content.is_empty() {
        resp.content = env.content;
        resp.stop = env.stop;
    }
    if !env.id.is_empty() {
        resp.id = env.id;
    }
    if !env.model.is_empty() {
        resp.model = env.model;
    }
    if env.usage != TokenUsage::default() {
        resp.usage = env.usage;
    }
    resp.created = env.created;
    resp.ext = env.ext;
    resp
}

#[derive(Debug)]
struct OpenItem {
    index: u32,
    kind: BlockKind,
    output_index: u32,
    item_id: String,
    acc: String,
}

/// IR events -> Responses SSE.
#[derive(Debug)]
pub struct Encoder {
    id: String,
    model: String,
    created_at: u64,
    started: bool,
    next_output: u32,
    open: Vec<OpenItem>,
    items: Vec<Value>,
    usage: TokenUsage,
}

impl Encoder {
    /// Encoder reporting `model` to the client.
    #[must_use]
    pub fn new(model: &str) -> Self {
        Self {
            id: new_id("resp"),
            model: model.to_string(),
            created_at: now_ts(),
            started: false,
            next_output: 0,
            open: Vec::new(),
            items: Vec::new(),
            usage: TokenUsage::default(),
        }
    }
}

impl StreamEncoder for Encoder {
    #[allow(clippy::too_many_lines)]
    fn event(&mut self, event: &Event) -> Vec<Value> {
        let mut out = Vec::new();
        if !self.started {
            self.started = true;
            if let Event::Start { id, .. } = event {
                if !id.is_empty() {
                    self.id.clone_from(id);
                }
            }
            let mut created = envelope(
                &self.id,
                &self.model,
                self.created_at,
                "in_progress",
                &[],
                Value::Null,
            );
            created["output"] = json!([]);
            out.push(json!({"type": "response.created", "response": created}));
        }
        match event {
            Event::BlockStart { index, kind } => {
                let output_index = self.next_output;
                let item_id = match kind {
                    BlockKind::Text => format!("msg_{output_index}"),
                    BlockKind::ToolCall { id, .. } if !id.is_empty() => id.clone(),
                    BlockKind::ToolCall { .. } => format!("call_{output_index}"),
                    // Reasoning is not surfaced to Responses clients.
                    BlockKind::Reasoning => String::new(),
                };
                match kind {
                    BlockKind::Text => {
                        out.push(json!({
                            "type": "response.output_item.added",
                            "output_index": output_index,
                            "item": {"type": "message", "id": item_id, "status": "in_progress", "role": "assistant", "content": []}
                        }));
                        out.push(json!({
                            "type": "response.content_part.added",
                            "item_id": item_id,
                            "output_index": output_index,
                            "content_index": 0,
                            "part": {"type": "output_text", "text": "", "annotations": []}
                        }));
                    }
                    BlockKind::ToolCall { name, .. } => out.push(json!({
                        "type": "response.output_item.added",
                        "output_index": output_index,
                        "item": {
                            "type": "function_call",
                            "id": item_id,
                            "call_id": item_id,
                            "name": name,
                            "arguments": "",
                            "status": "in_progress"
                        }
                    })),
                    BlockKind::Reasoning => {}
                }
                if !matches!(kind, BlockKind::Reasoning) {
                    self.next_output += 1;
                }
                self.open.push(OpenItem {
                    index: *index,
                    kind: kind.clone(),
                    output_index,
                    item_id,
                    acc: String::new(),
                });
            }
            Event::Delta { index, text } => {
                if let Some(it) = self.open.iter_mut().find(|it| it.index == *index) {
                    it.acc.push_str(text);
                    match it.kind {
                        BlockKind::Text => out.push(json!({
                            "type": "response.output_text.delta",
                            "item_id": it.item_id,
                            "output_index": it.output_index,
                            "content_index": 0,
                            "delta": text
                        })),
                        BlockKind::ToolCall { .. } => out.push(json!({
                            "type": "response.function_call_arguments.delta",
                            "item_id": it.item_id,
                            "output_index": it.output_index,
                            "delta": text
                        })),
                        BlockKind::Reasoning => {}
                    }
                }
            }
            Event::BlockStop { index } => {
                if let Some(pos) = self.open.iter().position(|it| it.index == *index) {
                    let it = self.open.remove(pos);
                    match &it.kind {
                        BlockKind::Text => {
                            out.push(json!({
                                "type": "response.output_text.done",
                                "item_id": it.item_id,
                                "output_index": it.output_index,
                                "content_index": 0,
                                "text": it.acc
                            }));
                            let item = message_item(&it.item_id, &it.acc);
                            out.push(json!({"type": "response.output_item.done", "output_index": it.output_index, "item": item}));
                            self.items.push(item);
                        }
                        BlockKind::ToolCall { name, .. } => {
                            out.push(json!({
                                "type": "response.function_call_arguments.done",
                                "item_id": it.item_id,
                                "output_index": it.output_index,
                                "arguments": it.acc
                            }));
                            let item = function_item(&it.item_id, name, &it.acc);
                            out.push(json!({"type": "response.output_item.done", "output_index": it.output_index, "item": item}));
                            self.items.push(item);
                        }
                        BlockKind::Reasoning => {}
                    }
                }
            }
            Event::Usage(u) => self.usage = *u,
            Event::Finish(_) => out.push(json!({
                "type": "response.completed",
                "response": envelope(
                    &self.id,
                    &self.model,
                    self.created_at,
                    "completed",
                    &self.items,
                    responses_usage(&self.usage)
                )
            })),
            Event::Start { .. } => {}
        }
        out
    }
}
