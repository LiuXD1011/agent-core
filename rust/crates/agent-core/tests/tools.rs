use agent_core::tools::{
    execute_tool_calls, fail_tool_calls_from_truncated_message, AfterToolCallContext,
    AfterToolCallResult, AgentTool, BeforeToolCallContext, BeforeToolCallResult, NoopToolHooks,
    ToolHooks,
};
use agent_core::types::{
    AssistantMessage, CancellationToken, Content, Context, Event, ExecutionMode, Message,
    StopReason, ToolCall, ToolResult, Usage, Value,
};
use serde_json::json;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Barrier, Mutex,
};
use std::thread;
use std::time::Duration;

fn assistant(calls: Vec<ToolCall>) -> AssistantMessage {
    AssistantMessage {
        content: calls.into_iter().map(Content::ToolCall).collect(),
        stop_reason: StopReason::ToolUse,
        error_message: None,
        usage: None,
        timestamp: 0,
        protocol_metadata: None,
    }
}

fn call(id: &str, name: &str, arguments: Value) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments,
    }
}

struct EchoTool {
    mode: Option<ExecutionMode>,
    trace: Arc<Mutex<Vec<String>>>,
    fail: bool,
    terminate: bool,
}

impl EchoTool {
    fn plain() -> Self {
        Self {
            mode: None,
            trace: Arc::default(),
            fail: false,
            terminate: false,
        }
    }
}

impl AgentTool for EchoTool {
    fn name(&self) -> &str {
        "echo"
    }
    fn execution_mode(&self) -> Option<ExecutionMode> {
        self.mode
    }
    fn parameters(&self) -> Value {
        json!({"type":"object", "properties":{"value":{"type":"string"}}, "required":["value"], "additionalProperties":false})
    }
    fn execute(
        &self,
        id: &str,
        args: &Value,
        _signal: &CancellationToken,
        on_update: &mut dyn FnMut(ToolResult),
    ) -> Result<ToolResult, String> {
        self.trace.lock().unwrap().push(format!("{id}:start"));
        on_update(ToolResult::text("partial"));
        self.trace.lock().unwrap().push(format!("{id}:end"));
        if self.fail {
            return Err("fixture failed".into());
        }
        Ok(ToolResult {
            content: vec![Content::text(args["value"].to_string())],
            details: args.clone(),
            usage: Some(Usage {
                input: 1,
                output: 2,
                total_tokens: 3,
                ..Usage::default()
            }),
            added_tool_names: vec!["new-tool".into()],
            terminate: self.terminate,
        })
    }
}

#[test]
fn sequential_events_keep_raw_arguments_and_flush_updates_before_results() {
    let tools: Vec<Box<dyn AgentTool>> = vec![Box::new(EchoTool::plain())];
    let raw = json!({"value":123});
    let mut events = Vec::new();
    let batch = execute_tool_calls(
        &tools,
        &Context::default(),
        &assistant(vec![call("a", "echo", raw.clone())]),
        ExecutionMode::Sequential,
        &NoopToolHooks,
        &CancellationToken::new(),
        &mut |event| events.push(event),
    );
    assert!(!batch.terminate);
    assert_eq!(batch.messages.len(), 1);
    assert!(!batch.messages[0].is_error);
    assert_eq!(batch.messages[0].details, json!({"value":"123"}));
    assert_eq!(batch.messages[0].added_tool_names, vec!["new-tool"]);
    assert_eq!(batch.messages[0].usage.as_ref().unwrap().total_tokens, 3);
    assert!(matches!(&events[0], Event::ToolExecutionStart { args, .. } if args == &raw));
    assert!(matches!(&events[1], Event::ToolExecutionUpdate { args, .. } if args == &raw));
    assert!(matches!(
        events[2],
        Event::ToolExecutionEnd {
            is_error: false,
            ..
        }
    ));
    assert!(matches!(events[3], Event::MessageStart { .. }));
    assert!(matches!(events[4], Event::MessageEnd { .. }));
}

