//! Agent orchestration with provider streaming snapshots and cooperative cancellation.
//!
//! The transcript stays in agent-message form until the provider boundary. Queue
//! polling happens only after a complete tool batch, matching the TypeScript loop.

use std::error::Error;
use std::fmt::{Display, Formatter};

use crate::tools::{
    execute_tool_calls, fail_tool_calls_from_truncated_message, AgentTool, ToolHooks,
};
use crate::types::{
    timestamp, AssistantMessage, CancellationToken, Content, Context, Event, ExecutionMode,
    Message, StopReason, ToolCall, ToolResultMessage, Value,
};

/// Providers report optional partial snapshots before returning a final message.
/// Request failures are normalized by the loop into an error assistant message.
pub trait Provider {
    fn complete(
        &mut self,
        context: &Context,
        signal: &CancellationToken,
        on_update: &mut dyn FnMut(AssistantMessage),
    ) -> Result<AssistantMessage, String>;

    /// Summary requests never execute tools. Transport providers must omit tool definitions.
    fn complete_summary(
        &mut self,
        context: &Context,
        signal: &CancellationToken,
        on_update: &mut dyn FnMut(AssistantMessage),
    ) -> Result<AssistantMessage, String> {
        self.complete(context, signal, on_update)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LoopConfig {
    pub execution_mode: ExecutionMode,
    /// A bound on provider calls, including calls triggered by queued messages.
    pub max_turns: usize,
}

impl Default for LoopConfig {
    fn default() -> Self {
        Self {
            execution_mode: ExecutionMode::Auto,
            max_turns: 128,
        }
    }
}

/// Completed-turn hooks observe the appended assistant response and tool results.
pub struct CompletedTurn<'a> {
    pub message: &'a AssistantMessage,
    pub tool_results: &'a [ToolResultMessage],
    pub context: &'a Context,
    pub new_messages: &'a [Message],
}

pub trait LoopHooks {
    /// Runs at a fully paired request boundary, including the initial queued prompt prefix.
    /// A replacement changes the actual loop context before provider conversion.
    fn prepare_request(
        &mut self,
        _context: &Context,
        _signal: &CancellationToken,
    ) -> Result<Option<Context>, String> {
        Ok(None)
    }

    /// Consume a request-context replacement made by bounded provider recovery.
    fn take_request_context(&mut self) -> Option<Context> {
        None
    }

    /// Called before conversion; the stored transcript is not replaced.
    fn transform_context(
        &mut self,
        messages: &[Message],
        _signal: &CancellationToken,
    ) -> Vec<Message> {
        messages.to_vec()
    }

    /// Apps can convert custom messages here. UI-only messages are omitted by default.
    fn convert_to_llm(&mut self, messages: &[Message]) -> Vec<Message> {
        messages
            .iter()
            .filter(|message| !matches!(message, Message::Custom { .. }))
            .cloned()
            .collect()
    }

    fn get_steering_messages(&mut self) -> Vec<Message> {
        Vec::new()
    }

    fn get_follow_up_messages(&mut self) -> Vec<Message> {
        Vec::new()
    }

    /// Reports eligible queued work without consuming it. Paused or held items
    /// should not require another turn. Used only at an exhausted turn budget.
    fn has_pending_messages(&self) -> bool {
        false
    }

    /// Called only when another turn is needed. `None` preserves the runtime context.
    fn prepare_next_turn(&mut self, _turn: &CompletedTurn<'_>) -> Option<Context> {
        None
    }

    /// Runs after `turn_end`, before either message queue is polled.
    fn should_stop_after_turn(&mut self, _turn: &CompletedTurn<'_>) -> bool {
        false
    }
}

pub struct NoopLoopHooks;

impl LoopHooks for NoopLoopHooks {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LoopError {
    EmptyContext,
    AssistantContinuation,
    MaxTurnsExceeded { max_turns: usize },
}

impl Display for LoopError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyContext => formatter.write_str("Cannot continue: no messages in context"),
            Self::AssistantContinuation => {
                formatter.write_str("Cannot continue from message role: assistant")
            }
            Self::MaxTurnsExceeded { max_turns } => {
                write!(
                    formatter,
                    "Agent loop reached its limit of {max_turns} turns"
                )
            }
        }
    }
}

impl Error for LoopError {}

/// Starts a prompt run without changing the caller's context.
#[allow(clippy::too_many_arguments)]
pub fn run_agent_loop(
    prompts: Vec<Message>,
    context: &Context,
    config: LoopConfig,
    provider: &mut dyn Provider,
    tools: &[Box<dyn AgentTool>],
    hooks: &mut dyn LoopHooks,
    tool_hooks: &dyn ToolHooks,
    signal: &CancellationToken,
    emit: &mut dyn FnMut(Event),
) -> Result<Vec<Message>, LoopError> {
    let mut current_context = context.clone();
    current_context.messages.extend(prompts.iter().cloned());
    emit(Event::AgentStart);
    emit(Event::TurnStart);
    for prompt in &prompts {
        emit_message(prompt, emit);
    }
    run_loop(
        current_context,
        prompts,
        config,
        provider,
        tools,
        hooks,
        tool_hooks,
        signal,
        emit,
    )
}

