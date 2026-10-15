use agent_core::agent_loop::{run_agent_loop, LoopConfig, LoopHooks, Provider};
use agent_core::tools::{AgentTool, BeforeToolCallContext, BeforeToolCallResult, ToolHooks};
use agent_core::types::{
    AssistantMessage, CancellationToken, Content, Context, Event, Message, StopReason, ToolCall,
    ToolResult, Usage,
};
use agent_runtime::automatic::{
    check_and_compact, estimate_context_tokens, AutomationHandle, AutomationOptions,
    AutomationState, ModelBudget, RuntimeEvent, UpdatePlanTool,
};
use agent_runtime::compaction::compact_session;
use agent_runtime::runtime::{run_with_provider, MessageQueues, RunOptions};
use agent_runtime::session::{Session, SessionStore};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::fs;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

struct Fixed {
    responses: VecDeque<Result<AssistantMessage, String>>,
    summaries: VecDeque<Result<AssistantMessage, String>>,
    partials: VecDeque<Vec<AssistantMessage>>,
    requests: Vec<Context>,
    summary_requests: Vec<Context>,
    cancel_summary: bool,
}
impl Fixed {
    fn new(responses: Vec<Result<AssistantMessage, String>>) -> Self {
        Self {
            responses: responses.into(),
            summaries: vec![Ok(AssistantMessage::text("checkpoint"))].into(),
            partials: VecDeque::new(),
            requests: vec![],
            summary_requests: vec![],
            cancel_summary: false,
        }
    }
}
impl Provider for Fixed {
    fn complete(
        &mut self,
        request: &Context,
        _: &CancellationToken,
        update: &mut dyn FnMut(AssistantMessage),
    ) -> Result<AssistantMessage, String> {
        self.requests.push(request.clone());
        for partial in self.partials.pop_front().unwrap_or_default() {
            update(partial);
        }
        self.responses
            .pop_front()
            .expect("unexpected fixture request")
    }
    fn complete_summary(
        &mut self,
        request: &Context,
        signal: &CancellationToken,
        _: &mut dyn FnMut(AssistantMessage),
    ) -> Result<AssistantMessage, String> {
        self.summary_requests.push(request.clone());
        if self.cancel_summary {
            signal.cancel();
        }
        self.summaries
            .pop_front()
            .expect("unexpected fixture summary")
    }
}

fn history(chars: usize) -> Session {
    let mut session = Session::default();
    session.context.system_prompt = "system".into();
    session
        .append_message(Message::user("x".repeat(chars)))
        .unwrap();
    session
        .append_message(Message::Assistant(AssistantMessage::text("old answer")))
        .unwrap();
    session
}

fn options(window: u64) -> RunOptions {
    RunOptions {
        automation: AutomationOptions {
            enabled: true,
            model: Some(ModelBudget {
                provider: "fixture".into(),
                id: "fixture-model".into(),
                context_window: window,
                max_output_tokens: 64,
                cost: json!({"input":1,"output":1,"cacheWrite":1.25,"cacheRead":0.1}),
            }),
            reserve_tokens: 50,
            summary_token_estimate: 10,
            ..Default::default()
        },
        ..Default::default()
    }
}

fn call(name: &str, id: &str, arguments: Value) -> AssistantMessage {
    let mut response = AssistantMessage::text("");
    response.stop_reason = StopReason::ToolUse;
    response.content = vec![Content::ToolCall(ToolCall {
        id: id.into(),
        name: name.into(),
        arguments,
    })];
    response
}

struct CountingTool {
    calls: Arc<AtomicUsize>,
    output_chars: usize,
}
impl AgentTool for CountingTool {
    fn name(&self) -> &str {
        "count"
    }
    fn execute(
        &self,
        _: &str,
        _: &Value,
        _: &CancellationToken,
        _: &mut dyn FnMut(ToolResult),
    ) -> Result<ToolResult, String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(ToolResult::text("r".repeat(self.output_chars)))
    }
}