#[test]
fn unknown_tool_and_invalid_schema_are_error_results_with_call_identity() {
    let tool = EchoTool::plain();
    let trace = tool.trace.clone();
    let tools: Vec<Box<dyn AgentTool>> = vec![Box::new(tool)];
    let batch = execute_tool_calls(
        &tools,
        &Context::default(),
        &assistant(vec![
            call("missing", "absent", json!({})),
            call("bad", "echo", json!({"extra":true})),
        ]),
        ExecutionMode::Sequential,
        &NoopToolHooks,
        &CancellationToken::new(),
        &mut |_| {},
    );
    assert!(trace.lock().unwrap().is_empty());
    assert_eq!(batch.messages[0].tool_call_id, "missing");
    assert_eq!(
        batch.messages[0].content,
        vec![Content::text("Tool absent not found")]
    );
    assert!(batch.messages.iter().all(|message| message.is_error));
    assert!(
        matches!(&batch.messages[1].content[0], Content::Text { text } if text.contains("required"))
    );
}

struct MutableHooks {
    after_calls: AtomicUsize,
}
impl ToolHooks for MutableHooks {
    fn before_tool_call(
        &self,
        context: BeforeToolCallContext<'_>,
        _signal: &CancellationToken,
    ) -> Result<BeforeToolCallResult, String> {
        assert_eq!(context.args["value"], "raw");
        context.args["value"] = json!(456);
        Ok(BeforeToolCallResult::default())
    }
    fn after_tool_call(
        &self,
        context: AfterToolCallContext<'_>,
        _signal: &CancellationToken,
    ) -> Result<Option<AfterToolCallResult>, String> {
        self.after_calls.fetch_add(1, Ordering::Relaxed);
        assert_eq!(context.args["value"], 456);
        assert_eq!(context.tool_call.arguments["value"], "raw");
        Ok(Some(AfterToolCallResult {
            content: Some(vec![Content::text("replacement")]),
            details: Some(json!({"patched":true})),
            usage: Some(Usage {
                total_tokens: 9,
                ..Usage::default()
            }),
            terminate: Some(true),
            is_error: Some(false),
        }))
    }
}

#[test]
fn hooks_mutate_arguments_without_revalidation_and_replace_fields() {
    let tools: Vec<Box<dyn AgentTool>> = vec![Box::new(EchoTool::plain())];
    let hooks = MutableHooks {
        after_calls: AtomicUsize::new(0),
    };
    let batch = execute_tool_calls(
        &tools,
        &Context::default(),
        &assistant(vec![call("a", "echo", json!({"value":"raw"}))]),
        ExecutionMode::Sequential,
        &hooks,
        &CancellationToken::new(),
        &mut |_| {},
    );
    assert_eq!(hooks.after_calls.load(Ordering::Relaxed), 1);
    assert!(batch.terminate);
    assert_eq!(
        batch.messages[0].content,
        vec![Content::text("replacement")]
    );
    assert_eq!(batch.messages[0].details, json!({"patched":true}));
    assert_eq!(batch.messages[0].usage.as_ref().unwrap().total_tokens, 9);
    assert_eq!(batch.messages[0].added_tool_names, vec!["new-tool"]);
}

struct BlockHooks {
    after_calls: AtomicUsize,
}
impl ToolHooks for BlockHooks {
    fn before_tool_call(
        &self,
        context: BeforeToolCallContext<'_>,
        _signal: &CancellationToken,
    ) -> Result<BeforeToolCallResult, String> {
        Ok(BeforeToolCallResult {
            block: context.tool_call.id == "blocked",
            reason: Some("blocked fixture".into()),
            terminate: true,
        })
    }
    fn after_tool_call(
        &self,
        _context: AfterToolCallContext<'_>,
        _signal: &CancellationToken,
    ) -> Result<Option<AfterToolCallResult>, String> {
        self.after_calls.fetch_add(1, Ordering::Relaxed);
        Ok(None)
    }
}

#[test]
fn blocked_calls_skip_execution_and_after_hook_and_mixed_batch_does_not_terminate() {
    let tool = EchoTool::plain();
    let trace = tool.trace.clone();
    let tools: Vec<Box<dyn AgentTool>> = vec![Box::new(tool)];
    let hooks = BlockHooks {
        after_calls: AtomicUsize::new(0),
    };
    let batch = execute_tool_calls(
        &tools,
        &Context::default(),
        &assistant(vec![
            call("blocked", "echo", json!({"value":"a"})),
            call("allowed", "echo", json!({"value":"b"})),
        ]),
        ExecutionMode::Parallel,
        &hooks,
        &CancellationToken::new(),
        &mut |_| {},
    );
    assert!(!batch.terminate);
    assert_eq!(*trace.lock().unwrap(), vec!["allowed:start", "allowed:end"]);
    assert_eq!(hooks.after_calls.load(Ordering::Relaxed), 1);
    assert!(batch.messages[0].is_error);
    assert_eq!(
        batch.messages[0].content,
        vec![Content::text("blocked fixture")]
    );
    assert!(!batch.messages[1].is_error);
}

