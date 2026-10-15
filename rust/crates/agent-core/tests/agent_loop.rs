//! Offline parity fixtures for the original TypeScript agent-loop contract.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use agent_core::agent_loop::{
    run_agent_loop, run_agent_loop_continue, CompletedTurn, LoopConfig, LoopError, LoopHooks,
    NoopLoopHooks, Provider,
};
use agent_core::tools::{AgentTool, NoopToolHooks};
use agent_core::types::{
    AssistantMessage, CancellationToken, Content, Context, Event, ExecutionMode, Message,
    StopReason, ToolCall, ToolResult, Value,
};
use serde_json::json;

struct FauxProvider {
    responses: VecDeque<Result<AssistantMessage, String>>,
    updates: VecDeque<Vec<AssistantMessage>>,
    requests: Vec<Context>,
    cancel_during_request: bool,
}

impl FauxProvider {
    fn new(responses: Vec<AssistantMessage>) -> Self {
        Self {
            responses: responses.into_iter().map(Ok).collect(),
            updates: VecDeque::new(),
            requests: Vec::new(),
            cancel_during_request: false,
        }
    }
}

impl Provider for FauxProvider {
    fn complete(
        &mut self,
        context: &Context,
        signal: &CancellationToken,
        on_update: &mut dyn FnMut(AssistantMessage),
    ) -> Result<AssistantMessage, String> {
        self.requests.push(context.clone());
        for update in self.updates.pop_front().unwrap_or_default() {
            on_update(update);
        }
        if self.cancel_during_request {
            signal.cancel();
        }
        self.responses
            .pop_front()
            .unwrap_or_else(|| Err("Unexpected provider call".to_owned()))
    }
}

type SharedToolCalls = Arc<Mutex<Vec<String>>>;

struct EchoTool {
    calls: SharedToolCalls,
    terminate: bool,
}

impl AgentTool for EchoTool {
    fn name(&self) -> &str {
        "echo"
    }

    fn validate_arguments(&self, args: &Value) -> Result<(), String> {
        if args.get("value").and_then(Value::as_str).is_some() {
            Ok(())
        } else {
            Err("value must be a string".to_owned())
        }
    }

    fn execute(
        &self,
        _call_id: &str,
        args: &Value,
        _signal: &CancellationToken,
        _on_update: &mut dyn FnMut(ToolResult),
    ) -> Result<ToolResult, String> {
        let value = args["value"].as_str().unwrap().to_owned();
        self.calls.lock().unwrap().push(value.clone());
        let mut result = ToolResult::text(format!("echoed: {value}"));
        result.terminate = self.terminate;
        Ok(result)
    }
}

fn tool_response(values: &[&str], reason: StopReason) -> AssistantMessage {
    let mut response = AssistantMessage::text("");
    response.stop_reason = reason;
    response.content = values
        .iter()
        .enumerate()
        .map(|(index, value)| {
            Content::ToolCall(ToolCall {
                id: format!("tool-{}", index + 1),
                name: "echo".to_owned(),
                arguments: json!({ "value": value }),
            })
        })
        .collect();
    response
}

fn echo_tools(terminate: bool) -> (Vec<Box<dyn AgentTool>>, SharedToolCalls) {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let tools: Vec<Box<dyn AgentTool>> = vec![Box::new(EchoTool {
        calls: calls.clone(),
        terminate,
    })];
    (tools, calls)
}

fn run(
    provider: &mut dyn Provider,
    tools: &[Box<dyn AgentTool>],
    hooks: &mut dyn LoopHooks,
) -> (Vec<Message>, Vec<Event>) {
    let mut events = Vec::new();
    let messages = run_agent_loop(
        vec![Message::user("start")],
        &Context::default(),
        LoopConfig::default(),
        provider,
        tools,
        hooks,
        &NoopToolHooks,
        &CancellationToken::new(),
        &mut |event| events.push(event),
    )
    .unwrap();
    (messages, events)
}

fn event_name(event: &Event) -> &'static str {
    match event {
        Event::AgentStart => "agent_start",
        Event::TurnStart => "turn_start",
        Event::MessageStart { .. } => "message_start",
        Event::MessageUpdate { .. } => "message_update",
        Event::MessageEnd { .. } => "message_end",
        Event::ToolExecutionStart { .. } => "tool_execution_start",
        Event::ToolExecutionUpdate { .. } => "tool_execution_update",
        Event::ToolExecutionEnd { .. } => "tool_execution_end",
        Event::TurnEnd { .. } => "turn_end",
        Event::AgentEnd { .. } => "agent_end",
    }
}

fn roles(messages: &[Message]) -> Vec<&'static str> {
    messages.iter().map(Message::role).collect()
}

