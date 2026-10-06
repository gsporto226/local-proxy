//! Format-neutral intermediate representation (IR) for chat requests,
//! responses, and stream events.
//!
//! Each wire format (`anthropic`, `openai` chat, `openai-responses`) has one
//! codec module that decodes its JSON into these types and encodes them back.
//! Translating A -> B is `decode_A` then `encode_B`; same-format traffic skips
//! the IR entirely and stays a raw passthrough.
//!
//! The IR is an allowlist: a field reaches an upstream only if it is modeled
//! here and the target encoder writes it. Anything a client sends that the IR
//! does not model (`metadata`, `context_management`, ...) is dropped on
//! translation instead of leaking into a format that rejects it.

use std::collections::BTreeMap;

use serde_json::Value;

pub use crate::translate::TokenUsage;

/// Wire format of a request or response body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// Anthropic Messages API.
    Anthropic,
    /// `OpenAI` Chat Completions API.
    Openai,
    /// `OpenAI` Responses API.
    Responses,
}

/// A chat request.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Request {
    /// Model id as the client sent it (routing rewrites it later).
    pub model: String,
    /// System / developer instructions, in order.
    pub system: Vec<Block>,
    /// Conversation turns.
    pub messages: Vec<Message>,
    /// Tools the model may call.
    pub tools: Vec<Tool>,
    /// Tool-use policy; `None` means the format's default.
    pub tool_choice: Option<ToolChoice>,
    /// Output token cap.
    pub max_tokens: Option<u64>,
    /// Sampling temperature.
    pub temperature: Option<f64>,
    /// Nucleus sampling.
    pub top_p: Option<f64>,
    /// Stop sequences.
    pub stop: Vec<String>,
    /// Reasoning depth.
    pub effort: Option<Effort>,
    /// Whether the client wants a stream.
    pub stream: bool,
    /// Prompt-cache routing key (Responses `prompt_cache_key`). Requests that
    /// share a key and a byte-identical prefix hit the same cache.
    pub cache_key: Option<String>,
    /// Everything the source format sent that the IR does not model.
    pub ext: Ext,
}

/// A content part plus its prompt-cache breakpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    /// The content.
    pub part: Part,
    /// Cache breakpoint ending at this block (Anthropic `cache_control`).
    /// Formats with automatic prefix caching ignore it.
    pub cache: Option<Cache>,
}

impl Block {
    /// A block without a cache breakpoint.
    #[must_use]
    pub const fn new(part: Part) -> Self {
        Self { part, cache: None }
    }
}

/// A prompt-cache breakpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cache {
    /// Time to live as the client sent it (`5m`, `1h`); `None` is the default.
    pub ttl: Option<String>,
}

/// Namespaced feature store: `"<format>:<field>"` -> raw JSON.
///
/// Keys look like `anthropic:context_management`. Decoders put every field they do not model
/// here; encoders never emit it wholesale, they probe the keys they understand.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Ext(BTreeMap<String, Value>);

impl Ext {
    /// Store `value` under `key`.
    pub fn insert(&mut self, key: impl Into<String>, value: Value) {
        self.0.insert(key.into(), value);
    }

    /// The value under `key`, if present.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.0.get(key)
    }

    /// Whether `key` is present.
    #[must_use]
    pub fn has(&self, key: &str) -> bool {
        self.0.contains_key(key)
    }

    /// Entries whose key starts with `<format>:`.
    pub fn namespace<'a>(&'a self, format: &'a str) -> impl Iterator<Item = (&'a str, &'a Value)> {
        self.0.iter().filter_map(move |(k, v)| {
            k.strip_prefix(format)
                .and_then(|rest| rest.strip_prefix(':'))
                .map(|field| (field, v))
        })
    }
}

/// Who authored a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// The user (tool results ride on user turns).
    User,
    /// The model.
    Assistant,
}

/// One conversation turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    /// Author.
    pub role: Role,
    /// Content in order.
    pub content: Vec<Block>,
}

/// A piece of message content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Part {
    /// Plain text.
    Text(String),
    /// An image input.
    Image(Image),
    /// A tool invocation by the assistant.
    ToolCall(ToolCall),
    /// The result of a tool call, sent back by the user side.
    ToolResult(ToolResult),
    /// Model reasoning. `signature` is Anthropic's opaque thinking signature;
    /// encoders for formats that cannot carry reasoning drop the part.
    Reasoning {
        /// Reasoning text (may be empty when only a signature exists).
        text: String,
        /// Opaque provider signature, if any.
        signature: Option<String>,
    },
}

