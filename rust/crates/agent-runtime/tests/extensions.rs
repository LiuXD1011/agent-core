#[path = "common/native_extension.rs"]
mod fixture;
use agent_core::agent_loop::Provider;
use agent_core::tools::{
    AfterToolCallContext, AfterToolCallResult, AgentTool, BeforeToolCallContext,
    BeforeToolCallResult,
};
use agent_core::types::{
    AssistantMessage, CancellationToken, Content, Context, Event, Message, StopReason, ToolCall,
    ToolResult, Usage,
};
use agent_runtime::automatic::{estimate_context_tokens_for_model, AutomationOptions, ModelBudget};
use agent_runtime::extensions::{
    custom_message, execute_extension_command, extension_context, load_native_extensions,
    BeforeAgentStartEvent, BeforeAgentStartResult, CommandResult, CustomMessageContent, Extension,
    ExtensionCommand, ExtensionContext, ExtensionMode, LifecycleEvent, ModelIdentity,
    SessionShutdownReason, SessionStartReason, SharedRegistry, UiBroker,
};
use agent_runtime::runtime::{configured_tools, run_with_provider, MessageQueues, RunOptions};
use agent_runtime::session::{Session, SessionStore};
use fixture::{assert_gone, wait_pid, Fixture};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::fs;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Default)]
struct Fixed {
    responses: VecDeque<AssistantMessage>,
    requests: Vec<Context>,
}
impl Provider for Fixed {
    fn complete(
        &mut self,
        context: &Context,
        _: &CancellationToken,
        _: &mut dyn FnMut(AssistantMessage),
    ) -> Result<AssistantMessage, String> {
        self.requests.push(context.clone());
        Ok(self
            .responses
            .pop_front()
            .unwrap_or_else(|| AssistantMessage::text("done")))
    }
}
fn tool_call(name: &str) -> AssistantMessage {
    AssistantMessage {
        content: vec![Content::ToolCall(ToolCall {
            id: "call-1".into(),
            name: name.into(),
            arguments: json!({"text":"original"}),
        })],
        stop_reason: StopReason::ToolUse,
        ..AssistantMessage::text("")
    }
}
fn load(fixture: &Fixture, session: &Session) -> SharedRegistry {
    let context = extension_context(
        session,
        &SharedRegistry::default(),
        ExtensionMode::Rpc,
        None,
    )
    .unwrap();
    load_native_extensions(
        std::slice::from_ref(&fixture.manifest),
        &context,
        &CancellationToken::new(),
    )
    .unwrap()
}
fn write_args(fixture: &Fixture, args: Vec<String>) {
    let mut manifest = fixture.manifest();
    manifest.args = args;
    fs::write(&fixture.manifest, serde_json::to_vec(&manifest).unwrap()).unwrap();
}