#[test]
fn prompt_emits_complete_lifecycle_and_preserves_input_context() {
    let context = Context {
        system_prompt: "System prompt".to_owned(),
        messages: vec![Message::user("earlier")],
    };
    let snapshot = context.clone();
    let mut provider = FauxProvider::new(vec![AssistantMessage::text("hello")]);
    let mut events = Vec::new();
    let messages = run_agent_loop(
        vec![Message::user("new")],
        &context,
        LoopConfig::default(),
        &mut provider,
        &[],
        &mut NoopLoopHooks,
        &NoopToolHooks,
        &CancellationToken::new(),
        &mut |event| events.push(event),
    )
    .unwrap();
    assert_eq!(context, snapshot);
    assert_eq!(roles(&messages), ["user", "assistant"]);
    assert_eq!(provider.requests[0].messages.len(), 2);
    assert_eq!(provider.requests[0].system_prompt, "System prompt");
    assert_eq!(
        events.iter().map(event_name).collect::<Vec<_>>(),
        [
            "agent_start",
            "turn_start",
            "message_start",
            "message_end",
            "message_start",
            "message_end",
            "turn_end",
            "agent_end"
        ]
    );
    assert!(
        matches!(events.last(), Some(Event::AgentEnd { messages: final_messages }) if final_messages == &messages)
    );
}

#[test]
fn streaming_updates_replace_partial_and_emit_one_assistant_start_and_end() {
    let mut provider = FauxProvider::new(vec![AssistantMessage::text("complete")]);
    provider.updates.push_back(vec![
        AssistantMessage::text("c"),
        AssistantMessage::text("comp"),
    ]);
    let (messages, events) = run(&mut provider, &[], &mut NoopLoopHooks);
    assert_eq!(messages[1].text(), "complete");
    let assistant_events: Vec<_> = events
        .iter()
        .filter(|event| matches!(event, Event::MessageStart { message } | Event::MessageUpdate { message } | Event::MessageEnd { message } if matches!(message, Message::Assistant(_))))
        .map(event_name)
        .collect();
    assert_eq!(
        assistant_events,
        ["message_start", "message_update", "message_end"]
    );
}

#[test]
fn tool_results_enter_context_before_the_next_provider_call() {
    let mut provider = FauxProvider::new(vec![
        tool_response(&["hello"], StopReason::ToolUse),
        AssistantMessage::text("done"),
    ]);
    let (tools, calls) = echo_tools(false);
    let (messages, events) = run(&mut provider, &tools, &mut NoopLoopHooks);
    assert_eq!(*calls.lock().unwrap(), ["hello"]);
    assert_eq!(
        roles(&messages),
        ["user", "assistant", "toolResult", "assistant"]
    );
    assert_eq!(
        roles(&provider.requests[1].messages),
        ["user", "assistant", "toolResult"]
    );
    assert_eq!(provider.requests[1].messages[2].text(), "echoed: hello");
    assert_eq!(
        events.iter().map(event_name).collect::<Vec<_>>(),
        [
            "agent_start",
            "turn_start",
            "message_start",
            "message_end",
            "message_start",
            "message_end",
            "tool_execution_start",
            "tool_execution_end",
            "message_start",
            "message_end",
            "turn_end",
            "turn_start",
            "message_start",
            "message_end",
            "turn_end",
            "agent_end"
        ]
    );
}

#[test]
fn length_truncated_tool_calls_fail_without_execution_and_allow_retry() {
    let mut provider = FauxProvider::new(vec![
        tool_response(&["hel"], StopReason::Length),
        AssistantMessage::text("retry done"),
    ]);
    let (tools, calls) = echo_tools(false);
    let (messages, _) = run(&mut provider, &tools, &mut NoopLoopHooks);
    assert!(calls.lock().unwrap().is_empty());
    assert_eq!(provider.requests.len(), 2);
    assert!(matches!(&messages[2], Message::ToolResult(result) if result.is_error));
    assert!(messages[2].text().contains("output token limit"));
}

#[derive(Default)]
struct PollHooks {
    steering_polls: usize,
    follow_up_polls: usize,
    prepare_calls: usize,
    stop: bool,
    completed_roles: Vec<&'static str>,
}

impl LoopHooks for PollHooks {
    fn get_steering_messages(&mut self) -> Vec<Message> {
        self.steering_polls += 1;
        Vec::new()
    }

    fn get_follow_up_messages(&mut self) -> Vec<Message> {
        self.follow_up_polls += 1;
        Vec::new()
    }

    fn prepare_next_turn(&mut self, _turn: &CompletedTurn<'_>) -> Option<Context> {
        self.prepare_calls += 1;
        None
    }

    fn should_stop_after_turn(&mut self, turn: &CompletedTurn<'_>) -> bool {
        self.completed_roles = roles(&turn.context.messages);
        self.stop
    }
}

#[test]
fn provider_failure_is_an_error_message_and_stops_without_queue_polling() {
    let mut provider = FauxProvider::new(Vec::new());
    provider
        .responses
        .push_back(Err("offline failure".to_owned()));
    let mut hooks = PollHooks::default();
    let (messages, events) = run(&mut provider, &[], &mut hooks);
    assert!(
        matches!(&messages[1], Message::Assistant(message) if message.stop_reason == StopReason::Error && message.error_message.as_deref() == Some("offline failure"))
    );
    assert_eq!(hooks.steering_polls, 1);
    assert_eq!(hooks.follow_up_polls, 0);
    assert_eq!(hooks.prepare_calls, 0);
    assert!(
        matches!(events[events.len() - 2], Event::TurnEnd { ref tool_results, .. } if tool_results.is_empty())
    );
    assert!(matches!(events.last(), Some(Event::AgentEnd { .. })));
}

