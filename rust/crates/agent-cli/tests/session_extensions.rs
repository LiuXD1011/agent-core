use agent_cli::args::Args;
use agent_cli::client::Client;
use agent_cli::rpc::run_rpc;
use agent_cli::terminal_client::TerminalClient;
use agent_core::types::{AssistantMessage, CancellationToken, Message, ThinkingLevel};
use agent_runtime::extensions::*;
use agent_runtime::session::{Session, SessionStore};
use serde_json::{json, Value};
use std::fs;
use std::io::{self, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("session-events-{}", Session::default().id));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn args(&self) -> Args {
        Args {
            cwd: Some(self.0.clone()),
            agent_dir: Some(self.0.join("agent")),
            auth_dir: Some(self.0.join("auth")),
            session_dir: Some(self.0.join("sessions")),
            api_key: Some(String::new()),
            no_skills: true,
            no_prompts: true,
            no_context_files: true,
            ..Default::default()
        }
    }
    fn client(&self) -> Client {
        Client::from_args(&self.args()).unwrap()
    }
    fn store(&self) -> SessionStore {
        SessionStore::new(self.0.join("sessions"))
    }
    fn bytes(&self, session: &Session) -> Vec<u8> {
        fs::read(self.store().path(&session.id).unwrap()).unwrap()
    }
    fn manifest(&self, args: Vec<String>) -> PathBuf {
        let path = self.0.join("extension.json");
        fs::write(&path,json!({"id":"fixture","executable":native_executable(),"args":args,"requestTimeoutMs":30000,"maxFrameBytes":1048576}).to_string()).unwrap();
        path
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn wait(mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !predicate() {
        assert!(Instant::now() < deadline, "Fixture did not settle");
        thread::sleep(Duration::from_millis(2));
    }
}
#[derive(Default)]
struct Observer {
    records: Mutex<Vec<(String, ExtensionContext)>>,
    entered: AtomicBool,
    veto: bool,
    error: bool,
    hang_before: bool,
    hang_info: bool,
    forbidden_thread: Option<thread::ThreadId>,
}
impl Extension for Observer {
    fn id(&self) -> &str {
        "session-observer"
    }
    fn commands(&self) -> Vec<Arc<dyn ExtensionCommand>> {
        vec![Arc::new(NameCommand)]
    }
    fn before_session_change(
        &self,
        event: &SessionBeforeEvent,
        context: &ExtensionContext,
        signal: &CancellationToken,
    ) -> Result<SessionBeforeResult, String> {
        if let Some(ui) = self.forbidden_thread {
            assert_ne!(thread::current().id(), ui);
        }
        self.records
            .lock()
            .unwrap()
            .push((event.name().into(), context.clone()));
        self.entered.store(true, Ordering::SeqCst);
        if self.hang_before {
            while !signal.is_cancelled() {
                thread::sleep(Duration::from_millis(2));
            }
            return Err("Before callback aborted".into());
        }
        if self.error {
            return Err("Ordinary before callback error".into());
        }
        Ok(SessionBeforeResult { cancel: self.veto })
    }
    fn on_event(
        &self,
        event: &LifecycleEvent,
        context: &ExtensionContext,
        signal: &CancellationToken,
    ) -> Result<(), String> {
        let name = match event {
            LifecycleEvent::SessionInfoChanged { name } => {
                assert_eq!(&context.session_name, name);
                "session_info_changed"
            }
            LifecycleEvent::SessionStart { .. } => "session_start",
            LifecycleEvent::SessionShutdown { .. } => "session_shutdown",
            _ => return Ok(()),
        };
        self.records
            .lock()
            .unwrap()
            .push((name.into(), context.clone()));
        if name == "session_info_changed" {
            if let Some(ui) = self.forbidden_thread {
                assert_ne!(thread::current().id(), ui);
            }
            self.entered.store(true, Ordering::SeqCst);
            if self.hang_info {
                while !signal.is_cancelled() {
                    thread::sleep(Duration::from_millis(2));
                }
                return Err("Name notification aborted".into());
            }
            if self.error {
                return Err("Ordinary name callback error".into());
            }
        }
        Ok(())
    }
}
struct NameCommand;
impl ExtensionCommand for NameCommand {
    fn name(&self) -> &str {
        "rename"
    }
    fn execute(
        &self,
        args: &str,
        _: &ExtensionContext,
        _: &CancellationToken,
    ) -> Result<CommandResult, String> {
        Ok(CommandResult {
            session_name: Some(args.into()),
            state: json!({"name":args}),
            ..Default::default()
        })
    }
}
#[test]
fn cli_names_normalize_clear_and_observers_receive_fresh_committed_context() {
    let fixture = Fixture::new();
    let mut client = fixture.client();
    let observer = Arc::new(Observer::default());
    client
        .options
        .extensions
        .register(observer.clone())
        .unwrap();
    client
        .set_session_name("  one\r\n\ntwo  three ", &CancellationToken::new())
        .unwrap();
    assert_eq!(client.session.name, "one two  three");
    assert_eq!(
        fixture.store().list().unwrap()[0]
            .get_session_name()
            .unwrap()
            .as_deref(),
        Some("one two  three")
    );
    client
        .run(
            Message::user("/name \r\n"),
            &CancellationToken::new(),
            &mut |_| panic!("Name emitted a model event"),
        )
        .unwrap();
    assert_eq!(client.session.get_session_name().unwrap(), None);
    assert!(client.session.context.messages.is_empty());
    let records = observer.records.lock().unwrap();
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].1.session_name.as_deref(), Some("one two  three"));
    assert_eq!(records[1].1.session_name, None);
}
#[test]
fn cli_new_resume_and_fork_veto_preserve_memory_disk_and_old_runtime() {
    let fixture = Fixture::new();
    let mut client = fixture.client();
    let entry = client
        .session
        .append_message(Message::user("edit me"))
        .unwrap();
    client.save().unwrap();
    let target = Session {
        workspace: fixture.0.clone(),
        ..Default::default()
    };
    fixture.store().save(&target).unwrap();
    let observer = Arc::new(Observer {
        veto: true,
        ..Default::default()
    });
    client
        .options
        .extensions
        .register(observer.clone())
        .unwrap();
    let memory = serde_json::to_value(&client.session).unwrap();
    let disk = fixture.bytes(&client.session);
    let target_disk = fixture.bytes(&target);
    assert!(client
        .new_session()
        .unwrap_err()
        .contains("cancelled by extension"));
    assert!(client
        .resume(&target.id)
        .unwrap_err()
        .contains("cancelled by extension"));
    assert!(client
        .run(
            Message::user(format!("/fork {entry}")),
            &CancellationToken::new(),
            &mut |_| panic!("Fork called model")
        )
        .unwrap_err()
        .contains("cancelled by extension"));
    assert_eq!(serde_json::to_value(&client.session).unwrap(), memory);
    assert_eq!(fixture.bytes(&client.session), disk);
    assert_eq!(fixture.bytes(&target), target_disk);
    assert_eq!(fixture.store().list().unwrap().len(), 2);
    let records = observer.records.lock().unwrap();
    assert_eq!(records.len(), 3);
    assert!(records
        .iter()
        .all(|(name, ctx)| name.starts_with("session_before")
            && ctx.session_id.as_ref() == Some(&client.session.id)));
}
#[test]
fn cli_veto_does_not_materialize_an_unsaved_session() {
    let fixture = Fixture::new();
    let mut client = fixture.client();
    client
        .options
        .extensions
        .register(Arc::new(Observer {
            veto: true,
            ..Default::default()
        }))
        .unwrap();
    assert!(client.new_session().is_err());
    assert!(fixture.store().list().unwrap().is_empty());
}
#[test]
fn cli_ordinary_session_hook_error_does_not_veto_and_name_save_failure_does_not_publish() {
    let fixture = Fixture::new();
    let mut client = fixture.client();
    let observer = Arc::new(Observer {
        error: true,
        ..Default::default()
    });
    client
        .options
        .extensions
        .register(observer.clone())
        .unwrap();
    let old = client.session.id.clone();
    client.new_session().unwrap();
    assert_ne!(client.session.id, old);
    assert_eq!(
        client.options.extensions.drain_diagnostics()[0].event,
        "session_before_switch"
    );
    let before = serde_json::to_value(&client.session).unwrap();
    let count = observer.records.lock().unwrap().len();
    let bad = fixture.0.join("not-a-directory");
    fs::write(&bad, b"fixture").unwrap();
    client.store = Some(SessionStore::new(&bad));
    assert!(client
        .set_session_name("not committed", &CancellationToken::new())
        .is_err());
    assert_eq!(serde_json::to_value(&client.session).unwrap(), before);
    assert_eq!(observer.records.lock().unwrap().len(), count);
}

