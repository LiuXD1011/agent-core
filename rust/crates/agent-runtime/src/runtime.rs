//! Shared execution boundary. Clients own their UI and storage policy, not an agent loop.

use crate::automatic::{
    check_and_compact, estimate_context_tokens, estimate_context_tokens_for_model,
    recoverable_response, AutomationHandle, AutomationOptions, UpdatePlanTool,
};
use crate::builtins::{create_tools, tool_definitions};
use crate::extensions::{
    custom_message_content, extension_context, BeforeAgentStartEvent, ExtensionContext,
    ExtensionMode, LifecycleEvent, ModelIdentity, SharedRegistry,
};
use crate::provider::{ModelProvider, ProviderSettings};
use crate::retry::{has_partial_output, is_context_overflow, RetryingProvider};
use crate::session::Session;
use agent_core::agent_loop::{run_agent_loop, LoopConfig, LoopHooks, Provider};
use agent_core::tools::AgentTool;
use agent_core::types::{
    AgentEvent, AssistantMessage, CancellationToken, Content, Context, Message, StopReason,
    UserMessage,
};
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::{BTreeSet, VecDeque};
use std::path::Path;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum QueueMode {
    All,
    #[default]
    OneAtATime,
}

impl QueueMode {
    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "all" => Ok(Self::All),
            "one-at-a-time" => Ok(Self::OneAtATime),
            _ => Err("Queue mode must be all or one-at-a-time".into()),
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::OneAtATime => "one-at-a-time",
        }
    }
}

#[derive(Default)]
struct Pending {
    steering: VecDeque<Message>,
    follow_up: VecDeque<Message>,
    steering_mode: QueueMode,
    follow_up_mode: QueueMode,
}

#[derive(Clone, Default)]
pub struct MessageQueues(Arc<Mutex<Pending>>);

