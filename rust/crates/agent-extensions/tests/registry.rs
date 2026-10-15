use agent_core::agent_loop::{run_agent_loop, LoopConfig, NoopLoopHooks, Provider};
use agent_core::tools::{
    execute_tool_calls, AfterToolCallContext, AfterToolCallResult, AgentTool,
    BeforeToolCallContext, BeforeToolCallResult, ToolHooks,
};
use agent_core::types::{
    AssistantMessage, CancellationToken, Content, Context, Event, ExecutionMode, Message,
    StopReason, ToolCall, ToolResult, Usage, Value,
};
use agent_extensions::*;
use serde_json::json;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};

struct EchoTool {
    name: String,
    label: String,
    schema: Value,
    calls: Arc<AtomicUsize>,
}

#[test]
fn commands_normalize_multiline_session_names_and_allow_explicit_clear() {
    for (value, expected) in [(" a\r\n\nb  ", "a b"), (" \r\n ", "")] {
        let registry = SharedRegistry::default();
        registry
            .register(Arc::new(Fixture {
                commands: vec![Arc::new(Command {
                    name: "rename-fixture".into(),
                    result: CommandResult {
                        session_name: Some(value.into()),
                        ..Default::default()
                    },
                    fail: false,
                    cancel: false,
                })],
                ..Fixture::new("name-fixture")
            }))
            .unwrap();
        let result = registry
            .dispatch_command(
                "rename-fixture",
                "",
                &ExtensionContext::default(),
                &CancellationToken::new(),
            )
            .unwrap();
        assert_eq!(result.session_name.as_deref(), Some(expected));
    }
}

impl EchoTool {
    fn new(name: &str, label: &str) -> Arc<Self> {
        Arc::new(Self {
            name: name.into(),
            label: label.into(),
            schema: json!({"type":"object","properties":{"value":{"type":"integer"}},"required":["value"],"additionalProperties":false}),
            calls: Arc::new(AtomicUsize::new(0)),
        })
    }
}

impl AgentTool for EchoTool {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        &self.label
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
        self.calls.fetch_add(1, Ordering::SeqCst);
        let mut result = ToolResult::text(&self.label);
        result.details = args.clone();
        Ok(result)
    }
}

struct Command {
    name: String,
    result: CommandResult,
    fail: bool,
    cancel: bool,
}

impl Command {
    fn new(name: &str, text: &str) -> Arc<Self> {
        Arc::new(Self {
            name: name.into(),
            result: CommandResult {
                text: text.into(),
                ..Default::default()
            },
            fail: false,
            cancel: false,
        })
    }
}

impl ExtensionCommand for Command {
    fn name(&self) -> &str {
        &self.name
    }
    fn execute(
        &self,
        _args: &str,
        _ctx: &ExtensionContext,
        signal: &CancellationToken,
    ) -> Result<CommandResult, String> {
        if self.cancel {
            signal.cancel();
        }
        if self.fail {
            return Err("command failed".into());
        }
        Ok(self.result.clone())
    }
}

#[derive(Default)]
struct Fixture {
    id: String,
    tools: Vec<Arc<dyn AgentTool>>,
    commands: Vec<Arc<dyn ExtensionCommand>>,
    chain: bool,
    fail: bool,
    panic: bool,
    block: bool,
    cancel: bool,
    bad_start: bool,
    after: Option<AfterToolCallResult>,
    seen: Arc<Mutex<Vec<String>>>,
    reentrant: Option<SharedRegistry>,
}

impl Fixture {
    fn new(id: &str) -> Self {
        Self {
            id: id.into(),
            ..Self::default()
        }
    }
    fn check(&self, signal: &CancellationToken) -> Result<(), String> {
        if self.cancel {
            signal.cancel();
        }
        assert!(!self.panic, "fixture panic");
        if self.fail {
            Err("fixture failed".into())
        } else {
            Ok(())
        }
    }
}