#[test]
fn a_fully_blocked_terminating_batch_stops_but_empty_batch_does_not() {
    let tools: Vec<Box<dyn AgentTool>> = vec![Box::new(EchoTool::plain())];
    let hooks = BlockHooks {
        after_calls: AtomicUsize::new(0),
    };
    let batch = execute_tool_calls(
        &tools,
        &Context::default(),
        &assistant(vec![call("blocked", "echo", json!({"value":"a"}))]),
        ExecutionMode::Auto,
        &hooks,
        &CancellationToken::new(),
        &mut |_| {},
    );
    assert!(batch.terminate);
    let empty = execute_tool_calls(
        &tools,
        &Context::default(),
        &assistant(vec![]),
        ExecutionMode::Parallel,
        &hooks,
        &CancellationToken::new(),
        &mut |_| {},
    );
    assert!(!empty.terminate);
    assert!(empty.messages.is_empty());
}

struct RecoverHooks;
impl ToolHooks for RecoverHooks {
    fn after_tool_call(
        &self,
        context: AfterToolCallContext<'_>,
        _signal: &CancellationToken,
    ) -> Result<Option<AfterToolCallResult>, String> {
        assert!(context.is_error);
        assert_eq!(
            context.result.content,
            vec![Content::text("fixture failed")]
        );
        Ok(Some(AfterToolCallResult {
            content: Some(vec![Content::text("recovered")]),
            is_error: Some(false),
            ..AfterToolCallResult::default()
        }))
    }
}

#[test]
fn execution_failures_become_results_and_after_hook_can_recover() {
    let mut tool = EchoTool::plain();
    tool.fail = true;
    let tools: Vec<Box<dyn AgentTool>> = vec![Box::new(tool)];
    let batch = execute_tool_calls(
        &tools,
        &Context::default(),
        &assistant(vec![call("a", "echo", json!({"value":"raw"}))]),
        ExecutionMode::Parallel,
        &RecoverHooks,
        &CancellationToken::new(),
        &mut |_| {},
    );
    assert!(!batch.messages[0].is_error);
    assert_eq!(batch.messages[0].content, vec![Content::text("recovered")]);
}

struct FailingHooks {
    before: bool,
}
impl ToolHooks for FailingHooks {
    fn before_tool_call(
        &self,
        _context: BeforeToolCallContext<'_>,
        _signal: &CancellationToken,
    ) -> Result<BeforeToolCallResult, String> {
        if self.before {
            Err("before failed".into())
        } else {
            Ok(BeforeToolCallResult::default())
        }
    }
    fn after_tool_call(
        &self,
        _context: AfterToolCallContext<'_>,
        _signal: &CancellationToken,
    ) -> Result<Option<AfterToolCallResult>, String> {
        Err("after failed".into())
    }
}

#[test]
fn hook_failures_become_error_results() {
    for before in [false, true] {
        let tools: Vec<Box<dyn AgentTool>> = vec![Box::new(EchoTool::plain())];
        let batch = execute_tool_calls(
            &tools,
            &Context::default(),
            &assistant(vec![call("a", "echo", json!({"value":"raw"}))]),
            ExecutionMode::Parallel,
            &FailingHooks { before },
            &CancellationToken::new(),
            &mut |_| {},
        );
        assert!(batch.messages[0].is_error);
        assert_eq!(
            batch.messages[0].content,
            vec![Content::text(if before {
                "before failed"
            } else {
                "after failed"
            })]
        );
    }
}

struct PreparedTool;
impl AgentTool for PreparedTool {
    fn name(&self) -> &str {
        "prepare"
    }
    fn prepare_arguments(&self, args: &Value) -> Result<Value, String> {
        Ok(json!({"value":args["legacy"]}))
    }
    fn parameters(&self) -> Value {
        json!({"type":"object", "properties":{"value":{"type":"integer"}}, "required":["value"]})
    }
    fn execute(
        &self,
        _id: &str,
        args: &Value,
        _signal: &CancellationToken,
        _update: &mut dyn FnMut(ToolResult),
    ) -> Result<ToolResult, String> {
        Ok(ToolResult {
            details: args.clone(),
            ..ToolResult::default()
        })
    }
}