#[test]
fn first_request_compacts_once_and_preserves_prompt_events_and_tree_record_once() {
    let mut session = history(1600);
    let original = session.context.messages.clone();
    let options = options(400);
    let mut provider = Fixed::new(vec![Ok(AssistantMessage::text("finished"))]);
    let prompt = Message::user("actual prompt");
    let mut events = vec![];
    run_with_provider(
        &mut session,
        prompt.clone(),
        &mut provider,
        &[],
        &options,
        &MessageQueues::default(),
        &CancellationToken::new(),
        &mut |event| events.push(event),
    )
    .unwrap();
    assert_eq!(provider.summary_requests.len(), 1);
    assert_eq!(provider.requests.len(), 1);
    assert_eq!(provider.requests[0].messages.len(), 2);
    assert!(provider.requests[0].messages[0]
        .text()
        .contains("checkpoint"));
    assert_eq!(provider.requests[0].messages[1], prompt);
    assert_eq!(session.compactions[0].archived_messages, original);
    assert_eq!(session.context.messages.len(), 3);
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, Event::MessageStart { message } if message == &prompt))
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, Event::MessageEnd { message } if message == &prompt))
            .count(),
        1
    );
    assert_eq!(
        session
            .entries
            .iter()
            .filter(|entry| entry.field("message") == &serde_json::to_value(&prompt).unwrap())
            .count(),
        1
    );
    assert!(options.automation_state.state().cache_debt_tokens > 0.0);
}

#[test]
fn initial_queued_prefix_is_included_before_budget_check_without_duplicate_prompt() {
    let mut session = history(1600);
    let options = options(400);
    let queues = MessageQueues::default();
    let initial = Message::user("initial prompt");
    let steering = Message::user("steered prompt");
    queues.steer(steering.clone());
    let mut provider = Fixed::new(vec![Ok(AssistantMessage::text("finished"))]);
    let mut events = vec![];
    run_with_provider(
        &mut session,
        initial.clone(),
        &mut provider,
        &[],
        &options,
        &queues,
        &CancellationToken::new(),
        &mut |event| events.push(event),
    )
    .unwrap();
    assert_eq!(provider.requests[0].messages.last(), Some(&steering));
    assert!(session.compactions[0].archived_messages.contains(&initial));
    for message in [&initial, &steering] {
        assert_eq!(events.iter().filter(|event| matches!(event, Event::MessageEnd { message: observed } if observed == message)).count(), 1);
        assert_eq!(
            session
                .entries
                .iter()
                .filter(|entry| entry.field("message") == &serde_json::to_value(message).unwrap())
                .count(),
            1
        );
    }
    assert!(queues.is_empty());
}

#[test]
fn default_does_not_launch_paid_summary_or_retry_even_if_context_is_large() {
    let mut session = history(1600);
    let mut provider = Fixed::new(vec![Err("HTTP 503".into())]);
    let options = RunOptions::default();
    assert!(run_with_provider(
        &mut session,
        Message::user("prompt"),
        &mut provider,
        &[],
        &options,
        &MessageQueues::default(),
        &CancellationToken::new(),
        &mut |_| {}
    )
    .is_err());
    assert_eq!(provider.requests.len(), 1);
    assert!(provider.summary_requests.is_empty());
    assert!(session.compactions.is_empty());
    assert_eq!(session.runtime_state("automation").unwrap(), None);
}

