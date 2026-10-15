//! Read-only import of the observed TypeScript session JSONL versions 1–3.
//!
//! Branch selection is explicit. Every successfully decoded original record,
//! including extension and future record metadata, survives the native save.

use crate::session::Session;
use crate::session_tree::SessionEntry;
use agent_core::types::{
    AssistantMessage, Content, Context, Message, StopReason, ToolResultMessage, Usage, UserMessage,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LeafSelection {
    Latest,
    Root,
    Entry(String),
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct LegacyModel {
    pub provider: String,
    pub model_id: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct LegacyImportMetadata {
    pub source_path: PathBuf,
    pub header: Value,
    pub records: Vec<Value>,
    pub selected_leaf_id: Option<String>,
    pub selected_entry_ids: Vec<String>,
    pub context_entry_ids: Vec<String>,
    pub thinking_level: String,
    pub model: Option<LegacyModel>,
    pub diagnostics: Vec<String>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct LegacyLeaf {
    pub id: String,
    pub entry_type: String,
    pub preview: String,
}

#[derive(Clone, Debug)]
pub struct LegacySession {
    pub source_path: PathBuf,
    pub header: Value,
    pub records: Vec<Value>,
    pub diagnostics: Vec<String>,
    entries: Vec<Value>,
    by_id: BTreeMap<String, usize>,
}

pub fn load_legacy_session(path: &Path) -> Result<LegacySession, String> {
    let source_path =
        fs::canonicalize(path).map_err(|error| format!("{}: {error}", path.display()))?;
    let content = fs::read_to_string(&source_path).map_err(|error| error.to_string())?;
    parse_legacy_session(&content, source_path)
}

pub fn parse_legacy_session(content: &str, source_path: PathBuf) -> Result<LegacySession, String> {
    let mut records: Vec<Value> = Vec::new();
    let mut diagnostics = Vec::new();
    for (index, line) in content.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Value>(line) {
            Ok(record) if record.is_object() => records.push(record),
            Ok(_) => diagnostics.push(format!("Line {}: record is not an object", index + 1)),
            Err(error) => diagnostics.push(format!("Line {}: malformed JSON: {error}", index + 1)),
        }
    }
    let header = records
        .first()
        .filter(|record| record["type"] == "session")
        .ok_or("First parsed record must be a session header")?
        .clone();
    required_string(&header, "id")?;
    let version = match header.get("version") {
        None => 1,
        Some(version) => version
            .as_u64()
            .ok_or("Session version must be an integer")?,
    };
    if !(1..=3).contains(&version) {
        return Err(format!("Unsupported legacy session version: {version}"));
    }
    let mut entries = Vec::new();
    let mut previous_id: Option<String> = None;
    for (index, original) in records.iter().enumerate().skip(1) {
        if original["type"] == "session" {
            return Err("Multiple session headers are ambiguous".into());
        }
        let mut entry = original.clone();
        if version == 1 {
            let id = format!("legacy-{index}");
            entry["id"] = json!(id);
            entry["parentId"] = json!(previous_id);
            previous_id = Some(id);
        }
        if version < 3 && entry["type"] == "message" && entry["message"]["role"] == "hookMessage" {
            entry["message"]["role"] = json!("custom");
        }
        if entry["id"].as_str().is_none()
            || !entry
                .get("parentId")
                .is_some_and(|id| id.is_null() || id.is_string())
        {
            diagnostics.push(format!(
                "Record {} has no valid id/parentId; preserved as metadata only",
                index + 1
            ));
            continue;
        }
        entries.push(entry);
    }
    if version == 1 {
        for entry in &mut entries {
            if entry["type"] == "compaction" {
                if let Some(index) = entry["firstKeptEntryIndex"].as_u64() {
                    if records
                        .get(index as usize)
                        .is_some_and(|record| record["type"] != "session")
                    {
                        entry["firstKeptEntryId"] = json!(format!("legacy-{index}"));
                    }
                    entry
                        .as_object_mut()
                        .expect("validated object")
                        .remove("firstKeptEntryIndex");
                }
            }
        }
    }
    let mut by_id = BTreeMap::new();
    for (index, entry) in entries.iter().enumerate() {
        let id = required_string(entry, "id")?;
        if by_id.insert(id.to_owned(), index).is_some() {
            return Err(format!("Duplicate session entry id: {id}"));
        }
    }
    Ok(LegacySession {
        source_path,
        header,
        records,
        diagnostics,
        entries,
        by_id,
    })
}

impl LegacySession {
    pub(crate) fn migrated_entries(&self) -> &[Value] {
        &self.entries
    }
    pub fn leaves(&self) -> Vec<LegacyLeaf> {
        let parents: BTreeSet<&str> = self
            .entries
            .iter()
            .filter_map(|entry| entry["parentId"].as_str())
            .collect();
        self.entries
            .iter()
            .filter(|entry| !parents.contains(entry["id"].as_str().unwrap_or_default()))
            .map(|entry| {
                let preview = entry["message"]["content"]
                    .as_str()
                    .or_else(|| entry["summary"].as_str())
                    .unwrap_or_default()
                    .chars()
                    .take(80)
                    .collect();
                LegacyLeaf {
                    id: entry["id"].as_str().unwrap_or_default().into(),
                    entry_type: entry["type"].as_str().unwrap_or("unknown").into(),
                    preview,
                }
            })
            .collect()
    }

    pub fn import(&self, selection: LeafSelection) -> Result<Session, String> {
        self.import_with_workspace(selection, None)
    }

    pub fn import_with_workspace(
        &self,
        selection: LeafSelection,
        workspace_override: Option<&Path>,
    ) -> Result<Session, String> {
        let leaf_id = match selection {
            LeafSelection::Latest => self
                .entries
                .last()
                .and_then(|entry| entry["id"].as_str())
                .map(str::to_owned),
            LeafSelection::Root => None,
            LeafSelection::Entry(id) => {
                if !self.by_id.contains_key(&id) {
                    return Err(format!("Session entry not found: {id}"));
                }
                Some(id)
            }
        };
        let mut path = Vec::new();
        let mut next = leaf_id.as_deref();
        let mut visited = BTreeSet::new();
        while let Some(id) = next {
            if !visited.insert(id) {
                return Err(format!("Cycle in session branch at {id}"));
            }
            let entry = self
                .by_id
                .get(id)
                .and_then(|index| self.entries.get(*index))
                .ok_or_else(|| format!("Missing parent entry: {id}"))?;
            path.push(entry);
            next = entry["parentId"].as_str().filter(|id| !id.is_empty());
        }
        path.reverse();
        let mut context_entries = path.clone();
        if let Some(compaction_index) = path.iter().rposition(|entry| {
            entry["type"] == "compaction" && entry.get("activeMessages").is_none()
        }) {
            let compaction = path[compaction_index];
            let first_kept = required_string(compaction, "firstKeptEntryId")?;
            context_entries = vec![compaction];
            if let Some(start) = path[..compaction_index]
                .iter()
                .position(|entry| entry["id"] == first_kept)
            {
                context_entries.extend_from_slice(&path[start..compaction_index]);
            }
            context_entries.extend_from_slice(&path[compaction_index + 1..]);
        }
        let mut thinking_level = "off".to_owned();
        let mut model = None;
        for entry in &path {
            if entry["type"] == "thinking_level_change" {
                thinking_level = required_string(entry, "thinkingLevel")?.into();
            } else if entry["type"] == "model_change" {
                model = Some(LegacyModel {
                    provider: required_string(entry, "provider")?.into(),
                    model_id: required_string(entry, "modelId")?.into(),
                });
            } else if entry["type"] == "message" && entry["message"]["role"] == "assistant" {
                if let (Some(provider), Some(model_id)) = (
                    entry["message"]["provider"].as_str(),
                    entry["message"]["model"].as_str(),
                ) {
                    model = Some(LegacyModel {
                        provider: provider.into(),
                        model_id: model_id.into(),
                    });
                }
            }
        }
        let mut messages = Vec::new();
        let mut diagnostics = self.diagnostics.clone();
        for entry in &context_entries {
            if let Some(message) = context_message(entry, &mut diagnostics)? {
                messages.push(message);
            }
        }
        let workspace = match workspace_override {
            Some(path) => path.to_owned(),
            None => PathBuf::from(
                self.header["cwd"]
                    .as_str()
                    .filter(|cwd| !cwd.is_empty())
                    .ok_or("Session cwd is missing; select an explicit workspace")?,
            ),
        };
        let name = self
            .entries
            .iter()
            .rev()
            .find(|entry| entry["type"] == "session_info")
            .and_then(|entry| entry["name"].as_str())
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_owned)
            .or_else(|| {
                messages
                    .iter()
                    .find(|message| matches!(message, Message::User(_)))
                    .map(|message| message.text().chars().take(24).collect())
            })
            .unwrap_or_else(|| "导入会话".into());
        let updated_at = messages
            .iter()
            .map(|message| match message {
                Message::User(message) => message.timestamp,
                Message::Assistant(message) => message.timestamp,
                Message::ToolResult(message) => message.timestamp,
                Message::Custom { timestamp, .. } => *timestamp,
            })
            .max()
            .unwrap_or(0);
        let mut session = Session {
            name,
            workspace,
            updated_at,
            context: Context {
                system_prompt: Session::default().context.system_prompt,
                messages,
            },
            imported: Some(LegacyImportMetadata {
                source_path: self.source_path.clone(),
                header: self.header.clone(),
                records: self.records.clone(),
                selected_leaf_id: leaf_id.clone(),
                selected_entry_ids: path
                    .iter()
                    .map(|entry| entry["id"].as_str().unwrap_or_default().into())
                    .collect(),
                context_entry_ids: context_entries
                    .iter()
                    .map(|entry| entry["id"].as_str().unwrap_or_default().into())
                    .collect(),
                thinking_level,
                model,
                diagnostics,
            }),
            entries: self
                .entries
                .iter()
                .cloned()
                .map(SessionEntry::from_value)
                .collect::<Result<_, _>>()?,
            leaf_id,
            tree_version: 1,
            ..Session::default()
        };
        session.refresh_tree_context()?;
        Ok(session)
    }
}

fn required_string<'a>(value: &'a Value, field: &str) -> Result<&'a str, String> {
    value[field]
        .as_str()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("Missing string field: {field}"))
}

