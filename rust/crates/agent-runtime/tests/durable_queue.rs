use agent_core::agent_loop::Provider;
use agent_core::types::{
    AssistantMessage, CancellationToken, Content, Context, Message, UserMessage,
};
use agent_runtime::runtime::{run_with_provider, MessageQueues, QueueError, QueueLane, RunOptions};
use agent_runtime::session::{Session, SessionStore};
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("agent-durable-{}", Session::default().id));
        fs::create_dir(&path).unwrap();
        Self(fs::canonicalize(path).unwrap())
    }
    fn open(&self) -> MessageQueues {
        let queues = MessageQueues::open_durable(&self.0, "session-a").unwrap();
        if queues.snapshot().workspace.is_none()
            && queues.is_empty()
            && queues.snapshot().uncertain.is_empty()
        {
            queues.bind_workspace(&self.0).unwrap();
        }
        queues
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn image(text: &str) -> Message {
    Message::User(UserMessage {
        content: vec![
            Content::text(text),
            Content::Image {
                data: "AQID".into(),
                mime_type: "image/png".into(),
            },
        ],
        timestamp: 123,
    })
}
#[derive(Default)]
struct Fixture(usize);
impl Provider for Fixture {
    fn complete(
        &mut self,
        _: &Context,
        _: &CancellationToken,
        _: &mut dyn FnMut(AssistantMessage),
    ) -> Result<AssistantMessage, String> {
        self.0 += 1;
        Ok(AssistantMessage::text("local answer"))
    }
}
#[test]
fn pending_round_trip_preserves_raw_images_order_and_ids_without_auto_dispatch() {
    let dir = Directory::new();
    let queues = dir.open();
    let first = queues
        .try_enqueue(
            image("prepared A"),
            QueueLane::Steering,
            Some(image("raw A")),
        )
        .unwrap();
    let second = queues
        .try_enqueue(Message::user("B"), QueueLane::Steering, None)
        .unwrap();
    queues.reorder(&[second, first, second]).unwrap();
    queues.try_set_paused(true).unwrap();
    drop(queues);
    let queues = dir.open();
    let state = queues.snapshot();
    assert!(state.recovery_required && state.paused);
    assert_eq!(state.items[0].id, second);
    assert_eq!(state.items[1].id, first);
    assert_eq!(queues.raw_input(first).unwrap(), image("raw A"));
    assert!(queues.try_take_next().unwrap().is_none());
    assert_eq!(
        queues.try_set_paused(false),
        Err(QueueError::RecoveryRequired)
    );
    queues.resume_recovered().unwrap();
    assert_eq!(queues.try_take_next().unwrap().unwrap().id, second);
    let third = queues
        .try_enqueue(Message::user("C"), QueueLane::Steering, None)
        .unwrap();
    assert!(third.0 > second.0);
}
#[test]
fn immediate_inflight_is_quarantined_and_only_explicit_retry_releases_it() {
    let dir = Directory::new();
    let queues = dir.open();
    let id = queues
        .try_enqueue(image("request"), QueueLane::Immediate, Some(image("raw")))
        .unwrap();
    let item = queues.try_take_next().unwrap().unwrap();
    assert_eq!(item.id, id);
    drop(queues);
    let queues = dir.open();
    assert_eq!(queues.snapshot().uncertain[0].id, id);
    assert!(queues.try_take_next().unwrap().is_none());
    queues.resume_recovered().unwrap();
    assert!(queues.try_take_next().unwrap().is_none());
    queues.retry_uncertain(id).unwrap();
    queues.try_set_paused(false).unwrap();
    assert_eq!(
        queues.try_take_next().unwrap().unwrap().message,
        image("request")
    );
    assert_eq!(queues.raw_input(id).unwrap(), image("raw"));
}
#[test]
fn failed_reservation_write_keeps_pending_and_halts_without_receipt() {
    let dir = Directory::new();
    let queues = dir.open();
    let id = queues
        .try_enqueue(Message::user("retained"), QueueLane::Immediate, None)
        .unwrap();
    let path = dir.0.join("session-a.json");
    fs::remove_file(&path).unwrap();
    fs::create_dir(&path).unwrap();
    assert!(queues.try_take_next().is_err());
    let state = queues.snapshot();
    assert_eq!(state.items[0].id, id);
    assert!(state.uncertain.is_empty() && state.halted && state.error.is_some());
    assert!(queues
        .try_enqueue(Message::user("not committed"), QueueLane::Steering, None)
        .is_err());
}
#[test]
fn external_revision_change_is_rejected_without_overwriting_file() {
    let dir = Directory::new();
    let queues = dir.open();
    queues
        .try_enqueue(Message::user("first"), QueueLane::Steering, None)
        .unwrap();
    let path = dir.0.join("session-a.json");
    let mut changed: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    changed["revision"] = 100.into();
    let bytes = serde_json::to_vec(&changed).unwrap();
    fs::write(&path, &bytes).unwrap();
    assert_eq!(
        queues.try_enqueue(Message::user("second"), QueueLane::Steering, None),
        Err(QueueError::StoreConflict)
    );
    assert_eq!(fs::read(&path).unwrap(), bytes);
    assert_eq!(queues.len(), 1);
}
#[test]
fn store_lock_blocks_second_owner_and_releases_on_drop() {
    let dir = Directory::new();
    let first = dir.open();
    assert!(matches!(
        MessageQueues::open_durable(&dir.0, "session-a"),
        Err(QueueError::StoreBusy)
    ));
    drop(first);
    let second = dir.open();
    assert!(second.is_empty());
}
#[test]
fn corrupt_future_and_wrong_identity_are_preserved() {
    let dir = Directory::new();
    let path = dir.0.join("session-a.json");
    for bytes in [b"not json".to_vec(), br#"{"schema":999}"#.to_vec()] {
        fs::write(&path, &bytes).unwrap();
        assert!(matches!(
            MessageQueues::open_durable(&dir.0, "session-a"),
            Err(QueueError::InvalidStore)
        ));
        assert_eq!(fs::read(&path).unwrap(), bytes);
    }
    fs::remove_file(&path).unwrap();
    let queues = dir.open();
    queues
        .try_enqueue(Message::user("x"), QueueLane::Steering, None)
        .unwrap();
    drop(queues);
    let mut data: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    data["session_id"] = "different".into();
    let bytes = serde_json::to_vec(&data).unwrap();
    fs::write(&path, &bytes).unwrap();
    assert!(matches!(
        MessageQueues::open_durable(&dir.0, "session-a"),
        Err(QueueError::InvalidStore)
    ));
    assert_eq!(fs::read(path).unwrap(), bytes);
}
#[test]
fn initial_admission_and_busy_admission_require_explicit_saved_checkpoint() {
    let dir = Directory::new();
    let queues = dir.open();
    queues.set_track_admissions(true);
    let initial = queues
        .try_enqueue(
            Message::user("initial"),
            QueueLane::Steering,
            Some(Message::user("raw initial")),
        )
        .unwrap();
    let item = queues.try_take_next().unwrap().unwrap();
    let follow = queues
        .try_enqueue(Message::user("follow"), QueueLane::Steering, None)
        .unwrap();
    let mut session = Session {
        id: "session-a".into(),
        workspace: dir.0.clone(),
        ..Session::default()
    };
    let mut fixture = Fixture::default();
    run_with_provider(
        &mut session,
        item.message,
        &mut fixture,
        &[],
        &RunOptions {
            queued_prompt_id: Some(initial),
            ..Default::default()
        },
        &queues,
        &CancellationToken::new(),
        &mut |_| {},
    )
    .unwrap();
    assert!(fixture.0 > 0);
    assert_eq!(queues.take_admitted_ids(), vec![initial, follow]);
    assert_eq!(queues.snapshot().uncertain.len(), 2);
    let older = Session {
        id: session.id.clone(),
        workspace: session.workspace.clone(),
        ..Session::default()
    };
    queues.confirm_saved_session(&older).unwrap();
    assert_eq!(queues.snapshot().uncertain.len(), 2);
    let mut replaced = session.clone();
    for entry in &mut replaced.entries {
        entry.fields.remove("queueReceipt");
    }
    queues.confirm_saved_session(&replaced).unwrap();
    assert_eq!(queues.snapshot().uncertain.len(), 2);
    SessionStore::new(dir.0.join("sessions"))
        .save(&session)
        .unwrap();
    queues.confirm_saved_session(&session).unwrap();
    assert!(queues.snapshot().uncertain.is_empty());
    assert!(queues.raw_input(initial).is_none());
    drop(queues);
    assert!(dir.open().snapshot().uncertain.is_empty());
}
#[test]
fn successful_run_without_store_checkpoint_stays_uncertain_after_restart() {
    let dir = Directory::new();
    let queues = dir.open();
    let id = queues
        .try_enqueue(Message::user("input"), QueueLane::Immediate, None)
        .unwrap();
    let item = queues.try_take_next().unwrap().unwrap();
    let mut session = Session {
        id: "session-a".into(),
        workspace: dir.0.clone(),
        ..Session::default()
    };
    run_with_provider(
        &mut session,
        item.message,
        &mut Fixture::default(),
        &[],
        &RunOptions {
            queued_prompt_id: Some(id),
            ..Default::default()
        },
        &queues,
        &CancellationToken::new(),
        &mut |_| {},
    )
    .unwrap();
    drop(queues);
    let queues = dir.open();
    assert_eq!(queues.snapshot().uncertain[0].id, id);
    assert!(queues.try_take_next().unwrap().is_none());
}
#[test]
fn exact_initial_reservation_validation_prevents_wrong_id_admission() {
    let dir = Directory::new();
    let queues = dir.open();
    let id = queues
        .try_enqueue(Message::user("reserved"), QueueLane::Steering, None)
        .unwrap();
    queues.try_take_next().unwrap();
    let mut session = Session {
        id: "session-a".into(),
        workspace: dir.0.clone(),
        ..Session::default()
    };
    let mut fixture = Fixture::default();
    assert!(run_with_provider(
        &mut session,
        Message::user("different"),
        &mut fixture,
        &[],
        &RunOptions {
            queued_prompt_id: Some(id),
            ..Default::default()
        },
        &queues,
        &CancellationToken::new(),
        &mut |_| {}
    )
    .is_err());
    assert_eq!(fixture.0, 0);
    queues.confirm_saved_session(&session).unwrap();
    assert_eq!(queues.snapshot().uncertain.len(), 1);
}
#[test]
fn empty_reservation_is_a_noop_and_halt_invalidates_snapshot_cache() {
    let dir = Directory::new();
    let queues = dir.open();
    let revision = queues.revision();
    assert!(queues.try_take_next().unwrap().is_none());
    assert_eq!(queues.revision(), revision);
    queues.halt_in_memory();
    assert!(queues.revision() > revision);
    assert!(queues.snapshot().halted);
}
#[test]
fn queue_crash_child() {
    let Some(path) = std::env::var_os("AGENT_DURABLE_QUEUE_CHILD") else {
        return;
    };
    let path = PathBuf::from(path);
    let queues = MessageQueues::open_durable(&path, "session-a").unwrap();
    queues.bind_workspace(&path).unwrap();
    queues
        .try_enqueue(
            image("pending"),
            QueueLane::FollowUp,
            Some(image("raw pending")),
        )
        .unwrap();
    queues
        .try_enqueue(
            image("inflight"),
            QueueLane::Immediate,
            Some(image("raw inflight")),
        )
        .unwrap();
    queues.try_take_next().unwrap().unwrap();
    fs::write(path.join("ready"), b"reserved").unwrap();
    loop {
        std::thread::sleep(Duration::from_secs(1));
    }
}
#[test]
fn killed_process_recovers_pending_and_inflight_without_replay_or_stale_lock() {
    let dir = Directory::new();
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "queue_crash_child", "--nocapture"])
        .env("AGENT_DURABLE_QUEUE_CHILD", &dir.0)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    while !dir.0.join("ready").exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    let ready = dir.0.join("ready").exists();
    let _ = child.kill();
    let status = child.wait().unwrap();
    assert!(ready && !status.success());
    let queues = dir.open();
    assert_eq!(queues.len(), 1);
    assert_eq!(queues.snapshot().uncertain.len(), 1);
    assert!(queues.try_take_next().unwrap().is_none());
    assert_eq!(queues.snapshot().items[0].message, image("pending"));
    assert_eq!(
        queues.raw_input(queues.snapshot().uncertain[0].id).unwrap(),
        image("raw inflight")
    );
}
#[test]
fn workspace_and_session_binding_reject_cross_context_before_provider_call() {
    let dir = Directory::new();
    let queues = dir.open();
    let id = queues
        .try_enqueue(Message::user("bound"), QueueLane::Immediate, None)
        .unwrap();
    assert_eq!(
        queues.bind_workspace(&dir.0.join("elsewhere")),
        Err(QueueError::RecoveryRequired)
    );
    let mut fixture = Fixture::default();
    let mut session = Session {
        id: "different-session".into(),
        workspace: dir.0.clone(),
        ..Session::default()
    };
    assert!(run_with_provider(
        &mut session,
        Message::user("start"),
        &mut fixture,
        &[],
        &RunOptions::default(),
        &queues,
        &CancellationToken::new(),
        &mut |_| {}
    )
    .is_err());
    session.id = "session-a".into();
    session.workspace = dir.0.parent().unwrap().to_path_buf();
    assert!(run_with_provider(
        &mut session,
        Message::user("start"),
        &mut fixture,
        &[],
        &RunOptions::default(),
        &queues,
        &CancellationToken::new(),
        &mut |_| {}
    )
    .is_err());
    assert_eq!(fixture.0, 0);
    assert_eq!(queues.snapshot().items[0].id, id);
}
#[test]
fn explicit_send_now_resumes_only_selected_lane_after_stop_but_not_recovery() {
    let dir = Directory::new();
    let queues = dir.open();
    queues
        .try_enqueue(Message::user("ordinary"), QueueLane::Steering, None)
        .unwrap();
    let id = queues
        .try_enqueue(Message::user("now"), QueueLane::Steering, None)
        .unwrap();
    queues.try_set_paused(true).unwrap();
    queues.halt_in_memory();
    queues.promote_for_dispatch(id).unwrap();
    assert!(queues.snapshot().paused);
    assert_eq!(queues.try_take_next().unwrap().unwrap().id, id);
    assert!(queues.try_take_next().unwrap().is_none());
    drop(queues);
    let queues = dir.open();
    let other = queues.snapshot().items[0].id;
    assert_eq!(
        queues.promote_for_dispatch(other),
        Err(QueueError::RecoveryRequired)
    );
    assert!(queues.try_take_next().unwrap().is_none());
}