impl Extension for Fixture {
    fn id(&self) -> &str {
        &self.id
    }
    fn tools(&self) -> Vec<Arc<dyn AgentTool>> {
        self.tools.clone()
    }
    fn commands(&self) -> Vec<Arc<dyn ExtensionCommand>> {
        self.commands.clone()
    }
    fn before_agent_start(
        &self,
        event: &BeforeAgentStartEvent,
        context: &ExtensionContext,
        signal: &CancellationToken,
    ) -> Result<BeforeAgentStartResult, String> {
        self.seen.lock().unwrap().push(format!(
            "start:{}:{}",
            event.system_prompt, context.context.system_prompt
        ));
        self.check(signal)?;
        if self.bad_start {
            return Ok(BeforeAgentStartResult {
                messages: vec![Message::user("forged")],
                system_prompt: Some("bad".into()),
            });
        }
        if !self.chain {
            return Ok(Default::default());
        }
        Ok(BeforeAgentStartResult {
            messages: vec![Message::Custom {
                kind: self.id.clone(),
                data: json!({"content":"note"}),
                timestamp: 1,
            }],
            system_prompt: Some(format!("{}+{}", event.system_prompt, self.id)),
        })
    }
    fn context(
        &self,
        messages: &[Message],
        _context: &ExtensionContext,
        signal: &CancellationToken,
    ) -> Result<Option<Vec<Message>>, String> {
        self.check(signal)?;
        if let Some(registry) = &self.reentrant {
            registry.register(Arc::new(Fixture::new("added-inside-hook")))?;
            registry.unregister(&self.id)?;
        }
        if !self.chain {
            return Ok(None);
        }
        let mut current = messages.to_vec();
        current.push(Message::user(&self.id));
        Ok(Some(current))
    }
    fn before_tool_call(
        &self,
        call: BeforeToolCallContext<'_>,
        context: &ExtensionContext,
        signal: &CancellationToken,
    ) -> Result<BeforeToolCallResult, String> {
        self.seen.lock().unwrap().push(format!(
            "before:{}:{}",
            call.args,
            context.context.messages.len()
        ));
        self.check(signal)?;
        if self.chain {
            call.args["value"] = json!(format!("{}+{}", call.args["value"], self.id));
        }
        Ok(BeforeToolCallResult {
            block: self.block,
            reason: self.block.then(|| self.id.clone()),
            terminate: self.block,
        })
    }
    fn after_tool_call(
        &self,
        call: AfterToolCallContext<'_>,
        _context: &ExtensionContext,
        signal: &CancellationToken,
    ) -> Result<Option<AfterToolCallResult>, String> {
        self.seen.lock().unwrap().push(format!(
            "after:{}:{}:{}",
            call.result.content.len(),
            call.is_error,
            call.result.details
        ));
        self.check(signal)?;
        Ok(self.after.clone())
    }
    fn on_event(
        &self,
        event: &LifecycleEvent,
        _context: &ExtensionContext,
        signal: &CancellationToken,
    ) -> Result<(), String> {
        self.seen.lock().unwrap().push(format!(
            "event:{}:{}",
            event.is_terminal(),
            signal.is_cancelled()
        ));
        self.check(signal)
    }
}

fn registry_with(entries: Vec<Fixture>) -> SharedRegistry {
    let registry = SharedRegistry::default();
    for entry in entries {
        registry.register(Arc::new(entry)).unwrap();
    }
    registry
}

fn call_assistant() -> AssistantMessage {
    AssistantMessage {
        content: vec![Content::ToolCall(ToolCall {
            id: "call".into(),
            name: "echo".into(),
            arguments: json!({"value":"7"}),
        })],
        stop_reason: StopReason::ToolUse,
        ..AssistantMessage::text("")
    }
}

#[test]
fn registrations_preserve_order_and_are_shared_across_clones() {
    let registry = SharedRegistry::default();
    let cloned = registry.clone();
    registry.register(Arc::new(Fixture::new("second"))).unwrap();
    cloned.register(Arc::new(Fixture::new("first"))).unwrap();
    assert_eq!(registry.ids(), ["second", "first"]);
    assert!(format!("{registry:?}").contains("second"));
}

#[test]
fn invalid_schema_registration_is_atomic() {
    let mut fixture = Fixture::new("bad-schema");
    fixture.tools = vec![
        EchoTool::new("valid", "ok"),
        Arc::new(EchoTool {
            schema: json!({"type":"object","properties":{"x":{"pattern":".*"}}}),
            name: "invalid".into(),
            label: "bad".into(),
            calls: Arc::new(AtomicUsize::new(0)),
        }),
    ];
    let registry = SharedRegistry::default();
    assert!(registry
        .register(Arc::new(fixture))
        .unwrap_err()
        .contains("unsupported schema keyword pattern"));
    assert!(registry.ids().is_empty());
    assert!(registry.tools().is_empty());
}

