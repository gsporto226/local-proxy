use std::convert::Infallible;
use std::pin::Pin;
use std::time::Instant;

use axum::response::sse::Event;
use futures_util::{Stream, StreamExt};
use serde_json::Value;

use crate::ir::{self, Format, StreamDecoder, StreamEncoder};
use crate::sse::{sse_frames, SseError, SseFrame};
use crate::stats::{self, StatLine};
use crate::translate::{EnergyCost, TokenUsage};

/// A stream of already-framed SSE `Event`s ready to be sent to the client.
pub type UpstreamStream = Pin<Box<dyn Stream<Item = Result<Event, Infallible>> + Send>>;

/// Deferred best-effort stats capture for a streaming request.
///
/// A streaming request cannot be recorded up-front: its token usage is only
/// known once the SSE stream has been fully read. This type carries the request
/// metadata and records the row (with the cumulative usage) when the stream
/// completes. A failure is logged and ignored, as with non-streaming capture.
pub struct StreamCapture {
    endpoint: &'static str,
    provider: String,
    alias: String,
    model: String,
    status: u16,
    started: Instant,
    session_id: String,
}

impl StreamCapture {
    /// Build a capture handle for a proxied streaming request.
    #[must_use]
    pub fn new(
        endpoint: &'static str,
        provider: &str,
        alias: &str,
        model: &str,
        status: u16,
        started: Instant,
        session_id: &str,
    ) -> Self {
        Self {
            endpoint,
            provider: provider.to_string(),
            alias: alias.to_string(),
            model: model.to_string(),
            status,
            started,
            session_id: session_id.to_string(),
        }
    }

    /// Write the stats row with the final cumulative `usage`, plus any energy
    /// and cost metadata observed in the stream (`NeuralWatt`-style). Best-effort.
    /// When no `NeuralWatt` cost was observed, falls back to the `usage.cost`
    /// reported by OpenAI/OpenRouter-style upstreams.
    pub fn record(&self, usage: TokenUsage, energy: Option<EnergyCost>, cost: Option<EnergyCost>) {
        stats::record(
            self.started,
            StatLine {
                endpoint: self.endpoint,
                provider: self.provider.clone(),
                alias: self.alias.clone(),
                model: self.model.clone(),
                input_tokens: usage.input,
                output_tokens: usage.output,
                streamed: true,
                status: self.status,
                error: self.status >= 400,
                energy,
                cost: cost.or_else(|| usage.as_cost()),
                session_id: self.session_id.clone(),
                cache_hit: usage.cache_hit(),
            },
        );
    }
}

/// Convert a machine-produced JSON payload into an SSE `Event`. Anthropic and
/// Responses events carry their name in `type`; `OpenAI` chat chunks carry no
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
    /// The accumulated usage observed so far (final once [`Self::finalize`] ran).
    fn usage(&self) -> TokenUsage;
}

/// Parse an `SSE` comment line carrying `NeuralWatt` metadata.
///
/// Comments look like `energy {...}` or `cost {...}`; the leading `:` and
/// space are stripped by the `SSE` parser, so `comment` is `energy {...}`.
#[must_use]
pub fn parse_energy_comment(comment: &str) -> Option<(Option<EnergyCost>, Option<EnergyCost>)> {
    let (kind, json) = comment.split_once(' ')?;
    let Ok(value) = serde_json::from_str::<Value>(json.trim()) else {
        return None;
    };
    match kind {
        "energy" => Some((crate::translate::energy_from_payload(&value), None)),
        "cost" => Some((None, crate::translate::cost_from_payload(&value))),
        _ => None,
    }
}