/// Image source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Image {
    /// Remote or `data:` URL.
    Url(String),
    /// Inline base64 bytes.
    Base64 {
        /// MIME type, e.g. `image/png`.
        media_type: String,
        /// Base64 payload.
        data: String,
    },
}

/// A tool invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCall {
    /// Call id, echoed by the matching [`ToolResult`].
    pub id: String,
    /// Tool name.
    pub name: String,
    /// Arguments as a JSON value (encoders stringify where the format wants it).
    pub arguments: Value,
}

/// The output of a tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolResult {
    /// Id of the [`ToolCall`] this answers.
    pub call_id: String,
    /// Result text.
    pub content: String,
    /// Whether the tool reported failure.
    pub is_error: bool,
}

/// A tool definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tool {
    /// A client-defined function.
    Function {
        /// Name.
        name: String,
        /// Description.
        description: String,
        /// JSON Schema of the arguments.
        schema: Value,
        /// Cache breakpoint after this tool definition.
        cache: Option<Cache>,
    },
    /// The provider's built-in web search (Responses `web_search`); encoders
    /// for formats without it emit a synthetic function or drop it.
    WebSearch,
}

/// Tool-use policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolChoice {
    /// Model decides.
    Auto,
    /// Never call tools.
    None,
    /// Must call some tool.
    Required,
    /// Must call this tool.
    Named(String),
}

/// Reasoning depth, the union of what the formats express. Encoders clamp to
/// what their format accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Effort {
    /// Least reasoning.
    Minimal,
    /// Low.
    Low,
    /// Medium.
    Medium,
    /// High.
    High,
    /// Above high (`xhigh` / `max`).
    Max,
}

/// A complete (non-streamed) model response.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Response {
    /// Response id.
    pub id: String,
    /// Model that answered.
    pub model: String,
    /// Output parts (`Text`, `ToolCall`, `Reasoning`).
    pub content: Vec<Part>,
    /// Why generation stopped.
    pub stop: StopReason,
    /// Creation time (unix seconds) reported upstream, if any.
    pub created: Option<u64>,
    /// Token usage.
    pub usage: TokenUsage,
    /// Unmodeled response fields from the source format.
    pub ext: Ext,
}

/// Why generation stopped.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum StopReason {
    /// Natural end of turn.
    #[default]
    EndTurn,
    /// Hit the token cap.
    MaxTokens,
    /// Stopped to call tools.
    ToolUse,
    /// Hit a stop sequence.
    StopSequence,
}

/// A stream event. Blocks have an explicit lifecycle (start, deltas, stop)
/// keyed by `index`; decoders of formats where it is implicit (`OpenAI` chunks)
/// synthesize the start/stop events.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// Stream opened.
    Start {
        /// Response id.
        id: String,
        /// Model.
        model: String,
    },
    /// A content block opened.
    BlockStart {
        /// Block index.
        index: u32,
        /// What the block holds.
        kind: BlockKind,
    },
    /// Incremental block content.
    Delta {
        /// Block index.
        index: u32,
        /// Text, reasoning, or tool-argument JSON fragment.
        text: String,
    },
    /// A content block closed.
    BlockStop {
        /// Block index.
        index: u32,
    },
    /// Usage update (cumulative).
    Usage(TokenUsage),
    /// Generation finished.
    Finish(StopReason),
}

/// Kind of a streamed content block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlockKind {
    /// Text output.
    Text,
    /// Reasoning output.
    Reasoning,
    /// A tool call; deltas carry argument JSON fragments.
    ToolCall {
        /// Call id.
        id: String,
        /// Tool name.
        name: String,
    },
}

// ---------------------------------------------------------------------------
// dispatch
// ---------------------------------------------------------------------------

pub mod anthropic;
pub mod openai;
pub mod responses;

use crate::sse::SseFrame;
use crate::translate::TranslateError;

impl From<crate::config::ProviderFormat> for Format {
    fn from(f: crate::config::ProviderFormat) -> Self {
        match f {
            crate::config::ProviderFormat::Anthropic => Self::Anthropic,
            crate::config::ProviderFormat::Openai => Self::Openai,
            crate::config::ProviderFormat::OpenaiResponses => Self::Responses,
        }
    }
}