#[test]
fn error_assistant_messages_never_execute_embedded_tool_calls() {
    let mut response = tool_response(&["not executed"], StopReason::Error);
    response.error_message = Some("provider error".to_owned());
    let mut provider = FauxProvider::new(vec![response]);
    let (tools, calls) = echo_tools(false);
    let (messages, events) = run(&mut provider, &tools, &mut NoopLoopHooks);
    assert_eq!(messages.len(), 3);
    assert!(matches!(&messages[2], Message::ToolResult(result) if result.is_error));
    assert_eq!(
        messages[2].text(),
        "Tool call not executed: provider response failed"
    );
    assert!(calls.lock().unwrap().is_empty());
    assert!(!events
        .iter()
        .any(|event| matches!(event, Event::ToolExecutionStart { .. })));
}

#[test]
fn cancelled_before_request_never_calls_the_provider() {
    let signal = CancellationToken::new();
    signal.cancel();
    let mut provider = FauxProvider::new(vec![AssistantMessage::text("unexpected")]);
    let mut events = Vec::new();
    let messages = run_agent_loop(
        vec![Message::user("start")],
        &Context::default(),
        LoopConfig::default(),
        &mut provider,
        &[],
        &mut NoopLoopHooks,
        &NoopToolHooks,
        &signal,
        &mut |event| events.push(event),
    )
    .unwrap();
    assert!(provider.requests.is_empty());
    assert!(
        matches!(&messages[1], Message::Assistant(message) if message.stop_reason == StopReason::Aborted)
    );
    assert!(matches!(events.last(), Some(Event::AgentEnd { .. })));
}

#[test]
fn cancellation_during_provider_normalizes_success_and_skips_tools() {
    let mut provider =
        FauxProvider::new(vec![tool_response(&["not executed"], StopReason::ToolUse)]);
    provider.cancel_during_request = true;
    let (tools, calls) = echo_tools(false);
    let (messages, _) = run(&mut provider, &tools, &mut NoopLoopHooks);
    assert!(calls.lock().unwrap().is_empty());
    assert!(
        matches!(&messages[1], Message::Assistant(message) if message.stop_reason == StopReason::Aborted)
    );
}

#[test]
fn continuation_rejects_empty_and_assistant_context_before_emitting_events() {
    for (context, expected) in [
        (Context::default(), LoopError::EmptyContext),
        (
            Context {
                system_prompt: String::new(),
                messages: vec![Message::Assistant(AssistantMessage::text("previous"))],
            },
            LoopError::AssistantContinuation,
        ),
    ] {
        let mut provider = FauxProvider::new(Vec::new());
        let mut events = Vec::new();
        let result = run_agent_loop_continue(
            &context,
            LoopConfig::default(),
            &mut provider,
            &[],
            &mut NoopLoopHooks,
            &NoopToolHooks,
            &CancellationToken::new(),
            &mut |event| events.push(event),
        );
        assert_eq!(result, Err(expected));
        assert!(events.is_empty());
        assert!(provider.requests.is_empty());
    }
}

#[test]
fn continuation_returns_only_new_messages_without_replaying_user_events() {
    let context = Context {
        system_prompt: String::new(),
        messages: vec![Message::user("existing")],
    };
    let mut provider = FauxProvider::new(vec![AssistantMessage::text("continued")]);
    let mut events = Vec::new();
    let messages = run_agent_loop_continue(
        &context,
        LoopConfig::default(),
        &mut provider,
        &[],
        &mut NoopLoopHooks,
        &NoopToolHooks,
        &CancellationToken::new(),
        &mut |event| events.push(event),
    )
    .unwrap();
    assert_eq!(roles(&messages), ["assistant"]);
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, Event::MessageEnd { .. }))
            .count(),
        1
    );
    assert_eq!(context.messages.len(), 1);
}

#[derive(Default)]
struct ConvertHooks {
    calls: Vec<&'static str>,
    converted_roles: Vec<&'static str>,
}

impl LoopHooks for ConvertHooks {
    fn transform_context(
        &mut self,
        messages: &[Message],
        _signal: &CancellationToken,
    ) -> Vec<Message> {
        self.calls.push("transform");
        messages[messages.len().saturating_sub(2)..].to_vec()
    }

    fn convert_to_llm(&mut self, messages: &[Message]) -> Vec<Message> {
        self.calls.push("convert");
        self.converted_roles = roles(messages);
        messages
            .iter()
            .map(|message| match message {
                Message::Custom { data, .. } => Message::user(data["text"].as_str().unwrap()),
                _ => message.clone(),
            })
            .collect()
    }
}

#[test]
fn transform_precedes_conversion_without_pruning_stored_transcript() {
    let context = Context {
        system_prompt: "system".to_owned(),
        messages: vec![
            Message::user("old"),
            Message::Assistant(AssistantMessage::text("old response")),
            Message::user("recent"),
        ],
    };
    let mut provider = FauxProvider::new(vec![AssistantMessage::text("done")]);
    let mut hooks = ConvertHooks::default();
    let mut events = Vec::new();
    run_agent_loop(
        vec![Message::user("new")],
        &context,
        LoopConfig::default(),
        &mut provider,
        &[],
        &mut hooks,
        &NoopToolHooks,
        &CancellationToken::new(),
        &mut |event| events.push(event),
    )
    .unwrap();
    assert_eq!(hooks.calls, ["transform", "convert"]);
    assert_eq!(provider.requests[0].messages.len(), 2);
    assert_eq!(provider.requests[0].messages[0].text(), "recent");
    assert_eq!(context.messages.len(), 3);
}