/// Continues an existing transcript without replaying existing message events.
/// Custom final messages are accepted; their provider conversion is the caller's hook.
#[allow(clippy::too_many_arguments)]
pub fn run_agent_loop_continue(
    context: &Context,
    config: LoopConfig,
    provider: &mut dyn Provider,
    tools: &[Box<dyn AgentTool>],
    hooks: &mut dyn LoopHooks,
    tool_hooks: &dyn ToolHooks,
    signal: &CancellationToken,
    emit: &mut dyn FnMut(Event),
) -> Result<Vec<Message>, LoopError> {
    match context.messages.last() {
        None => return Err(LoopError::EmptyContext),
        Some(Message::Assistant(_)) => return Err(LoopError::AssistantContinuation),
        Some(_) => {}
    }
    emit(Event::AgentStart);
    emit(Event::TurnStart);
    run_loop(
        context.clone(),
        Vec::new(),
        config,
        provider,
        tools,
        hooks,
        tool_hooks,
        signal,
        emit,
    )
}

#[allow(clippy::too_many_arguments)]
fn run_loop(
    mut context: Context,
    mut new_messages: Vec<Message>,
    config: LoopConfig,
    provider: &mut dyn Provider,
    tools: &[Box<dyn AgentTool>],
    hooks: &mut dyn LoopHooks,
    tool_hooks: &dyn ToolHooks,
    signal: &CancellationToken,
    emit: &mut dyn FnMut(Event),
) -> Result<Vec<Message>, LoopError> {
    // Queue hooks consume entries. Never dequeue work that this run cannot admit.
    let mut pending_messages = if config.max_turns > 0 && !signal.is_cancelled() {
        hooks.get_steering_messages()
    } else {
        Vec::new()
    };
    let mut completed_turn: Option<(AssistantMessage, Vec<ToolResultMessage>)> = None;
    let mut turns = 0;

    loop {
        let mut has_more_tool_calls = true;
        while has_more_tool_calls || !pending_messages.is_empty() {
            if turns >= config.max_turns {
                emit(Event::AgentEnd {
                    messages: new_messages,
                });
                return Err(LoopError::MaxTurnsExceeded {
                    max_turns: config.max_turns,
                });
            }

            if let Some((message, tool_results)) = &completed_turn {
                let turn = CompletedTurn {
                    message,
                    tool_results,
                    context: &context,
                    new_messages: &new_messages,
                };
                if let Some(replacement) = hooks.prepare_next_turn(&turn) {
                    context = replacement;
                }
                // Preparation may itself queue steering. Poll again only when
                // nothing was already drained (preserves one-at-a-time hooks).
                if pending_messages.is_empty() && !signal.is_cancelled() {
                    pending_messages = hooks.get_steering_messages();
                }
                emit(Event::TurnStart);
            }

            for message in pending_messages.drain(..) {
                emit_message(&message, emit);
                context.messages.push(message.clone());
                new_messages.push(message);
            }

            let message = stream_assistant_response(&mut context, provider, hooks, signal, emit);
            turns += 1;
            new_messages.push(Message::Assistant(message.clone()));

            if matches!(message.stop_reason, StopReason::Error | StopReason::Aborted) {
                let mut tool_results = Vec::new();
                close_unexecuted_tool_calls(
                    &message.tool_calls(),
                    &mut tool_results,
                    if message.stop_reason == StopReason::Aborted {
                        "Operation aborted"
                    } else {
                        "Tool call not executed: provider response failed"
                    },
                    emit,
                );
                for result in &tool_results {
                    let result_message = Message::ToolResult(result.clone());
                    context.messages.push(result_message.clone());
                    new_messages.push(result_message);
                }
                emit(Event::TurnEnd {
                    message,
                    tool_results,
                });
                return finish(new_messages, emit);
            }

            let tool_calls = message.tool_calls();
            let tool_results = if tool_calls.is_empty() {
                has_more_tool_calls = false;
                Vec::new()
            } else {
                let mut executed = if message.stop_reason == StopReason::Length {
                    fail_tool_calls_from_truncated_message(&tool_calls, emit)
                } else {
                    execute_tool_calls(
                        tools,
                        &context,
                        &message,
                        config.execution_mode,
                        tool_hooks,
                        signal,
                        emit,
                    )
                };
                // Native persistence needs every advertised call to have a result.
                // Unlike the TypeScript executor's cancelled prefix, close omitted
                // calls with error artifacts without executing their tools.
                if signal.is_cancelled()
                    && close_unexecuted_tool_calls(
                        &tool_calls,
                        &mut executed.messages,
                        "Operation aborted",
                        emit,
                    )
                {
                    executed.terminate = false;
                }
                has_more_tool_calls = !executed.terminate;
                for result in &executed.messages {
                    let result_message = Message::ToolResult(result.clone());
                    context.messages.push(result_message.clone());
                    new_messages.push(result_message);
                }
                executed.messages
            };

            emit(Event::TurnEnd {
                message: message.clone(),
                tool_results: tool_results.clone(),
            });
            let turn = CompletedTurn {
                message: &message,
                tool_results: &tool_results,
                context: &context,
                new_messages: &new_messages,
            };
            if hooks.should_stop_after_turn(&turn) {
                return finish(new_messages, emit);
            }
            completed_turn = Some((message, tool_results));
            if turns < config.max_turns && !signal.is_cancelled() {
                pending_messages = hooks.get_steering_messages();
            }
        }

        // Inspect without consuming: eligible queued work still requires a turn,
        // while a naturally completed answer with no eligible work succeeds.
        if turns >= config.max_turns && !signal.is_cancelled() && hooks.has_pending_messages() {
            emit(Event::AgentEnd {
                messages: new_messages,
            });
            return Err(LoopError::MaxTurnsExceeded {
                max_turns: config.max_turns,
            });
        }
        if turns >= config.max_turns || signal.is_cancelled() {
            return finish(new_messages, emit);
        }
        pending_messages = hooks.get_follow_up_messages();
        if pending_messages.is_empty() {
            return finish(new_messages, emit);
        }
    }
}