fn parse_content(value: &Value) -> Result<Vec<Content>, String> {
    match value {
        Value::Null => Ok(vec![]),
        Value::String(text) => Ok(vec![Content::text(text)]),
        Value::Array(parts) => parts
            .iter()
            .map(|part| {
                serde_json::from_value(part.clone())
                    .map_err(|error| format!("Unsupported message content: {error}"))
            })
            .collect(),
        _ => Err("Message content must be text or an array".into()),
    }
}

fn parse_usage(value: &Value) -> Option<Usage> {
    value.as_object().map(|_| Usage {
        input: value["input"].as_u64().unwrap_or(0),
        output: value["output"].as_u64().unwrap_or(0),
        cache_read: value["cacheRead"].as_u64().unwrap_or(0),
        cache_write: value["cacheWrite"].as_u64().unwrap_or(0),
        total_tokens: value["totalTokens"].as_u64().unwrap_or(0),
    })
}

fn replay_diagnostic(diagnostics: &mut Vec<String>, entry_id: &Value, index: usize, problem: &str) {
    diagnostics.push(format!(
        "Entry {entry_id}, content {index}: {problem}; original content retained in imported records"
    ));
}

fn content_signature<'a>(
    block: &'a Value,
    field: &str,
    entry_id: &Value,
    index: usize,
    diagnostics: &mut Vec<String>,
) -> Option<&'a str> {
    match block.get(field) {
        None | Some(Value::Null) => None,
        Some(Value::String(signature)) => (!signature.is_empty()).then_some(signature.as_str()),
        Some(_) => {
            replay_diagnostic(
                diagnostics,
                entry_id,
                index,
                &format!("{field} is not a string"),
            );
            None
        }
    }
}

