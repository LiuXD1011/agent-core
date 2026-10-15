use crate::UiHandle;
use agent_core::tools::{
    AfterToolCallContext, AfterToolCallResult, AgentTool, BeforeToolCallContext,
    BeforeToolCallResult,
};
pub use agent_core::types::ThinkingLevel;
use agent_core::types::{timestamp, CancellationToken, Content, Context, Event, Message, Value};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionMode {
    Tui,
    Rpc,
    Json,
    Desktop,
    #[default]
    Print,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelIdentity {
    pub provider: String,
    pub model: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelSelectSource {
    Set,
    Restore,
}

/// A fresh, owned snapshot; it contains no API keys or provider credentials.
/// Hosts must construct it again after session replacement, not reuse captured session data.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct ExtensionContext {
    pub context: Context,
    pub workspace: PathBuf,
    pub session_id: Option<String>,
    pub session_name: Option<String>,
    pub mode: ExtensionMode,
    pub has_ui: bool,
    #[serde(skip)]
    pub ui: UiHandle,
    pub model: Option<ModelIdentity>,
    pub thinking_level: ThinkingLevel,
    /// Current branch state, loaded by the host from native `extension:{id}` entries.
    pub states: BTreeMap<String, Value>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputSource {
    #[default]
    Interactive,
    Rpc,
    Extension,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum InputStreamingBehavior {
    #[serde(rename = "steer")]
    Steer,
    #[serde(rename = "followUp")]
    FollowUp,
}

/// Raw incoming input, before skill/template expansion or queue delivery.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct InputEvent {
    pub text: String,
    pub images: Vec<Content>,
    pub source: InputSource,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub streaming_behavior: Option<InputStreamingBehavior>,
}

/// None (including JSON null) preserves images; Some(empty) explicitly clears.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum InputResult {
    #[default]
    Continue,
    Handled,
    Transform {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        images: Option<Vec<Content>>,
    },
}

/// Host and transformed inputs share bounds, before any provider request.
pub fn validate_input(event: &InputEvent) -> Result<(), String> {
    if event.images.len() > 16 {
        return Err("Input exceeds 16 images".into());
    }
    if event.text.len() > 1_048_576 {
        return Err("Input text exceeds 1MiB".into());
    }
    let mut bytes = 0usize;
    for image in &event.images {
        let Content::Image { data, mime_type } = image else {
            return Err("Input images must contain only image blocks".into());
        };
        bytes = bytes
            .saturating_add(data.len())
            .saturating_add(mime_type.len());
    }
    if bytes > 67_108_864 {
        return Err("Input images exceed 64MiB".into());
    }
    Ok(())
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct BeforeAgentStartEvent {
    pub prompt: String,
    /// Hosts supply image blocks only, preserving the expanded input's attachments.
    pub images: Vec<Content>,
    pub system_prompt: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct BeforeAgentStartResult {
    /// Only Custom messages can be injected here. User input is emitted by the host once.
    pub messages: Vec<Message>,
    pub system_prompt: Option<String>,
}

/// The observed TS custom-message content shape, stored inside native Custom.data.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CustomMessageContent {
    Text(String),
    Blocks(Vec<Content>),
}

/// Builds a native extension message. `display` controls presentation, not LLM inclusion.
pub fn custom_message(
    kind: impl Into<String>,
    content: CustomMessageContent,
    display: bool,
    details: Value,
) -> Result<Message, String> {
    let kind = kind.into();
    if kind.trim().is_empty() {
        return Err("Custom message kind must be nonempty".into());
    }
    let message = Message::Custom {
        kind,
        data: serde_json::json!({"content":content,"display":display,"details":details}),
        timestamp: timestamp(),
    };
    custom_message_content(&message)?;
    Ok(message)
}

/// Extracts native Custom.data.content for host conversion to a User message.
/// A missing content field is UI/state-only. Thinking and tool calls are rejected.
pub fn custom_message_content(message: &Message) -> Result<Option<Vec<Content>>, String> {
    let Message::Custom { data, .. } = message else {
        return Ok(None);
    };
    let Some(content) = data.get("content") else {
        return Ok(None);
    };
    let content = serde_json::from_value::<CustomMessageContent>(content.clone())
        .map_err(|_| "Custom content must be text or text/image blocks".to_owned())?;
    let blocks = match content {
        CustomMessageContent::Text(text) => vec![Content::text(text)],
        CustomMessageContent::Blocks(blocks) => blocks,
    };
    if blocks
        .iter()
        .any(|block| !matches!(block, Content::Text { .. } | Content::Image { .. }))
    {
        return Err("Custom content must contain only text and images".into());
    }
    Ok(Some(blocks))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStartReason {
    Startup,
    Reload,
    New,
    Resume,
    Fork,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionShutdownReason {
    Quit,
    Reload,
    New,
    Resume,
    Fork,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionSwitchReason {
    New,
    Resume,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ForkPosition {
    #[default]
    Before,
    At,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum SessionBeforeEvent {
    #[serde(rename = "session_before_switch")]
    Switch {
        reason: SessionSwitchReason,
        target_session_file: Option<PathBuf>,
    },
    #[serde(rename = "session_before_fork")]
    Fork {
        entry_id: String,
        position: ForkPosition,
    },
}

impl SessionBeforeEvent {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Switch { .. } => "session_before_switch",
            Self::Fork { .. } => "session_before_fork",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionBeforeResult {
    pub cancel: bool,
}

/// Session names follow the original store: one space per CR/LF run, then JS trim.
/// The empty string is an explicit clear, not an invalid command result.
pub fn normalize_session_name(name: &str) -> String {
    let mut normalized = String::with_capacity(name.len());
    let mut newline = false;
    for character in name.chars() {
        if matches!(character, '\r' | '\n') {
            if !newline {
                normalized.push(' ');
            }
            newline = true;
        } else {
            normalized.push(character);
            newline = false;
        }
    }
    normalized
        .trim_matches(|character| {
            matches!(
                character,
                '\t' | '\u{000b}' | '\u{000c}' | ' ' | '\u{00a0}' | '\u{1680}' | '\u{2000}'
                    ..='\u{200a}'
                        | '\u{2028}'
                        | '\u{2029}'
                        | '\u{202f}'
                        | '\u{205f}'
                        | '\u{3000}'
                        | '\u{feff}'
            )
        })
        .to_owned()
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum LifecycleEvent {
    Agent {
        event: Event,
    },
    SessionStart {
        reason: SessionStartReason,
        previous_session_file: Option<PathBuf>,
    },
    SessionShutdown {
        reason: SessionShutdownReason,
        target_session_file: Option<PathBuf>,
    },
    SessionInfoChanged {
        name: Option<String>,
    },
    ThinkingLevelSelect {
        level: ThinkingLevel,
        previous_level: ThinkingLevel,
    },
    /// A post-commit observation, never a veto or a synthesized transcript message.
    ModelSelect {
        model: ModelIdentity,
        previous_model: Option<ModelIdentity>,
        source: ModelSelectSource,
    },
    AgentSettled,
}

impl LifecycleEvent {
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::SessionShutdown { .. }
                | Self::AgentSettled
                | Self::Agent {
                    event: Event::AgentEnd { .. }
                }
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotificationLevel {
    Info,
    Warning,
    Error,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum UiRequest {
    Notification {
        level: NotificationLevel,
        text: String,
    },
    SetStatus {
        key: String,
        text: Option<String>,
    },
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct CommandResult {
    pub text: String,
    /// Only User and Custom messages may be appended by a command.
    pub messages: Vec<Message>,
    pub session_name: Option<String>,
    /// Null leaves state unchanged; other JSON is saved under the command's extension ID.
    /// State is a native runtime_state entry, never an LLM message.
    pub state: Value,
    pub ui: Vec<UiRequest>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandInfo {
    pub extension_id: String,
    pub name: String,
    pub invocation_name: String,
    pub description: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtensionDiagnostic {
    pub extension_id: String,
    pub event: String,
    pub error: String,
}

pub trait ExtensionCommand: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str {
        ""
    }
    fn execute(
        &self,
        args: &str,
        context: &ExtensionContext,
        signal: &CancellationToken,
    ) -> Result<CommandResult, String>;
}

/// Implementations opt in to individual hooks. Registration runs once and must not
/// start long-lived resources; use SessionStart/SessionShutdown to manage those.
pub trait Extension: Send + Sync {
    fn id(&self) -> &str;
    /// Nonblocking host binding shared by tool, event and command callbacks.
    fn bind_ui(&self, _ui: UiHandle) -> Result<(), String> {
        Ok(())
    }
    fn tools(&self) -> Vec<Arc<dyn AgentTool>> {
        Vec::new()
    }
    fn commands(&self) -> Vec<Arc<dyn ExtensionCommand>> {
        Vec::new()
    }
    fn before_session_change(
        &self,
        _event: &SessionBeforeEvent,
        _context: &ExtensionContext,
        _signal: &CancellationToken,
    ) -> Result<SessionBeforeResult, String> {
        Ok(SessionBeforeResult::default())
    }
    fn input(
        &self,
        _event: &InputEvent,
        _context: &ExtensionContext,
        _signal: &CancellationToken,
    ) -> Result<InputResult, String> {
        Ok(InputResult::Continue)
    }
    fn before_agent_start(
        &self,
        _event: &BeforeAgentStartEvent,
        _context: &ExtensionContext,
        _signal: &CancellationToken,
    ) -> Result<BeforeAgentStartResult, String> {
        Ok(BeforeAgentStartResult::default())
    }
    fn context(
        &self,
        _messages: &[Message],
        _context: &ExtensionContext,
        _signal: &CancellationToken,
    ) -> Result<Option<Vec<Message>>, String> {
        Ok(None)
    }
    fn before_tool_call(
        &self,
        _call: BeforeToolCallContext<'_>,
        _context: &ExtensionContext,
        _signal: &CancellationToken,
    ) -> Result<BeforeToolCallResult, String> {
        Ok(BeforeToolCallResult::default())
    }
    fn after_tool_call(
        &self,
        _call: AfterToolCallContext<'_>,
        _context: &ExtensionContext,
        _signal: &CancellationToken,
    ) -> Result<Option<AfterToolCallResult>, String> {
        Ok(None)
    }
    fn on_event(
        &self,
        _event: &LifecycleEvent,
        _context: &ExtensionContext,
        _signal: &CancellationToken,
    ) -> Result<(), String> {
        Ok(())
    }
}