#[test]
fn tool_result_pressure_compacts_after_complete_batch_and_never_reexecutes_tool() {
    let mut session = history(600);
    let options = options(1200);
    let calls = Arc::new(AtomicUsize::new(0));
    let tools: Vec<Box<dyn AgentTool>> = vec![Box::new(CountingTool {
        calls: calls.clone(),
        output_chars: 4000,
    })];
    let mut provider = Fixed::new(vec![
        Ok(call("count", "call-1", json!({}))),
        Ok(AssistantMessage::text("finished")),
    ]);
    run_with_provider(
        &mut session,
        Message::user("new prompt"),
        &mut provider,
        &tools,
        &options,
        &MessageQueues::default(),
        &CancellationToken::new(),
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(provider.summary_requests.len(), 1);
    assert_eq!(provider.requests.len(), 2);
    assert!(provider.requests[1].messages[0]
        .text()
        .contains("checkpoint"));
    assert!(!provider.requests[1]
        .messages
        .iter()
        .any(|message| message.text() == "x".repeat(600)));
    agent_runtime::automatic::validate_request_boundary(&provider.requests[1].messages).unwrap();
    assert!(provider.requests[1].messages.iter().any(
        |message| matches!(message, Message::ToolResult(result) if result.tool_call_id == "call-1")
    ));
    assert_eq!(options.automation_state.state().request_count, 2);
}

#[test]
fn summary_failure_preserves_completed_history_and_does_not_call_provider_or_tools() {
    let mut session = history(1600);
    let original = session.context.messages.clone();
    let options = options(400);
    let mut provider = Fixed::new(vec![Ok(call("count", "never", json!({})))]);
    provider.summaries = vec![Err("deterministic summary failure".into())].into();
    let calls = Arc::new(AtomicUsize::new(0));
    let tools: Vec<Box<dyn AgentTool>> = vec![Box::new(CountingTool {
        calls: calls.clone(),
        output_chars: 1,
    })];
    let mut events = vec![];
    let prompt = Message::user("accepted prompt");
    assert_eq!(
        run_with_provider(
            &mut session,
            prompt.clone(),
            &mut provider,
            &tools,
            &options,
            &MessageQueues::default(),
            &CancellationToken::new(),
            &mut |event| events.push(event)
        )
        .unwrap_err(),
        "deterministic summary failure"
    );
    assert!(session.context.messages.starts_with(&original));
    assert_eq!(session.context.messages.last(), Some(&prompt));
    assert!(session.compactions.is_empty());
    assert!(provider.requests.is_empty());
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(matches!(events.last(), Some(Event::AgentEnd { .. })));
    assert!(options
        .automation_state
        .drain_events()
        .iter()
        .any(|event| matches!(event, RuntimeEvent::CompactionEnd { error: Some(_), .. })));
}

#[test]
fn cancelled_summary_is_transactional_and_ends_run_without_executing_tools() {
    let mut session = history(1600);
    let options = options(400);
    let signal = CancellationToken::new();
    let mut provider = Fixed::new(vec![]);
    provider.cancel_summary = true;
    let prompt = Message::user("prompt");
    assert!(run_with_provider(
        &mut session,
        prompt.clone(),
        &mut provider,
        &[],
        &options,
        &MessageQueues::default(),
        &signal,
        &mut |_| {}
    )
    .unwrap_err()
    .contains("aborted"));
    assert_eq!(session.context.messages.last(), Some(&prompt));
    assert!(session.compactions.is_empty());
    assert!(provider.requests.is_empty());
}

#[test]
fn oversized_checkpoint_is_rejected_without_mutating_session() {
    let mut session = history(1600);
    session.append_message(Message::user("keep")).unwrap();
    let before = serde_json::to_value(&session).unwrap();
    let request = session.context.clone();
    let options = options(400);
    let mut provider = Fixed::new(vec![]);
    provider.summaries = vec![Ok(AssistantMessage::text("huge".repeat(2000)))].into();
    assert!(check_and_compact(
        &mut session,
        &request,
        &mut provider,
        &options.automation,
        &options.automation_state,
        &CancellationToken::new(),
        false
    )
    .unwrap_err()
    .contains("positive measured"));
    assert_eq!(serde_json::to_value(&session).unwrap(), before);
}

#[test]
fn economic_horizon_is_absent_without_measured_completed_plan_boundary() {
    let mut session = history(1600);
    session.append_message(Message::user("keep")).unwrap();
    let request = session.context.clone();
    let options = options(100_000);
    let mut provider = Fixed::new(vec![]);
    assert!(!check_and_compact(
        &mut session,
        &request,
        &mut provider,
        &options.automation,
        &options.automation_state,
        &CancellationToken::new(),
        false
    )
    .unwrap());
    let report = options.automation_state.snapshot();
    assert_eq!(report.decisions[0].reason, "horizon_unavailable");
    assert_eq!(report.decisions[0].expected_remaining_requests, None);
    assert!(provider.summary_requests.is_empty());
}

#[test]
fn real_plan_tool_boundary_measures_requests_and_triggers_economic_compaction() {
    let mut session = history(1600);
    let mut options = options(100_000);
    options.automation.model.as_mut().unwrap().cost = json!({"cacheWrite":0.1,"cacheRead":0.1});
    let plan = json!({"steps":[{"id":"one","goal":"completed work","status":"completed"},{"id":"two","goal":"remaining work","status":"in_progress"}]});
    let tools: Vec<Box<dyn AgentTool>> = vec![Box::new(UpdatePlanTool {
        handle: options.automation_state.clone(),
    })];
    let mut provider = Fixed::new(vec![
        Ok(call("update_plan", "plan-1", plan)),
        Ok(AssistantMessage::text("finished")),
    ]);
    run_with_provider(
        &mut session,
        Message::user("new prompt"),
        &mut provider,
        &tools,
        &options,
        &MessageQueues::default(),
        &CancellationToken::new(),
        &mut |_| {},
    )
    .unwrap();
    let report = options.automation_state.snapshot();
    assert!(report
        .decisions
        .iter()
        .any(|decision| decision.reason == "economic"
            && decision.expected_remaining_requests == Some(2.0)));
    assert_eq!(report.state.completed_boundary_request_counts, [1]);
    assert_eq!(report.state.request_count, 2);
    assert_eq!(provider.summary_requests.len(), 1);
    agent_runtime::automatic::validate_request_boundary(&provider.requests[1].messages).unwrap();
}

#[test]
fn debt_and_real_request_counters_survive_native_save_reload_and_new_handle() {
    let mut session = history(1600);
    let first_options = options(400);
    run_with_provider(
        &mut session,
        Message::user("first"),
        &mut Fixed::new(vec![Ok(AssistantMessage::text("answer"))]),
        &[],
        &first_options,
        &MessageQueues::default(),
        &CancellationToken::new(),
        &mut |_| {},
    )
    .unwrap();
    let first: AutomationState =
        serde_json::from_value(session.runtime_state("automation").unwrap().unwrap()).unwrap();
    let path = std::env::temp_dir().join(format!("automatic-state-{}", session.id));
    let store = SessionStore::new(&path);
    store.save(&session).unwrap();
    let mut restored = store.list().unwrap().pop().unwrap();
    let mut next = options(100_000);
    next.automation_state = AutomationHandle::default();
    run_with_provider(
        &mut restored,
        Message::user("second"),
        &mut Fixed::new(vec![Ok(AssistantMessage::text("answer"))]),
        &[],
        &next,
        &MessageQueues::default(),
        &CancellationToken::new(),
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(
        next.automation_state.state().request_count,
        first.request_count + 1
    );
    assert_eq!(next.automation_state.state().native_compaction_count, 1);
    assert!(next.automation_state.state().cache_debt_tokens < first.cache_debt_tokens);
    assert!(next.automation_state.state().cache_debt_tokens > 0.0);
    fs::remove_file(path.join(format!("{}.json", session.id))).unwrap();
    fs::remove_dir(path).unwrap();
}

#[test]
fn overflow_code_recovery_replaces_real_core_context_and_preserves_user_and_tools_once() {
    let mut session = history(1600);
    let options = options(100_000);
    let calls = Arc::new(AtomicUsize::new(0));
    let tools: Vec<Box<dyn AgentTool>> = vec![Box::new(CountingTool {
        calls: calls.clone(),
        output_chars: 1,
    })];
    let mut provider = Fixed::new(vec![
        Err("模型服务返回 HTTP 400 (context_length_exceeded)".into()),
        Ok(call("count", "call-1", json!({}))),
        Ok(AssistantMessage::text("finished")),
    ]);
    provider
        .partials
        .push_back(vec![AssistantMessage::text("")]);
    let prompt = Message::user("prompt");
    let mut events = vec![];
    run_with_provider(
        &mut session,
        prompt.clone(),
        &mut provider,
        &tools,
        &options,
        &MessageQueues::default(),
        &CancellationToken::new(),
        &mut |event| events.push(event),
    )
    .unwrap();
    assert_eq!(provider.requests.len(), 3);
    assert_eq!(provider.summary_requests.len(), 1);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    for request in &provider.requests[1..] {
        assert!(request.messages[0].text().contains("checkpoint"));
        assert!(!request
            .messages
            .iter()
            .any(|message| message.text() == "x".repeat(1600)));
        assert_eq!(
            request
                .messages
                .iter()
                .filter(|message| message == &&prompt)
                .count(),
            1
        );
    }
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, Event::MessageStart { message } if message == &prompt))
            .count(),
        1
    );
    assert_eq!(
        session
            .context
            .messages
            .iter()
            .filter(|message| message == &&prompt)
            .count(),
        1
    );
    agent_runtime::automatic::validate_request_boundary(&session.context.messages).unwrap();
}

