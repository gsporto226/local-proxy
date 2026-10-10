//! Translation behavior, ported from the per-pair translators the IR replaced.

use serde_json::{json, Value};

use super::*;
use crate::domain::translate::TranslateError;

fn req(from: Format, to: Format, body: Value) -> Result<Value, TranslateError> {
    translate_request(from, to, body)
}
fn anthropic_to_openai_request(b: Value) -> Result<Value, TranslateError> {
    req(Format::Anthropic, Format::Openai, b)
}
fn openai_to_anthropic_request(b: Value) -> Result<Value, TranslateError> {
    req(Format::Openai, Format::Anthropic, b)
}
fn responses_to_openai_request(b: Value) -> Result<Value, TranslateError> {
    req(Format::Responses, Format::Openai, b)
}
fn responses_to_anthropic_request(b: Value) -> Result<Value, TranslateError> {
    req(Format::Responses, Format::Anthropic, b)
}
fn openai_to_responses_request(b: Value) -> Result<Value, TranslateError> {
    req(Format::Openai, Format::Responses, b)
}
fn anthropic_to_responses_request(b: Value) -> Result<Value, TranslateError> {
    req(Format::Anthropic, Format::Responses, b)
}

#[allow(clippy::unnecessary_wraps)]
fn resp(from: Format, to: Format, body: &Value, model: &str) -> Result<Value, TranslateError> {
    let mut r = decode_response(from, body);
    r.model = model.to_string();
    Ok(encode_response(to, &r))
}
#[allow(clippy::needless_pass_by_value)]
fn responses_to_openai_response(b: Value, m: &str) -> Result<Value, TranslateError> {
    resp(Format::Responses, Format::Openai, &b, m)
}
#[allow(clippy::needless_pass_by_value)]
fn responses_to_anthropic_response(b: Value, m: &str) -> Result<Value, TranslateError> {
    resp(Format::Responses, Format::Anthropic, &b, m)
}
#[allow(clippy::needless_pass_by_value)]
fn anthropic_to_openai_response(b: Value, m: &str) -> Result<Value, TranslateError> {
    resp(Format::Anthropic, Format::Openai, &b, m)
}
#[allow(clippy::needless_pass_by_value)]
fn openai_to_anthropic_response(b: Value, m: &str) -> Result<Value, TranslateError> {
    resp(Format::Openai, Format::Anthropic, &b, m)
}
#[allow(clippy::needless_pass_by_value)]
fn anthropic_to_responses_response(b: Value, m: &str) -> Result<Value, TranslateError> {
    resp(Format::Anthropic, Format::Responses, &b, m)
}
#[allow(clippy::needless_pass_by_value)]
fn openai_to_responses_response(b: Value, m: &str) -> Result<Value, TranslateError> {
    resp(Format::Openai, Format::Responses, &b, m)
}

/// Fold Responses SSE event payloads into a Responses response body.
fn responses_events_to_response(events: &[Value]) -> Value {
    encode_response(Format::Responses, &responses::aggregate(events))
}

#[test]
fn a_to_o_system_string_and_array() {
    let body = json!({
        "model": "m",
        "system": "be terse",
        "messages": [{"role": "user", "content": "hi"}]
    });
    let out = anthropic_to_openai_request(body).unwrap();
    let msgs = out["messages"].as_array().unwrap();
    assert_eq!(msgs[0]["role"], "system");
    assert_eq!(msgs[0]["content"], "be terse");
    assert_eq!(msgs[1]["role"], "user");
    assert_eq!(msgs[1]["content"], "hi");

    let body = json!({
        "system": [{"type": "text", "text": "a"}, {"type": "text", "text": "b"}],
        "messages": []
    });
    let out = anthropic_to_openai_request(body).unwrap();
    let msgs = out["messages"].as_array().unwrap();
    assert_eq!(msgs[0]["content"], "a\nb");
}

#[test]
fn a_to_o_max_tokens_default() {
    let body = json!({"messages": [{"role": "user", "content": "hi"}]});
    let out = anthropic_to_openai_request(body).unwrap();
    assert_eq!(out["max_tokens"], 4096);

    let body = json!({"max_tokens": 100, "messages": [{"role": "user", "content": "hi"}]});
    let out = anthropic_to_openai_request(body).unwrap();
    assert_eq!(out["max_tokens"], 100);
}

