use serde::{Deserialize, Serialize};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::{SystemTime, UNIX_EPOCH};

pub use serde_json::Value;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingLevel {
    #[default]
    Off,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl ThinkingLevel {
    pub const ALL: [Self; 7] = [
        Self::Off,
        Self::Minimal,
        Self::Low,
        Self::Medium,
        Self::High,
        Self::Xhigh,
        Self::Max,
    ];
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
        }
    }
}
impl std::str::FromStr for ThinkingLevel {
    type Err = String;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|level| level.as_str() == value)
            .ok_or_else(|| {
                "Thinking level must be off, minimal, low, medium, high, xhigh or max".into()
            })
    }
}
impl std::fmt::Display for ThinkingLevel {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Clone, Debug, Default)]
pub struct CancellationToken(Arc<AtomicBool>);

impl CancellationToken {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
    pub fn is_aborted(&self) -> bool {
        self.is_cancelled()
    }
}

pub type AbortSignal = CancellationToken;

pub fn timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub total_tokens: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum StopReason {
    #[serde(rename = "stop")]
    Stop,
    #[serde(rename = "toolUse")]
    ToolUse,
    #[serde(rename = "length")]
    Length,
    #[serde(rename = "error")]
    Error,
    #[serde(rename = "aborted")]
    Aborted,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Content {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "thinking")]
    Thinking { thinking: String },
    #[serde(rename = "image")]
    Image {
        data: String,
        #[serde(rename = "mimeType")]
        mime_type: String,
    },
    #[serde(rename = "toolCall")]
    ToolCall(ToolCall),
}

impl Content {
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into() }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UserMessage {
    pub content: Vec<Content>,
    pub timestamp: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssistantMessage {
    pub content: Vec<Content>,
    pub stop_reason: StopReason,
    pub error_message: Option<String>,
    pub usage: Option<Usage>,
    pub timestamp: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol_metadata: Option<Value>,
}

impl AssistantMessage {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            content: vec![Content::text(text)],
            stop_reason: StopReason::Stop,
            error_message: None,
            usage: None,
            timestamp: timestamp(),
            protocol_metadata: None,
        }
    }
    pub fn error(text: impl Into<String>, aborted: bool) -> Self {
        Self {
            content: vec![],
            stop_reason: if aborted {
                StopReason::Aborted
            } else {
                StopReason::Error
            },
            error_message: Some(text.into()),
            usage: None,
            timestamp: timestamp(),
            protocol_metadata: None,
        }
    }
    pub fn tool_calls(&self) -> Vec<ToolCall> {
        self.content
            .iter()
            .filter_map(|item| match item {
                Content::ToolCall(call) => Some(call.clone()),
                _ => None,
            })
            .collect()
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolResult {
    pub content: Vec<Content>,
    pub details: Value,
    pub usage: Option<Usage>,
    pub added_tool_names: Vec<String>,
    pub terminate: bool,
}

impl ToolResult {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            content: vec![Content::text(text)],
            ..Self::default()
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolResultMessage {
    pub tool_call_id: String,
    pub tool_name: String,
    pub content: Vec<Content>,
    pub details: Value,
    pub usage: Option<Usage>,
    pub added_tool_names: Vec<String>,
    pub is_error: bool,
    pub timestamp: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "role")]
pub enum Message {
    #[serde(rename = "user")]
    User(UserMessage),
    #[serde(rename = "assistant")]
    Assistant(AssistantMessage),
    #[serde(rename = "toolResult")]
    ToolResult(ToolResultMessage),
    #[serde(rename = "custom")]
    Custom {
        kind: String,
        data: Value,
        timestamp: u64,
    },
}

impl Message {
    pub fn user(text: impl Into<String>) -> Self {
        Self::User(UserMessage {
            content: vec![Content::text(text)],
            timestamp: timestamp(),
        })
    }
    pub fn role(&self) -> &'static str {
        match self {
            Self::User(_) => "user",
            Self::Assistant(_) => "assistant",
            Self::ToolResult(_) => "toolResult",
            Self::Custom { .. } => "custom",
        }
    }
    pub fn text(&self) -> String {
        let content = match self {
            Self::User(m) => &m.content,
            Self::Assistant(m) => &m.content,
            Self::ToolResult(m) => &m.content,
            Self::Custom { .. } => return String::new(),
        };
        content
            .iter()
            .filter_map(|item| match item {
                Content::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Context {
    pub system_prompt: String,
    pub messages: Vec<Message>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ExecutionMode {
    Sequential,
    Parallel,
    #[default]
    Auto,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum AgentEvent {
    AgentStart,
    TurnStart,
    MessageStart {
        message: Message,
    },
    MessageUpdate {
        message: Message,
    },
    MessageEnd {
        message: Message,
    },
    ToolExecutionStart {
        tool_call_id: String,
        tool_name: String,
        args: Value,
    },
    ToolExecutionUpdate {
        tool_call_id: String,
        tool_name: String,
        args: Value,
        partial_result: ToolResult,
    },
    ToolExecutionEnd {
        tool_call_id: String,
        tool_name: String,
        result: ToolResult,
        is_error: bool,
    },
    TurnEnd {
        message: AssistantMessage,
        tool_results: Vec<ToolResultMessage>,
    },
    AgentEnd {
        messages: Vec<Message>,
    },
}

pub type Event = AgentEvent;