// Match google-shared.ts's TYPE_BYTES replay validation without rewriting the opaque value.
fn valid_google_signature(signature: &str) -> bool {
    let bytes = signature.as_bytes();
    let unpadded = bytes
        .iter()
        .rposition(|byte| *byte != b'=')
        .map_or(0, |position| position + 1);
    !bytes.is_empty()
        && bytes.len().is_multiple_of(4)
        && unpadded > 0
        && bytes.len() - unpadded <= 2
        && bytes[..unpadded]
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/'))
}

// Exact shortHash from packages/ai/src/utils/hash.ts, including JS UTF-16 code units.
fn responses_short_hash(text: &str) -> String {
    let mut h1 = 0xdead_beef_u32;
    let mut h2 = 0x41c6_ce57_u32;
    for unit in text.encode_utf16() {
        h1 = (h1 ^ u32::from(unit)).wrapping_mul(2_654_435_761);
        h2 = (h2 ^ u32::from(unit)).wrapping_mul(1_597_334_677);
    }
    h1 = (h1 ^ (h1 >> 16)).wrapping_mul(2_246_822_507)
        ^ (h2 ^ (h2 >> 13)).wrapping_mul(3_266_489_909);
    h2 = (h2 ^ (h2 >> 16)).wrapping_mul(2_246_822_507)
        ^ (h1 ^ (h1 >> 13)).wrapping_mul(3_266_489_909);
    let mut result = String::new();
    for mut hash in [h2, h1] {
        let mut digits = Vec::new();
        loop {
            digits.push(char::from_digit(hash % 36, 36).expect("base36 digit"));
            hash /= 36;
            if hash == 0 {
                break;
            }
        }
        result.extend(digits.into_iter().rev());
    }
    result
}