impl MessageQueues {
    pub fn steer(&self, message: Message) {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .steering
            .push_back(message);
    }
    pub fn follow_up(&self, message: Message) {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .follow_up
            .push_back(message);
    }
    pub fn len(&self) -> usize {
        let pending = self.0.lock().unwrap_or_else(|e| e.into_inner());
        pending.steering.len() + pending.follow_up.len()
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    pub fn pop_next(&self) -> Option<Message> {
        let mut pending = self.0.lock().unwrap_or_else(|e| e.into_inner());
        pending
            .steering
            .pop_front()
            .or_else(|| pending.follow_up.pop_front())
    }
    pub fn modes(&self) -> (QueueMode, QueueMode) {
        let pending = self.0.lock().unwrap_or_else(|e| e.into_inner());
        (pending.steering_mode, pending.follow_up_mode)
    }
    pub fn set_steering_mode(&self, mode: QueueMode) {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .steering_mode = mode;
    }
    pub fn set_follow_up_mode(&self, mode: QueueMode) {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .follow_up_mode = mode;
    }
    pub fn clear(&self) -> (Vec<Message>, Vec<Message>) {
        let mut pending = self.0.lock().unwrap_or_else(|e| e.into_inner());
        (
            pending.steering.drain(..).collect(),
            pending.follow_up.drain(..).collect(),
        )
    }
}

impl LoopHooks for MessageQueues {
    fn get_steering_messages(&mut self) -> Vec<Message> {
        let mut pending = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let count = if pending.steering_mode == QueueMode::All {
            pending.steering.len()
        } else {
            1.min(pending.steering.len())
        };
        pending.steering.drain(..count).collect()
    }
    fn get_follow_up_messages(&mut self) -> Vec<Message> {
        let mut pending = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let count = if pending.follow_up_mode == QueueMode::All {
            pending.follow_up.len()
        } else {
            1.min(pending.follow_up.len())
        };
        pending.follow_up.drain(..count).collect()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct RunOptions {
    pub allow_changes: bool,
    pub allow_commands: bool,
    pub no_tools: bool,
    pub tools: Option<Vec<String>>,
    pub exclude_tools: Vec<String>,
    pub max_turns: usize,
    pub system_prompt_suffix: String,
    pub automation: AutomationOptions,
    #[serde(skip)]
    pub automation_state: AutomationHandle,
    #[serde(skip)]
    pub extensions: SharedRegistry,
    pub extension_mode: ExtensionMode,
    pub extension_model: Option<ModelIdentity>,
}

impl Default for RunOptions {
    fn default() -> Self {
        Self {
            allow_changes: false,
            allow_commands: false,
            no_tools: false,
            tools: None,
            exclude_tools: vec![],
            max_turns: 128,
            system_prompt_suffix: String::new(),
            automation: AutomationOptions::default(),
            automation_state: AutomationHandle::default(),
            extensions: SharedRegistry::default(),
            extension_mode: ExtensionMode::default(),
            extension_model: None,
        }
    }
}

pub fn run_prompt(
    session: &mut Session,
    prompt: Message,
    settings: &ProviderSettings,
    options: &RunOptions,
    queues: &MessageQueues,
    signal: &CancellationToken,
    emit: &mut dyn FnMut(AgentEvent),
) -> Result<(), String> {
    let tools = configured_tools(&session.workspace, options)?;
    let mut provider = ModelProvider {
        settings: settings.clone(),
        tool_definitions: tool_definitions(&tools),
    };
    run_with_provider(
        session,
        prompt,
        &mut provider,
        &tools,
        options,
        queues,
        signal,
        emit,
    )
}

pub fn configured_tools(
    workspace: &Path,
    options: &RunOptions,
) -> Result<Vec<Box<dyn AgentTool>>, String> {
    let mut tools = options.extensions.tools();
    tools.extend(create_tools(
        workspace,
        options.allow_changes,
        options.allow_commands,
    ));
    if options.automation.enabled {
        tools.push(Box::new(UpdatePlanTool {
            handle: options.automation_state.clone(),
        }));
    }
    let mut names = BTreeSet::new();
    tools.retain(|tool| {
        !options.no_tools
            && (options.allow_changes || !matches!(tool.name(), "write" | "edit"))
            && (options.allow_commands || tool.name() != "run_command")
            && names.insert(tool.name().to_owned())
            && options
                .tools
                .as_ref()
                .is_none_or(|names| names.iter().any(|name| name == tool.name()))
            && !options.exclude_tools.iter().any(|name| name == tool.name())
    });
    if let Some(requested) = &options.tools {
        for name in requested {
            if !options.exclude_tools.contains(name)
                && !options.no_tools
                && !tools.iter().any(|tool| tool.name() == name)
            {
                return Err(format!("Tool not enabled: {name}"));
            }
        }
    }
    Ok(tools)
}

#[allow(clippy::too_many_arguments)]
pub fn run_with_provider(
    session: &mut Session,
    prompt: Message,
    provider: &mut dyn Provider,
    tools: &[Box<dyn AgentTool>],
    options: &RunOptions,
    queues: &MessageQueues,
    signal: &CancellationToken,
    emit: &mut dyn FnMut(AgentEvent),
) -> Result<(), String> {
    if !session.workspace.is_dir() {
        return Err(format!(
            "Workspace is not a directory: {}",
            session.workspace.display()
        ));
    }
    options.automation.validate()?;
    options.automation_state.bind_session(session);
    let saved_state = session.runtime_state("automation")?;
    let persist_state =
        options.automation.enabled || options.automation.retry.enabled || saved_state.is_some();
    if let Some(state) = saved_state {
        options.automation_state.restore_state(
            serde_json::from_value(state)
                .map_err(|error| format!("Automatic runtime state: {error}"))?,
        )?;
        options.automation_state.bind_session(session);
    }
    if let Some(model) = &options.automation.model {
        options.automation_state.bind_model(model);
    }
    if session.context.messages.is_empty() && session.name == "新会话" {
        session.name = prompt.text().chars().take(24).collect();
    }
    let mut context = session.context.clone();
    if !options.system_prompt_suffix.is_empty() {
        context.system_prompt.push_str("\n\n");
        context
            .system_prompt
            .push_str(&options.system_prompt_suffix);
    }
    let mut extension_snapshot = extension_context(
        session,
        &options.extensions,
        options.extension_mode,
        options.extension_model.clone(),
    )?;
    extension_snapshot.context = context.clone();
    let mut prompts = vec![prompt.clone()];
    if !options.extensions.ids().is_empty() && !signal.is_cancelled() {
        let images = match &prompt {
            Message::User(user) => user
                .content
                .iter()
                .filter(|content| matches!(content, Content::Image { .. }))
                .cloned()
                .collect(),
            _ => vec![],
        };
        let before = options.extensions.before_agent_start(
            &BeforeAgentStartEvent {
                prompt: prompt.text(),
                images,
                system_prompt: context.system_prompt.clone(),
            },
            &extension_snapshot,
            signal,
        );
        let before = match before {
            Ok(before) => before,
            Err(error) => {
                let _ = options.extensions.emit_event(
                    &LifecycleEvent::AgentSettled,
                    &extension_snapshot,
                    signal,
                );
                return Err(error);
            }
        };
        prompts.extend(before.messages);
        if let Some(system_prompt) = before.system_prompt {
            context.system_prompt = system_prompt;
            extension_snapshot.context = context.clone();
        }
    }
    let tool_hooks = options.extensions.tool_hooks(extension_snapshot);
    let stored = Rc::new(RefCell::new(session.clone()));
    let failure = Rc::new(RefCell::new(None));
    let replacement = Rc::new(RefCell::new(None));
    let shared_provider = Rc::new(RefCell::new(provider));
    let mut hooks = RuntimeHooks {
        session: stored.clone(),
        provider: shared_provider.clone(),
        queues: queues.clone(),
        options: options.automation.clone(),
        handle: options.automation_state.clone(),
        failure: failure.clone(),
        replacement: replacement.clone(),
        extensions: options.extensions.clone(),
        extension_mode: options.extension_mode,
        extension_model: options.extension_model.clone(),
        system_prompt: context.system_prompt.clone(),
    };
    let mut provider = RuntimeProvider {
        session: stored.clone(),
        provider: shared_provider.clone(),
        options: options.automation.clone(),
        handle: options.automation_state.clone(),
        failure: failure.clone(),
        replacement,
        extensions: options.extensions.clone(),
        extension_mode: options.extension_mode,
        extension_model: options.extension_model.clone(),
    };
    let result = run_agent_loop(
        prompts,
        &context,
        LoopConfig {
            max_turns: options.max_turns,
            ..Default::default()
        },
        &mut provider,
        if options.no_tools { &[] } else { tools },
        &mut hooks,
        &tool_hooks,
        signal,
        &mut |event| {
            if let AgentEvent::MessageEnd { message } = &event {
                // A preparation failure is diagnostic, not a fabricated transcript entry.
                if failure.borrow().is_none() {
                    if let Err(error) = stored.borrow_mut().append_message(message.clone()) {
                        *failure.borrow_mut() = Some(error);
                        signal.cancel();
                    }
                }
            }
            let mut snapshot = lifecycle_context(&stored.borrow(), options);
            snapshot
                .context
                .system_prompt
                .clone_from(&context.system_prompt);
            let _ = options.extensions.emit_event(
                &LifecycleEvent::Agent {
                    event: event.clone(),
                },
                &snapshot,
                signal,
            );
            emit(event);
        },
    );
    *session = stored.borrow().clone();
    let mut outcome = (|| {
        if let Some(error) = failure.borrow().clone() {
            return Err(error);
        }
        result.map_err(|e| e.to_string())?;
        if let Some(message) =
            session
                .context
                .messages
                .iter()
                .rev()
                .find_map(|message| match message {
                    Message::Assistant(message) => Some(message),
                    _ => None,
                })
        {
            if matches!(message.stop_reason, StopReason::Error | StopReason::Aborted) {
                return Err(message
                    .error_message
                    .clone()
                    .unwrap_or_else(|| "Agent run failed".into()));
            }
        }
        // A completed answer may also report silent overflow. Preserve it; never rerun its tools.
        if options.automation.enabled && !signal.is_cancelled() {
            let mut request = session.context.clone();
            request.system_prompt.clone_from(&context.system_prompt);
            request.messages = llm_messages(&request.messages)?;
            let mut inner = shared_provider.borrow_mut();
            check_and_compact(
                session,
                &request,
                &mut **inner,
                &options.automation,
                &options.automation_state,
                signal,
                false,
            )?;
        }
        Ok(())
    })();
    if persist_state {
        let saved = serde_json::to_value(options.automation_state.state())
            .map_err(|error| error.to_string())
            .and_then(|state| session.record_runtime_state("automation", state));
        if let Err(error) = saved {
            outcome = Err(match outcome {
                Ok(()) => error,
                Err(previous) => format!("{previous}; runtime state save: {error}"),
            });
        }
    }
    options.automation_state.bind_session(session);
    let _ = options.extensions.emit_event(
        &LifecycleEvent::AgentSettled,
        &lifecycle_context(session, options),
        signal,
    );
    outcome
}

// Terminal cleanup must still observe a failed transcript/state transaction.
fn lifecycle_context(session: &Session, options: &RunOptions) -> ExtensionContext {
    extension_context(
        session,
        &options.extensions,
        options.extension_mode,
        options.extension_model.clone(),
    )
    .unwrap_or_else(|_| ExtensionContext {
        context: session.context.clone(),
        workspace: session.workspace.clone(),
        session_id: Some(session.id.clone()),
        mode: options.extension_mode,
        has_ui: matches!(
            options.extension_mode,
            ExtensionMode::Tui | ExtensionMode::Desktop
        ),
        model: options.extension_model.clone(),
        ..Default::default()
    })
}

fn llm_messages(messages: &[Message]) -> Result<Vec<Message>, String> {
    let mut converted = Vec::new();
    for message in messages {
        if let Message::Custom { timestamp, .. } = message {
            if let Some(content) = custom_message_content(message)? {
                converted.push(Message::User(UserMessage {
                    content,
                    timestamp: *timestamp,
                }));
            }
        } else {
            converted.push(message.clone());
        }
    }
    Ok(converted)
}

type SharedProvider<'a> = Rc<RefCell<&'a mut dyn Provider>>;
type SharedSession = Rc<RefCell<Session>>;
type Failure = Rc<RefCell<Option<String>>>;
type Replacement = Rc<RefCell<Option<Context>>>;

struct RuntimeHooks<'a> {
    session: SharedSession,
    provider: SharedProvider<'a>,
    queues: MessageQueues,
    options: AutomationOptions,
    handle: AutomationHandle,
    failure: Failure,
    replacement: Replacement,
    extensions: SharedRegistry,
    extension_mode: ExtensionMode,
    extension_model: Option<ModelIdentity>,
    system_prompt: String,
}

impl LoopHooks for RuntimeHooks<'_> {
    fn transform_context(
        &mut self,
        messages: &[Message],
        signal: &CancellationToken,
    ) -> Vec<Message> {
        let result = extension_context(
            &self.session.borrow(),
            &self.extensions,
            self.extension_mode,
            self.extension_model.clone(),
        )
        .and_then(|mut snapshot| {
            snapshot.context.messages = messages.to_vec();
            snapshot
                .context
                .system_prompt
                .clone_from(&self.system_prompt);
            self.extensions
                .transform_context(messages, &snapshot, signal)
        });
        match result {
            Ok(messages) => messages,
            Err(error) => {
                if !signal.is_cancelled() {
                    *self.failure.borrow_mut() = Some(error);
                    signal.cancel();
                }
                messages.to_vec()
            }
        }
    }
    fn convert_to_llm(&mut self, messages: &[Message]) -> Vec<Message> {
        match llm_messages(messages) {
            Ok(messages) => messages,
            Err(error) => {
                *self.failure.borrow_mut() = Some(error);
                vec![]
            }
        }
    }
    fn prepare_request(
        &mut self,
        context: &Context,
        signal: &CancellationToken,
    ) -> Result<Option<Context>, String> {
        if let Some(error) = self.failure.borrow().clone() {
            return Err(error);
        }
        if !self.options.enabled || signal.is_cancelled() {
            return Ok(None);
        }
        let mut candidate = self.session.borrow().clone();
        let mut request = candidate.context.clone();
        request.system_prompt = context.system_prompt.clone();
        let mut budget_context = request.clone();
        budget_context.messages = llm_messages(&budget_context.messages)?;
        let mut inner = self.provider.borrow_mut();
        match check_and_compact(
            &mut candidate,
            &budget_context,
            &mut **inner,
            &self.options,
            &self.handle,
            signal,
            false,
        ) {
            Ok(changed) => {
                if changed {
                    *self.session.borrow_mut() = candidate.clone();
                }
                request.messages = candidate.context.messages;
                Ok((changed || request.messages != context.messages).then_some(request))
            }
            Err(error) => {
                *self.failure.borrow_mut() = Some(error.clone());
                Err(error)
            }
        }
    }
    fn get_steering_messages(&mut self) -> Vec<Message> {
        self.queues.get_steering_messages()
    }
    fn get_follow_up_messages(&mut self) -> Vec<Message> {
        self.queues.get_follow_up_messages()
    }
    fn take_request_context(&mut self) -> Option<Context> {
        self.replacement.borrow_mut().take()
    }
}

struct RuntimeProvider<'a> {
    session: SharedSession,
    provider: SharedProvider<'a>,
    options: AutomationOptions,
    handle: AutomationHandle,
    failure: Failure,
    replacement: Replacement,
    extensions: SharedRegistry,
    extension_mode: ExtensionMode,
    extension_model: Option<ModelIdentity>,
}