#[test]
fn a_to_o_images() {
    let body = json!({
        "messages": [{
            "role": "user",
            "content": [
                {"type": "text", "text": "what is this"},
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAA"}},
                {"type": "image", "source": {"type": "url", "url": "https://example.com/a.png"}}
            ]
        }]
    });
    let out = anthropic_to_openai_request(body).unwrap();
    let msgs = out["messages"].as_array().unwrap();
    let content = msgs[0]["content"].as_array().unwrap();
    assert_eq!(content[0]["type"], "text");
    assert_eq!(content[1]["type"], "image_url");
    assert_eq!(content[1]["image_url"]["url"], "data:image/png;base64,AAA");
    assert_eq!(content[2]["image_url"]["url"], "https://example.com/a.png");
}

#[test]
fn a_to_o_tool_use_and_tool_result() {
    let body = json!({
        "messages": [
            {"role": "user", "content": [
                {"type": "text", "text": "check weather"},
                {"type": "tool_result", "tool_use_id": "call_1", "content": "sunny"}
            ]},
            {"role": "assistant", "content": [
                {"type": "text", "text": "ok"},
                {"type": "tool_use", "id": "call_1", "name": "weather", "input": {"city": "sp"}}
            ]}
        ]
    });
    let out = anthropic_to_openai_request(body).unwrap();
    let msgs = out["messages"].as_array().unwrap();
    // user text then tool message
    assert_eq!(msgs[0]["role"], "user");
    assert_eq!(msgs[1]["role"], "tool");
    assert_eq!(msgs[1]["tool_call_id"], "call_1");
    assert_eq!(msgs[1]["content"], "sunny");
    // assistant with tool_calls
    assert_eq!(msgs[2]["role"], "assistant");
    let tcs = msgs[2]["tool_calls"].as_array().unwrap();
    assert_eq!(tcs[0]["id"], "call_1");
    assert_eq!(tcs[0]["function"]["name"], "weather");
    assert_eq!(tcs[0]["function"]["arguments"], r#"{"city":"sp"}"#);
}

#[test]
fn a_to_o_tools_drop_regex_patterns() {
    let body = json!({
        "tools": [{
            "name": "artifact",
            "description": "d",
            "input_schema": {
                "type": "object",
                "properties": {
                    "name": {"type": "string", "pattern": "^[^\\0]*$", "maxLength": 10},
                    "nested": {"anyOf": [{"type": "string", "pattern": "^a$"}]},
                    "map": {"type": "object", "patternProperties": {"^x": {"type": "string"}}}
                },
                "required": ["name"]
            }
        }]
    });
    let out = anthropic_to_openai_request(body).unwrap();
    let schema = &out["tools"][0]["function"]["parameters"];
    assert!(schema["properties"]["name"].get("pattern").is_none());
    assert!(schema["properties"]["nested"]["anyOf"][0]
        .get("pattern")
        .is_none());
    assert!(schema["properties"]["map"]
        .get("patternProperties")
        .is_none());
    assert_eq!(schema["properties"]["name"]["maxLength"], 10);
    assert_eq!(schema["required"], json!(["name"]));
}

#[test]
fn a_to_o_tools_and_tool_choice() {
    let body = json!({
        "tools": [
            {"name": "w", "description": "weather", "input_schema": {"type": "object", "properties": {"city": {"type": "string"}}}}
        ],
        "tool_choice": {"type": "tool", "name": "w"},
        "stop_sequences": ["END", "STOP"]
    });
    let out = anthropic_to_openai_request(body).unwrap();
    let tools = out["tools"].as_array().unwrap();
    assert_eq!(tools[0]["type"], "function");
    assert_eq!(tools[0]["function"]["name"], "w");
    assert_eq!(
        tools[0]["function"]["parameters"]["properties"]["city"]["type"],
        "string"
    );
    assert_eq!(out["tool_choice"]["type"], "function");
    assert_eq!(out["tool_choice"]["function"]["name"], "w");
    assert_eq!(out["stop"], json!(["END", "STOP"]));

    let body = json!({"tool_choice": {"type": "none"}});
    let out = anthropic_to_openai_request(body).unwrap();
    assert_eq!(out["tool_choice"], "none");

    // `any` (must call some tool) is OpenAI `required`.
    let body = json!({"tool_choice": {"type": "any"}});
    let out = anthropic_to_openai_request(body).unwrap();
    assert_eq!(out["tool_choice"], "required");
}

#[test]
fn a_to_o_reasoning_effort_extraction() {
    let body = json!({
        "thinking": {"type": "enabled", "budget_tokens": 1024, "effort": "high"},
        "messages": []
    });
    let out = anthropic_to_openai_request(body).unwrap();
    assert_eq!(out["reasoning_effort"], "high");
    assert!(out.get("thinking").is_none());

    let body = json!({"effort": 9, "messages": []});
    let out = anthropic_to_openai_request(body).unwrap();
    assert_eq!(out["reasoning_effort"], "high");

    let body = json!({"effort": 2, "messages": []});
    let out = anthropic_to_openai_request(body).unwrap();
    assert_eq!(out["reasoning_effort"], "low");

    let body = json!({"depth": "none", "messages": []});
    let out = anthropic_to_openai_request(body).unwrap();
    assert_eq!(out["reasoning_effort"], "low");

    let body = json!({"messages": []});
    let out = anthropic_to_openai_request(body).unwrap();
    assert!(out.get("reasoning_effort").is_none());
}

#[test]
fn a_to_o_strips_cache_control_recursively() {
    let body = json!({
        "system": [{"type": "text", "text": "s", "cache_control": {"type": "ephemeral"}}],
        "messages": [{"role": "user", "content": [
            {"type": "text", "text": "t", "cache_control": {"type": "ephemeral"}}
        ]}],
        "tools": [{"name": "t", "input_schema": {}, "cache_control": {"type": "ephemeral"}}]
    });
    let out = anthropic_to_openai_request(body).unwrap();
    let text = serde_json::to_string(&out).unwrap();
    assert!(!text.contains("cache_control"));
}

#[test]
fn o_to_a_system_join_and_image() {
    let body = json!({
        "model": "m",
        "messages": [
            {"role": "system", "content": "be terse"},
            {"role": "developer", "content": "use pt-br"},
            {"role": "user", "content": [
                {"type": "text", "text": "describe"},
                {"type": "image_url", "image_url": {"url": "data:image/jpeg;base64,QkFN"}}
            ]}
        ]
    });
    let out = openai_to_anthropic_request(body).unwrap();
    assert_eq!(out["system"], "be terse\n\nuse pt-br");
    let msgs = out["messages"].as_array().unwrap();
    assert_eq!(msgs[0]["role"], "user");
    let blocks = msgs[0]["content"].as_array().unwrap();
    assert_eq!(blocks[1]["type"], "image");
    assert_eq!(blocks[1]["source"]["type"], "base64");
    assert_eq!(blocks[1]["source"]["media_type"], "image/jpeg");
    assert_eq!(blocks[1]["source"]["data"], "QkFN");
}

#[test]
fn o_to_a_tool_calls_and_tool_message() {
    let body = json!({
        "messages": [
            {"role": "assistant", "content": null, "tool_calls": [
                {"id": "c1", "type": "function", "function": {"name": "w", "arguments": "{\"city\":\"sp\"}"}}
            ]},
            {"role": "tool", "tool_call_id": "c1", "content": "sunny"}
        ],
        "tools": [{"type": "function", "function": {"name": "w", "parameters": {"type": "object", "properties": {}}}}],
        "tool_choice": {"type": "function", "function": {"name": "w"}},
        "stop": ["END"],
        "max_completion_tokens": 200
    });
    let out = openai_to_anthropic_request(body).unwrap();
    let msgs = out["messages"].as_array().unwrap();
    let blocks = msgs[0]["content"].as_array().unwrap();
    assert_eq!(blocks[0]["type"], "tool_use");
    assert_eq!(blocks[0]["id"], "c1");
    assert_eq!(blocks[0]["name"], "w");
    assert_eq!(blocks[0]["input"]["city"], "sp");
    assert_eq!(msgs[1]["content"][0]["type"], "tool_result");
    assert_eq!(msgs[1]["content"][0]["tool_use_id"], "c1");
    assert_eq!(out["tools"][0]["name"], "w");
    assert_eq!(out["tools"][0]["input_schema"]["type"], "object");
    assert_eq!(out["tool_choice"]["type"], "tool");
    assert_eq!(out["tool_choice"]["name"], "w");
    assert_eq!(out["stop_sequences"], json!(["END"]));
    assert_eq!(out["max_tokens"], 200);
}

#[test]
fn o_to_a_tool_choice_strings() {
    let body = json!({"tool_choice": "none", "messages": []});
    let out = openai_to_anthropic_request(body).unwrap();
    assert_eq!(out["tool_choice"]["type"], "none");

    let body = json!({"tool_choice": "required", "messages": []});
    let out = openai_to_anthropic_request(body).unwrap();
    assert_eq!(out["tool_choice"]["type"], "any");
}

#[test]
fn responses_to_openai_chat() {
    let body = json!({
        "model": "m",
        "instructions": "be terse",
        "input": [
            {"role": "user", "type": "message", "content": [{"type": "input_text", "text": "hi"}]},
            {"type": "function_call", "call_id": "c1", "name": "w", "arguments": "{\"q\":\"x\"}"},
            {"type": "function_call_output", "call_id": "c1", "output": "done"}
        ],
        "tools": [
            {"type": "function", "name": "f", "description": "d", "parameters": {"type": "object", "properties": {}}},
            {"type": "web_search"}
        ],
        "max_output_tokens": 300
    });
    let out = responses_to_openai_request(body).unwrap();
    let msgs = out["messages"].as_array().unwrap();
    assert_eq!(msgs[0]["role"], "system");
    assert_eq!(msgs[0]["content"], "be terse");
    assert_eq!(msgs[1]["role"], "user");
    assert_eq!(msgs[2]["role"], "assistant");
    assert_eq!(msgs[2]["tool_calls"][0]["id"], "c1");
    assert_eq!(msgs[3]["role"], "tool");
    assert_eq!(msgs[3]["tool_call_id"], "c1");
    assert_eq!(out["tools"].as_array().unwrap().len(), 1);
    assert_eq!(out["max_tokens"], 300);
}

#[test]
fn responses_to_anthropic_keeps_web_search() {
    let body = json!({
        "input": [],
        "tools": [{"type": "web_search"}]
    });
    let out = responses_to_anthropic_request(body).unwrap();
    let tools = out["tools"].as_array().unwrap();
    assert_eq!(tools[0]["name"], "web_search");
}

#[test]
fn openai_request_to_responses() {
    let body = json!({
        "model": "m",
        "messages": [
            {"role": "system", "content": "be terse"},
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": null, "tool_calls": [
                {"id": "c1", "type": "function", "function": {"name": "w", "arguments": "{\"q\":\"x\"}"}}
            ]},
            {"role": "tool", "tool_call_id": "c1", "content": "done"}
        ],
        "tools": [
            {"type": "function", "function": {"name": "f", "description": "d",
                "parameters": {"type": "object", "properties": {}}}}
        ],
        "max_tokens": 300,
        "n": 1,
        "response_format": {"type": "json_object"}
    });
    let out = openai_to_responses_request(body).unwrap();
    assert_eq!(out["instructions"], "be terse");
    let input = out["input"].as_array().unwrap();
    assert_eq!(input[0]["type"], "message");
    assert_eq!(input[0]["role"], "user");
    assert_eq!(input[0]["content"][0]["type"], "input_text");
    assert_eq!(input[1]["type"], "function_call");
    assert_eq!(input[1]["call_id"], "c1");
    assert_eq!(input[1]["arguments"], "{\"q\":\"x\"}");
    assert_eq!(input[2]["type"], "function_call_output");
    assert_eq!(input[2]["output"], "done");
    // tools flattened to the Responses shape
    assert_eq!(out["tools"][0]["type"], "function");
    assert_eq!(out["tools"][0]["name"], "f");
    assert_eq!(out["max_output_tokens"], 300);
    // chat-only knobs dropped, `store` defaulted to false
    assert!(out.get("n").is_none());
    assert!(out.get("response_format").is_none());
    assert!(out.get("messages").is_none());
    assert_eq!(out["store"], false);
}