#[test]
fn continuation_accepts_custom_messages_with_explicit_provider_conversion() {
    let context = Context {
        system_prompt: String::new(),
        messages: vec![Message::Custom {
            kind: "hook".to_owned(),
            data: json!({"text":"custom content"}),
            timestamp: 0,
        }],
    };
    let mut provider = FauxProvider::new(vec![AssistantMessage::text("done")]);
    let mut hooks = ConvertHooks::default();
    let messages = run_agent_loop_continue(
        &context,
        LoopConfig::default(),
        &mut provider,
        &[],
        &mut hooks,
        &NoopToolHooks,
        &CancellationToken::new(),
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(roles(&messages), ["assistant"]);
    assert_eq!(roles(&provider.requests[0].messages), ["user"]);
    assert_eq!(provider.requests[0].messages[0].text(), "custom content");
}

#[test]
fn default_conversion_filters_custom_ui_messages() {
    let context = Context {
        system_prompt: String::new(),
        messages: vec![Message::Custom {
            kind: "notification".to_owned(),
            data: json!({}),
            timestamp: 0,
        }],
    };
    let mut provider = FauxProvider::new(vec![AssistantMessage::text("done")]);
    run_agent_loop(
        vec![Message::user("new")],
        &context,
        LoopConfig::default(),
        &mut provider,
        &[],
        &mut NoopLoopHooks,
        &NoopToolHooks,
        &CancellationToken::new(),
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(roles(&provider.requests[0].messages), ["user"]);
}

struct QueueHooks {
    steering: VecDeque<Vec<Message>>,
    follow_up: VecDeque<Vec<Message>>,
}

impl LoopHooks for QueueHooks {
    fn has_pending_messages(&self) -> bool {
        self.steering
            .iter()
            .chain(&self.follow_up)
            .any(|batch| !batch.is_empty())
    }

    fn get_steering_messages(&mut self) -> Vec<Message> {
        self.steering.pop_front().unwrap_or_default()
    }

    fn get_follow_up_messages(&mut self) -> Vec<Message> {
        self.follow_up.pop_front().unwrap_or_default()
    }
}

#[test]
fn steering_queued_at_start_is_injected_before_first_assistant() {
    let mut hooks = QueueHooks {
        steering: VecDeque::from([vec![Message::user("already queued")]]),
        follow_up: VecDeque::new(),
    };
    let mut provider = FauxProvider::new(vec![AssistantMessage::text("done")]);
    let (messages, _) = run(&mut provider, &[], &mut hooks);
    assert_eq!(roles(&messages), ["user", "user", "assistant"]);
    assert_eq!(provider.requests[0].messages[1].text(), "already queued");
}

#[test]
fn steering_is_injected_after_every_tool_in_the_current_batch() {
    let mut hooks = QueueHooks {
        steering: VecDeque::from([Vec::new(), vec![Message::user("interrupt")]]),
        follow_up: VecDeque::new(),
    };
    let mut provider = FauxProvider::new(vec![
        tool_response(&["first", "second"], StopReason::ToolUse),
        AssistantMessage::text("done"),
    ]);
    let (tools, calls) = echo_tools(false);
    let (messages, events) = run(&mut provider, &tools, &mut hooks);
    assert_eq!(*calls.lock().unwrap(), ["first", "second"]);
    assert_eq!(
        roles(&messages),
        [
            "user",
            "assistant",
            "toolResult",
            "toolResult",
            "user",
            "assistant"
        ]
    );
    assert_eq!(
        provider.requests[1].messages.last().unwrap().text(),
        "interrupt"
    );
    let messages_in_event_order: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            Event::MessageStart {
                message: Message::ToolResult(result),
            } => Some(result.tool_call_id.clone()),
            Event::MessageStart {
                message: Message::User(message),
            } => Some(Message::User(message.clone()).text()),
            _ => None,
        })
        .collect();
    assert_eq!(
        messages_in_event_order,
        ["start", "tool-1", "tool-2", "interrupt"]
    );
}

#[test]
fn follow_up_waits_until_steering_is_drained_and_agent_would_stop() {
    let mut hooks = QueueHooks {
        steering: VecDeque::from([Vec::new(), vec![Message::user("steer")]]),
        follow_up: VecDeque::from([vec![Message::user("follow up")]]),
    };
    let mut provider = FauxProvider::new(vec![
        AssistantMessage::text("first"),
        AssistantMessage::text("second"),
        AssistantMessage::text("third"),
    ]);
    let (messages, _) = run(&mut provider, &[], &mut hooks);
    assert_eq!(
        roles(&messages),
        [
            "user",
            "assistant",
            "user",
            "assistant",
            "user",
            "assistant"
        ]
    );
    assert_eq!(
        provider.requests[1].messages.last().unwrap().text(),
        "steer"
    );
    assert_eq!(
        provider.requests[2].messages.last().unwrap().text(),
        "follow up"
    );
}

struct PrepareHooks {
    prepare_calls: usize,
    steering_polls: usize,
    queue_in_prepare: bool,
    pending: Vec<Message>,
}

