use agent_core::types::{CancellationToken, Message};
use agent_runtime::session::{Session, SessionStore};
use agent_runtime::task_store::{TaskChange, TaskState, TaskStoreError};
use std::fs;
use std::path::PathBuf;
struct Fixture {
    directory: PathBuf,
    store: SessionStore,
    session: Session,
}
impl Fixture {
    fn new() -> Self {
        let session = Session::default();
        let directory = std::env::temp_dir().join(format!("agent-task-state-{}", session.id));
        let store = SessionStore::new(&directory);
        store.save(&session).unwrap();
        Self {
            directory,
            store,
            session,
        }
    }
    fn path(&self) -> PathBuf {
        self.directory.join("task-catalog.state")
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.directory);
    }
}
#[test]
fn pin_archive_restore_survive_reopen_without_changing_session_or_queue() {
    let fixture = Fixture::new();
    let tasks = fixture.store.task_store();
    let id = &fixture.session.id;
    let before = fs::read(fixture.store.path(id).unwrap()).unwrap();
    let queue = fixture.directory.join(format!("{id}.queue"));
    fs::create_dir(&queue).unwrap();
    fs::write(queue.join("sentinel"), b"queue").unwrap();
    assert_eq!(tasks.load(id).unwrap(), TaskState::default());
    let signal = CancellationToken::new();
    assert!(
        tasks
            .update(id, 0, TaskChange::SetPinned(true), &signal)
            .unwrap()
            .pinned
    );
    let archived = tasks
        .update(id, 1, TaskChange::SetArchived(true), &signal)
        .unwrap();
    assert!(archived.pinned && archived.archived);
    assert_eq!(
        SessionStore::new(&fixture.directory)
            .task_store()
            .load(id)
            .unwrap(),
        archived
    );
    let restored = tasks
        .update(id, 2, TaskChange::SetArchived(false), &signal)
        .unwrap();
    assert_eq!(
        restored,
        TaskState {
            revision: 3,
            pinned: true,
            archived: false,
            pinned_rank: Some(0)
        }
    );
    assert_eq!(fs::read(fixture.store.path(id).unwrap()).unwrap(), before);
    assert_eq!(fs::read(queue.join("sentinel")).unwrap(), b"queue");
    assert_eq!(
        fixture
            .store
            .load_with_diagnostics()
            .unwrap()
            .sessions
            .len(),
        1
    );
    assert!(fixture
        .store
        .load_with_diagnostics()
        .unwrap()
        .diagnostics
        .is_empty());
}
#[test]
fn stale_runtime_session_save_does_not_clobber_task_organization() {
    let fixture = Fixture::new();
    let tasks = fixture.store.task_store();
    let id = &fixture.session.id;
    tasks
        .update(
            id,
            0,
            TaskChange::SetArchived(true),
            &CancellationToken::new(),
        )
        .unwrap();
    let mut stale = fixture.session.clone();
    stale
        .append_message(Message::user("runtime change"))
        .unwrap();
    stale.metadata.insert(
        "unknown-extension".into(),
        serde_json::json!({"keep": true}),
    );
    fixture.store.save(&stale).unwrap();
    assert!(tasks.load(id).unwrap().archived);
    assert_eq!(
        fixture.store.list().unwrap()[0].metadata["unknown-extension"]["keep"],
        true
    );
}
#[test]
fn stale_revision_and_cancelled_operation_preserve_committed_bytes() {
    let fixture = Fixture::new();
    let tasks = fixture.store.task_store();
    let id = &fixture.session.id;
    tasks
        .update(
            id,
            0,
            TaskChange::SetPinned(true),
            &CancellationToken::new(),
        )
        .unwrap();
    let before = fs::read(fixture.path()).unwrap();
    assert_eq!(
        tasks.update(
            id,
            0,
            TaskChange::SetArchived(true),
            &CancellationToken::new()
        ),
        Err(TaskStoreError::Conflict)
    );
    let signal = CancellationToken::new();
    signal.cancel();
    assert_eq!(
        tasks.update(id, 1, TaskChange::SetArchived(true), &signal),
        Err(TaskStoreError::Cancelled)
    );
    assert_eq!(fs::read(fixture.path()).unwrap(), before);
}
#[test]
fn missing_session_invalid_identifier_and_corrupt_schema_fail_closed() {
    let fixture = Fixture::new();
    let tasks = fixture.store.task_store();
    assert_eq!(tasks.load("missing"), Err(TaskStoreError::MissingSession));
    assert_eq!(tasks.load("../escape"), Err(TaskStoreError::InvalidId));
    for value in [
        serde_json::json!({}),
        serde_json::json!({"schema":2,"revision":1,"pinned_ids":[],"records":{}}),
        serde_json::json!({"schema":1,"revision":1,"pinned_ids":["missing"],"records":{}}),
        serde_json::json!({"schema":1,"revision":0,"pinned_ids":[],"records":{}}),
    ] {
        let bytes = serde_json::to_vec(&value).unwrap();
        fs::write(fixture.path(), &bytes).unwrap();
        assert_eq!(
            tasks.load(&fixture.session.id),
            Err(TaskStoreError::InvalidStore)
        );
        assert_eq!(
            tasks.update(
                &fixture.session.id,
                0,
                TaskChange::SetPinned(true),
                &CancellationToken::new()
            ),
            Err(TaskStoreError::InvalidStore)
        );
        assert_eq!(fs::read(fixture.path()).unwrap(), bytes);
    }
}
#[test]
fn unknown_task_fields_survive_known_field_updates() {
    let fixture = Fixture::new();
    let value = serde_json::json!({"schema":1,"revision":1,"pinned_ids":[],"records":{fixture.session.id.clone():{"archived":false,"record_future":[1,2]}},"future":{"nested":[1,"two"]}});
    fs::write(fixture.path(), serde_json::to_vec(&value).unwrap()).unwrap();
    fixture
        .store
        .task_store()
        .update(
            &fixture.session.id,
            1,
            TaskChange::SetPinned(true),
            &CancellationToken::new(),
        )
        .unwrap();
    let after: serde_json::Value =
        serde_json::from_slice(&fs::read(fixture.path()).unwrap()).unwrap();
    assert_eq!(after["future"], value["future"]);
    assert_eq!(
        after["records"][&fixture.session.id]["record_future"],
        value["records"][&fixture.session.id]["record_future"]
    );
}
#[test]
fn competing_writers_have_one_winner_and_no_lost_state() {
    let fixture = Fixture::new();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
    let workers: Vec<_> = (0..8)
        .map(|_| {
            let tasks = fixture.store.task_store();
            let id = fixture.session.id.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                tasks.update(
                    &id,
                    0,
                    TaskChange::SetPinned(true),
                    &CancellationToken::new(),
                )
            })
        })
        .collect();
    let mut successes = 0;
    for worker in workers {
        match worker.join().unwrap() {
            Ok(_) => successes += 1,
            Err(TaskStoreError::Busy | TaskStoreError::Conflict) => {}
            other => panic!("unexpected: {other:?}"),
        }
    }
    assert_eq!(successes, 1);
    assert_eq!(
        fixture
            .store
            .task_store()
            .load(&fixture.session.id)
            .unwrap()
            .revision,
        1
    );
}
#[test]
fn pin_order_is_explicit_and_archive_preserves_rank() {
    let fixture = Fixture::new();
    let tasks = fixture.store.task_store();
    let id = fixture.session.id.clone();
    let other = Session::default();
    fixture.store.save(&other).unwrap();
    let signal = CancellationToken::new();
    tasks
        .update(&id, 0, TaskChange::SetPinned(true), &signal)
        .unwrap();
    tasks
        .update(&other.id, 1, TaskChange::SetPinned(true), &signal)
        .unwrap();
    assert_eq!(
        tasks.load_catalog().unwrap().pinned_ids,
        vec![other.id.clone(), id.clone()]
    );
    tasks
        .update(&id, 2, TaskChange::SetPinned(true), &signal)
        .unwrap();
    assert_eq!(tasks.load(&id).unwrap().pinned_rank, Some(1));
    tasks
        .update(&id, 3, TaskChange::SetArchived(true), &signal)
        .unwrap();
    assert_eq!(tasks.load(&id).unwrap().pinned_rank, Some(1));
    assert_eq!(
        tasks.update(&other.id, 3, TaskChange::SetArchived(true), &signal),
        Err(TaskStoreError::Conflict)
    );
    assert_eq!(
        tasks.reorder_pinned(4, vec![id.clone(), id.clone()], &signal),
        Err(TaskStoreError::InvalidStore)
    );
    assert_eq!(
        tasks.reorder_pinned(4, vec![id.clone()], &signal),
        Err(TaskStoreError::InvalidStore)
    );
    let reordered = tasks
        .reorder_pinned(4, vec![id.clone(), other.id.clone()], &signal)
        .unwrap();
    assert_eq!(reordered.states[&id].pinned_rank, Some(0));
    tasks
        .update(&id, 5, TaskChange::SetArchived(false), &signal)
        .unwrap();
    assert_eq!(tasks.load(&id).unwrap().pinned_rank, Some(0));
    tasks
        .update(&id, 6, TaskChange::SetPinned(false), &signal)
        .unwrap();
    assert_eq!(tasks.load_catalog().unwrap().pinned_ids, vec![other.id]);
}
#[test]
fn cross_process_writers_share_lock_and_revision() {
    const ROOT: &str = "AGENT_TASK_TEST_DIRECTORY";
    if let Some(directory) = std::env::var_os(ROOT) {
        let store = SessionStore::new(PathBuf::from(directory));
        let id = std::env::var("AGENT_TASK_TEST_SESSION").unwrap();
        match store.task_store().update(
            &id,
            0,
            TaskChange::SetPinned(true),
            &CancellationToken::new(),
        ) {
            Ok(_) | Err(TaskStoreError::Busy | TaskStoreError::Conflict) => {}
            other => panic!("unexpected child result: {other:?}"),
        }
        return;
    }
    let fixture = Fixture::new();
    let mut children: Vec<_> = (0..4)
        .map(|_| {
            std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "cross_process_writers_share_lock_and_revision",
                    "--nocapture",
                ])
                .env(ROOT, &fixture.directory)
                .env("AGENT_TASK_TEST_SESSION", &fixture.session.id)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .unwrap()
        })
        .collect();
    for child in &mut children {
        assert!(child.wait().unwrap().success());
    }
    let state = fixture
        .store
        .task_store()
        .load(&fixture.session.id)
        .unwrap();
    assert_eq!(state.revision, 1);
    assert!(state.pinned);
}
#[test]
fn malformed_large_and_exhausted_catalogs_are_never_overwritten() {
    let fixture = Fixture::new();
    let tasks = fixture.store.task_store();
    let signal = CancellationToken::new();
    let inputs = [vec![b' '; 1024 * 1024 + 1], serde_json::to_vec(&serde_json::json!({"schema":1,"revision":1,"pinned_ids":[fixture.session.id,fixture.session.id],"records":{fixture.session.id.clone():{"archived":false}}})).unwrap()];
    for bytes in inputs {
        fs::write(fixture.path(), &bytes).unwrap();
        assert_eq!(tasks.load_catalog(), Err(TaskStoreError::InvalidStore));
        assert_eq!(fs::read(fixture.path()).unwrap(), bytes);
    }
    let value = serde_json::json!({"schema":1,"revision":u64::MAX,"pinned_ids":[],"records":{}});
    let bytes = serde_json::to_vec(&value).unwrap();
    fs::write(fixture.path(), &bytes).unwrap();
    assert_eq!(
        tasks.update(
            &fixture.session.id,
            u64::MAX,
            TaskChange::SetPinned(true),
            &signal
        ),
        Err(TaskStoreError::InvalidStore)
    );
    assert_eq!(fs::read(fixture.path()).unwrap(), bytes);
}
#[test]
fn missing_session_pin_can_be_removed_without_resetting_catalog() {
    let fixture = Fixture::new();
    let tasks = fixture.store.task_store();
    let signal = CancellationToken::new();
    tasks
        .update(&fixture.session.id, 0, TaskChange::SetPinned(true), &signal)
        .unwrap();
    fs::remove_file(fixture.store.path(&fixture.session.id).unwrap()).unwrap();
    tasks
        .reorder_pinned(1, vec![fixture.session.id.clone()], &signal)
        .unwrap();
    tasks
        .update(
            &fixture.session.id,
            2,
            TaskChange::SetPinned(false),
            &signal,
        )
        .unwrap();
    assert!(tasks.load_catalog().unwrap().pinned_ids.is_empty());
    assert_eq!(
        tasks.update(&fixture.session.id, 3, TaskChange::SetPinned(true), &signal),
        Err(TaskStoreError::MissingSession)
    );
}
#[cfg(windows)]
#[test]
fn failed_atomic_publication_preserves_bytes_and_cleans_temporary() {
    use std::os::windows::fs::OpenOptionsExt;
    let fixture = Fixture::new();
    let tasks = fixture.store.task_store();
    let signal = CancellationToken::new();
    tasks
        .update(&fixture.session.id, 0, TaskChange::SetPinned(true), &signal)
        .unwrap();
    let before = fs::read(fixture.path()).unwrap();
    let guard = fs::OpenOptions::new()
        .read(true)
        .share_mode(1)
        .open(fixture.path())
        .unwrap();
    assert_eq!(
        tasks.update(
            &fixture.session.id,
            1,
            TaskChange::SetArchived(true),
            &signal
        ),
        Err(TaskStoreError::Storage)
    );
    assert_eq!(fs::read(fixture.path()).unwrap(), before);
    assert!(!fs::read_dir(&fixture.directory).unwrap().any(|entry| entry
        .unwrap()
        .path()
        .extension()
        .is_some_and(|ext| ext == "tmp")));
    drop(guard);
    tasks
        .update(
            &fixture.session.id,
            1,
            TaskChange::SetArchived(true),
            &signal,
        )
        .unwrap();
}