#[test]
fn unsupported_and_malformed_schemas_are_not_silently_accepted() {
    for schema in [
        json!({}),
        json!({"type":"array"}),
        json!({"type":"object","required":[1]}),
        json!({"type":"object","required":["x","x"]}),
        json!({"type":"object","properties":[]}),
        json!({"type":"object","oneOf":[]}),
        json!({"type":"object","items":7}),
        json!({"type":"object","minItems":-1}),
        json!({"type":"object","multipleOf":0}),
        json!({"type":"object","typo":true}),
        json!({"type":"object","properties":{"x":{"type":"bogus"}}}),
        json!({"type":"object","$ref":"#/x"}),
    ] {
        assert!(validate_tool_schema(&schema).is_err(), "{schema}");
    }
    validate_tool_schema(&json!({"type":"object","properties":{"value":{"anyOf":[{"type":"integer","minimum":0},{"type":"null"}]}},"required":["value"],"additionalProperties":false})).unwrap();
}

#[test]
fn builtin_and_host_reserved_commands_reject_transactionally() {
    for name in [
        "help",
        "quit",
        "tree",
        "sessions",
        "compact",
        "extensions",
        "export-jsonl",
        "model",
    ] {
        let mut entry = Fixture::new("reserved");
        entry.commands.push(Command::new(name, "bad"));
        let registry = SharedRegistry::default();
        assert!(registry
            .register(Arc::new(entry))
            .unwrap_err()
            .contains("reserved"));
        assert!(registry.ids().is_empty());
    }
    let registry = SharedRegistry::default();
    registry.reserve_commands(&["host-only"]).unwrap();
    registry.reserve_tools(&["host-tool"]).unwrap();
    let mut entry = Fixture::new("host-conflict");
    entry.tools.push(EchoTool::new("host-tool", "bad"));
    assert!(registry.register(Arc::new(entry)).is_err());
}

#[test]
fn names_and_intra_extension_duplicates_fail_without_partial_install() {
    let registry = SharedRegistry::default();
    for id in ["", "bad name", "../x", "/command", "中文"] {
        assert!(registry.register(Arc::new(Fixture::new(id))).is_err());
    }
    let mut entry = Fixture::new("duplicate-tools");
    entry.tools = vec![EchoTool::new("echo", "a"), EchoTool::new("echo", "b")];
    assert!(registry
        .register(Arc::new(entry))
        .unwrap_err()
        .contains("Duplicate tool"));
    let mut entry = Fixture::new("duplicate-commands");
    entry.commands = vec![Command::new("hello", "a"), Command::new("hello", "b")];
    assert!(registry
        .register(Arc::new(entry))
        .unwrap_err()
        .contains("Duplicate command"));
    registry.register(Arc::new(Fixture::new("valid"))).unwrap();
    assert!(registry.register(Arc::new(Fixture::new("valid"))).is_err());
    assert_eq!(registry.ids(), ["valid"]);
}

#[test]
fn first_tool_wins_then_unregister_exposes_next_and_invalidates_old_wrapper() {
    let mut first = Fixture::new("first");
    first.tools.push(EchoTool::new("read", "first"));
    let mut second = Fixture::new("second");
    second.tools.push(EchoTool::new("read", "second"));
    let registry = registry_with(vec![first, second]);
    let old = registry.tools();
    assert_eq!(old[0].description(), "first");
    assert_eq!(registry.drain_diagnostics().len(), 1);
    assert!(registry.unregister("first").unwrap());
    assert!(old[0]
        .execute(
            "x",
            &json!({"value":1}),
            &CancellationToken::new(),
            &mut |_| {}
        )
        .unwrap_err()
        .contains("stale"));
    assert_eq!(registry.tools()[0].description(), "second");
    assert!(!registry.unregister("unknown").unwrap());
}

#[test]
fn reregistration_does_not_reactivate_stale_tool_wrappers() {
    let registry = SharedRegistry::default();
    let mut first = Fixture::new("same");
    first.tools.push(EchoTool::new("echo", "old"));
    registry.register(Arc::new(first)).unwrap();
    let old = registry.tools();
    registry.unregister("same").unwrap();
    let mut replacement = Fixture::new("same");
    replacement.tools.push(EchoTool::new("echo", "new"));
    registry.register(Arc::new(replacement)).unwrap();
    assert!(old[0].prepare_arguments(&json!({"value":1})).is_err());
    assert_eq!(registry.tools()[0].description(), "new");
}