impl LoopHooks for PrepareHooks {
    fn get_steering_messages(&mut self) -> Vec<Message> {
        self.steering_polls += 1;
        std::mem::take(&mut self.pending)
    }

    fn prepare_next_turn(&mut self, turn: &CompletedTurn<'_>) -> Option<Context> {
        self.prepare_calls += 1;
        if self.queue_in_prepare {
            self.pending
                .push(Message::user("queued during preparation"));
        }
        let mut replacement = turn.context.clone();
        replacement.system_prompt = "replacement prompt".to_owned();
        Some(replacement)
    }
}

#[test]
fn prepare_next_turn_updates_context_and_picks_up_new_steering() {
    let mut hooks = PrepareHooks {
        prepare_calls: 0,
        steering_polls: 0,
        queue_in_prepare: true,
        pending: Vec::new(),
    };
    let mut provider = FauxProvider::new(vec![
        tool_response(&["hello"], StopReason::ToolUse),
        AssistantMessage::text("done"),
    ]);
    let (tools, _) = echo_tools(false);
    run(&mut provider, &tools, &mut hooks);
    assert_eq!(hooks.prepare_calls, 1);
    assert_eq!(provider.requests[1].system_prompt, "replacement prompt");
    assert_eq!(
        provider.requests[1].messages.last().unwrap().text(),
        "queued during preparation"
    );
}

#[test]
fn stop_after_turn_sees_appended_results_and_prevents_queue_polling() {
    let mut hooks = PollHooks {
        stop: true,
        ..PollHooks::default()
    };
    let mut provider = FauxProvider::new(vec![tool_response(&["hello"], StopReason::ToolUse)]);
    let (tools, calls) = echo_tools(false);
    let (messages, _) = run(&mut provider, &tools, &mut hooks);
    assert_eq!(*calls.lock().unwrap(), ["hello"]);
    assert_eq!(hooks.completed_roles, ["user", "assistant", "toolResult"]);
    assert_eq!(messages.len(), 3);
    assert_eq!(hooks.steering_polls, 1);
    assert_eq!(hooks.follow_up_polls, 0);
    assert_eq!(hooks.prepare_calls, 0);
    assert_eq!(provider.requests.len(), 1);
}

#[test]
fn all_terminating_tool_results_stop_before_another_provider_call() {
    let mut provider = FauxProvider::new(vec![tool_response(
        &["first", "second"],
        StopReason::ToolUse,
    )]);
    let (tools, calls) = echo_tools(true);
    let (messages, _) = run(&mut provider, &tools, &mut NoopLoopHooks);
    assert_eq!(*calls.lock().unwrap(), ["first", "second"]);
    assert_eq!(provider.requests.len(), 1);
    assert_eq!(messages.len(), 4);
}

#[test]
fn turn_limit_returns_error_after_last_completed_turn_and_emits_agent_end() {
    let mut provider = FauxProvider::new(vec![tool_response(&["repeat"], StopReason::ToolUse)]);
    let (tools, _) = echo_tools(false);
    let mut events = Vec::new();
    let result = run_agent_loop(
        vec![Message::user("start")],
        &Context::default(),
        LoopConfig {
            execution_mode: ExecutionMode::Sequential,
            max_turns: 1,
        },
        &mut provider,
        &tools,
        &mut NoopLoopHooks,
        &NoopToolHooks,
        &CancellationToken::new(),
        &mut |event| events.push(event),
    );
    assert_eq!(result, Err(LoopError::MaxTurnsExceeded { max_turns: 1 }));
    assert_eq!(provider.requests.len(), 1);
    assert!(matches!(&events[events.len() - 2], Event::TurnEnd { .. }));
    assert!(matches!(events.last(), Some(Event::AgentEnd { messages }) if messages.len() == 3));
}

#[test]
fn zero_turn_limit_never_calls_provider_and_still_ends_lifecycle() {
    let mut provider = FauxProvider::new(Vec::new());
    let mut events = Vec::new();
    let result = run_agent_loop(
        vec![Message::user("start")],
        &Context::default(),
        LoopConfig {
            max_turns: 0,
            ..LoopConfig::default()
        },
        &mut provider,
        &[],
        &mut NoopLoopHooks,
        &NoopToolHooks,
        &CancellationToken::new(),
        &mut |event| events.push(event),
    );
    assert_eq!(result, Err(LoopError::MaxTurnsExceeded { max_turns: 0 }));
    assert!(provider.requests.is_empty());
    assert!(matches!(events.last(), Some(Event::AgentEnd { messages }) if messages.len() == 1));
}

#[test]
fn zero_budget_or_pre_cancelled_run_preserves_both_queues() {
    for cancelled in [false, true] {
        let signal = CancellationToken::new();
        if cancelled {
            signal.cancel();
        }
        let mut hooks = QueueHooks {
            steering: VecDeque::from([vec![Message::user("steer later")]]),
            follow_up: VecDeque::from([vec![Message::user("follow later")]]),
        };
        let mut provider = FauxProvider::new(Vec::new());
        let mut events = Vec::new();
        let result = run_agent_loop(
            vec![Message::user("start")],
            &Context::default(),
            LoopConfig {
                max_turns: usize::from(cancelled),
                ..Default::default()
            },
            &mut provider,
            &[],
            &mut hooks,
            &NoopToolHooks,
            &signal,
            &mut |event| events.push(event),
        );
        assert_eq!(result.is_ok(), cancelled);
        assert!(provider.requests.is_empty());
        assert_eq!(hooks.steering.len(), 1);
        assert_eq!(hooks.follow_up.len(), 1);
        assert!(!events.iter().any(|event| matches!(event, Event::MessageEnd { message } if message.text().ends_with("later"))));
    }
}

