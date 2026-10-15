//! OpenAI Responses wire protocol, based on the installed SDK event types.
use super::sse::{json_event, next_event, RESPONSE_LIMIT};
use agent_core::types::{
    AssistantMessage, CancellationToken, Content, Context, Message, StopReason, ToolCall, Usage,
};
use serde_json::{json, Value};
use std::collections::{btree_map::Entry, BTreeMap, BTreeSet};
use std::io::BufRead;

fn input_content(content: &[Content]) -> Vec<Value> {
    content.iter().filter_map(|block| match block {
        Content::Text { text } => Some(json!({"type":"input_text","text":text})),
        Content::Image { data, mime_type } => Some(json!({"type":"input_image","detail":"auto","image_url":format!("data:{mime_type};base64,{data}")})),
        _ => None,
    }).collect()
}

pub fn responses_request_payload(context: &Context, model: &str, tools: &[Value]) -> Value {
    responses_request_payload_for_protocol(context, model, tools, "openai-responses")
}

pub(super) fn responses_request_payload_for_protocol(
    context: &Context,
    model: &str,
    tools: &[Value],
    protocol: &str,
) -> Value {
    let mut input = Vec::new();
    for message in &context.messages {
        match message {
            Message::User(user) => {
                let content = input_content(&user.content);
                if !content.is_empty() {
                    input.push(json!({"role":"user","content":content}));
                }
            }
            Message::Assistant(assistant) => {
                let metadata = assistant.protocol_metadata.as_ref().filter(|metadata| {
                    metadata["protocol"] == protocol && metadata["model"] == model
                });
                for (index, block) in assistant.content.iter().enumerate() {
                    let item =
                        metadata.and_then(|metadata| metadata["items"].get(index.to_string()));
                    match block {
                        Content::Thinking { thinking } => {
                            if let Some(item) = item.filter(|item| item["type"] == "reasoning") {
                                input.push(item.clone());
                            } else if !thinking.is_empty() {
                                input.push(json!({"role":"assistant","content":thinking}));
                            }
                        }
                        Content::Text { text } => {
                            if let Some(item) = item.filter(|item| item["type"] == "message") {
                                input.push(item.clone());
                            } else if !text.is_empty() {
                                input.push(json!({"role":"assistant","content":text}));
                            }
                        }
                        Content::ToolCall(call) => {
                            let call_id = call.id.split('|').next().unwrap_or(&call.id);
                            let mut function = json!({"type":"function_call","call_id":call_id,"name":call.name,"arguments":call.arguments.to_string()});
                            if let Some(item) = item.filter(|item| item["type"] == "function_call")
                            {
                                if item["id"].as_str().is_some_and(|id| id.starts_with("fc_")) {
                                    function["id"] = item["id"].clone();
                                }
                                if let Some(namespace) = item.get("namespace") {
                                    function["namespace"] = namespace.clone();
                                }
                            }
                            input.push(function);
                        }
                        _ => {}
                    }
                }
            }
            Message::ToolResult(result) => {
                let output = if result
                    .content
                    .iter()
                    .any(|block| matches!(block, Content::Image { .. }))
                {
                    json!(input_content(&result.content))
                } else {
                    let text = message.text();
                    json!(if text.is_empty() {
                        "(no tool output)".to_owned()
                    } else {
                        text
                    })
                };
                input.push(json!({"type":"function_call_output","call_id":result.tool_call_id.split('|').next().unwrap_or(&result.tool_call_id),"output":output}));
            }
            Message::Custom { .. } => {}
        }
    }
    let mut payload = json!({"model":model,"input":input,"stream":true,"store":false,"include":["reasoning.encrypted_content"]});
    if !context.system_prompt.is_empty() {
        payload["instructions"] = json!(context.system_prompt);
    }
    if !tools.is_empty() {
        payload["tools"] = json!(tools.iter().map(|tool| {
            let function = tool.get("function").unwrap_or(tool);
            json!({"type":"function","name":function["name"],"description":function["description"],"parameters":function["parameters"],"strict":false})
        }).collect::<Vec<_>>());
    }
    payload
}

struct Slot {
    id: String,
    kind: String,
    position: usize,
    arguments: String,
    parts: BTreeMap<u64, String>,
    closed: bool,
}

fn nonempty<'a>(value: &'a Value, field: &str) -> Result<&'a str, String> {
    value[field]
        .as_str()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("Responses {field} missing"))
}

