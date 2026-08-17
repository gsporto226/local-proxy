use std::collections::HashMap;
use std::convert::Infallible;
use std::pin::Pin;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::response::sse::Event;
use futures_util::{Stream, StreamExt};
use serde_json::{json, Value};

use crate::sse::{sse_frames, SseError, SseFrame};
use crate::translate::{parse_usage, responses_usage, TokenUsage};

/// A stream of already-framed SSE `Event`s ready to be sent to the client.
pub type UpstreamStream = Pin<Box<dyn Stream<Item = Result<Event, Infallible>> + Send>>;

fn now_ts() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn new_id(prefix: &str) -> String {
    let u = uuid::Uuid::new_v4().simple();
    format!("{prefix}_{u}")
}

/// Convert a machine-produced JSON payload into an SSE `Event`. Anthropic and
/// Responses events carry their name in `type`; OpenAI chat chunks carry no
/// event name; a string payload becomes a raw `data:` line (`[DONE]`).
fn to_event(value: Value) -> Event {
    if let Value::String(s) = &value {
        return Event::default().data(s);
    }
    match value.get("type").and_then(Value::as_str) {
        Some(name) => Event::default().event(name).json_data(value).unwrap(),
        None => Event::default().json_data(value).unwrap(),
    }
}

// ---------------------------------------------------------------------------
// machine driver
// ---------------------------------------------------------------------------

trait Machine: Send {
    /// Translate one upstream SSE frame into zero or more client-side events.
    fn process(&mut self, frame: &SseFrame) -> Vec<Value>;
    /// Produce any remaining terminal events once the upstream stream ends.
    fn finalize(&mut self) -> Vec<Value>;
}

struct Driver<M: Machine> {
    frames: Pin<Box<dyn Stream<Item = Result<SseFrame, SseError>> + Send>>,
    machine: M,
    pending: Vec<Value>,
    done: bool,
    finalized: bool,
}

async fn drive<M: Machine>(mut st: Driver<M>) -> Option<(Result<Event, Infallible>, Driver<M>)> {
    loop {
        if !st.pending.is_empty() {
            return Some((Ok(to_event(st.pending.remove(0))), st));
        }
        if st.done {
            if !st.finalized {
                st.finalized = true;
                st.pending = st.machine.finalize();
                if st.pending.is_empty() {
                    return None;
                }
                continue;
            }
            return None;
        }
        match st.frames.next().await {
            Some(Ok(frame)) => st.pending = st.machine.process(&frame),
            Some(Err(_)) | None => st.done = true,
        }
    }
}

fn build<M: Machine + 'static>(resp: reqwest::Response, machine: M) -> UpstreamStream {
    let driver = Driver {
        frames: Box::pin(sse_frames(resp)),
        machine,
        pending: Vec::new(),
        done: false,
        finalized: false,
    };
    Box::pin(futures_util::stream::unfold(driver, drive))
}

// ---------------------------------------------------------------------------
// shared helpers
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
struct ToolBlock {
    block: u32,
    id: String,
    name: String,
    args: String,
}

#[derive(Debug, Clone, Default)]
struct ResponsesTool {
    item_id: String,
    name: String,
    args: String,
    output_index: u32,
}

fn anthropic_content_block_start(index: u32, block: Value) -> Value {
    json!({"type": "content_block_start", "index": index, "content_block": block})
}

fn anthropic_content_block_delta(index: u32, delta: Value) -> Value {
    json!({"type": "content_block_delta", "index": index, "delta": delta})
}

fn anthropic_content_block_stop(index: u32) -> Value {
    json!({"type": "content_block_stop", "index": index})
}

fn anthropic_message_start(id: &str, model: &str, usage: &TokenUsage) -> Value {
    json!({
        "type": "message_start",
        "message": {
            "id": id,
            "type": "message",
            "role": "assistant",
            "model": model,
            "content": [],
            "stop_reason": null,
            "stop_sequence": null,
            "usage": {"input_tokens": usage.input, "output_tokens": usage.output}
        }
    })
}

fn anthropic_message_delta(stop_reason: &str, usage: &TokenUsage) -> Value {
    json!({
        "type": "message_delta",
        "delta": {"stop_reason": stop_reason, "stop_sequence": null},
        "usage": {"input_tokens": usage.input, "output_tokens": usage.output}
    })
}

fn openai_chunk(
    id: &str,
    model: &str,
    created: u64,
    delta: Value,
    finish_reason: Option<&str>,
) -> Value {
    json!({
        "id": id,
        "object": "chat.completion.chunk",
        "created": created,
        "model": model,
        "choices": [{"index": 0, "delta": delta, "finish_reason": finish_reason}]
    })
}

fn responses_created(id: &str, model: &str, created_at: u64) -> Value {
    json!({
        "type": "response.created",
        "response": {
            "id": id,
            "object": "response",
            "created_at": created_at,
            "status": "in_progress",
            "model": model,
            "output": [],
            "parallel_tool_calls": true,
            "usage": null
        }
    })
}

/// Merge a partial usage payload (e.g. only `output_tokens` from an Anthropic
/// `message_delta`) into the accumulated usage, preserving earlier fields.
fn merge_usage(acc: &mut TokenUsage, part: TokenUsage) {
    if part.input > 0 {
        acc.input = part.input;
    }
    if part.output > 0 {
        acc.output = part.output;
    }
    if part.reasoning > 0 {
        acc.reasoning = part.reasoning;
    }
}

fn responses_output_item_added(output_index: u32, item: Value) -> Value {
    json!({"type": "response.output_item.added", "output_index": output_index, "item": item})
}

fn responses_output_item_done(output_index: u32, item: Value) -> Value {
    json!({"type": "response.output_item.done", "output_index": output_index, "item": item})
}

// ---------------------------------------------------------------------------
// OpenAI chat completions -> Anthropic Messages
// ---------------------------------------------------------------------------