#[test]
fn native_tool_runs_core_hooks_streams_and_converts_custom_prompt_without_reentry_deadlock() {
    let fixture = Fixture::new(&[]);
    let record = fixture.path.join("requests.jsonl");
    write_args(
        &fixture,
        vec![
            "--inject".into(),
            "--mutate".into(),
            "--patch".into(),
            "--echo-context".into(),
            format!("--record={}", record.display()),
        ],
    );
    let mut session = fixture.session();
    session.context.system_prompt = "original system".into();
    let registry = load(&fixture, &session);
    let options = RunOptions {
        extensions: registry.clone(),
        tools: Some(vec!["fixture_echo".into()]),
        extension_mode: ExtensionMode::Rpc,
        extension_model: Some(ModelIdentity {
            provider: "offline".into(),
            model: "fixture".into(),
        }),
        ..Default::default()
    };
    let tools = configured_tools(&session.workspace, &options).unwrap();
    let mut provider = Fixed {
        responses: vec![tool_call("fixture_echo"), AssistantMessage::text("done")].into(),
        ..Default::default()
    };
    let mut events = vec![];
    run_with_provider(
        &mut session,
        Message::user("actual user"),
        &mut provider,
        &tools,
        &options,
        &MessageQueues::default(),
        &CancellationToken::new(),
        &mut |event| events.push(event),
    )
    .unwrap();
    assert!(registry.drain_diagnostics().is_empty());
    assert_eq!(provider.requests[0].system_prompt, "native system");
    assert_eq!(session.context.system_prompt, "original system");
    assert_eq!(
        provider.requests[0]
            .messages
            .iter()
            .filter(|message| message.text() == "actual user")
            .count(),
        1
    );
    assert_eq!(
        provider.requests[0]
            .messages
            .iter()
            .filter(|message| message.text() == "native injected")
            .count(),
        1
    );
    assert!(session
        .context
        .messages
        .iter()
        .any(|message| matches!(message, Message::Custom { .. })));
    let result = session
        .context
        .messages
        .iter()
        .find_map(|message| {
            if let Message::ToolResult(result) = message {
                Some(result)
            } else {
                None
            }
        })
        .unwrap();
    assert!(result.is_error);
    assert_eq!(result.details, Value::Null);
    assert_eq!(result.content, vec![Content::text("mutated by native")]);
    assert!(events
        .iter()
        .any(|event| matches!(event, Event::ToolExecutionUpdate { .. })));
    let requests = fs::read_to_string(record)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    let execute = requests
        .iter()
        .find(|request| request["method"] == "execute")
        .unwrap();
    assert_eq!(execute["params"]["context"]["mode"], "rpc");
    assert_eq!(execute["params"]["context"]["model"]["model"], "fixture");
    assert!(requests
        .iter()
        .any(|request| request["method"] == "lifecycle"
            && request["params"]["event"]["event"]["type"] == "tool_execution_update"));
    assert_eq!(
        requests
            .iter()
            .filter(|request| request["params"]["event"]["type"] == "agent_settled")
            .count(),
        1
    );
}

#[test]
fn native_commands_atomically_persist_messages_title_ui_and_branch_local_state() {
    let fixture = Fixture::new(&[]);
    let mut session = fixture.session();
    let registry = load(&fixture, &session);
    assert!(
        !extension_context(&session, &registry, ExtensionMode::Desktop, None)
            .unwrap()
            .has_ui
    );
    let host_ui = UiBroker::new();
    registry.bind_ui(host_ui.handle()).unwrap();
    let context = extension_context(&session, &registry, ExtensionMode::Desktop, None).unwrap();
    assert!(context.has_ui);
    let result = execute_extension_command(
        &mut session,
        &registry,
        "fixture_echo",
        "中文🙂",
        &context,
        &CancellationToken::new(),
    )
    .unwrap();
    assert_eq!(result.text, "native command");
    assert_eq!(result.ui.len(), 1);
    assert_eq!(session.name, "Native Fixture");
    assert_eq!(session.context.messages.len(), 1);
    assert!(matches!(session.context.messages[0], Message::User(_)));
    assert_eq!(
        session.runtime_state("extension:fixture").unwrap(),
        Some(json!({"count":1}))
    );
    let store = SessionStore::new(fixture.path.join("sessions"));
    store.save(&session).unwrap();
    let mut restored = store.list().unwrap().pop().unwrap();
    assert_eq!(
        extension_context(&restored, &registry, ExtensionMode::Print, None)
            .unwrap()
            .states["fixture"],
        json!({"count":1})
    );
    execute_extension_command(
        &mut restored,
        &registry,
        "fixture_echo",
        "",
        &context,
        &CancellationToken::new(),
    )
    .unwrap();
    assert_eq!(
        restored.runtime_state("extension:fixture").unwrap(),
        Some(json!({"count":2}))
    );
    restored.navigate(None).unwrap();
    assert!(
        !extension_context(&restored, &registry, ExtensionMode::Print, None)
            .unwrap()
            .states
            .contains_key("fixture")
    );
}

