use super::sse::{json_event, next_event, RESPONSE_LIMIT};
use agent_core::types::{
    AssistantMessage, CancellationToken, Content, Context, Message, StopReason, ToolCall, Usage,
};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::io::BufRead;

fn content_blocks(content: &[Content]) -> Vec<Value> {
    content.iter().filter_map(|block| match block {
        Content::Text { text } => (!text.is_empty()).then(|| json!({"type":"text","text":text})),
        Content::Thinking { thinking } => (!thinking.is_empty()).then(|| json!({"type":"text","text":thinking})),
        Content::Image { data, mime_type } => Some(json!({"type":"image","source":{"type":"base64","media_type":mime_type,"data":data}})),
        Content::ToolCall(_) => None,
    }).collect()
}

fn push_message(messages: &mut Vec<Value>, role: &str, content: Vec<Value>) {
    if content.is_empty() {
        return;
    }
    if let Some(last) = messages.last_mut().filter(|last| last["role"] == role) {
        last["content"]
            .as_array_mut()
            .expect("Owned message content is an array")
            .extend(content);
    } else {
        messages.push(json!({"role":role,"content":content}));
    }
}

pub fn anthropic_request_payload(context: &Context, model: &str, tools: &[Value]) -> Value {
    let mut messages = Vec::new();
    for message in &context.messages {
        match message {
            Message::User(user) => {
                push_message(&mut messages, "user", content_blocks(&user.content))
            }
            Message::Assistant(assistant) => {
                let metadata = assistant.protocol_metadata.as_ref().filter(|metadata| {
                    metadata["protocol"] == "anthropic" && metadata["model"] == model
                });
                let content = assistant.content.iter().enumerate().filter_map(|(index, block)| {
                    let signature = metadata.and_then(|metadata| metadata["blocks"].get(index.to_string()));
                    match block {
                        Content::Thinking { thinking } => {
                            if let Some(signature) = signature {
                                if signature["kind"] == "redacted" {
                                    return Some(json!({"type":"redacted_thinking","data":signature["signature"]}));
                                }
                                if let Some(signature) = signature["signature"].as_str().filter(|signature| !signature.is_empty()) {
                                    return Some(json!({"type":"thinking","thinking":thinking,"signature":signature}));
                                }
                            }
                            (!thinking.is_empty()).then(|| json!({"type":"text","text":thinking}))
                        }
                        Content::ToolCall(call) => Some(json!({"type":"tool_use","id":call.id,"name":call.name,"input":call.arguments})),
                        _ => content_blocks(std::slice::from_ref(block)).into_iter().next(),
                    }
                }).collect();
                push_message(&mut messages, "assistant", content);
            }
            Message::ToolResult(result) => push_message(
                &mut messages,
                "user",
                vec![
                    json!({"type":"tool_result","tool_use_id":result.tool_call_id,"content":content_blocks(&result.content),"is_error":result.is_error}),
                ],
            ),
            Message::Custom { .. } => {}
        }
    }
    let mut payload = json!({"model":model,"messages":messages,"max_tokens":8192,"stream":true});
    if !context.system_prompt.is_empty() {
        payload["system"] = json!(context.system_prompt);
    }
    if !tools.is_empty() {
        payload["tools"] = json!(tools.iter().map(|tool| {
            let function = tool.get("function").unwrap_or(tool);
            json!({"name":function["name"],"description":function["description"],"input_schema":function.get("parameters").cloned().unwrap_or_else(|| json!({"type":"object"}))})
        }).collect::<Vec<_>>());
    }
    payload
}

struct Block {
    position: usize,
    arguments: String,
    closed: bool,
}

fn update_usage(message: &mut AssistantMessage, incoming: &Value) {
    if !incoming.is_object() {
        return;
    }
    let usage = message.usage.get_or_insert_with(Usage::default);
    for (key, field) in [
        ("input_tokens", &mut usage.input),
        ("output_tokens", &mut usage.output),
        ("cache_read_input_tokens", &mut usage.cache_read),
        ("cache_creation_input_tokens", &mut usage.cache_write),
    ] {
        if let Some(value) = incoming[key].as_u64() {
            *field = value;
        }
    }
    usage.total_tokens = usage
        .input
        .saturating_add(usage.output)
        .saturating_add(usage.cache_read)
        .saturating_add(usage.cache_write);
}