#[test]
fn openai_request_to_responses_keeps_explicit_store() {
    let body = json!({"model": "m", "messages": [], "store": true});
    let out = openai_to_responses_request(body).unwrap();
    assert_eq!(out["store"], true);
}

#[test]
fn anthropic_request_to_responses() {
    let body = json!({
        "model": "m",
        "max_tokens": 100,
        "system": "sys",
        "messages": [{"role": "user", "content": "hi"}],
        "tools": [{"name": "f", "description": "d", "input_schema": {"type": "object"}}]
    });
    let out = anthropic_to_responses_request(body).unwrap();
    assert_eq!(out["instructions"], "sys");
    assert_eq!(out["input"][0]["role"], "user");
    assert_eq!(out["input"][0]["content"][0]["text"], "hi");
    assert_eq!(out["tools"][0]["name"], "f");
    assert_eq!(out["max_output_tokens"], 100);
    assert_eq!(out["store"], false);
}

#[test]
fn openai_request_to_responses_translates_reasoning_effort() {
    // The chat spelling has to become a `reasoning` object: forwarding the
    // raw field is a hard upstream error ("Unsupported parameter:
    // reasoning_effort").
    let body = json!({
        "model": "m",
        "messages": [{"role": "user", "content": "hi"}],
        "reasoning_effort": "high"
    });
    let out = openai_to_responses_request(body).unwrap();
    assert_eq!(out["reasoning"]["effort"], "high");
    assert!(out.get("reasoning_effort").is_none());
}