#[test]
fn exhausted_budget_retains_queues_for_natural_stop_and_tool_continuation() {
    for tool_continuation in [false, true] {
        let response = if tool_continuation {
            tool_response(&["first"], StopReason::ToolUse)
        } else {
            AssistantMessage::text("done")
        };
        let mut provider = FauxProvider::new(vec![response]);
        let (tools, _) = echo_tools(false);
        let mut hooks = QueueHooks {
            steering: VecDeque::from([Vec::new(), vec![Message::user("steer later")]]),
            follow_up: VecDeque::from([vec![Message::user("follow later")]]),
        };
        let result = run_agent_loop(
            vec![Message::user("start")],
            &Context::default(),
            LoopConfig {
                max_turns: 1,
                ..Default::default()
            },
            &mut provider,
            &tools,
            &mut hooks,
            &NoopToolHooks,
            &CancellationToken::new(),
            &mut |_| {},
        );
        assert_eq!(result, Err(LoopError::MaxTurnsExceeded { max_turns: 1 }));
        assert_eq!(provider.requests.len(), 1);
        assert_eq!(hooks.steering.len(), 1);
        assert_eq!(hooks.follow_up.len(), 1);
    }
}

#[test]
fn exhausted_budget_without_eligible_queue_finishes_successfully() {
    let mut hooks = QueueHooks {
        steering: VecDeque::new(),
        follow_up: VecDeque::new(),
    };
    let mut provider = FauxProvider::new(vec![AssistantMessage::text("done")]);
    let messages = run_agent_loop(
        vec![Message::user("start")],
        &Context::default(),
        LoopConfig {
            max_turns: 1,
            ..Default::default()
        },
        &mut provider,
        &[],
        &mut hooks,
        &NoopToolHooks,
        &CancellationToken::new(),
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(roles(&messages), ["user", "assistant"]);
    assert_eq!(provider.requests.len(), 1);
}

#[test]
fn cancellation_at_turn_end_preserves_unadmitted_queue_items() {
    for tool_continuation in [false, true] {
        let response = if tool_continuation {
            tool_response(&["first"], StopReason::ToolUse)
        } else {
            AssistantMessage::text("done")
        };
        let mut provider = FauxProvider::new(vec![response]);
        let (tools, _) = echo_tools(false);
        let signal = CancellationToken::new();
        let mut hooks = QueueHooks {
            steering: VecDeque::from([Vec::new(), vec![Message::user("steer later")]]),
            follow_up: VecDeque::from([vec![Message::user("follow later")]]),
        };
        let messages = run_agent_loop(
            vec![Message::user("start")],
            &Context::default(),
            LoopConfig::default(),
            &mut provider,
            &tools,
            &mut hooks,
            &NoopToolHooks,
            &signal,
            &mut |event| {
                if matches!(event, Event::TurnEnd { .. }) {
                    signal.cancel();
                }
            },
        )
        .unwrap();
        assert_eq!(provider.requests.len(), 1);
        assert_eq!(hooks.steering.len(), 1);
        assert_eq!(hooks.follow_up.len(), 1);
        assert!(!messages
            .iter()
            .any(|message| message.text().ends_with("later")));
    }
}

#[test]
fn failed_stream_replaces_its_partial_without_a_duplicate_message_start() {
    let mut provider = FauxProvider::new(Vec::new());
    provider
        .responses
        .push_back(Err("stream failure".to_owned()));
    provider
        .updates
        .push_back(vec![AssistantMessage::text("partial")]);
    let (messages, events) = run(&mut provider, &[], &mut NoopLoopHooks);
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[1].text(), "partial");
    assert!(
        matches!(&messages[1], Message::Assistant(message) if message.stop_reason == StopReason::Error)
    );
    let assistant_events: Vec<_> = events
        .iter()
        .filter(|event| matches!(event, Event::MessageStart { message } | Event::MessageEnd { message } if matches!(message, Message::Assistant(_))))
        .map(event_name)
        .collect();
    assert_eq!(assistant_events, ["message_start", "message_end"]);
}

#[test]
fn aborted_stream_preserves_the_generated_partial_text() {
    let mut provider = FauxProvider::new(Vec::new());
    provider
        .responses
        .push_back(Err("Operation aborted".to_owned()));
    provider
        .updates
        .push_back(vec![AssistantMessage::text("partial before abort")]);
    provider.cancel_during_request = true;
    let (messages, events) = run(&mut provider, &[], &mut NoopLoopHooks);
    assert_eq!(messages[1].text(), "partial before abort");
    assert!(
        matches!(&messages[1], Message::Assistant(message) if message.stop_reason == StopReason::Aborted)
    );
    assert!(matches!(events.last(), Some(Event::AgentEnd { .. })));
}

struct OneAtATimeHooks {
    queued: VecDeque<Message>,
    polls: usize,
    prepare_calls: usize,
}

