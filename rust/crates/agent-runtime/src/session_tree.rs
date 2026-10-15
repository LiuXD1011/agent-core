//! Append-only session history. The active context is a projection, not the history.

use crate::compaction::NativeCompaction;
use crate::legacy_session::{
    context_message, parse_iso_timestamp, parse_legacy_session, LegacyModel,
};
use crate::session::Session;
use agent_core::types::{timestamp, Content, Message, ThinkingLevel, UserMessage};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

const TREE_VERSION: u32 = 1;
const SUMMARY_PREFIX: &str = "The conversation history before this point was compacted into the following summary:\n\n<summary>\n";
const SUMMARY_SUFFIX: &str = "\n</summary>";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionEntry {
    pub id: String,
    #[serde(rename = "parentId")]
    pub parent_id: Option<String>,
    #[serde(rename = "type")]
    pub entry_type: String,
    #[serde(default)]
    pub timestamp: Value,
    #[serde(default, flatten)]
    pub fields: Map<String, Value>,
}

impl SessionEntry {
    pub fn field(&self, name: &str) -> &Value {
        self.fields.get(name).unwrap_or(&Value::Null)
    }
    pub fn from_value(value: Value) -> Result<Self, String> {
        serde_json::from_value(value).map_err(|error| format!("Session entry decode: {error}"))
    }
    pub fn to_value(&self) -> Value {
        serde_json::to_value(self).expect("session entries contain only JSON values")
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionTreeNode {
    pub entry: SessionEntry,
    pub children: Vec<SessionTreeNode>,
    pub label: Option<String>,
    pub label_timestamp: Option<Value>,
}

#[derive(Default)]
struct Projection {
    messages: Vec<Message>,
    message_ids: Vec<String>,
    context_ids: Vec<String>,
    thinking: String,
    model: Option<LegacyModel>,
    diagnostics: Vec<String>,
}

fn summary_message(summary: &str, time: u64) -> Message {
    Message::User(UserMessage {
        content: vec![Content::text(format!(
            "{SUMMARY_PREFIX}{summary}{SUMMARY_SUFFIX}"
        ))],
        timestamp: time,
    })
}

fn validate_entries(entries: &[SessionEntry]) -> Result<BTreeMap<&str, usize>, String> {
    let mut index = BTreeMap::new();
    for (position, entry) in entries.iter().enumerate() {
        if entry.id.is_empty() || entry.entry_type.is_empty() {
            return Err("Session entry id/type must not be empty".into());
        }
        if index.insert(entry.id.as_str(), position).is_some() {
            return Err(format!("Duplicate session entry id: {}", entry.id));
        }
    }
    // Mark whole chains once: deep linear timelines must not require quadratic walks.
    let mut finished = BTreeSet::new();
    for entry in entries {
        let mut visiting = BTreeSet::new();
        let mut next = Some(entry.id.as_str());
        while let Some(id) = next {
            if finished.contains(id) {
                break;
            }
            if !visiting.insert(id) {
                return Err(format!("Cycle in session branch at {id}"));
            }
            let entry = &entries[*index
                .get(id)
                .ok_or_else(|| format!("Missing parent entry: {id}"))?];
            next = entry.parent_id.as_deref();
        }
        finished.extend(visiting);
    }
    Ok(index)
}

fn path_indices(entries: &[SessionEntry], leaf: Option<&str>) -> Result<Vec<usize>, String> {
    let index = validate_entries(entries)?;
    let mut path = Vec::new();
    let mut next = leaf;
    while let Some(id) = next {
        let position = *index
            .get(id)
            .ok_or_else(|| format!("Session entry not found: {id}"))?;
        path.push(position);
        next = entries[position].parent_id.as_deref();
    }
    path.reverse();
    Ok(path)
}

fn project(entries: &[SessionEntry], leaf: Option<&str>) -> Result<Projection, String> {
    let path = path_indices(entries, leaf)?;
    let mut projection = Projection {
        thinking: "off".into(),
        ..Projection::default()
    };
    for position in &path {
        let entry = &entries[*position];
        match entry.entry_type.as_str() {
            "thinking_level_change" => {
                projection.thinking = entry
                    .field("thinkingLevel")
                    .as_str()
                    .ok_or("Invalid thinking level entry")?
                    .into()
            }
            "model_change" => {
                projection.model = Some(LegacyModel {
                    provider: entry
                        .field("provider")
                        .as_str()
                        .ok_or("Invalid model provider entry")?
                        .into(),
                    model_id: entry
                        .field("modelId")
                        .as_str()
                        .ok_or("Invalid model identity entry")?
                        .into(),
                })
            }
            "message" => {
                let message = &entry.field("message");
                if message["role"] == "assistant" {
                    if let (Some(provider), Some(model)) =
                        (message["provider"].as_str(), message["model"].as_str())
                    {
                        projection.model = Some(LegacyModel {
                            provider: provider.into(),
                            model_id: model.into(),
                        });
                    }
                }
            }
            _ => {}
        }
    }
    let latest = path
        .iter()
        .rposition(|position| entries[*position].entry_type == "compaction");
    let context_path = if let Some(compaction_index) = latest {
        let checkpoint = &entries[path[compaction_index]];
        if let Some(active) = checkpoint.fields.get("activeMessages") {
            projection.messages = serde_json::from_value(active.clone())
                .map_err(|error| format!("Invalid checkpoint context: {error}"))?;
            projection.message_ids =
                serde_json::from_value(checkpoint.field("activeEntryIds").clone())
                    .map_err(|error| format!("Invalid checkpoint message IDs: {error}"))?;
            if projection.messages.len() != projection.message_ids.len()
                || projection.message_ids.first() != Some(&checkpoint.id)
            {
                return Err(format!("Invalid checkpoint mapping: {}", checkpoint.id));
            }
            let ancestors: BTreeSet<_> = path[..=compaction_index]
                .iter()
                .map(|position| entries[*position].id.as_str())
                .collect();
            if projection
                .message_ids
                .iter()
                .any(|id| !ancestors.contains(id.as_str()))
            {
                return Err(format!(
                    "Checkpoint references another branch: {}",
                    checkpoint.id
                ));
            }
            projection.context_ids = projection.message_ids.clone();
            path[compaction_index + 1..].to_vec()
        } else {
            let first_kept = checkpoint
                .field("firstKeptEntryId")
                .as_str()
                .ok_or("Missing compaction firstKeptEntryId")?;
            let start = if compaction_index == 0 {
                0
            } else {
                path[..compaction_index]
                    .iter()
                    .position(|position| entries[*position].id == first_kept)
                    .ok_or_else(|| format!("Invalid compaction firstKeptEntryId: {first_kept}"))?
            };
            let mut context_path = vec![path[compaction_index]];
            context_path.extend_from_slice(&path[start..compaction_index]);
            context_path.extend_from_slice(&path[compaction_index + 1..]);
            context_path
        }
    } else {
        path
    };
    for position in context_path {
        let entry = &entries[position];
        projection.context_ids.push(entry.id.clone());
        let message = if entry.fields.get("native") == Some(&Value::Bool(true))
            && entry.entry_type == "message"
        {
            Some(
                serde_json::from_value(entry.field("message").clone())
                    .map_err(|error| format!("Invalid native message {}: {error}", entry.id))?,
            )
        } else {
            context_message(&entry.to_value(), &mut projection.diagnostics)?
        };
        if let Some(message) = message {
            projection.messages.push(message);
            projection.message_ids.push(entry.id.clone());
        }
    }
    Ok(projection)
}

/// A target may end in a user turn, but not halfway through a tool exchange.
pub fn validate_tool_pairs(messages: &[Message]) -> Result<(), String> {
    let mut pending: BTreeMap<&str, usize> = BTreeMap::new();
    for message in messages {
        match message {
            Message::Assistant(assistant) => {
                if !pending.is_empty() {
                    return Err("Unresolved tool calls before assistant message".into());
                }
                for block in &assistant.content {
                    if let Content::ToolCall(call) = block {
                        if call.id.is_empty() || pending.insert(&call.id, 1).is_some() {
                            return Err(format!("Duplicate or empty tool call id: {}", call.id));
                        }
                    }
                }
            }
            Message::ToolResult(result) => {
                if pending.remove(result.tool_call_id.as_str()).is_none() {
                    return Err(format!("Orphan tool result: {}", result.tool_call_id));
                }
            }
            Message::User(_) if !pending.is_empty() => {
                return Err("Unresolved tool calls before user message".into())
            }
            _ => {}
        }
    }
    if !pending.is_empty() {
        return Err(format!(
            "Unresolved tool calls: {}",
            pending.keys().copied().collect::<Vec<_>>().join(", ")
        ));
    }
    Ok(())
}

impl Session {
    pub(crate) fn refresh_tree_context(&mut self) -> Result<(), String> {
        let projection = project(&self.entries, self.leaf_id.as_deref())?;
        self.apply_projection(projection);
        Ok(())
    }
    fn append_raw(&mut self, entry_type: &str, fields: Map<String, Value>, time: u64) -> String {
        let mut id = if self.tree_version == 0 {
            format!("native-{}-{}", self.id, self.entries.len())
        } else {
            format!("entry-{}", Session::default().id)
        };
        while self.entries.iter().any(|entry| entry.id == id) {
            id = format!("entry-{}", Session::default().id);
        }
        self.entries.push(SessionEntry {
            id: id.clone(),
            parent_id: self.leaf_id.clone(),
            entry_type: entry_type.into(),
            timestamp: json!(time),
            fields,
        });
        self.leaf_id = Some(id.clone());
        id
    }
    fn append_native(&mut self, message: Message) -> Result<String, String> {
        let time = match &message {
            Message::User(message) => message.timestamp,
            Message::Assistant(message) => message.timestamp,
            Message::ToolResult(message) => message.timestamp,
            Message::Custom { timestamp, .. } => *timestamp,
        };
        let mut fields = Map::new();
        fields.insert("native".into(), Value::Bool(true));
        fields.insert(
            "message".into(),
            serde_json::to_value(&message).map_err(|error| error.to_string())?,
        );
        let id = self.append_raw("message", fields, time);
        self.context.messages.push(message);
        Ok(id)
    }
    fn apply_projection(&mut self, projection: Projection) {
        self.context.messages = projection.messages;
        if let Some(imported) = &mut self.imported {
            imported.selected_leaf_id = self.leaf_id.clone();
            imported.selected_entry_ids = path_indices(&self.entries, self.leaf_id.as_deref())
                .unwrap_or_default()
                .iter()
                .map(|position| self.entries[*position].id.clone())
                .collect();
            imported.context_entry_ids = projection.context_ids;
            imported.thinking_level = projection.thinking;
            imported.model = projection.model;
            for diagnostic in projection.diagnostics {
                if !imported.diagnostics.contains(&diagnostic) {
                    imported.diagnostics.push(diagnostic);
                }
            }
        }
    }
    /// Upgrade old flat snapshots lazily, including all imported sibling branches.
    /// Prefix-only writes from older Rust clients are incorporated without discarding entries.
    pub fn ensure_tree(&mut self) -> Result<(), String> {
        let mut candidate = self.clone();
        candidate.hydrate_tree()?;
        *self = candidate;
        Ok(())
    }
    fn hydrate_tree(&mut self) -> Result<(), String> {
        let migrating = self.tree_version == 0;
        if self.tree_version > TREE_VERSION {
            return Err(format!(
                "Unsupported session tree version: {}",
                self.tree_version
            ));
        }
        if self.tree_version == 0 {
            if !self.entries.is_empty() || self.leaf_id.is_some() {
                return Err("Session tree has entries without a version".into());
            }
            let active = self.context.messages.clone();
            if let Some(imported) = &self.imported {
                let lines = imported
                    .records
                    .iter()
                    .map(Value::to_string)
                    .collect::<Vec<_>>()
                    .join("\n");
                let source = parse_legacy_session(&lines, imported.source_path.clone())?;
                self.entries = source
                    .migrated_entries()
                    .iter()
                    .cloned()
                    .map(SessionEntry::from_value)
                    .collect::<Result<_, _>>()?;
                self.leaf_id = imported.selected_leaf_id.clone();
                self.context.messages = project(&self.entries, self.leaf_id.as_deref())?.messages;
                // Older Rust imports stored native checkpoints separately from
                // the source JSONL tree. Attach these without losing siblings,
                // already-kept source entries, or native messages after import.
                for record in self.compactions.clone() {
                    if record.archived_messages.starts_with(&self.context.messages) {
                        let extra =
                            record.archived_messages[self.context.messages.len()..].to_vec();
                        for message in extra {
                            self.append_native(message)?;
                        }
                    } else if !self.context.messages.starts_with(&record.archived_messages) {
                        return Err(
                            "Imported compaction archive diverges from selected source branch"
                                .into(),
                        );
                    }
                    let projection = project(&self.entries, self.leaf_id.as_deref())?;
                    let cut = record.archived_messages.len();
                    let mut messages = vec![summary_message(&record.summary, record.timestamp)];
                    messages.extend_from_slice(&self.context.messages[cut..]);
                    let mut fields = Map::new();
                    fields.insert("summary".into(), json!(record.summary));
                    fields.insert("archiveId".into(), json!(record.id));
                    fields.insert("activeMessages".into(), json!(messages));
                    let id = self.append_raw("compaction", fields, record.timestamp);
                    let mut ids = vec![id];
                    ids.extend_from_slice(&projection.message_ids[cut..]);
                    self.entries
                        .last_mut()
                        .expect("appended checkpoint")
                        .fields
                        .insert("activeEntryIds".into(), json!(ids));
                    self.context.messages = messages;
                }
                if !active.starts_with(&self.context.messages) {
                    return Err(
                        "Imported snapshot context diverges from selected original branch".into(),
                    );
                }
                self.context.messages = active.clone();
            } else {
                self.context.messages.clear();
                let mut previous_summary = None;
                for record in self.compactions.clone() {
                    for (position, message) in record.archived_messages.iter().enumerate() {
                        if position == 0
                            && previous_summary
                                .as_ref()
                                .is_some_and(|summary| message.text() == *summary)
                        {
                            continue;
                        }
                        self.append_native(message.clone())?;
                    }
                    let summary = summary_message(&record.summary, record.timestamp);
                    let mut fields = Map::new();
                    fields.insert("summary".into(), json!(record.summary));
                    fields.insert("archiveId".into(), json!(record.id));
                    fields.insert("activeMessages".into(), json!([summary.clone()]));
                    let id = self.append_raw("compaction", fields, record.timestamp);
                    self.entries
                        .last_mut()
                        .expect("appended checkpoint")
                        .fields
                        .insert("activeEntryIds".into(), json!([id]));
                    self.context.messages = vec![summary.clone()];
                    previous_summary = Some(summary.text());
                }
                for (position, message) in active.iter().enumerate() {
                    if position == 0
                        && previous_summary
                            .as_ref()
                            .is_some_and(|summary| message.text() == *summary)
                    {
                        continue;
                    }
                    self.append_native(message.clone())?;
                }
                self.context.messages = active.clone();
            }
        }
        let projection = project(&self.entries, self.leaf_id.as_deref())?;
        if self.context.messages == projection.messages {
            if migrating {
                self.tree_version = TREE_VERSION;
            }
            return Ok(());
        }
        if !self.context.messages.starts_with(&projection.messages) {
            return Err("Session context diverges from selected tree branch".into());
        }
        let extras = self.context.messages[projection.messages.len()..].to_vec();
        self.context.messages = projection.messages;
        for message in extras {
            self.append_native(message)?;
        }
        if migrating {
            self.tree_version = TREE_VERSION;
        }
        Ok(())
    }
    pub fn append_message(&mut self, message: Message) -> Result<String, String> {
        self.ensure_tree()?;
        let id = self.append_native(message)?;
        self.updated_at = timestamp();
        Ok(id)
    }
    /// Display names are global session metadata, not active-branch LLM messages.
    /// The original store appends every explicit set, including an empty clear.
    pub fn append_session_info(&mut self, name: &str) -> Result<String, String> {
        let mut candidate = self.clone();
        candidate.ensure_tree()?;
        let name = agent_extensions::normalize_session_name(name);
        let fields = [("name".into(), json!(name))].into_iter().collect();
        let id = candidate.append_raw("session_info", fields, timestamp());
        candidate.name = name;
        candidate.updated_at = timestamp();
        *self = candidate;
        Ok(id)
    }
    pub fn get_session_name(&self) -> Result<Option<String>, String> {
        let entries = self.get_entries()?;
        for entry in entries.iter().rev() {
            if entry.entry_type == "session_info" {
                let name = entry
                    .field("name")
                    .as_str()
                    .ok_or("Invalid session name entry")?;
                let name = agent_extensions::normalize_session_name(name);
                return Ok((!name.is_empty()).then_some(name));
            }
        }
        Ok(None)
    }
    /// Thinking is branch-local, unlike the global session name.
    pub fn get_thinking_level(&self) -> Result<Option<ThinkingLevel>, String> {
        self.get_branch(None)?
            .iter()
            .rev()
            .find(|entry| entry.entry_type == "thinking_level_change")
            .map(|entry| {
                entry
                    .field("thinkingLevel")
                    .as_str()
                    .ok_or("Invalid thinking level entry")?
                    .parse()
            })
            .transpose()
    }
    pub fn append_thinking_level(
        &mut self,
        level: ThinkingLevel,
    ) -> Result<Option<String>, String> {
        if self.get_thinking_level()? == Some(level) {
            return Ok(None);
        }
        let mut candidate = self.clone();
        candidate.ensure_tree()?;
        let fields = [("thinkingLevel".into(), json!(level))]
            .into_iter()
            .collect();
        let id = candidate.append_raw("thinking_level_change", fields, timestamp());
        candidate.refresh_tree_context()?;
        candidate.updated_at = timestamp();
        *self = candidate;
        Ok(Some(id))
    }
    pub fn record_runtime_state(&mut self, key: &str, data: Value) -> Result<String, String> {
        if key.trim().is_empty() {
            return Err("Runtime state key must not be empty".into());
        }
        self.ensure_tree()?;
        let fields = [("key".into(), json!(key)), ("data".into(), data)]
            .into_iter()
            .collect();
        let id = self.append_raw("runtime_state", fields, timestamp());
        self.updated_at = timestamp();
        Ok(id)
    }
    pub fn runtime_state(&self, key: &str) -> Result<Option<Value>, String> {
        if key.trim().is_empty() {
            return Err("Runtime state key must not be empty".into());
        }
        Ok(self
            .get_branch(None)?
            .into_iter()
            .rev()
            .find(|entry| {
                entry.entry_type == "runtime_state" && entry.field("key").as_str() == Some(key)
            })
            .map(|entry| entry.field("data").clone()))
    }
    pub fn record_compaction(
        &mut self,
        record: NativeCompaction,
        active_messages: Vec<Message>,
    ) -> Result<String, String> {
        let mut candidate = self.clone();
        candidate.ensure_tree()?;
        if record.id.is_empty()
            || record.summary.trim().is_empty()
            || candidate
                .compactions
                .iter()
                .any(|existing| existing.id == record.id)
        {
            return Err("Invalid or duplicate compaction record".into());
        }
        if !candidate
            .context
            .messages
            .starts_with(&record.archived_messages)
        {
            return Err("Compaction archive is not an active context prefix".into());
        }
        let kept = &candidate.context.messages[record.archived_messages.len()..];
        if kept.len() != record.kept_message_count
            || active_messages.len() != kept.len() + 1
            || active_messages[1..] != *kept
            || active_messages[0] != summary_message(&record.summary, record.timestamp)
        {
            return Err("Compaction context does not match archive and kept messages".into());
        }
        validate_tool_pairs(&record.archived_messages)?;
        validate_tool_pairs(&active_messages)?;
        let projection = project(&candidate.entries, candidate.leaf_id.as_deref())?;
        let mut fields = Map::new();
        fields.insert("summary".into(), json!(record.summary));
        fields.insert("archiveId".into(), json!(record.id));
        fields.insert("activeMessages".into(), json!(active_messages));
        fields.insert("usage".into(), json!(record.usage));
        if let Some(id) = projection.message_ids.get(record.archived_messages.len()) {
            fields.insert("firstKeptEntryId".into(), json!(id));
        }
        let id = candidate.append_raw("compaction", fields, record.timestamp);
        let mut active_ids = vec![id.clone()];
        active_ids.extend_from_slice(&projection.message_ids[record.archived_messages.len()..]);
        candidate
            .entries
            .last_mut()
            .expect("appended checkpoint")
            .fields
            .insert("activeEntryIds".into(), json!(active_ids));
        candidate.context.messages = active_messages;
        candidate.compactions.push(record);
        candidate.updated_at = timestamp();
        let projection = project(&candidate.entries, candidate.leaf_id.as_deref())?;
        candidate.apply_projection(projection);
        *self = candidate;
        Ok(id)
    }
    pub fn get_entries(&self) -> Result<Vec<SessionEntry>, String> {
        let mut snapshot = self.clone();
        snapshot.ensure_tree()?;
        Ok(snapshot.entries)
    }
    pub fn get_leaf_id(&self) -> Result<Option<String>, String> {
        let mut snapshot = self.clone();
        snapshot.ensure_tree()?;
        Ok(snapshot.leaf_id)
    }
    pub fn get_branch(&self, leaf: Option<&str>) -> Result<Vec<SessionEntry>, String> {
        let mut snapshot = self.clone();
        snapshot.ensure_tree()?;
        let indices = path_indices(&snapshot.entries, leaf.or(snapshot.leaf_id.as_deref()))?;
        Ok(indices
            .into_iter()
            .map(|position| snapshot.entries[position].clone())
            .collect())
    }
    pub fn navigate(&mut self, target: Option<&str>) -> Result<(), String> {
        let mut candidate = self.clone();
        candidate.ensure_tree()?;
        let projection = project(&candidate.entries, target)?;
        validate_tool_pairs(&projection.messages)?;
        candidate.leaf_id = target.map(str::to_owned);
        candidate.apply_projection(projection);
        candidate.updated_at = timestamp();
        *self = candidate;
        Ok(())
    }
    pub fn fork(&mut self, user_entry_id: &str) -> Result<String, String> {
        let mut candidate = self.clone();
        candidate.ensure_tree()?;
        let entry = candidate
            .entries
            .iter()
            .find(|entry| entry.id == user_entry_id)
            .ok_or_else(|| format!("Session entry not found: {user_entry_id}"))?;
        if entry.entry_type != "message" || entry.field("message")["role"] != "user" {
            return Err("Fork target must be a user message".into());
        }
        let value = &entry.field("message")["content"];
        let text = if let Some(text) = value.as_str() {
            text.to_owned()
        } else {
            value
                .as_array()
                .ok_or("Invalid user message content")?
                .iter()
                .filter_map(|part| part["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n")
        };
        let parent = entry.parent_id.clone();
        candidate.navigate(parent.as_deref())?;
        *self = candidate;
        Ok(text)
    }
    pub fn clone_current_branch(&self) -> Result<Session, String> {
        let mut clone = self.clone();
        clone.ensure_tree()?;
        validate_tool_pairs(&clone.context.messages)?;
        // Labels are global bookmarks, but are also real parent-chain entries.
        // Resolve them before extracting a branch, then re-chain and recreate only
        // bookmarks whose targets survived, matching createBranchedSession().
        let mut labels: BTreeMap<String, SessionEntry> = BTreeMap::new();
        for entry in &clone.entries {
            if entry.entry_type == "label" {
                let target = entry
                    .field("targetId")
                    .as_str()
                    .ok_or("Invalid label target")?;
                if entry
                    .field("label")
                    .as_str()
                    .is_some_and(|label| !label.is_empty())
                {
                    labels.insert(target.into(), entry.clone());
                } else {
                    labels.remove(target);
                }
            }
        }
        let branch = clone.get_branch(None)?;
        let latest_name = clone
            .entries
            .iter()
            .rev()
            .find(|entry| entry.entry_type == "session_info")
            .cloned();
        let mut retained = Vec::new();
        let mut parent = None;
        let mut pending_labels = Vec::new();
        let mut replacements: BTreeMap<String, String> = BTreeMap::new();
        for mut entry in branch {
            if entry.entry_type == "label" {
                pending_labels.push(entry.id);
                continue;
            }
            for label in pending_labels.drain(..) {
                replacements.insert(label, entry.id.clone());
            }
            entry.parent_id = parent;
            if let Some(id) = entry
                .field("firstKeptEntryId")
                .as_str()
                .and_then(|id| replacements.get(id))
                .cloned()
            {
                entry.fields.insert("firstKeptEntryId".into(), json!(id));
            }
            parent = Some(entry.id.clone());
            retained.push(entry);
        }
        clone.entries = retained;
        clone.leaf_id = parent;
        let retained_ids: BTreeSet<_> =
            clone.entries.iter().map(|entry| entry.id.clone()).collect();
        for (target, entry) in labels {
            if retained_ids.contains(&target) {
                clone.append_raw("label", entry.fields, timestamp());
                clone.entries.last_mut().expect("appended label").timestamp = entry.timestamp;
            }
        }
        if let Some(mut entry) = latest_name.filter(|entry| !retained_ids.contains(&entry.id)) {
            entry.parent_id = clone.leaf_id.clone();
            clone.leaf_id = Some(entry.id.clone());
            clone.entries.push(entry);
        }
        let parent_id = clone.id.clone();
        clone.id = Session::default().id;
        clone.updated_at = timestamp();
        clone
            .metadata
            .insert("parentSessionId".into(), json!(parent_id));
        let projection = project(&clone.entries, clone.leaf_id.as_deref())?;
        clone.apply_projection(projection);
        Ok(clone)
    }
    pub fn get_tree(&self) -> Result<Vec<SessionTreeNode>, String> {
        let entries = self.get_entries()?;
        let index = validate_entries(&entries)?;
        let mut children: BTreeMap<Option<&str>, Vec<usize>> = BTreeMap::new();
        let mut labels: BTreeMap<&str, (String, Value)> = BTreeMap::new();
        for (position, entry) in entries.iter().enumerate() {
            children
                .entry(entry.parent_id.as_deref())
                .or_default()
                .push(position);
            if entry.entry_type == "label" {
                if let Some(target) = entry.field("targetId").as_str() {
                    if !index.contains_key(target) {
                        return Err(format!("Invalid label target: {target}"));
                    }
                    if let Some(label) = entry
                        .field("label")
                        .as_str()
                        .filter(|label| !label.is_empty())
                    {
                        labels.insert(target, (label.into(), entry.timestamp.clone()));
                    } else {
                        labels.remove(target);
                    }
                }
            }
        }
        for positions in children.values_mut() {
            positions.sort_by(|a, b| {
                entry_time(&entries[*a])
                    .cmp(&entry_time(&entries[*b]))
                    .then(a.cmp(b))
            });
        }
        let roots = children.get(&None).cloned().unwrap_or_default();
        let mut built: BTreeMap<usize, SessionTreeNode> = BTreeMap::new();
        let mut stack: Vec<_> = roots
            .iter()
            .rev()
            .map(|position| (*position, false))
            .collect();
        while let Some((position, visited)) = stack.pop() {
            let descendants = children
                .get(&Some(entries[position].id.as_str()))
                .cloned()
                .unwrap_or_default();
            if !visited {
                stack.push((position, true));
                stack.extend(descendants.into_iter().rev().map(|child| (child, false)));
                continue;
            }
            let label = labels.get(entries[position].id.as_str());
            let node = SessionTreeNode {
                entry: entries[position].clone(),
                children: descendants
                    .into_iter()
                    .map(|child| built.remove(&child).expect("child visited before parent"))
                    .collect(),
                label: label.map(|(label, _)| label.clone()),
                label_timestamp: label.map(|(_, time)| time.clone()),
            };
            built.insert(position, node);
        }
        Ok(roots
            .into_iter()
            .map(|position| built.remove(&position).expect("root visited"))
            .collect())
    }
}

fn entry_time(entry: &SessionEntry) -> u64 {
    entry
        .timestamp
        .as_u64()
        .or_else(|| entry.timestamp.as_str().and_then(parse_iso_timestamp))
        .unwrap_or_default()
}

/// Resumable active branch, with a v3 header and original raw payloads intact.
/// Native checkpoints carry their exact projection as well as firstKeptEntryId.
pub fn export_branch_jsonl(session: &Session, path: &Path) -> Result<(), String> {
    let mut lines=vec![json!({"type":"session","version":3,"id":session.id,"timestamp":timestamp(),"cwd":session.workspace,"nativeTreeVersion":TREE_VERSION}).to_string()];
    lines.extend(
        session
            .get_branch(None)?
            .iter()
            .map(|entry| entry.to_value().to_string()),
    );
    // Archives may be stored in an off-branch export-only record; retain them as a tail.
    let branch_ids: BTreeSet<_> = session
        .get_branch(None)?
        .into_iter()
        .map(|entry| entry.id)
        .collect();
    let mut parent = session.get_leaf_id()?;
    let entries = session.get_entries()?;
    let latest_name = entries
        .iter()
        .rev()
        .find(|entry| entry.entry_type == "session_info")
        .map(|entry| entry.id.clone());
    for mut entry in entries.into_iter().filter(|entry| {
        !branch_ids.contains(&entry.id)
            && (latest_name.as_ref() == Some(&entry.id)
                || (entry.entry_type == "custom"
                    && entry.fields.get("customType") == Some(&json!("export:context-archives"))))
    }) {
        entry.parent_id = parent;
        parent = Some(entry.id.clone());
        lines.push(entry.to_value().to_string());
    }
    fs::write(path, format!("{}\n", lines.join("\n"))).map_err(|error| error.to_string())
}