#[test]
fn openai_request_to_responses_keeps_client_reasoning_object() {
    // A Responses-native client sending `reasoning` directly is left alone.
    let body = json!({
        "model": "m",
        "messages": [{"role": "user", "content": "hi"}],
        "reasoning": {"effort": "xhigh", "summary": "auto"}
    });
    let out = openai_to_responses_request(body).unwrap();
    assert_eq!(out["reasoning"]["effort"], "xhigh");
    assert_eq!(out["reasoning"]["summary"], "auto");
}

#[test]
fn anthropic_thinking_effort_reaches_responses() {
    // End to end through the Anthropic -> Responses chain: a client's
    // `thinking.effort` must land as `reasoning.effort`, not be dropped or
    // forwarded as a stray field.
    let body = json!({
        "model": "m",
        "max_tokens": 10,
        "thinking": {"type": "enabled", "effort": "medium"},
        "messages": [{"role": "user", "content": "hi"}]
    });
    let out = anthropic_to_responses_request(body).unwrap();
    assert_eq!(out["reasoning"]["effort"], "medium");
    assert!(out.get("thinking").is_none());
    assert!(out.get("reasoning_effort").is_none());
}

#[test]
fn responses_events_fold_into_one_response() {
    let events = vec![
        json!({"type": "response.created", "response": {"id": "resp_9", "model": "gpt-6-sol", "created_at": 7}}),
        json!({"type": "response.output_item.added", "output_index": 0,
                   "item": {"type": "message", "id": "msg_1", "role": "assistant", "content": []}}),
        json!({"type": "response.output_text.delta", "output_index": 0, "delta": "po"}),
        json!({"type": "response.output_text.delta", "output_index": 0, "delta": "ng"}),
        json!({"type": "response.completed", "response": {"id": "resp_9", "status": "completed",
                   "usage": {"input_tokens": 3, "output_tokens": 2}}}),
    ];
    let out = responses_events_to_response(&events);
    assert_eq!(out["id"], "resp_9");
    assert_eq!(out["object"], "response");
    assert_eq!(out["model"], "gpt-6-sol");
    assert_eq!(out["created_at"], 7);
    assert_eq!(out["status"], "completed");
    assert_eq!(out["output"][0]["type"], "message");
    assert_eq!(out["output"][0]["content"][0]["text"], "pong");
    assert_eq!(out["output"][0]["content"][0]["type"], "output_text");
    assert_eq!(out["usage"]["output_tokens"], 2);
}