pub fn parse_anthropic_sse(
    mut reader: impl BufRead,
    signal: &CancellationToken,
    on_update: &mut dyn FnMut(AssistantMessage),
) -> Result<AssistantMessage, String> {
    let mut message = AssistantMessage::text("");
    message.content.clear();
    message.protocol_metadata = Some(json!({"protocol":"anthropic","blocks":{}}));
    let mut blocks: BTreeMap<u64, Block> = BTreeMap::new();
    let mut started = false;
    let mut finished = false;
    let mut reason = None;
    let mut response_bytes = 0;
    while let Some(data) = next_event(&mut reader, signal)? {
        response_bytes += data.len();
        if response_bytes > RESPONSE_LIMIT {
            return Err("Model response exceeds limit".into());
        }
        let event = json_event(&data)?;
        let kind = event["type"]
            .as_str()
            .ok_or("Anthropic event type missing")?;
        if kind == "error" {
            return Err("Anthropic service reported a stream error".into());
        }
        if kind == "ping" {
            continue;
        }
        if !started && kind != "message_start" {
            return Err("Anthropic stream missing message_start".into());
        }
        match kind {
            "message_start" => {
                if started {
                    return Err("Duplicate Anthropic message_start".into());
                }
                started = true;
                update_usage(&mut message, &event["message"]["usage"]);
            }
            "content_block_start" => {
                let index = event["index"]
                    .as_u64()
                    .ok_or("Content block index missing")?;
                if blocks.contains_key(&index) {
                    return Err("Duplicate content block index".into());
                }
                let incoming = &event["content_block"];
                let content = match incoming["type"].as_str() {
                    Some("text") => Content::text(incoming["text"].as_str().unwrap_or("")),
                    Some("thinking") => Content::Thinking {
                        thinking: incoming["thinking"].as_str().unwrap_or("").to_owned(),
                    },
                    Some("redacted_thinking") => Content::Thinking {
                        thinking: "[Reasoning redacted]".to_owned(),
                    },
                    Some("tool_use") => {
                        let id = incoming["id"]
                            .as_str()
                            .filter(|id| !id.is_empty())
                            .ok_or("Tool call ID missing")?;
                        let name = incoming["name"]
                            .as_str()
                            .filter(|name| !name.is_empty())
                            .ok_or("Tool call name missing")?;
                        Content::ToolCall(ToolCall {
                            id: id.to_owned(),
                            name: name.to_owned(),
                            arguments: incoming.get("input").cloned().unwrap_or_else(|| json!({})),
                        })
                    }
                    _ => return Err("Unsupported Anthropic content block".into()),
                };
                let position = message.content.len();
                if incoming["type"] == "thinking" || incoming["type"] == "redacted_thinking" {
                    let signature = if incoming["type"] == "redacted_thinking" {
                        &incoming["data"]
                    } else {
                        &incoming["signature"]
                    };
                    message
                        .protocol_metadata
                        .as_mut()
                        .expect("Metadata initialized")["blocks"][position.to_string()] = json!({"kind":if incoming["type"] == "redacted_thinking" { "redacted" } else { "thinking" },"signature":signature.as_str().unwrap_or("")});
                }
                message.content.push(content);
                blocks.insert(
                    index,
                    Block {
                        position,
                        arguments: String::new(),
                        closed: false,
                    },
                );
                on_update(message.clone());
            }
            "content_block_delta" => {
                let block = blocks
                    .get_mut(
                        &event["index"]
                            .as_u64()
                            .ok_or("Content block index missing")?,
                    )
                    .ok_or("Unknown content block delta")?;
                if block.closed {
                    return Err("Delta for closed content block".into());
                }
                let delta = &event["delta"];
                match (delta["type"].as_str(), &mut message.content[block.position]) {
                    (Some("text_delta"), Content::Text { text }) => {
                        text.push_str(delta["text"].as_str().ok_or("Text delta missing")?)
                    }
                    (Some("thinking_delta"), Content::Thinking { thinking }) => thinking
                        .push_str(delta["thinking"].as_str().ok_or("Thinking delta missing")?),
                    (Some("input_json_delta"), Content::ToolCall(_)) => block.arguments.push_str(
                        delta["partial_json"]
                            .as_str()
                            .ok_or("Tool arguments delta missing")?,
                    ),
                    (Some("signature_delta"), Content::Thinking { .. }) => {
                        let metadata = message
                            .protocol_metadata
                            .as_mut()
                            .expect("Metadata initialized");
                        let signature = metadata["blocks"][block.position.to_string()]["signature"]
                            .as_str()
                            .unwrap_or("")
                            .to_owned()
                            + delta["signature"]
                                .as_str()
                                .ok_or("Thinking signature missing")?;
                        metadata["blocks"][block.position.to_string()]["signature"] =
                            json!(signature);
                    }
                    _ => return Err("Mismatched Anthropic content delta".into()),
                }
                on_update(message.clone());
            }
            "content_block_stop" => {
                let block = blocks
                    .get_mut(
                        &event["index"]
                            .as_u64()
                            .ok_or("Content block index missing")?,
                    )
                    .ok_or("Unknown content block stop")?;
                if block.closed {
                    return Err("Duplicate content block stop".into());
                }
                block.closed = true;
            }
            "message_delta" => {
                if let Some(value) = event["delta"]["stop_reason"].as_str() {
                    if reason.as_deref().is_some_and(|reason| reason != value) {
                        return Err("Conflicting Anthropic stop reason".into());
                    }
                    reason = Some(value.to_owned());
                }
                update_usage(&mut message, &event["usage"]);
                on_update(message.clone());
            }
            "message_stop" => {
                finished = true;
                break;
            }
            _ => return Err("Unsupported Anthropic stream event".into()),
        }
    }
    if signal.is_cancelled() {
        return Err("Operation aborted".into());
    }
    if !started || !finished || blocks.values().any(|block| !block.closed) {
        return Err("Anthropic stream ended before message_stop or block completion".into());
    }
    message.stop_reason = match reason.as_deref() {
        Some("end_turn" | "stop_sequence" | "pause_turn") => StopReason::Stop,
        Some("tool_use") => StopReason::ToolUse,
        Some("max_tokens") => StopReason::Length,
        Some(_) => return Err("Anthropic provider returned an unsupported stop reason".into()),
        None => return Err("Anthropic stream ended without a stop reason".into()),
    };
    let mut ids = BTreeSet::new();
    for block in blocks.values() {
        if let Content::ToolCall(call) = &mut message.content[block.position] {
            if !ids.insert(call.id.clone()) {
                return Err("Duplicate tool call ID".into());
            }
            if message.stop_reason == StopReason::Length {
                call.arguments = Value::Null;
            } else {
                if !block.arguments.is_empty() {
                    call.arguments = serde_json::from_str(&block.arguments)
                        .map_err(|_| "Invalid Anthropic tool arguments")?;
                }
                if !call.arguments.is_object() {
                    return Err("Tool arguments must be an object".into());
                }
            }
        }
    }
    if message.stop_reason == StopReason::ToolUse && ids.is_empty() {
        return Err("Tool-use stop has no tool calls".into());
    }
    Ok(message)
}