impl LoopHooks for OneAtATimeHooks {
    fn get_steering_messages(&mut self) -> Vec<Message> {
        self.polls += 1;
        if self.polls == 1 {
            return Vec::new();
        }
        self.queued.pop_front().into_iter().collect()
    }

    fn prepare_next_turn(&mut self, _turn: &CompletedTurn<'_>) -> Option<Context> {
        self.prepare_calls += 1;
        if self.prepare_calls == 1 {
            self.queued.push_back(Message::user("third"));
        }
        None
    }
}

#[test]
fn preparation_does_not_drain_an_extra_one_at_a_time_steering_message() {
    let mut provider = FauxProvider::new(vec![
        AssistantMessage::text("one"),
        AssistantMessage::text("two"),
        AssistantMessage::text("three"),
        AssistantMessage::text("four"),
    ]);
    let mut hooks = OneAtATimeHooks {
        queued: VecDeque::from([Message::user("first"), Message::user("second")]),
        polls: 0,
        prepare_calls: 0,
    };
    run(&mut provider, &[], &mut hooks);
    assert_eq!(provider.requests.len(), 4);
    assert_eq!(
        provider.requests[1].messages.last().unwrap().text(),
        "first"
    );
    assert_eq!(
        provider.requests[2].messages.last().unwrap().text(),
        "second"
    );
    assert_eq!(
        provider.requests[3].messages.last().unwrap().text(),
        "third"
    );
    assert_eq!(hooks.polls, 5);
    assert_eq!(hooks.prepare_calls, 3);
}

#[test]
fn cancellation_after_tool_result_ends_with_aborted_assistant_without_new_request() {
    let signal = CancellationToken::new();
    let mut provider = FauxProvider::new(vec![tool_response(&["first"], StopReason::ToolUse)]);
    let (tools, calls) = echo_tools(false);
    let mut events = Vec::new();
    let messages = run_agent_loop(
        vec![Message::user("start")],
        &Context::default(),
        LoopConfig::default(),
        &mut provider,
        &tools,
        &mut NoopLoopHooks,
        &NoopToolHooks,
        &signal,
        &mut |event| {
            if matches!(event, Event::ToolExecutionEnd { .. }) {
                signal.cancel();
            }
            events.push(event);
        },
    )
    .unwrap();
    assert_eq!(*calls.lock().unwrap(), ["first"]);
    assert_eq!(provider.requests.len(), 1);
    assert_eq!(
        roles(&messages),
        ["user", "assistant", "toolResult", "assistant"]
    );
    assert!(
        matches!(messages.last(), Some(Message::Assistant(message)) if message.stop_reason == StopReason::Aborted)
    );
    assert!(matches!(events.last(), Some(Event::AgentEnd { .. })));
}