/// Decode a request body in format `f`.
///
/// # Errors
///
/// Returns [`TranslateError`] when the body is not an object or a field has
/// the wrong shape.
pub fn decode_request(f: Format, body: Value) -> Result<Request, TranslateError> {
    match f {
        Format::Anthropic => anthropic::decode_request(body),
        Format::Openai => openai::decode_request(body),
        Format::Responses => responses::decode_request(body),
    }
}

/// Encode `req` as a request body in format `f`.
#[must_use]
pub fn encode_request(f: Format, req: &Request) -> Value {
    match f {
        Format::Anthropic => anthropic::encode_request(req),
        Format::Openai => openai::encode_request(req),
        Format::Responses => responses::encode_request(req),
    }
}

/// Translate a request body from format `from` to format `to`.
///
/// # Errors
///
/// Returns [`TranslateError`] when the body cannot be decoded.
pub fn translate_request(from: Format, to: Format, body: Value) -> Result<Value, TranslateError> {
    Ok(encode_request(to, &decode_request(from, body)?))
}

/// Decode a non-streamed response body in format `f`.
#[must_use]
pub fn decode_response(f: Format, body: &Value) -> Response {
    match f {
        Format::Anthropic => anthropic::decode_response(body),
        Format::Openai => openai::decode_response(body),
        Format::Responses => responses::decode_response(body),
    }
}

/// Encode `resp` as a non-streamed response body in format `f`.
#[must_use]
pub fn encode_response(f: Format, resp: &Response) -> Value {
    match f {
        Format::Anthropic => anthropic::encode_response(resp),
        Format::Openai => openai::encode_response(resp),
        Format::Responses => responses::encode_response(resp),
    }
}

/// Turns upstream SSE frames of one format into IR [`Event`]s.
pub trait StreamDecoder: Send {
    /// Decode one frame.
    fn frame(&mut self, frame: &SseFrame) -> Vec<Event>;
    /// Close whatever is still open and emit the final usage and
    /// [`Event::Finish`]. Called once, when the upstream stream ends.
    fn finish(&mut self) -> Vec<Event>;
}

/// Turns IR [`Event`]s into client SSE payloads of one format. A JSON string
/// payload is a raw `data:` line (`[DONE]`).
pub trait StreamEncoder: Send {
    /// Encode one event.
    fn event(&mut self, event: &Event) -> Vec<Value>;
}

/// A stream decoder for upstream format `f`.
#[must_use]
pub fn stream_decoder(f: Format) -> Box<dyn StreamDecoder> {
    match f {
        Format::Anthropic => Box::new(anthropic::Decoder::default()),
        Format::Openai => Box::new(openai::Decoder::default()),
        Format::Responses => Box::new(responses::Decoder::default()),
    }
}

/// A stream encoder for client format `f`; `model` is reported to the client.
#[must_use]
pub fn stream_encoder(f: Format, model: &str) -> Box<dyn StreamEncoder> {
    match f {
        Format::Anthropic => Box::new(anthropic::Encoder::new(model)),
        Format::Openai => Box::new(openai::Encoder::new(model)),
        Format::Responses => Box::new(responses::Encoder::new(model)),
    }
}

/// Fold a complete event sequence into a [`Response`] (a non-streaming client
/// of a stream-only upstream).
#[must_use]
pub fn fold(events: &[Event]) -> Response {
    let mut resp = Response::default();
    // (index, part, accumulated tool-argument JSON)
    let mut blocks: Vec<(u32, Part, String)> = Vec::new();
    for event in events {
        match event {
            Event::Start { id, model } => {
                resp.id.clone_from(id);
                resp.model.clone_from(model);
            }
            Event::BlockStart { index, kind } => {
                let part = match kind {
                    BlockKind::Text => Part::Text(String::new()),
                    BlockKind::Reasoning => Part::Reasoning {
                        text: String::new(),
                        signature: None,
                    },
                    BlockKind::ToolCall { id, name } => Part::ToolCall(ToolCall {
                        id: id.clone(),
                        name: name.clone(),
                        arguments: Value::Null,
                    }),
                };
                blocks.push((*index, part, String::new()));
            }
            Event::Delta { index, text } => {
                if let Some((_, part, args)) = blocks.iter_mut().find(|(i, ..)| i == index) {
                    match part {
                        Part::Text(t) | Part::Reasoning { text: t, .. } => t.push_str(text),
                        _ => args.push_str(text),
                    }
                }
            }
            Event::Usage(u) => resp.usage = *u,
            Event::Finish(stop) => resp.stop = *stop,
            Event::BlockStop { .. } => {}
        }
    }
    resp.content = blocks
        .into_iter()
        .map(|(_, part, args)| match part {
            Part::ToolCall(mut call) => {
                call.arguments = parse_args(&args);
                Part::ToolCall(call)
            }
            other => other,
        })
        .collect();
    resp
}

