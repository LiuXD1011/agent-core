#[path = "common/native_extension.rs"]
mod fixture;

use agent_core::types::CancellationToken;
use agent_runtime::extension_process::NativeProcess;
use agent_runtime::extensions::{
    execute_extension_command, extension_context, load_native_extensions, ExtensionMode,
    SharedRegistry, UiBroker, UiDialog, UiDialogRequest, UiReply,
};
use fixture::{assert_gone, wait_pid, Fixture};
use serde_json::{json, Value};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

fn dialog(broker: &UiBroker) -> UiDialogRequest {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let mut requests = broker.drain_requests();
        if let Some(request) = requests.pop() {
            assert!(requests.is_empty());
            return request;
        }
        assert!(Instant::now() < deadline, "Native dialog was not published");
        thread::sleep(Duration::from_millis(5));
    }
}

fn process(fixture: &Fixture, broker: &UiBroker) -> Arc<NativeProcess> {
    let mut manifest = fixture.manifest();
    manifest.request_timeout_ms = 5000;
    let process =
        Arc::new(NativeProcess::spawn(manifest, &fixture.path, &CancellationToken::new()).unwrap());
    process.bind_ui(broker.handle());
    process
}

fn call_ui(
    process: &NativeProcess,
    signal: &CancellationToken,
    timeout: Option<u64>,
) -> Result<Value, String> {
    let mut options = json!({"title":"真实 input 中文🙂","placeholder":"hint"});
    if let Some(timeout) = timeout {
        options["timeout"] = json!(timeout);
    }
    process.request(
        "ui",
        json!({"dialog":{"method":"input"},"options":options}),
        signal,
        &mut |_| {},
    )
}

#[test]
fn native_command_waits_for_all_four_real_dialog_responses_without_messages() {
    let fixture = Fixture::new(&["--ui-command"]);
    let broker = UiBroker::new();
    let session = fixture.session();
    let mut context = extension_context(
        &session,
        &SharedRegistry::default(),
        ExtensionMode::Rpc,
        None,
    )
    .unwrap();
    context.ui = broker.handle();
    context.has_ui = true;
    let registry = load_native_extensions(
        std::slice::from_ref(&fixture.manifest),
        &context,
        &CancellationToken::new(),
    )
    .unwrap();
    let before = serde_json::to_value(&session).unwrap();
    let worker = thread::spawn(move || {
        let mut session = session;
        let result = execute_extension_command(
            &mut session,
            &registry,
            "fixture_echo",
            "",
            &context,
            &CancellationToken::new(),
        )
        .unwrap();
        (session, result)
    });
    let replies = [
        UiReply::Value {
            value: "蓝🙂".into(),
        },
        UiReply::Confirmed { confirmed: true },
        UiReply::Value {
            value: "typed 中文🙂".into(),
        },
        UiReply::Value {
            value: "edited\nmultiline 🙂".into(),
        },
    ];
    for (index, reply) in replies.iter().enumerate() {
        let request = dialog(&broker);
        assert!(!worker.is_finished(), "command completed before its reply");
        assert!(broker.is_pending(&request.id));
        match (&request.dialog, index) {
            (UiDialog::Select { options, .. }, 0) => assert_eq!(options, &["red", "蓝🙂"]),
            (UiDialog::Confirm { message, .. }, 1) => assert_eq!(message, "Continue?"),
            (UiDialog::Input { placeholder, .. }, 2) => {
                assert_eq!(placeholder.as_deref(), Some("placeholder"))
            }
            (UiDialog::Editor { prefill, .. }, 3) => {
                assert_eq!(prefill.as_deref(), Some("original\ntext"))
            }
            _ => panic!("wrong native dialog order"),
        }
        let invalid = if index < 2 {
            UiReply::Value {
                value: "invalid answer".into(),
            }
        } else {
            UiReply::Confirmed { confirmed: true }
        };
        assert!(broker.respond(&request.id, invalid).is_err());
        broker.respond(&request.id, reply.clone()).unwrap();
        assert!(broker.respond(&request.id, reply.clone()).is_err());
    }
    let (session, result) = worker.join().unwrap();
    assert_eq!(serde_json::to_value(session).unwrap(), before);
    assert!(result.messages.is_empty());
    assert_eq!(
        serde_json::from_str::<Value>(&result.text).unwrap(),
        serde_json::to_value(replies).unwrap()
    );
    assert!(broker.pending_ids().is_empty());
}

#[test]
fn print_dialogs_cancel_immediately_and_broker_rebinding_reaches_existing_process() {
    let fixture = Fixture::new(&[]);
    let disabled = UiBroker::default();
    let process = process(&fixture, &disabled);
    assert_eq!(
        call_ui(&process, &CancellationToken::new(), None).unwrap(),
        json!({"cancelled":true})
    );
    assert!(disabled.drain_requests().is_empty());
    let broker = UiBroker::new();
    process.bind_ui(broker.handle());
    let active = process.clone();
    let worker = thread::spawn(move || call_ui(&active, &CancellationToken::new(), None));
    let request = dialog(&broker);
    broker
        .respond(
            &request.id,
            UiReply::Value {
                value: "rebound live UI".into(),
            },
        )
        .unwrap();
    assert_eq!(
        worker.join().unwrap().unwrap(),
        json!({"value":"rebound live UI"})
    );
}

