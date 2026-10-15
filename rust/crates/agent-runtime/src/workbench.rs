//! Read-only task projections shared by native clients. No filesystem, provider,
//! credential access or second source of session truth lives in this module.
use crate::session::Session;
use agent_core::types::{Content, Message, StopReason};
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TaskPhase {
    #[default]
    Draft,
    Running,
    Waiting,
    Completed,
    Interrupted,
    Failed,
    Limited,
}

impl TaskPhase {
    pub fn from_messages(messages: &[Message], running: bool) -> Self {
        if running {
            return Self::Running;
        }
        messages
            .iter()
            .rev()
            .find_map(|message| match message {
                Message::User(_) | Message::ToolResult(_) => Some(Self::Waiting),
                Message::Assistant(assistant) => Some(match assistant.stop_reason {
                    StopReason::Stop => Self::Completed,
                    StopReason::ToolUse => Self::Waiting,
                    StopReason::Length => Self::Limited,
                    StopReason::Error => Self::Failed,
                    StopReason::Aborted => Self::Interrupted,
                }),
                Message::Custom { .. } => None,
            })
            .unwrap_or(Self::Draft)
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Draft => "未开始",
            Self::Running => "进行中",
            Self::Waiting => "待继续",
            Self::Completed => "已完成",
            Self::Interrupted => "已停止",
            Self::Failed => "出现错误",
            Self::Limited => "输出已达上限",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StepOutcome {
    Unfinished,
    Succeeded,
    Failed,
    Unmatched,
}

/// Indices refer to the message slice used to build this projection. They are not
/// persistent IDs, executable paths, or evidence that a file still exists.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecutionStep {
    pub tool_name: String,
    pub call_message: Option<usize>,
    pub result_message: Option<usize>,
    pub outcome: StepOutcome,
    pub path: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordedArtifact {
    pub path: String,
    pub operation: String,
    pub result_message: usize,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TaskProjection {
    pub phase: TaskPhase,
    pub user_turns: usize,
    pub steps: Vec<ExecutionStep>,
    pub artifacts: Vec<RecordedArtifact>,
}

impl TaskProjection {
    pub fn from_session(session: &Session, running: bool) -> Self {
        Self::from_messages(&session.context.messages, running)
    }

    pub fn from_messages(messages: &[Message], running: bool) -> Self {
        let mut projection = Self::default();
        let mut pending: BTreeMap<(&str, &str), Vec<usize>> = BTreeMap::new();
        for (index, message) in messages.iter().enumerate() {
            match message {
                Message::User(_) => {
                    projection.user_turns += 1;
                    // A result in another user turn must not attach to an old call.
                    pending.clear();
                }
                Message::Assistant(assistant) => {
                    for block in &assistant.content {
                        if let Content::ToolCall(call) = block {
                            pending
                                .entry((&call.id, &call.name))
                                .or_default()
                                .push(projection.steps.len());
                            projection.steps.push(ExecutionStep {
                                tool_name: call.name.clone(),
                                call_message: Some(index),
                                result_message: None,
                                outcome: StepOutcome::Unfinished,
                                path: call
                                    .arguments
                                    .get("path")
                                    .and_then(|p| p.as_str())
                                    .filter(|p| {
                                        !p.is_empty()
                                            && p.len() <= 4096
                                            && !p.chars().any(char::is_control)
                                    })
                                    .map(str::to_owned),
                            });
                        }
                    }
                }
                Message::ToolResult(result) => {
                    let candidates =
                        pending.remove(&(result.tool_call_id.as_str(), result.tool_name.as_str()));
                    if let Some(candidates) = candidates.filter(|items| items.len() == 1) {
                        let step = &mut projection.steps[candidates[0]];
                        step.result_message = Some(index);
                        step.outcome = if result.is_error {
                            StepOutcome::Failed
                        } else {
                            StepOutcome::Succeeded
                        };
                        if !result.is_error && matches!(step.tool_name.as_str(), "write" | "edit") {
                            if let Some(path) = &step.path {
                                projection.artifacts.push(RecordedArtifact {
                                    path: path.clone(),
                                    operation: step.tool_name.clone(),
                                    result_message: index,
                                });
                            }
                        }
                    } else {
                        projection.steps.push(ExecutionStep {
                            tool_name: result.tool_name.clone(),
                            call_message: None,
                            result_message: Some(index),
                            outcome: StepOutcome::Unmatched,
                            path: None,
                        });
                    }
                }
                Message::Custom { .. } => {}
            }
        }
        projection.phase = TaskPhase::from_messages(messages, running);
        projection
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TaskFilter {
    #[default]
    All,
    Completed,
    Attention,
}

impl TaskFilter {
    pub const fn matches(self, phase: TaskPhase) -> bool {
        match self {
            Self::All => true,
            Self::Completed => matches!(phase, TaskPhase::Completed),
            Self::Attention => matches!(
                phase,
                TaskPhase::Waiting
                    | TaskPhase::Interrupted
                    | TaskPhase::Failed
                    | TaskPhase::Limited
            ),
        }
    }
}

/// Searches names and workspace paths only: no hidden scan of message bodies or
/// private credential/configuration files. Whitespace-only queries match all.
pub fn matches_query(session: &Session, query: &str) -> bool {
    let query = query.trim().to_lowercase();
    query.is_empty()
        || session.name.to_lowercase().contains(&query)
        || session
            .workspace
            .to_string_lossy()
            .to_lowercase()
            .contains(&query)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HistoryError {
    InvalidBranch,
    InvalidMessage,
}

impl std::fmt::Display for HistoryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::InvalidBranch => "分支历史校验失败，请检查会话记录后刷新。",
            Self::InvalidMessage => "历史消息解析失败，原始会话未修改。",
        })
    }
}
impl std::error::Error for HistoryError {}

/// Display-only, bounded content. Deliberately not Debug: a diff can contain
/// user document data. It is never treated as a path to open or a command.
pub struct ArtifactDetail {
    pub entry_id: String,
    pub diff: Option<String>,
    pub diff_truncated: bool,
}

/// A projection of the selected ancestor chain, including pre-compaction tool
/// exchanges. Indexes in `projection` address this snapshot, NOT context.messages.
pub struct BranchSnapshot {
    pub projection: TaskProjection,
    pub skipped_messages: usize,
    details: BTreeMap<usize, ArtifactDetail>,
}

impl BranchSnapshot {
    pub fn from_session(session: &Session) -> Result<Self, HistoryError> {
        let entries = session
            .get_branch(None)
            .map_err(|_| HistoryError::InvalidBranch)?;
        let mut messages = Vec::new();
        let mut ids = Vec::new();
        let mut skipped_messages = 0;
        for entry in entries.iter().filter(|entry| entry.entry_type == "message") {
            let mut diagnostics = Vec::new();
            let message =
                crate::legacy_session::context_message(&entry.to_value(), &mut diagnostics)
                    .map_err(|_| HistoryError::InvalidMessage)?;
            if let Some(message) = message {
                ids.push(entry.id.clone());
                messages.push(message);
            } else {
                skipped_messages += 1;
            }
        }
        let projection = TaskProjection::from_messages(&messages, false);
        let mut details = BTreeMap::new();
        // Match the UI's most-recent 100 record window. Retain no raw arguments,
        // tool outputs or image payloads in the long-lived snapshot.
        for artifact in projection.artifacts.iter().rev().take(100) {
            if let Some(Message::ToolResult(result)) = messages.get(artifact.result_message) {
                let original = result.details.get("diff").and_then(|value| value.as_str());
                let diff = original.map(|text| text.chars().take(24_000).collect::<String>());
                let diff_truncated = original
                    .zip(diff.as_ref())
                    .is_some_and(|(original, preview)| original.len() > preview.len());
                details.insert(
                    artifact.result_message,
                    ArtifactDetail {
                        entry_id: ids[artifact.result_message].clone(),
                        diff,
                        diff_truncated,
                    },
                );
            }
        }
        Ok(Self {
            projection,
            skipped_messages,
            details,
        })
    }

    pub fn artifact_detail(&self, result_message: usize) -> Option<&ArtifactDetail> {
        self.details.get(&result_message)
    }
}
