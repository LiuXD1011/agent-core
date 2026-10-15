//! Host integration for explicit, versioned native extensions.

use crate::extension_process::NativeProcess;
pub use crate::extension_process::{
    NativeExtensionManifest, EXTENSION_PROTOCOL, EXTENSION_PROTOCOL_VERSION,
};
use crate::session::Session;
use agent_core::tools::{
    AfterToolCallContext, AfterToolCallResult, AgentTool, BeforeToolCallContext,
    BeforeToolCallResult,
};
use agent_core::types::{
    CancellationToken, Content, ExecutionMode, Message, ToolResult, Usage, UserMessage,
};
pub use agent_extensions::{
    custom_message, custom_message_content, BeforeAgentStartEvent, BeforeAgentStartResult,
    CommandInfo, CommandResult, CustomMessageContent, Extension, ExtensionCommand,
    ExtensionContext, ExtensionDiagnostic, ExtensionMode, ForkPosition, InputEvent, InputResult,
    InputSource, InputStreamingBehavior, LifecycleEvent, ModelIdentity, ModelSelectSource,
    NotificationLevel, RegistryToolHooks, SessionBeforeEvent, SessionBeforeResult,
    SessionShutdownReason, SessionStartReason, SessionSwitchReason, SharedRegistry, UiBroker,
    UiDialog, UiDialogRequest, UiHandle, UiReply, UiRequest,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct NativeToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: Value,
    pub execution_mode: Option<String>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct NativeCommandDefinition {
    pub name: String,
    pub description: String,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct NativeExtensionDescriptor {
    pub id: String,
    pub tools: Vec<NativeToolDefinition>,
    pub commands: Vec<NativeCommandDefinition>,
    pub hooks: Vec<String>,
}

/// No directory discovery or old JavaScript execution occurs. Every executable
/// must be named by an explicitly supplied manifest.
pub fn load_native_extensions(
    paths: &[PathBuf],
    context: &ExtensionContext,
    signal: &CancellationToken,
) -> Result<SharedRegistry, String> {
    let registry = SharedRegistry::default();
    registry.bind_ui(context.ui.clone())?;
    for path in paths {
        let manifest = NativeExtensionManifest::load(path)?;
        let process = Arc::new(NativeProcess::spawn(manifest, &context.workspace, signal)?);
        process.bind_ui(context.ui.clone());
        let response = process.request(
            "initialize",
            json!({"extensionId":process.id(),"context":context}),
            signal,
            &mut |_| {},
        )?;
        let descriptor: NativeExtensionDescriptor = serde_json::from_value(response)
            .map_err(|error| format!("Extension initialize: {error}"))?;
        if descriptor.id != process.id() {
            return Err("Extension descriptor id differs from manifest".into());
        }
        if descriptor.tools.len() > 128
            || descriptor.commands.len() > 128
            || descriptor.hooks.len() > 7
        {
            return Err("Extension registration limit exceeded".into());
        }
        for hook in &descriptor.hooks {
            if !matches!(
                hook.as_str(),
                "input"
                    | "session_before"
                    | "before_agent_start"
                    | "context"
                    | "before_tool_call"
                    | "after_tool_call"
                    | "lifecycle"
            ) {
                return Err(format!("Unsupported native extension hook {hook}"));
            }
        }
        for tool in &descriptor.tools {
            parse_mode(tool.execution_mode.as_deref())?;
        }
        let binding = Arc::new(NativeBinding {
            process,
            context: Mutex::new(context.clone()),
            pending: Mutex::new(VecDeque::new()),
            deferred_error: Mutex::new(None),
        });
        registry.register(Arc::new(NativeExtension {
            descriptor,
            binding,
        }))?;
    }
    Ok(registry)
}

pub fn extension_context(
    session: &Session,
    registry: &SharedRegistry,
    mode: ExtensionMode,
    model: Option<ModelIdentity>,
) -> Result<ExtensionContext, String> {
    let mut states = BTreeMap::new();
    for id in registry.ids() {
        if let Some(state) = session.runtime_state(&format!("extension:{id}"))? {
            states.insert(id, state);
        }
    }
    Ok(ExtensionContext {
        context: session.context.clone(),
        workspace: session.workspace.clone(),
        session_id: Some(session.id.clone()),
        session_name: session.get_session_name()?,
        mode,
        has_ui: registry.ui().is_enabled()
            && matches!(
                mode,
                ExtensionMode::Tui | ExtensionMode::Rpc | ExtensionMode::Desktop
            ),
        ui: registry.ui(),
        thinking_level: session.get_thinking_level()?.unwrap_or_default(),
        model,
        states,
    })
}

/// Invoked on the old active registry/current session before candidate mutation,
/// persistence or runtime replacement. A veto is distinct from token cancellation.
pub fn prepare_session_change(
    session: &Session,
    registry: &SharedRegistry,
    mode: ExtensionMode,
    model: Option<ModelIdentity>,
    event: &SessionBeforeEvent,
    signal: &CancellationToken,
) -> Result<bool, String> {
    let context = extension_context(session, registry, mode, model)?;
    Ok(!registry
        .emit_session_before(event, &context, signal)?
        .cancel)
}

/// Observation after the host has committed memory and durable storage.
/// Callback errors do not undo the name; cancellation only stops notification.
pub fn notify_session_info_changed(
    session: &Session,
    registry: &SharedRegistry,
    mode: ExtensionMode,
    model: Option<ModelIdentity>,
    signal: &CancellationToken,
) -> Result<(), String> {
    let context = extension_context(session, registry, mode, model)?;
    registry.emit_event(
        &LifecycleEvent::SessionInfoChanged {
            name: session.get_session_name()?,
        },
        &context,
        signal,
    )
}

/// Reads only the native current branch's explicit model-selection state. Imported
/// metadata, credentials, and old provider-response identities are not inferred.
pub fn stored_model_selection(session: &Session) -> Result<Option<ModelIdentity>, String> {
    let Some(value) = session.runtime_state("model-selection")? else {
        return Ok(None);
    };
    let object = value
        .as_object()
        .filter(|object| object.len() == 2)
        .ok_or("Invalid model-selection state: expected provider/model strings")?;
    let provider = object
        .get("provider")
        .and_then(Value::as_str)
        .ok_or("Invalid model-selection provider")?;
    let model = object
        .get("model")
        .and_then(Value::as_str)
        .ok_or("Invalid model-selection model")?;
    let identity = ModelIdentity {
        provider: provider.into(),
        model: model.into(),
    };
    validate_model_identity(&identity)?;
    Ok(Some(identity))
}

/// Saves a bounded identity-only state entry, without changing LLM messages or
/// storing provider configuration. Repeating the current branch value is a no-op.
pub fn record_model_selection(session: &mut Session, model: &ModelIdentity) -> Result<(), String> {
    validate_model_identity(model)?;
    if stored_model_selection(session)?.as_ref() == Some(model) {
        return Ok(());
    }
    let mut candidate = session.clone();
    candidate.record_runtime_state(
        "model-selection",
        serde_json::to_value(model).map_err(|error| error.to_string())?,
    )?;
    *session = candidate;
    Ok(())
}

/// Hosts commit their validated model/session candidate before calling this
/// cancellable, best-effort observation. A delivery error cannot undo that commit.
pub fn notify_model_select(
    session: &Session,
    registry: &SharedRegistry,
    mode: ExtensionMode,
    model: ModelIdentity,
    previous_model: Option<ModelIdentity>,
    source: ModelSelectSource,
    signal: &CancellationToken,
) -> Result<(), String> {
    validate_model_identity(&model)?;
    if let Some(previous) = &previous_model {
        validate_model_identity(previous)?;
    }
    if previous_model.as_ref() == Some(&model) {
        return Ok(());
    }
    let context = extension_context(session, registry, mode, Some(model.clone()))?;
    registry.emit_event(
        &LifecycleEvent::ModelSelect {
            model,
            previous_model,
            source,
        },
        &context,
        signal,
    )
}

fn validate_model_identity(model: &ModelIdentity) -> Result<(), String> {
    if model.provider.is_empty()
        || model.provider.len() > 128
        || !model
            .provider
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err("Model provider must be 1..128 ASCII name bytes".into());
    }
    if model.model.len() > 1024
        || model.model.trim() != model.model
        || model.model.chars().any(char::is_control)
        || (model.model.is_empty() && model.provider != "demo")
    {
        return Err("Model ID must be 1..1024 trimmed bytes without control characters (empty only for demo)".into());
    }
    Ok(())
}

pub fn execute_extension_command(
    session: &mut Session,
    registry: &SharedRegistry,
    name: &str,
    args: &str,
    context: &ExtensionContext,
    signal: &CancellationToken,
) -> Result<CommandResult, String> {
    let info = registry
        .resolve_command(name)
        .ok_or_else(|| format!("Extension command {name} not found"))?;
    let fresh = extension_context(session, registry, context.mode, context.model.clone())?;
    let result = registry.dispatch_command(name, args, &fresh, signal)?;
    let mut candidate = session.clone();
    for message in &result.messages {
        candidate.append_message(message.clone())?;
    }
    if let Some(name) = &result.session_name {
        candidate.append_session_info(name)?;
    }
    if !result.state.is_null() {
        candidate.record_runtime_state(
            &format!("extension:{}", info.extension_id),
            result.state.clone(),
        )?;
    }
    if signal.is_cancelled() {
        return Err("Extension command aborted".into());
    }
    *session = candidate;
    Ok(result)
}

/// Applies raw-input hooks only. Hosts route commands first, then call this once
/// before their template/skill expansion and before steering/follow-up enqueue.
pub fn prepare_input(
    session: &Session,
    registry: &SharedRegistry,
    prompt: Message,
    source: InputSource,
    context: &ExtensionContext,
    signal: &CancellationToken,
) -> Result<Option<Message>, String> {
    prepare_input_with_behavior(session, registry, prompt, source, context, signal, None)
}

#[allow(clippy::too_many_arguments)]
pub fn prepare_input_with_behavior(
    session: &Session,
    registry: &SharedRegistry,
    prompt: Message,
    source: InputSource,
    context: &ExtensionContext,
    signal: &CancellationToken,
    streaming_behavior: Option<InputStreamingBehavior>,
) -> Result<Option<Message>, String> {
    let Message::User(user) = &prompt else {
        return Err("Incoming input must be a User message".into());
    };
    let mut images = vec![];
    for block in &user.content {
        match block {
            Content::Image { .. } => images.push(block.clone()),
            Content::Text { .. } => {}
            _ => return Err("Incoming input must contain only text and image blocks".into()),
        }
    }
    let event = InputEvent {
        text: prompt.text(),
        images,
        source,
        streaming_behavior,
    };
    let fresh = extension_context(session, registry, context.mode, context.model.clone())?;
    match registry.emit_input(&event, &fresh, signal)? {
        InputResult::Continue => Ok(Some(prompt)),
        InputResult::Handled => Ok(None),
        InputResult::Transform { text, images } => {
            let mut content = vec![Content::text(text)];
            content.extend(images.unwrap_or(event.images));
            Ok(Some(Message::User(UserMessage {
                content,
                timestamp: user.timestamp,
            })))
        }
    }
}

fn parse_mode(mode: Option<&str>) -> Result<Option<ExecutionMode>, String> {
    match mode {
        None => Ok(None),
        Some("parallel") => Ok(Some(ExecutionMode::Parallel)),
        Some("sequential") => Ok(Some(ExecutionMode::Sequential)),
        _ => Err("Native tool executionMode must be parallel or sequential".into()),
    }
}

struct PendingEvent {
    params: Value,
    terminal: bool,
}
struct NativeBinding {
    process: Arc<NativeProcess>,
    context: Mutex<ExtensionContext>,
    pending: Mutex<VecDeque<PendingEvent>>,
    deferred_error: Mutex<Option<String>>,
}
impl NativeBinding {
    fn update_context(&self, context: &ExtensionContext) {
        *self
            .context
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = context.clone();
    }
    fn current_context(&self) -> ExtensionContext {
        self.context
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }
    fn call(
        &self,
        method: &str,
        params: Value,
        signal: &CancellationToken,
        on_update: &mut dyn FnMut(Value),
    ) -> Result<Value, String> {
        let result = self.process.request(method, params, signal, on_update);
        self.flush_pending(signal);
        result
    }
    // Streaming callbacks can synchronously emit lifecycle events while the
    // serialized transport is locked. Queue them instead of acquiring that lock.
    fn flush_pending(&self, signal: &CancellationToken) {
        let deadline = Instant::now() + Duration::from_millis(500);
        loop {
            let next = self
                .pending
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .pop_front();
            let Some(next) = next else {
                return;
            };
            let cleanup = CancellationToken::default();
            let token = if next.terminal { &cleanup } else { signal };
            let result = self.process.request_with_timeout(
                "lifecycle",
                next.params,
                token,
                deadline.saturating_duration_since(Instant::now()),
                &mut |_| {},
            );
            if let Err(error) = result {
                *self
                    .deferred_error
                    .lock()
                    .unwrap_or_else(|error| error.into_inner()) = Some(error);
                self.pending
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .clear();
                return;
            }
        }
    }
    fn decode<T: serde::de::DeserializeOwned>(
        &self,
        value: Value,
        kind: &str,
    ) -> Result<T, String> {
        serde_json::from_value(value).map_err(|error| {
            self.process.shutdown();
            format!("Malformed native {kind}: {error}")
        })
    }
}

struct NativeExtension {
    descriptor: NativeExtensionDescriptor,
    binding: Arc<NativeBinding>,
}
impl NativeExtension {
    fn subscribed(&self, hook: &str) -> bool {
        self.descriptor
            .hooks
            .iter()
            .any(|registered| registered == hook)
    }
}
impl Extension for NativeExtension {
    fn id(&self) -> &str {
        &self.descriptor.id
    }
    fn bind_ui(&self, ui: UiHandle) -> Result<(), String> {
        self.binding.process.bind_ui(ui);
        Ok(())
    }
    fn tools(&self) -> Vec<Arc<dyn AgentTool>> {
        self.descriptor
            .tools
            .iter()
            .map(|definition| {
                Arc::new(NativeTool {
                    definition: definition.clone(),
                    binding: self.binding.clone(),
                }) as Arc<dyn AgentTool>
            })
            .collect()
    }
    fn commands(&self) -> Vec<Arc<dyn ExtensionCommand>> {
        self.descriptor
            .commands
            .iter()
            .map(|definition| {
                Arc::new(NativeCommand {
                    definition: definition.clone(),
                    binding: self.binding.clone(),
                }) as Arc<dyn ExtensionCommand>
            })
            .collect()
    }
    fn before_session_change(
        &self,
        event: &SessionBeforeEvent,
        context: &ExtensionContext,
        signal: &CancellationToken,
    ) -> Result<SessionBeforeResult, String> {
        self.binding.update_context(context);
        if !self.subscribed("session_before") {
            return Ok(SessionBeforeResult::default());
        }
        let value = self.binding.call(
            "session_before",
            json!({"event":event,"context":context}),
            signal,
            &mut |_| {},
        )?;
        if value.is_null() {
            return Ok(SessionBeforeResult::default());
        }
        self.binding.decode(value, "session_before result")
    }
    fn input(
        &self,
        event: &InputEvent,
        context: &ExtensionContext,
        signal: &CancellationToken,
    ) -> Result<InputResult, String> {
        self.binding.update_context(context);
        if !self.subscribed("input") {
            return Ok(InputResult::Continue);
        }
        let value = self.binding.call(
            "input",
            json!({"event":event,"context":context}),
            signal,
            &mut |_| {},
        )?;
        if value.is_null() {
            return Ok(InputResult::Continue);
        }
        self.binding.decode(value, "input result")
    }
    fn before_agent_start(
        &self,
        event: &BeforeAgentStartEvent,
        context: &ExtensionContext,
        signal: &CancellationToken,
    ) -> Result<BeforeAgentStartResult, String> {
        self.binding.update_context(context);
        if !self.subscribed("before_agent_start") {
            return Ok(BeforeAgentStartResult::default());
        }
        let value = self.binding.call(
            "before_agent_start",
            json!({"event":event,"context":context}),
            signal,
            &mut |_| {},
        )?;
        self.binding.decode(value, "before_agent_start result")
    }
    fn context(
        &self,
        messages: &[Message],
        context: &ExtensionContext,
        signal: &CancellationToken,
    ) -> Result<Option<Vec<Message>>, String> {
        self.binding.update_context(context);
        if !self.subscribed("context") {
            return Ok(None);
        }
        let value = self.binding.call(
            "context",
            json!({"messages":messages,"context":context}),
            signal,
            &mut |_| {},
        )?;
        self.binding.decode(value, "context result")
    }
    fn before_tool_call(
        &self,
        call: BeforeToolCallContext<'_>,
        context: &ExtensionContext,
        signal: &CancellationToken,
    ) -> Result<BeforeToolCallResult, String> {
        self.binding.update_context(context);
        if !self.subscribed("before_tool_call") {
            return Ok(BeforeToolCallResult::default());
        }
        let value=self.binding.call("before_tool_call",json!({"assistantMessage":call.assistant_message,"toolCall":call.tool_call,"args":call.args,"context":context}),signal,&mut |_|{})?;
        let args = value.get("args").cloned();
        let patch: BeforePatch = self.binding.decode(value, "before_tool_call result")?;
        if let Some(args) = args {
            *call.args = args;
        }
        Ok(BeforeToolCallResult {
            block: patch.block,
            reason: patch.reason,
            terminate: patch.terminate,
        })
    }
    fn after_tool_call(
        &self,
        call: AfterToolCallContext<'_>,
        context: &ExtensionContext,
        signal: &CancellationToken,
    ) -> Result<Option<AfterToolCallResult>, String> {
        self.binding.update_context(context);
        if !self.subscribed("after_tool_call") {
            return Ok(None);
        }
        let value=self.binding.call("after_tool_call",json!({"assistantMessage":call.assistant_message,"toolCall":call.tool_call,"args":call.args,"result":call.result,"isError":call.is_error,"context":context}),signal,&mut |_|{})?;
        if value.is_null() {
            return Ok(None);
        }
        let details = value.get("details").cloned();
        let patch: AfterPatch = self.binding.decode(value, "after_tool_call result")?;
        Ok(Some(AfterToolCallResult {
            content: patch.content,
            details,
            usage: patch.usage,
            terminate: patch.terminate,
            is_error: patch.is_error,
        }))
    }
    fn on_event(
        &self,
        event: &LifecycleEvent,
        context: &ExtensionContext,
        signal: &CancellationToken,
    ) -> Result<(), String> {
        self.binding.update_context(context);
        let shutdown = matches!(event, LifecycleEvent::SessionShutdown { .. });
        let result = (|| {
            if !self.subscribed("lifecycle") {
                return Ok(());
            }
            let params = json!({"event":event,"context":context});
            if self.binding.process.is_busy() && !shutdown {
                let mut pending = self
                    .binding
                    .pending
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                if pending.len() >= 128
                    || pending
                        .iter()
                        .map(|entry| entry.params.to_string().len())
                        .sum::<usize>()
                        .saturating_add(params.to_string().len())
                        > 8 * 1024 * 1024
                {
                    self.binding.process.abort();
                    return Err("Deferred native lifecycle limit exceeded".into());
                }
                pending.push_back(PendingEvent {
                    params,
                    terminal: event.is_terminal(),
                });
                return Ok(());
            }
            self.binding.flush_pending(signal);
            let cleanup = CancellationToken::default();
            let value = if event.is_terminal() {
                self.binding.process.request_with_timeout(
                    "lifecycle",
                    params,
                    &cleanup,
                    Duration::from_millis(250),
                    &mut |_| {},
                )
            } else {
                self.binding.call("lifecycle", params, signal, &mut |_| {})
            };
            value?;
            if let Some(error) = self
                .binding
                .deferred_error
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .take()
            {
                return Err(error);
            }
            Ok(())
        })();
        if shutdown {
            self.binding.process.shutdown();
        }
        result
    }
}

#[derive(Default, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
struct BeforePatch {
    args: Option<Value>,
    block: bool,
    reason: Option<String>,
    terminate: bool,
}
#[derive(Default, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
struct AfterPatch {
    content: Option<Vec<Content>>,
    details: Option<Value>,
    usage: Option<Usage>,
    terminate: Option<bool>,
    is_error: Option<bool>,
}
struct NativeTool {
    definition: NativeToolDefinition,
    binding: Arc<NativeBinding>,
}
impl AgentTool for NativeTool {
    fn name(&self) -> &str {
        &self.definition.name
    }
    fn description(&self) -> &str {
        &self.definition.description
    }
    fn parameters(&self) -> Value {
        self.definition.parameters.clone()
    }
    fn execution_mode(&self) -> Option<ExecutionMode> {
        parse_mode(self.definition.execution_mode.as_deref())
            .ok()
            .flatten()
    }
    fn execute(
        &self,
        call_id: &str,
        args: &Value,
        signal: &CancellationToken,
        on_update: &mut dyn FnMut(ToolResult),
    ) -> Result<ToolResult, String> {
        let mut malformed = None;
        let value=self.binding.call("execute",json!({"name":self.name(),"callId":call_id,"arguments":args,"context":self.binding.current_context()}),signal,&mut |update|{
            match serde_json::from_value(update){Ok(result)=>on_update(result),Err(error)=>{malformed=Some(format!("Malformed native tool update: {error}"));self.binding.process.abort();}}
        });
        if let Some(error) = malformed {
            self.binding.process.shutdown();
            return Err(error);
        }
        self.binding.decode(value?, "tool result")
    }
}
struct NativeCommand {
    definition: NativeCommandDefinition,
    binding: Arc<NativeBinding>,
}
impl ExtensionCommand for NativeCommand {
    fn name(&self) -> &str {
        &self.definition.name
    }
    fn description(&self) -> &str {
        &self.definition.description
    }
    fn execute(
        &self,
        args: &str,
        context: &ExtensionContext,
        signal: &CancellationToken,
    ) -> Result<CommandResult, String> {
        self.binding.update_context(context);
        let value = self.binding.call(
            "command",
            json!({"name":self.name(),"args":args,"context":context}),
            signal,
            &mut |_| {},
        )?;
        self.binding.decode(value, "command result")
    }
}
