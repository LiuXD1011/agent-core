#[path = "common/native_extension.rs"]
mod fixture;
use agent_core::types::{CancellationToken, Message};
use agent_runtime::extensions::*;
use agent_runtime::legacy_session::{parse_legacy_session, LeafSelection};
use agent_runtime::session::SessionStore;
use fixture::{assert_gone, wait_pid, Fixture};
use serde_json::Value;
use std::fs;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

fn configure(fixture: &Fixture, args: Vec<String>) {
    let mut manifest = fixture.manifest();
    manifest.args = args;
    manifest.request_timeout_ms = 30_000;
    fs::write(&fixture.manifest, serde_json::to_vec(&manifest).unwrap()).unwrap();
}
fn load(fixture: &Fixture) -> SharedRegistry {
    let context = extension_context(
        &fixture.session(),
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
fn switch() -> SessionBeforeEvent {
    SessionBeforeEvent::Switch {
        reason: SessionSwitchReason::New,
        target_session_file: None,
    }
}

#[test]
fn session_info_is_global_not_branch_local_and_empty_name_survives_save_reopen() {
    // Original global session-info and CR/LF regressions #3686 and #5996.
    let fixture = Fixture::new(&[]);
    let mut session = fixture.session();
    let first = session.append_message(Message::user("first")).unwrap();
    session
        .append_session_info("  alpha\r\n\nbeta  gamma  ")
        .unwrap();
    session.navigate(Some(&first)).unwrap();
    assert_eq!(
        session.get_session_name().unwrap().as_deref(),
        Some("alpha beta  gamma")
    );
    session.append_message(Message::user("sibling")).unwrap();
    session.append_session_info(" \n ").unwrap();
    session.navigate(Some(&first)).unwrap();
    assert_eq!(session.get_session_name().unwrap(), None);
    let entries = session.get_entries().unwrap();
    assert_eq!(
        entries
            .iter()
            .filter(|entry| entry.entry_type == "session_info")
            .count(),
        2
    );
    assert_eq!(session.context.messages.len(), 1);
    let store = SessionStore::new(fixture.path.join("sessions"));
    store.save(&session).unwrap();
    let reopened = store.list().unwrap().remove(0);
    assert_eq!(reopened.get_session_name().unwrap(), None);
    assert_eq!(reopened.get_entries().unwrap(), entries);
    assert_eq!(reopened.context.messages, session.context.messages);
    SessionStore::export_branch_jsonl(&reopened, &fixture.path.join("export.jsonl")).unwrap();
    let exported = fs::read_to_string(fixture.path.join("export.jsonl")).unwrap();
    assert!(exported
        .lines()
        .any(|line| serde_json::from_str::<Value>(line)
            .is_ok_and(|v| v["type"] == "session_info" && v["name"] == "")));
    let reimported = parse_legacy_session(&exported, fixture.path.join("export.jsonl"))
        .unwrap()
        .import(LeafSelection::Latest)
        .unwrap();
    assert_eq!(reimported.get_session_name().unwrap(), None);
    assert_eq!(reimported.context.messages, reopened.context.messages);
    assert_eq!(
        reopened
            .clone_current_branch()
            .unwrap()
            .get_session_name()
            .unwrap(),
        None
    );
}

struct NameObserver(Arc<Mutex<Vec<(LifecycleEvent, ExtensionContext)>>>);
impl Extension for NameObserver {
    fn id(&self) -> &str {
        "names"
    }
    fn on_event(
        &self,
        event: &LifecycleEvent,
        context: &ExtensionContext,
        _: &CancellationToken,
    ) -> Result<(), String> {
        self.0
            .lock()
            .unwrap()
            .push((event.clone(), context.clone()));
        Ok(())
    }
}
#[test]
fn info_helper_uses_committed_fresh_name_and_no_llm_message() {
    let fixture = Fixture::new(&[]);
    let registry = SharedRegistry::default();
    let observations = Arc::new(Mutex::new(vec![]));
    registry
        .register(Arc::new(NameObserver(observations.clone())))
        .unwrap();
    let mut session = fixture.session();
    for name in ["selected", "\r\n"] {
        session.append_session_info(name).unwrap();
        notify_session_info_changed(
            &session,
            &registry,
            ExtensionMode::Desktop,
            None,
            &CancellationToken::new(),
        )
        .unwrap();
    }
    let observations = observations.lock().unwrap();
    assert!(
        matches!(&observations[0].0,LifecycleEvent::SessionInfoChanged{name:Some(name)} if name=="selected")
    );
    assert_eq!(observations[0].1.session_name.as_deref(), Some("selected"));
    assert!(matches!(
        &observations[1].0,
        LifecycleEvent::SessionInfoChanged { name: None }
    ));
    assert!(session.context.messages.is_empty());
}

#[test]
fn native_name_command_persists_real_session_info_and_notifies_clear() {
    let fixture = Fixture::new(&[]);
    let record = fixture.path.join("requests.jsonl");
    configure(
        &fixture,
        vec![
            "--command=hello".into(),
            "--session-name= \r\n ".into(),
            format!("--record={}", record.display()),
        ],
    );
    let registry = load(&fixture);
    let mut session = fixture.session();
    let context = extension_context(&session, &registry, ExtensionMode::Rpc, None).unwrap();
    let result = execute_extension_command(
        &mut session,
        &registry,
        "hello",
        "clear",
        &context,
        &CancellationToken::new(),
    )
    .unwrap();
    assert_eq!(result.session_name.as_deref(), Some(""));
    assert_eq!(
        session
            .get_entries()
            .unwrap()
            .iter()
            .filter(|e| e.entry_type == "session_info")
            .count(),
        1
    );
    assert_eq!(session.context.messages, result.messages);
    assert!(!session
        .context
        .messages
        .iter()
        .any(|message| matches!(message, Message::Assistant(_) | Message::ToolResult(_))));
    notify_session_info_changed(
        &session,
        &registry,
        ExtensionMode::Rpc,
        None,
        &CancellationToken::new(),
    )
    .unwrap();
    let records: Vec<Value> = fs::read_to_string(&record)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let info = records
        .iter()
        .find(|v| {
            v["method"] == "lifecycle" && v["params"]["event"]["type"] == "session_info_changed"
        })
        .unwrap();
    assert_eq!(info["params"]["event"]["name"], Value::Null);
    assert_eq!(info["params"]["context"]["sessionName"], Value::Null);
    registry.unregister("fixture").unwrap();
}
#[test]
fn native_before_switch_and_fork_veto_without_mutating_session() {
    for (flag, event) in [
        ("--before-switch-cancel", switch()),
        (
            "--before-fork-cancel",
            SessionBeforeEvent::Fork {
                entry_id: "user-entry".into(),
                position: ForkPosition::Before,
            },
        ),
    ] {
        let fixture = Fixture::new(&[flag]);
        let registry = load(&fixture);
        let session = fixture.session();
        let before = serde_json::to_value(&session).unwrap();
        assert!(!prepare_session_change(
            &session,
            &registry,
            ExtensionMode::Rpc,
            None,
            &event,
            &CancellationToken::new()
        )
        .unwrap());
        assert_eq!(serde_json::to_value(&session).unwrap(), before);
        registry.unregister("fixture").unwrap();
    }
}
#[test]
fn native_ordinary_before_callback_error_is_diagnostic_and_allows_switch() {
    let fixture = Fixture::new(&["--before-switch-error"]);
    let registry = load(&fixture);
    assert!(prepare_session_change(
        &fixture.session(),
        &registry,
        ExtensionMode::Rpc,
        None,
        &switch(),
        &CancellationToken::new()
    )
    .unwrap());
    let diagnostics = registry.drain_diagnostics();
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0].event, "session_before_switch");
    registry.unregister("fixture").unwrap();
}
fn native_cancel(flag: &str, event: Option<SessionBeforeEvent>) {
    let fixture = Fixture::new(&[]);
    let parent = fixture.path.join("parent.pid");
    let child = fixture.path.join("child.pid");
    let record = fixture.path.join("record.jsonl");
    configure(
        &fixture,
        vec![
            flag.into(),
            format!("--pid={}", parent.display()),
            format!("--child={}", child.display()),
            format!("--record={}", record.display()),
        ],
    );
    let registry = load(&fixture);
    let mut session = fixture.session();
    session.append_session_info("committed").unwrap();
    let p = wait_pid(&parent);
    let c = wait_pid(&child);
    let signal = CancellationToken::new();
    let worker_signal = signal.clone();
    let worker_registry = registry.clone();
    let worker = thread::spawn(move || match event {
        Some(event) => prepare_session_change(
            &session,
            &worker_registry,
            ExtensionMode::Rpc,
            None,
            &event,
            &worker_signal,
        )
        .map(|_| ()),
        None => notify_session_info_changed(
            &session,
            &worker_registry,
            ExtensionMode::Rpc,
            None,
            &worker_signal,
        ),
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    while !fs::read_to_string(&record)
        .unwrap_or_default()
        .lines()
        .any(|line| {
            serde_json::from_str::<Value>(line).is_ok_and(|v| {
                v["method"] == "session_before"
                    || v["params"]["event"]["type"] == "session_info_changed"
            })
        })
    {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(5));
    }
    let start = Instant::now();
    signal.cancel();
    assert!(worker.join().unwrap().is_err());
    assert!(start.elapsed() < Duration::from_secs(2));
    assert_gone(p);
    assert_gone(c);
    registry.unregister("fixture").unwrap();
}
#[test]
fn native_hanging_before_switch_cancels_process_tree() {
    native_cancel("--hang-before-switch", Some(switch()));
}
#[test]
fn native_hanging_before_fork_cancels_process_tree() {
    native_cancel(
        "--hang-before-fork",
        Some(SessionBeforeEvent::Fork {
            entry_id: "user".into(),
            position: ForkPosition::Before,
        }),
    );
}
#[test]
fn native_hanging_info_notification_cancels_process_tree() {
    native_cancel("--hang-session-info", None);
}
