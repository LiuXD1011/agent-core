use agent_core::tools::AgentTool;
use agent_core::types::{CancellationToken, ToolResult};
use agent_extensions::*;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

fn wait(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !condition() {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(2));
    }
}
fn pending(broker: &UiBroker) -> UiDialogRequest {
    wait(|| !broker.pending_ids().is_empty());
    let requests = broker.drain_requests();
    assert_eq!(requests.len(), 1);
    requests.into_iter().next().unwrap()
}
#[test]
fn select_really_waits_for_a_host_reply_and_rejects_foreign_values() {
    let broker = UiBroker::new();
    let ui = broker.handle();
    let worker = thread::spawn(move || {
        ui.select(
            "Pick",
            &["A".into(), "B".into()],
            None,
            &CancellationToken::new(),
        )
    });
    let request = pending(&broker);
    assert!(!worker.is_finished());
    assert!(broker
        .respond(
            &request.id,
            UiReply::Value {
                value: "foreign".into()
            }
        )
        .is_err());
    assert!(broker.is_pending(&request.id));
    broker
        .respond(&request.id, UiReply::Value { value: "B".into() })
        .unwrap();
    assert_eq!(worker.join().unwrap().unwrap().as_deref(), Some("B"));
    assert!(broker
        .respond(&request.id, UiReply::Value { value: "A".into() })
        .is_err());
    assert!(broker.pending_ids().is_empty());
}
#[test]
fn confirm_reply_is_typed_and_false_is_a_real_answer() {
    let broker = UiBroker::new();
    let ui = broker.handle();
    let worker =
        thread::spawn(move || ui.confirm("Confirm", "Message", None, &CancellationToken::new()));
    let request = pending(&broker);
    assert!(broker
        .respond(
            &request.id,
            UiReply::Value {
                value: "true".into()
            }
        )
        .is_err());
    assert!(broker
        .respond(&request.id, UiReply::Cancelled { cancelled: false })
        .is_err());
    broker
        .respond(&request.id, UiReply::Confirmed { confirmed: false })
        .unwrap();
    assert!(!worker.join().unwrap().unwrap());
}
#[test]
fn input_and_multiline_editor_preserve_values_without_messages_or_side_effects() {
    for editor in [false, true] {
        let broker = UiBroker::new();
        let ui = broker.handle();
        let worker = thread::spawn(move || {
            if editor {
                ui.editor("Edit", Some("old\ntext"), None, &CancellationToken::new())
            } else {
                ui.input(
                    "Input",
                    Some("placeholder"),
                    None,
                    &CancellationToken::new(),
                )
            }
        });
        let request = pending(&broker);
        broker
            .respond(
                &request.id,
                UiReply::Value {
                    value: "changed\nline".into(),
                },
            )
            .unwrap();
        assert_eq!(
            worker.join().unwrap().unwrap().as_deref(),
            Some("changed\nline")
        );
        assert!(broker.pending_ids().is_empty());
    }
}
#[test]
fn cancellation_and_timeout_dismiss_and_reject_late_reply() {
    for cancel in [false, true] {
        let broker = UiBroker::new();
        let ui = broker.handle();
        let signal = CancellationToken::new();
        let worker_signal = signal.clone();
        let worker = thread::spawn(move || {
            ui.input("Bounded", None, (!cancel).then_some(100), &worker_signal)
        });
        let request = pending(&broker);
        let start = Instant::now();
        if cancel {
            signal.cancel();
        }
        assert_eq!(worker.join().unwrap().unwrap(), None);
        assert!(start.elapsed() < Duration::from_secs(2));
        assert!(broker
            .respond(
                &request.id,
                UiReply::Value {
                    value: "late".into()
                }
            )
            .is_err());
        assert!(broker.pending_ids().is_empty());
    }
}
#[test]
fn closing_host_dismisses_pending_and_no_sender_or_request_survives() {
    let broker = UiBroker::new();
    let ui = broker.handle();
    let worker =
        thread::spawn(move || ui.confirm("Close", "No answer", None, &CancellationToken::new()));
    let request = pending(&broker);
    broker.close();
    assert!(!worker.join().unwrap().unwrap());
    assert!(broker.drain_requests().is_empty());
    assert!(broker.pending_ids().is_empty());
    assert!(broker
        .respond(&request.id, UiReply::Confirmed { confirmed: true })
        .is_err());
    assert!(!broker.handle().is_enabled());
    assert_eq!(
        broker
            .handle()
            .input("closed", None, None, &CancellationToken::new())
            .unwrap(),
        None
    );
}
#[test]
fn noninteractive_handle_and_zero_timeout_do_not_publish_dialogs() {
    let broker = UiBroker::default();
    assert_eq!(
        broker
            .handle()
            .select("Pick", &["A".into()], None, &CancellationToken::new())
            .unwrap(),
        None
    );
    assert!(!broker
        .handle()
        .confirm("Confirm", "x", None, &CancellationToken::new())
        .unwrap());
    assert!(broker.pending_ids().is_empty());
    let enabled = UiBroker::new();
    assert_eq!(
        enabled
            .handle()
            .editor("Edit", None, Some(0), &CancellationToken::new())
            .unwrap(),
        None
    );
    assert!(enabled.drain_requests().is_empty());
}
#[test]
fn pending_queue_is_bounded_and_oversize_reply_does_not_consume_a_request() {
    let broker = UiBroker::new();
    let mut workers = vec![];
    for _ in 0..32 {
        let ui = broker.handle();
        workers.push(thread::spawn(move || {
            ui.input("Queue", None, None, &CancellationToken::new())
        }));
    }
    wait(|| broker.pending_ids().len() == 32);
    assert!(broker
        .handle()
        .input("overflow", None, None, &CancellationToken::new())
        .unwrap_err()
        .contains("32"));
    let id = broker.pending_ids().remove(0);
    assert!(broker
        .respond(
            &id,
            UiReply::Value {
                value: "x".repeat(1_048_577)
            }
        )
        .is_err());
    assert!(broker.is_pending(&id));
    broker.close();
    for worker in workers {
        assert_eq!(worker.join().unwrap().unwrap(), None);
    }
    assert!(broker.pending_ids().is_empty());
}
#[test]
fn dialog_wire_shapes_match_rpc_methods_and_reject_unknown_fields() {
    let request = UiDialogRequest {
        id: "owned".into(),
        dialog: UiDialog::Input {
            title: "Title".into(),
            placeholder: Some("hint".into()),
            timeout: Some(25),
        },
    };
    let value = serde_json::to_value(&request).unwrap();
    assert_eq!(
        value,
        json!({"id":"owned","method":"input","title":"Title","placeholder":"hint","timeout":25})
    );
    assert_eq!(
        serde_json::from_value::<UiDialogRequest>(value).unwrap(),
        request
    );
    for invalid in [
        json!({"id":"owned","method":"input","title":"Title","secret":1}),
        json!({"id":1,"method":"input","title":"Title"}),
        json!({"method":"input","title":"Title"}),
    ] {
        assert!(serde_json::from_value::<UiDialogRequest>(invalid).is_err());
    }
    assert!(serde_json::from_value::<UiDialog>(
        json!({"method":"input","title":"Title","secret":1})
    )
    .is_err());
    assert!(
        serde_json::from_value::<UiReply>(json!({"confirmed":true,"value":"conflicting"})).is_err()
    );
}
struct Capture {
    id: &'static str,
    ui: Mutex<Option<UiHandle>>,
    fail: bool,
}
impl Extension for Capture {
    fn id(&self) -> &str {
        self.id
    }
    fn bind_ui(&self, ui: UiHandle) -> Result<(), String> {
        *self.ui.lock().unwrap() = Some(ui);
        if self.fail {
            Err("binding failed".into())
        } else {
            Ok(())
        }
    }
    fn on_event(
        &self,
        _: &LifecycleEvent,
        context: &ExtensionContext,
        _: &CancellationToken,
    ) -> Result<(), String> {
        *self.ui.lock().unwrap() = Some(context.ui.clone());
        Ok(())
    }
}
#[test]
fn unregister_revokes_captured_dialog_scope_and_leaves_other_extensions_enabled() {
    let broker = UiBroker::new();
    let registry = SharedRegistry::default();
    registry.bind_ui(broker.handle()).unwrap();
    let first = Arc::new(Capture {
        id: "first",
        ui: Mutex::new(None),
        fail: false,
    });
    let second = Arc::new(Capture {
        id: "second",
        ui: Mutex::new(None),
        fail: false,
    });
    registry.register(first.clone()).unwrap();
    registry.register(second.clone()).unwrap();
    registry
        .emit_event(
            &LifecycleEvent::AgentSettled,
            &ExtensionContext {
                has_ui: true,
                ..Default::default()
            },
            &CancellationToken::new(),
        )
        .unwrap();
    let ui = first.ui.lock().unwrap().clone().unwrap();
    let captured = ui.clone();
    let worker = thread::spawn(move || ui.input("Old", None, None, &CancellationToken::new()));
    let request = pending(&broker);
    registry.unregister("first").unwrap();
    assert_eq!(worker.join().unwrap().unwrap(), None);
    assert!(captured
        .input("Stale", None, None, &CancellationToken::new())
        .unwrap_err()
        .contains("stale"));
    assert!(broker
        .respond(
            &request.id,
            UiReply::Value {
                value: "late".into()
            }
        )
        .is_err());
    assert!(second.ui.lock().unwrap().as_ref().unwrap().is_enabled());
    assert!(broker.is_enabled());
}
#[test]
fn failed_registration_does_not_activate_its_captured_ui_scope() {
    let broker = UiBroker::new();
    let registry = SharedRegistry::default();
    registry.bind_ui(broker.handle()).unwrap();
    let extension = Arc::new(Capture {
        id: "failed",
        ui: Mutex::new(None),
        fail: true,
    });
    assert!(registry.register(extension.clone()).is_err());
    assert!(registry.ids().is_empty());
    assert!(!extension.ui.lock().unwrap().as_ref().unwrap().is_enabled());
    assert!(broker.pending_ids().is_empty());
}
#[test]
fn duplicate_registration_does_not_rebind_or_revoke_the_live_extension() {
    let broker = UiBroker::new();
    let registry = SharedRegistry::default();
    registry.bind_ui(broker.handle()).unwrap();
    let extension = Arc::new(Capture {
        id: "live",
        ui: Mutex::new(None),
        fail: false,
    });
    registry.register(extension.clone()).unwrap();
    assert!(extension.ui.lock().unwrap().as_ref().unwrap().is_enabled());
    assert!(registry.register(extension.clone()).is_err());
    assert!(extension.ui.lock().unwrap().as_ref().unwrap().is_enabled());
    assert_eq!(registry.ids(), vec!["live"]);
}