#[test]
fn preparation_precedes_schema_coercion_and_validation() {
    let tools: Vec<Box<dyn AgentTool>> = vec![Box::new(PreparedTool)];
    let batch = execute_tool_calls(
        &tools,
        &Context::default(),
        &assistant(vec![call("a", "prepare", json!({"legacy":"42"}))]),
        ExecutionMode::Sequential,
        &NoopToolHooks,
        &CancellationToken::new(),
        &mut |_| {},
    );
    assert!(!batch.messages[0].is_error);
    assert_eq!(batch.messages[0].details["value"].as_f64(), Some(42.0));
    assert_eq!(batch.messages[0].details["value"].as_u64(), Some(42));
}

struct SchemaTool {
    schema: Value,
}
impl AgentTool for SchemaTool {
    fn name(&self) -> &str {
        "schema"
    }
    fn parameters(&self) -> Value {
        self.schema.clone()
    }
    fn execute(
        &self,
        _id: &str,
        args: &Value,
        _signal: &CancellationToken,
        _update: &mut dyn FnMut(ToolResult),
    ) -> Result<ToolResult, String> {
        Ok(ToolResult {
            details: args.clone(),
            ..ToolResult::default()
        })
    }
}

#[test]
fn structural_schema_keywords_validate_nested_arrays_enum_bounds_and_additional_properties() {
    let schema = json!({
        "type":"object", "additionalProperties":false, "required":["items"],
        "properties": {
            "items":{"type":"array", "minItems":1, "maxItems":2, "uniqueItems":true,
                "items":{"type":"integer", "minimum":0, "maximum":10, "multipleOf":2}},
            "mode":{"type":"string", "enum":["read","write"]}
        }
    });
    let tools: Vec<Box<dyn AgentTool>> = vec![Box::new(SchemaTool { schema })];
    for input in [
        json!({"items":[]}),
        json!({"items":[2,2]}),
        json!({"items":[3]}),
        json!({"items":[12]}),
        json!({"items":[2],"extra":true}),
        json!({"items":[2],"mode":"other"}),
    ] {
        let batch = execute_tool_calls(
            &tools,
            &Context::default(),
            &assistant(vec![call("a", "schema", input)]),
            ExecutionMode::Auto,
            &NoopToolHooks,
            &CancellationToken::new(),
            &mut |_| {},
        );
        assert!(batch.messages[0].is_error);
    }
    let batch = execute_tool_calls(
        &tools,
        &Context::default(),
        &assistant(vec![call(
            "a",
            "schema",
            json!({"items":["2","4"],"mode":null}),
        )]),
        ExecutionMode::Auto,
        &NoopToolHooks,
        &CancellationToken::new(),
        &mut |_| {},
    );
    assert!(!batch.messages[0].is_error);
    assert_eq!(batch.messages[0].details, json!({"items":[2,4]}));
}

#[test]
fn unimplemented_schema_assertions_are_rejected_instead_of_ignored() {
    let tools: Vec<Box<dyn AgentTool>> = vec![Box::new(SchemaTool {
        schema: json!({"type":"object", "properties":{"name":{"type":"string","pattern":"^known$"}}, "required":["name"]}),
    })];
    let batch = execute_tool_calls(
        &tools,
        &Context::default(),
        &assistant(vec![call("a", "schema", json!({"name":"anything"}))]),
        ExecutionMode::Auto,
        &NoopToolHooks,
        &CancellationToken::new(),
        &mut |_| {},
    );
    assert!(batch.messages[0].is_error);
    assert!(
        matches!(&batch.messages[0].content[0], Content::Text { text } if text.contains("pattern requires a custom"))
    );
}

#[test]
fn cancellation_before_execution_yields_error_then_stops_sequential_batch() {
    let tool = EchoTool::plain();
    let trace = tool.trace.clone();
    let tools: Vec<Box<dyn AgentTool>> = vec![Box::new(tool)];
    let signal = CancellationToken::new();
    signal.cancel();
    let batch = execute_tool_calls(
        &tools,
        &Context::default(),
        &assistant(vec![
            call("a", "echo", json!({"value":"raw"})),
            call("b", "echo", json!({"value":"raw"})),
        ]),
        ExecutionMode::Sequential,
        &NoopToolHooks,
        &signal,
        &mut |_| {},
    );
    assert_eq!(batch.messages.len(), 1);
    assert!(batch.messages[0].is_error);
    assert_eq!(
        batch.messages[0].content,
        vec![Content::text("Operation aborted")]
    );
    assert!(trace.lock().unwrap().is_empty());
}