#[test]
fn partial_output_disables_overflow_replay_and_unknown_http_400_does_not_compact() {
    for (error, partial) in [
        (
            "HTTP 400 (context_length_exceeded)",
            Some(AssistantMessage::text("already visible")),
        ),
        ("HTTP 400", None),
    ] {
        let mut session = history(1600);
        let options = options(100_000);
        let mut provider = Fixed::new(vec![Err(error.into())]);
        if let Some(partial) = partial {
            provider.partials.push_back(vec![partial]);
        }
        assert!(run_with_provider(
            &mut session,
            Message::user("prompt"),
            &mut provider,
            &[],
            &options,
            &MessageQueues::default(),
            &CancellationToken::new(),
            &mut |_| {}
        )
        .is_err());
        assert_eq!(provider.requests.len(), 1);
        assert!(provider.summary_requests.is_empty());
        assert!(session.compactions.is_empty());
    }
}

#[test]
fn stale_usage_from_before_active_branch_checkpoint_does_not_retrigger_pressure() {
    let mut session = history(1600);
    session.append_message(Message::user("latest")).unwrap();
    let mut old = AssistantMessage::text("kept answer");
    old.usage = Some(Usage {
        total_tokens: 1_000_000,
        ..Default::default()
    });
    old.timestamp = 1;
    session.append_message(Message::Assistant(old)).unwrap();
    compact_session(
        &mut session,
        &mut Fixed::new(vec![]),
        1,
        &CancellationToken::new(),
    )
    .unwrap();
    assert!(estimate_context_tokens(&session, &session.context) < 100.0);
}