struct OaMachine {
    model: String,
    message_id: String,
    started: bool,
    next_block: u32,
    thinking: Option<u32>,
    text: Option<u32>,
    tools: HashMap<usize, ToolBlock>,
    pending_reasoning: String,
    usage: TokenUsage,
    stop_reason: Option<String>,
    saw_tools: bool,
}

impl OaMachine {
    fn new(model: String) -> Self {
        Self {
            model,
            message_id: new_id("msg_stream"),
            started: false,
            next_block: 0,
            thinking: None,
            text: None,
            tools: HashMap::new(),
            pending_reasoning: String::new(),
            usage: TokenUsage::default(),
            stop_reason: None,
            saw_tools: false,
        }
    }

    fn start(&mut self, events: &mut Vec<Value>) {
        if !self.started {
            self.started = true;
            events.push(anthropic_message_start(
                &self.message_id,
                &self.model,
                &self.usage,
            ));
        }
    }

    /// Emit buffered `reasoning_content` as a thinking block. Only opens a new
    /// block when no other content block is currently open (Anthropic blocks
    /// are sequential).
    fn flush_reasoning(&mut self, events: &mut Vec<Value>) {
        if self.pending_reasoning.is_empty() {
            return;
        }
        if self.text.is_some() || !self.tools.is_empty() || self.thinking.is_some() {
            return;
        }
        let index = self.next_block;
        self.next_block += 1;
        events.push(anthropic_content_block_start(
            index,
            json!({"type": "thinking", "thinking": ""}),
        ));
        events.push(anthropic_content_block_delta(
            index,
            json!({"type": "thinking_delta", "thinking": self.pending_reasoning}),
        ));
        self.thinking = Some(index);
        self.pending_reasoning.clear();
    }

    fn ensure_text_block(&mut self, events: &mut Vec<Value>) -> Option<u32> {
        self.flush_reasoning(events);
        if let Some(index) = self.text {
            return Some(index);
        }
        let index = self.next_block;
        self.next_block += 1;
        events.push(anthropic_content_block_start(
            index,
            json!({"type": "text", "text": ""}),
        ));
        self.text = Some(index);
        Some(index)
    }

    fn map_stop_reason(&self) -> &'static str {
        match self.stop_reason.as_deref() {
            Some("tool_calls") | Some("tool_call") => "tool_use",
            Some("length") | Some("max_tokens") => "max_tokens",
            Some(other) if other != "stop" && other != "end_turn" => "end_turn",
            _ => {
                if self.saw_tools || !self.tools.is_empty() {
                    "tool_use"
                } else {
                    "end_turn"
                }
            }
        }
    }
}

impl Machine for OaMachine {
    fn process(&mut self, frame: &SseFrame) -> Vec<Value> {
        let mut events = Vec::new();
        self.start(&mut events);
        if frame.is_done() {
            return events;
        }
        let Some(value) = frame.json() else {
            return events;
        };
        if let Some(u) = value.get("usage") {
            merge_usage(&mut self.usage, parse_usage(u));
        }

        let Some(choices) = value.get("choices").and_then(Value::as_array) else {
            return events;
        };
        let Some(choice) = choices.first() else {
            return events;
        };
        let delta = choice.get("delta").unwrap_or(&Value::Null);

        if let Some(r) = delta.get("reasoning_content").and_then(Value::as_str) {
            self.pending_reasoning.push_str(r);
        }

        if let Some(t) = delta.get("content").and_then(Value::as_str) {
            if let Some(index) = self.ensure_text_block(&mut events) {
                if !t.is_empty() {
                    events.push(anthropic_content_block_delta(
                        index,
                        json!({"type": "text_delta", "text": t}),
                    ));
                }
            }
        }

        if let Some(tool_calls) = delta.get("tool_calls").and_then(Value::as_array) {
            self.flush_reasoning(&mut events);
            for tc in tool_calls {
                let idx = tc.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                let entry = self.tools.entry(idx).or_insert_with(|| {
                    self.saw_tools = true;
                    let block = self.next_block;
                    self.next_block += 1;
                    let id = tc
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let id = if id.is_empty() {
                        format!("toolu_{idx}")
                    } else {
                        id
                    };
                    let name = tc
                        .get("function")
                        .and_then(|f| f.get("name"))
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    events.push(anthropic_content_block_start(
                        block,
                        json!({"type": "tool_use", "id": id, "name": name, "input": {}}),
                    ));
                    ToolBlock {
                        block,
                        id,
                        name,
                        args: String::new(),
                    }
                });
                if let Some(f) = tc.get("function") {
                    if let Some(n) = f.get("name").and_then(Value::as_str) {
                        if entry.name.is_empty() {
                            entry.name = n.to_string();
                        }
                    }
                    if let Some(a) = f.get("arguments").and_then(Value::as_str) {
                        if !a.is_empty() {
                            entry.args.push_str(a);
                            events.push(anthropic_content_block_delta(
                                entry.block,
                                json!({"type": "input_json_delta", "partial_json": a}),
                            ));
                        }
                    }
                }
            }
        }

        if let Some(fr) = choice.get("finish_reason").and_then(Value::as_str) {
            self.stop_reason = Some(fr.to_string());
        }
        events
    }

    fn finalize(&mut self) -> Vec<Value> {
        let mut events = Vec::new();
        self.start(&mut events);

        if let Some(index) = self.text.take() {
            events.push(anthropic_content_block_stop(index));
        }
        let mut tool_indices: Vec<usize> = self.tools.keys().copied().collect();
        tool_indices.sort_unstable();
        for idx in tool_indices {
            if let Some(tool) = self.tools.remove(&idx) {
                events.push(anthropic_content_block_stop(tool.block));
            }
        }
        self.flush_reasoning(&mut events);
        if let Some(index) = self.thinking.take() {
            events.push(anthropic_content_block_stop(index));
        }

        let stop_reason = self.map_stop_reason().to_string();
        events.push(anthropic_message_delta(&stop_reason, &self.usage));
        events.push(json!({"type": "message_stop"}));
        events
    }
}

