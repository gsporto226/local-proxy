use serde_json::{json, Value};

/// Errors that occur while translating requests/responses between provider
/// wire formats.
#[derive(Debug, thiserror::Error)]
pub enum TranslateError {
    /// The request body is not a JSON object.
    #[error("request body is not a JSON object")]
    NotObject,
    /// A required field is missing from the payload.
    #[error("missing required field: {0}")]
    MissingField(&'static str),
    /// A field carries an invalid value.
    #[error("invalid value for {field}: {detail}")]
    Invalid {
        /// The offending field name.
        field: &'static str,
        /// A human-readable description of the invalid value.
        detail: String,
    },
}

const DEFAULT_MAX_TOKENS: u64 = 4096;

// ---------------------------------------------------------------------------
// small value helpers
// ---------------------------------------------------------------------------

fn take(value: &mut Value, key: &str) -> Option<Value> {
    value.as_object_mut().and_then(|o| o.remove(key))
}

fn remove(value: &mut Value, key: &str) {
    if let Some(o) = value.as_object_mut() {
        o.remove(key);
    }
}

fn insert(value: &mut Value, key: &str, val: Value) {
    if let Some(o) = value.as_object_mut() {
        o.insert(key.to_string(), val);
    }
}

fn content_blocks(content: &Value) -> Vec<Value> {
    match content {
        Value::String(s) => vec![json!({"type": "text", "text": s})],
        Value::Array(arr) => arr.clone(),
        _ => Vec::new(),
    }
}

fn content_to_text(content: Value) -> String {
    match content {
        Value::String(s) => s,
        Value::Array(arr) => arr
            .iter()
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

/// Build the `content` value for an `OpenAI` user message. Pure-text parts become
/// a plain string (most compatible); anything with images stays an array.
fn openai_user_message(parts: Vec<Value>) -> Value {
    if parts.is_empty() {
        return Value::Null;
    }
    if parts
        .iter()
        .all(|p| p.get("type").and_then(Value::as_str) == Some("text"))
    {
        let joined = parts
            .iter()
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n");
        return json!(joined);
    }
    Value::Array(parts)
}

fn parse_arguments(args: Value) -> Value {
    match args {
        Value::String(s) => serde_json::from_str(&s).unwrap_or(Value::String(s)),
        other => other,
    }
}

fn arguments_to_string(args: Value) -> String {
    match args {
        Value::String(s) => s,
        other => serde_json::to_string(&other).unwrap_or_else(|_| "{}".to_string()),
    }
}

fn now_ts() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Recursively remove `cache_control` keys (Anthropic-only) from a value tree.
fn strip_cache_control(mut value: Value) -> Value {
    match &mut value {
        Value::Object(map) => {
            map.remove("cache_control");
            for v in map.values_mut() {
                *v = strip_cache_control(std::mem::take(v));
            }
        }
        Value::Array(arr) => {
            for v in arr {
                *v = strip_cache_control(std::mem::take(v));
            }
        }
        _ => {}
    }
    value
}

// ---------------------------------------------------------------------------
// reasoning effort extraction (anthropic -> openai)
// ---------------------------------------------------------------------------

fn normalize_effort(s: &str) -> Option<String> {
    match s.trim().to_ascii_lowercase().as_str() {
        "low" | "minimal" | "none" => Some("low".to_string()),
        "medium" | "moderate" => Some("medium".to_string()),
        "high" | "max" | "full" => Some("high".to_string()),
        _ => None,
    }
}

fn extract_reasoning_effort(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => normalize_effort(s),
        Value::Number(n) => {
            let f = n.as_f64()?;
            if f <= 3.0 {
                Some("low".to_string())
            } else if f <= 7.0 {
                Some("medium".to_string())
            } else if f <= 10.0 {
                Some("high".to_string())
            } else {
                None
            }
        }
        Value::Object(map) => {
            for key in ["effort", "level", "reasoning_effort", "depth"] {
                if let Some(v) = map.get(key) {
                    if let Some(s) = extract_reasoning_effort(v) {
                        return Some(s);
                    }
                }
            }
            None
        }
        _ => None,
    }
}

fn take_reasoning_effort(out: &mut Value) -> Option<String> {
    for key in [
        "thinking",
        "reasoning",
        "reasoning_effort",
        "effort",
        "level",
        "depth",
        "output_config",
    ] {
        if let Some(v) = take(out, key) {
            if let Some(e) = extract_reasoning_effort(&v) {
                return Some(e);
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// anthropic request -> openai request
// ---------------------------------------------------------------------------

/// Translate an Anthropic Messages request body into an `OpenAI`
/// chat-completions request body.
///
/// # Errors
///
/// Returns [`TranslateError::NotObject`] if the body is not a JSON object, or
/// [`TranslateError::Invalid`] when a field has an unexpected shape.
pub fn anthropic_to_openai_request(body: Value) -> Result<Value, TranslateError> {
    let mut out = strip_cache_control(body);
    if out.as_object().is_none() {
        return Err(TranslateError::NotObject);
    }

    let max_tokens = take(&mut out, "max_tokens");
    let system = take(&mut out, "system");
    let messages = take(&mut out, "messages");
    let tools = take(&mut out, "tools");
    let tool_choice = take(&mut out, "tool_choice");
    let stop_sequences = take(&mut out, "stop_sequences");
    let reasoning_effort = take_reasoning_effort(&mut out);

    remove(&mut out, "top_k");

    let mut openai_messages: Vec<Value> = Vec::new();
    if let Some(sys) = system {
        let text = system_to_text(&sys);
        openai_messages.push(json!({"role": "system", "content": text}));
    }
    if let Some(msgs) = messages {
        let arr = msgs.as_array().ok_or_else(|| TranslateError::Invalid {
            field: "messages",
            detail: "expected array".to_string(),
        })?;
        for m in arr {
            convert_anthropic_message(m, &mut openai_messages)?;
        }
    }
    insert(&mut out, "messages", Value::Array(openai_messages));

    insert(
        &mut out,
        "max_tokens",
        max_tokens.unwrap_or_else(|| json!(DEFAULT_MAX_TOKENS)),
    );

    if let Some(tools) = tools {
        insert(&mut out, "tools", anthropic_tools_to_openai(&tools)?);
    }
    if let Some(tc) = tool_choice {
        insert(
            &mut out,
            "tool_choice",
            anthropic_tool_choice_to_openai(&tc),
        );
    }
    if let Some(stops) = stop_sequences {
        insert(&mut out, "stop", stops);
    }
    if let Some(re) = reasoning_effort {
        insert(&mut out, "reasoning_effort", json!(re));
    }

    Ok(out)
}

fn system_to_text(system: &Value) -> String {
    match system {
        Value::String(s) => s.clone(),
        Value::Array(arr) => arr
            .iter()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn convert_anthropic_message(msg: &Value, out: &mut Vec<Value>) -> Result<(), TranslateError> {
    let role = msg
        .get("role")
        .and_then(Value::as_str)
        .ok_or(TranslateError::MissingField("message.role"))?;
    let content = msg.get("content").cloned().unwrap_or(Value::Null);

    match role {
        "user" => {
            let mut current_text: Vec<Value> = Vec::new();
            for block in content_blocks(&content) {
                match block.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        if let Some(t) = block.get("text").and_then(Value::as_str) {
                            if let Some(last) = current_text.last_mut() {
                                if last.get("type").and_then(Value::as_str) == Some("text") {
                                    let prev = last["text"].as_str().unwrap_or("").to_string();
                                    last["text"] = json!(format!("{prev}{t}"));
                                    continue;
                                }
                            }
                        }
                        current_text.push(block);
                    }
                    Some("image") => {
                        current_text.push(anthropic_image_to_openai(&block)?);
                    }
                    Some("tool_result") => {
                        if !current_text.is_empty() {
                            let v = openai_user_message(std::mem::take(&mut current_text));
                            out.push(json!({"role": "user", "content": v}));
                        }
                        out.push(anthropic_tool_result_to_openai(&block));
                    }
                    _ => {}
                }
            }
            if !current_text.is_empty() {
                let v = openai_user_message(std::mem::take(&mut current_text));
                out.push(json!({"role": "user", "content": v}));
            }
        }
        "assistant" => {
            let mut texts = Vec::new();
            let mut tool_calls = Vec::new();
            for block in content_blocks(&content) {
                match block.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        if let Some(t) = block.get("text").and_then(Value::as_str) {
                            texts.push(t.to_string());
                        }
                    }
                    Some("tool_use") => {
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
                        let input = block.get("input").cloned().unwrap_or(Value::Null);
                        tool_calls.push(json!({
                            "id": id,
                            "type": "function",
                            "function": { "name": name, "arguments": arguments_to_string(input) }
                        }));
                    }
                    _ => {}
                }
            }
            let mut m = json!({"role": "assistant"});
            if !texts.is_empty() {
                m["content"] = json!(texts.join("\n"));
            }
            if !tool_calls.is_empty() {
                m["tool_calls"] = Value::Array(tool_calls);
            }
            out.push(m);
        }
        "tool" => {
            let id = msg
                .get("tool_call_id")
                .or_else(|| msg.get("tool_use_id"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let text = content_to_text(content);
            out.push(json!({"role": "tool", "tool_call_id": id, "content": text}));
        }
        _ => {}
    }
    Ok(())
}

fn anthropic_image_to_openai(block: &Value) -> Result<Value, TranslateError> {
    let source = block
        .get("source")
        .ok_or(TranslateError::MissingField("image.source"))?;
    let stype = source.get("type").and_then(Value::as_str).unwrap_or("");
    let media_type = source
        .get("media_type")
        .and_then(Value::as_str)
        .unwrap_or("image/png");
    let url = match stype {
        "base64" => {
            let data = source.get("data").and_then(Value::as_str).unwrap_or("");
            format!("data:{media_type};base64,{data}")
        }
        "url" => source
            .get("url")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        _ => {
            return Err(TranslateError::Invalid {
                field: "image.source.type",
                detail: stype.to_string(),
            })
        }
    };
    Ok(json!({"type": "image_url", "image_url": {"url": url}}))
}

fn anthropic_tool_result_to_openai(block: &Value) -> Value {
    let id = block
        .get("tool_call_id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let text = content_to_text(block.get("content").cloned().unwrap_or(Value::Null));
    json!({"role": "tool", "tool_call_id": id, "content": text})
}

fn anthropic_tools_to_openai(tools: &Value) -> Result<Value, TranslateError> {
    let arr = tools.as_array().ok_or_else(|| TranslateError::Invalid {
        field: "tools",
        detail: "expected array".to_string(),
    })?;
    let mut out = Vec::new();
    for tool in arr {
        let name = tool
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let description = tool
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let input_schema = tool
            .get("input_schema")
            .cloned()
            .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
        out.push(json!({
            "type": "function",
            "function": {
                "name": name,
                "description": description,
                "parameters": input_schema
            }
        }));
    }
    Ok(Value::Array(out))
}

fn anthropic_tool_choice_to_openai(tc: &Value) -> Value {
    let t = tc.get("type").and_then(Value::as_str).unwrap_or("");
    match t {
        "none" => json!("none"),
        "tool" => {
            let name = tc
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            json!({"type": "function", "function": {"name": name}})
        }
        _ => json!("auto"),
    }
}

// ---------------------------------------------------------------------------
// openai request -> anthropic request
// ---------------------------------------------------------------------------

/// Translate an `OpenAI` chat-completions request body into an Anthropic
/// Messages request body.
///
/// # Errors
///
/// Returns [`TranslateError::NotObject`] if the body is not a JSON object, or
/// [`TranslateError::Invalid`] when a field has an unexpected shape.
pub fn openai_to_anthropic_request(body: Value) -> Result<Value, TranslateError> {
    let mut out = body;
    if out.as_object().is_none() {
        return Err(TranslateError::NotObject);
    }

    let messages = take(&mut out, "messages");
    let tools = take(&mut out, "tools");
    let tool_choice = take(&mut out, "tool_choice");
    let stop = take(&mut out, "stop");
    let max_tokens =
        take(&mut out, "max_tokens").or_else(|| take(&mut out, "max_completion_tokens"));

    for key in ["stream_options", "reasoning_effort"] {
        remove(&mut out, key);
    }

    let mut system_parts: Vec<String> = Vec::new();
    let mut anthropic_messages: Vec<Value> = Vec::new();
    if let Some(msgs) = messages {
        let arr = msgs.as_array().ok_or_else(|| TranslateError::Invalid {
            field: "messages",
            detail: "expected array".to_string(),
        })?;
        for m in arr {
            match m.get("role").and_then(Value::as_str).unwrap_or("") {
                "system" | "developer" => {
                    if let Some(c) = m.get("content") {
                        let text = content_to_text(c.clone());
                        if !text.is_empty() {
                            system_parts.push(text);
                        }
                    }
                }
                "user" => anthropic_messages.push(openai_user_to_anthropic(m)),
                "assistant" => anthropic_messages.push(openai_assistant_to_anthropic(m)),
                "tool" => anthropic_messages.push(openai_tool_to_anthropic(m)),
                _ => {}
            }
        }
    }

    if !system_parts.is_empty() {
        insert(&mut out, "system", json!(system_parts.join("\n\n")));
    }
    if !anthropic_messages.is_empty() {
        insert(&mut out, "messages", Value::Array(anthropic_messages));
    }
    if let Some(tools) = tools {
        insert(&mut out, "tools", openai_tools_to_anthropic(&tools)?);
    }
    if let Some(tc) = tool_choice {
        insert(
            &mut out,
            "tool_choice",
            openai_tool_choice_to_anthropic(&tc),
        );
    }
    if let Some(stop) = stop {
        let stops = match &stop {
            Value::String(_) => json!([stop]),
            Value::Array(_) => stop,
            _ => json!([]),
        };
        insert(&mut out, "stop_sequences", stops);
    }
    if let Some(mt) = max_tokens {
        insert(&mut out, "max_tokens", mt);
    }

    Ok(out)
}

fn openai_user_to_anthropic(m: &Value) -> Value {
    let content = m.get("content").cloned().unwrap_or(Value::Null);
    let mut blocks = Vec::new();
    match content {
        Value::String(s) => blocks.push(json!({"type": "text", "text": s})),
        Value::Array(arr) => {
            for b in arr {
                match b.get("type").and_then(Value::as_str) {
                    Some("text") => blocks.push(b),
                    Some("image_url") => blocks.push(openai_image_to_anthropic(&b)),
                    _ => {}
                }
            }
        }
        _ => {}
    }
    if blocks.is_empty() {
        blocks.push(json!({"type": "text", "text": ""}));
    }
    json!({"role": "user", "content": blocks})
}

fn openai_image_to_anthropic(block: &Value) -> Value {
    let url = block
        .get("image_url")
        .and_then(|u| u.get("url"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    url.strip_prefix("data:").map_or_else(
        || json!({"type": "image", "source": {"type": "url", "url": url.clone()}}),
        |rest| {
            let (meta, data) = rest.split_once(',').unwrap_or((rest, ""));
            let media_type = meta.trim_end_matches(";base64").to_string();
            json!({
                "type": "image",
                "source": {"type": "base64", "media_type": media_type, "data": data}
            })
        },
    )
}

fn openai_assistant_to_anthropic(m: &Value) -> Value {
    let content = m.get("content").cloned().unwrap_or(Value::Null);
    let mut blocks: Vec<Value> = Vec::new();
    match content {
        Value::String(s) => {
            if !s.is_empty() {
                blocks.push(json!({"type": "text", "text": s}));
            }
        }
        Value::Array(arr) => {
            for b in arr {
                if b.get("type").and_then(Value::as_str) == Some("text") {
                    blocks.push(b);
                }
            }
        }
        _ => {}
    }
    if let Some(tool_calls) = m.get("tool_calls").and_then(Value::as_array) {
        for tc in tool_calls {
            let id = tc
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let name = tc
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let args = tc
                .get("function")
                .and_then(|f| f.get("arguments"))
                .cloned()
                .unwrap_or(Value::Null);
            blocks.push(
                json!({"type": "tool_use", "id": id, "name": name, "input": parse_arguments(args)}),
            );
        }
    }
    json!({"role": "assistant", "content": blocks})
}

fn openai_tool_to_anthropic(m: &Value) -> Value {
    let id = m
        .get("tool_call_id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let text = content_to_text(m.get("content").cloned().unwrap_or(Value::Null));
    json!({
        "role": "user",
        "content": [{"type": "tool_result", "tool_use_id": id, "content": text}]
    })
}

fn openai_tools_to_anthropic(tools: &Value) -> Result<Value, TranslateError> {
    let arr = tools.as_array().ok_or_else(|| TranslateError::Invalid {
        field: "tools",
        detail: "expected array".to_string(),
    })?;
    let mut out = Vec::new();
    for tool in arr {
        if tool.get("type").and_then(Value::as_str) != Some("function") {
            continue;
        }
        let func = tool.get("function").cloned().unwrap_or(Value::Null);
        let name = func
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let description = func
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let mut parameters = func
            .get("parameters")
            .cloned()
            .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
        if !parameters.is_object() {
            parameters = json!({"type": "object", "properties": {}});
        }
        out.push(json!({"name": name, "description": description, "input_schema": parameters}));
    }
    Ok(Value::Array(out))
}

fn openai_tool_choice_to_anthropic(tc: &Value) -> Value {
    match tc {
        Value::String(s) => match s.as_str() {
            "none" => json!({"type": "none"}),
            "required" => json!({"type": "any"}),
            _ => json!({"type": "auto"}),
        },
        Value::Object(_) => {
            if tc.get("type").and_then(Value::as_str) == Some("function") {
                let name = tc
                    .get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                json!({"type": "tool", "name": name})
            } else {
                json!({"type": "auto"})
            }
        }
        _ => json!({"type": "auto"}),
    }
}

// ---------------------------------------------------------------------------
// responses request -> openai/anthropic request
// ---------------------------------------------------------------------------

/// Translate a Responses API request body into an `OpenAI` chat-completions
/// request body.
///
/// # Errors
///
/// Returns [`TranslateError::NotObject`] if the body is not a JSON object, or
/// [`TranslateError::Invalid`] when a field has an unexpected shape.
pub fn responses_to_openai_request(body: Value) -> Result<Value, TranslateError> {
    responses_to_chat(body, false)
}

/// Translate a Responses API request body into an Anthropic Messages request
/// body, keeping built-in tools (e.g. `web_search`) as synthetic functions.
///
/// # Errors
///
/// Returns [`TranslateError::NotObject`] if the body is not a JSON object, or
/// [`TranslateError::Invalid`] when a field has an unexpected shape.
pub fn responses_to_anthropic_request(body: Value) -> Result<Value, TranslateError> {
    let chat = responses_to_chat(body, true)?;
    openai_to_anthropic_request(chat)
}

/// Convert a Responses API request to an `OpenAI` chat-completions request.
/// When `keep_builtin_tools` is set, `web_search`/`web_fetch` are kept as
/// synthetic function tools (for providers that cannot handle them natively,
/// e.g. Anthropic).
fn responses_to_chat(body: Value, keep_builtin_tools: bool) -> Result<Value, TranslateError> {
    let mut out = body;
    if out.as_object().is_none() {
        return Err(TranslateError::NotObject);
    }

    let input = take(&mut out, "input");
    let instructions = take(&mut out, "instructions");
    let tools = take(&mut out, "tools");
    let max_output_tokens = take(&mut out, "max_output_tokens");

    let mut messages: Vec<Value> = Vec::new();
    if let Some(inst) = instructions {
        if let Some(s) = inst.as_str() {
            if !s.is_empty() {
                messages.push(json!({"role": "system", "content": s}));
            }
        }
    }
    if let Some(input) = input {
        let arr = input.as_array().ok_or_else(|| TranslateError::Invalid {
            field: "input",
            detail: "expected array".to_string(),
        })?;
        for item in arr {
            convert_responses_item_to_chat(item, &mut messages);
        }
    }
    if !messages.is_empty() {
        insert(&mut out, "messages", Value::Array(messages));
    }
    if let Some(tools) = tools {
        insert(
            &mut out,
            "tools",
            responses_tools_to_chat(&tools, keep_builtin_tools)?,
        );
    }
    if let Some(mot) = max_output_tokens {
        insert(&mut out, "max_tokens", mot);
    }

    Ok(out)
}

fn convert_responses_item_to_chat(item: &Value, out: &mut Vec<Value>) {
    let itype = item
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("message");
    match itype {
        "message" => {
            let role = item.get("role").and_then(Value::as_str).unwrap_or("user");
            let content = item.get("content").cloned().unwrap_or(Value::Null);
            let mut blocks: Vec<Value> = Vec::new();
            match content {
                Value::String(s) => blocks.push(json!({"type": "text", "text": s})),
                Value::Array(arr) => {
                    for b in arr {
                        match b.get("type").and_then(Value::as_str) {
                            Some("input_text" | "output_text") => {
                                blocks.push(json!({"type": "text", "text": b.get("text").cloned().unwrap_or(Value::Null)}));
                            }
                            Some("input_image") => {
                                let url = b
                                    .get("image_url")
                                    .and_then(Value::as_str)
                                    .unwrap_or("")
                                    .to_string();
                                blocks
                                    .push(json!({"type": "image_url", "image_url": {"url": url}}));
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
            if role == "system" || role == "developer" {
                let text = blocks
                    .iter()
                    .filter_map(|b| b.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join("\n");
                if let Some(existing) = out
                    .iter_mut()
                    .find(|m| m.get("role").and_then(Value::as_str) == Some("system"))
                {
                    if let Some(c) = existing.get_mut("content") {
                        if let Some(s) = c.as_str() {
                            *c = json!(format!("{s}\n\n{text}"));
                        }
                    }
                } else if !text.is_empty() {
                    out.push(json!({"role": "system", "content": text}));
                }
            } else {
                let content = if blocks.len() == 1
                    && blocks[0].get("type").and_then(Value::as_str) == Some("text")
                {
                    blocks[0].get("text").cloned().unwrap_or(Value::Null)
                } else {
                    Value::Array(blocks)
                };
                out.push(json!({"role": role, "content": content}));
            }
        }
        "function_call" => {
            let id = item
                .get("call_id")
                .or_else(|| item.get("id"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let name = item
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let args = item.get("arguments").cloned().unwrap_or(Value::Null);
            out.push(json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": id,
                    "type": "function",
                    "function": {"name": name, "arguments": arguments_to_string(args)}
                }]
            }));
        }
        "function_call_output" => {
            let id = item
                .get("call_id")
                .or_else(|| item.get("id"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let output = item.get("output").cloned().unwrap_or(Value::Null);
            let text = content_to_text(output);
            out.push(json!({"role": "tool", "tool_call_id": id, "content": text}));
        }
        _ => {}
    }
}

fn responses_tools_to_chat(
    tools: &Value,
    keep_builtin_tools: bool,
) -> Result<Value, TranslateError> {
    let arr = tools.as_array().ok_or_else(|| TranslateError::Invalid {
        field: "tools",
        detail: "expected array".to_string(),
    })?;
    let mut out = Vec::new();
    for tool in arr {
        match tool.get("type").and_then(Value::as_str) {
            Some("function") => {
                let name = tool
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let description = tool
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let parameters = tool
                    .get("parameters")
                    .cloned()
                    .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
                out.push(json!({
                    "type": "function",
                    "function": {"name": name, "description": description, "parameters": parameters}
                }));
            }
            Some("web_search" | "web_fetch") if keep_builtin_tools => {
                out.push(json!({
                    "type": "function",
                    "function": {
                        "name": "web_search",
                        "description": "Search the web",
                        "parameters": {"type": "object", "properties": {"query": {"type": "string"}}, "required": ["query"]}
                    }
                }));
            }
            _ => {}
        }
    }
    Ok(Value::Array(out))
}

// ---------------------------------------------------------------------------
// response conversions
// ---------------------------------------------------------------------------

/// Translate an Anthropic Messages response body into an `OpenAI`
/// chat-completions response body.
///
/// # Errors
///
/// Returns [`TranslateError::NotObject`] if the body is not a JSON object.
#[allow(clippy::needless_pass_by_value)]
pub fn anthropic_to_openai_response(body: Value, model: &str) -> Result<Value, TranslateError> {
    let obj = body.as_object().ok_or(TranslateError::NotObject)?;
    let id = obj
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("chatcmpl-")
        .to_string();

    let mut content_parts = Vec::new();
    let mut tool_calls = Vec::new();
    if let Some(content) = obj.get("content").and_then(Value::as_array) {
        for block in content {
            match block.get("type").and_then(Value::as_str) {
                Some("text") => {
                    if let Some(t) = block.get("text").and_then(Value::as_str) {
                        content_parts.push(t.to_string());
                    }
                }
                Some("tool_use") => {
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
                    let input = block.get("input").cloned().unwrap_or(Value::Null);
                    tool_calls.push(json!({
                        "id": id,
                        "type": "function",
                        "function": {"name": name, "arguments": arguments_to_string(input)}
                    }));
                }
                _ => {}
            }
        }
    }

    let content = if content_parts.is_empty() {
        Value::Null
    } else {
        Value::String(content_parts.join(""))
    };
    let mut message = json!({"role": "assistant"});
    if !content.is_null() {
        message["content"] = content;
    }
    if !tool_calls.is_empty() {
        message["tool_calls"] = Value::Array(tool_calls);
    }

    let stop_reason = obj
        .get("stop_reason")
        .and_then(Value::as_str)
        .unwrap_or("end_turn");
    let finish_reason = match stop_reason {
        "tool_use" => "tool_calls",
        "max_tokens" => "length",
        _ => "stop",
    };
    let usage = openai_usage(&parse_usage(obj.get("usage").unwrap_or(&Value::Null)));

    Ok(json!({
        "id": id,
        "object": "chat.completion",
        "created": now_ts(),
        "model": model,
        "choices": [{
            "index": 0,
            "message": message,
            "finish_reason": finish_reason,
            "logprobs": null
        }],
        "usage": usage,
        "system_fingerprint": null
    }))
}

/// Translate an `OpenAI` chat-completions response body into an Anthropic
/// Messages response body.
///
/// # Errors
///
/// Returns [`TranslateError::NotObject`] if the body is not a JSON object.
#[allow(clippy::needless_pass_by_value)]
pub fn openai_to_anthropic_response(body: Value, model: &str) -> Result<Value, TranslateError> {
    let obj = body.as_object().ok_or(TranslateError::NotObject)?;
    let id = obj
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("msg_")
        .to_string();

    let mut content: Vec<Value> = Vec::new();
    let mut stop_reason = "end_turn";
    if let Some(choices) = obj.get("choices").and_then(Value::as_array) {
        if let Some(choice) = choices.first() {
            if let Some(msg) = choice.get("message") {
                let c = msg.get("content").cloned().unwrap_or(Value::Null);
                match c {
                    Value::String(s) => {
                        if !s.is_empty() {
                            content.push(json!({"type": "text", "text": s}));
                        }
                    }
                    Value::Array(arr) => {
                        for b in arr {
                            if b.get("type").and_then(Value::as_str) == Some("text") {
                                content.push(b);
                            }
                        }
                    }
                    _ => {}
                }
                if let Some(tool_calls) = msg.get("tool_calls").and_then(Value::as_array) {
                    for tc in tool_calls {
                        let id = tc
                            .get("id")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        let name = tc
                            .get("function")
                            .and_then(|f| f.get("name"))
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        let args = tc
                            .get("function")
                            .and_then(|f| f.get("arguments"))
                            .cloned()
                            .unwrap_or(Value::Null);
                        content.push(json!({"type": "tool_use", "id": id, "name": name, "input": parse_arguments(args)}));
                    }
                }
            }
            let finish = choice
                .get("finish_reason")
                .and_then(Value::as_str)
                .unwrap_or("stop");
            stop_reason = match finish {
                "tool_calls" => "tool_use",
                "length" => "max_tokens",
                _ => "end_turn",
            };
        }
    }

    let usage = anthropic_usage(&parse_usage(obj.get("usage").unwrap_or(&Value::Null)));

    Ok(json!({
        "id": id,
        "type": "message",
        "role": "assistant",
        "model": model,
        "content": content,
        "stop_reason": stop_reason,
        "stop_sequence": null,
        "usage": usage
    }))
}

/// Translate an Anthropic Messages response body into a Responses API
/// response body.
///
/// # Errors
///
/// Returns [`TranslateError::NotObject`] if the body is not a JSON object.
#[allow(clippy::needless_pass_by_value)]
pub fn anthropic_to_responses_response(body: Value, model: &str) -> Result<Value, TranslateError> {
    let obj = body.as_object().ok_or(TranslateError::NotObject)?;
    let id = obj
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("resp_")
        .to_string();

    let mut output: Vec<Value> = Vec::new();
    if let Some(content) = obj.get("content").and_then(Value::as_array) {
        let mut text_items = Vec::new();
        for block in content {
            match block.get("type").and_then(Value::as_str) {
                Some("text") => {
                    if let Some(t) = block.get("text").and_then(Value::as_str) {
                        text_items
                            .push(json!({"type": "output_text", "text": t, "annotations": []}));
                    }
                }
                Some("tool_use") => {
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
                    let input = block.get("input").cloned().unwrap_or(Value::Null);
                    output.push(json!({
                        "type": "function_call",
                        "id": id,
                        "call_id": id,
                        "name": name,
                        "arguments": arguments_to_string(input),
                        "status": "completed"
                    }));
                }
                _ => {}
            }
        }
        if !text_items.is_empty() {
            output.push(json!({"type": "message", "role": "assistant", "content": text_items}));
        }
    }

    let usage = responses_usage(&parse_usage(obj.get("usage").unwrap_or(&Value::Null)));

    Ok(json!({
        "id": id,
        "object": "response",
        "created_at": now_ts(),
        "status": "completed",
        "model": model,
        "output": output,
        "parallel_tool_calls": true,
        "usage": usage
    }))
}

/// Translate an `OpenAI` chat-completions response body into a Responses API
/// response body.
///
/// # Errors
///
/// Returns [`TranslateError::NotObject`] if the body is not a JSON object.
#[allow(clippy::needless_pass_by_value)]
pub fn openai_to_responses_response(body: Value, model: &str) -> Result<Value, TranslateError> {
    let obj = body.as_object().ok_or(TranslateError::NotObject)?;
    let id = obj
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("resp_")
        .to_string();
    let created_at = obj
        .get("created")
        .and_then(Value::as_u64)
        .unwrap_or_else(now_ts);

    let mut output: Vec<Value> = Vec::new();
    if let Some(choices) = obj.get("choices").and_then(Value::as_array) {
        if let Some(choice) = choices.first() {
            if let Some(msg) = choice.get("message") {
                let c = msg.get("content").cloned().unwrap_or(Value::Null);
                let mut text_items = Vec::new();
                match c {
                    Value::String(s) => {
                        if !s.is_empty() {
                            text_items
                                .push(json!({"type": "output_text", "text": s, "annotations": []}));
                        }
                    }
                    Value::Array(arr) => {
                        for b in arr {
                            if b.get("type").and_then(Value::as_str) == Some("text") {
                                if let Some(t) = b.get("text").and_then(Value::as_str) {
                                    text_items.push(json!({"type": "output_text", "text": t, "annotations": []}));
                                }
                            }
                        }
                    }
                    _ => {}
                }
                if !text_items.is_empty() {
                    output.push(
                        json!({"type": "message", "role": "assistant", "content": text_items}),
                    );
                }
                if let Some(tool_calls) = msg.get("tool_calls").and_then(Value::as_array) {
                    for tc in tool_calls {
                        let id = tc
                            .get("id")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        let name = tc
                            .get("function")
                            .and_then(|f| f.get("name"))
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        let args = tc
                            .get("function")
                            .and_then(|f| f.get("arguments"))
                            .cloned()
                            .unwrap_or(Value::Null);
                        output.push(json!({
                            "type": "function_call",
                            "id": id,
                            "call_id": id,
                            "name": name,
                            "arguments": arguments_to_string(args),
                            "status": "completed"
                        }));
                    }
                }
            }
        }
    }

    let usage = responses_usage(&parse_usage(obj.get("usage").unwrap_or(&Value::Null)));

    Ok(json!({
        "id": id,
        "object": "response",
        "created_at": created_at,
        "status": "completed",
        "model": model,
        "output": output,
        "parallel_tool_calls": true,
        "usage": usage
    }))
}

// ---------------------------------------------------------------------------
// token usage
// ---------------------------------------------------------------------------

/// Aggregate token counts for a single request/response exchange.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenUsage {
    /// Number of input (prompt) tokens.
    pub input: u64,
    /// Number of output (completion) tokens.
    pub output: u64,
    /// Number of reasoning tokens, when reported by the upstream.
    pub reasoning: u64,
}

/// Tolerantly parse usage from any upstream shape (Anthropic, `OpenAI`, Responses).
#[must_use]
pub fn parse_usage(value: &Value) -> TokenUsage {
    let mut usage = TokenUsage::default();
    if let Some(obj) = value.as_object() {
        usage.input = num(obj.get("input_tokens"))
            .or_else(|| num(obj.get("prompt_tokens")))
            .unwrap_or(0);
        usage.output = num(obj.get("output_tokens"))
            .or_else(|| num(obj.get("completion_tokens")))
            .unwrap_or(0);
        if let Some(details) = obj.get("completion_tokens_details") {
            usage.reasoning = num(details.get("reasoning_tokens")).unwrap_or(0);
        }
        if let Some(details) = obj.get("output_tokens_details") {
            usage.reasoning = num(details.get("reasoning_tokens")).unwrap_or(usage.reasoning);
        }
    }
    usage
}

fn num(v: Option<&Value>) -> Option<u64> {
    v.and_then(Value::as_u64)
}

/// Serialize [`TokenUsage`] into an Anthropic-style usage object.
#[must_use]
pub fn anthropic_usage(u: &TokenUsage) -> Value {
    json!({"input_tokens": u.input, "output_tokens": u.output})
}

/// Serialize [`TokenUsage`] into an `OpenAI`-style usage object.
#[must_use]
pub fn openai_usage(u: &TokenUsage) -> Value {
    let mut details = json!({});
    if u.reasoning > 0 {
        insert(&mut details, "reasoning_tokens", json!(u.reasoning));
    }
    json!({
        "prompt_tokens": u.input,
        "completion_tokens": u.output,
        "total_tokens": u.input + u.output,
        "completion_tokens_details": details
    })
}

/// Serialize [`TokenUsage`] into a Responses-API-style usage object.
#[must_use]
pub fn responses_usage(u: &TokenUsage) -> Value {
    let mut out_details = json!({});
    if u.reasoning > 0 {
        insert(&mut out_details, "reasoning_tokens", json!(u.reasoning));
    }
    json!({
        "input_tokens": u.input,
        "output_tokens": u.output,
        "total_tokens": u.input + u.output,
        "input_tokens_details": {},
        "output_tokens_details": out_details
    })
}

// ---------------------------------------------------------------------------
// anthropic -> anthropic normalization (passthrough hygiene)
// ---------------------------------------------------------------------------

/// Strip fields that strict Anthropic upstreams reject: thinking/reasoning
/// effort knobs and `cache_control` markers.
#[must_use]
pub fn normalize_anthropic_request(body: &Value) -> Value {
    let mut out = strip_cache_control(body.clone());
    for key in [
        "thinking",
        "reasoning",
        "reasoning_effort",
        "effort",
        "level",
        "depth",
        "output_config",
    ] {
        remove(&mut out, key);
    }
    out
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

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
                    {"type": "tool_result", "tool_call_id": "call_1", "content": "sunny"}
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

        let body = json!({"tool_choice": {"type": "any"}});
        let out = anthropic_to_openai_request(body).unwrap();
        assert_eq!(out["tool_choice"], "auto");
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
    fn parse_usage_tolerant_shapes() {
        let a = json!({"input_tokens": 1, "output_tokens": 2});
        assert_eq!(
            parse_usage(&a),
            TokenUsage {
                input: 1,
                output: 2,
                reasoning: 0
            }
        );

        let o = json!({"prompt_tokens": 3, "completion_tokens": 4, "total_tokens": 7, "completion_tokens_details": {"reasoning_tokens": 1}});
        let u = parse_usage(&o);
        assert_eq!(u.input, 3);
        assert_eq!(u.output, 4);
        assert_eq!(u.reasoning, 1);

        let r = json!({"input_tokens": 9, "output_tokens": 8, "total_tokens": 17, "output_tokens_details": {"reasoning_tokens": 5}});
        let u = parse_usage(&r);
        assert_eq!(u.input, 9);
        assert_eq!(u.output, 8);
        assert_eq!(u.reasoning, 5);

        assert_eq!(parse_usage(&json!({})), TokenUsage::default());
    }

    #[test]
    fn normalize_anthropic_request_strips_knobs() {
        let body = json!({
            "model": "m",
            "thinking": {"type": "enabled"},
            "effort": "high",
            "messages": [{"role": "user", "content": [{"type": "text", "text": "x", "cache_control": {"type": "ephemeral"}}]}]
        });
        let out = normalize_anthropic_request(&body);
        assert!(out.get("thinking").is_none());
        assert!(out.get("effort").is_none());
        let text = serde_json::to_string(&out).unwrap();
        assert!(!text.contains("cache_control"));
        assert_eq!(out["messages"][0]["content"][0]["text"], "x");
    }

    #[test]
    fn unknown_extra_fields_preserved() {
        let body = json!({
            "model": "m",
            "metadata": {"user_id": "42"},
            "messages": [{"role": "user", "content": "hi"}]
        });
        let out = anthropic_to_openai_request(body).unwrap();
        assert_eq!(out["metadata"]["user_id"], "42");
    }
}