#[test]
fn short_dialog_timeout_or_host_close_cancels_only_the_dialog_and_keeps_transport() {
    for close in [false, true] {
        let fixture = Fixture::new(&[]);
        let broker = UiBroker::new();
        let process = process(&fixture, &broker);
        let active = process.clone();
        let start = Instant::now();
        let worker = thread::spawn(move || {
            call_ui(
                &active,
                &CancellationToken::new(),
                Some(if close { 5000 } else { 150 }),
            )
        });
        let request = dialog(&broker);
        if close {
            broker.close();
        }
        assert_eq!(worker.join().unwrap().unwrap(), json!({"cancelled":true}));
        assert!(start.elapsed() < Duration::from_secs(2));
        assert!(broker.pending_ids().is_empty());
        assert!(broker
            .respond(
                &request.id,
                UiReply::Value {
                    value: "late".into()
                }
            )
            .is_err());
        assert_eq!(
            process
                .request(
                    "echo",
                    json!({"alive":true}),
                    &CancellationToken::new(),
                    &mut |_| {}
                )
                .unwrap(),
            json!({"alive":true})
        );
    }
}

#[test]
fn cancelling_native_ui_closes_pending_request_and_the_entire_process_tree() {
    let fixture = Fixture::new(&[]);
    let broker = UiBroker::new();
    let pid_file = fixture.path.join("pid");
    let mut manifest = fixture.manifest();
    manifest.args.push(format!("--pid={}", pid_file.display()));
    manifest.request_timeout_ms = 30_000;
    let process =
        Arc::new(NativeProcess::spawn(manifest, &fixture.path, &CancellationToken::new()).unwrap());
    process.bind_ui(broker.handle());
    let pid = wait_pid(&pid_file);
    let signal = CancellationToken::new();
    let worker_signal = signal.clone();
    let active = process.clone();
    let worker = thread::spawn(move || call_ui(&active, &worker_signal, None));
    let request = dialog(&broker);
    let start = Instant::now();
    signal.cancel();
    assert!(worker.join().unwrap().unwrap_err().contains("aborted"));
    assert!(start.elapsed() < Duration::from_secs(2));
    assert!(broker.pending_ids().is_empty());
    assert!(broker
        .respond(
            &request.id,
            UiReply::Value {
                value: "late".into()
            }
        )
        .is_err());
    assert_gone(pid);
}

#[test]
fn native_ui_parent_deadline_and_crash_remove_pending_without_waiting_for_ui() {
    for crash in [false, true] {
        let fixture = Fixture::new(if crash { &["--ui-crash"] } else { &[] });
        let broker = UiBroker::new();
        let mut manifest = fixture.manifest();
        manifest.request_timeout_ms = 200;
        let process =
            NativeProcess::spawn(manifest, &fixture.path, &CancellationToken::new()).unwrap();
        process.bind_ui(broker.handle());
        let start = Instant::now();
        let error = call_ui(&process, &CancellationToken::new(), None).unwrap_err();
        assert!(
            if crash {
                error.contains("exited") || error.contains("stdout")
            } else {
                error.contains("timed out")
            },
            "{error}"
        );
        assert!(start.elapsed() < Duration::from_secs(2));
        assert!(broker.pending_ids().is_empty());
        assert!(broker.drain_requests().is_empty());
    }
}

#[test]
fn wrong_parent_overlap_early_result_and_duplicate_ui_ids_poison_process() {
    for flag in [
        "--ui-bad-parent",
        "--ui-overlap",
        "--ui-result-early",
        "--ui-duplicate",
    ] {
        let fixture = Fixture::new(&[flag]);
        let broker = UiBroker::new();
        let process = process(&fixture, &broker);
        let active = process.clone();
        let worker = thread::spawn(move || call_ui(&active, &CancellationToken::new(), None));
        if flag == "--ui-duplicate" {
            let request = dialog(&broker);
            broker.respond(&request.id, UiReply::cancelled()).unwrap();
        }
        let start = Instant::now();
        let error = worker.join().unwrap().unwrap_err();
        assert!(
            error.contains("mismatch")
                || error.contains("Overlapping")
                || error.contains("before its UI")
                || error.contains("Duplicate"),
            "{flag}: {error}"
        );
        assert!(start.elapsed() < Duration::from_secs(2));
        assert!(broker.pending_ids().is_empty());
        assert!(process
            .request("echo", json!({}), &CancellationToken::new(), &mut |_| {})
            .is_err());
    }
}