fn create_slot(message: &mut AssistantMessage, item: &Value) -> Result<Slot, String> {
    let id = nonempty(item, "id")?.to_owned();
    let kind = nonempty(item, "type")?.to_owned();
    let content = match kind.as_str() {
        "message" => Content::text(""),
        "reasoning" => Content::Thinking {
            thinking: String::new(),
        },
        "function_call" => Content::ToolCall(ToolCall {
            id: format!("{}|{id}", nonempty(item, "call_id")?),
            name: nonempty(item, "name")?.to_owned(),
            arguments: json!({}),
        }),
        _ => return Err("Responses output item type has not been migrated".into()),
    };
    let position = message.content.len();
    message.content.push(content);
    Ok(Slot {
        id,
        kind,
        position,
        arguments: item["arguments"].as_str().unwrap_or("").to_owned(),
        parts: BTreeMap::new(),
        closed: false,
    })
}

fn verify_item(slot: &Slot, item: &Value, content: &Content) -> Result<(), String> {
    if nonempty(item, "id")? != slot.id || nonempty(item, "type")? != slot.kind {
        return Err("Conflicting Responses output identity".into());
    }
    if let Content::ToolCall(call) = content {
        if nonempty(item, "name")? != call.name
            || format!("{}|{}", nonempty(item, "call_id")?, slot.id) != call.id
        {
            return Err("Conflicting Responses tool call identity".into());
        }
    }
    Ok(())
}

fn finalize_item(
    message: &mut AssistantMessage,
    slot: &mut Slot,
    item: &Value,
) -> Result<(), String> {
    verify_item(slot, item, &message.content[slot.position])?;
    match &mut message.content[slot.position] {
        Content::Text { text } => {
            let parts = item["content"]
                .as_array()
                .ok_or("Responses message content missing")?;
            *text = parts
                .iter()
                .map(|part| match part["type"].as_str() {
                    Some("output_text") => part["text"]
                        .as_str()
                        .ok_or_else(|| "Responses output text missing".to_owned()),
                    Some("refusal") => part["refusal"]
                        .as_str()
                        .ok_or_else(|| "Responses refusal text missing".to_owned()),
                    _ => Err("Unsupported Responses message content".into()),
                })
                .collect::<Result<Vec<_>, _>>()?
                .join("");
        }
        Content::Thinking { thinking } => {
            let summary = item["summary"]
                .as_array()
                .map(|parts| {
                    parts
                        .iter()
                        .filter_map(|part| part["text"].as_str())
                        .collect::<Vec<_>>()
                        .join("\n\n")
                })
                .unwrap_or_default();
            let content = item["content"]
                .as_array()
                .map(|parts| {
                    parts
                        .iter()
                        .filter_map(|part| part["text"].as_str())
                        .collect::<Vec<_>>()
                        .join("\n\n")
                })
                .unwrap_or_default();
            if !summary.is_empty() {
                *thinking = summary;
            } else if !content.is_empty() {
                *thinking = content;
            }
        }
        Content::ToolCall(_) => {
            slot.arguments = item["arguments"]
                .as_str()
                .ok_or("Responses tool arguments missing")?
                .to_owned();
        }
        _ => return Err("Unsupported Responses slot".into()),
    }
    let stored = &mut message
        .protocol_metadata
        .as_mut()
        .expect("Metadata initialized")["items"][slot.position.to_string()];
    if item["type"] == "reasoning"
        && item.get("encrypted_content").is_none()
        && stored.get("encrypted_content").is_some()
    {
        let encrypted = stored["encrypted_content"].clone();
        *stored = item.clone();
        stored["encrypted_content"] = encrypted;
    } else {
        *stored = item.clone();
    }
    slot.closed = true;
    Ok(())
}

pub fn parse_responses_sse(
    reader: impl BufRead,
    signal: &CancellationToken,
    on_update: &mut dyn FnMut(AssistantMessage),
) -> Result<AssistantMessage, String> {
    parse_native_responses(reader, signal, on_update, false)
}

pub fn parse_codex_sse(
    reader: impl BufRead,
    signal: &CancellationToken,
    on_update: &mut dyn FnMut(AssistantMessage),
) -> Result<AssistantMessage, String> {
    parse_native_responses(reader, signal, on_update, true)
}