// ---------------------------------------------------------------------------
// Anthropic Messages -> OpenAI chat completions
// ---------------------------------------------------------------------------

struct AoMachine {
    id: String,
    model: String,
    created: u64,
    started: bool,
    tools: HashMap<usize, ToolBlock>,
    usage: TokenUsage,
    stop_reason: Option<String>,
    saw_tools: bool,
}

impl AoMachine {
    fn new(model: String) -> Self {
        Self {
            id: new_id("chatcmpl-stream"),
            model,
            created: now_ts(),
            started: false,
            tools: HashMap::new(),
            usage: TokenUsage::default(),
            stop_reason: None,
            saw_tools: false,
        }
    }

    fn first_chunk(&mut self, events: &mut Vec<Value>) {
        if !self.started {
            self.started = true;
            events.push(openai_chunk(
                &self.id,
                &self.model,
                self.created,
                json!({"role": "assistant"}),
                None,
            ));
        }
    }

    fn map_finish_reason(&self) -> String {
        match self.stop_reason.as_deref() {
            Some("tool_use") => "tool_calls".to_string(),
            Some("max_tokens") | Some("max_tokens_reached") => "length".to_string(),
            Some(other) if other != "end_turn" && other != "stop_sequence" => other.to_string(),
            _ => {
                if self.saw_tools || !self.tools.is_empty() {
                    "tool_calls".to_string()
                } else {
                    "stop".to_string()
                }
            }
        }
    }
}

impl Machine for AoMachine {
    fn process(&mut self, frame: &SseFrame) -> Vec<Value> {
        let mut events = Vec::new();
        if frame.is_done() {
            return events;
        }
        let Some(value) = frame.json() else {
            return events;
        };
        let ty = value.get("type").and_then(Value::as_str).unwrap_or("");

        match ty {
            "message_start" => {
                if let Some(msg) = value.get("message") {
                    if let Some(id) = msg.get("id").and_then(Value::as_str) {
                        self.id = id.to_string();
                    }
                    if let Some(u) = msg.get("usage") {
                        merge_usage(&mut self.usage, parse_usage(u));
                    }
                }
                self.first_chunk(&mut events);
            }
            "content_block_start" => {
                let index = value.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                let block = value.get("content_block").unwrap_or(&Value::Null);
                if block.get("type").and_then(Value::as_str) == Some("tool_use") {
                    self.saw_tools = true;
                    let id = block
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let name = block
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    self.tools.insert(
                        index,
                        ToolBlock {
                            block: index as u32,
                            id,
                            name,
                            args: String::new(),
                        },
                    );
                    self.first_chunk(&mut events);
                    let tool = self.tools.get(&index).cloned().unwrap();
                    events.push(openai_chunk(
                        &self.id,
                        &self.model,
                        self.created,
                        json!({"tool_calls": [{
                            "index": index,
                            "id": tool.id,
                            "type": "function",
                            "function": {"name": tool.name, "arguments": ""}
                        }]}),
                        None,
                    ));
                }
            }
            "content_block_delta" => {
                let index = value.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                let delta = value.get("delta").unwrap_or(&Value::Null);
                match delta.get("type").and_then(Value::as_str) {
                    Some("text_delta") => {
                        if let Some(t) = delta.get("text").and_then(Value::as_str) {
                            if !t.is_empty() {
                                self.first_chunk(&mut events);
                                events.push(openai_chunk(
                                    &self.id,
                                    &self.model,
                                    self.created,
                                    json!({"content": t}),
                                    None,
                                ));
                            }
                        }
                    }
                    Some("input_json_delta") => {
                        if let Some(pj) = delta.get("partial_json").and_then(Value::as_str) {
                            if !pj.is_empty() {
                                if let Some(tool) = self.tools.get_mut(&index) {
                                    tool.args.push_str(pj);
                                }
                                events.push(openai_chunk(
                                    &self.id,
                                    &self.model,
                                    self.created,
                                    json!({"tool_calls": [{
                                        "index": index,
                                        "function": {"arguments": pj}
                                    }]}),
                                    None,
                                ));
                            }
                        }
                    }
                    Some("thinking_delta") => {
                        if let Some(t) = delta.get("thinking").and_then(Value::as_str) {
                            if !t.is_empty() {
                                self.first_chunk(&mut events);
                                events.push(openai_chunk(
                                    &self.id,
                                    &self.model,
                                    self.created,
                                    json!({"reasoning_content": t}),
                                    None,
                                ));
                            }
                        }
                    }
                    _ => {}
                }
            }
            "message_delta" => {
                if let Some(d) = value.get("delta") {
                    if let Some(sr) = d.get("stop_reason").and_then(Value::as_str) {
                        self.stop_reason = Some(sr.to_string());
                    }
                }
                if let Some(u) = value.get("usage") {
                    merge_usage(&mut self.usage, parse_usage(u));
                }
            }
            _ => {}
        }
        events
    }

    fn finalize(&mut self) -> Vec<Value> {
        let mut events = Vec::new();
        self.first_chunk(&mut events);

        let finish = self.map_finish_reason();
        let mut usage = None;
        if self.usage.input > 0 || self.usage.output > 0 {
            usage = Some(json!({
                "prompt_tokens": self.usage.input,
                "completion_tokens": self.usage.output,
                "total_tokens": self.usage.input + self.usage.output
            }));
        }
        let mut chunk = json!({
            "id": self.id,
            "object": "chat.completion.chunk",
            "created": self.created,
            "model": self.model,
            "choices": [{"index": 0, "delta": {}, "finish_reason": finish}]
        });
        if let Some(u) = usage {
            chunk["usage"] = u;
        }
        events.push(chunk);
        events.push(json!("[DONE]"));
        events
    }
}