/// Bookkeeping shared by the stream decoders: start emission, block indices,
/// open blocks, cumulative usage.
#[derive(Debug, Default)]
pub(crate) struct Emitter {
    started: bool,
    next: u32,
    open: Vec<u32>,
    pub(crate) usage: TokenUsage,
    pub(crate) saw_tool: bool,
    finished: bool,
}

impl Emitter {
    pub(crate) fn start(&mut self, out: &mut Vec<Event>, id: &str, model: &str) {
        if !self.started {
            self.started = true;
            out.push(Event::Start {
                id: id.to_string(),
                model: model.to_string(),
            });
        }
    }

    pub(crate) fn open(&mut self, out: &mut Vec<Event>, kind: BlockKind) -> u32 {
        self.start(out, "", "");
        if matches!(kind, BlockKind::ToolCall { .. }) {
            self.saw_tool = true;
        }
        let index = self.next;
        self.next += 1;
        self.open.push(index);
        out.push(Event::BlockStart { index, kind });
        index
    }

    pub(crate) fn delta(out: &mut Vec<Event>, index: u32, text: &str) {
        if !text.is_empty() {
            out.push(Event::Delta {
                index,
                text: text.to_string(),
            });
        }
    }

    pub(crate) fn close(&mut self, out: &mut Vec<Event>, index: u32) {
        if let Some(pos) = self.open.iter().position(|i| *i == index) {
            self.open.remove(pos);
            out.push(Event::BlockStop { index });
        }
    }

    /// Merge a usage report; later non-zero fields win.
    pub(crate) fn usage(&mut self, part: TokenUsage) {
        let acc = &mut self.usage;
        for (a, p) in [
            (&mut acc.input, part.input),
            (&mut acc.output, part.output),
            (&mut acc.reasoning, part.reasoning),
            (&mut acc.cache_read, part.cache_read),
            (&mut acc.cache_write, part.cache_write),
        ] {
            if p > 0 {
                *a = p;
            }
        }
        if part.cost_usd.is_some() {
            acc.cost_usd = part.cost_usd;
        }
    }

    pub(crate) fn finish(&mut self, stop: Option<StopReason>) -> Vec<Event> {
        let mut out = Vec::new();
        if self.finished {
            return out;
        }
        self.finished = true;
        self.start(&mut out, "", "");
        for index in std::mem::take(&mut self.open) {
            out.push(Event::BlockStop { index });
        }
        out.push(Event::Usage(self.usage));
        let stop = stop.unwrap_or(if self.saw_tool {
            StopReason::ToolUse
        } else {
            StopReason::EndTurn
        });
        out.push(Event::Finish(stop));
        out
    }
}

// ---------------------------------------------------------------------------
// shared helpers
// ---------------------------------------------------------------------------

/// Output cap sent to formats that require one when the client gave none.
pub(crate) const DEFAULT_MAX_TOKENS: u64 = 4096;

impl Effort {
    /// Parse a client effort spelling (`min`, `minimal`, `low`, ..., `xhigh`,
    /// `max`) or a 0-10 number.
    #[must_use]
    pub fn parse(value: &Value) -> Option<Self> {
        match value {
            Value::String(s) => match s.trim().to_ascii_lowercase().as_str() {
                "none" | "min" | "minimal" => Some(Self::Minimal),
                "low" => Some(Self::Low),
                "medium" | "moderate" => Some(Self::Medium),
                "high" => Some(Self::High),
                "xhigh" | "max" | "full" => Some(Self::Max),
                _ => None,
            },
            Value::Number(n) => match n.as_f64()? {
                f if f <= 3.0 => Some(Self::Low),
                f if f <= 7.0 => Some(Self::Medium),
                f if f <= 10.0 => Some(Self::High),
                _ => None,
            },
            Value::Object(map) => ["effort", "level", "reasoning_effort", "depth"]
                .iter()
                .find_map(|k| map.get(*k).and_then(Self::parse)),
            _ => None,
        }
    }

