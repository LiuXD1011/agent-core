use super::sse::{json_event, next_event, RESPONSE_LIMIT};
use agent_core::types::{
    timestamp, AssistantMessage, CancellationToken, Content, Context, Message, StopReason,
    ToolCall, Usage,
};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::io::BufRead;
use std::sync::atomic::{AtomicU64, Ordering};

fn parts(content: &[Content]) -> Vec<Value> {
    content
        .iter()
        .filter_map(|block| match block {
            Content::Text { text } => Some(json!({"text":text})),
            Content::Thinking { thinking } => Some(json!({"text":thinking})),
            Content::Image { data, mime_type } => {
                Some(json!({"inlineData":{"mimeType":mime_type,"data":data}}))
            }
            Content::ToolCall(_) => None,
        })
        .collect()
}

pub fn google_request_payload(context: &Context, model: &str, tools: &[Value]) -> Value {
    google_request_payload_for_protocol(context, model, tools, "google")
}
pub(super) fn google_request_payload_for_protocol(
    context: &Context,
    model: &str,
    tools: &[Value],
    protocol: &str,
) -> Value {
    let short_model = model
        .strip_prefix("publishers/google/models/")
        .or_else(|| model.strip_prefix("models/"))
        .unwrap_or(model);
    let major_version = short_model
        .strip_prefix("gemini-live-")
        .or_else(|| short_model.strip_prefix("gemini-"))
        .and_then(|version| {
            version
                .chars()
                .take_while(char::is_ascii_digit)
                .collect::<String>()
                .parse::<u32>()
                .ok()
        });
    let legacy_gemini = major_version.is_some_and(|major| major < 3);
    let mut contents: Vec<Value> = Vec::new();
    for message in &context.messages {
        match message {
            Message::User(user) => {
                contents.push(json!({"role":"user","parts":parts(&user.content)}))
            }
            Message::Assistant(assistant) => {
                let metadata = assistant.protocol_metadata.as_ref().filter(|metadata| {
                    metadata["protocol"] == protocol && metadata["model"] == model
                });
                let parts: Vec<_> = assistant
                    .content
                    .iter()
                    .enumerate()
                    .map(|(index, block)| {
                        let mut part = match block {
                            Content::ToolCall(call) => {
                                let mut function = json!({"name":call.name,"args":call.arguments});
                                if !legacy_gemini {
                                    function["id"] = json!(call.id);
                                }
                                json!({"functionCall":function})
                            }
                            Content::Thinking { thinking } if metadata.is_some() => {
                                json!({"text":thinking,"thought":true})
                            }
                            _ => parts(std::slice::from_ref(block))
                                .into_iter()
                                .next()
                                .unwrap_or_else(|| json!({})),
                        };
                        if let Some(signature) = metadata
                            .and_then(|metadata| metadata["signatures"].get(index.to_string()))
                            .and_then(Value::as_str)
                        {
                            part["thoughtSignature"] = json!(signature);
                        }
                        part
                    })
                    .collect();
                if !parts.is_empty() {
                    contents.push(json!({"role":"model","parts":parts}));
                }
            }
            Message::ToolResult(result) => {
                let images: Vec<_> = parts(&result.content)
                    .into_iter()
                    .filter(|part| part.get("inlineData").is_some())
                    .collect();
                let mut function = json!({"name":result.tool_name,"response":if result.is_error { json!({"error":message.text()}) } else { json!({"output":message.text()}) }});
                if !legacy_gemini {
                    function["id"] = json!(result.tool_call_id);
                }
                if !images.is_empty() && !legacy_gemini {
                    function["parts"] = json!(images);
                }
                let response = json!({"functionResponse":function});
                if let Some(last) = contents.last_mut().filter(|last| {
                    last["role"] == "user"
                        && last["parts"].as_array().is_some_and(|parts| {
                            parts
                                .iter()
                                .any(|part| part.get("functionResponse").is_some())
                        })
                }) {
                    last["parts"]
                        .as_array_mut()
                        .expect("Owned content array")
                        .push(response);
                } else {
                    contents.push(json!({"role":"user","parts":[response]}));
                }
                if legacy_gemini && !images.is_empty() {
                    contents.push(json!({"role":"user","parts":images}));
                }
            }
            Message::Custom { .. } => {}
        }
    }
    let mut payload = json!({"contents":contents});
    if !context.system_prompt.is_empty() {
        payload["systemInstruction"] = json!({"parts":[{"text":context.system_prompt}]});
    }
    if !tools.is_empty() {
        payload["tools"] = json!([{"functionDeclarations":tools.iter().map(|tool| {
            let function = tool.get("function").unwrap_or(tool);
            json!({"name":function["name"],"description":function["description"],"parametersJsonSchema":function.get("parameters").cloned().unwrap_or_else(|| json!({"type":"object"}))})
        }).collect::<Vec<_>>()}]);
    }
    payload
}