// ---------------------------------------------------------------------------
// Anthropic Messages -> OpenAI Responses events
// ---------------------------------------------------------------------------

struct A2RMachine {
    model: String,
    response_id: String,
    created_at: u64,
    started: bool,
    next_output: u32,
    text_block: Option<usize>,
    msg_item_id: String,
    msg_output_index: u32,
    msg_text: String,
    tools: HashMap<usize, ResponsesTool>,
    items: Vec<Value>,
    usage: TokenUsage,
    stop_reason: Option<String>,
}

impl A2RMachine {
    fn new(model: String) -> Self {
        Self {
            model,
            response_id: new_id("resp_stream"),
            created_at: now_ts(),
            started: false,
            next_output: 0,
            text_block: None,
            msg_item_id: String::new(),
            msg_output_index: 0,
            msg_text: String::new(),
            tools: HashMap::new(),
            items: Vec::new(),
            usage: TokenUsage::default(),
            stop_reason: None,
        }
    }

    fn ensure_started(&mut self, events: &mut Vec<Value>) {
        if !self.started {
            self.started = true;
            events.push(responses_created(
                &self.response_id,
                &self.model,
                self.created_at,
            ));
        }
    }

    fn message_item(&self, status: &str) -> Value {
        json!({
            "type": "message",
            "id": self.msg_item_id,
            "status": status,
            "role": "assistant",
            "content": [{"type": "output_text", "text": self.msg_text, "annotations": []}]
        })
    }

    fn function_call_item(&self, tool: &ResponsesTool, status: &str) -> Value {
        json!({
            "type": "function_call",
            "id": tool.item_id,
            "call_id": tool.item_id,
            "name": tool.name,
            "arguments": tool.args,
            "status": status
        })
    }
}

impl Machine for A2RMachine {
    fn process(&mut self, frame: &SseFrame) -> Vec<Value> {
        let mut events = Vec::new();
        if frame.is_done() {
            return events;
        }
        let Some(value) = frame.json() else {
            return events;
        };
        let ty = value.get("type").and_then(Value::as_str).unwrap_or("");

        match ty {
            "message_start" => {
                self.ensure_started(&mut events);
                if let Some(msg) = value.get("message") {
                    if let Some(id) = msg.get("id").and_then(Value::as_str) {
                        self.response_id = id.to_string();
                    }
                    if let Some(u) = msg.get("usage") {
                        merge_usage(&mut self.usage, parse_usage(u));
                    }
                }
            }
            "content_block_start" => {
                self.ensure_started(&mut events);
                let index = value.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                let block = value.get("content_block").unwrap_or(&Value::Null);
                match block.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        self.text_block = Some(index);
                        self.msg_item_id = format!("msg_{index}");
                        self.msg_output_index = self.next_output;
                        self.next_output += 1;
                        self.msg_text = String::new();
                        events.push(responses_output_item_added(
                            self.msg_output_index,
                            json!({
                                "type": "message",
                                "id": self.msg_item_id,
                                "status": "in_progress",
                                "role": "assistant",
                                "content": []
                            }),
                        ));
                        events.push(json!({
                            "type": "response.content_part.added",
                            "item_id": self.msg_item_id,
                            "output_index": self.msg_output_index,
                            "content_index": 0,
                            "part": {"type": "output_text", "text": "", "annotations": []}
                        }));
                    }
                    Some("tool_use") => {
                        let name = block
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        let id = block
                            .get("id")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        let item_id = if id.is_empty() {
                            format!("call_{index}")
                        } else {
                            id
                        };
                        let output_index = self.next_output;
                        self.next_output += 1;
                        self.tools.insert(
                            index,
                            ResponsesTool {
                                item_id: item_id.clone(),
                                name,
                                args: String::new(),
                                output_index,
                            },
                        );
                        events.push(responses_output_item_added(
                            output_index,
                            json!({
                                "type": "function_call",
                                "id": item_id,
                                "call_id": item_id,
                                "name": self.tools[&index].name,
                                "arguments": "",
                                "status": "in_progress"
                            }),
                        ));
                    }
                    _ => {}
                }
            }
            "content_block_delta" => {
                let index = value.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                let delta = value.get("delta").unwrap_or(&Value::Null);
                match delta.get("type").and_then(Value::as_str) {
                    Some("text_delta") => {
                        if let Some(t) = delta.get("text").and_then(Value::as_str) {
                            self.ensure_started(&mut events);
                            if self.text_block.is_none() {
                                // tolerate missing content_block_start
                                self.process(&SseFrame {
                                    event: Some("content_block_start".to_string()),
                                    data: json!({"type": "content_block_start", "index": index, "content_block": {"type": "text", "text": ""}}).to_string(),
                                })
                                .into_iter()
                                .for_each(|e| events.push(e));
                            }
                            self.msg_text.push_str(t);
                            events.push(json!({
                                "type": "response.output_text.delta",
                                "item_id": self.msg_item_id,
                                "output_index": self.msg_output_index,
                                "content_index": 0,
                                "delta": t
                            }));
                        }
                    }
                    Some("input_json_delta") => {
                        if let Some(pj) = delta.get("partial_json").and_then(Value::as_str) {
                            if let Some(tool) = self.tools.get_mut(&index) {
                                tool.args.push_str(pj);
                                events.push(json!({
                                    "type": "response.function_call_arguments.delta",
                                    "item_id": tool.item_id,
                                    "output_index": tool.output_index,
                                    "delta": pj
                                }));
                            }
                        }
                    }
                    _ => {}
                }
            }
            "content_block_stop" => {
                self.ensure_started(&mut events);
                let index = value.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                if self.text_block == Some(index) {
                    self.text_block = None;
                    events.push(json!({
                        "type": "response.output_text.done",
                        "item_id": self.msg_item_id,
                        "output_index": self.msg_output_index,
                        "content_index": 0,
                        "text": self.msg_text
                    }));
                    events.push(responses_output_item_done(
                        self.msg_output_index,
                        self.message_item("completed"),
                    ));
                    self.items.push(self.message_item("completed"));
                } else if let Some(tool) = self.tools.remove(&index) {
                    events.push(json!({
                        "type": "response.function_call_arguments.done",
                        "item_id": tool.item_id,
                        "output_index": tool.output_index,
                        "arguments": tool.args
                    }));
                    let item = self.function_call_item(&tool, "completed");
                    events.push(responses_output_item_done(tool.output_index, item.clone()));
                    self.items.push(item);
                }
            }
            "message_delta" => {
                if let Some(d) = value.get("delta") {
                    if let Some(sr) = d.get("stop_reason").and_then(Value::as_str) {
                        self.stop_reason = Some(sr.to_string());
                    }
                }
                if let Some(u) = value.get("usage") {
                    merge_usage(&mut self.usage, parse_usage(u));
                }
            }
            _ => {}
        }
        events
    }

    fn finalize(&mut self) -> Vec<Value> {
        let mut events = Vec::new();
        self.ensure_started(&mut events);

        // close any still-open message block
        if let Some(index) = self.text_block.take() {
            events.push(json!({
                "type": "response.output_text.done",
                "item_id": self.msg_item_id,
                "output_index": self.msg_output_index,
                "content_index": 0,
                "text": self.msg_text
            }));
            events.push(responses_output_item_done(
                self.msg_output_index,
                self.message_item("completed"),
            ));
            let _ = index;
            self.items.push(self.message_item("completed"));
        }
        let mut indices: Vec<usize> = self.tools.keys().copied().collect();
        indices.sort_unstable();
        for idx in indices {
            if let Some(tool) = self.tools.remove(&idx) {
                events.push(json!({
                    "type": "response.function_call_arguments.done",
                    "item_id": tool.item_id,
                    "output_index": tool.output_index,
                    "arguments": tool.args
                }));
                let item = self.function_call_item(&tool, "completed");
                events.push(responses_output_item_done(tool.output_index, item.clone()));
                self.items.push(item);
            }
        }

        events.push(json!({
            "type": "response.completed",
            "response": {
                "id": self.response_id,
                "object": "response",
                "created_at": self.created_at,
                "status": "completed",
                "model": self.model,
                "output": self.items,
                "parallel_tool_calls": true,
                "usage": responses_usage(&self.usage)
            }
        }));
        events
    }
}