struct ShutdownCapture {
    id: &'static str,
    failure: &'static str,
    ui: Mutex<Option<UiHandle>>,
    cleaned: AtomicBool,
    cleanup_ui_enabled: AtomicBool,
}
impl ShutdownCapture {
    fn new(id: &'static str, failure: &'static str) -> Arc<Self> {
        Arc::new(Self {
            id,
            failure,
            ui: Mutex::new(None),
            cleaned: AtomicBool::new(false),
            cleanup_ui_enabled: AtomicBool::new(false),
        })
    }
    fn captured_ui(&self) -> UiHandle {
        self.ui.lock().unwrap().clone().unwrap()
    }
}
impl Extension for ShutdownCapture {
    fn id(&self) -> &str {
        self.id
    }
    fn bind_ui(&self, ui: UiHandle) -> Result<(), String> {
        *self.ui.lock().unwrap() = Some(ui);
        Ok(())
    }
    fn on_event(
        &self,
        event: &LifecycleEvent,
        context: &ExtensionContext,
        _: &CancellationToken,
    ) -> Result<(), String> {
        if matches!(event, LifecycleEvent::SessionShutdown { .. }) {
            self.cleaned.store(true, Ordering::Release);
            self.cleanup_ui_enabled
                .store(context.ui.is_enabled(), Ordering::Release);
            match self.failure {
                "error" => return Err("cleanup failed".into()),
                "panic" => panic!("cleanup panicked"),
                _ => {}
            }
        }
        Ok(())
    }
}
fn shutdown() -> LifecycleEvent {
    LifecycleEvent::SessionShutdown {
        reason: SessionShutdownReason::Reload,
        target_session_file: None,
    }
}
fn interactive_context() -> ExtensionContext {
    ExtensionContext {
        has_ui: true,
        ..Default::default()
    }
}
#[test]
fn shutdown_revokes_old_scopes_after_cleanup_without_closing_the_shared_host() {
    let broker = UiBroker::new();
    let old = SharedRegistry::default();
    old.bind_ui(broker.handle()).unwrap();
    let first = ShutdownCapture::new("old", "error");
    let second = ShutdownCapture::new("sibling", "");
    old.register(first.clone()).unwrap();
    old.register(second.clone()).unwrap();
    let captured = first.captured_ui();
    old.emit_event(
        &LifecycleEvent::AgentSettled,
        &interactive_context(),
        &CancellationToken::new(),
    )
    .unwrap();
    assert!(captured.is_enabled());
    old.emit_event(
        &shutdown(),
        &interactive_context(),
        &CancellationToken::new(),
    )
    .unwrap();
    for extension in [first, second] {
        assert!(extension.cleaned.load(Ordering::Acquire));
        assert!(extension.cleanup_ui_enabled.load(Ordering::Acquire));
        assert!(!extension.captured_ui().is_enabled());
    }
    assert!(captured
        .input("Old background", None, None, &CancellationToken::new())
        .unwrap_err()
        .contains("stale"));
    assert!(broker.is_enabled());
    let replacement = SharedRegistry::default();
    replacement.bind_ui(broker.handle()).unwrap();
    let fresh = ShutdownCapture::new("new", "");
    replacement.register(fresh.clone()).unwrap();
    assert!(fresh.captured_ui().is_enabled());
    assert!(broker.pending_ids().is_empty());
}
#[test]
fn checked_shutdown_failure_and_panic_revoke_the_entire_original_snapshot() {
    for failure in ["error", "panic"] {
        let broker = UiBroker::new();
        let registry = SharedRegistry::default();
        registry.bind_ui(broker.handle()).unwrap();
        let first = ShutdownCapture::new("first", failure);
        let later = ShutdownCapture::new("later", "");
        registry.register(first.clone()).unwrap();
        registry.register(later.clone()).unwrap();
        assert!(registry
            .emit_event_checked(
                &shutdown(),
                &interactive_context(),
                &CancellationToken::new()
            )
            .is_err());
        assert!(first.cleaned.load(Ordering::Acquire));
        assert!(first.cleanup_ui_enabled.load(Ordering::Acquire));
        assert!(!later.cleaned.load(Ordering::Acquire));
        assert!(!first.captured_ui().is_enabled());
        assert!(!later.captured_ui().is_enabled());
        assert!(broker.is_enabled());
        assert_eq!(registry.drain_diagnostics().len(), 1);
    }
}
#[test]
fn shutdown_cancels_a_captured_pending_dialog_and_rejects_its_late_reply() {
    let broker = UiBroker::new();
    let registry = SharedRegistry::default();
    registry.bind_ui(broker.handle()).unwrap();
    let extension = ShutdownCapture::new("old", "");
    registry.register(extension.clone()).unwrap();
    let ui = extension.captured_ui();
    let worker = thread::spawn(move || {
        ui.input("Background pending", None, None, &CancellationToken::new())
    });
    let request = pending(&broker);
    registry
        .emit_event(
            &shutdown(),
            &interactive_context(),
            &CancellationToken::new(),
        )
        .unwrap();
    assert_eq!(worker.join().unwrap().unwrap(), None);
    assert!(broker.pending_ids().is_empty());
    assert!(broker
        .respond(
            &request.id,
            UiReply::Value {
                value: "late".into()
            }
        )
        .is_err());
    assert!(broker.is_enabled());
}

