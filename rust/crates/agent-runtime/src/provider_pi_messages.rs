//! Project-owned pi-messages HTTP/SSE protocol, not an OpenAI compatibility alias.
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
pub struct PiMessagesOptions {
    pub temperature: Option<f64>,
    pub max_tokens: Option<u64>,
    pub tool_choice: Option<Value>,
    pub reasoning: Option<String>,
    pub session_id: Option<String>,
    pub cache_retention: Option<String>,
    pub debug: bool,
}
impl PiMessagesOptions {
    pub fn validate(&self) -> Result<(), String> {
        if self.temperature.is_some_and(|value| !value.is_finite())
            || self.max_tokens == Some(0)
            || self.reasoning.as_ref().is_some_and(|level| {
                !matches!(
                    level.as_str(),
                    "off" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max"
                )
            })
            || self
                .cache_retention
                .as_ref()
                .is_some_and(|value| !matches!(value.as_str(), "none" | "short" | "long"))
        {
            return Err("Invalid pi-messages generation options".into());
        }
        if let Some(choice) = &self.tool_choice {
            if !(matches!(choice.as_str(), Some("auto" | "none" | "required"))
                || choice["type"] == "function"
                    && choice["function"]["name"]
                        .as_str()
                        .is_some_and(|name| !name.is_empty()))
            {
                return Err("Invalid pi-messages tool choice".into());
            }
        }
        Ok(())
    }
}
pub fn pi_messages_request_payload(
    context: &Context,
    model: &str,
    tools: &[Value],
    options: &PiMessagesOptions,
) -> Result<Value, String> {
    options.validate()?;
    let mut messages = vec![];
    for message in &context.messages {
        if matches!(message, Message::Custom { .. }) {
            continue;
        }
        let mut wire = serde_json::to_value(message)
            .map_err(|_| "pi-messages context serialization failed")?;
        wire.as_object_mut()
            .expect("Message object")
            .remove("protocolMetadata");
        if let Message::Assistant(assistant) = message {
            let metadata = assistant.protocol_metadata.as_ref();
            let protocol = metadata
                .and_then(|value| value["protocol"].as_str())
                .unwrap_or("native");
            wire["api"] = json!(match protocol {
                "anthropic" => "anthropic-messages",
                "google" => "google-generative-ai",
                other => other,
            });
            wire["provider"] = metadata
                .and_then(|value| value.get("provider"))
                .cloned()
                .unwrap_or(json!("native"));
            wire["model"] = metadata
                .and_then(|value| value.get("model"))
                .cloned()
                .unwrap_or(json!("unknown"));
            if let Some(usage) = metadata.and_then(|value| value.get("pi_usage")) {
                wire["usage"] = usage.clone();
            }
            if let Some(id) = metadata.and_then(|value| value.get("response_id")) {
                wire["responseId"] = id.clone();
            }
            if let Some(level) = metadata.and_then(|value| value.get("provider_thinking_level")) {
                wire["providerThinkingLevel"] = level.clone();
            }
            if let Some(content) = wire["content"].as_array_mut() {
                for (index, part) in content.iter_mut().enumerate() {
                    if let Some(signature) =
                        metadata.and_then(|value| value["signatures"].get(index.to_string()))
                    {
                        match part["type"].as_str() {
                            Some("text") => part["textSignature"] = signature.clone(),
                            Some("thinking") => part["thinkingSignature"] = signature.clone(),
                            Some("toolCall") => part["thoughtSignature"] = signature.clone(),
                            _ => {}
                        }
                    }
                    if let Some(redacted) =
                        metadata.and_then(|value| value["redacted"].get(index.to_string()))
                    {
                        part["redacted"] = redacted.clone();
                    }
                    if let Some(tool) =
                        metadata.and_then(|value| value["tool_metadata"].get(index.to_string()))
                    {
                        for name in ["thoughtSignature", "namespace"] {
                            if let Some(value) = tool.get(name) {
                                part[name] = value.clone();
                            }
                        }
                    }
                }
            }
        }
        messages.push(wire);
    }
    let mut wire_options = json!({});
    for (name, value) in [
        ("temperature", options.temperature.map(|v| json!(v))),
        ("maxTokens", options.max_tokens.map(|v| json!(v))),
        ("reasoning", options.reasoning.as_ref().map(|v| json!(v))),
        ("sessionId", options.session_id.as_ref().map(|v| json!(v))),
        (
            "cacheRetention",
            options.cache_retention.as_ref().map(|v| json!(v)),
        ),
        ("toolChoice", options.tool_choice.clone()),
    ] {
        if let Some(value) = value {
            wire_options[name] = value;
        }
    }
    let tools:Vec<_>=tools.iter().map(|tool|{let function=tool.get("function").unwrap_or(tool);json!({"name":function["name"],"description":function["description"],"parameters":function["parameters"]})}).collect();
    let mut context = json!({"systemPrompt":context.system_prompt,"messages":messages});
    if !tools.is_empty() {
        context["tools"] = json!(tools);
    }
    Ok(json!({"model":model,"context":context,"options":wire_options}))
}
struct Slot {
    position: usize,
    kind: String,
    arguments: String,
    closed: bool,
}
pub fn parse_pi_messages_sse(
    mut reader: impl BufRead,
    signal: &CancellationToken,
    on_update: &mut dyn FnMut(AssistantMessage),
) -> Result<AssistantMessage, String> {
    let mut message = AssistantMessage::text("");
    message.content.clear();
    message.protocol_metadata =
        Some(json!({"protocol":"pi-messages","signatures":{},"redacted":{},"tool_metadata":{}}));
    let mut slots: BTreeMap<u64, Slot> = BTreeMap::new();
    let mut started = false;
    let mut bytes = 0usize;
    while let Some(data) = next_event(&mut reader, signal)? {
        if data == "[DONE]" {
            break;
        }
        bytes = bytes.saturating_add(data.len());
        if bytes > RESPONSE_LIMIT {
            return Err("pi-messages response exceeds limit".into());
        }
        let event = json_event(&data)?;
        let kind = event["type"]
            .as_str()
            .ok_or("pi-messages event type missing")?;
        if kind == "start" {
            if started {
                return Err("Duplicate pi-messages start".into());
            }
            started = true;
            on_update(message.clone());
            continue;
        }
        if !started && kind != "error" {
            return Err("pi-messages stream missing start".into());
        }
        if matches!(kind, "done" | "error") {
            crate::auth::check_cancelled(signal)?;
            let usage = event
                .get("usage")
                .ok_or("pi-messages terminal usage missing")?;
            for key in ["input", "output", "cacheRead", "cacheWrite", "totalTokens"] {
                if usage[key].as_u64().is_none() {
                    return Err("Invalid pi-messages terminal usage".into());
                }
            }
            message.usage = Some(
                serde_json::from_value::<Usage>(usage.clone())
                    .map_err(|_| "Invalid pi-messages usage")?,
            );
            let metadata = message.protocol_metadata.as_mut().expect("Owned metadata");
            metadata["pi_usage"] = usage.clone();
            for (source, target) in [
                ("responseId", "response_id"),
                ("providerThinkingLevel", "provider_thinking_level"),
                ("rewrite", "rewrite"),
            ] {
                if let Some(value) = event.get(source) {
                    metadata[target] = value.clone();
                }
            }
            message.stop_reason = match (kind, event["reason"].as_str()) {
                ("done", Some("stop")) => StopReason::Stop,
                ("done", Some("length")) => StopReason::Length,
                ("done", Some("toolUse")) => StopReason::ToolUse,
                ("error", Some("aborted")) => StopReason::Aborted,
                ("error", Some("error")) => StopReason::Error,
                _ => return Err("Invalid pi-messages terminal reason".into()),
            };
            if kind == "error" {
                message.error_message = Some("pi-messages service reported an error".into());
            }
            if kind == "done" && slots.values().any(|slot| !slot.closed) {
                return Err("pi-messages stream ended with an open content block".into());
            }
            return Ok(message);
        }
        let index = event["contentIndex"]
            .as_u64()
            .filter(|index| *index < 8192)
            .ok_or("Invalid pi-messages content index")?;
        if let Some(block_kind) = kind.strip_suffix("_start") {
            if slots.contains_key(&index) {
                return Err("Duplicate pi-messages content index".into());
            }
            let content = match block_kind {
                "text" => Content::text(""),
                "thinking" => Content::Thinking {
                    thinking: String::new(),
                },
                "toolcall" => Content::ToolCall(ToolCall {
                    id: event["id"]
                        .as_str()
                        .filter(|id| !id.is_empty())
                        .ok_or("pi-messages tool ID missing")?
                        .into(),
                    name: event["toolName"]
                        .as_str()
                        .filter(|name| !name.is_empty())
                        .ok_or("pi-messages tool name missing")?
                        .into(),
                    arguments: json!({}),
                }),
                _ => return Err("Unknown pi-messages block type".into()),
            };
            let position = message.content.len();
            message.content.push(content);
            slots.insert(
                index,
                Slot {
                    position,
                    kind: block_kind.into(),
                    arguments: String::new(),
                    closed: false,
                },
            );
        } else {
            let (block_kind, action) = kind.rsplit_once('_').ok_or("Unknown pi-messages event")?;
            let slot = slots
                .get_mut(&index)
                .ok_or("pi-messages delta precedes its content start")?;
            if slot.closed || slot.kind != block_kind {
                return Err("Conflicting pi-messages content state".into());
            }
            let content = &mut message.content[slot.position];
            if action == "delta" {
                let delta = event["delta"].as_str().ok_or("pi-messages delta missing")?;
                match content {
                    Content::Text { text } => text.push_str(delta),
                    Content::Thinking { thinking } => thinking.push_str(delta),
                    Content::ToolCall(call) => {
                        slot.arguments.push_str(delta);
                        if let Ok(args) = serde_json::from_str::<Value>(&slot.arguments) {
                            if args.is_object() {
                                call.arguments = args;
                            }
                        }
                    }
                    _ => unreachable!("Only streamable blocks"),
                }
            } else if action == "end" {
                match content {
                    Content::Text { text } => {
                        *text = event["content"]
                            .as_str()
                            .ok_or("pi-messages final text missing")?
                            .into()
                    }
                    Content::Thinking { thinking } => {
                        *thinking = event["content"]
                            .as_str()
                            .ok_or("pi-messages final thinking missing")?
                            .into()
                    }
                    Content::ToolCall(call) => {
                        let wire = &event["toolCall"];
                        if wire["id"] != call.id
                            || wire["name"] != call.name
                            || !wire["arguments"].is_object()
                        {
                            return Err("Conflicting pi-messages final tool call".into());
                        }
                        if !slot.arguments.is_empty() {
                            let args: Value = serde_json::from_str(&slot.arguments)
                                .map_err(|_| "Malformed pi-messages tool JSON")?;
                            if args != wire["arguments"] {
                                return Err("Conflicting pi-messages tool arguments".into());
                            }
                        }
                        call.arguments = wire["arguments"].clone();
                    }
                    _ => unreachable!("Only streamable blocks"),
                }
                if let Some(signature) = event.get("contentSignature") {
                    if !signature.is_string() {
                        return Err("Invalid pi-messages content signature".into());
                    }
                    message.protocol_metadata.as_mut().expect("Owned metadata")["signatures"]
                        [slot.position.to_string()] = signature.clone();
                }
                if let Some(redacted) = event.get("redacted") {
                    if !redacted.is_boolean() {
                        return Err("Invalid pi-messages redaction field".into());
                    }
                    message.protocol_metadata.as_mut().expect("Owned metadata")["redacted"]
                        [slot.position.to_string()] = redacted.clone();
                }
                slot.closed = true;
                if block_kind == "toolcall" {
                    message.protocol_metadata.as_mut().expect("Owned metadata")["tool_metadata"]
                        [slot.position.to_string()] = event["toolCall"].clone();
                }
            } else {
                return Err("Unknown pi-messages block action".into());
            }
        }
        let ids: Vec<_> = message
            .tool_calls()
            .iter()
            .map(|call| call.id.clone())
            .collect();
        let unique: BTreeSet<_> = ids.iter().collect();
        if unique.len() != ids.len() {
            return Err("Duplicate pi-messages tool ID".into());
        }
        on_update(message.clone());
    }
    Err("pi-messages stream ended without a terminal event".into())
}
