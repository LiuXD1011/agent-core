#[path = "common/native_extension.rs"]
mod fixture;
use agent_core::types::{CancellationToken, Message};
use agent_runtime::extensions::{
    extension_context, load_native_extensions, notify_model_select, record_model_selection,
    stored_model_selection, ExtensionMode, ModelIdentity, ModelSelectSource, SharedRegistry,
    UiBroker,
};
use agent_runtime::session::{Session, SessionStore};
use fixture::{assert_gone, wait_pid, Fixture};
use serde_json::{json, Value};
use std::fs;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

fn identity(provider: &str, model: &str) -> ModelIdentity {
    ModelIdentity {
        provider: provider.into(),
        model: model.into(),
    }
}
fn configure(fixture: &Fixture, id: &str, args: Vec<String>) {
    let mut manifest = fixture.manifest();
    manifest.id = id.into();
    manifest.args = args;
    fs::write(&fixture.manifest, serde_json::to_vec(&manifest).unwrap()).unwrap();
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
fn records(path: &Path) -> Vec<Value> {
    let text = fs::read_to_string(path).unwrap();
    let end = text.rfind('\n').map_or(0, |index| index + 1);
    text[..end]
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}
fn model_events(path: &Path) -> Vec<Value> {
    records(path)
        .into_iter()
        .filter(|record| {
            record["method"] == "lifecycle" && record["params"]["event"]["type"] == "model_select"
        })
        .collect()
}

#[test]
fn model_selection_state_is_identity_only_idempotent_and_restores_along_native_branch_ancestry() {
    let fixture = Fixture::new(&[]);
    let mut session = fixture.session();
    session.metadata.insert(
        "model".into(),
        json!({"provider":"legacy","model":"not-native"}),
    );
    assert_eq!(stored_model_selection(&session).unwrap(), None);
    let first = identity("demo", "");
    record_model_selection(&mut session, &first).unwrap();
    let first_leaf = session.get_leaf_id().unwrap();
    let before = serde_json::to_value(&session).unwrap();
    record_model_selection(&mut session, &first).unwrap();
    assert_eq!(serde_json::to_value(&session).unwrap(), before);
    let user = session.append_message(Message::user("real user")).unwrap();
    let second = identity("openai-compatible", "fixture/模型🙂");
    record_model_selection(&mut session, &second).unwrap();
    assert_eq!(
        session.runtime_state("model-selection").unwrap(),
        Some(json!({"provider":"openai-compatible","model":"fixture/模型🙂"}))
    );
    assert_eq!(stored_model_selection(&session).unwrap(), Some(second));
    assert_eq!(session.context.messages.len(), 1);
    session.navigate(Some(&user)).unwrap();
    assert_eq!(
        stored_model_selection(&session).unwrap(),
        Some(first.clone())
    );
    let other = identity("anthropic", "another-fixture");
    record_model_selection(&mut session, &other).unwrap();
    let other_leaf = session.get_leaf_id().unwrap();
    session.navigate(first_leaf.as_deref()).unwrap();
    assert_eq!(stored_model_selection(&session).unwrap(), Some(first));
    assert!(session.context.messages.is_empty());
    let store = SessionStore::new(fixture.path.join("sessions"));
    store.save(&session).unwrap();
    let mut restored = store.list().unwrap().pop().unwrap();
    assert_eq!(
        serde_json::to_value(&restored).unwrap(),
        serde_json::to_value(&session).unwrap()
    );
    restored.navigate(other_leaf.as_deref()).unwrap();
    assert_eq!(stored_model_selection(&restored).unwrap(), Some(other));
    restored.navigate(None).unwrap();
    assert_eq!(stored_model_selection(&restored).unwrap(), None);
}

#[test]
fn malformed_and_oversized_model_selection_fields_fail_without_mutating_session() {
    let mut session = Session::default();
    for model in [
        identity("", "fixture"),
        identity("a/b", "fixture"),
        identity(&"p".repeat(129), "fixture"),
        identity("fixture", ""),
        identity("fixture", " padded"),
        identity("fixture", "line\nbreak"),
        identity("fixture", &"x".repeat(1025)),
    ] {
        let before = serde_json::to_value(&session).unwrap();
        assert!(record_model_selection(&mut session, &model).is_err());
        assert_eq!(serde_json::to_value(&session).unwrap(), before);
    }
    for value in [
        Value::Null,
        json!({"provider":"fixture","model":7}),
        json!({"provider":"fixture","model":"id","apiKey":"synthetic-unaccepted"}),
        json!({"provider":"fixture"}),
        json!({"provider":"demo","model":"\u{0000}"}),
    ] {
        let mut malformed = Session::default();
        malformed
            .record_runtime_state("model-selection", value)
            .unwrap();
        let before = serde_json::to_value(&malformed).unwrap();
        assert!(stored_model_selection(&malformed).is_err());
        assert_eq!(serde_json::to_value(&malformed).unwrap(), before);
    }
    record_model_selection(&mut session, &identity(&"p".repeat(128), &"x".repeat(1024))).unwrap();
    assert!(stored_model_selection(&session).unwrap().is_some());
}

#[test]
fn native_set_and_restore_use_existing_lifecycle_frames_fresh_snapshots_and_no_fake_messages() {
    let fixture = Fixture::new(&[]);
    let record = fixture.path.join("requests");
    configure(
        &fixture,
        "fixture",
        vec![format!("--record={}", record.display())],
    );
    let mut session = fixture.session();
    let registry = load(&fixture, &session);
    let host_ui = UiBroker::new();
    registry.bind_ui(host_ui.handle()).unwrap();
    session
        .append_message(Message::user("latest actual user"))
        .unwrap();
    session
        .record_runtime_state("extension:fixture", json!({"currentBranch":true}))
        .unwrap();
    let previous = identity("demo", "");
    let model = identity("openai-compatible", "fixture/模型🙂");
    record_model_selection(&mut session, &model).unwrap();
    let committed = serde_json::to_value(&session).unwrap();
    notify_model_select(
        &session,
        &registry,
        ExtensionMode::Desktop,
        model.clone(),
        Some(previous.clone()),
        ModelSelectSource::Set,
        &CancellationToken::new(),
    )
    .unwrap();
    notify_model_select(
        &session,
        &registry,
        ExtensionMode::Desktop,
        model.clone(),
        Some(model.clone()),
        ModelSelectSource::Set,
        &CancellationToken::new(),
    )
    .unwrap();
    notify_model_select(
        &session,
        &registry,
        ExtensionMode::Desktop,
        model.clone(),
        None,
        ModelSelectSource::Restore,
        &CancellationToken::new(),
    )
    .unwrap();
    let observed = model_events(&record);
    assert_eq!(observed.len(), 2);
    assert_eq!(
        observed[0]["params"]["event"],
        json!({"type":"model_select","model":model,"previousModel":previous,"source":"set"})
    );
    assert_eq!(observed[1]["params"]["event"]["source"], "restore");
    assert_eq!(observed[1]["params"]["event"]["previousModel"], Value::Null);
    for event in &observed {
        assert_eq!(event["protocol"], "agent-core-extension");
        assert_eq!(event["version"], 1);
        assert_eq!(
            event["params"]["context"]["model"],
            serde_json::to_value(&model).unwrap()
        );
        assert_eq!(event["params"]["context"]["mode"], "desktop");
        assert_eq!(event["params"]["context"]["hasUi"], true);
        assert_eq!(
            event["params"]["context"]["states"]["fixture"],
            json!({"currentBranch":true})
        );
        assert_eq!(
            event["params"]["context"]["context"]["messages"][0]["content"][0]["text"],
            "latest actual user"
        );
    }
    assert_eq!(serde_json::to_value(&session).unwrap(), committed);
    assert!(registry.drain_diagnostics().is_empty());
}

#[test]
fn ordinary_native_model_select_errors_continue_the_chain_and_keep_the_committed_identity() {
    let first = Fixture::new(&["--model-select-error"]);
    let second = Fixture::new(&[]);
    let first_record = first.path.join("requests");
    let second_record = second.path.join("requests");
    configure(
        &first,
        "first",
        vec![
            "--model-select-error".into(),
            format!("--record={}", first_record.display()),
        ],
    );
    configure(
        &second,
        "second",
        vec![
            "--tool=second_tool".into(),
            format!("--record={}", second_record.display()),
        ],
    );
    let mut session = first.session();
    let empty = SharedRegistry::default();
    let context = extension_context(&session, &empty, ExtensionMode::Rpc, None).unwrap();
    let registry = load_native_extensions(
        &[first.manifest.clone(), second.manifest.clone()],
        &context,
        &CancellationToken::new(),
    )
    .unwrap();
    let model = identity("fixture", "committed");
    record_model_selection(&mut session, &model).unwrap();
    let committed = serde_json::to_value(&session).unwrap();
    notify_model_select(
        &session,
        &registry,
        ExtensionMode::Rpc,
        model.clone(),
        None,
        ModelSelectSource::Restore,
        &CancellationToken::new(),
    )
    .unwrap();
    assert_eq!(model_events(&first_record).len(), 1);
    assert_eq!(model_events(&second_record).len(), 1);
    let diagnostics = registry.drain_diagnostics();
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0].extension_id, "first");
    assert_eq!(diagnostics[0].event, "model_select");
    assert!(diagnostics[0].error.contains("model_select fixture error"));
    assert_eq!(stored_model_selection(&session).unwrap(), Some(model));
    assert_eq!(serde_json::to_value(&session).unwrap(), committed);
    let fresh = extension_context(&session, &registry, ExtensionMode::Rpc, None).unwrap();
    assert!(registry
        .dispatch_command(
            "fixture_echo:1",
            "still alive",
            &fresh,
            &CancellationToken::new()
        )
        .is_ok());
}

