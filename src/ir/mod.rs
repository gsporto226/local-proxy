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