// ---------------------------------------------------------------------------
// OpenAI chat completions -> OpenAI Responses events
// ---------------------------------------------------------------------------

struct O2RMachine {
    model: String,
    response_id: String,
    created_at: u64,
    started: bool,
    next_output: u32,
    msg_item_id: String,
    msg_output_index: u32,
    msg_open: bool,
    msg_text: String,
    tools: HashMap<usize, ResponsesTool>,
    items: Vec<Value>,
    usage: TokenUsage,
    stop_reason: Option<String>,
}

impl O2RMachine {
    fn new(model: String) -> Self {
        Self {
            model,
            response_id: new_id("resp_stream"),
            created_at: now_ts(),
            started: false,
            next_output: 0,
            msg_item_id: String::new(),
            msg_output_index: 0,
            msg_open: false,
            msg_text: String::new(),
            tools: HashMap::new(),
            items: Vec::new(),
            usage: TokenUsage::default(),
            stop_reason: None,
        }
    }

    fn ensure_started(&mut self, events: &mut Vec<Value>, id: Option<&str>) {
        if !self.started {
            self.started = true;
            if let Some(id) = id.filter(|s| !s.is_empty()) {
                self.response_id = id.to_string();
            }
            events.push(responses_created(
                &self.response_id,
                &self.model,
                self.created_at,
            ));
        }
    }

    fn message_item(&self, status: &str) -> Value {
        json!({
            "type": "message",
            "id": self.msg_item_id,
            "status": status,
            "role": "assistant",
            "content": [{"type": "output_text", "text": self.msg_text, "annotations": []}]
        })
    }

    fn function_call_item(&self, tool: &ResponsesTool, status: &str) -> Value {
        json!({
            "type": "function_call",
            "id": tool.item_id,
            "call_id": tool.item_id,
            "name": tool.name,
            "arguments": tool.args,
            "status": status
        })
    }

    fn open_message(&mut self, events: &mut Vec<Value>) {
        if !self.msg_open {
            self.msg_open = true;
            self.msg_item_id = format!("msg_{}", self.next_output);
            self.msg_output_index = self.next_output;
            self.next_output += 1;
            self.msg_text = String::new();
            events.push(responses_output_item_added(
                self.msg_output_index,
                json!({
                    "type": "message",
                    "id": self.msg_item_id,
                    "status": "in_progress",
                    "role": "assistant",
                    "content": []
                }),
            ));
            events.push(json!({
                "type": "response.content_part.added",
                "item_id": self.msg_item_id,
                "output_index": self.msg_output_index,
                "content_index": 0,
                "part": {"type": "output_text", "text": "", "annotations": []}
            }));
        }
    }

    fn close_message(&mut self, events: &mut Vec<Value>) {
        if self.msg_open {
            self.msg_open = false;
            events.push(json!({
                "type": "response.output_text.done",
                "item_id": self.msg_item_id,
                "output_index": self.msg_output_index,
                "content_index": 0,
                "text": self.msg_text
            }));
            let item = self.message_item("completed");
            events.push(responses_output_item_done(
                self.msg_output_index,
                item.clone(),
            ));
            self.items.push(item);
        }
    }