#[test]
fn responses_events_fold_function_calls() {
    let events = vec![
        json!({"type": "response.output_item.added", "output_index": 0,
                   "item": {"type": "function_call", "call_id": "c1", "name": "w", "arguments": ""}}),
        json!({"type": "response.function_call_arguments.delta", "output_index": 0, "delta": "{\"q\":"}),
        json!({"type": "response.function_call_arguments.delta", "output_index": 0, "delta": "1}"}),
    ];
    let out = responses_events_to_response(&events);
    let item = &out["output"][0];
    assert_eq!(item["type"], "function_call");
    assert_eq!(item["name"], "w");
    assert_eq!(item["arguments"], "{\"q\":1}");
    assert_eq!(item["status"], "completed");
}

#[test]
fn responses_events_prefer_completed_output_over_deltas() {
    // A completed event carrying the finished items wins over the deltas.
    let events = vec![
        json!({"type": "response.output_text.delta", "output_index": 0, "delta": "partial"}),
        json!({"type": "response.completed", "response": {
                "status": "completed",
                "output": [{"type": "message", "content": [{"type": "output_text", "text": "final"}]}],
                "usage": {"input_tokens": 1, "output_tokens": 1}}}),
    ];
    let out = responses_events_to_response(&events);
    assert_eq!(out["output"][0]["content"][0]["text"], "final");
}