fn responses_text_item(
    block: &Value,
    signature: &str,
    entry_id: &Value,
    index: usize,
    diagnostics: &mut Vec<String>,
) -> Option<Value> {
    let parsed = signature
        .starts_with('{')
        .then(|| serde_json::from_str::<Value>(signature).ok())
        .flatten();
    let v1 = parsed
        .as_ref()
        .filter(|parsed| parsed["v"] == 1 && parsed["id"].is_string());
    if signature.starts_with('{') && v1.is_none() {
        replay_diagnostic(diagnostics, entry_id, index,
            "textSignature is not TextSignatureV1; using the observed legacy plain-ID interpretation");
    }
    let id = v1
        .and_then(|parsed| parsed["id"].as_str())
        .unwrap_or(signature);
    if id.is_empty() {
        replay_diagnostic(diagnostics, entry_id, index,
            "empty Responses text ID requires a TypeScript message-index fallback; excluded from protocol replay");
        return None;
    }
    let id = if id.encode_utf16().count() > 64 {
        format!("msg_{}", responses_short_hash(id))
    } else {
        id.to_owned()
    };
    let mut item = json!({"type":"message","role":"assistant","id":id,"status":"completed","content":[{"type":"output_text","text":block["text"],"annotations":[]}]});
    if let Some(phase @ ("commentary" | "final_answer")) =
        v1.and_then(|parsed| parsed["phase"].as_str())
    {
        item["phase"] = json!(phase);
    }
    Some(item)
}

fn valid_reasoning_item(item: &Value) -> bool {
    item["type"] == "reasoning"
        && item["id"].as_str().is_some_and(|id| !id.is_empty())
        && item["summary"].as_array().is_some_and(|summary| {
            summary
                .iter()
                .all(|part| part["type"] == "summary_text" && part["text"].is_string())
        })
        && item.get("content").is_none_or(|content| {
            content.as_array().is_some_and(|parts| {
                parts
                    .iter()
                    .all(|part| part["type"] == "reasoning_text" && part["text"].is_string())
            })
        })
        && item
            .get("encrypted_content")
            .is_none_or(|encrypted| encrypted.is_null() || encrypted.is_string())
        && item.get("status").is_none_or(|status| {
            matches!(
                status.as_str(),
                Some("in_progress" | "completed" | "incomplete")
            )
        })
}