struct Driver<M: Machine> {
    frames: Pin<Box<dyn Stream<Item = Result<SseFrame, SseError>> + Send>>,
    machine: M,
    pending: Vec<Value>,
    done: bool,
    finalized: bool,
    capture: Option<StreamCapture>,
    reported: bool,
    energy: Option<EnergyCost>,
    cost: Option<EnergyCost>,
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
                if !st.reported {
                    st.reported = true;
                    if let Some(c) = &st.capture {
                        c.record(st.machine.usage(), st.energy, st.cost);
                    }
                }
                if st.pending.is_empty() {
                    return None;
                }
                continue;
            }
            return None;
        }
        match st.frames.next().await {
            Some(Ok(frame)) => {
                for comment in &frame.comments {
                    if let Some((e, c)) = parse_energy_comment(comment) {
                        if e.is_some() {
                            st.energy = e;
                        }
                        if c.is_some() {
                            st.cost = c;
                        }
                    }
                }
                st.pending = st.machine.process(&frame);
            }
            Some(Err(_)) | None => st.done = true,
        }
    }
}

fn build<M: Machine + 'static>(
    resp: reqwest::Response,
    machine: M,
    capture: Option<StreamCapture>,
) -> UpstreamStream {
    let driver = Driver {
        frames: Box::pin(sse_frames(resp)),
        machine,
        pending: Vec::new(),
        done: false,
        finalized: false,
        capture,
        reported: false,
        energy: None,
        cost: None,
    };
    Box::pin(futures_util::stream::unfold(driver, drive))
}

// ---------------------------------------------------------------------------
// IR bridge
// ---------------------------------------------------------------------------

/// Upstream frames -> IR events (decoder) -> client payloads (encoder).
struct IrMachine {
    dec: Box<dyn StreamDecoder>,
    enc: Box<dyn StreamEncoder>,
    usage: TokenUsage,
}

impl IrMachine {
    fn new(upstream: Format, client: Format, model: &str) -> Self {
        Self {
            dec: ir::stream_decoder(upstream),
            enc: ir::stream_encoder(client, model),
            usage: TokenUsage::default(),
        }
    }

    fn encode(&mut self, events: Vec<ir::Event>) -> Vec<Value> {
        let mut out = Vec::new();
        for event in events {
            if let ir::Event::Usage(u) = &event {
                self.usage = *u;
            }
            out.extend(self.enc.event(&event));
        }
        out
    }
}

impl Machine for IrMachine {
    fn process(&mut self, frame: &SseFrame) -> Vec<Value> {
        let events = self.dec.frame(frame);
        self.encode(events)
    }

    fn finalize(&mut self) -> Vec<Value> {
        let events = self.dec.finish();
        self.encode(events)
    }

    fn usage(&self) -> TokenUsage {
        self.usage
    }
}

/// Translate an `upstream`-format SSE response into a `client`-format SSE
/// stream reporting `model`.
///
/// Pass an optional [`StreamCapture`] to record cumulative usage stats when the
/// stream completes.
// The `Pin<Box<dyn Stream>>` return type is already `#[must_use]`. Older clippy
// suggests adding the attribute (`must_use_candidate`, it does not recognize
// `Pin`), newer clippy rejects it as redundant (`double_must_use`).
#[allow(clippy::must_use_candidate)]
pub fn translate(
    resp: reqwest::Response,
    upstream: Format,
    client: Format,
    model: &str,
    capture: Option<StreamCapture>,
) -> UpstreamStream {
    build(resp, IrMachine::new(upstream, client, model), capture)
}