#[test]
fn responses_events_surface_errors_and_incomplete_status() {
    let failed = vec![
        json!({"type": "response.failed", "response": {"status": "failed",
            "error": {"message": "boom"}}}),
    ];
    let out = responses_events_to_response(&failed);
    assert_eq!(out["status"], "failed");
    assert_eq!(out["error"]["message"], "boom");

    let incomplete = vec![json!({"type": "response.completed",
            "response": {"status": "incomplete"}})];
    assert_eq!(
        responses_events_to_response(&incomplete)["status"],
        "incomplete"
    );
}

#[test]
fn responses_events_tolerate_an_empty_stream() {
    let out = responses_events_to_response(&[]);
    assert_eq!(out["object"], "response");
    assert_eq!(out["status"], "completed");
    assert_eq!(out["output"].as_array().unwrap().len(), 0);
}

#[test]
fn responses_response_to_openai_chat() {
    let body = json!({
        "id": "resp_1",
        "created_at": 42,
        "status": "completed",
        "output": [
            {"type": "message", "role": "assistant",
             "content": [{"type": "output_text", "text": "hi", "annotations": []}]},
            {"type": "function_call", "call_id": "c1", "name": "w", "arguments": "{\"q\":1}"}
        ],
        "usage": {"input_tokens": 3, "output_tokens": 2}
    });
    let out = responses_to_openai_response(body, "m").unwrap();
    assert_eq!(out["id"], "resp_1");
    assert_eq!(out["object"], "chat.completion");
    assert_eq!(out["created"], 42);
    assert_eq!(out["choices"][0]["message"]["content"], "hi");
    assert_eq!(out["choices"][0]["message"]["tool_calls"][0]["id"], "c1");
    assert_eq!(
        out["choices"][0]["message"]["tool_calls"][0]["function"]["name"],
        "w"
    );
    assert_eq!(out["choices"][0]["finish_reason"], "tool_calls");
    assert_eq!(out["usage"]["prompt_tokens"], 3);
    assert_eq!(out["usage"]["completion_tokens"], 2);
}

#[test]
fn responses_response_to_openai_chat_without_tools() {
    let body = json!({
        "output": [{"type": "message", "content": [{"type": "output_text", "text": "ok"}]}]
    });
    let out = responses_to_openai_response(body, "m").unwrap();
    assert_eq!(out["choices"][0]["finish_reason"], "stop");
    assert!(out["choices"][0]["message"].get("tool_calls").is_none());
}

#[test]
fn responses_response_to_anthropic() {
    let body = json!({
        "id": "resp_1",
        "output": [{"type": "message", "content": [{"type": "output_text", "text": "hi"}]}],
        "usage": {"input_tokens": 3, "output_tokens": 2}
    });
    let out = responses_to_anthropic_response(body, "m").unwrap();
    assert_eq!(out["type"], "message");
    assert_eq!(out["content"][0]["text"], "hi");
    assert_eq!(out["usage"]["output_tokens"], 2);
}

#[test]
fn anthropic_response_to_openai() {
    let body = json!({
        "id": "msg_1",
        "type": "message",
        "role": "assistant",
        "model": "claude",
        "content": [
            {"type": "text", "text": "hello"},
            {"type": "tool_use", "id": "c1", "name": "w", "input": {"q": "x"}}
        ],
        "stop_reason": "tool_use",
        "stop_sequence": null,
        "usage": {"input_tokens": 10, "output_tokens": 5}
    });
    let out = anthropic_to_openai_response(body, "claude").unwrap();
    assert_eq!(out["object"], "chat.completion");
    let choice = &out["choices"][0];
    assert_eq!(choice["finish_reason"], "tool_calls");
    assert_eq!(choice["message"]["content"], "hello");
    assert_eq!(choice["message"]["tool_calls"][0]["id"], "c1");
    assert_eq!(out["usage"]["prompt_tokens"], 10);
    assert_eq!(out["usage"]["completion_tokens"], 5);
    assert_eq!(out["usage"]["total_tokens"], 15);
}