fn legacy_protocol_metadata(
    message: &Value,
    entry_id: &Value,
    diagnostics: &mut Vec<String>,
) -> Option<Value> {
    let blocks = message["content"].as_array()?;
    let protocol = match message["api"].as_str() {
        Some("anthropic-messages") => "anthropic",
        Some("google-generative-ai" | "google-vertex") => "google",
        Some("openai-responses" | "azure-openai-responses" | "openai-codex-responses") => {
            "openai-responses"
        }
        _ => {
            if blocks.iter().any(|block| {
                [
                    "thinkingSignature",
                    "textSignature",
                    "thoughtSignature",
                    "namespace",
                ]
                .iter()
                .any(|field| block.get(field).is_some())
                    || block["redacted"] == true
            }) {
                diagnostics.push(format!("Entry {entry_id}: unknown or missing API {:?}; replay metadata retained only in imported records", message["api"]));
            }
            return None;
        }
    };
    let Some(model) = message["model"].as_str().filter(|model| !model.is_empty()) else {
        diagnostics.push(format!("Entry {entry_id}: missing model identity; replay metadata retained only in imported records"));
        return None;
    };
    let mut metadata = match protocol {
        "anthropic" => json!({"protocol":protocol,"model":model,"blocks":{}}),
        "google" => json!({"protocol":protocol,"model":model,"signatures":{}}),
        _ => json!({"protocol":protocol,"model":model,"items":{}}),
    };
    for (index, block) in blocks.iter().enumerate() {
        match (protocol, block["type"].as_str()) {
            ("anthropic", Some("thinking")) => {
                let signature =
                    content_signature(block, "thinkingSignature", entry_id, index, diagnostics)
                        .filter(|signature| !signature.trim().is_empty());
                if let Some(signature) = signature {
                    metadata["blocks"][index.to_string()] = json!({"kind":if block["redacted"] == true {"redacted"} else {"thinking"},"signature":signature});
                } else if block["redacted"] == true {
                    replay_diagnostic(
                        diagnostics,
                        entry_id,
                        index,
                        "redacted thinking lacks an opaque payload for replay",
                    );
                }
            }
            ("google", kind) => {
                let field = match kind {
                    Some("thinking") => "thinkingSignature",
                    Some("text") => "textSignature",
                    Some("toolCall") => "thoughtSignature",
                    _ => continue,
                };
                if let Some(signature) =
                    content_signature(block, field, entry_id, index, diagnostics)
                {
                    if valid_google_signature(signature) {
                        metadata["signatures"][index.to_string()] = json!(signature);
                    } else {
                        replay_diagnostic(diagnostics, entry_id, index, "Google thought signature is not valid base64; excluded from replay as in the TypeScript converter");
                    }
                }
            }
            ("openai-responses", Some("thinking")) => {
                if let Some(signature) =
                    content_signature(block, "thinkingSignature", entry_id, index, diagnostics)
                {
                    match serde_json::from_str::<Value>(signature) {
                        Ok(item) if valid_reasoning_item(&item) => metadata["items"][index.to_string()] = item,
                        _ => replay_diagnostic(diagnostics, entry_id, index, "thinkingSignature is not a complete Responses reasoning item; excluded from protocol replay"),
                    }
                }
            }
            ("openai-responses", Some("text")) => {
                if let Some(signature) =
                    content_signature(block, "textSignature", entry_id, index, diagnostics)
                {
                    if let Some(item) =
                        responses_text_item(block, signature, entry_id, index, diagnostics)
                    {
                        metadata["items"][index.to_string()] = item;
                    }
                }
            }
            ("openai-responses", Some("toolCall")) => {
                let mut ids = block["id"].as_str().unwrap_or_default().split('|');
                let call_id = ids.next().unwrap_or_default();
                let item_id = ids.next();
                let mut item = json!({"type":"function_call","call_id":call_id,"name":block["name"],"arguments":block["arguments"].to_string()});
                if let Some(id) = item_id.filter(|id| id.starts_with("fc_")) {
                    item["id"] = json!(id);
                }
                if item_id.is_some_and(|id| id.starts_with("ctc_")) {
                    replay_diagnostic(diagnostics, entry_id, index, "custom-tool item requires the original grammar-tool configuration; only function-call projection is available");
                }
                match block.get("namespace") {
                    Some(Value::String(namespace)) => item["namespace"] = json!(namespace),
                    Some(Value::Null) | None => {}
                    _ => replay_diagnostic(
                        diagnostics,
                        entry_id,
                        index,
                        "tool namespace is not a string",
                    ),
                }
                metadata["items"][index.to_string()] = item;
            }
            _ => {}
        }
    }
    if protocol == "openai-responses" {
        if let Some(response_id) = message["responseId"].as_str() {
            metadata["response_id"] = json!(response_id);
        }
    }
    Some(metadata)
}