fn stream_assistant_response(
    context: &mut Context,
    provider: &mut dyn Provider,
    hooks: &mut dyn LoopHooks,
    signal: &CancellationToken,
    emit: &mut dyn FnMut(Event),
) -> AssistantMessage {
    let preparation = hooks.prepare_request(context, signal);
    let preparation_error = match preparation {
        Ok(Some(replacement)) => {
            *context = replacement;
            None
        }
        Ok(None) => None,
        Err(error) => Some(error),
    };
    let transformed = hooks.transform_context(&context.messages, signal);
    let request = Context {
        system_prompt: context.system_prompt.clone(),
        messages: hooks.convert_to_llm(&transformed),
    };
    let mut partial_added = false;
    let response = if let Some(error) = preparation_error {
        Ok(AssistantMessage::error(error, signal.is_cancelled()))
    } else if signal.is_cancelled() {
        Ok(AssistantMessage::error("Operation aborted", true))
    } else {
        provider.complete(&request, signal, &mut |partial| {
            let partial = Message::Assistant(partial);
            if partial_added {
                if let Some(last) = context.messages.last_mut() {
                    *last = partial.clone();
                }
                emit(Event::MessageUpdate { message: partial });
            } else {
                context.messages.push(partial.clone());
                partial_added = true;
                emit(Event::MessageStart { message: partial });
            }
        })
    };
    let mut message = response.unwrap_or_else(|error| {
        if partial_added {
            if let Some(Message::Assistant(partial)) = context.messages.last() {
                let mut failed = partial.clone();
                failed.stop_reason = if signal.is_cancelled() {
                    StopReason::Aborted
                } else {
                    StopReason::Error
                };
                failed.error_message = Some(error);
                return failed;
            }
        }
        AssistantMessage::error(error, signal.is_cancelled())
    });
    if signal.is_cancelled() && message.stop_reason != StopReason::Aborted {
        message.stop_reason = StopReason::Aborted;
        message.error_message = Some("Operation aborted".to_owned());
    }
    if let Some(replacement) = hooks.take_request_context() {
        *context = replacement;
        if partial_added {
            // Preserve the existing streaming event slot without replacing a real user message.
            context.messages.push(Message::Assistant(message.clone()));
        }
    }
    let final_message = Message::Assistant(message.clone());
    if partial_added {
        if let Some(last) = context.messages.last_mut() {
            *last = final_message.clone();
        }
    } else {
        context.messages.push(final_message.clone());
        emit(Event::MessageStart {
            message: final_message.clone(),
        });
    }
    emit(Event::MessageEnd {
        message: final_message,
    });
    message
}

fn emit_message(message: &Message, emit: &mut dyn FnMut(Event)) {
    emit(Event::MessageStart {
        message: message.clone(),
    });
    emit(Event::MessageEnd {
        message: message.clone(),
    });
}

/// Persistence-only closure: omitted calls have not started execution, so only
/// message artifacts are emitted, never tool execution lifecycle events.
fn close_unexecuted_tool_calls(
    calls: &[ToolCall],
    results: &mut Vec<ToolResultMessage>,
    error: &str,
    emit: &mut dyn FnMut(Event),
) -> bool {
    let mut added = false;
    for call in calls {
        if results.iter().any(|result| result.tool_call_id == call.id) {
            continue;
        }
        let result = ToolResultMessage {
            tool_call_id: call.id.clone(),
            tool_name: call.name.clone(),
            content: vec![Content::text(error)],
            details: Value::Object(Default::default()),
            usage: None,
            added_tool_names: Vec::new(),
            is_error: true,
            timestamp: timestamp(),
        };
        emit_message(&Message::ToolResult(result.clone()), emit);
        results.push(result);
        added = true;
    }
    added
}

fn finish(
    new_messages: Vec<Message>,
    emit: &mut dyn FnMut(Event),
) -> Result<Vec<Message>, LoopError> {
    emit(Event::AgentEnd {
        messages: new_messages.clone(),
    });
    Ok(new_messages)
}