#[test]
fn terminal_lifecycle_uses_a_short_cleanup_token_even_when_caller_cancelled() {
    let fixture = Fixture::new(&[]);
    let record = fixture.path.join("events.jsonl");
    let pid_file = fixture.path.join("pid");
    write_args(
        &fixture,
        vec![
            format!("--record={}", record.display()),
            format!("--pid={}", pid_file.display()),
        ],
    );
    let session = fixture.session();
    let registry = load(&fixture, &session);
    let pid = wait_pid(&pid_file);
    let context = extension_context(&session, &registry, ExtensionMode::Rpc, None).unwrap();
    let signal = CancellationToken::new();
    signal.cancel();
    let start = Instant::now();
    registry
        .emit_event(
            &LifecycleEvent::SessionShutdown {
                reason: SessionShutdownReason::Quit,
                target_session_file: None,
            },
            &context,
            &signal,
        )
        .unwrap();
    assert!(start.elapsed() < Duration::from_secs(2));
    assert_gone(pid);
    assert!(fs::read_to_string(record)
        .unwrap()
        .contains("session_shutdown"));
    assert!(registry.drain_diagnostics().is_empty());
}

#[test]
fn stalled_terminal_cleanup_is_bounded_and_still_closes_its_job() {
    let fixture = Fixture::new(&[]);
    let pid_file = fixture.path.join("pid");
    write_args(
        &fixture,
        vec![
            "--hang-lifecycle".into(),
            format!("--pid={}", pid_file.display()),
        ],
    );
    let session = fixture.session();
    let registry = load(&fixture, &session);
    let pid = wait_pid(&pid_file);
    let context = extension_context(&session, &registry, ExtensionMode::Rpc, None).unwrap();
    let start = Instant::now();
    registry
        .emit_event(
            &LifecycleEvent::SessionShutdown {
                reason: SessionShutdownReason::Reload,
                target_session_file: None,
            },
            &context,
            &CancellationToken::new(),
        )
        .unwrap();
    assert!(start.elapsed() < Duration::from_secs(2));
    assert_gone(pid);
    assert!(registry
        .drain_diagnostics()
        .iter()
        .any(|diagnostic| diagnostic.error.contains("timed out")));
}

#[test]
fn native_builtin_overrides_obey_all_host_tool_enablement_flags() {
    for (name, enabled) in [
        ("read", true),
        ("write", false),
        ("edit", false),
        ("run_command", false),
    ] {
        let flag = format!("--tool={name}");
        let fixture = Fixture::new(&[&flag]);
        let session = fixture.session();
        let registry = load(&fixture, &session);
        let options = RunOptions {
            extensions: registry,
            ..Default::default()
        };
        let tools = configured_tools(&session.workspace, &options).unwrap();
        let matching = tools
            .iter()
            .filter(|tool| tool.name() == name)
            .collect::<Vec<_>>();
        assert_eq!(matching.len(), usize::from(enabled));
        if enabled {
            assert_eq!(matching[0].description(), "Offline native fixture");
        }
        assert!(configured_tools(
            &session.workspace,
            &RunOptions {
                no_tools: true,
                ..options.clone()
            }
        )
        .unwrap()
        .is_empty());
        assert!(!configured_tools(
            &session.workspace,
            &RunOptions {
                exclude_tools: vec![name.into()],
                ..options.clone()
            }
        )
        .unwrap()
        .iter()
        .any(|tool| tool.name() == name));
        assert_eq!(
            configured_tools(
                &session.workspace,
                &RunOptions {
                    allow_changes: true,
                    allow_commands: true,
                    tools: Some(vec![name.into()]),
                    ..options
                }
            )
            .unwrap()
            .len(),
            1
        );
    }
}