pub(crate) fn context_message(
    entry: &Value,
    diagnostics: &mut Vec<String>,
) -> Result<Option<Message>, String> {
    if entry["native"] == true && entry["type"] == "message" {
        return serde_json::from_value(entry["message"].clone())
            .map(Some)
            .map_err(|error| format!("Invalid native message: {error}"));
    }
    if !matches!(
        entry["type"].as_str(),
        Some("message" | "compaction" | "branch_summary" | "custom_message")
    ) {
        return Ok(None);
    }
    let message = if entry["type"] == "message" {
        &entry["message"]
    } else {
        entry
    };
    if entry["type"] == "message"
        && !matches!(
            message["role"].as_str(),
            Some("user" | "custom" | "assistant" | "toolResult" | "bashExecution")
        )
    {
        diagnostics.push(format!(
            "Unknown message role {:?} in {}; retained as metadata, not sent to provider",
            message["role"], entry["id"]
        ));
        return Ok(None);
    }
    let timestamp = message["timestamp"]
        .as_u64()
        .or_else(|| entry["timestamp"].as_u64())
        .or_else(|| entry["timestamp"].as_str().and_then(parse_iso_timestamp))
        .ok_or_else(|| format!("Invalid timestamp in entry {}", entry["id"]))?;
    let user = |content| Message::User(UserMessage { content, timestamp });
    match entry["type"].as_str().unwrap_or_default() {
        "compaction" => Ok(Some(user(vec![Content::text(format!("The conversation history before this point was compacted into the following summary:\n\n<summary>\n{}\n</summary>", entry["summary"].as_str().ok_or("Compaction summary must be text")?))]))),
        "branch_summary" => {
            let summary = entry["summary"].as_str().unwrap_or_default();
            Ok((!summary.is_empty()).then(|| user(vec![Content::text(format!("The following is a summary of a branch that this conversation came back from:\n\n<summary>\n{summary}</summary>"))])))
        }
        "custom_message" => Ok(Some(user(parse_content(&entry["content"])?))),
        "message" => match message["role"].as_str().unwrap_or_default() {
            "user" | "custom" => Ok(Some(user(parse_content(&message["content"])?))),
            "assistant" => Ok(Some(Message::Assistant(AssistantMessage {
                content: parse_content(&message["content"])?,
                stop_reason: serde_json::from_value::<StopReason>(message["stopReason"].clone()).map_err(|error| format!("Invalid stopReason: {error}"))?,
                error_message: message["errorMessage"].as_str().map(str::to_owned),
                usage: parse_usage(&message["usage"]), timestamp,
                protocol_metadata: legacy_protocol_metadata(message, &entry["id"], diagnostics),
            }))),
            "toolResult" => Ok(Some(Message::ToolResult(ToolResultMessage {
                tool_call_id: required_string(message, "toolCallId")?.into(), tool_name: required_string(message, "toolName")?.into(),
                content: parse_content(&message["content"])? , details: message["details"].clone(), usage: parse_usage(&message["usage"]),
                added_tool_names: message["addedToolNames"].as_array().map(|names| names.iter().filter_map(Value::as_str).map(str::to_owned).collect()).unwrap_or_default(),
                is_error: message["isError"].as_bool().unwrap_or(false), timestamp,
            }))),
            "bashExecution" => {
                if message["excludeFromContext"].as_bool() == Some(true) { return Ok(None); }
                let command = required_string(message, "command")?;
                let output = message["output"].as_str().unwrap_or_default();
                let mut text = format!("Ran `{command}`\n");
                if output.is_empty() { text.push_str("(no output)"); } else { text.push_str(&format!("```\n{output}\n```")); }
                if message["cancelled"].as_bool() == Some(true) { text.push_str("\n\n(command cancelled)"); }
                else if let Some(code) = message["exitCode"].as_i64().filter(|code| *code != 0) { text.push_str(&format!("\n\nCommand exited with code {code}")); }
                if message["truncated"].as_bool() == Some(true) {
                    if let Some(path) = message["fullOutputPath"].as_str() { text.push_str(&format!("\n\n[Output truncated. Full output: {path}]")); }
                }
                Ok(Some(user(vec![Content::text(text)])))
            }
            role => {
                diagnostics.push(format!("Unknown message role {role:?} in {}; retained as metadata, not sent to provider", entry["id"]));
                Ok(None)
            }
        },
        _ => Ok(None),
    }
}

