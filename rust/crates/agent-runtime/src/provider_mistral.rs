//! The original mistral-conversations adapter uses native /v1/chat/completions,
//! with Mistral thinking chunks, nine-character tool IDs and cache accounting.
use super::sse::{json_event, next_event, RESPONSE_LIMIT};
use agent_core::types::{
    AssistantMessage, CancellationToken, Content, Context, Message, StopReason, ToolCall, Usage,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::io::BufRead;

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct MistralOptions {
    pub temperature: Option<f64>,
    pub max_tokens: Option<u64>,
    pub tool_choice: Option<Value>,
    pub reasoning_effort: Option<String>,
    pub prompt_mode: Option<String>,
    pub session_id: Option<String>,
    pub cache_retention: Option<String>,
}
impl MistralOptions {
    pub fn validate(&self) -> Result<(), String> {
        if self.temperature.is_some_and(|value| !value.is_finite())
            || self.max_tokens == Some(0)
            || self
                .reasoning_effort
                .as_ref()
                .is_some_and(|value| !matches!(value.as_str(), "none" | "high"))
            || self
                .prompt_mode
                .as_ref()
                .is_some_and(|value| value != "reasoning")
            || self
                .cache_retention
                .as_ref()
                .is_some_and(|value| !matches!(value.as_str(), "none" | "short" | "long"))
        {
            return Err("Invalid Mistral generation options".into());
        }
        if let Some(session) = &self.session_id {
            super::validate_header(session)?;
        }
        if let Some(choice) = &self.tool_choice {
            if !(matches!(choice.as_str(), Some("auto" | "none" | "any" | "required"))
                || choice["type"] == "function"
                    && choice["function"]["name"]
                        .as_str()
                        .is_some_and(|name| !name.is_empty()))
            {
                return Err("Invalid Mistral tool choice".into());
            }
        }
        Ok(())
    }
    pub(super) fn cache_key(&self) -> Option<&str> {
        self.session_id
            .as_deref()
            .filter(|id| !id.is_empty() && self.cache_retention.as_deref() != Some("none"))
    }
}
fn base36(mut value: u32) -> String {
    let mut bytes = vec![];
    loop {
        bytes.push(b"0123456789abcdefghijklmnopqrstuvwxyz"[(value % 36) as usize]);
        value /= 36;
        if value == 0 {
            break;
        }
    }
    bytes.reverse();
    String::from_utf8(bytes).expect("ASCII")
}
fn short_hash(value: &str) -> String {
    let (mut h1, mut h2) = (0xdeadbeefu32, 0x41c6ce57u32);
    for ch in value.encode_utf16() {
        h1 = (h1 ^ u32::from(ch)).wrapping_mul(2654435761);
        h2 = (h2 ^ u32::from(ch)).wrapping_mul(1597334677);
    }
    h1 = (h1 ^ (h1 >> 16)).wrapping_mul(2246822507) ^ (h2 ^ (h2 >> 13)).wrapping_mul(3266489909);
    h2 = (h2 ^ (h2 >> 16)).wrapping_mul(2246822507) ^ (h1 ^ (h1 >> 13)).wrapping_mul(3266489909);
    format!("{}{}", base36(h2), base36(h1))
}
fn derived_id(source: &str, attempt: usize) -> String {
    let normalized: String = source.chars().filter(char::is_ascii_alphanumeric).collect();
    if attempt == 0 && normalized.len() == 9 {
        return normalized;
    }
    let seed = if normalized.is_empty() {
        source
    } else {
        &normalized
    };
    let seed = if attempt == 0 {
        seed.to_owned()
    } else {
        format!("{seed}:{attempt}")
    };
    short_hash(&seed).chars().take(9).collect()
}
pub fn mistral_request_payload(
    context: &Context,
    model: &str,
    tools: &[Value],
    options: &MistralOptions,
) -> Result<Value, String> {
    options.validate()?;
    let supports_images = crate::catalog::find("mistral", model)
        .is_none_or(|record| record.input.iter().any(|input| input == "image"));
    let mut ids: BTreeMap<String, String> = BTreeMap::new();
    let mut used = BTreeSet::new();
    let mut normalize = |source: &str| {
        if let Some(id) = ids.get(source) {
            return id.clone();
        }
        let mut attempt = 0;
        loop {
            let id = derived_id(source, attempt);
            if used.insert(id.clone()) {
                ids.insert(source.to_owned(), id.clone());
                return id;
            }
            attempt += 1;
        }
    };
    let parts = |content: &[Content]| -> Vec<Value> {
        content.iter().filter_map(|part|match part{Content::Text{text}=>Some(json!({"type":"text","text":text})),Content::Thinking{thinking}=>Some(json!({"type":"thinking","thinking":[{"type":"text","text":thinking}]})),Content::Image{data,mime_type}=>Some(json!({"type":"image_url","image_url":format!("data:{mime_type};base64,{data}")})),_=>None}).collect()
    };
    let mut messages = vec![];
    if !context.system_prompt.is_empty() {
        messages.push(json!({"role":"system","content":context.system_prompt}));
    }
    for message in &context.messages {
        match message {
            Message::User(user) => {
                let content: Vec<_> = parts(&user.content)
                    .into_iter()
                    .filter(|part| supports_images || part["type"] != "image_url")
                    .collect();
                if !content.is_empty() {
                    messages.push(json!({"role":"user","content":content}));
                } else if user
                    .content
                    .iter()
                    .any(|part| matches!(part, Content::Image { .. }))
                {
                    messages.push(json!({"role":"user","content":"(image omitted: model does not support images)"}));
                }
            }
            Message::Assistant(assistant) => {
                let content: Vec<_> = parts(&assistant.content)
                    .into_iter()
                    .filter(|part| match part["type"].as_str() {
                        Some("text") => part["text"]
                            .as_str()
                            .is_some_and(|text| !text.trim().is_empty()),
                        Some("thinking") => part["thinking"][0]["text"]
                            .as_str()
                            .is_some_and(|text| !text.trim().is_empty()),
                        _ => false,
                    })
                    .collect();
                let mut entry = json!({"role":"assistant","prefix":false});
                let calls:Vec<_>=assistant.tool_calls().iter().map(|call|json!({"id":normalize(&call.id),"type":"function","function":{"name":call.name,"arguments":call.arguments.to_string()},"index":0})).collect();
                if content.is_empty() && calls.is_empty() {
                    continue;
                }
                if !content.is_empty() {
                    entry["content"] = json!(content);
                }
                if !calls.is_empty() {
                    entry["tool_calls"] = json!(calls);
                }
                messages.push(entry);
            }
            Message::ToolResult(tool) => {
                let text = message.text();
                let has_images = tool
                    .content
                    .iter()
                    .any(|part| matches!(part, Content::Image { .. }));
                let mut text = if !text.trim().is_empty() {
                    format!(
                        "{}{}",
                        text,
                        if has_images && !supports_images {
                            "\n[tool image omitted: model does not support images]"
                        } else {
                            ""
                        }
                    )
                } else if has_images {
                    if supports_images {
                        "(see attached image)"
                    } else {
                        "(image omitted: model does not support images)"
                    }
                    .into()
                } else {
                    "(no tool output)".into()
                };
                if tool.is_error {
                    text = format!("[tool error] {text}");
                }
                let mut content = vec![json!({"type":"text","text":text})];
                if supports_images {
                    content.extend(
                        parts(&tool.content)
                            .into_iter()
                            .filter(|part| part["type"] == "image_url"),
                    );
                }
                messages.push(json!({"role":"tool","tool_call_id":normalize(&tool.tool_call_id),"name":tool.tool_name,"content":content}));
            }
            Message::Custom { .. } => {}
        }
    }
    let mut payload = json!({"model":model,"stream":true,"messages":messages});
    if !tools.is_empty() {
        payload["tools"]=json!(tools.iter().map(|tool|{let function=tool.get("function").unwrap_or(tool);json!({"type":"function","function":{"name":function["name"],"description":function["description"],"parameters":function["parameters"],"strict":function["strict"].as_bool().unwrap_or(false)}})}).collect::<Vec<_>>());
    }
    for (name, value) in [
        ("temperature", options.temperature.map(|v| json!(v))),
        ("max_tokens", options.max_tokens.map(|v| json!(v))),
        ("tool_choice", options.tool_choice.clone()),
        (
            "prompt_mode",
            options.prompt_mode.as_ref().map(|v| json!(v)),
        ),
        (
            "reasoning_effort",
            options.reasoning_effort.as_ref().map(|v| json!(v)),
        ),
        ("prompt_cache_key", options.cache_key().map(|v| json!(v))),
    ] {
        if let Some(value) = value {
            payload[name] = value;
        }
    }
    Ok(payload)
}
struct CallSlot {
    position: usize,
    arguments: String,
}
pub fn parse_mistral_sse(
    mut reader: impl BufRead,
    signal: &CancellationToken,
    on_update: &mut dyn FnMut(AssistantMessage),
) -> Result<AssistantMessage, String> {
    let mut message = AssistantMessage::text("");
    message.content.clear();
    message.protocol_metadata = Some(json!({"protocol":"mistral-conversations"}));
    let mut calls: BTreeMap<String, CallSlot> = BTreeMap::new();
    let mut reason = None;
    let mut bytes = 0usize;
    while let Some(data) = next_event(&mut reader, signal)? {
        if data == "[DONE]" {
            break;
        }
        bytes = bytes.saturating_add(data.len());
        if bytes > RESPONSE_LIMIT {
            return Err("Mistral response exceeds limit".into());
        }
        let event = json_event(&data)?;
        if event.get("error").is_some() {
            return Err("Mistral reported a stream error".into());
        }
        if let Some(id) = event["id"].as_str() {
            message.protocol_metadata.as_mut().expect("Owned metadata")["response_id"] = json!(id);
        }
        if let Some(usage) = event.get("usage").filter(|usage| usage.is_object()) {
            let input = usage["prompt_tokens"].as_u64().unwrap_or(0);
            let cached = usage["prompt_tokens_details"]["cached_tokens"]
                .as_u64()
                .or_else(|| usage["promptTokensDetails"]["cachedTokens"].as_u64())
                .or_else(|| usage["prompt_token_details"]["cached_tokens"].as_u64())
                .or_else(|| usage["promptTokenDetails"]["cachedTokens"].as_u64())
                .or_else(|| usage["num_cached_tokens"].as_u64())
                .or_else(|| usage["numCachedTokens"].as_u64())
                .unwrap_or(0)
                .min(input);
            let output = usage["completion_tokens"].as_u64().unwrap_or(0);
            message.usage = Some(Usage {
                input: input - cached,
                output,
                cache_read: cached,
                cache_write: 0,
                total_tokens: usage["total_tokens"]
                    .as_u64()
                    .filter(|v| *v > 0)
                    .unwrap_or(input.saturating_add(output)),
            });
        }
        let choices = event["choices"]
            .as_array()
            .ok_or("Invalid Mistral streaming event")?;
        if let Some(choice) = choices.first() {
            if let Some(finish) = choice["finish_reason"].as_str() {
                let mapped = match finish {
                    "stop" => StopReason::Stop,
                    "length" | "model_length" => StopReason::Length,
                    "tool_calls" => StopReason::ToolUse,
                    _ => return Err("Mistral stopped with an error reason".into()),
                };
                if reason.as_ref().is_some_and(|old| old != &mapped) {
                    return Err("Conflicting Mistral finish reasons".into());
                }
                reason = Some(mapped);
            }
            let delta = &choice["delta"];
            let chunks = if let Some(text) = delta["content"].as_str() {
                vec![json!({"type":"text","text":text})]
            } else {
                delta["content"].as_array().cloned().unwrap_or_default()
            };
            for chunk in chunks {
                let (thinking, text) = match chunk["type"].as_str() {
                    Some("text") => (
                        false,
                        chunk["text"]
                            .as_str()
                            .ok_or("Invalid Mistral text chunk")?
                            .to_owned(),
                    ),
                    Some("thinking") => {
                        let parts = chunk["thinking"]
                            .as_array()
                            .ok_or("Invalid Mistral thinking chunk")?;
                        (
                            true,
                            parts
                                .iter()
                                .map(|part| {
                                    part["text"].as_str().ok_or("Invalid Mistral thinking text")
                                })
                                .collect::<Result<Vec<_>, _>>()?
                                .join(""),
                        )
                    }
                    _ => return Err("Unsupported Mistral output chunk".into()),
                };
                if thinking {
                    if let Some(Content::Thinking { thinking }) = message.content.last_mut() {
                        thinking.push_str(&text);
                    } else {
                        message.content.push(Content::Thinking { thinking: text });
                    }
                } else if let Some(Content::Text { text: old }) = message.content.last_mut() {
                    old.push_str(&text);
                } else {
                    message.content.push(Content::text(text));
                }
            }
            for call in delta["tool_calls"].as_array().into_iter().flatten() {
                let index = call["index"].as_u64().unwrap_or(0);
                let key = if let Some(index) = call["index"].as_u64() {
                    format!("index:{index}")
                } else if let Some(id) = call["id"]
                    .as_str()
                    .filter(|id| !id.is_empty() && *id != "null")
                {
                    format!("id:{id}")
                } else if calls.len() == 1 {
                    calls.keys().next().expect("One slot").clone()
                } else {
                    format!("index:{index}")
                };
                if let std::collections::btree_map::Entry::Vacant(entry) = calls.entry(key.clone())
                {
                    let id = call["id"]
                        .as_str()
                        .filter(|id| !id.is_empty() && *id != "null")
                        .map(str::to_owned)
                        .unwrap_or_else(|| derived_id(&format!("toolcall:{index}"), 0));
                    let position = message.content.len();
                    message.content.push(Content::ToolCall(ToolCall {
                        id,
                        name: call["function"]["name"].as_str().unwrap_or("").into(),
                        arguments: json!({}),
                    }));
                    entry.insert(CallSlot {
                        position,
                        arguments: String::new(),
                    });
                }
                let slot = calls.get_mut(&key).expect("Owned slot");
                if let Content::ToolCall(current) = &mut message.content[slot.position] {
                    if let Some(id) = call["id"]
                        .as_str()
                        .filter(|id| !id.is_empty() && *id != "null")
                    {
                        if current.id != id {
                            return Err("Conflicting Mistral tool ID".into());
                        }
                    }
                    if let Some(name) = call["function"]["name"]
                        .as_str()
                        .filter(|name| !name.is_empty())
                    {
                        if current.name.is_empty() {
                            current.name = name.into();
                        } else if current.name != name {
                            return Err("Conflicting Mistral tool name".into());
                        }
                    }
                    if let Some(args) = call["function"].get("arguments") {
                        if let Some(text) = args.as_str() {
                            slot.arguments.push_str(text);
                        } else if args.is_object() && slot.arguments.is_empty() {
                            slot.arguments = args.to_string();
                        } else {
                            return Err("Invalid Mistral tool argument delta".into());
                        }
                    }
                    if let Ok(value) = serde_json::from_str::<Value>(&slot.arguments) {
                        if value.is_object() {
                            current.arguments = value;
                        }
                    }
                }
            }
        }
        on_update(message.clone());
    }
    crate::auth::check_cancelled(signal)?;
    let mut ids = BTreeSet::new();
    for slot in calls.values() {
        if let Content::ToolCall(call) = &mut message.content[slot.position] {
            let value: Value = serde_json::from_str(&slot.arguments)
                .map_err(|_| "Malformed Mistral tool arguments")?;
            if !value.is_object() || call.name.is_empty() || !ids.insert(call.id.clone()) {
                return Err("Invalid Mistral final tool call".into());
            }
            call.arguments = value;
        }
    }
    message.stop_reason = reason.ok_or("Mistral stream ended without a finish reason")?;
    Ok(message)
}