#[test]
fn openai_response_to_anthropic() {
    let body = json!({
        "id": "cmpl_1",
        "object": "chat.completion",
        "created": 123,
        "model": "gpt",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "hi", "reasoning_content": "secret thinking"},
            "finish_reason": "stop",
            "logprobs": null
        }],
        "usage": {"prompt_tokens": 7, "completion_tokens": 3, "total_tokens": 10, "completion_tokens_details": {"reasoning_tokens": 2}}
    });
    let out = openai_to_anthropic_response(body, "gpt").unwrap();
    assert_eq!(out["type"], "message");
    assert_eq!(out["stop_reason"], "end_turn");
    let text = serde_json::to_string(&out["content"]).unwrap();
    assert!(!text.contains("secret thinking"));
    assert_eq!(out["usage"]["input_tokens"], 7);
    assert_eq!(out["usage"]["output_tokens"], 3);
}

#[test]
fn anthropic_response_to_responses() {
    let body = json!({
        "id": "msg_1",
        "content": [{"type": "text", "text": "hi"}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 4, "output_tokens": 2}
    });
    let out = anthropic_to_responses_response(body, "claude").unwrap();
    assert_eq!(out["object"], "response");
    assert_eq!(out["output"][0]["type"], "message");
    assert_eq!(out["output"][0]["content"][0]["type"], "output_text");
    assert_eq!(out["usage"]["input_tokens"], 4);
    assert_eq!(out["usage"]["output_tokens"], 2);
}

#[test]
fn openai_response_to_responses() {
    let body = json!({
        "id": "cmpl_1",
        "created": 123,
        "model": "gpt",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "hi", "tool_calls": []},
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 5, "completion_tokens": 5, "total_tokens": 10}
    });
    let out = openai_to_responses_response(body, "gpt").unwrap();
    assert_eq!(out["object"], "response");
    assert_eq!(out["output"][0]["content"][0]["text"], "hi");
    assert_eq!(out["usage"]["total_tokens"], 10);
}

#[test]
fn unmodeled_fields_do_not_leak_but_stay_probeable() {
    let body = json!({
        "model": "m",
        "max_tokens": 10,
        "metadata": {"user_id": "u"},
        "context_management": {"edits": []},
        "messages": [{"role": "user", "content": "hi"}]
    });
    let ir = decode_request(Format::Anthropic, body).unwrap();
    assert!(ir.ext.has("anthropic:context_management"));
    assert_eq!(ir.ext.get("anthropic:metadata").unwrap()["user_id"], "u");
    assert_eq!(ir.ext.namespace("anthropic").count(), 2);
    for to in [Format::Openai, Format::Responses] {
        let out = encode_request(to, &ir);
        assert!(out.get("metadata").is_none(), "{to:?}: {out}");
        assert!(out.get("context_management").is_none(), "{to:?}: {out}");
    }
}

#[test]
fn anthropic_cache_control_survives_the_ir() {
    let body = json!({
        "model": "m",
        "max_tokens": 10,
        "system": [{"type": "text", "text": "sys", "cache_control": {"type": "ephemeral", "ttl": "1h"}}],
        "tools": [{"name": "t", "input_schema": {"type": "object"}, "cache_control": {"type": "ephemeral"}}],
        "messages": [{"role": "user", "content": [
            {"type": "text", "text": "a", "cache_control": {"type": "ephemeral"}}
        ]}]
    });
    let out = encode_request(
        Format::Anthropic,
        &decode_request(Format::Anthropic, body).unwrap(),
    );
    assert_eq!(out["system"][0]["cache_control"]["ttl"], "1h");
    assert_eq!(out["tools"][0]["cache_control"]["type"], "ephemeral");
    assert_eq!(
        out["messages"][0]["content"][0]["cache_control"]["type"],
        "ephemeral"
    );
}

/// A conversation, optionally with one more exchange appended.
fn conversation(extra_turn: bool) -> Value {
    let mut messages = vec![
        json!({"role": "user", "content": "find the bug"}),
        json!({"role": "assistant", "content": [
            {"type": "text", "text": "looking"},
            {"type": "tool_use", "id": "t1", "name": "read", "input": {"path": "a.rs"}}
        ]}),
        json!({"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "t1", "content": "fn main() {}"}
        ]}),
    ];
    if extra_turn {
        messages.push(json!({"role": "assistant", "content": "found it"}));
        messages.push(json!({"role": "user", "content": "fix it"}));
    }
    json!({
        "model": "m",
        "max_tokens": 100,
        "system": "be terse",
        "tools": [{"name": "read", "input_schema": {"type": "object", "properties": {"path": {"type": "string"}}}}],
        "messages": messages
    })
}