#[test]
fn duplicate_commands_get_unique_observed_ts_aliases_including_literal_colons() {
    let mut a = Fixture::new("a");
    a.commands = vec![
        Command::new("hello", "a"),
        Command::new("hello:2", "literal"),
    ];
    let mut b = Fixture::new("b");
    b.commands.push(Command::new("hello", "b"));
    let registry = registry_with(vec![a, b]);
    assert_eq!(
        registry
            .commands()
            .iter()
            .map(|info| info.invocation_name.as_str())
            .collect::<Vec<_>>(),
        ["hello:1", "hello:2", "hello:3"]
    );
    assert!(!registry.contains_command("hello"));
    assert_eq!(
        registry.resolve_command("hello:3").unwrap().extension_id,
        "b"
    );
    assert_eq!(
        registry
            .dispatch_command(
                "hello:3",
                "",
                &ExtensionContext::default(),
                &CancellationToken::new()
            )
            .unwrap()
            .text,
        "b"
    );
}

#[test]
fn unknown_command_is_explicit_and_does_not_dispatch_as_a_prompt() {
    assert!(SharedRegistry::default()
        .dispatch_command(
            "missing",
            "",
            &ExtensionContext::default(),
            &CancellationToken::new()
        )
        .unwrap_err()
        .contains("not found"));
}

#[test]
fn validated_commands_return_metadata_ui_and_user_or_custom_messages() {
    let result = CommandResult {
        text: "display only".into(),
        messages: vec![Message::user("next")],
        session_name: Some("  named  ".into()),
        state: json!({"count":2}),
        ui: vec![
            UiRequest::Notification {
                level: NotificationLevel::Info,
                text: "ok".into(),
            },
            UiRequest::SetStatus {
                key: "state".into(),
                text: None,
            },
        ],
    };
    let mut entry = Fixture::new("command");
    entry.commands.push(Arc::new(Command {
        name: "hello".into(),
        result,
        fail: false,
        cancel: false,
    }));
    let registry = registry_with(vec![entry]);
    let result = registry
        .dispatch_command(
            "hello",
            "",
            &ExtensionContext::default(),
            &CancellationToken::new(),
        )
        .unwrap();
    assert_eq!(result.session_name.as_deref(), Some("named"));
    assert_eq!(result.state["count"], 2);
    assert_eq!(result.messages.len(), 1);
    assert_eq!(result.ui.len(), 2);
}

#[test]
fn commands_cannot_forge_assistants_tool_results_or_invalid_metadata() {
    for result in [
        CommandResult {
            messages: vec![Message::Assistant(AssistantMessage::text("forged"))],
            ..Default::default()
        },
        CommandResult {
            ui: vec![UiRequest::SetStatus {
                key: "".into(),
                text: None,
            }],
            ..Default::default()
        },
        CommandResult {
            messages: vec![Message::User(agent_core::types::UserMessage {
                content: vec![Content::ToolCall(ToolCall {
                    id: "x".into(),
                    name: "x".into(),
                    arguments: json!({}),
                })],
                timestamp: 1,
            })],
            ..Default::default()
        },
    ] {
        let mut entry = Fixture::new("bad-result");
        entry.commands.push(Arc::new(Command {
            name: "hello".into(),
            result,
            fail: false,
            cancel: false,
        }));
        let registry = registry_with(vec![entry]);
        assert!(registry
            .dispatch_command(
                "hello",
                "",
                &ExtensionContext::default(),
                &CancellationToken::new()
            )
            .is_err());
        assert_eq!(registry.drain_diagnostics().len(), 1);
    }
}

#[test]
fn command_failure_and_cancellation_return_no_partial_command_result() {
    let mut entry = Fixture::new("failed");
    entry.commands.push(Arc::new(Command {
        name: "hello".into(),
        result: Default::default(),
        fail: true,
        cancel: false,
    }));
    let registry = registry_with(vec![entry]);
    assert!(registry
        .dispatch_command(
            "hello",
            "",
            &ExtensionContext::default(),
            &CancellationToken::new()
        )
        .is_err());
    assert_eq!(registry.drain_diagnostics()[0].event, "command");
    let mut entry = Fixture::new("cancelled");
    entry.commands.push(Arc::new(Command {
        name: "hello".into(),
        result: Default::default(),
        fail: false,
        cancel: true,
    }));
    let registry = registry_with(vec![entry]);
    assert_eq!(
        registry
            .dispatch_command(
                "hello",
                "",
                &ExtensionContext::default(),
                &CancellationToken::new()
            )
            .unwrap_err(),
        "Operation aborted"
    );
}