#[test]
fn native_reserved_commands_and_malformed_tool_result_are_reported() {
    let fixture = Fixture::new(&["--command=new"]);
    let context = extension_context(
        &fixture.session(),
        &SharedRegistry::default(),
        ExtensionMode::Rpc,
        None,
    )
    .unwrap();
    assert!(load_native_extensions(
        std::slice::from_ref(&fixture.manifest),
        &context,
        &CancellationToken::new()
    )
    .unwrap_err()
    .contains("reserved"));
    let fixture = Fixture::new(&["--bad-tool-result"]);
    let mut session = fixture.session();
    let registry = load(&fixture, &session);
    let options = RunOptions {
        extensions: registry,
        tools: Some(vec!["fixture_echo".into()]),
        ..Default::default()
    };
    let tools = configured_tools(&session.workspace, &options).unwrap();
    let mut provider = Fixed {
        responses: vec![tool_call("fixture_echo"), AssistantMessage::text("done")].into(),
        ..Default::default()
    };
    run_with_provider(
        &mut session,
        Message::user("call"),
        &mut provider,
        &tools,
        &options,
        &MessageQueues::default(),
        &CancellationToken::new(),
        &mut |_| {},
    )
    .unwrap();
    assert!(session.context.messages.iter().any(|message|matches!(message,Message::ToolResult(result) if result.is_error && message.text().contains("Malformed native tool result"))));
}

struct Echo {
    calls: Arc<AtomicUsize>,
    name: &'static str,
}
impl AgentTool for Echo {
    fn name(&self) -> &str {
        self.name
    }
    fn execute(
        &self,
        _: &str,
        args: &Value,
        _: &CancellationToken,
        _: &mut dyn FnMut(ToolResult),
    ) -> Result<ToolResult, String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(ToolResult::text(args["text"].as_str().unwrap_or("echo")))
    }
}
struct InProcess {
    calls: Arc<AtomicUsize>,
    events: Arc<Mutex<Vec<LifecycleEvent>>>,
    block: bool,
    fail: bool,
}
impl Extension for InProcess {
    fn id(&self) -> &str {
        "in_process"
    }
    fn tools(&self) -> Vec<Arc<dyn AgentTool>> {
        vec![Arc::new(Echo {
            calls: self.calls.clone(),
            name: "fixture",
        })]
    }
    fn before_agent_start(
        &self,
        event: &BeforeAgentStartEvent,
        _: &ExtensionContext,
        _: &CancellationToken,
    ) -> Result<BeforeAgentStartResult, String> {
        assert_eq!(event.images.len(), 1);
        Ok(BeforeAgentStartResult {
            messages: vec![custom_message(
                "native",
                CustomMessageContent::Text("injected".into()),
                false,
                Value::Null,
            )
            .unwrap()],
            system_prompt: Some("hook system".into()),
        })
    }
    fn context(
        &self,
        messages: &[Message],
        context: &ExtensionContext,
        _: &CancellationToken,
    ) -> Result<Option<Vec<Message>>, String> {
        assert_eq!(context.context.system_prompt, "hook system");
        let mut messages = messages.to_vec();
        messages.push(Message::user("ephemeral view"));
        Ok(Some(messages))
    }
    fn before_tool_call(
        &self,
        call: BeforeToolCallContext<'_>,
        _: &ExtensionContext,
        _: &CancellationToken,
    ) -> Result<BeforeToolCallResult, String> {
        if self.fail {
            return Err("hook failed closed".into());
        }
        *call.args = json!({"text":"before mutation"});
        Ok(BeforeToolCallResult {
            block: self.block,
            reason: Some("fixture blocked".into()),
            terminate: self.block,
        })
    }
    fn after_tool_call(
        &self,
        _: AfterToolCallContext<'_>,
        _: &ExtensionContext,
        _: &CancellationToken,
    ) -> Result<Option<AfterToolCallResult>, String> {
        Ok(Some(AfterToolCallResult {
            content: Some(vec![Content::text("after patch")]),
            details: Some(json!({"patched":true})),
            ..Default::default()
        }))
    }
    fn on_event(
        &self,
        event: &LifecycleEvent,
        _: &ExtensionContext,
        _: &CancellationToken,
    ) -> Result<(), String> {
        self.events.lock().unwrap().push(event.clone());
        Ok(())
    }
}
fn image_prompt() -> Message {
    let mut prompt = Message::user("actual user");
    if let Message::User(user) = &mut prompt {
        user.content.push(Content::Image {
            data: "aW1hZ2U=".into(),
            mime_type: "image/png".into(),
        });
    }
    prompt
}

