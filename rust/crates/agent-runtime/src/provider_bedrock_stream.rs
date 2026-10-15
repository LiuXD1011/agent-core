//! CRC-verified Amazon EventStream framing and Bedrock ConverseStream events.
use super::sse::{preserved_error_code, RESPONSE_LIMIT};
use agent_core::types::{
    AssistantMessage, CancellationToken, Content, StopReason, ToolCall, Usage,
};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::{json, Value};
use std::collections::{btree_map::Entry, BTreeMap, BTreeSet};
use std::io::{self, Read};

#[derive(PartialEq)]
pub enum EventStreamHeader {
    Bool(bool),
    Byte(i8),
    Short(i16),
    Integer(i32),
    Long(i64),
    Bytes(Vec<u8>),
    String(String),
    Timestamp(u64),
    Uuid([u8; 16]),
}
pub struct EventStreamFrame {
    pub headers: BTreeMap<String, EventStreamHeader>,
    pub payload: Vec<u8>,
    /// Counts framing and headers as well as payload toward the agent response limit.
    pub total_length: usize,
}
fn transport_error(error: io::Error, signal: &CancellationToken) -> String {
    if signal.is_cancelled() {
        return "Operation aborted".into();
    }
    if error.kind() == io::ErrorKind::UnexpectedEof {
        return "Bedrock EventStream ended mid-frame".into();
    }
    let text = error.to_string();
    if let Some(http) = text.strip_prefix("模型服务返回 HTTP ") {
        let mut fields = http.split_whitespace();
        if let Some(status) = fields
            .next()
            .and_then(|value| value.parse::<u16>().ok())
            .filter(|status| (100..=599).contains(status))
        {
            let code = fields.next().filter(|code| {
                code.strip_prefix('(')
                    .and_then(|code| code.strip_suffix(')'))
                    .is_some_and(preserved_error_code)
            });
            return code.map_or_else(
                || format!("模型服务返回 HTTP {status}"),
                |code| format!("模型服务返回 HTTP {status} {code}"),
            );
        }
    }
    "Model response transport failed".into()
}
fn exact(
    reader: &mut impl Read,
    bytes: &mut [u8],
    signal: &CancellationToken,
) -> Result<(), String> {
    let mut used = 0;
    while used < bytes.len() {
        if signal.is_cancelled() {
            return Err("Operation aborted".into());
        }
        let count = reader
            .read(&mut bytes[used..])
            .map_err(|error| transport_error(error, signal))?;
        if count == 0 {
            return Err("Bedrock EventStream ended mid-frame".into());
        }
        used += count;
    }
    Ok(())
}
fn take<'a>(bytes: &mut &'a [u8], count: usize) -> Result<&'a [u8], String> {
    let value = bytes.get(..count).ok_or("Truncated EventStream header")?;
    *bytes = &bytes[count..];
    Ok(value)
}
pub fn read_eventstream_frame(
    reader: &mut impl Read,
    signal: &CancellationToken,
) -> Result<Option<EventStreamFrame>, String> {
    if signal.is_cancelled() {
        return Err("Operation aborted".into());
    }
    let mut prelude = [0u8; 12];
    let first = reader
        .read(&mut prelude[..1])
        .map_err(|error| transport_error(error, signal))?;
    if first == 0 {
        return Ok(None);
    }
    exact(reader, &mut prelude[1..], signal)?;
    if crc32fast::hash(&prelude[..8])
        != u32::from_be_bytes(prelude[8..12].try_into().expect("Prelude CRC"))
    {
        return Err("Invalid EventStream prelude CRC".into());
    }
    let total = u32::from_be_bytes(prelude[..4].try_into().expect("Prelude length")) as usize;
    let headers_length =
        u32::from_be_bytes(prelude[4..8].try_into().expect("Header length")) as usize;
    // Agent response policy, not the Smithy service's 24MB payload/128kB header constraints.
    if !(16..=RESPONSE_LIMIT).contains(&total) || headers_length > total - 16 {
        return Err("EventStream frame length exceeds agent limit or is invalid".into());
    }
    let mut remainder = vec![0u8; total - 12];
    exact(reader, &mut remainder, signal)?;
    let checksum = u32::from_be_bytes(
        remainder[remainder.len() - 4..]
            .try_into()
            .expect("Frame CRC"),
    );
    let mut hasher = crc32fast::Hasher::new();
    hasher.update(&prelude);
    hasher.update(&remainder[..remainder.len() - 4]);
    if hasher.finalize() != checksum {
        return Err("Invalid EventStream message CRC".into());
    }
    let mut bytes = &remainder[..headers_length];
    let mut headers = BTreeMap::new();
    while !bytes.is_empty() {
        let length = take(&mut bytes, 1)?[0] as usize;
        if length == 0 {
            return Err("EventStream header name is empty".into());
        }
        let name = std::str::from_utf8(take(&mut bytes, length)?)
            .map_err(|_| "Invalid EventStream header name UTF-8")?
            .to_owned();
        let kind = take(&mut bytes, 1)?[0];
        let value = match kind {
            0 => EventStreamHeader::Bool(true),
            1 => EventStreamHeader::Bool(false),
            2 => EventStreamHeader::Byte(take(&mut bytes, 1)?[0] as i8),
            3 => EventStreamHeader::Short(i16::from_be_bytes(
                take(&mut bytes, 2)?.try_into().expect("Header short"),
            )),
            4 => EventStreamHeader::Integer(i32::from_be_bytes(
                take(&mut bytes, 4)?.try_into().expect("Header integer"),
            )),
            5 => EventStreamHeader::Long(i64::from_be_bytes(
                take(&mut bytes, 8)?.try_into().expect("Header long"),
            )),
            6 | 7 => {
                let length =
                    u16::from_be_bytes(take(&mut bytes, 2)?.try_into().expect("Header length"))
                        as usize;
                let value = take(&mut bytes, length)?;
                if kind == 6 {
                    EventStreamHeader::Bytes(value.to_vec())
                } else {
                    EventStreamHeader::String(
                        std::str::from_utf8(value)
                            .map_err(|_| "Invalid EventStream string UTF-8")?
                            .to_owned(),
                    )
                }
            }
            8 => EventStreamHeader::Timestamp(u64::from_be_bytes(
                take(&mut bytes, 8)?.try_into().expect("Header timestamp"),
            )),
            9 => EventStreamHeader::Uuid(take(&mut bytes, 16)?.try_into().expect("Header UUID")),
            _ => return Err("Unknown EventStream header type".into()),
        };
        if headers.insert(name, value).is_some() {
            return Err("Duplicate EventStream header name".into());
        }
    }
    Ok(Some(EventStreamFrame {
        headers,
        payload: remainder[headers_length..remainder.len() - 4].to_vec(),
        total_length: total,
    }))
}
fn header<'a>(frame: &'a EventStreamFrame, name: &str) -> Result<&'a str, String> {
    match frame.headers.get(name) {
        Some(EventStreamHeader::String(value)) => Ok(value),
        _ => Err("Required EventStream string header missing".into()),
    }
}
fn exception(kind: &str) -> String {
    let status = match kind.to_ascii_lowercase().as_str() {
        "internalserverexception" => Some(500),
        "serviceunavailableexception" => Some(503),
        "throttlingexception" => Some(429),
        "validationexception" => Some(400),
        "accessdeniedexception" => Some(403),
        "modelstreamerrorexception" => Some(424),
        "modeltimeoutexception" => Some(408),
        _ => None,
    };
    status.map_or_else(
        || "Bedrock EventStream reported an error".into(),
        |status| format!("Bedrock stream returned HTTP {status}"),
    )
}
enum Kind {
    Text,
    Thinking,
    Tool,
    Image,
}
struct Slot {
    position: usize,
    kind: Kind,
    closed: bool,
    arguments: String,
    bytes: Vec<u8>,
    signature: String,
    redacted: bool,
}
fn index(event: &Value) -> Result<u64, String> {
    event["contentBlockIndex"]
        .as_u64()
        .ok_or_else(|| "Bedrock content block index missing".into())
}
fn number(value: &Value, field: &str) -> Result<u64, String> {
    if value.get(field).is_none() {
        return Ok(0);
    }
    value[field]
        .as_u64()
        .ok_or_else(|| "Invalid Bedrock token usage".into())
}
pub fn parse_bedrock_stream(
    mut reader: impl Read,
    signal: &CancellationToken,
    on_update: &mut dyn FnMut(AssistantMessage),
) -> Result<AssistantMessage, String> {
    let mut message = AssistantMessage::text("");
    message.content.clear();
    message.protocol_metadata = Some(json!({"protocol":"bedrock-converse-stream","blocks":{}}));
    let mut slots: BTreeMap<u64, Slot> = BTreeMap::new();
    let mut ids = BTreeSet::new();
    let mut started = false;
    let mut stopped = false;
    let mut metadata_seen = false;
    let mut total = 0usize;
    while let Some(frame) = read_eventstream_frame(&mut reader, signal)? {
        total = total.saturating_add(frame.total_length);
        if total > RESPONSE_LIMIT {
            return Err("Model response exceeds limit".into());
        }
        match header(&frame, ":message-type")? {
            "exception" => return Err(exception(header(&frame, ":exception-type")?)),
            "error" => return Err(exception(header(&frame, ":error-code")?)),
            "event" => {}
            _ => return Err("Unsupported EventStream message type".into()),
        }
        if let Some(content_type) = frame.headers.get(":content-type") {
            if !matches!(content_type,EventStreamHeader::String(value) if value=="application/json")
            {
                return Err("Unsupported Bedrock EventStream content type".into());
            }
        }
        let event: Value = serde_json::from_slice(&frame.payload)
            .map_err(|_| "Invalid Bedrock EventStream JSON")?;
        match header(&frame, ":event-type")? {
            "messageStart" => {
                if started || stopped || event["role"] != "assistant" {
                    return Err("Invalid Bedrock message start".into());
                }
                started = true;
            }
            "contentBlockStart" => {
                if !started || stopped {
                    return Err("Bedrock content outside an active message".into());
                }
                let index = index(&event)?;
                if slots.contains_key(&index) {
                    return Err("Duplicate Bedrock content block start".into());
                }
                let start = event["start"]
                    .as_object()
                    .filter(|start| start.len() == 1)
                    .ok_or("Invalid Bedrock content start union")?;
                let (kind, content) = if let Some(tool) = start.get("toolUse") {
                    let id = tool["toolUseId"]
                        .as_str()
                        .filter(|id| !id.is_empty())
                        .ok_or("Bedrock tool ID missing")?;
                    let name = tool["name"]
                        .as_str()
                        .filter(|name| !name.is_empty())
                        .ok_or("Bedrock tool name missing")?;
                    if !ids.insert(id.to_owned()) {
                        return Err("Duplicate Bedrock tool ID".into());
                    }
                    (
                        Kind::Tool,
                        Content::ToolCall(ToolCall {
                            id: id.into(),
                            name: name.into(),
                            arguments: json!({}),
                        }),
                    )
                } else if let Some(image) = start.get("image") {
                    let mime = match image["format"].as_str() {
                        Some("jpeg") => "image/jpeg",
                        Some("png") => "image/png",
                        Some("gif") => "image/gif",
                        Some("webp") => "image/webp",
                        _ => return Err("Unsupported Bedrock image format".into()),
                    };
                    (
                        Kind::Image,
                        Content::Image {
                            data: String::new(),
                            mime_type: mime.into(),
                        },
                    )
                } else {
                    return Err("Bedrock content block start type has not been migrated".into());
                };
                let position = message.content.len();
                message.content.push(content);
                slots.insert(
                    index,
                    Slot {
                        position,
                        kind,
                        closed: false,
                        arguments: String::new(),
                        bytes: vec![],
                        signature: String::new(),
                        redacted: false,
                    },
                );
            }
            "contentBlockDelta" => {
                if !started || stopped {
                    return Err("Bedrock content outside an active message".into());
                }
                let index = index(&event)?;
                let delta = event["delta"]
                    .as_object()
                    .filter(|delta| delta.len() == 1)
                    .ok_or("Invalid Bedrock content delta union")?;
                if let Entry::Vacant(entry) = slots.entry(index) {
                    let (kind, content) = if delta.contains_key("text") {
                        (Kind::Text, Content::text(""))
                    } else if delta.contains_key("reasoningContent") {
                        (
                            Kind::Thinking,
                            Content::Thinking {
                                thinking: String::new(),
                            },
                        )
                    } else {
                        return Err("Bedrock tool/image delta without a start".into());
                    };
                    let position = message.content.len();
                    message.content.push(content);
                    entry.insert(Slot {
                        position,
                        kind,
                        closed: false,
                        arguments: String::new(),
                        bytes: vec![],
                        signature: String::new(),
                        redacted: false,
                    });
                }
                let slot = slots.get_mut(&index).expect("Slot initialized");
                if slot.closed {
                    return Err("Bedrock delta after content block stop".into());
                }
                match (&slot.kind, &mut message.content[slot.position]) {
                    (Kind::Text, Content::Text { text }) => text.push_str(
                        delta
                            .get("text")
                            .and_then(Value::as_str)
                            .ok_or("Conflicting Bedrock text delta")?,
                    ),
                    (Kind::Tool, Content::ToolCall(call)) => {
                        slot.arguments.push_str(
                            delta
                                .get("toolUse")
                                .and_then(|tool| tool["input"].as_str())
                                .ok_or("Conflicting Bedrock tool delta")?,
                        );
                        if let Ok(value) = serde_json::from_str::<Value>(&slot.arguments) {
                            if value.is_object() {
                                call.arguments = value;
                            }
                        }
                    }
                    (Kind::Thinking, Content::Thinking { thinking }) => {
                        let reasoning = delta
                            .get("reasoningContent")
                            .and_then(Value::as_object)
                            .filter(|value| value.len() == 1)
                            .ok_or("Conflicting Bedrock reasoning delta")?;
                        if let Some(text) = reasoning.get("text").and_then(Value::as_str) {
                            thinking.push_str(text);
                        } else if let Some(signature) =
                            reasoning.get("signature").and_then(Value::as_str)
                        {
                            if !slot.redacted {
                                slot.signature.push_str(signature);
                            }
                        } else if let Some(redacted) =
                            reasoning.get("redactedContent").and_then(Value::as_str)
                        {
                            let bytes = STANDARD
                                .decode(redacted)
                                .map_err(|_| "Invalid Bedrock redacted base64")?;
                            if !slot.redacted {
                                slot.redacted = true;
                                slot.signature.clear();
                                thinking.push_str("[Reasoning redacted]");
                            }
                            slot.bytes.extend(bytes);
                        } else {
                            return Err("Unsupported Bedrock reasoning delta".into());
                        }
                        message.protocol_metadata.as_mut().expect("Metadata")["blocks"]
                            [slot.position.to_string()] = if slot.redacted {
                            json!({"kind":"redacted","data":STANDARD.encode(&slot.bytes)})
                        } else {
                            json!({"kind":"thinking","signature":slot.signature})
                        };
                    }
                    (Kind::Image, Content::Image { data, .. }) => {
                        let image = delta
                            .get("image")
                            .ok_or("Conflicting Bedrock image delta")?;
                        if image.get("error").is_some() {
                            return Err("Bedrock image stream returned an error".into());
                        }
                        let bytes = image["source"]["bytes"]
                            .as_str()
                            .ok_or("Bedrock image delta bytes missing")?;
                        slot.bytes.extend(
                            STANDARD
                                .decode(bytes)
                                .map_err(|_| "Invalid Bedrock image base64")?,
                        );
                        *data = STANDARD.encode(&slot.bytes);
                    }
                    _ => return Err("Conflicting Bedrock content delta".into()),
                }
            }
            "contentBlockStop" => {
                if !started || stopped {
                    return Err("Bedrock block stop outside an active message".into());
                }
                let slot = slots
                    .get_mut(&index(&event)?)
                    .ok_or("Bedrock block stop without content")?;
                if slot.closed {
                    return Err("Duplicate Bedrock block stop".into());
                }
                slot.closed = true;
            }
            "messageStop" => {
                if !started || stopped {
                    return Err("Invalid Bedrock message stop".into());
                }
                message.stop_reason = match event["stopReason"].as_str() {
                    Some("end_turn" | "stop_sequence") => {
                        if ids.is_empty() {
                            StopReason::Stop
                        } else {
                            StopReason::ToolUse
                        }
                    }
                    Some("tool_use") if !ids.is_empty() => StopReason::ToolUse,
                    Some("max_tokens" | "model_context_window_exceeded") => StopReason::Length,
                    Some("tool_use") => return Err("Bedrock tool-use stop has no calls".into()),
                    _ => return Err("Bedrock provider stopped without a usable response".into()),
                };
                for slot in slots.values() {
                    if let Content::ToolCall(call) = &mut message.content[slot.position] {
                        call.arguments = if message.stop_reason == StopReason::Length {
                            Value::Null
                        } else {
                            let arguments: Value = serde_json::from_str(&slot.arguments)
                                .map_err(|_| "Invalid Bedrock tool arguments")?;
                            if !arguments.is_object() {
                                return Err("Bedrock tool arguments must be an object".into());
                            }
                            arguments
                        };
                    }
                }
                stopped = true;
            }
            "metadata" => {
                if !stopped || metadata_seen {
                    return Err("Invalid Bedrock metadata event order".into());
                }
                metadata_seen = true;
                if let Some(usage) = event.get("usage") {
                    let input = number(usage, "inputTokens")?;
                    let output = number(usage, "outputTokens")?;
                    message.usage = Some(Usage {
                        input,
                        output,
                        cache_read: number(usage, "cacheReadInputTokens")?,
                        cache_write: number(usage, "cacheWriteInputTokens")?,
                        total_tokens: usage
                            .get("totalTokens")
                            .map_or(Ok(input.saturating_add(output)), |value| {
                                value.as_u64().ok_or("Invalid Bedrock token usage")
                            })?,
                    });
                    if let Some(details) = usage
                        .get("cacheDetails")
                        .filter(|details| details.is_array())
                    {
                        message.protocol_metadata.as_mut().expect("Metadata")["cache_details"] =
                            details.clone();
                    }
                }
            }
            "internalServerException"
            | "serviceUnavailableException"
            | "throttlingException"
            | "validationException"
            | "modelStreamErrorException" => return Err(exception(header(&frame, ":event-type")?)),
            _ => return Err("Bedrock event type has not been migrated".into()),
        }
        on_update(message.clone());
    }
    if signal.is_cancelled() {
        return Err("Operation aborted".into());
    }
    if !started || !stopped {
        return Err("Bedrock stream ended without messageStop".into());
    }
    Ok(message)
}