// ---------------------------------------------------------------------------
// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn stream_capture_persists_reported_cache_hits_and_misses() {
        let _guard = crate::TEST_STATE_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("LOCAL_PROXY_CONFIG_DIR", dir.path());
        for cache_read in [None, Some(0), Some(5)] {
            StreamCapture::new(
                "/v1/messages",
                "anthropic",
                "work",
                "claude-test",
                200,
                Instant::now(),
                "sess-stream",
            )
            .record(
                TokenUsage {
                    cache_read,
                    ..TokenUsage::default()
                },
                None,
                None,
            );
        }
        let summary = stats::summary(stats::StatsFilter {
            window: stats::TimeWindow { since: None },
            scope: stats::StatsScope::Session("sess-stream"),
        })
        .unwrap()
        .unwrap();
        assert_eq!(summary.cache.hit_requests, 1);
        assert_eq!(summary.cache.reported_requests, 2);
        std::env::remove_var("LOCAL_PROXY_CONFIG_DIR");
    }

    #[allow(clippy::needless_pass_by_value)]
    fn frame(data: Value) -> SseFrame {
        SseFrame {
            event: None,
            data: data.to_string(),
            comments: Vec::new(),
        }
    }

    #[allow(clippy::needless_pass_by_value)]
    fn oai_chunk(delta: Value, finish: Option<&str>) -> SseFrame {
        frame(json!({
            "id": "cmpl_1",
            "object": "chat.completion.chunk",
            "created": 1,
            "model": "m",
            "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]
        }))
    }

    #[allow(clippy::needless_pass_by_value)]
    fn anthro(ty: &str, extra: Value) -> SseFrame {
        let mut v = json!({"type": ty});
        if let Some(o) = extra.as_object() {
            v.as_object_mut().unwrap().extend(o.clone());
        }
        frame(v)
    }

    #[allow(clippy::needless_pass_by_value)]
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

    // ---- usage accumulation across frames ----

    #[test]
    fn ao_machine_accumulates_usage_across_frames() {
        // Anthropic -> OpenAI: usage arrives in message_start (input) and
        // message_delta (output). After streaming, the machine's cumulative
        // usage should reflect both.
        let mut m = IrMachine::new(Format::Anthropic, Format::Openai, "claude");
        m.process(&anthro(
            "message_start",
            json!({"message": {"id": "msg_1", "usage": {"input_tokens": 5, "output_tokens": 0}}}),
        ));
        m.process(&anthro(
            "message_delta",
            json!({"delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 7}}),
        ));
        m.finalize();
        assert_eq!(
            m.usage(),
            TokenUsage {
                input: 5,
                output: 7,
                reasoning: 0,
                cost_usd: None,
                ..TokenUsage::default()
            }
        );
        assert_eq!(m.usage().output, 7);
    }

    #[test]
    fn oa_machine_usage_covers_openai_chunk() {
        // OpenAI -> Anthropic: usage rides on a chunk with `usage`.
        let mut m = IrMachine::new(Format::Openai, Format::Anthropic, "gpt");
        m.process(&oai_chunk(json!({"content": "hi"}), Some("stop")));
        m.process(&frame(json!({
            "id": "cmpl_1",
            "object": "chat.completion.chunk",
            "choices": [],
            "usage": {"prompt_tokens": 11, "completion_tokens": 4}
        })));
        m.finalize();
        assert_eq!(
            m.usage(),
            TokenUsage {
                input: 11,
                output: 4,
                reasoning: 0,
                cost_usd: None,
                ..TokenUsage::default()
            }
        );
    }

    #[test]
    fn parses_energy_and_cost_comments() {
        let (e, c) =
            parse_energy_comment("energy {\"energy_joules\": 4.99, \"energy_kwh\": 1.385e-6}")
                .unwrap();
        let e = e.unwrap();
        assert_eq!(e.energy_kwh, Some(1.385e-6));
        assert_eq!(e.energy_joules, Some(4.99));
        assert!(c.is_none());

        let (e2, c2) = parse_energy_comment(
            "cost {\"request_cost_usd\": 1.04e-5, \"cache_savings_usd\": 0.0}",
        )
        .unwrap();
        assert!(e2.is_none());
        assert_eq!(c2.unwrap().request_cost_usd, Some(1.04e-5));

        // unknown / malformed comments are ignored
        assert!(parse_energy_comment("keep-alive").is_none());
        assert!(parse_energy_comment("energy not-json").is_none());
    }

    // ---- OpenAI -> Anthropic (text) ----

    #[test]
    fn oa_text_stream() {
        let events = run(
            IrMachine::new(Format::Openai, Format::Anthropic, "m"),
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
            IrMachine::new(Format::Openai, Format::Anthropic, "m"),
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
            IrMachine::new(Format::Openai, Format::Anthropic, "m"),
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
            IrMachine::new(Format::Anthropic, Format::Openai, "claude"),
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
            IrMachine::new(Format::Anthropic, Format::Openai, "claude"),
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
            IrMachine::new(Format::Anthropic, Format::Responses, "claude"),
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
            IrMachine::new(Format::Anthropic, Format::Responses, "claude"),
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
            IrMachine::new(Format::Openai, Format::Responses, "gpt"),
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
            IrMachine::new(Format::Openai, Format::Responses, "gpt"),
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

    // ---- OpenAI Responses -> Anthropic ----

    /// Build a `response.output_text.delta` frame.
    fn resp_text_delta(delta: &str) -> SseFrame {
        frame(json!({
            "type": "response.output_text.delta",
            "item_id": "msg_1",
            "output_index": 0,
            "content_index": 0,
            "delta": delta
        }))
    }

    #[test]
    fn r2a_text_stream() {
        let events = run(
            IrMachine::new(Format::Responses, Format::Anthropic, "gpt"),
            vec![
                frame(json!({"type": "response.created", "response": {"id": "resp_1"}})),
                resp_text_delta("hello"),
                resp_text_delta(" world"),
                frame(json!({
                    "type": "response.completed",
                    "response": {"id": "resp_1", "status": "completed",
                        "usage": {"input_tokens": 3, "output_tokens": 2}}
                })),
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
        assert_eq!(events[0]["message"]["id"], "resp_1");
        assert_eq!(events[0]["message"]["model"], "gpt");
        assert_eq!(events[2]["delta"]["text"], "hello");
        assert_eq!(events[3]["delta"]["text"], " world");
        // stop reason defaults to end_turn with no tools
        assert_eq!(events[5]["delta"]["stop_reason"], "end_turn");
        assert_eq!(events[5]["usage"]["output_tokens"], 2);
    }

    #[test]
    fn r2a_tool_call_stream() {
        let events = run(
            IrMachine::new(Format::Responses, Format::Anthropic, "gpt"),
            vec![
                frame(json!({
                    "type": "response.output_item.added",
                    "output_index": 0,
                    "item": {"type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "weather"}
                })),
                frame(json!({
                    "type": "response.function_call_arguments.delta",
                    "item_id": "fc_1",
                    "delta": "{\"city\":"
                })),
                frame(json!({
                    "type": "response.function_call_arguments.delta",
                    "item_id": "fc_1",
                    "delta": "\"sp\"}"
                })),
                frame(json!({
                    "type": "response.completed",
                    "response": {"status": "completed"}
                })),
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
        let start = &events[1]["content_block"];
        assert_eq!(start["type"], "tool_use");
        assert_eq!(start["id"], "call_1");
        assert_eq!(start["name"], "weather");
        assert_eq!(events[2]["delta"]["partial_json"], "{\"city\":");
        assert_eq!(events[3]["delta"]["partial_json"], "\"sp\"}");
        assert_eq!(events[5]["delta"]["stop_reason"], "tool_use");
    }

    #[test]
    fn r2a_interleaves_thinking_then_text_closing_blocks() {
        let events = run(
            IrMachine::new(Format::Responses, Format::Anthropic, "gpt"),
            vec![
                frame(json!({"type": "response.reasoning_summary_text.delta", "delta": "hmm"})),
                resp_text_delta("ok"),
            ],
        );
        let t = types(&events);
        assert_eq!(
            t,
            [
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_stop",
                "content_block_start",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop",
            ]
        );
        assert_eq!(events[1]["content_block"]["type"], "thinking");
        assert_eq!(events[2]["delta"]["type"], "thinking_delta");
        assert_eq!(events[4]["content_block"]["type"], "text");
    }

    #[test]
    fn r2a_incomplete_reports_max_tokens() {
        let events = run(
            IrMachine::new(Format::Responses, Format::Anthropic, "gpt"),
            vec![frame(json!({
                "type": "response.completed",
                "response": {"status": "incomplete"}
            }))],
        );
        let delta = events
            .iter()
            .find(|e| e["type"] == "message_delta")
            .expect("message_delta");
        assert_eq!(delta["delta"]["stop_reason"], "max_tokens");
    }

    // ---- OpenAI Responses -> OpenAI chat ----

    #[test]
    fn r2o_text_stream() {
        let events = run(
            IrMachine::new(Format::Responses, Format::Openai, "gpt"),
            vec![
                frame(json!({"type": "response.created", "response": {"id": "resp_1"}})),
                resp_text_delta("hi"),
                resp_text_delta(" there"),
                frame(json!({
                    "type": "response.completed",
                    "response": {"status": "completed",
                        "usage": {"input_tokens": 5, "output_tokens": 4}}
                })),
            ],
        );
        assert_eq!(events[0]["choices"][0]["delta"]["role"], "assistant");
        assert_eq!(events[0]["id"], "resp_1");
        assert_eq!(events[1]["choices"][0]["delta"]["content"], "hi");
        assert_eq!(events[2]["choices"][0]["delta"]["content"], " there");
        let last = events.last().unwrap();
        assert_eq!(last, &json!("[DONE]"));
        let final_chunk = &events[events.len() - 2];
        assert_eq!(final_chunk["choices"][0]["finish_reason"], "stop");
        assert_eq!(final_chunk["usage"]["prompt_tokens"], 5);
        assert_eq!(final_chunk["usage"]["completion_tokens"], 4);
    }

    #[test]
    fn r2o_tool_call_stream() {
        let events = run(
            IrMachine::new(Format::Responses, Format::Openai, "gpt"),
            vec![
                frame(json!({
                    "type": "response.output_item.added",
                    "item": {"type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "weather"}
                })),
                frame(json!({
                    "type": "response.function_call_arguments.delta",
                    "item_id": "fc_1",
                    "delta": "{\"c\":1}"
                })),
            ],
        );
        let start = events
            .iter()
            .find(|e| e["choices"][0]["delta"]["tool_calls"].is_array())
            .expect("tool call chunk");
        let call = &start["choices"][0]["delta"]["tool_calls"][0];
        assert_eq!(call["index"], 0);
        assert_eq!(call["id"], "call_1");
        assert_eq!(call["function"]["name"], "weather");
        let args = events
            .iter()
            .filter_map(|e| {
                e["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"].as_str()
            })
            .find(|a| !a.is_empty())
            .expect("arguments delta");
        assert_eq!(args, "{\"c\":1}");
        let final_chunk = &events[events.len() - 2];
        assert_eq!(final_chunk["choices"][0]["finish_reason"], "tool_calls");
    }

    #[test]
    fn r2o_incomplete_reports_length() {
        let events = run(
            IrMachine::new(Format::Responses, Format::Openai, "gpt"),
            vec![frame(json!({
                "type": "response.completed",
                "response": {"status": "incomplete"}
            }))],
        );
        let final_chunk = &events[events.len() - 2];
        assert_eq!(final_chunk["choices"][0]["finish_reason"], "length");
    }

    #[test]
    fn r2_empty_streams_still_terminate() {
        let a = run(
            IrMachine::new(Format::Responses, Format::Anthropic, "m"),
            vec![],
        );
        assert_eq!(
            types(&a),
            ["message_start", "message_delta", "message_stop"]
        );

        let o = run(
            IrMachine::new(Format::Responses, Format::Openai, "m"),
            vec![],
        );
        assert_eq!(o.last().unwrap(), &json!("[DONE]"));
        assert_eq!(o[o.len() - 2]["choices"][0]["finish_reason"], "stop");
    }

    // ---- empty stream still emits a valid terminal sequence ----

    #[test]
    fn oa_empty_stream_still_completes() {
        let events = run(
            IrMachine::new(Format::Openai, Format::Anthropic, "m"),
            vec![],
        );
        let t = types(&events);
        assert_eq!(t, ["message_start", "message_delta", "message_stop"]);
    }

    #[test]
    fn a2r_empty_stream_still_completes() {
        let events = run(
            IrMachine::new(Format::Anthropic, Format::Responses, "m"),
            vec![],
        );
        let t = types(&events);
        assert_eq!(t, ["response.created", "response.completed"]);
    }
}