#[test]
fn in_process_extension_hooks_use_the_real_loop_and_do_not_persist_context_views() {
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = Arc::new(Mutex::new(vec![]));
    let registry = SharedRegistry::default();
    registry
        .register(Arc::new(InProcess {
            calls: calls.clone(),
            events: observed.clone(),
            block: false,
            fail: false,
        }))
        .unwrap();
    let mut session = Session::default();
    let options = RunOptions {
        extensions: registry,
        ..Default::default()
    };
    let tools = configured_tools(&session.workspace, &options).unwrap();
    let mut provider = Fixed {
        responses: vec![tool_call("fixture"), AssistantMessage::text("done")].into(),
        ..Default::default()
    };
    run_with_provider(
        &mut session,
        image_prompt(),
        &mut provider,
        &tools,
        &options,
        &MessageQueues::default(),
        &CancellationToken::new(),
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(provider.requests[0].system_prompt, "hook system");
    assert_eq!(
        provider.requests[0].messages.last().unwrap().text(),
        "ephemeral view"
    );
    assert!(provider.requests[0]
        .messages
        .iter()
        .any(|message| message.text() == "injected"));
    assert!(!session
        .context
        .messages
        .iter()
        .any(|message| message.text() == "ephemeral view"));
    assert_ne!(session.context.system_prompt, "hook system");
    assert!(session.context.messages.iter().any(|message|matches!(message,Message::ToolResult(result) if result.details==json!({"patched":true}) && message.text()=="after patch")));
    assert_eq!(
        observed
            .lock()
            .unwrap()
            .iter()
            .filter(|event| matches!(event, LifecycleEvent::AgentSettled))
            .count(),
        1
    );
}

#[test]
fn blocking_and_failed_before_tool_hooks_never_execute_the_tool() {
    for fail in [false, true] {
        let calls = Arc::new(AtomicUsize::new(0));
        let registry = SharedRegistry::default();
        registry
            .register(Arc::new(InProcess {
                calls: calls.clone(),
                events: Arc::new(Mutex::new(vec![])),
                block: !fail,
                fail,
            }))
            .unwrap();
        let mut session = Session::default();
        let options = RunOptions {
            extensions: registry.clone(),
            ..Default::default()
        };
        let tools = configured_tools(&session.workspace, &options).unwrap();
        let mut provider = Fixed {
            responses: vec![tool_call("fixture"), AssistantMessage::text("done")].into(),
            ..Default::default()
        };
        run_with_provider(
            &mut session,
            image_prompt(),
            &mut provider,
            &tools,
            &options,
            &MessageQueues::default(),
            &CancellationToken::new(),
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(session
            .context
            .messages
            .iter()
            .any(|message| matches!(message,Message::ToolResult(result) if result.is_error)));
        if fail {
            assert!(registry
                .drain_diagnostics()
                .iter()
                .any(|error| error.error.contains("failed closed")));
        }
    }
}

struct InvalidCommand {
    cancel: bool,
}
impl ExtensionCommand for InvalidCommand {
    fn name(&self) -> &str {
        "invalid"
    }
    fn execute(
        &self,
        _: &str,
        _: &ExtensionContext,
        signal: &CancellationToken,
    ) -> Result<CommandResult, String> {
        if self.cancel {
            signal.cancel();
        }
        Ok(CommandResult {
            messages: vec![Message::Assistant(AssistantMessage::text("forged"))],
            session_name: Some("changed".into()),
            state: json!({"changed":true}),
            ..Default::default()
        })
    }
}
impl Extension for InvalidCommand {
    fn id(&self) -> &str {
        "invalid"
    }
    fn commands(&self) -> Vec<Arc<dyn ExtensionCommand>> {
        vec![Arc::new(Self {
            cancel: self.cancel,
        })]
    }
}
#[test]
fn failed_or_cancelled_commands_leave_the_entire_session_unchanged() {
    for cancel in [false, true] {
        let registry = SharedRegistry::default();
        registry
            .register(Arc::new(InvalidCommand { cancel }))
            .unwrap();
        let mut session = Session::default();
        let before = serde_json::to_value(&session).unwrap();
        let context = extension_context(&session, &registry, ExtensionMode::Print, None).unwrap();
        assert!(execute_extension_command(
            &mut session,
            &registry,
            "invalid",
            "",
            &context,
            &CancellationToken::new()
        )
        .is_err());
        assert_eq!(serde_json::to_value(session).unwrap(), before);
    }
}

#[test]
fn options_roundtrip_excludes_live_registry_and_startup_context_is_fresh() {
    let registry = SharedRegistry::default();
    registry
        .register(Arc::new(InvalidCommand { cancel: false }))
        .unwrap();
    let options = RunOptions {
        extensions: registry.clone(),
        extension_mode: ExtensionMode::Desktop,
        extension_model: Some(ModelIdentity {
            provider: "fixture".into(),
            model: "offline".into(),
        }),
        ..Default::default()
    };
    let json = serde_json::to_value(&options).unwrap();
    assert!(json.get("extensions").is_none());
    let restored: RunOptions = serde_json::from_value(json).unwrap();
    assert!(restored.extensions.ids().is_empty());
    assert_eq!(restored.extension_mode, ExtensionMode::Desktop);
    let mut session = Session::default();
    session
        .record_runtime_state("extension:invalid", json!({"branch":1}))
        .unwrap();
    let context = extension_context(
        &session,
        &registry,
        ExtensionMode::Desktop,
        options.extension_model,
    )
    .unwrap();
    assert_eq!(context.states["invalid"], json!({"branch":1}));
    registry
        .emit_event(
            &LifecycleEvent::SessionStart {
                reason: SessionStartReason::Resume,
                previous_session_file: None,
            },
            &context,
            &CancellationToken::new(),
        )
        .unwrap();
}

#[test]
fn unverified_azure_identity_never_reuses_catalog_usage_as_a_token_quote() {
    let model = ModelBudget {
        provider: "azure-openai-responses".into(),
        id: "arbitrary-deployment".into(),
        context_window: 100_000,
        max_output_tokens: 4000,
        cost: Value::Null,
    };
    let mut session = Session::default();
    session.append_message(Message::user("hello")).unwrap();
    let mut assistant = AssistantMessage::text("short");
    assistant.usage = Some(Usage {
        total_tokens: 90_000,
        ..Default::default()
    });
    assistant.protocol_metadata =
        Some(json!({"provider":model.provider,"model":model.id,"model_identity_verified":false}));
    session
        .append_message(Message::Assistant(assistant))
        .unwrap();
    assert!(estimate_context_tokens_for_model(&session, &session.context, &model) < 100.0);
    if let Message::Assistant(assistant) = session.context.messages.last_mut().unwrap() {
        assistant.protocol_metadata.as_mut().unwrap()["model_identity_verified"] = json!(true);
    }
    assert!(estimate_context_tokens_for_model(&session, &session.context, &model) >= 90_000.0);
}

#[test]
fn overflow_recovery_keeps_custom_messages_and_transforms_the_retry_request_once() {
    struct Overflow {
        requests: Vec<Context>,
        summaries: usize,
    }
    impl Provider for Overflow {
        fn complete(
            &mut self,
            context: &Context,
            _: &CancellationToken,
            _: &mut dyn FnMut(AssistantMessage),
        ) -> Result<AssistantMessage, String> {
            self.requests.push(context.clone());
            if self.requests.len() == 1 {
                Err("HTTP 400 context_length_exceeded".into())
            } else {
                Ok(AssistantMessage::text("done"))
            }
        }
        fn complete_summary(
            &mut self,
            _: &Context,
            _: &CancellationToken,
            _: &mut dyn FnMut(AssistantMessage),
        ) -> Result<AssistantMessage, String> {
            self.summaries += 1;
            Ok(AssistantMessage::text("checkpoint"))
        }
    }
    let registry = SharedRegistry::default();
    registry
        .register(Arc::new(InProcess {
            calls: Arc::new(AtomicUsize::new(0)),
            events: Arc::new(Mutex::new(vec![])),
            block: false,
            fail: false,
        }))
        .unwrap();
    let mut session = Session::default();
    session
        .append_message(Message::user("old history ".repeat(160)))
        .unwrap();
    session
        .append_message(Message::Assistant(AssistantMessage::text("old answer")))
        .unwrap();
    let options = RunOptions {
        extensions: registry,
        automation: AutomationOptions {
            enabled: true,
            model: Some(ModelBudget {
                provider: "fixture".into(),
                id: "offline".into(),
                context_window: 100_000,
                max_output_tokens: 1000,
                cost: Value::Null,
            }),
            reserve_tokens: 1000,
            summary_token_estimate: 100,
            keep_recent_user_turns: 1,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut provider = Overflow {
        requests: vec![],
        summaries: 0,
    };
    run_with_provider(
        &mut session,
        image_prompt(),
        &mut provider,
        &[],
        &options,
        &MessageQueues::default(),
        &CancellationToken::new(),
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(provider.summaries, 1);
    assert_eq!(provider.requests.len(), 2);
    for request in &provider.requests {
        assert_eq!(request.system_prompt, "hook system");
        assert_eq!(request.messages.last().unwrap().text(), "ephemeral view");
        assert_eq!(
            request
                .messages
                .iter()
                .filter(|message| message.text() == "injected")
                .count(),
            1
        );
        assert!(!request
            .messages
            .iter()
            .any(|message| matches!(message, Message::Custom { .. })));
    }
    assert!(provider.requests[1].messages[0]
        .text()
        .contains("checkpoint"));
    assert_eq!(
        session
            .context
            .messages
            .iter()
            .filter(|message| matches!(message, Message::Custom { .. }))
            .count(),
        1
    );
    assert!(!session
        .context
        .messages
        .iter()
        .any(|message| message.text() == "ephemeral view"));
}

#[test]
fn cancelled_real_loop_still_emits_terminal_lifecycle_and_records_aborted_prompt() {
    let observed = Arc::new(Mutex::new(vec![]));
    let registry = SharedRegistry::default();
    registry
        .register(Arc::new(InProcess {
            calls: Arc::new(AtomicUsize::new(0)),
            events: observed.clone(),
            block: false,
            fail: false,
        }))
        .unwrap();
    let signal = CancellationToken::new();
    signal.cancel();
    let mut session = Session::default();
    let options = RunOptions {
        extensions: registry,
        ..Default::default()
    };
    assert!(run_with_provider(
        &mut session,
        image_prompt(),
        &mut Fixed::default(),
        &[],
        &options,
        &MessageQueues::default(),
        &signal,
        &mut |_| {}
    )
    .is_err());
    assert_eq!(session.context.messages.len(), 2);
    assert!(
        matches!(&session.context.messages[1],Message::Assistant(message) if message.stop_reason==StopReason::Aborted)
    );
    let observed = observed.lock().unwrap();
    assert_eq!(observed.len(), 2);
    assert!(matches!(
        observed[0],
        LifecycleEvent::Agent {
            event: Event::AgentEnd { .. }
        }
    ));
    assert!(matches!(observed[1], LifecycleEvent::AgentSettled));
}