#[test]
fn catalog_budget_retains_actual_window_and_complete_cost_metadata() {
    let model = agent_runtime::catalog::models()
        .iter()
        .find(|model| model.provider == "anthropic")
        .unwrap();
    let budget = ModelBudget::from_catalog(&model.provider, &model.id).unwrap();
    assert_eq!(budget.context_window, model.context_window);
    assert_eq!(budget.max_output_tokens, model.max_tokens);
    assert_eq!(budget.cost, model.cost);
    assert_eq!(ModelBudget::from_catalog("fixture", "not-in-catalog"), None);
}

#[test]
fn core_prepare_and_recovery_hooks_replace_tool_context_without_losing_last_user() {
    struct Hooks {
        replacement: Option<Context>,
    }
    impl LoopHooks for Hooks {
        fn prepare_request(
            &mut self,
            context: &Context,
            _: &CancellationToken,
        ) -> Result<Option<Context>, String> {
            let mut context = context.clone();
            context.system_prompt = "prepared".into();
            Ok(Some(context))
        }
        fn take_request_context(&mut self) -> Option<Context> {
            self.replacement.take()
        }
    }
    struct Observer(Arc<Mutex<Vec<Context>>>);
    impl ToolHooks for Observer {
        fn before_tool_call(
            &self,
            context: BeforeToolCallContext<'_>,
            _: &CancellationToken,
        ) -> Result<BeforeToolCallResult, String> {
            self.0.lock().unwrap().push(context.context.clone());
            Ok(Default::default())
        }
    }
    let prompt = Message::user("real user");
    let replacement = Context {
        system_prompt: "recovered".into(),
        messages: vec![Message::user("checkpoint"), prompt.clone()],
    };
    let mut hooks = Hooks {
        replacement: Some(replacement),
    };
    let seen = Arc::new(Mutex::new(vec![]));
    let calls = Arc::new(AtomicUsize::new(0));
    let tools: Vec<Box<dyn AgentTool>> = vec![Box::new(CountingTool {
        calls,
        output_chars: 1,
    })];
    let mut provider = Fixed::new(vec![
        Ok(call("count", "call-1", json!({}))),
        Ok(AssistantMessage::text("done")),
    ]);
    provider
        .partials
        .push_back(vec![AssistantMessage::text("")]);
    run_agent_loop(
        vec![prompt.clone()],
        &Context::default(),
        LoopConfig::default(),
        &mut provider,
        &tools,
        &mut hooks,
        &Observer(seen.clone()),
        &CancellationToken::new(),
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(provider.requests[0].system_prompt, "prepared");
    let observed = seen.lock().unwrap();
    assert_eq!(observed[0].system_prompt, "recovered");
    assert_eq!(observed[0].messages[1], prompt);
    assert!(matches!(
        observed[0].messages.last(),
        Some(Message::Assistant(_))
    ));
}

#[test]
fn navigate_restores_branch_specific_request_and_debt_state_not_latest_global_state() {
    let mut session = history(20);
    let mut options = RunOptions::default();
    options.automation.retry.enabled = true;
    run_with_provider(
        &mut session,
        Message::user("first"),
        &mut Fixed::new(vec![Ok(AssistantMessage::text("answer"))]),
        &[],
        &options,
        &MessageQueues::default(),
        &CancellationToken::new(),
        &mut |_| {},
    )
    .unwrap();
    let first_leaf = session.leaf_id.clone().unwrap();
    run_with_provider(
        &mut session,
        Message::user("second"),
        &mut Fixed::new(vec![Ok(AssistantMessage::text("answer"))]),
        &[],
        &options,
        &MessageQueues::default(),
        &CancellationToken::new(),
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(options.automation_state.state().request_count, 2);
    session.navigate(Some(&first_leaf)).unwrap();
    run_with_provider(
        &mut session,
        Message::user("alternate"),
        &mut Fixed::new(vec![Ok(AssistantMessage::text("answer"))]),
        &[],
        &options,
        &MessageQueues::default(),
        &CancellationToken::new(),
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(options.automation_state.state().request_count, 2);
    assert!(!session
        .context
        .messages
        .iter()
        .any(|message| message.text() == "second"));
    assert!(session
        .entries
        .iter()
        .any(|entry| entry.field("message")["content"][0]["text"] == "second"));
}

#[test]
fn selected_older_checkpoint_is_synthetic_even_when_global_archive_has_later_checkpoint() {
    let mut session = history(1600);
    session.append_message(Message::user("kept")).unwrap();
    compact_session(
        &mut session,
        &mut Fixed::new(vec![]),
        1,
        &CancellationToken::new(),
    )
    .unwrap();
    let old_leaf = session.leaf_id.clone().unwrap();
    session
        .append_message(Message::user("new branch work"))
        .unwrap();
    let mut later = Fixed::new(vec![]);
    later.summaries = vec![Ok(AssistantMessage::text("later checkpoint"))].into();
    compact_session(&mut session, &mut later, 1, &CancellationToken::new()).unwrap();
    session.navigate(Some(&old_leaf)).unwrap();
    assert_eq!(session.compactions.len(), 2);
    assert!(agent_runtime::compaction::prepare_compaction(&session, 1)
        .unwrap_err()
        .contains("No completed history"));
    let handle = AutomationHandle::default();
    handle.bind_session(&session);
    assert_eq!(handle.state().native_compaction_count, 1);
}

#[test]
fn unknown_model_requires_explicit_budget_before_any_request_or_input_commit() {
    let mut session = history(20);
    let before = serde_json::to_value(&session).unwrap();
    let options = RunOptions {
        automation: AutomationOptions {
            enabled: true,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut provider = Fixed::new(vec![]);
    assert!(run_with_provider(
        &mut session,
        Message::user("prompt"),
        &mut provider,
        &[],
        &options,
        &MessageQueues::default(),
        &CancellationToken::new(),
        &mut |_| {}
    )
    .unwrap_err()
    .contains("explicit ModelBudget"));
    assert_eq!(serde_json::to_value(&session).unwrap(), before);
    assert!(provider.requests.is_empty());
    assert!(provider.summary_requests.is_empty());
}

#[test]
fn prepare_request_failure_ends_core_loop_without_provider_or_tool_calls() {
    struct Fail;
    impl LoopHooks for Fail {
        fn prepare_request(
            &mut self,
            _: &Context,
            _: &CancellationToken,
        ) -> Result<Option<Context>, String> {
            Err("transactional preparation failure".into())
        }
    }
    let mut provider = Fixed::new(vec![]);
    let mut events = vec![];
    let output = run_agent_loop(
        vec![Message::user("prompt")],
        &Context::default(),
        LoopConfig::default(),
        &mut provider,
        &[],
        &mut Fail,
        &agent_core::tools::NoopToolHooks,
        &CancellationToken::new(),
        &mut |event| events.push(event),
    )
    .unwrap();
    assert!(provider.requests.is_empty());
    assert!(
        matches!(output.last(), Some(Message::Assistant(message)) if message.error_message.as_deref() == Some("transactional preparation failure"))
    );
    assert!(matches!(events.last(), Some(Event::AgentEnd { .. })));
}

#[test]
fn navigate_before_any_saved_automation_state_does_not_leak_other_branch_counters() {
    let mut session = history(20);
    let old_leaf = session.leaf_id.clone().unwrap();
    let mut options = RunOptions::default();
    options.automation.retry.enabled = true;
    run_with_provider(
        &mut session,
        Message::user("first"),
        &mut Fixed::new(vec![Ok(AssistantMessage::text("answer"))]),
        &[],
        &options,
        &MessageQueues::default(),
        &CancellationToken::new(),
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(options.automation_state.state().request_count, 1);
    session.navigate(Some(&old_leaf)).unwrap();
    assert_eq!(session.runtime_state("automation").unwrap(), None);
    run_with_provider(
        &mut session,
        Message::user("alternate"),
        &mut Fixed::new(vec![Ok(AssistantMessage::text("answer"))]),
        &[],
        &options,
        &MessageQueues::default(),
        &CancellationToken::new(),
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(options.automation_state.state().request_count, 1);
}

#[test]
fn changing_model_route_invalidates_cache_debt_and_horizon_without_inventing_requests() {
    let mut session = history(1600);
    let first = options(400);
    run_with_provider(
        &mut session,
        Message::user("first"),
        &mut Fixed::new(vec![Ok(AssistantMessage::text("answer"))]),
        &[],
        &first,
        &MessageQueues::default(),
        &CancellationToken::new(),
        &mut |_| {},
    )
    .unwrap();
    assert!(first.automation_state.state().cache_debt_tokens > 0.0);
    let mut next = options(100_000);
    next.automation.model.as_mut().unwrap().id = "other-model".into();
    run_with_provider(
        &mut session,
        Message::user("next"),
        &mut Fixed::new(vec![Ok(AssistantMessage::text("answer"))]),
        &[],
        &next,
        &MessageQueues::default(),
        &CancellationToken::new(),
        &mut |_| {},
    )
    .unwrap();
    let state = next.automation_state.state();
    assert_eq!(state.model_identity.as_deref(), Some("fixture/other-model"));
    assert_eq!(state.request_count, 2);
    assert_eq!(state.cache_debt_tokens, 0.0);
    assert!(state.completed_boundary_request_counts.is_empty());
}

#[test]
fn foreign_or_unknown_usage_does_not_trigger_paid_current_model_compaction() {
    for metadata in [
        None,
        Some(json!({"protocol":"fixture","provider":"fixture","model":"foreign-model"})),
    ] {
        let mut session = history(20);
        let mut old = AssistantMessage::text("old answer");
        old.usage = Some(Usage {
            total_tokens: 1_000_000,
            ..Default::default()
        });
        old.protocol_metadata = metadata;
        session.append_message(Message::Assistant(old)).unwrap();
        let options = options(1000);
        let mut provider = Fixed::new(vec![Ok(AssistantMessage::text("done"))]);
        run_with_provider(
            &mut session,
            Message::user("prompt"),
            &mut provider,
            &[],
            &options,
            &MessageQueues::default(),
            &CancellationToken::new(),
            &mut |_| {},
        )
        .unwrap();
        assert!(provider.summary_requests.is_empty());
        assert!(options
            .automation_state
            .snapshot()
            .decisions
            .iter()
            .all(|decision| decision.write_tokens < 100.0));
    }
}

#[test]
fn manual_checkpoint_sync_preserves_usage_prices_debt_and_persists_branch_without_extra_call() {
    let mut session = history(1600);
    session.append_message(Message::user("latest")).unwrap();
    let before = session.clone();
    let mut provider = Fixed::new(vec![]);
    let usage = Usage {
        input: 10,
        output: 5,
        total_tokens: 15,
        ..Default::default()
    };
    let mut summary = AssistantMessage::text("checkpoint");
    summary.usage = Some(usage.clone());
    provider.summaries = vec![Ok(summary)].into();
    let result =
        compact_session(&mut session, &mut provider, 1, &CancellationToken::new()).unwrap();
    let options = options(100_000);
    agent_runtime::automatic::record_manual_checkpoint(
        &mut session,
        &before,
        &result,
        options.automation.model.as_ref(),
        &options.automation_state,
    )
    .unwrap();
    assert_eq!(provider.summary_requests.len(), 1);
    assert!(provider.requests.is_empty());
    let state: AutomationState =
        serde_json::from_value(session.runtime_state("automation").unwrap().unwrap()).unwrap();
    assert_eq!(state.request_count, 0);
    assert_eq!(state.native_compaction_count, 1);
    assert_eq!(state.last_summary_usage, Some(usage));
    assert_eq!(state.last_context_tokens, None);
    assert!(state.last_checkpoint_context_tokens.unwrap() < 100.0);
    assert!(state.cache_debt_tokens > 0.0);
    assert!(!state.cache_debt_unpriced);
    assert!(state.plan.is_empty());
    assert_eq!(options.automation_state.state(), state);
}

#[test]
fn manual_checkpoint_invalid_result_or_state_is_transactional_for_session_and_handle() {
    let mut session = history(1600);
    session.append_message(Message::user("latest")).unwrap();
    let before = session.clone();
    let result = compact_session(
        &mut session,
        &mut Fixed::new(vec![]),
        1,
        &CancellationToken::new(),
    )
    .unwrap();
    let original = serde_json::to_value(&session).unwrap();
    let options = options(100_000);
    let initial_state = options.automation_state.state();
    let mut invalid = result.clone();
    invalid.id = "uncommitted".into();
    assert!(agent_runtime::automatic::record_manual_checkpoint(
        &mut session,
        &before,
        &invalid,
        options.automation.model.as_ref(),
        &options.automation_state
    )
    .is_err());
    assert_eq!(serde_json::to_value(&session).unwrap(), original);
    assert_eq!(options.automation_state.state(), initial_state);
    let mut corrupt_before = before.clone();
    corrupt_before
        .record_runtime_state(
            "automation",
            json!({"requestCount":2,"lastBoundaryRequestCount":3}),
        )
        .unwrap();
    assert!(agent_runtime::automatic::record_manual_checkpoint(
        &mut session,
        &corrupt_before,
        &result,
        options.automation.model.as_ref(),
        &options.automation_state
    )
    .is_err());
    assert_eq!(serde_json::to_value(&session).unwrap(), original);
    assert_eq!(options.automation_state.state(), initial_state);
}

#[test]
fn unknown_manual_price_is_explicitly_unpriced_and_does_not_invent_cost_or_requests() {
    let mut session = history(1600);
    session.append_message(Message::user("latest")).unwrap();
    let before = session.clone();
    let result = compact_session(
        &mut session,
        &mut Fixed::new(vec![]),
        1,
        &CancellationToken::new(),
    )
    .unwrap();
    let handle = AutomationHandle::default();
    agent_runtime::automatic::record_manual_checkpoint(
        &mut session,
        &before,
        &result,
        None,
        &handle,
    )
    .unwrap();
    let state = handle.state();
    assert!(state.cache_debt_unpriced);
    assert_eq!(state.request_count, 0);
    assert_eq!(state.cache_debt_tokens, 0.0);
    assert_eq!(
        session.runtime_state("automation").unwrap().unwrap()["cacheDebtUnpriced"],
        true
    );
}

#[test]
fn unrelated_previous_handle_session_does_not_erase_new_manual_ledger() {
    let unrelated = history(20);
    let handle = AutomationHandle::default();
    handle.bind_session(&unrelated);
    let mut session = history(1600);
    session.append_message(Message::user("latest")).unwrap();
    let before = session.clone();
    let result = compact_session(
        &mut session,
        &mut Fixed::new(vec![]),
        1,
        &CancellationToken::new(),
    )
    .unwrap();
    let options = options(100_000);
    agent_runtime::automatic::record_manual_checkpoint(
        &mut session,
        &before,
        &result,
        options.automation.model.as_ref(),
        &handle,
    )
    .unwrap();
    assert_eq!(
        handle.snapshot().session_id.as_deref(),
        Some(session.id.as_str())
    );
    assert_eq!(handle.state().native_compaction_count, 1);
    assert!(handle.state().cache_debt_tokens > 0.0);
}

#[test]
fn zero_separate_cache_write_rate_uses_actual_uncached_input_price_not_free_rewrite() {
    let mut session = history(1600);
    session.append_message(Message::user("latest")).unwrap();
    let request = session.context.clone();
    let mut options = options(100_000);
    options.automation.model.as_mut().unwrap().cost =
        json!({"input":2,"cacheWrite":0,"cacheRead":0.5});
    let mut provider = Fixed::new(vec![]);
    assert!(!check_and_compact(
        &mut session,
        &request,
        &mut provider,
        &options.automation,
        &options.automation_state,
        &CancellationToken::new(),
        false
    )
    .unwrap());
    assert_eq!(
        options.automation_state.snapshot().decisions[0].cache_write_read_ratio,
        Some(4.0)
    );
    options.automation.model.as_mut().unwrap().cost = json!({"cacheWrite":0,"cacheRead":0.5});
    assert!(!check_and_compact(
        &mut session,
        &request,
        &mut provider,
        &options.automation,
        &options.automation_state,
        &CancellationToken::new(),
        false
    )
    .unwrap());
    assert_eq!(
        options.automation_state.snapshot().decisions[1].cache_write_read_ratio,
        None
    );
}