impl Provider for RuntimeProvider<'_> {
    fn complete(
        &mut self,
        context: &Context,
        signal: &CancellationToken,
        update: &mut dyn FnMut(AssistantMessage),
    ) -> Result<AssistantMessage, String> {
        if let Some(error) = self.failure.borrow().clone() {
            return Err(error);
        }
        let tokens = self.options.model.as_ref().map_or_else(
            || estimate_context_tokens(&self.session.borrow(), context),
            |model| estimate_context_tokens_for_model(&self.session.borrow(), context, model),
        );
        let mut inner = self.provider.borrow_mut();
        let mut retry = RetryingProvider::new(
            &mut **inner,
            self.options.retry.clone(),
            self.handle.clone(),
            "agent",
            Some(tokens),
        );
        let mut produced_output = false;
        let result = retry.complete(context, signal, &mut |partial| {
            produced_output |= has_partial_output(&partial);
            update(partial);
        });
        if produced_output || !self.options.enabled || signal.is_cancelled() {
            return result;
        }
        let recover = match &result {
            Err(error) => is_context_overflow(error),
            Ok(message) => {
                (message.stop_reason == StopReason::Error
                    && message
                        .error_message
                        .as_ref()
                        .is_some_and(|error| is_context_overflow(error)))
                    || self
                        .options
                        .model
                        .as_ref()
                        .is_some_and(|model| recoverable_response(message, model))
            }
        };
        if !recover {
            return result;
        }
        let mut candidate = self.session.borrow().clone();
        match check_and_compact(
            &mut candidate,
            context,
            &mut **inner,
            &self.options,
            &self.handle,
            signal,
            true,
        ) {
            Ok(true) => {
                let mut request = candidate.context.clone();
                request.system_prompt = context.system_prompt.clone();
                let mut snapshot = extension_context(
                    &candidate,
                    &self.extensions,
                    self.extension_mode,
                    self.extension_model.clone(),
                )?;
                snapshot.context = request.clone();
                *self.session.borrow_mut() = candidate;
                *self.replacement.borrow_mut() = Some(request.clone());
                request.messages = llm_messages(&self.extensions.transform_context(
                    &request.messages,
                    &snapshot,
                    signal,
                )?)?;
                update(AssistantMessage::text(""));
                let tokens = self.options.model.as_ref().map_or_else(
                    || estimate_context_tokens(&self.session.borrow(), &request),
                    |model| {
                        estimate_context_tokens_for_model(&self.session.borrow(), &request, model)
                    },
                );
                let mut retry = RetryingProvider::new(
                    &mut **inner,
                    self.options.retry.clone(),
                    self.handle.clone(),
                    "overflow_recovery",
                    Some(tokens),
                );
                // One compact-and-retry attempt. A second overflow is returned normally.
                retry.complete(&request, signal, update)
            }
            Ok(false) => result,
            Err(error) => {
                *self.failure.borrow_mut() = Some(error.clone());
                Err(error)
            }
        }
    }
}