#[test]
fn before_start_chains_system_prompt_and_collects_custom_messages() {
    let mut a = Fixture::new("a");
    a.chain = true;
    let a_seen = a.seen.clone();
    let mut b = Fixture::new("b");
    b.chain = true;
    let b_seen = b.seen.clone();
    let registry = registry_with(vec![a, b]);
    let event = BeforeAgentStartEvent {
        prompt: "input".into(),
        system_prompt: "base".into(),
        ..Default::default()
    };
    let result = registry
        .before_agent_start(
            &event,
            &ExtensionContext::default(),
            &CancellationToken::new(),
        )
        .unwrap();
    assert_eq!(result.system_prompt.as_deref(), Some("base+a+b"));
    assert_eq!(result.messages.len(), 2);
    assert_eq!(a_seen.lock().unwrap()[0], "start:base:base");
    assert_eq!(b_seen.lock().unwrap()[0], "start:base+a:base+a");
    assert_eq!(event.system_prompt, "base");
}

#[test]
fn ordinary_hook_failures_and_panics_are_diagnostic_and_later_handlers_run() {
    let mut bad = Fixture::new("failed");
    bad.fail = true;
    let mut panic = Fixture::new("panic");
    panic.panic = true;
    let mut good = Fixture::new("good");
    good.chain = true;
    let registry = registry_with(vec![bad, panic, good]);
    let result = registry
        .before_agent_start(
            &BeforeAgentStartEvent::default(),
            &ExtensionContext::default(),
            &CancellationToken::new(),
        )
        .unwrap();
    assert_eq!(result.messages.len(), 1);
    let diagnostics = registry.drain_diagnostics();
    assert_eq!(diagnostics.len(), 2);
    assert!(diagnostics[1].error.contains("panicked"));
}

#[test]
fn invalid_before_start_result_is_not_partially_applied() {
    let mut bad = Fixture::new("bad");
    bad.bad_start = true;
    let registry = registry_with(vec![bad]);
    let result = registry
        .before_agent_start(
            &BeforeAgentStartEvent::default(),
            &ExtensionContext::default(),
            &CancellationToken::new(),
        )
        .unwrap();
    assert_eq!(result, BeforeAgentStartResult::default());
    assert_eq!(registry.drain_diagnostics().len(), 1);
}

#[test]
fn context_transform_is_ordered_nonpersistent_and_failure_tolerant() {
    let mut a = Fixture::new("a");
    a.chain = true;
    let mut failed = Fixture::new("failed");
    failed.fail = true;
    let mut b = Fixture::new("b");
    b.chain = true;
    let registry = registry_with(vec![a, failed, b]);
    let original = vec![Message::user("input")];
    let result = registry
        .transform_context(
            &original,
            &ExtensionContext::default(),
            &CancellationToken::new(),
        )
        .unwrap();
    assert_eq!(
        result.iter().map(Message::text).collect::<Vec<_>>(),
        ["input", "a", "b"]
    );
    assert_eq!(original.len(), 1);
    assert_eq!(registry.drain_diagnostics().len(), 1);
}

#[test]
fn before_tool_patches_chain_after_schema_validation_without_revalidation() {
    let tool = EchoTool::new("echo", "ran");
    let mut a = Fixture::new("a");
    a.chain = true;
    a.tools.push(tool.clone());
    let mut b = Fixture::new("b");
    b.chain = true;
    let seen = b.seen.clone();
    let registry = registry_with(vec![a, b]);
    let assistant = call_assistant();
    let context = Context {
        messages: vec![Message::user("input")],
        ..Default::default()
    };
    let result = execute_tool_calls(
        &registry.tools(),
        &context,
        &assistant,
        ExecutionMode::Sequential,
        &registry.tool_hooks(ExtensionContext::default()),
        &CancellationToken::new(),
        &mut |_| {},
    );
    assert_eq!(tool.calls.load(Ordering::SeqCst), 1);
    assert!(!result.messages[0].is_error);
    assert!(result.messages[0].details["value"]
        .as_str()
        .unwrap()
        .contains("+b"));
    assert!(seen.lock().unwrap()[0].contains("+a"));
    assert!(seen.lock().unwrap()[0].ends_with(":1"));
}