fn parse_native_responses(
    mut reader: impl BufRead,
    signal: &CancellationToken,
    on_update: &mut dyn FnMut(AssistantMessage),
    codex: bool,
) -> Result<AssistantMessage, String> {
    let mut message = AssistantMessage::text("");
    message.content.clear();
    message.protocol_metadata = Some(json!({"protocol":"openai-responses","items":{}}));
    if codex {
        message
            .protocol_metadata
            .as_mut()
            .expect("Metadata initialized")["protocol"] = json!("openai-codex-responses");
    }
    let mut slots: BTreeMap<u64, Slot> = BTreeMap::new();
    let mut response_bytes = 0;
    let mut terminal = false;
    while let Some(data) = next_event(&mut reader, signal)? {
        if data == "[DONE]" {
            break;
        }
        response_bytes += data.len();
        if response_bytes > RESPONSE_LIMIT {
            return Err("Model response exceeds limit".into());
        }
        let mut event = json_event(&data)?;
        if codex
            && matches!(
                event["type"].as_str(),
                Some("response.done" | "response.completed" | "response.incomplete")
            )
        {
            event["type"] = json!(if event["response"]["status"] == "incomplete" {
                "response.incomplete"
            } else {
                "response.completed"
            });
        }
        let kind = nonempty(&event, "type")?;
        match kind {
            "response.created" | "response.in_progress" | "response.queued" => {
                if let Some(id) = event["response"]["id"].as_str() {
                    message
                        .protocol_metadata
                        .as_mut()
                        .expect("Metadata initialized")["response_id"] = json!(id);
                }
            }
            "response.output_item.added" | "response.output_item.done" => {
                let index = event["output_index"]
                    .as_u64()
                    .ok_or("Responses output index missing")?;
                if kind == "response.output_item.added" && slots.contains_key(&index) {
                    return Err("Duplicate Responses output index".into());
                }
                if let Entry::Vacant(entry) = slots.entry(index) {
                    let slot = create_slot(&mut message, &event["item"])?;
                    entry.insert(slot);
                }
                let slot = slots.get_mut(&index).expect("Slot inserted");
                if kind == "response.output_item.done" {
                    if slot.closed {
                        return Err("Duplicate Responses output item completion".into());
                    }
                    finalize_item(&mut message, slot, &event["item"])?;
                }
                on_update(message.clone());
            }
            "response.output_text.delta"
            | "response.output_text.done"
            | "response.refusal.delta"
            | "response.refusal.done"
            | "response.reasoning_summary_text.delta"
            | "response.reasoning_summary_text.done"
            | "response.reasoning_text.delta"
            | "response.reasoning_text.done"
            | "response.function_call_arguments.delta"
            | "response.function_call_arguments.done" => {
                let index = event["output_index"]
                    .as_u64()
                    .ok_or("Responses output index missing")?;
                let slot = slots
                    .get_mut(&index)
                    .ok_or("Responses delta has no output item")?;
                if slot.closed || nonempty(&event, "item_id")? != slot.id {
                    return Err("Responses delta references a closed or conflicting item".into());
                }
                if kind.starts_with("response.function_call_arguments") {
                    if slot.kind != "function_call" {
                        return Err("Responses argument delta type mismatch".into());
                    }
                    if kind.ends_with(".done") {
                        slot.arguments = event["arguments"]
                            .as_str()
                            .ok_or("Responses tool arguments missing")?
                            .to_owned();
                    } else {
                        slot.arguments.push_str(
                            event["delta"]
                                .as_str()
                                .ok_or("Responses argument delta missing")?,
                        );
                    }
                } else {
                    let thinking = kind.starts_with("response.reasoning");
                    if (thinking && slot.kind != "reasoning")
                        || (!thinking && slot.kind != "message")
                    {
                        return Err("Responses text delta type mismatch".into());
                    }
                    let part_index = event[if thinking && kind.contains("summary") {
                        "summary_index"
                    } else {
                        "content_index"
                    }]
                    .as_u64()
                    .unwrap_or(0);
                    let part = slot.parts.entry(part_index).or_default();
                    if kind.ends_with(".done") {
                        *part = event[if kind.contains("refusal") {
                            "refusal"
                        } else {
                            "text"
                        }]
                        .as_str()
                        .ok_or("Responses final text missing")?
                        .to_owned();
                    } else {
                        part.push_str(
                            event["delta"]
                                .as_str()
                                .ok_or("Responses text delta missing")?,
                        );
                    }
                    let joined = slot
                        .parts
                        .values()
                        .map(String::as_str)
                        .collect::<Vec<_>>()
                        .join(if thinking { "\n\n" } else { "" });
                    match &mut message.content[slot.position] {
                        Content::Text { text } => *text = joined,
                        Content::Thinking { thinking } => *thinking = joined,
                        _ => return Err("Responses text content type mismatch".into()),
                    }
                }
                on_update(message.clone());
            }
            "response.completed" | "response.incomplete" => {
                let response = &event["response"];
                if codex {
                    if let Some(end_turn) = response["end_turn"].as_bool() {
                        message
                            .protocol_metadata
                            .as_mut()
                            .expect("Metadata initialized")["end_turn"] = json!(end_turn);
                    }
                }
                message.stop_reason = match (kind, response["status"].as_str()) {
                    ("response.completed", Some("completed")) => StopReason::Stop,
                    ("response.incomplete", Some("incomplete"))
                        if response["incomplete_details"]["reason"] == "max_output_tokens" =>
                    {
                        StopReason::Length
                    }
                    _ => {
                        return Err(
                            "Responses ended with an unsuccessful or conflicting status".into()
                        )
                    }
                };
                if let Some(output) = response["output"].as_array() {
                    for (index, item) in output.iter().enumerate() {
                        let index = index as u64;
                        if let Entry::Vacant(entry) = slots.entry(index) {
                            let slot = create_slot(&mut message, item)?;
                            entry.insert(slot);
                        }
                        finalize_item(
                            &mut message,
                            slots.get_mut(&index).expect("Slot inserted"),
                            item,
                        )?;
                    }
                }
                if let Some(id) = response["id"].as_str() {
                    message
                        .protocol_metadata
                        .as_mut()
                        .expect("Metadata initialized")["response_id"] = json!(id);
                }
                if let Some(usage) = response.get("usage").filter(|usage| usage.is_object()) {
                    let cache_read = usage["input_tokens_details"]["cached_tokens"]
                        .as_u64()
                        .unwrap_or(0);
                    let cache_write = usage["input_tokens_details"]["cache_write_tokens"]
                        .as_u64()
                        .unwrap_or(0);
                    message.usage = Some(Usage {
                        input: usage["input_tokens"]
                            .as_u64()
                            .unwrap_or(0)
                            .saturating_sub(cache_read)
                            .saturating_sub(cache_write),
                        output: usage["output_tokens"].as_u64().unwrap_or(0),
                        cache_read,
                        cache_write,
                        total_tokens: usage["total_tokens"].as_u64().unwrap_or(0),
                    });
                    message
                        .protocol_metadata
                        .as_mut()
                        .expect("Metadata initialized")["usage_details"] = usage.clone();
                }
                terminal = true;
                break;
            }
            "response.failed" | "error" => {
                return Err("Responses service reported a failed response".into())
            }
            "response.content_part.added"
            | "response.content_part.done"
            | "response.reasoning_summary_part.added"
            | "response.reasoning_summary_part.done" => {}
            _ => return Err("Responses stream event has not been migrated".into()),
        }
    }
    if signal.is_cancelled() {
        return Err("Operation aborted".into());
    }
    if !terminal {
        return Err("Responses stream ended before a terminal response event".into());
    }
    if message.stop_reason != StopReason::Length && slots.values().any(|slot| !slot.closed) {
        return Err("Responses stream ended before output items completed".into());
    }
    let mut ids = BTreeSet::new();
    for slot in slots.values() {
        if let Content::ToolCall(call) = &mut message.content[slot.position] {
            let call_id = call.id.split('|').next().unwrap_or(&call.id);
            if !ids.insert(call_id.to_owned()) {
                return Err("Duplicate Responses tool call ID".into());
            }
            if message.stop_reason == StopReason::Length {
                call.arguments = Value::Null;
            } else {
                call.arguments = serde_json::from_str(&slot.arguments)
                    .map_err(|_| "Invalid Responses tool arguments")?;
                if !call.arguments.is_object() {
                    return Err("Responses tool arguments must be an object".into());
                }
            }
        }
    }
    if message.stop_reason == StopReason::Stop && !ids.is_empty() {
        message.stop_reason = StopReason::ToolUse;
    }
    Ok(message)
}