pub fn parse_google_sse(
    mut reader: impl BufRead,
    signal: &CancellationToken,
    on_update: &mut dyn FnMut(AssistantMessage),
) -> Result<AssistantMessage, String> {
    static NEXT_CALL: AtomicU64 = AtomicU64::new(0);
    let mut message = AssistantMessage::text("");
    message.content.clear();
    message.protocol_metadata = Some(json!({"protocol":"google","signatures":{}}));
    let mut reason = None;
    let mut ids = BTreeSet::new();
    let mut response_bytes = 0;
    while let Some(data) = next_event(&mut reader, signal)? {
        if data == "[DONE]" {
            break;
        }
        response_bytes += data.len();
        if response_bytes > RESPONSE_LIMIT {
            return Err("Model response exceeds limit".into());
        }
        let event = json_event(&data)?;
        if event.get("error").is_some() {
            return Err("Google service reported a stream error".into());
        }
        if event["promptFeedback"].get("blockReason").is_some() {
            return Err("Google provider blocked the prompt".into());
        }
        if let Some(candidate) = event["candidates"]
            .as_array()
            .and_then(|candidates| candidates.first())
        {
            if let Some(parts) = candidate["content"]["parts"].as_array() {
                if reason.is_some() && !parts.is_empty() {
                    return Err("Content received after Google finish reason".into());
                }
                for part in parts {
                    let signature = part["thoughtSignature"]
                        .as_str()
                        .filter(|signature| !signature.is_empty());
                    let position = if let Some(text) = part["text"].as_str() {
                        let thinking = part["thought"] == true;
                        let append = match message.content.last() {
                            Some(Content::Text { .. }) => !thinking,
                            Some(Content::Thinking { .. }) => thinking,
                            _ => false,
                        } && message
                            .protocol_metadata
                            .as_ref()
                            .expect("Metadata initialized")["signatures"]
                            .get(message.content.len().saturating_sub(1).to_string())
                            .is_none();
                        if append {
                            let position = message.content.len() - 1;
                            match &mut message.content[position] {
                                Content::Text { text: existing } => existing.push_str(text),
                                Content::Thinking { thinking: existing } => existing.push_str(text),
                                _ => unreachable!(),
                            }
                            position
                        } else {
                            let position = message.content.len();
                            message.content.push(if thinking {
                                Content::Thinking {
                                    thinking: text.to_owned(),
                                }
                            } else {
                                Content::text(text)
                            });
                            position
                        }
                    } else if let Some(call) = part.get("functionCall") {
                        let name = call["name"]
                            .as_str()
                            .filter(|name| !name.is_empty())
                            .ok_or("Google tool name missing")?;
                        let args = call.get("args").cloned().unwrap_or_else(|| json!({}));
                        if !args.is_object() {
                            return Err("Tool arguments must be an object".into());
                        }
                        let id = call["id"]
                            .as_str()
                            .filter(|id| !id.is_empty())
                            .map(str::to_owned)
                            .unwrap_or_else(|| {
                                format!(
                                    "google_{}_{}",
                                    timestamp(),
                                    NEXT_CALL.fetch_add(1, Ordering::Relaxed)
                                )
                            });
                        if !ids.insert(id.clone()) {
                            return Err("Duplicate Google tool call ID".into());
                        }
                        let position = message.content.len();
                        message.content.push(Content::ToolCall(ToolCall {
                            id,
                            name: name.to_owned(),
                            arguments: args,
                        }));
                        position
                    } else if let Some(image) = part.get("inlineData") {
                        let data = image["data"].as_str().ok_or("Google image data missing")?;
                        let mime_type = image["mimeType"]
                            .as_str()
                            .ok_or("Google image MIME type missing")?;
                        let position = message.content.len();
                        message.content.push(Content::Image {
                            data: data.to_owned(),
                            mime_type: mime_type.to_owned(),
                        });
                        position
                    } else {
                        return Err("Unsupported Google response part".into());
                    };
                    if let Some(signature) = signature {
                        message
                            .protocol_metadata
                            .as_mut()
                            .expect("Metadata initialized")["signatures"][position.to_string()] =
                            json!(signature);
                    }
                }
            }
            if let Some(finish) = candidate["finishReason"].as_str() {
                if reason.as_deref().is_some_and(|reason| reason != finish) {
                    return Err("Conflicting Google finish reason".into());
                }
                reason = Some(finish.to_owned());
            }
        }
        if let Some(usage) = event.get("usageMetadata") {
            let cache = usage["cachedContentTokenCount"].as_u64().unwrap_or(0);
            message.usage = Some(Usage {
                input: usage["promptTokenCount"]
                    .as_u64()
                    .unwrap_or(0)
                    .saturating_sub(cache),
                output: usage["candidatesTokenCount"]
                    .as_u64()
                    .unwrap_or(0)
                    .saturating_add(usage["thoughtsTokenCount"].as_u64().unwrap_or(0)),
                cache_read: cache,
                cache_write: 0,
                total_tokens: usage["totalTokenCount"].as_u64().unwrap_or(0),
            });
        }
        on_update(message.clone());
    }
    if signal.is_cancelled() {
        return Err("Operation aborted".into());
    }
    message.stop_reason = match reason.as_deref() {
        Some("STOP") if !ids.is_empty() => StopReason::ToolUse,
        Some("STOP") => StopReason::Stop,
        Some("MAX_TOKENS") => StopReason::Length,
        Some(_) => return Err("Google provider stopped without a usable response".into()),
        None => return Err("Google stream ended without a finish reason".into()),
    };
    if message.stop_reason == StopReason::Length {
        for block in &mut message.content {
            if let Content::ToolCall(call) = block {
                call.arguments = Value::Null;
            }
        }
    }
    Ok(message)
}