    /// The Responses API spelling, which has `minimal` and `xhigh`.
    #[must_use]
    pub const fn as_responses(self) -> &'static str {
        match self {
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Max => "xhigh",
        }
    }

    /// The `low`/`medium`/`high` spelling every upstream accepts.
    // ponytail: clamps minimal->low and max->high because not every upstream
    // model takes `minimal`/`xhigh`; pass them through per provider if needed.
    #[must_use]
    pub const fn as_lmh(self) -> &'static str {
        match self {
            Self::Minimal | Self::Low => "low",
            Self::Medium => "medium",
            Self::High | Self::Max => "high",
        }
    }
}

impl Image {
    /// The image as a URL (`data:` for inline bytes).
    #[must_use]
    pub fn to_url(&self) -> String {
        match self {
            Self::Url(u) => u.clone(),
            Self::Base64 { media_type, data } => format!("data:{media_type};base64,{data}"),
        }
    }

    /// Parse a URL, splitting `data:` URLs into inline bytes.
    #[must_use]
    pub fn from_url(url: &str) -> Self {
        url.strip_prefix("data:").map_or_else(
            || Self::Url(url.to_string()),
            |rest| {
                let (meta, data) = rest.split_once(',').unwrap_or((rest, ""));
                Self::Base64 {
                    media_type: meta.trim_end_matches(";base64").to_string(),
                    data: data.to_string(),
                }
            },
        )
    }
}

/// Concatenated text of a string or an array of `{text}` blocks.
pub(crate) fn text_of(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(arr) => arr
            .iter()
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect(),
        _ => String::new(),
    }
}

/// Tool arguments as a JSON value (strings are parsed when they hold JSON).
pub(crate) fn parse_args(raw: &str) -> Value {
    if raw.trim().is_empty() {
        return Value::Object(serde_json::Map::new());
    }
    serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.to_string()))
}

/// Tool arguments from a wire value (`"{...}"` string or object).
pub(crate) fn args_value(v: Option<&Value>) -> Value {
    match v {
        Some(Value::String(s)) => parse_args(s),
        Some(other) => other.clone(),
        None => Value::Object(serde_json::Map::new()),
    }
}

/// Tool arguments as the JSON string the `OpenAI` formats carry.
pub(crate) fn args_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => serde_json::to_string(other).unwrap_or_else(|_| "{}".to_string()),
    }
}

/// `v[key]` as a string, or empty.
pub(crate) fn str_of<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}

/// Stop sequences from a string or array.
pub(crate) fn stops(v: Option<&Value>) -> Vec<String> {
    match v {
        Some(Value::String(s)) => vec![s.clone()],
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    }
}

/// Move every field of `obj` not in `used` into an [`Ext`] under `ns:`.
pub(crate) fn ext_rest(obj: serde_json::Map<String, Value>, used: &[&str], ns: &str) -> Ext {
    let mut ext = Ext::default();
    for (k, v) in obj {
        if !used.contains(&k.as_str()) {
            ext.insert(format!("{ns}:{k}"), v);
        }
    }
    ext
}

/// Default JSON Schema for a tool without parameters.
pub(crate) fn empty_schema() -> Value {
    serde_json::json!({"type": "object", "properties": {}})
}

/// Fresh id with `prefix`.
pub(crate) fn new_id(prefix: &str) -> String {
    format!("{prefix}_{}", uuid::Uuid::new_v4().simple())
}

/// Unix seconds now.
pub(crate) fn now_ts() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Append `blocks` as a `role` turn. Tool calls and tool results merge into a
/// previous same-role turn (some formats send each as its own item; providers
/// expect them grouped). Anything else starts a new turn, so an earlier turn
/// never changes when a later one arrives (the cached prefix stays stable).
pub(crate) fn push_message(messages: &mut Vec<Message>, role: Role, blocks: Vec<Block>) {
    if blocks.is_empty() {
        return;
    }
    let is_tool = blocks
        .iter()
        .all(|b| matches!(b.part, Part::ToolCall(_) | Part::ToolResult(_)));
    match messages.last_mut() {
        Some(last) if last.role == role && is_tool => last.content.extend(blocks),
        _ => messages.push(Message {
            role,
            content: blocks,
        }),
    }
}

/// The usage fields every format shares.
pub(crate) fn usage_from(v: &Value) -> TokenUsage {
    crate::translate::parse_usage(v)
}

#[cfg(test)]
mod tests;