#[test]
fn cancelled_tool_batch_closes_unexecuted_calls_and_can_resume_after_reload() {
    let signal = CancellationToken::new();
    let mut provider = FauxProvider::new(vec![tool_response(
        &["first", "must not execute"],
        StopReason::ToolUse,
    )]);
    let (tools, calls) = echo_tools(false);
    let mut events = Vec::new();
    let messages = run_agent_loop(
        vec![Message::user("start")],
        &Context::default(),
        LoopConfig {
            execution_mode: ExecutionMode::Sequential,
            ..LoopConfig::default()
        },
        &mut provider,
        &tools,
        &mut NoopLoopHooks,
        &NoopToolHooks,
        &signal,
        &mut |event| {
            if matches!(&event, Event::ToolExecutionEnd { tool_call_id, .. } if tool_call_id == "tool-1") {
                signal.cancel();
            }
            events.push(event);
        },
    )
    .unwrap();

    assert_eq!(*calls.lock().unwrap(), ["first"]);
    assert_eq!(provider.requests.len(), 1);
    assert_eq!(
        roles(&messages),
        ["user", "assistant", "toolResult", "toolResult", "assistant"]
    );
    let Message::ToolResult(first) = &messages[2] else {
        panic!("First result missing")
    };
    assert!(!first.is_error);
    assert_eq!(first.content, vec![Content::text("echoed: first")]);
    let Message::ToolResult(omitted) = &messages[3] else {
        panic!("Omitted result missing")
    };
    assert_eq!(omitted.tool_call_id, "tool-2");
    assert_eq!(omitted.tool_name, "echo");
    assert_eq!(omitted.content, vec![Content::text("Operation aborted")]);
    assert_eq!(omitted.details, json!({}));
    assert_eq!(omitted.usage, None);
    assert!(omitted.added_tool_names.is_empty());
    assert!(omitted.is_error);
    assert!(
        matches!(messages.last(), Some(Message::Assistant(message)) if message.stop_reason == StopReason::Aborted)
    );

    let omitted_events: Vec<_> = events.iter().filter(|event| matches!(event, Event::MessageStart { message: Message::ToolResult(result) } | Event::MessageEnd { message: Message::ToolResult(result) } if result.tool_call_id == "tool-2")).map(event_name).collect();
    assert_eq!(omitted_events, ["message_start", "message_end"]);
    assert!(!events.iter().any(|event| matches!(event, Event::ToolExecutionStart { tool_call_id, .. } if tool_call_id == "tool-2")));
    assert!(events.iter().any(|event| matches!(event, Event::TurnEnd { tool_results, .. } if tool_results.len() == 2 && tool_results[1] == *omitted)));

    // A new user prompt permits continuation after persisted aborted assistant state.
    let saved = serde_json::to_vec(&Context {
        system_prompt: String::new(),
        messages,
    })
    .unwrap();
    let mut restored: Context = serde_json::from_slice(&saved).unwrap();
    restored.messages.push(Message::user("resume"));
    let mut resumed_provider = FauxProvider::new(vec![AssistantMessage::text("resumed")]);
    let resumed_messages = run_agent_loop_continue(
        &restored,
        LoopConfig::default(),
        &mut resumed_provider,
        &tools,
        &mut NoopLoopHooks,
        &NoopToolHooks,
        &CancellationToken::new(),
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(resumed_provider.requests.len(), 1);
    assert_eq!(resumed_messages[0].text(), "resumed");
    let request = &resumed_provider.requests[0];
    let Message::Assistant(tool_message) = &request.messages[1] else {
        panic!("Tool-call message missing")
    };
    for call in tool_message.tool_calls() {
        assert_eq!(request.messages.iter().filter(|message| matches!(message, Message::ToolResult(result) if result.tool_call_id == call.id)).count(), 1);
    }
    assert_eq!(request.messages[2].text(), "echoed: first");
    assert_eq!(request.messages[3].text(), "Operation aborted");
    assert_eq!(*calls.lock().unwrap(), ["first"]);
}

#[test]
fn provider_completion_cancel_closes_calls_before_resuming_saved_context() {
    let mut provider = FauxProvider::new(vec![tool_response(
        &["must not execute"],
        StopReason::ToolUse,
    )]);
    provider.cancel_during_request = true;
    let (tools, calls) = echo_tools(false);
    let (messages, events) = run(&mut provider, &tools, &mut NoopLoopHooks);
    assert_eq!(provider.requests.len(), 1);
    assert!(calls.lock().unwrap().is_empty());
    assert_eq!(roles(&messages), ["user", "assistant", "toolResult"]);
    assert_eq!(messages[2].text(), "Operation aborted");
    assert!(
        matches!(&messages[1], Message::Assistant(message) if message.stop_reason == StopReason::Aborted)
    );
    assert!(
        matches!(&messages[2], Message::ToolResult(result) if result.is_error && result.tool_call_id == "tool-1")
    );
    assert!(!events.iter().any(|event| matches!(
        event,
        Event::ToolExecutionStart { .. } | Event::ToolExecutionEnd { .. }
    )));
    assert!(events.iter().any(|event| matches!(event, Event::TurnEnd { tool_results, .. } if tool_results.len() == 1 && tool_results[0].tool_call_id == "tool-1")));

    let restored: Context = serde_json::from_slice(
        &serde_json::to_vec(&Context {
            system_prompt: String::new(),
            messages,
        })
        .unwrap(),
    )
    .unwrap();
    let mut resumed_provider = FauxProvider::new(vec![AssistantMessage::text("resumed")]);
    run_agent_loop_continue(
        &restored,
        LoopConfig::default(),
        &mut resumed_provider,
        &tools,
        &mut NoopLoopHooks,
        &NoopToolHooks,
        &CancellationToken::new(),
        &mut |_| {},
    )
    .unwrap();
    let request = &resumed_provider.requests[0];
    let Message::Assistant(assistant) = &request.messages[1] else {
        panic!("Assistant missing")
    };
    let Message::ToolResult(result) = &request.messages[2] else {
        panic!("Tool result missing")
    };
    assert_eq!(assistant.tool_calls()[0].id, result.tool_call_id);
    assert_eq!(request.messages[2].text(), "Operation aborted");
    assert!(calls.lock().unwrap().is_empty());
}

#[test]
fn partial_provider_error_preserves_text_and_closes_calls_without_execution() {
    let mut partial = tool_response(&["must not execute"], StopReason::ToolUse);
    partial
        .content
        .insert(0, Content::text("generated before failure"));
    let mut provider = FauxProvider::new(Vec::new());
    provider.updates.push_back(vec![partial]);
    provider
        .responses
        .push_back(Err("stream failure".to_owned()));
    let (tools, calls) = echo_tools(false);
    let (messages, events) = run(&mut provider, &tools, &mut NoopLoopHooks);
    assert_eq!(messages[1].text(), "generated before failure");
    assert!(
        matches!(&messages[1], Message::Assistant(message) if message.stop_reason == StopReason::Error && message.error_message.as_deref() == Some("stream failure"))
    );
    assert!(
        matches!(&messages[2], Message::ToolResult(result) if result.is_error && result.tool_call_id == "tool-1")
    );
    assert_eq!(
        messages[2].text(),
        "Tool call not executed: provider response failed"
    );
    assert_eq!(provider.requests.len(), 1);
    assert!(calls.lock().unwrap().is_empty());
    assert!(!events.iter().any(|event| matches!(
        event,
        Event::ToolExecutionStart { .. } | Event::ToolExecutionEnd { .. }
    )));
    assert!(events.iter().any(|event| matches!(event, Event::TurnEnd { tool_results, .. } if tool_results.len() == 1 && tool_results[0].tool_call_id == "tool-1")));
    assert!(matches!(events.last(), Some(Event::AgentEnd { .. })));
}