#[test]
fn blocked_tool_short_circuits_later_hooks_and_terminates_batch() {
    let tool = EchoTool::new("echo", "never");
    let mut blocker = Fixture::new("blocker");
    blocker.block = true;
    blocker.tools.push(tool.clone());
    let later = Fixture::new("later");
    let seen = later.seen.clone();
    let registry = registry_with(vec![blocker, later]);
    let result = execute_tool_calls(
        &registry.tools(),
        &Context::default(),
        &call_assistant(),
        ExecutionMode::Sequential,
        &registry.tool_hooks(ExtensionContext::default()),
        &CancellationToken::new(),
        &mut |_| {},
    );
    assert!(result.terminate);
    assert!(result.messages[0].is_error);
    assert_eq!(tool.calls.load(Ordering::SeqCst), 0);
    assert!(seen.lock().unwrap().is_empty());
}

#[test]
fn preflight_failure_is_fail_closed_not_an_ordinary_ignored_hook() {
    let tool = EchoTool::new("echo", "never");
    let mut failed = Fixture::new("failed");
    failed.fail = true;
    failed.tools.push(tool.clone());
    let registry = registry_with(vec![failed]);
    let result = execute_tool_calls(
        &registry.tools(),
        &Context::default(),
        &call_assistant(),
        ExecutionMode::Sequential,
        &registry.tool_hooks(ExtensionContext::default()),
        &CancellationToken::new(),
        &mut |_| {},
    );
    assert_eq!(tool.calls.load(Ordering::SeqCst), 0);
    assert!(result.messages[0].is_error);
    assert_eq!(registry.drain_diagnostics()[0].event, "tool_call");
}

#[test]
fn after_tool_patches_chain_preserving_omitted_fields_and_zero_usage() {
    let mut a = Fixture::new("a");
    a.after = Some(AfterToolCallResult {
        content: Some(vec![Content::text("replacement")]),
        details: Some(Value::Null),
        usage: Some(Usage::default()),
        is_error: Some(true),
        terminate: Some(true),
    });
    let mut b = Fixture::new("b");
    b.after = Some(AfterToolCallResult {
        terminate: Some(false),
        ..Default::default()
    });
    let seen = b.seen.clone();
    let registry = registry_with(vec![a, b]);
    let assistant = call_assistant();
    let call = assistant.tool_calls().remove(0);
    let original = ToolResult::text("original");
    let patch = registry
        .tool_hooks(ExtensionContext::default())
        .after_tool_call(
            AfterToolCallContext {
                assistant_message: &assistant,
                tool_call: &call,
                args: &json!({}),
                result: &original,
                is_error: false,
                context: &Context::default(),
            },
            &CancellationToken::new(),
        )
        .unwrap()
        .unwrap();
    assert_eq!(patch.content, Some(vec![Content::text("replacement")]));
    assert_eq!(patch.details, Some(Value::Null));
    assert_eq!(patch.usage, Some(Usage::default()));
    assert_eq!(patch.terminate, Some(false));
    assert_eq!(patch.is_error, Some(true));
    assert_eq!(seen.lock().unwrap()[0], "after:1:true:null");
    assert_eq!(original.content, vec![Content::text("original")]);
}

#[test]
fn after_tool_failure_keeps_real_output_and_runs_later_patches() {
    let mut failed = Fixture::new("failed");
    failed.fail = true;
    let mut later = Fixture::new("later");
    later.after = Some(AfterToolCallResult {
        details: Some(json!({"retained":true})),
        ..Default::default()
    });
    let registry = registry_with(vec![failed, later]);
    let assistant = call_assistant();
    let call = assistant.tool_calls().remove(0);
    let original = ToolResult::text("original");
    let patch = registry
        .tool_hooks(ExtensionContext::default())
        .after_tool_call(
            AfterToolCallContext {
                assistant_message: &assistant,
                tool_call: &call,
                args: &json!({}),
                result: &original,
                is_error: false,
                context: &Context::default(),
            },
            &CancellationToken::new(),
        )
        .unwrap()
        .unwrap();
    assert!(patch.content.is_none());
    assert_eq!(patch.details, Some(json!({"retained":true})));
    assert_eq!(registry.drain_diagnostics().len(), 1);
}