struct ReusableTool(Arc<AtomicUsize>);
impl AgentTool for ReusableTool {
    fn name(&self) -> &str {
        "reusable_tool"
    }
    fn execute(
        &self,
        _: &str,
        _: &Value,
        _: &CancellationToken,
        _: &mut dyn FnMut(ToolResult),
    ) -> Result<ToolResult, String> {
        self.0.fetch_add(1, Ordering::AcqRel);
        Ok(ToolResult::text("still registered"))
    }
}
struct Reusable {
    ui: Mutex<Option<UiHandle>>,
    started: Mutex<Vec<UiHandle>>,
    fail_start: AtomicBool,
    settled: AtomicUsize,
    tool_calls: Arc<AtomicUsize>,
}
impl Reusable {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            ui: Mutex::new(None),
            started: Mutex::new(Vec::new()),
            fail_start: AtomicBool::new(false),
            settled: AtomicUsize::new(0),
            tool_calls: Arc::new(AtomicUsize::new(0)),
        })
    }
    fn bound(&self) -> UiHandle {
        self.ui.lock().unwrap().clone().unwrap()
    }
}
impl Extension for Reusable {
    fn id(&self) -> &str {
        "reusable"
    }
    fn tools(&self) -> Vec<Arc<dyn AgentTool>> {
        vec![Arc::new(ReusableTool(self.tool_calls.clone()))]
    }
    fn bind_ui(&self, ui: UiHandle) -> Result<(), String> {
        *self.ui.lock().unwrap() = Some(ui);
        Ok(())
    }
    fn on_event(
        &self,
        event: &LifecycleEvent,
        context: &ExtensionContext,
        _: &CancellationToken,
    ) -> Result<(), String> {
        match event {
            LifecycleEvent::SessionStart { .. } => {
                self.started.lock().unwrap().push(context.ui.clone());
                if self.fail_start.load(Ordering::Acquire) {
                    return Err("candidate startup failed".into());
                }
            }
            LifecycleEvent::AgentSettled => {
                assert!(context.ui.is_enabled());
                self.settled.fetch_add(1, Ordering::AcqRel);
            }
            _ => {}
        }
        Ok(())
    }
}
fn session_context(id: &str) -> ExtensionContext {
    ExtensionContext {
        session_id: Some(id.into()),
        has_ui: true,
        ..Default::default()
    }
}
fn start(reason: SessionStartReason) -> LifecycleEvent {
    LifecycleEvent::SessionStart {
        reason,
        previous_session_file: None,
    }
}
#[test]
fn shared_registry_candidate_start_then_old_shutdown_preserves_new_ui_tools_and_lifecycle() {
    for same_session in [false, true] {
        let broker = UiBroker::new();
        let registry = SharedRegistry::default();
        registry.bind_ui(broker.handle()).unwrap();
        let extension = Reusable::new();
        registry.register(extension.clone()).unwrap();
        let tools = registry.tools();
        let original = session_context("original");
        registry
            .emit_event_checked(
                &start(SessionStartReason::Startup),
                &original,
                &CancellationToken::new(),
            )
            .unwrap();
        let old = extension.started.lock().unwrap()[0].clone();
        let old_pending = old.clone();
        let worker = thread::spawn(move || {
            old_pending.input("Old pending", None, None, &CancellationToken::new())
        });
        let request = pending(&broker);
        let candidate = session_context(if same_session { "original" } else { "new" });
        registry
            .emit_event_checked(
                &start(if same_session {
                    SessionStartReason::Reload
                } else {
                    SessionStartReason::New
                }),
                &candidate,
                &CancellationToken::new(),
            )
            .unwrap();
        let fresh = extension.started.lock().unwrap()[1].clone();
        assert!(old.is_enabled());
        assert!(fresh.is_enabled());
        registry
            .emit_event(&shutdown(), &original, &CancellationToken::new())
            .unwrap();
        assert_eq!(worker.join().unwrap().unwrap(), None);
        assert!(!old.is_enabled());
        assert!(fresh.is_enabled());
        assert!(extension.bound().is_enabled());
        assert!(broker
            .respond(
                &request.id,
                UiReply::Value {
                    value: "late".into()
                }
            )
            .is_err());
        let ui = fresh.clone();
        let worker =
            thread::spawn(move || ui.input("New pending", None, None, &CancellationToken::new()));
        let request = pending(&broker);
        broker
            .respond(
                &request.id,
                UiReply::Value {
                    value: "new reply".into(),
                },
            )
            .unwrap();
        assert_eq!(
            worker.join().unwrap().unwrap().as_deref(),
            Some("new reply")
        );
        tools[0]
            .execute("call", &json!({}), &CancellationToken::new(), &mut |_| {})
            .unwrap();
        assert_eq!(extension.tool_calls.load(Ordering::Acquire), 1);
        registry
            .emit_event(
                &LifecycleEvent::AgentSettled,
                &candidate,
                &CancellationToken::new(),
            )
            .unwrap();
        assert_eq!(extension.settled.load(Ordering::Acquire), 1);
        registry.unregister("reusable").unwrap();
        assert!(!fresh.is_enabled());
        assert!(tools[0]
            .execute("stale", &json!({}), &CancellationToken::new(), &mut |_| {})
            .is_err());
        assert!(broker.is_enabled());
    }
}
#[test]
fn failed_candidate_start_revokes_candidate_ui_and_restores_the_shared_old_binding() {
    let broker = UiBroker::new();
    let registry = SharedRegistry::default();
    registry.bind_ui(broker.handle()).unwrap();
    let extension = Reusable::new();
    registry.register(extension.clone()).unwrap();
    let original = session_context("old");
    registry
        .emit_event_checked(
            &start(SessionStartReason::Startup),
            &original,
            &CancellationToken::new(),
        )
        .unwrap();
    let old = extension.bound();
    extension.fail_start.store(true, Ordering::Release);
    assert!(registry
        .emit_event_checked(
            &start(SessionStartReason::New),
            &session_context("failed"),
            &CancellationToken::new()
        )
        .is_err());
    assert!(old.is_enabled());
    assert!(extension.bound().is_enabled());
    assert!(!extension.started.lock().unwrap()[1].is_enabled());
    registry
        .emit_event(
            &LifecycleEvent::AgentSettled,
            &original,
            &CancellationToken::new(),
        )
        .unwrap();
    assert_eq!(extension.settled.load(Ordering::Acquire), 1);
    let ui = extension.bound();
    let worker = thread::spawn(move || {
        ui.input("Original continues", None, None, &CancellationToken::new())
    });
    let request = pending(&broker);
    broker
        .respond(
            &request.id,
            UiReply::Value {
                value: "old reply".into(),
            },
        )
        .unwrap();
    assert_eq!(
        worker.join().unwrap().unwrap().as_deref(),
        Some("old reply")
    );
}