#[test]
fn cancelling_native_model_notification_kills_its_entire_job_but_does_not_undo_the_commit() {
    let fixture = Fixture::new(&[]);
    let record = fixture.path.join("requests");
    let parent_pid = fixture.path.join("parent-pid");
    let child_pid = fixture.path.join("child-pid");
    configure(
        &fixture,
        "fixture",
        vec![
            "--hang-model-select".into(),
            format!("--record={}", record.display()),
            format!("--pid={}", parent_pid.display()),
            format!("--child={}", child_pid.display()),
        ],
    );
    let mut session = fixture.session();
    let registry = load(&fixture, &session);
    let parent = wait_pid(&parent_pid);
    let child = wait_pid(&child_pid);
    let model = identity("fixture", "committed");
    record_model_selection(&mut session, &model).unwrap();
    let committed = serde_json::to_value(&session).unwrap();
    let snapshot = session.clone();
    let signal = CancellationToken::new();
    let worker_signal = signal.clone();
    let worker_model = model.clone();
    let worker = thread::spawn(move || {
        notify_model_select(
            &snapshot,
            &registry,
            ExtensionMode::Rpc,
            worker_model,
            None,
            ModelSelectSource::Set,
            &worker_signal,
        )
    });
    let deadline = Instant::now() + Duration::from_secs(3);
    while model_events(&record).is_empty() {
        assert!(
            Instant::now() < deadline,
            "Model notification did not reach the process"
        );
        thread::sleep(Duration::from_millis(2));
    }
    let start = Instant::now();
    signal.cancel();
    assert!(worker.join().unwrap().unwrap_err().contains("aborted"));
    assert!(start.elapsed() < Duration::from_secs(2));
    assert_gone(parent);
    assert_gone(child);
    assert_eq!(stored_model_selection(&session).unwrap(), Some(model));
    assert_eq!(serde_json::to_value(&session).unwrap(), committed);
}

#[test]
fn already_cancelled_native_notification_sends_no_lifecycle_frame_or_transcript_message() {
    let fixture = Fixture::new(&[]);
    let record = fixture.path.join("requests");
    configure(
        &fixture,
        "fixture",
        vec![format!("--record={}", record.display())],
    );
    let session = fixture.session();
    let registry = load(&fixture, &session);
    let signal = CancellationToken::new();
    signal.cancel();
    assert!(notify_model_select(
        &session,
        &registry,
        ExtensionMode::Rpc,
        identity("demo", ""),
        None,
        ModelSelectSource::Restore,
        &signal
    )
    .is_err());
    assert!(model_events(&record).is_empty());
    assert!(session.context.messages.is_empty());
    assert_eq!(stored_model_selection(&session).unwrap(), None);
}