    fn close_tool(&mut self, events: &mut Vec<Value>, tool: ResponsesTool) {
        events.push(json!({
            "type": "response.function_call_arguments.done",
            "item_id": tool.item_id,
            "output_index": tool.output_index,
            "arguments": tool.args
        }));
        let item = self.function_call_item(&tool, "completed");
        events.push(responses_output_item_done(tool.output_index, item.clone()));
        self.items.push(item);
    }
}

impl Machine for O2RMachine {
    fn process(&mut self, frame: &SseFrame) -> Vec<Value> {
        let mut events = Vec::new();
        if frame.is_done() {
            return events;
        }
        let Some(value) = frame.json() else {
            return events;
        };
        let id = value.get("id").and_then(Value::as_str);
        if let Some(u) = value.get("usage") {
            merge_usage(&mut self.usage, parse_usage(u));
        }
        self.ensure_started(&mut events, id);

        let Some(choices) = value.get("choices").and_then(Value::as_array) else {
            return events;
        };
        let Some(choice) = choices.first() else {
            return events;
        };
        let delta = choice.get("delta").unwrap_or(&Value::Null);

        if let Some(t) = delta.get("content").and_then(Value::as_str) {
            if !t.is_empty() {
                self.open_message(&mut events);
                self.msg_text.push_str(t);
                events.push(json!({
                    "type": "response.output_text.delta",
                    "item_id": self.msg_item_id,
                    "output_index": self.msg_output_index,
                    "content_index": 0,
                    "delta": t
                }));
            }
        }

        if let Some(tool_calls) = delta.get("tool_calls").and_then(Value::as_array) {
            for tc in tool_calls {
                let idx = tc.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                let entry = self.tools.entry(idx).or_insert_with(|| {
                    let output_index = self.next_output;
                    self.next_output += 1;
                    let item_id = format!("call_{idx}");
                    let name = tc
                        .get("function")
                        .and_then(|f| f.get("name"))
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    events.push(responses_output_item_added(
                        output_index,
                        json!({
                            "type": "function_call",
                            "id": item_id,
                            "call_id": item_id,
                            "name": name,
                            "arguments": "",
                            "status": "in_progress"
                        }),
                    ));
                    ResponsesTool {
                        item_id,
                        name,
                        args: String::new(),
                        output_index,
                    }
                });
                if let Some(f) = tc.get("function") {
                    if let Some(n) = f.get("name").and_then(Value::as_str) {
                        if entry.name.is_empty() {
                            entry.name = n.to_string();
                        }
                    }
                    if let Some(a) = f.get("arguments").and_then(Value::as_str) {
                        if !a.is_empty() {
                            entry.args.push_str(a);
                            events.push(json!({
                                "type": "response.function_call_arguments.delta",
                                "item_id": entry.item_id,
                                "output_index": entry.output_index,
                                "delta": a
                            }));
                        }
                    }
                }
            }
        }

        if let Some(fr) = choice.get("finish_reason").and_then(Value::as_str) {
            self.stop_reason = Some(fr.to_string());
        }
        events
    }

    fn finalize(&mut self) -> Vec<Value> {
        let mut events = Vec::new();
        self.ensure_started(&mut events, None);
        self.close_message(&mut events);
        let mut indices: Vec<usize> = self.tools.keys().copied().collect();
        indices.sort_unstable();
        for idx in indices {
            if let Some(tool) = self.tools.remove(&idx) {
                self.close_tool(&mut events, tool);
            }
        }
        events.push(json!({
            "type": "response.completed",
            "response": {
                "id": self.response_id,
                "object": "response",
                "created_at": self.created_at,
                "status": "completed",
                "model": self.model,
                "output": self.items,
                "parallel_tool_calls": true,
                "usage": responses_usage(&self.usage)
            }
        }));
        events
    }
}

// ---------------------------------------------------------------------------
// public entry points
// ---------------------------------------------------------------------------

/// OpenAI chat-completions upstream -> Anthropic Messages SSE stream.
pub fn anthropic_from_openai(resp: reqwest::Response, model: String) -> UpstreamStream {
    build(resp, OaMachine::new(model))
}

/// Anthropic Messages upstream -> OpenAI chat-completions SSE stream.
pub fn openai_from_anthropic(resp: reqwest::Response, model: String) -> UpstreamStream {
    build(resp, AoMachine::new(model))
}

/// Anthropic Messages upstream -> OpenAI Responses SSE stream.
pub fn responses_from_anthropic(resp: reqwest::Response, model: String) -> UpstreamStream {
    build(resp, A2RMachine::new(model))
}

