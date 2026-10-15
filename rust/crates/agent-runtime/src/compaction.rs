//! Transactional conversation checkpoints shared by explicit and opt-in automatic compaction.

use crate::session::Session;
use agent_core::agent_loop::Provider;
use agent_core::types::{
    timestamp, CancellationToken, Content, Context, Message, StopReason, Usage, UserMessage,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::panic::{catch_unwind, AssertUnwindSafe};

const SUMMARY_PREFIX: &str = "The conversation history before this point was compacted into the following summary:\n\n<summary>\n";
const SUMMARY_SUFFIX: &str = "\n</summary>";
const SUMMARY_SYSTEM_PROMPT: &str = "You are a context summarization assistant. Read the provided conversation and produce a structured checkpoint for another assistant. Do NOT continue the conversation or answer its questions. Do NOT call tools. Only output the summary.";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NativeCompaction {
    pub id: String,
    pub timestamp: u64,
    pub summary: String,
    pub archived_messages: Vec<Message>,
    pub kept_message_count: usize,
    pub usage: Option<Usage>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CompactionResult {
    pub id: String,
    pub summary: String,
    pub archived_message_count: usize,
    pub kept_message_count: usize,
    pub usage: Option<Usage>,
}

#[derive(Clone, Debug)]
pub struct CompactionPreparation {
    pub archived_messages: Vec<Message>,
    pub kept_messages: Vec<Message>,
}

pub fn active_checkpoint(session: &Session) -> Result<Option<(String, u64)>, String> {
    let branch = session.get_branch(None)?;
    let Some(summary) = branch
        .iter()
        .rev()
        .find(|entry| entry.entry_type == "compaction")
        .and_then(|entry| entry.field("summary").as_str())
    else {
        return Ok(None);
    };
    let Some(Message::User(first)) = session.context.messages.first() else {
        return Ok(None);
    };
    let expected = format!("{SUMMARY_PREFIX}{summary}{SUMMARY_SUFFIX}");
    Ok((session.context.messages[0].text() == expected)
        .then(|| (summary.to_owned(), first.timestamp)))
}

/// Pure cut-point calculation. No provider request or persistence occurs here.
pub fn prepare_compaction(
    session: &Session,
    keep_recent_user_turns: usize,
) -> Result<CompactionPreparation, String> {
    let messages = &session.context.messages;
    let synthetic_first = active_checkpoint(session)?.is_some();
    let user_starts: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter_map(|(index, message)| {
            (matches!(message, Message::User(_)) && !(synthetic_first && index == 0))
                .then_some(index)
        })
        .collect();
    let keep = keep_recent_user_turns.max(1);
    let mut cut = if user_starts.len() > keep {
        user_starts[user_starts.len() - keep]
    } else {
        0
    };

    // Match results to their closest preceding pending call. Calls and results may
    // straddle a nonstandard imported user boundary; keep that entire earlier turn.
    let mut pending: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    let mut pairs = vec![];
    for (index, message) in messages.iter().enumerate() {
        match message {
            Message::Assistant(assistant) => {
                for part in &assistant.content {
                    if let Content::ToolCall(call) = part {
                        pending.entry(&call.id).or_default().push(index);
                    }
                }
            }
            Message::ToolResult(result) => {
                let call = pending
                    .get_mut(result.tool_call_id.as_str())
                    .and_then(Vec::pop)
                    .ok_or_else(|| {
                        format!("Tool result has no preceding call: {}", result.tool_call_id)
                    })?;
                pairs.push((call, index));
            }
            _ => {}
        }
    }
    loop {
        let crossing = pairs
            .iter()
            .filter(|(call, result)| *call < cut && *result >= cut)
            .map(|(call, _)| *call)
            .min();
        let Some(call) = crossing else {
            break;
        };
        cut = user_starts
            .iter()
            .copied()
            .filter(|start| *start <= call)
            .next_back()
            .unwrap_or(0);
    }
    if pending.values().flatten().any(|index| *index < cut) {
        return Err("Compaction would archive an unfinished tool call".into());
    }
    if pending.values().any(|calls| !calls.is_empty()) {
        return Err("Compaction requires a complete boundary with no unfinished tool calls".into());
    }
    if cut == 0 || (synthetic_first && cut == 1) {
        return Err("No completed history is eligible for compaction".into());
    }
    Ok(CompactionPreparation {
        archived_messages: messages[..cut].to_vec(),
        kept_messages: messages[cut..].to_vec(),
    })
}

/// At least the newest complete user turn survives, even when `keep_recent_user_turns` is zero.
/// Provider failure, cancellation, truncated/empty output, and attempted tools leave `session` unchanged.
pub fn compact_session(
    session: &mut Session,
    provider: &mut dyn Provider,
    keep_recent_user_turns: usize,
    signal: &CancellationToken,
) -> Result<CompactionResult, String> {
    if signal.is_cancelled() {
        return Err("Compaction aborted".into());
    }
    let CompactionPreparation {
        archived_messages,
        kept_messages,
    } = prepare_compaction(session, keep_recent_user_turns)?;
    let conversation = serialize_conversation(&archived_messages);
    let prompt = format!("<conversation>\n{conversation}\n</conversation>\n\nCreate a concise checkpoint with these headings: ## Goal, ## Constraints & Preferences, ## Progress (Done / In Progress / Blocked), ## Key Decisions, ## Next Steps, ## Critical Context. Preserve exact paths, function names, errors, and unresolved work. Existing summary messages are prior checkpoints: consolidate their important facts with the newer progress.");
    let request = Context {
        system_prompt: SUMMARY_SYSTEM_PROMPT.into(),
        messages: vec![Message::user(prompt)],
    };
    let response = catch_unwind(AssertUnwindSafe(|| {
        provider.complete_summary(&request, signal, &mut |_| {})
    }))
    .map_err(|_| "Summarization provider panicked".to_owned())??;
    if signal.is_cancelled() || response.stop_reason == StopReason::Aborted {
        return Err("Compaction aborted".into());
    }
    match response.stop_reason {
        StopReason::Error => {
            return Err(format!(
                "Summarization failed: {}",
                response.error_message.as_deref().unwrap_or("Unknown error")
            ))
        }
        StopReason::Length => {
            return Err("Summarization hit the token cap; checkpoint is incomplete".into())
        }
        StopReason::ToolUse => return Err("Summarization attempted to call a tool".into()),
        _ => {}
    }
    if !response.tool_calls().is_empty() {
        return Err("Summarization attempted to call a tool".into());
    }
    let mut summary = response
        .content
        .iter()
        .filter_map(|part| match part {
            Content::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_owned();
    if summary.is_empty() {
        return Err("Summarization returned an empty checkpoint".into());
    }

    // Preserve the same read-versus-modified file distinction as the TS checkpoint.
    let mut read_files = BTreeSet::new();
    let mut modified_files = BTreeSet::new();
    for message in session
        .compactions
        .iter()
        .flat_map(|compaction| &compaction.archived_messages)
        .chain(&archived_messages)
    {
        if let Message::Assistant(assistant) = message {
            for call in assistant.tool_calls() {
                if let Some(path) = call.arguments["path"].as_str() {
                    match call.name.as_str() {
                        "read" => {
                            read_files.insert(path.to_owned());
                        }
                        "write" | "edit" => {
                            modified_files.insert(path.to_owned());
                        }
                        _ => {}
                    }
                }
            }
        }
    }
    read_files.retain(|path| !modified_files.contains(path));
    if !read_files.is_empty() {
        summary.push_str(&format!(
            "\n\n<read-files>\n{}\n</read-files>",
            read_files.into_iter().collect::<Vec<_>>().join("\n")
        ));
    }
    if !modified_files.is_empty() {
        summary.push_str(&format!(
            "\n\n<modified-files>\n{}\n</modified-files>",
            modified_files.into_iter().collect::<Vec<_>>().join("\n")
        ));
    }
    if signal.is_cancelled() {
        return Err("Compaction aborted".into());
    }
    let id = format!("compaction-{}", Session::default().id);
    let time = timestamp();
    let result = CompactionResult {
        id: id.clone(),
        summary: summary.clone(),
        archived_message_count: archived_messages.len(),
        kept_message_count: kept_messages.len(),
        usage: response.usage.clone(),
    };
    let mut active_messages = vec![Message::User(UserMessage {
        content: vec![Content::text(format!(
            "{SUMMARY_PREFIX}{summary}{SUMMARY_SUFFIX}"
        ))],
        timestamp: time,
    })];
    active_messages.extend(kept_messages);
    session.record_compaction(
        NativeCompaction {
            id,
            timestamp: time,
            summary,
            archived_messages,
            kept_message_count: result.kept_message_count,
            usage: response.usage,
        },
        active_messages,
    )?;
    Ok(result)
}

pub fn serialize_conversation(messages: &[Message]) -> String {
    let mut sections = vec![];
    for message in messages {
        match message {
            Message::User(_) => {
                let text = message.text();
                if !text.is_empty() {
                    sections.push(format!("[User]: {text}"));
                }
            }
            Message::Assistant(assistant) => {
                let mut thoughts = vec![];
                let mut calls = vec![];
                for part in &assistant.content {
                    match part {
                        Content::Thinking { thinking } => thoughts.push(thinking.as_str()),
                        Content::ToolCall(call) => {
                            calls.push(format!("{}({})", call.name, call.arguments))
                        }
                        _ => {}
                    }
                }
                if !thoughts.is_empty() {
                    sections.push(format!("[Assistant thinking]: {}", thoughts.join("\n")));
                }
                let text = message.text();
                if !text.is_empty() {
                    sections.push(format!("[Assistant]: {text}"));
                }
                if !calls.is_empty() {
                    sections.push(format!("[Assistant tool calls]: {}", calls.join("; ")));
                }
            }
            Message::ToolResult(_) => {
                let text = message.text();
                let count = text.chars().count();
                if count > 2000 {
                    sections.push(format!(
                        "[Tool result]: {}\n\n[... {} more characters truncated]",
                        text.chars().take(2000).collect::<String>(),
                        count - 2000
                    ));
                } else if !text.is_empty() {
                    sections.push(format!("[Tool result]: {text}"));
                }
            }
            Message::Custom { .. } => {}
        }
    }
    sections.join("\n\n")
}