#[test]
fn encoding_keeps_the_cached_prefix_byte_identical() {
    // Prompt caches key on a byte-identical prefix: a later turn must only
    // append, never rewrite what an earlier turn encoded.
    for to in [Format::Anthropic, Format::Openai, Format::Responses] {
        let short = translate_request(Format::Anthropic, to, conversation(false)).unwrap();
        let long = translate_request(Format::Anthropic, to, conversation(true)).unwrap();
        let key = if to == Format::Responses {
            "input"
        } else {
            "messages"
        };
        let (s, l) = (
            short[key].as_array().unwrap(),
            long[key].as_array().unwrap(),
        );
        assert!(l.len() > s.len(), "{to:?}");
        assert_eq!(s.as_slice(), &l[..s.len()], "{to:?}: prefix changed");
        for field in ["system", "instructions", "tools"] {
            assert_eq!(short.get(field), long.get(field), "{to:?}: {field} changed");
        }
        // and encoding is deterministic
        let again = translate_request(Format::Anthropic, to, conversation(false)).unwrap();
        assert_eq!(
            serde_json::to_string(&short).unwrap(),
            serde_json::to_string(&again).unwrap()
        );
    }
}

#[test]
fn cache_usage_is_reported_across_formats() {
    // Anthropic reports cached tokens apart from input_tokens.
    let body = json!({
        "id": "msg_1",
        "content": [{"type": "text", "text": "hi"}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 10, "output_tokens": 2, "cache_read_input_tokens": 90, "cache_creation_input_tokens": 5}
    });
    let r = decode_response(Format::Anthropic, &body);
    assert_eq!(r.usage.input, 105);
    assert_eq!(r.usage.cache_read, Some(90));
    let openai = encode_response(Format::Openai, &r);
    assert_eq!(openai["usage"]["prompt_tokens"], 105);
    assert_eq!(
        openai["usage"]["prompt_tokens_details"]["cached_tokens"],
        90
    );
    let responses = encode_response(Format::Responses, &r);
    assert_eq!(
        responses["usage"]["input_tokens_details"]["cached_tokens"],
        90
    );
    // and back to Anthropic's split shape
    let back = encode_response(Format::Anthropic, &decode_response(Format::Openai, &openai));
    assert_eq!(back["usage"]["cache_read_input_tokens"], 90);
    assert_eq!(back["usage"]["input_tokens"], 15);
}

#[test]
fn emitter_preserves_an_explicit_zero_cache_read_report() {
    let mut emitter = Emitter::default();
    emitter.usage(crate::domain::translate::TokenUsage {
        cache_read: Some(0),
        ..crate::domain::translate::TokenUsage::default()
    });
    emitter.usage(crate::domain::translate::TokenUsage::default());
    assert_eq!(emitter.usage.cache_read, Some(0));
}

#[test]
fn effort_keeps_its_full_range_where_the_format_has_it() {
    let body = json!({"model": "m", "max_tokens": 5, "output_config": {"effort": "max"},
        "messages": [{"role": "user", "content": "hi"}]});
    let ir = decode_request(Format::Anthropic, body).unwrap();
    assert_eq!(ir.effort, Some(Effort::Max));
    assert_eq!(
        encode_request(Format::Responses, &ir)["reasoning"]["effort"],
        "xhigh"
    );
    assert_eq!(
        encode_request(Format::Openai, &ir)["reasoning_effort"],
        "high"
    );
    assert_eq!(Effort::parse(&json!("min")), Some(Effort::Minimal));
}

#[test]
fn parallel_tool_calls_and_results_group_into_one_turn() {
    // Responses sends each call and each output as its own item; chat
    // providers need all results right after the assistant turn.
    let body = json!({"model": "m", "input": [
        {"type": "message", "role": "user", "content": "go"},
        {"type": "function_call", "call_id": "a", "name": "f", "arguments": "{}"},
        {"type": "function_call", "call_id": "b", "name": "f", "arguments": "{}"},
        {"type": "function_call_output", "call_id": "a", "output": "1"},
        {"type": "function_call_output", "call_id": "b", "output": "2"}
    ]});
    let out = translate_request(Format::Responses, Format::Openai, body).unwrap();
    let roles: Vec<&str> = out["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["role"].as_str().unwrap())
        .collect();
    assert_eq!(roles, ["user", "assistant", "tool", "tool"]);
    assert_eq!(
        out["messages"][1]["tool_calls"].as_array().unwrap().len(),
        2
    );
}