struct CancelBeforeHooks;
impl ToolHooks for CancelBeforeHooks {
    fn before_tool_call(
        &self,
        _context: BeforeToolCallContext<'_>,
        signal: &CancellationToken,
    ) -> Result<BeforeToolCallResult, String> {
        signal.cancel();
        Ok(BeforeToolCallResult {
            block: true,
            reason: Some("not used".into()),
            terminate: true,
        })
    }
}

#[test]
fn cancellation_in_before_hook_takes_precedence_over_blocking() {
    let tools: Vec<Box<dyn AgentTool>> = vec![Box::new(EchoTool::plain())];
    let batch = execute_tool_calls(
        &tools,
        &Context::default(),
        &assistant(vec![call("a", "echo", json!({"value":"raw"}))]),
        ExecutionMode::Parallel,
        &CancelBeforeHooks,
        &CancellationToken::new(),
        &mut |_| {},
    );
    assert!(!batch.terminate);
    assert_eq!(
        batch.messages[0].content,
        vec![Content::text("Operation aborted")]
    );
}

#[test]
fn auto_and_explicit_sequential_tool_modes_preserve_effectful_order() {
    for mode in [None, Some(ExecutionMode::Sequential)] {
        for batch_mode in [ExecutionMode::Auto, ExecutionMode::Parallel] {
            if mode.is_none() && batch_mode == ExecutionMode::Parallel {
                continue;
            }
            let mut tool = EchoTool::plain();
            tool.mode = mode;
            let trace = tool.trace.clone();
            let tools: Vec<Box<dyn AgentTool>> = vec![Box::new(tool)];
            execute_tool_calls(
                &tools,
                &Context::default(),
                &assistant(vec![
                    call("a", "echo", json!({"value":"a"})),
                    call("b", "echo", json!({"value":"b"})),
                ]),
                batch_mode,
                &NoopToolHooks,
                &CancellationToken::new(),
                &mut |_| {},
            );
            assert_eq!(
                *trace.lock().unwrap(),
                vec!["a:start", "a:end", "b:start", "b:end"]
            );
        }
    }
}

struct ConcurrentTool {
    running: AtomicUsize,
    peak: AtomicUsize,
    initial: Barrier,
    cancel: bool,
    executed: AtomicUsize,
}
impl ConcurrentTool {
    fn new(cancel: bool) -> Self {
        Self {
            running: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
            initial: Barrier::new(4),
            cancel,
            executed: AtomicUsize::new(0),
        }
    }
}
impl AgentTool for ConcurrentTool {
    fn name(&self) -> &str {
        "read"
    }
    fn execution_mode(&self) -> Option<ExecutionMode> {
        Some(ExecutionMode::Parallel)
    }
    fn execute(
        &self,
        id: &str,
        _args: &Value,
        signal: &CancellationToken,
        _update: &mut dyn FnMut(ToolResult),
    ) -> Result<ToolResult, String> {
        self.executed.fetch_add(1, Ordering::Relaxed);
        let running = self.running.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(running, Ordering::SeqCst);
        if id.parse::<usize>().unwrap() < 4 {
            self.initial.wait();
            if self.cancel {
                signal.cancel();
            }
            self.initial.wait();
        }
        thread::sleep(Duration::from_millis(2));
        self.running.fetch_sub(1, Ordering::SeqCst);
        Ok(ToolResult::text(id))
    }
}