// SessionManager writes ISO UTC timestamps. Offset forms are accepted explicitly.
pub(crate) fn parse_iso_timestamp(text: &str) -> Option<u64> {
    let (date, time) = text.split_once('T')?;
    let mut date = date.split('-');
    let year = date.next()?.parse::<i64>().ok()?;
    let month = date.next()?.parse::<i64>().ok()?;
    let day = date.next()?.parse::<i64>().ok()?;
    if date.next().is_some() || !(0..=9999).contains(&year) || !(1..=12).contains(&month) {
        return None;
    }
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let max_day = match month {
        2 if leap => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    };
    if !(1..=max_day).contains(&day) {
        return None;
    }
    let (time, offset) = if let Some(time) = time.strip_suffix('Z') {
        (time, 0)
    } else {
        let index = time.rfind(['+', '-'])?;
        let zone = &time[index + 1..];
        let (hours, minutes) = zone.split_once(':')?;
        let hours = hours.parse::<i64>().ok()?;
        let minutes = minutes.parse::<i64>().ok()?;
        if hours > 23 || minutes > 59 {
            return None;
        }
        (
            &time[..index],
            (hours * 60 + minutes)
                * 60
                * if time.as_bytes()[index] == b'+' {
                    1
                } else {
                    -1
                },
        )
    };
    let mut clock = time.split(':');
    let hour = clock.next()?.parse::<i64>().ok()?;
    let minute = clock.next()?.parse::<i64>().ok()?;
    let second = clock.next()?;
    if clock.next().is_some() || !(0..=23).contains(&hour) || !(0..=59).contains(&minute) {
        return None;
    }
    let (second, fraction) = second.split_once('.').unwrap_or((second, ""));
    let second = second.parse::<i64>().ok()?;
    if !(0..=59).contains(&second) || !fraction.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let milliseconds = format!("{fraction:0<3}")[..3].parse::<i64>().ok()?;
    let adjusted_year = year - i64::from(month <= 2);
    let era = adjusted_year.div_euclid(400);
    let year_of_era = adjusted_year - era * 400;
    let shifted_month = month + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * shifted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146_097 + day_of_era - 719_468;
    ((days * 86_400 + hour * 3600 + minute * 60 + second - offset) * 1000 + milliseconds)
        .try_into()
        .ok()
}