#[test]
fn cancellation_skips_new_work_but_preserves_completed_tool_result() {
    let fixture = Fixture::new("observer");
    let seen = fixture.seen.clone();
    let registry = registry_with(vec![fixture]);
    let signal = CancellationToken::new();
    signal.cancel();
    assert_eq!(
        registry
            .transform_context(&[], &ExtensionContext::default(), &signal)
            .unwrap_err(),
        "Operation aborted"
    );
    let assistant = call_assistant();
    let call = assistant.tool_calls().remove(0);
    let result = ToolResult::text("completed");
    assert!(registry
        .tool_hooks(ExtensionContext::default())
        .after_tool_call(
            AfterToolCallContext {
                assistant_message: &assistant,
                tool_call: &call,
                args: &json!({}),
                result: &result,
                is_error: false,
                context: &Context::default()
            },
            &signal
        )
        .unwrap()
        .is_none());
    assert!(seen.lock().unwrap().is_empty());
}

#[test]
fn cancelled_runs_still_emit_terminal_cleanup_lifecycle() {
    let fixture = Fixture::new("observer");
    let seen = fixture.seen.clone();
    let registry = registry_with(vec![fixture]);
    let signal = CancellationToken::new();
    signal.cancel();
    registry
        .emit_event(
            &LifecycleEvent::Agent {
                event: Event::AgentEnd { messages: vec![] },
            },
            &ExtensionContext::default(),
            &signal,
        )
        .unwrap();
    registry
        .emit_event(
            &LifecycleEvent::AgentSettled,
            &ExtensionContext::default(),
            &signal,
        )
        .unwrap();
    registry
        .emit_event(
            &LifecycleEvent::SessionShutdown {
                reason: SessionShutdownReason::Quit,
                target_session_file: None,
            },
            &ExtensionContext::default(),
            &signal,
        )
        .unwrap();
    assert_eq!(seen.lock().unwrap().len(), 3);
    assert!(registry
        .emit_event(
            &LifecycleEvent::Agent {
                event: Event::AgentStart
            },
            &ExtensionContext::default(),
            &signal
        )
        .is_err());
}

#[test]
fn callbacks_can_register_and_unregister_without_holding_registry_lock() {
    let registry = SharedRegistry::default();
    let mut fixture = Fixture::new("reentrant");
    fixture.reentrant = Some(registry.clone());
    registry.register(Arc::new(fixture)).unwrap();
    registry
        .transform_context(&[], &ExtensionContext::default(), &CancellationToken::new())
        .unwrap();
    assert_eq!(registry.ids(), ["added-inside-hook"]);
}

#[test]
fn concurrent_registration_is_atomic_and_thread_safe() {
    let registry = SharedRegistry::default();
    std::thread::scope(|scope| {
        for index in 0..8 {
            let registry = registry.clone();
            scope.spawn(move || {
                registry
                    .register(Arc::new(Fixture::new(&format!("extension-{index}"))))
                    .unwrap()
            });
        }
    });
    assert_eq!(registry.ids().len(), 8);
    assert!(registry.reserve_commands(&["hello"]).is_ok());
}

#[test]
fn states_and_typed_results_serialize_without_secrets_or_unknown_ui_variants() {
    let mut context = ExtensionContext::default();
    context.states.insert("ext".into(), json!({"count":1}));
    context.model = Some(ModelIdentity {
        provider: "demo".into(),
        model: "offline".into(),
    });
    let encoded = serde_json::to_value(&context).unwrap();
    assert_eq!(encoded["states"]["ext"]["count"], 1);
    assert_eq!(
        serde_json::from_value::<ExtensionContext>(encoded).unwrap(),
        context
    );
    assert!(serde_json::from_value::<UiRequest>(json!({"type":"confirm","title":"fake"})).is_err());
    assert!(serde_json::from_value::<CommandResult>(json!({"unknown":true})).is_err());
}

#[test]
fn custom_content_matches_observed_ts_shape_without_forging_tool_calls() {
    let message = custom_message(
        "native-note",
        CustomMessageContent::Text("note".into()),
        false,
        json!({"state":1}),
    )
    .unwrap();
    assert_eq!(
        custom_message_content(&message).unwrap(),
        Some(vec![Content::text("note")])
    );
    let encoded = serde_json::to_value(&message).unwrap();
    assert_eq!(encoded["data"]["display"], false);
    assert_eq!(encoded["data"]["details"]["state"], 1);
    assert!(custom_message(
        "note",
        CustomMessageContent::Blocks(vec![Content::Thinking {
            thinking: "forged".into()
        }]),
        true,
        Value::Null
    )
    .is_err());
    let state = Message::Custom {
        kind: "state-only".into(),
        data: json!({"count":1}),
        timestamp: 1,
    };
    assert_eq!(custom_message_content(&state).unwrap(), None);
}