struct ChannelRead {
    receiver: mpsc::Receiver<Vec<u8>>,
    pending: io::Cursor<Vec<u8>>,
}
impl Read for ChannelRead {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        if self.pending.position() == self.pending.get_ref().len() as u64 {
            self.pending = io::Cursor::new(self.receiver.recv().unwrap_or_default());
        }
        self.pending.read(bytes)
    }
}
struct Sink {
    sender: mpsc::Sender<Value>,
    pending: Vec<u8>,
}
impl Write for Sink {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.pending.extend_from_slice(bytes);
        while let Some(end) = self.pending.iter().position(|b| *b == b'\n') {
            let record: Vec<_> = self.pending.drain(..=end).collect();
            let _ = self.sender.send(serde_json::from_slice(&record).unwrap());
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
struct Rpc {
    sender: Option<mpsc::Sender<Vec<u8>>>,
    receiver: mpsc::Receiver<Value>,
    worker: Option<JoinHandle<Result<(), String>>>,
    signal: CancellationToken,
    records: Vec<Value>,
}
impl Rpc {
    fn new(client: Client) -> Self {
        let (sender, input) = mpsc::channel();
        let (output, receiver) = mpsc::channel();
        let signal = CancellationToken::new();
        let cancellation = signal.clone();
        let worker = thread::spawn(move || {
            run_rpc(
                BufReader::new(ChannelRead {
                    receiver: input,
                    pending: io::Cursor::new(vec![]),
                }),
                &mut Sink {
                    sender: output,
                    pending: vec![],
                },
                client,
                &cancellation,
            )
        });
        Self {
            sender: Some(sender),
            receiver,
            worker: Some(worker),
            signal,
            records: vec![],
        }
    }
    fn send(&self, value: Value) {
        self.sender
            .as_ref()
            .unwrap()
            .send(format!("{value}\n").into_bytes())
            .unwrap();
    }
    fn until(&mut self, predicate: impl Fn(&Value) -> bool) -> Value {
        loop {
            let value = self
                .receiver
                .recv_timeout(Duration::from_secs(10))
                .unwrap_or_else(|error| panic!("{error}: {:?}", self.records));
            let done = predicate(&value);
            self.records.push(value.clone());
            if done {
                return value;
            }
        }
    }
    fn response(&mut self, id: &str) -> Value {
        self.until(|v| v["type"] == "response" && v["id"] == id)
    }
    fn query(&mut self, kind: &str) -> Value {
        self.send(json!({"type":kind,"id":"query"}));
        self.response("query")["data"].clone()
    }
    fn finish(&mut self) {
        self.sender.take();
        wait(|| self.worker.as_ref().unwrap().is_finished());
        self.worker.take().unwrap().join().unwrap().unwrap();
    }
}
impl Drop for Rpc {
    fn drop(&mut self) {
        self.signal.cancel();
        self.sender.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
#[test]
fn rpc_veto_is_cancelled_success_and_never_replaces_committed_tree() {
    let fixture = Fixture::new();
    let mut client = fixture.client();
    let entry = client
        .session
        .append_message(Message::user("edit me"))
        .unwrap();
    client.save().unwrap();
    let original = client.session.clone();
    let disk = fixture.bytes(&original);
    let target = Session {
        workspace: fixture.0.clone(),
        ..Default::default()
    };
    fixture.store().save(&target).unwrap();
    client
        .options
        .extensions
        .register(Arc::new(Observer {
            veto: true,
            ..Default::default()
        }))
        .unwrap();
    let mut rpc = Rpc::new(client);
    for command in [
        json!({"type":"new_session","id":"veto"}),
        json!({"type":"switch_session","id":"veto","sessionPath":fixture.store().path(&target.id).unwrap()}),
        json!({"type":"fork","id":"veto","entryId":entry}),
    ] {
        rpc.send(command);
        let result = rpc.response("veto");
        assert_eq!(result["success"], true);
        assert_eq!(result["data"]["cancelled"], true);
        rpc.until(|v| v["type"] == "agent_settled");
        assert_eq!(rpc.query("get_state")["sessionId"], original.id);
        assert_eq!(
            rpc.query("get_tree")["leafId"],
            json!(original.get_leaf_id().unwrap())
        );
        assert_eq!(fixture.bytes(&original), disk);
    }
    assert!(!rpc
        .records
        .iter()
        .any(|v| v["type"] == "agent_start" || v["type"] == "message_end"));
    rpc.finish();
}
#[test]
fn rpc_extension_name_commits_tree_before_cancellable_notification() {
    let fixture = Fixture::new();
    let client = fixture.client();
    let observer = Arc::new(Observer {
        hang_info: true,
        ..Default::default()
    });
    client
        .options
        .extensions
        .register(observer.clone())
        .unwrap();
    let id = client.session.id.clone();
    let mut rpc = Rpc::new(client);
    rpc.send(
        json!({"type":"extension_command","id":"rename","name":"rename","args":" new\nname "}),
    );
    wait(|| observer.entered.load(Ordering::SeqCst));
    assert_eq!(rpc.query("get_state")["sessionName"], "new name");
    let tree = rpc.query("get_entries");
    assert_eq!(tree["committed"], true);
    assert!(tree["entries"]
        .as_array()
        .unwrap()
        .iter()
        .any(|entry| entry["type"] == "session_info" && entry["name"] == "new name"));
    assert_eq!(fixture.store().list().unwrap()[0].id, id);
    assert_eq!(
        fixture.store().list().unwrap()[0]
            .get_session_name()
            .unwrap()
            .as_deref(),
        Some("new name")
    );
    let start = Instant::now();
    rpc.send(json!({"type":"abort","id":"abort"}));
    assert_eq!(rpc.response("abort")["success"], true);
    assert_eq!(rpc.response("rename")["success"], false);
    rpc.until(|v| v["type"] == "agent_settled");
    assert!(start.elapsed() < Duration::from_secs(2));
    assert_eq!(rpc.query("get_state")["sessionName"], "new name");
    assert!(fixture.store().list().unwrap()[0]
        .context
        .messages
        .is_empty());
    rpc.finish();
}
fn native_executable() -> &'static Path {
    static EXE: OnceLock<PathBuf> = OnceLock::new();
    EXE.get_or_init(|| {
        let dir =
            std::env::temp_dir().join(format!("session-events-compiler-{}", Session::default().id));
        fs::create_dir(&dir).unwrap();
        let exe = dir.join(if cfg!(windows) {
            "fixture.exe"
        } else {
            "fixture"
        });
        let result = Command::new(std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into()))
            .args(["--edition=2021", "-O"])
            .arg(
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../agent-runtime/examples/native_extension_fixture.rs"),
            )
            .arg("-o")
            .arg(&exe)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        exe
    })
}
#[test]
fn rpc_real_native_hanging_before_switch_aborts_under_two_seconds_without_commit() {
    let fixture = Fixture::new();
    let record = fixture.0.join("requests.jsonl");
    let manifest = fixture.manifest(vec![
        "--hang-before-switch".into(),
        format!("--record={}", record.display()),
    ]);
    let client = Client::from_args(&Args {
        extensions: vec![manifest],
        ..fixture.args()
    })
    .unwrap();
    client.save().unwrap();
    let original = client.session.clone();
    let disk = fixture.bytes(&original);
    let mut rpc = Rpc::new(client);
    rpc.send(json!({"type":"new_session","id":"new"}));
    wait(|| {
        fs::read_to_string(&record)
            .unwrap_or_default()
            .lines()
            .any(|line| {
                serde_json::from_str::<Value>(line).is_ok_and(|v| v["method"] == "session_before")
            })
    });
    assert_eq!(rpc.query("get_state")["sessionId"], original.id);
    let start = Instant::now();
    rpc.send(json!({"type":"abort","id":"abort"}));
    assert_eq!(rpc.response("abort")["success"], true);
    assert_eq!(rpc.response("new")["success"], false);
    rpc.until(|v| v["type"] == "agent_settled");
    assert!(start.elapsed() < Duration::from_secs(2));
    assert_eq!(fixture.bytes(&original), disk);
    rpc.finish();
}
#[test]
fn tui_before_session_callback_runs_off_ui_and_cancel_preserves_snapshot() {
    let fixture = Fixture::new();
    let client = fixture.client();
    client.save().unwrap();
    let original = client.session.clone();
    let disk = fixture.bytes(&original);
    let observer = Arc::new(Observer {
        hang_before: true,
        forbidden_thread: Some(thread::current().id()),
        ..Default::default()
    });
    client
        .options
        .extensions
        .register(observer.clone())
        .unwrap();
    let mut tui = TerminalClient::new(client);
    tui.submit("/new", CancellationToken::new()).unwrap();
    wait(|| observer.entered.load(Ordering::SeqCst));
    let start = Instant::now();
    tui.cancel();
    wait(|| {
        tui.poll();
        !tui.model.running
    });
    assert!(start.elapsed() < Duration::from_secs(2));
    assert_eq!(tui.model.messages, original.context.messages);
    assert_eq!(fixture.bytes(&original), disk);
    assert_eq!(fixture.store().list().unwrap().len(), 1);
}
#[test]
fn tui_name_and_fork_commands_are_workers_and_do_not_run_provider() {
    let fixture = Fixture::new();
    let mut client = fixture.client();
    let entry = client
        .session
        .append_message(Message::user("edit draft"))
        .unwrap();
    client
        .session
        .append_message(Message::Assistant(AssistantMessage::text("old")))
        .unwrap();
    client.save().unwrap();
    let observer = Arc::new(Observer {
        forbidden_thread: Some(thread::current().id()),
        ..Default::default()
    });
    client
        .options
        .extensions
        .register(observer.clone())
        .unwrap();
    let mut tui = TerminalClient::new(client);
    tui.submit("/name  chosen\nname ", CancellationToken::new())
        .unwrap();
    wait(|| {
        tui.poll();
        !tui.model.running
    });
    assert_eq!(
        fixture.store().list().unwrap()[0]
            .get_session_name()
            .unwrap()
            .as_deref(),
        Some("chosen name")
    );
    tui.submit(&format!("/fork {entry}"), CancellationToken::new())
        .unwrap();
    wait(|| {
        tui.poll();
        !tui.model.running
    });
    assert!(tui.model.messages.is_empty());
    assert_eq!(tui.model.editor.text(), "edit draft");
    assert!(observer
        .records
        .lock()
        .unwrap()
        .iter()
        .any(|(kind, _)| kind == "session_before_fork"));
}

#[derive(Default)]
struct ThinkingObserver {
    trace: Mutex<Vec<(ThinkingLevel, ThinkingLevel, ExtensionContext)>>,
    entered: AtomicBool,
    hang: bool,
    error: bool,
    ui_thread: Option<thread::ThreadId>,
}
impl Extension for ThinkingObserver {
    fn id(&self) -> &str {
        "thinking-observer"
    }
    fn on_event(
        &self,
        event: &LifecycleEvent,
        context: &ExtensionContext,
        signal: &CancellationToken,
    ) -> Result<(), String> {
        if let LifecycleEvent::ThinkingLevelSelect {
            level,
            previous_level,
        } = event
        {
            if let Some(ui) = self.ui_thread {
                assert_ne!(thread::current().id(), ui);
            }
            assert_eq!(*level, context.thinking_level);
            self.trace
                .lock()
                .unwrap()
                .push((*level, *previous_level, context.clone()));
            self.entered.store(true, Ordering::SeqCst);
            if self.hang {
                while !signal.is_cancelled() {
                    thread::sleep(Duration::from_millis(2));
                }
                return Err("Thinking notification aborted".into());
            }
            if self.error {
                return Err("Ordinary thinking observation error".into());
            }
        }
        Ok(())
    }
}
#[test]
fn cli_thinking_flag_all_levels_default_and_branch_restore_are_real_metadata() {
    let fixture = Fixture::new();
    let default = fixture.client();
    assert_eq!(default.session.get_thinking_level().unwrap(), None);
    assert_eq!(default.settings.options.thinking_level, None);
    drop(default);
    for level in ThinkingLevel::ALL {
        let args = agent_cli::args::parse(vec![
            "--thinking".into(),
            level.to_string(),
            "--cwd".into(),
            fixture.0.to_string_lossy().into_owned(),
            "--no-session".into(),
            "--no-skills".into(),
            "--no-prompt-templates".into(),
            "--no-context-files".into(),
        ])
        .unwrap();
        let client = Client::from_args(&args).unwrap();
        assert_eq!(client.thinking_level(), level);
        assert_eq!(client.session.get_thinking_level().unwrap(), Some(level));
        assert!(client.session.context.messages.is_empty());
    }
    let mut client = fixture.client();
    let observer = Arc::new(ThinkingObserver::default());
    client
        .options
        .extensions
        .register(observer.clone())
        .unwrap();
    client
        .set_thinking_level(ThinkingLevel::High, &CancellationToken::new())
        .unwrap();
    let first = client
        .session
        .append_message(Message::user("branch A"))
        .unwrap();
    client
        .set_thinking_level(ThinkingLevel::Low, &CancellationToken::new())
        .unwrap();
    client
        .session
        .append_message(Message::user("branch B"))
        .unwrap();
    client
        .run(
            Message::user(format!("/navigate {first}")),
            &CancellationToken::new(),
            &mut |_| panic!("Metadata navigation invoked model"),
        )
        .unwrap();
    assert_eq!(client.thinking_level(), ThinkingLevel::High);
    assert_eq!(
        client.session.get_thinking_level().unwrap(),
        Some(ThinkingLevel::High)
    );
    client
        .set_thinking_level(ThinkingLevel::High, &CancellationToken::new())
        .unwrap();
    assert_eq!(observer.trace.lock().unwrap().len(), 3);
    let restored = fixture.store().list().unwrap().remove(0);
    assert_eq!(
        restored.get_thinking_level().unwrap(),
        Some(ThinkingLevel::High)
    );
    assert_eq!(restored.context.messages.len(), 1);
}
#[test]
fn cli_thinking_cancel_or_save_failure_preserves_entire_session_and_settings() {
    let fixture = Fixture::new();
    let mut client = fixture.client();
    let before = serde_json::to_value(&client.session).unwrap();
    let signal = CancellationToken::new();
    signal.cancel();
    assert!(client
        .set_thinking_level(ThinkingLevel::High, &signal)
        .is_err());
    assert_eq!(serde_json::to_value(&client.session).unwrap(), before);
    assert_eq!(client.settings.options.thinking_level, None);
    let bad = fixture.0.join("bad-store");
    fs::write(&bad, b"fixture").unwrap();
    client.store = Some(SessionStore::new(bad));
    assert!(client
        .set_thinking_level(ThinkingLevel::High, &CancellationToken::new())
        .is_err());
    assert_eq!(serde_json::to_value(&client.session).unwrap(), before);
    assert_eq!(client.settings.options.thinking_level, None);
}
#[test]
fn cli_same_provider_model_selection_preserves_explicit_options_and_thinking() {
    let fixture = Fixture::new();
    let mut client = fixture.client();
    client.settings.options.max_tokens = Some(2048);
    client
        .settings
        .options
        .headers
        .insert("x-fixture".into(), Some("preserved".into()));
    client
        .set_thinking_level(ThinkingLevel::High, &CancellationToken::new())
        .unwrap();
    client
        .choose_model("demo", "named", &CancellationToken::new())
        .unwrap();
    assert_eq!(client.settings.options.max_tokens, Some(2048));
    assert_eq!(
        client.settings.options.headers["x-fixture"].as_deref(),
        Some("preserved")
    );
    assert_eq!(client.thinking_level(), ThinkingLevel::High);
    assert_eq!(
        client.session.get_thinking_level().unwrap(),
        Some(ThinkingLevel::High)
    );
}
#[test]
fn rpc_thinking_commit_is_visible_during_notification_and_cancel_keeps_it() {
    let fixture = Fixture::new();
    let client = fixture.client();
    let observer = Arc::new(ThinkingObserver {
        hang: true,
        ..Default::default()
    });
    client
        .options
        .extensions
        .register(observer.clone())
        .unwrap();
    let mut rpc = Rpc::new(client);
    rpc.send(json!({"type":"set_thinking_level","id":"select","level":"high"}));
    wait(|| observer.entered.load(Ordering::SeqCst));
    assert_eq!(rpc.query("get_state")["thinkingLevel"], "high");
    let tree = rpc.query("get_entries");
    assert!(tree["entries"]
        .as_array()
        .unwrap()
        .iter()
        .any(|entry| entry["type"] == "thinking_level_change" && entry["thinkingLevel"] == "high"));
    let start = Instant::now();
    rpc.send(json!({"type":"abort","id":"abort"}));
    rpc.response("abort");
    assert_eq!(rpc.response("select")["success"], false);
    rpc.until(|v| v["type"] == "agent_settled");
    assert!(start.elapsed() < Duration::from_secs(2));
    assert_eq!(rpc.query("get_state")["thinkingLevel"], "high");
    rpc.finish();
    assert_eq!(
        fixture.store().list().unwrap()[0]
            .get_thinking_level()
            .unwrap(),
        Some(ThinkingLevel::High)
    );
    assert!(fixture.store().list().unwrap()[0]
        .context
        .messages
        .is_empty());
}
#[test]
fn tui_thinking_command_notifies_on_worker_and_never_enters_input_or_model() {
    let fixture = Fixture::new();
    let client = fixture.client();
    let observer = Arc::new(ThinkingObserver {
        error: true,
        ui_thread: Some(thread::current().id()),
        ..Default::default()
    });
    client
        .options
        .extensions
        .register(observer.clone())
        .unwrap();
    let mut tui = TerminalClient::new(client);
    tui.submit("/thinking high", CancellationToken::new())
        .unwrap();
    wait(|| {
        tui.poll();
        !tui.model.running
    });
    assert_eq!(observer.trace.lock().unwrap().len(), 1);
    assert!(tui.model.messages.is_empty());
    assert_eq!(
        fixture.store().list().unwrap()[0]
            .get_thinking_level()
            .unwrap(),
        Some(ThinkingLevel::High)
    );
    assert!(tui.model.status.contains("high"));
}