/// OpenAI chat-completions upstream -> OpenAI Responses SSE stream.
pub fn responses_from_openai(resp: reqwest::Response, model: String) -> UpstreamStream {
    build(resp, O2RMachine::new(model))
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(data: Value) -> SseFrame {
        SseFrame {
            event: None,
            data: data.to_string(),
        }
    }

    fn oai_chunk(delta: Value, finish: Option<&str>) -> SseFrame {
        frame(json!({
            "id": "cmpl_1",
            "object": "chat.completion.chunk",
            "created": 1,
            "model": "m",
            "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]
        }))
    }

    fn anthro(ty: &str, extra: Value) -> SseFrame {
        let mut v = json!({"type": ty});
        if let Some(o) = extra.as_object() {
            v.as_object_mut().unwrap().extend(o.clone());
        }
        frame(v)
    }

    fn run(mut m: impl Machine, frames: Vec<SseFrame>) -> Vec<Value> {
        let mut out = Vec::new();
        for f in &frames {
            out.extend(m.process(f));
        }
        out.extend(m.finalize());
        out
    }

    fn types(events: &[Value]) -> Vec<String> {
        events
            .iter()
            .filter_map(|e| e.get("type").and_then(Value::as_str).map(str::to_string))
            .collect()
    }

    // ---- OpenAI -> Anthropic (text) ----

    #[test]
    fn oa_text_stream() {
        let events = run(
            OaMachine::new("m".to_string()),
            vec![
                oai_chunk(json!({"role": "assistant", "content": "Hel"}), None),
                oai_chunk(json!({"content": "lo"}), None),
                oai_chunk(json!({}), Some("stop")),
            ],
        );
        let t = types(&events);
        assert_eq!(
            t,
            [
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop",
            ]
        );
        assert_eq!(events[0]["message"]["model"], "m");
        assert_eq!(events[2]["delta"]["text"], "Hel");
        assert_eq!(events[3]["delta"]["text"], "lo");
        let md = events
            .iter()
            .find(|e| e["type"] == "message_delta")
            .unwrap();
        assert_eq!(md["delta"]["stop_reason"], "end_turn");
    }

    // ---- OpenAI -> Anthropic (tool calls with partial JSON) ----

    #[test]
    fn oa_tool_calls_accumulate() {
        let events = run(
            OaMachine::new("m".to_string()),
            vec![
                oai_chunk(
                    json!({"tool_calls": [{
                        "index": 0, "id": "call_1", "type": "function",
                        "function": {"name": "weather", "arguments": ""}
                    }]}),
                    None,
                ),
                oai_chunk(
                    json!({"tool_calls": [{"index": 0, "function": {"arguments": "{\"city\":\"sp\""}}]}),
                    None,
                ),
                oai_chunk(
                    json!({"tool_calls": [{"index": 0, "function": {"arguments": "\"}"}}]}),
                    None,
                ),
                oai_chunk(json!({}), Some("tool_calls")),
            ],
        );
        let start = events
            .iter()
            .find(|e| e["type"] == "content_block_start")
            .unwrap();
        assert_eq!(start["content_block"]["type"], "tool_use");
        assert_eq!(start["content_block"]["id"], "call_1");
        assert_eq!(start["content_block"]["name"], "weather");
        let md = events
            .iter()
            .find(|e| e["type"] == "message_delta")
            .unwrap();
        assert_eq!(md["delta"]["stop_reason"], "tool_use");
        let partials: Vec<_> = events
            .iter()
            .filter(|e| e["type"] == "content_block_delta")
            .filter_map(|e| e["delta"]["partial_json"].as_str())
            .collect();
        assert_eq!(partials, ["{\"city\":\"sp\"", "\"}"]);
    }

    // ---- OpenAI -> Anthropic (reasoning_content -> thinking block) ----

    #[test]
    fn oa_reasoning_becomes_thinking() {
        let events = run(
            OaMachine::new("m".to_string()),
            vec![
                oai_chunk(json!({"reasoning_content": "hmm"}), None),
                oai_chunk(json!({"content": "answer"}), None),
                oai_chunk(json!({}), Some("stop")),
            ],
        );
        let thinking = events
            .iter()
            .find(|e| e["content_block"]["type"] == "thinking")
            .unwrap();
        assert_eq!(thinking["content_block"]["type"], "thinking");
        let td = events
            .iter()
            .find(|e| e["delta"]["type"] == "thinking_delta")
            .unwrap();
        assert_eq!(td["delta"]["thinking"], "hmm");
        // thinking block precedes the text block
        let text_start = events
            .iter()
            .position(|e| e["content_block"]["type"] == "text")
            .unwrap();
        let thinking_start = events
            .iter()
            .position(|e| e["content_block"]["type"] == "thinking")
            .unwrap();
        assert!(thinking_start < text_start);
    }

    // ---- Anthropic -> OpenAI (text) ----

    #[test]
    fn ao_text_stream() {
        let events = run(
            AoMachine::new("claude".to_string()),
            vec![
                anthro(
                    "message_start",
                    json!({"message": {"id": "msg_1", "model": "claude", "usage": {"input_tokens": 3, "output_tokens": 0}}}),
                ),
                anthro(
                    "content_block_start",
                    json!({"index": 0, "content_block": {"type": "text", "text": ""}}),
                ),
                anthro(
                    "content_block_delta",
                    json!({"index": 0, "delta": {"type": "text_delta", "text": "hi"}}),
                ),
                anthro("content_block_stop", json!({"index": 0})),
                anthro(
                    "message_delta",
                    json!({"delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 2}}),
                ),
                anthro("message_stop", json!({})),
            ],
        );
        assert_eq!(events[0]["choices"][0]["delta"]["role"], "assistant");
        assert_eq!(events[1]["choices"][0]["delta"]["content"], "hi");
        let last = events.last().unwrap();
        assert_eq!(last, &json!("[DONE]"));
        let finish = events
            .iter()
            .find(|e| e.get("choices").is_some() && e["choices"][0]["finish_reason"].is_string())
            .unwrap();
        assert_eq!(finish["choices"][0]["finish_reason"], "stop");
        assert_eq!(finish["usage"]["completion_tokens"], 2);
    }

    // ---- Anthropic -> OpenAI (tool calls) ----

    #[test]
    fn ao_tool_calls_stream() {
        let events = run(
            AoMachine::new("claude".to_string()),
            vec![
                anthro("message_start", json!({"message": {"id": "msg_1"}})),
                anthro(
                    "content_block_start",
                    json!({"index": 0, "content_block": {"type": "tool_use", "id": "toolu_1", "name": "weather", "input": {}}}),
                ),
                anthro(
                    "content_block_delta",
                    json!({"index": 0, "delta": {"type": "input_json_delta", "partial_json": "{\"c\":"}}),
                ),
                anthro("content_block_stop", json!({"index": 0})),
                anthro(
                    "message_delta",
                    json!({"delta": {"stop_reason": "tool_use"}}),
                ),
                anthro("message_stop", json!({})),
            ],
        );
        let tool_chunk = events
            .iter()
            .find(|e| {
                e.get("choices").is_some() && e["choices"][0]["delta"]["tool_calls"].is_array()
            })
            .unwrap();
        let tc = &tool_chunk["choices"][0]["delta"]["tool_calls"][0];
        assert_eq!(tc["id"], "toolu_1");
        assert_eq!(tc["function"]["name"], "weather");
        let finish = events
            .iter()
            .find(|e| e.get("choices").is_some() && e["choices"][0]["finish_reason"].is_string())
            .unwrap();
        assert_eq!(finish["choices"][0]["finish_reason"], "tool_calls");
    }

    // ---- Anthropic -> Responses ----

    #[test]
    fn a2r_full_sequence() {
        let events = run(
            A2RMachine::new("claude".to_string()),
            vec![
                anthro(
                    "message_start",
                    json!({"message": {"id": "msg_1", "usage": {"input_tokens": 2, "output_tokens": 0}}}),
                ),
                anthro(
                    "content_block_start",
                    json!({"index": 0, "content_block": {"type": "text", "text": ""}}),
                ),
                anthro(
                    "content_block_delta",
                    json!({"index": 0, "delta": {"type": "text_delta", "text": "olá"}}),
                ),
                anthro("content_block_stop", json!({"index": 0})),
                anthro(
                    "message_delta",
                    json!({"delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 3}}),
                ),
                anthro("message_stop", json!({})),
            ],
        );
        let t = types(&events);
        assert_eq!(
            t,
            [
                "response.created",
                "response.output_item.added",
                "response.content_part.added",
                "response.output_text.delta",
                "response.output_text.done",
                "response.output_item.done",
                "response.completed",
            ]
        );
        assert_eq!(events[2]["part"]["type"], "output_text");
        assert_eq!(events[3]["delta"], "olá");
        let done = events[4].clone();
        assert_eq!(done["text"], "olá");
        let completed = events.last().unwrap();
        assert_eq!(completed["response"]["status"], "completed");
        assert_eq!(
            completed["response"]["output"][0]["content"][0]["text"],
            "olá"
        );
        assert_eq!(completed["response"]["usage"]["input_tokens"], 2);
    }

    #[test]
    fn a2r_tool_sequence() {
        let events = run(
            A2RMachine::new("claude".to_string()),
            vec![
                anthro("message_start", json!({"message": {"id": "msg_1"}})),
                anthro(
                    "content_block_start",
                    json!({"index": 0, "content_block": {"type": "tool_use", "id": "toolu_9", "name": "weather", "input": {}}}),
                ),
                anthro(
                    "content_block_delta",
                    json!({"index": 0, "delta": {"type": "input_json_delta", "partial_json": "{\"city\":\"sp\"}"}}),
                ),
                anthro("content_block_stop", json!({"index": 0})),
                anthro(
                    "message_delta",
                    json!({"delta": {"stop_reason": "tool_use"}}),
                ),
                anthro("message_stop", json!({})),
            ],
        );
        let t = types(&events);
        assert_eq!(
            t,
            [
                "response.created",
                "response.output_item.added",
                "response.function_call_arguments.delta",
                "response.function_call_arguments.done",
                "response.output_item.done",
                "response.completed",
            ]
        );
        let item = events[1]["item"].clone();
        assert_eq!(item["type"], "function_call");
        assert_eq!(item["name"], "weather");
        let completed = events.last().unwrap();
        let out = &completed["response"]["output"][0];
        assert_eq!(out["type"], "function_call");
        assert_eq!(out["arguments"], "{\"city\":\"sp\"}");
    }

    // ---- OpenAI chat -> Responses ----

    #[test]
    fn o2r_full_sequence() {
        let events = run(
            O2RMachine::new("gpt".to_string()),
            vec![
                oai_chunk(json!({"role": "assistant", "content": "hi"}), None),
                oai_chunk(json!({"content": " there"}), None),
                oai_chunk(json!({}), Some("stop")),
            ],
        );
        let t = types(&events);
        assert_eq!(
            t,
            [
                "response.created",
                "response.output_item.added",
                "response.content_part.added",
                "response.output_text.delta",
                "response.output_text.delta",
                "response.output_text.done",
                "response.output_item.done",
                "response.completed",
            ]
        );
        let completed = events.last().unwrap();
        assert_eq!(
            completed["response"]["output"][0]["content"][0]["text"],
            "hi there"
        );
    }

    #[test]
    fn o2r_tool_sequence() {
        let events = run(
            O2RMachine::new("gpt".to_string()),
            vec![
                oai_chunk(
                    json!({"tool_calls": [{"index": 0, "id": "c1", "type": "function", "function": {"name": "w", "arguments": ""}}]}),
                    None,
                ),
                oai_chunk(
                    json!({"tool_calls": [{"index": 0, "function": {"arguments": "{}"}}]}),
                    None,
                ),
                oai_chunk(json!({}), Some("tool_calls")),
            ],
        );
        let t = types(&events);
        assert_eq!(
            t,
            [
                "response.created",
                "response.output_item.added",
                "response.function_call_arguments.delta",
                "response.function_call_arguments.done",
                "response.output_item.done",
                "response.completed",
            ]
        );
        let completed = events.last().unwrap();
        let out = &completed["response"]["output"][0];
        assert_eq!(out["arguments"], "{}");
        assert_eq!(out["name"], "w");
    }

    // ---- empty stream still emits a valid terminal sequence ----

    #[test]
    fn oa_empty_stream_still_completes() {
        let events = run(OaMachine::new("m".to_string()), vec![]);
        let t = types(&events);
        assert_eq!(t, ["message_start", "message_delta", "message_stop"]);
    }

    #[test]
    fn a2r_empty_stream_still_completes() {
        let events = run(A2RMachine::new("m".to_string()), vec![]);
        let t = types(&events);
        assert_eq!(t, ["response.created", "response.completed"]);
    }
}