#[test]
fn cancellation_inside_before_hook_prevents_later_hooks_and_execution() {
    let tool = EchoTool::new("echo", "never");
    let mut first = Fixture::new("cancel");
    first.cancel = true;
    first.tools.push(tool.clone());
    let later = Fixture::new("later");
    let seen = later.seen.clone();
    let registry = registry_with(vec![first, later]);
    let signal = CancellationToken::new();
    let result = execute_tool_calls(
        &registry.tools(),
        &Context::default(),
        &call_assistant(),
        ExecutionMode::Sequential,
        &registry.tool_hooks(ExtensionContext::default()),
        &signal,
        &mut |_| {},
    );
    assert!(result.messages[0].is_error);
    assert_eq!(tool.calls.load(Ordering::SeqCst), 0);
    assert!(seen.lock().unwrap().is_empty());
    assert!(signal.is_cancelled());
}

#[test]
fn diagnostics_are_bounded_and_drained_across_shared_handles() {
    let mut fixture = Fixture::new("failing");
    fixture.fail = true;
    let registry = registry_with(vec![fixture]);
    for _ in 0..1100 {
        registry
            .transform_context(&[], &ExtensionContext::default(), &CancellationToken::new())
            .unwrap();
    }
    let clone = registry.clone();
    assert_eq!(clone.drain_diagnostics().len(), 1024);
    assert!(registry.drain_diagnostics().is_empty());
}

#[test]
fn checked_lifecycle_reports_candidate_failure_but_observation_remains_best_effort() {
    let mut failed = Fixture::new("failed-candidate");
    failed.fail = true;
    let later = Fixture::new("later");
    let seen = later.seen.clone();
    let registry = registry_with(vec![failed, later]);
    let event = LifecycleEvent::SessionStart {
        reason: SessionStartReason::Reload,
        previous_session_file: None,
    };
    assert!(registry
        .emit_event_checked(
            &event,
            &ExtensionContext::default(),
            &CancellationToken::new()
        )
        .unwrap_err()
        .contains("failed-candidate lifecycle"));
    assert!(seen.lock().unwrap().is_empty());
    assert_eq!(registry.drain_diagnostics().len(), 1);
    registry
        .emit_event(
            &event,
            &ExtensionContext::default(),
            &CancellationToken::new(),
        )
        .unwrap();
    assert_eq!(seen.lock().unwrap().len(), 1);
    assert_eq!(registry.drain_diagnostics().len(), 1);
}

struct FixedProvider;
impl Provider for FixedProvider {
    fn complete(
        &mut self,
        context: &Context,
        _signal: &CancellationToken,
        _update: &mut dyn FnMut(AssistantMessage),
    ) -> Result<AssistantMessage, String> {
        if context
            .messages
            .iter()
            .any(|message| matches!(message, Message::ToolResult(_)))
        {
            Ok(AssistantMessage::text("done"))
        } else {
            Ok(call_assistant())
        }
    }
}

#[test]
fn registered_native_tool_runs_through_actual_core_loop_with_pairing() {
    let tool = EchoTool::new("echo", "offline output");
    let mut extension = Fixture::new("native");
    extension.tools.push(tool.clone());
    let registry = registry_with(vec![extension]);
    let mut events = Vec::new();
    let messages = run_agent_loop(
        vec![Message::user("input")],
        &Context::default(),
        LoopConfig::default(),
        &mut FixedProvider,
        &registry.tools(),
        &mut NoopLoopHooks,
        &registry.tool_hooks(ExtensionContext::default()),
        &CancellationToken::new(),
        &mut |event| events.push(event),
    )
    .unwrap();
    assert_eq!(tool.calls.load(Ordering::SeqCst), 1);
    assert_eq!(messages.len(), 4);
    assert_eq!(messages[2].text(), "offline output");
    assert_eq!(messages[3].text(), "done");
    assert!(matches!(events.last(), Some(Event::AgentEnd { .. })));
}