#[test]
fn parallel_and_auto_use_at_most_four_workers_and_keep_source_result_order() {
    for mode in [ExecutionMode::Parallel, ExecutionMode::Auto] {
        let tool = Arc::new(ConcurrentTool::new(false));
        struct Shared(Arc<ConcurrentTool>);
        impl AgentTool for Shared {
            fn name(&self) -> &str {
                self.0.name()
            }
            fn execution_mode(&self) -> Option<ExecutionMode> {
                self.0.execution_mode()
            }
            fn execute(
                &self,
                id: &str,
                args: &Value,
                signal: &CancellationToken,
                update: &mut dyn FnMut(ToolResult),
            ) -> Result<ToolResult, String> {
                self.0.execute(id, args, signal, update)
            }
        }
        let tools: Vec<Box<dyn AgentTool>> = vec![Box::new(Shared(tool.clone()))];
        let mut events = Vec::new();
        let batch = execute_tool_calls(
            &tools,
            &Context::default(),
            &assistant(
                (0..11)
                    .map(|index| call(&index.to_string(), "read", json!({})))
                    .collect(),
            ),
            mode,
            &NoopToolHooks,
            &CancellationToken::new(),
            &mut |event| events.push(event),
        );
        assert_eq!(tool.peak.load(Ordering::SeqCst), 4);
        assert_eq!(
            batch
                .messages
                .iter()
                .map(|message| message.tool_call_id.clone())
                .collect::<Vec<_>>(),
            (0..11).map(|index| index.to_string()).collect::<Vec<_>>()
        );
        let final_end = events
            .iter()
            .rposition(|event| matches!(event, Event::ToolExecutionEnd { .. }))
            .unwrap();
        let first_result = events
            .iter()
            .position(|event| matches!(event, Event::MessageStart { .. }))
            .unwrap();
        assert!(final_end < first_result);
    }
}

#[test]
fn queued_parallel_calls_do_not_execute_after_cancellation() {
    let tools: Vec<Box<dyn AgentTool>> = vec![Box::new(ConcurrentTool::new(true))];
    let batch = execute_tool_calls(
        &tools,
        &Context::default(),
        &assistant(
            (0..12)
                .map(|index| call(&index.to_string(), "read", json!({})))
                .collect(),
        ),
        ExecutionMode::Auto,
        &NoopToolHooks,
        &CancellationToken::new(),
        &mut |_| {},
    );
    assert_eq!(batch.messages.len(), 12);
    assert!(batch.messages[..4].iter().all(|message| !message.is_error));
    assert!(batch.messages[4..].iter().all(|message| message.is_error));
    assert!(batch.messages[4..]
        .iter()
        .all(|message| message.content == vec![Content::text("Operation aborted")]));
}

struct CompletionTool {
    started: Barrier,
}
impl AgentTool for CompletionTool {
    fn name(&self) -> &str {
        "completion"
    }
    fn execution_mode(&self) -> Option<ExecutionMode> {
        Some(ExecutionMode::Parallel)
    }
    fn execute(
        &self,
        id: &str,
        _args: &Value,
        _signal: &CancellationToken,
        _update: &mut dyn FnMut(ToolResult),
    ) -> Result<ToolResult, String> {
        self.started.wait();
        if id == "first" {
            thread::sleep(Duration::from_millis(30));
        }
        Ok(ToolResult::text(id))
    }
}

#[test]
fn completion_events_can_be_out_of_order_but_message_artifacts_are_ordered() {
    let tools: Vec<Box<dyn AgentTool>> = vec![Box::new(CompletionTool {
        started: Barrier::new(2),
    })];
    let mut events = Vec::new();
    let batch = execute_tool_calls(
        &tools,
        &Context::default(),
        &assistant(vec![
            call("first", "completion", json!({})),
            call("second", "completion", json!({})),
        ]),
        ExecutionMode::Parallel,
        &NoopToolHooks,
        &CancellationToken::new(),
        &mut |event| events.push(event),
    );
    let ended: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            Event::ToolExecutionEnd { tool_call_id, .. } => Some(tool_call_id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(ended, vec!["second", "first"]);
    assert_eq!(
        batch
            .messages
            .iter()
            .map(|message| message.tool_call_id.as_str())
            .collect::<Vec<_>>(),
        vec!["first", "second"]
    );
    let artifacts: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            Event::MessageEnd {
                message: Message::ToolResult(message),
            } => Some(message.tool_call_id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(artifacts, vec!["first", "second"]);
}

#[test]
fn truncated_calls_emit_errors_and_never_terminate_the_batch() {
    let calls = vec![
        call("a", "echo", json!({"value":"partial"})),
        call("b", "read", json!({})),
    ];
    let mut events = Vec::new();
    let batch = fail_tool_calls_from_truncated_message(&calls, &mut |event| events.push(event));
    assert!(!batch.terminate);
    assert_eq!(batch.messages.len(), 2);
    assert!(batch.messages.iter().all(|message| message.is_error));
    assert!(batch.messages.iter().all(|message| matches!(&message.content[0], Content::Text { text } if text.contains("output token limit"))));
    assert_eq!(events.len(), 8);
}
