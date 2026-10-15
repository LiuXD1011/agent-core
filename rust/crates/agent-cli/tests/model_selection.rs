//! Offline model configuration commits, native notifications, branch restore and RPC cancellation.
use agent_cli::args::{parse, Args, OutputMode};
use agent_cli::auth_commands::auth_store_for;
use agent_cli::client::Client;
use agent_cli::rpc::run_rpc;
use agent_core::types::{CancellationToken, Message};
use agent_runtime::auth::CodexOAuthConfig;
use agent_runtime::extensions::{
    stored_model_selection, Extension, ExtensionContext, LifecycleEvent, ModelSelectSource,
};
use agent_runtime::provider::ProviderMode;
use agent_runtime::session::{Session, SessionStore};
use serde_json::{json, Value};
use std::fs;
use std::io::{self, BufReader, Read, Write};
use std::net::TcpListener;
#[cfg(windows)]
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("model-selection-{}", Session::default().id));
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
    fn sessions(&self) -> Vec<Session> {
        SessionStore::new(self.0.join("sessions")).list().unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        assert!(self.0.starts_with(std::env::temp_dir()));
        let _ = fs::remove_dir_all(&self.0);
    }
}
#[derive(Default)]
struct Observer {
    records: Mutex<Vec<(LifecycleEvent, ExtensionContext)>>,
    entered: AtomicBool,
    hang: bool,
    error: bool,
}
impl Extension for Observer {
    fn id(&self) -> &str {
        "model-observer"
    }
    fn on_event(
        &self,
        event: &LifecycleEvent,
        context: &ExtensionContext,
        signal: &CancellationToken,
    ) -> Result<(), String> {
        if matches!(event, LifecycleEvent::ModelSelect { .. }) {
            self.records
                .lock()
                .unwrap()
                .push((event.clone(), context.clone()));
        }
        if matches!(
            event,
            LifecycleEvent::ModelSelect {
                source: ModelSelectSource::Set,
                ..
            }
        ) {
            self.entered.store(true, Ordering::SeqCst);
            if self.error {
                return Err("offline notification error".into());
            }
            if self.hang {
                while !signal.is_cancelled() {
                    thread::sleep(Duration::from_millis(2));
                }
                return Err("offline model notification cancelled".into());
            }
        }
        Ok(())
    }
}
fn wait(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !condition() {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(2));
    }
}

#[test]
fn client_commits_identity_only_and_notifies_after_commit_once() {
    let fixture = Fixture::new();
    let mut client = fixture.client();
    client.settings.api_key = "not-in-session-fixture".into();
    let observer = Arc::new(Observer::default());
    client
        .options
        .extensions
        .register(observer.clone())
        .unwrap();
    client
        .choose_model("demo", "selected", &CancellationToken::new())
        .unwrap();
    let count = client.session.get_entries().unwrap().len();
    client
        .choose_model("demo", "selected", &CancellationToken::new())
        .unwrap();
    assert_eq!(client.session.get_entries().unwrap().len(), count);
    assert_eq!(observer.records.lock().unwrap().len(), 1);
    let records = observer.records.lock().unwrap();
    let (event, context) = &records[0];
    assert!(
        matches!(event,LifecycleEvent::ModelSelect{source:ModelSelectSource::Set,previous_model:Some(previous),model} if previous.provider=="demo"&&previous.model.is_empty()&&model.model=="selected")
    );
    assert_eq!(context.model.as_ref().unwrap().model, "selected");
    assert_eq!(context.states.len(), 0);
    let sessions = fixture.sessions();
    assert!(sessions[0].context.messages.is_empty());
    assert_eq!(
        stored_model_selection(&sessions[0]).unwrap().unwrap().model,
        "selected"
    );
    assert!(!serde_json::to_string(&sessions)
        .unwrap()
        .contains("not-in-session-fixture"));
}
#[test]
fn invalid_or_cancelled_selection_preserves_config_session_and_does_not_notify() {
    let fixture = Fixture::new();
    let mut client = fixture.client();
    let observer = Arc::new(Observer::default());
    client
        .options
        .extensions
        .register(observer.clone())
        .unwrap();
    let before = serde_json::to_value(&client.session).unwrap();
    assert!(client
        .choose_model("unknown-fixture-provider", "id", &CancellationToken::new())
        .is_err());
    let token = CancellationToken::new();
    token.cancel();
    assert!(client.choose_model("demo", "new", &token).is_err());
    assert_eq!(client.settings.model, "");
    assert_eq!(serde_json::to_value(&client.session).unwrap(), before);
    assert!(observer.records.lock().unwrap().is_empty());
    assert!(fixture.sessions().is_empty());
}
#[test]
fn notification_error_is_diagnostic_not_a_model_veto() {
    let fixture = Fixture::new();
    let mut client = fixture.client();
    client
        .options
        .extensions
        .register(Arc::new(Observer {
            error: true,
            ..Default::default()
        }))
        .unwrap();
    client
        .choose_model("demo", "committed", &CancellationToken::new())
        .unwrap();
    assert_eq!(client.settings.model, "committed");
    let diagnostics = client.options.extensions.drain_diagnostics();
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0].event, "model_select");
    assert_eq!(
        stored_model_selection(&fixture.sessions()[0])
            .unwrap()
            .unwrap()
            .model,
        "committed"
    );
}
#[test]
fn cancelled_post_commit_notification_retains_selection_and_never_creates_messages() {
    let fixture = Fixture::new();
    let mut client = fixture.client();
    let observer = Arc::new(Observer {
        hang: true,
        ..Default::default()
    });
    client
        .options
        .extensions
        .register(observer.clone())
        .unwrap();
    let token = CancellationToken::new();
    let signal = token.clone();
    let worker = thread::spawn(move || {
        let result = client.choose_model("demo", "committed", &signal);
        (client, result)
    });
    wait(|| observer.entered.load(Ordering::SeqCst));
    let start = Instant::now();
    token.cancel();
    let (client, result) = worker.join().unwrap();
    assert!(start.elapsed() < Duration::from_secs(2));
    assert!(result.is_err());
    assert_eq!(client.settings.model, "committed");
    assert!(client.session.context.messages.is_empty());
    assert_eq!(
        stored_model_selection(&fixture.sessions()[0])
            .unwrap()
            .unwrap()
            .model,
        "committed"
    );
}
#[test]
fn session_startup_restores_native_selection_but_explicit_cli_model_wins() {
    let fixture = Fixture::new();
    let mut client = fixture.client();
    client
        .choose_model("demo", "saved", &CancellationToken::new())
        .unwrap();
    let id = client.session.id.clone();
    drop(client);
    let restored = Client::from_args(&Args {
        session: Some(id.clone()),
        ..fixture.args()
    })
    .unwrap();
    assert_eq!(restored.settings.model, "saved");
    let explicit = Client::from_args(&Args {
        session: Some(id),
        provider: Some("demo".into()),
        model: Some("explicit".into()),
        ..fixture.args()
    })
    .unwrap();
    assert_eq!(explicit.settings.model, "explicit");
    assert_eq!(
        stored_model_selection(&explicit.session)
            .unwrap()
            .unwrap()
            .model,
        "saved"
    );
}
#[test]
fn cli_builtins_use_whitespace_tokens_and_never_fall_into_model_input() {
    let fixture = Fixture::new();
    let mut client = fixture.client();
    for command in ["/model", "/resume", "/fork"] {
        assert!(client
            .run(
                Message::user(command),
                &CancellationToken::new(),
                &mut |_| panic!("Builtin emitted agent event")
            )
            .unwrap_err()
            .contains("Usage:"));
    }
    client
        .run(
            Message::user("/model\tdemo\tnew"),
            &CancellationToken::new(),
            &mut |_| panic!("Selection emitted agent event"),
        )
        .unwrap();
    assert_eq!(client.settings.model, "new");
    assert!(client.session.context.messages.is_empty());
    let args = parse(
        ["--aws-sts-endpoint", "http://127.0.0.1:1/sts"]
            .into_iter()
            .map(str::to_owned),
    )
    .unwrap();
    assert_eq!(
        args.aws_sts_endpoint.as_deref(),
        Some("http://127.0.0.1:1/sts")
    );
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
    broken: Arc<AtomicBool>,
}
impl Write for Sink {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.broken.load(Ordering::SeqCst) {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "offline closed output",
            ));
        }
        self.pending.extend_from_slice(bytes);
        while let Some(index) = self.pending.iter().position(|byte| *byte == b'\n') {
            let line: Vec<_> = self.pending.drain(..=index).collect();
            self.sender
                .send(serde_json::from_slice(&line).unwrap())
                .unwrap();
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
    records: Vec<Value>,
    broken: Arc<AtomicBool>,
}
impl Rpc {
    fn new(client: Client) -> Self {
        Self::new_with_signal(client, CancellationToken::new())
    }
    fn new_with_signal(client: Client, interrupt: CancellationToken) -> Self {
        let (sender, input) = mpsc::channel();
        let (output, receiver) = mpsc::channel();
        let broken = Arc::new(AtomicBool::new(false));
        let worker_broken = broken.clone();
        let worker = thread::spawn(move || {
            run_rpc(
                BufReader::new(ChannelRead {
                    receiver: input,
                    pending: io::Cursor::new(vec![]),
                }),
                &mut Sink {
                    sender: output,
                    pending: vec![],
                    broken: worker_broken,
                },
                client,
                &interrupt,
            )
        });
        Self {
            sender: Some(sender),
            receiver,
            worker: Some(worker),
            records: vec![],
            broken,
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
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let value = self
                .receiver
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .unwrap_or_else(|error| panic!("RPC {error}; {:?}", self.records));
            let done = predicate(&value);
            self.records.push(value.clone());
            if done {
                return value;
            }
        }
    }
    fn response(&mut self, id: &str) -> Value {
        self.until(|value| value["type"] == "response" && value["id"] == id)
    }
    fn state(&mut self) -> Value {
        self.send(json!({"type":"get_state","id":"state"}));
        self.response("state")["data"].clone()
    }
    fn settled(&mut self) -> Value {
        self.until(|value| value["type"] == "agent_settled")
    }
    fn finish(&mut self) {
        self.send(json!({"type":"shutdown","id":"shutdown"}));
        assert_eq!(self.response("shutdown")["success"], true);
        self.sender.take();
        self.join().unwrap();
    }
    fn join(&mut self) -> Result<(), String> {
        wait(|| self.worker.as_ref().unwrap().is_finished());
        self.worker.take().unwrap().join().unwrap()
    }
}
impl Drop for Rpc {
    fn drop(&mut self) {
        self.sender.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
#[test]
fn rpc_post_commit_notification_is_responsive_and_reports_nonstreaming_busy_state() {
    let fixture = Fixture::new();
    let client = fixture.client();
    let observer = Arc::new(Observer {
        hang: true,
        ..Default::default()
    });
    client
        .options
        .extensions
        .register(observer.clone())
        .unwrap();
    let mut rpc = Rpc::new(client);
    rpc.send(json!({"type":"set_model","id":"choose","provider":"demo","modelId":"committed"}));
    wait(|| observer.entered.load(Ordering::SeqCst));
    let state = rpc.state();
    assert_eq!(state["model"]["id"], "committed");
    assert_eq!(state["isSelectingModel"], true);
    assert_eq!(state["isStreaming"], false);
    for kind in ["prompt", "steer", "follow_up"] {
        rpc.send(json!({"type":kind,"id":kind,"message":"not accepted"}));
        assert_eq!(rpc.response(kind)["success"], false);
    }
    let start = Instant::now();
    rpc.send(json!({"type":"abort","id":"abort"}));
    assert_eq!(rpc.settled()["success"], false);
    assert!(start.elapsed() < Duration::from_secs(2));
    assert!(rpc
        .records
        .iter()
        .any(|value| value["id"] == "choose" && value["success"] == false));
    assert!(!rpc
        .records
        .iter()
        .any(|value| value["type"] == "agent_start" || value["type"] == "message_end"));
    assert_eq!(rpc.state()["model"]["id"], "committed");
    rpc.finish();
    assert_eq!(
        stored_model_selection(&fixture.sessions()[0])
            .unwrap()
            .unwrap()
            .model,
        "committed"
    );
}

fn native_executable() -> &'static Path {
    static EXE: OnceLock<PathBuf> = OnceLock::new();
    EXE.get_or_init(|| {
        let path =
            std::env::temp_dir().join(format!("model-native-compiler-{}", Session::default().id));
        fs::create_dir(&path).unwrap();
        let exe = path.join(if cfg!(windows) {
            "fixture.exe"
        } else {
            "fixture"
        });
        let mut command = Command::new(std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into()));
        #[cfg(windows)]
        command.creation_flags(0x0800_0000);
        let output = command
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
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        exe
    })
}
fn native_end(output_loss: bool) {
    let fixture = Fixture::new();
    let record = fixture.0.join("requests.jsonl");
    let manifest = fixture.0.join("extension.json");
    fs::write(&manifest,json!({"id":"fixture","executable":native_executable(),"args":["--hang-model-select",format!("--record={}",record.display())],"requestTimeoutMs":30000,"maxFrameBytes":1048576}).to_string()).unwrap();
    let client = Client::from_args(&Args {
        extensions: vec![manifest],
        ..fixture.args()
    })
    .unwrap();
    let mut rpc = Rpc::new(client);
    rpc.send(
        json!({"type":"set_model","id":"choose","provider":"demo","modelId":"native-committed"}),
    );
    rpc.until(|value| value["type"] == "model_selected");
    wait(|| {
        fs::read_to_string(&record)
            .unwrap_or_default()
            .lines()
            .any(|line| {
                serde_json::from_str::<Value>(line).is_ok_and(|v| {
                    v["method"] == "lifecycle"
                        && v["params"]["event"]["type"] == "model_select"
                        && v["params"]["event"]["source"] == "set"
                })
            })
    });
    let start = Instant::now();
    if output_loss {
        rpc.broken.store(true, Ordering::SeqCst);
        rpc.send(json!({"type":"get_state","id":"broken"}));
        assert!(rpc.join().unwrap_err().contains("RPC output failed"));
        rpc.sender.take();
    } else {
        rpc.sender.take();
        rpc.join().unwrap();
    }
    assert!(start.elapsed() < Duration::from_secs(2));
    assert_eq!(
        stored_model_selection(&fixture.sessions()[0])
            .unwrap()
            .unwrap()
            .model,
        "native-committed"
    );
    assert!(fixture.sessions()[0].context.messages.is_empty());
}
#[test]
fn rpc_eof_cancels_native_notification_and_keeps_committed_selection() {
    native_end(false);
}
#[test]
fn rpc_lost_output_cancels_native_notification_and_keeps_committed_selection() {
    native_end(true);
}

fn startup_notification_end(kind: &str) {
    let fixture = Fixture::new();
    let record = fixture.0.join("requests.jsonl");
    let manifest = fixture.0.join("extension.json");
    fs::write(&manifest, json!({"id":"fixture","executable":native_executable(),"args":["--hang-model-restore",format!("--record={}",record.display())],"requestTimeoutMs":30000,"maxFrameBytes":1048576}).to_string()).unwrap();
    // The CLI constructor must not wait for a model notification before reading RPC input.
    let start = Instant::now();
    let client = Client::from_args(&Args {
        mode: OutputMode::Rpc,
        extensions: vec![manifest],
        ..fixture.args()
    })
    .unwrap();
    assert!(start.elapsed() < Duration::from_secs(2));
    let interrupt = CancellationToken::new();
    let mut rpc = Rpc::new_with_signal(client, interrupt.clone());
    wait(|| {
        fs::read_to_string(&record)
            .unwrap_or_default()
            .lines()
            .any(|line| {
                serde_json::from_str::<Value>(line).is_ok_and(|value| {
                    value["method"] == "lifecycle"
                        && value["params"]["event"]["type"] == "model_select"
                        && value["params"]["event"]["source"] == "restore"
                })
            })
    });
    rpc.send(json!({"type":"prompt","id":"queued","message":"never submitted"}));
    let state = rpc.state();
    assert_eq!(state["isStartingModelNotification"], true);
    assert_eq!(state["isSelectingModel"], true);
    assert_eq!(state["isStreaming"], false);
    assert_eq!(state["pendingStartupCommandCount"], 1);
    let start = Instant::now();
    match kind {
        "abort" => {
            rpc.send(json!({"type":"abort","id":"abort"}));
            assert_eq!(rpc.response("abort")["success"], true);
            assert_eq!(rpc.response("queued")["success"], false);
            assert_eq!(rpc.settled()["success"], false);
            assert!(start.elapsed() < Duration::from_secs(2));
            assert_eq!(rpc.state()["isStartingModelNotification"], false);
            rpc.finish();
        }
        "eof" => {
            rpc.sender.take();
            rpc.join().unwrap();
            assert!(start.elapsed() < Duration::from_secs(2));
            assert_eq!(rpc.response("queued")["success"], false);
            assert_eq!(rpc.settled()["success"], false);
        }
        "output" => {
            rpc.broken.store(true, Ordering::SeqCst);
            rpc.send(json!({"type":"get_state","id":"broken"}));
            assert!(rpc.join().unwrap_err().contains("RPC output failed"));
            rpc.sender.take();
            assert!(start.elapsed() < Duration::from_secs(2));
        }
        "interrupt" => {
            interrupt.cancel();
            rpc.join().unwrap();
            rpc.sender.take();
            assert!(start.elapsed() < Duration::from_secs(2));
            assert_eq!(rpc.response("queued")["success"], false);
            assert_eq!(rpc.settled()["success"], false);
        }
        _ => panic!("Unknown fixture end"),
    }
    assert!(!rpc
        .records
        .iter()
        .any(|value| value["type"] == "agent_start" || value["type"] == "message_end"));
    let records: Vec<Value> = fs::read_to_string(&record)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(
        records
            .iter()
            .filter(|value| value["method"] == "lifecycle"
                && value["params"]["event"]["type"] == "model_select"
                && value["params"]["event"]["source"] == "restore")
            .count(),
        1
    );
    assert!(fixture.sessions()[0].context.messages.is_empty());
    assert!(stored_model_selection(&fixture.sessions()[0])
        .unwrap()
        .is_none());
}

#[test]
fn rpc_abort_during_startup_restore_is_readable_and_rejects_buffered_prompt() {
    startup_notification_end("abort");
}
#[test]
fn rpc_eof_during_startup_restore_settles_once_without_model_input() {
    startup_notification_end("eof");
}
#[test]
fn rpc_output_loss_during_startup_restore_cancels_native_process() {
    startup_notification_end("output");
}
#[test]
fn rpc_interrupt_during_startup_restore_rejects_buffered_input_and_settles() {
    startup_notification_end("interrupt");
}

#[derive(Default)]
struct StartupGate {
    entered: AtomicBool,
    released: AtomicBool,
}
impl Extension for StartupGate {
    fn id(&self) -> &str {
        "startup-gate"
    }
    fn on_event(
        &self,
        event: &LifecycleEvent,
        _: &ExtensionContext,
        signal: &CancellationToken,
    ) -> Result<(), String> {
        if matches!(
            event,
            LifecycleEvent::ModelSelect {
                source: ModelSelectSource::Restore,
                ..
            }
        ) {
            self.entered.store(true, Ordering::SeqCst);
            while !self.released.load(Ordering::SeqCst) {
                if signal.is_cancelled() {
                    return Err("Startup gate cancelled".into());
                }
                thread::sleep(Duration::from_millis(2));
            }
        }
        Ok(())
    }
}

#[test]
fn rpc_successful_startup_replays_bounded_commands_in_order_without_false_settled() {
    let fixture = Fixture::new();
    let manifest = fixture.0.join("extension.json");
    fs::write(&manifest,json!({"id":"fixture","executable":native_executable(),"args":[],"requestTimeoutMs":30000,"maxFrameBytes":1048576}).to_string()).unwrap();
    let client = Client::from_args(&Args {
        mode: OutputMode::Rpc,
        extensions: vec![manifest],
        ..fixture.args()
    })
    .unwrap();
    let gate = Arc::new(StartupGate::default());
    client.options.extensions.register(gate.clone()).unwrap();
    let mut rpc = Rpc::new(client);
    wait(|| gate.entered.load(Ordering::SeqCst));
    for index in 0..32 {
        rpc.send(json!({"type":"set_session_name","id":index.to_string(),"name":format!("name-{index}")}));
    }
    rpc.send(json!({"type":"set_session_name","id":"overflow","name":"not accepted"}));
    assert_eq!(rpc.response("overflow")["success"], false);
    assert_eq!(rpc.state()["pendingStartupCommandCount"], 32);
    gate.released.store(true, Ordering::SeqCst);
    for index in 0..32 {
        assert_eq!(rpc.response(&index.to_string())["success"], true);
    }
    assert_eq!(rpc.state()["sessionName"], "name-31");
    assert!(!rpc.records.iter().any(
        |value| value["type"] == "agent_settled" || value["command"] == "startup_model_restore"
    ));
    rpc.finish();
    assert!(fixture.sessions()[0].context.messages.is_empty());
}

struct StartupReplayGate {
    startup: StartupGate,
    name_entered: AtomicBool,
    names: Mutex<Vec<String>>,
}
impl Extension for StartupReplayGate {
    fn id(&self) -> &str {
        "startup-replay-gate"
    }
    fn on_event(
        &self,
        event: &LifecycleEvent,
        context: &ExtensionContext,
        signal: &CancellationToken,
    ) -> Result<(), String> {
        self.startup.on_event(event, context, signal)?;
        if let LifecycleEvent::SessionInfoChanged { name: Some(name) } = event {
            self.names.lock().unwrap().push(name.clone());
            if !self.name_entered.swap(true, Ordering::SeqCst) {
                while !signal.is_cancelled() {
                    thread::sleep(Duration::from_millis(2));
                }
                return Err("Startup replay notification cancelled".into());
            }
        }
        Ok(())
    }
}
#[test]
fn rpc_startup_replay_stays_readable_and_interrupts_resolve_every_accepted_command_once() {
    for end in ["abort", "eof", "interrupt"] {
        let fixture = Fixture::new();
        let manifest = fixture.0.join("extension.json");
        fs::write(&manifest,json!({"id":"fixture","executable":native_executable(),"args":[],"requestTimeoutMs":30000,"maxFrameBytes":1048576}).to_string()).unwrap();
        let client = Client::from_args(&Args {
            mode: OutputMode::Rpc,
            extensions: vec![manifest],
            ..fixture.args()
        })
        .unwrap();
        let gate = Arc::new(StartupReplayGate {
            startup: StartupGate::default(),
            name_entered: AtomicBool::new(false),
            names: Mutex::new(Vec::new()),
        });
        client.options.extensions.register(gate.clone()).unwrap();
        let interrupt = CancellationToken::new();
        let mut rpc = Rpc::new_with_signal(client, interrupt.clone());
        wait(|| gate.startup.entered.load(Ordering::SeqCst));
        for (id, name) in [
            ("first", "kept commit"),
            ("second", "not committed"),
            ("third", "also not committed"),
        ] {
            rpc.send(json!({"type":"set_session_name","id":id,"name":name}));
        }
        assert_eq!(rpc.state()["pendingStartupCommandCount"], 3);
        gate.startup.released.store(true, Ordering::SeqCst);
        wait(|| gate.name_entered.load(Ordering::SeqCst));
        let state = rpc.state();
        assert_eq!(state["pendingStartupCommandCount"], 2);
        assert_eq!(state["sessionName"], "kept commit");
        assert_eq!(state["isStartingModelNotification"], false);
        rpc.send(json!({"type":"set_session_name","id":"busy","name":"must not overtake"}));
        assert_eq!(rpc.response("busy")["success"], false);
        let start = Instant::now();
        match end {
            "abort" => rpc.send(json!({"type":"abort","id":"abort"})),
            "eof" => {
                rpc.sender.take();
            }
            "interrupt" => interrupt.cancel(),
            _ => unreachable!(),
        }
        assert_eq!(rpc.settled()["success"], false);
        assert!(start.elapsed() < Duration::from_secs(2));
        for id in ["first", "second", "third"] {
            let responses: Vec<_> = rpc
                .records
                .iter()
                .filter(|record| record["type"] == "response" && record["id"] == id)
                .collect();
            assert_eq!(responses.len(), 1, "{end}: {id}");
            assert_eq!(responses[0]["success"], false);
        }
        assert_eq!(gate.names.lock().unwrap().as_slice(), ["kept commit"]);
        assert!(!rpc
            .records
            .iter()
            .any(|record| record["type"] == "agent_start" || record["type"] == "message_end"));
        if end == "abort" {
            let state = rpc.state();
            assert_eq!(state["pendingStartupCommandCount"], 0);
            assert_eq!(state["sessionName"], "kept commit");
            rpc.send(json!({"type":"set_session_name","id":"fresh","name":"fresh commit"}));
            assert_eq!(rpc.response("fresh")["success"], true);
            assert_eq!(rpc.state()["sessionName"], "fresh commit");
            rpc.finish();
        } else {
            rpc.join().unwrap();
            rpc.sender.take();
        }
        assert!(fixture.sessions()[0].context.messages.is_empty());
        assert_eq!(
            fixture.sessions()[0].get_session_name().unwrap().as_deref(),
            Some(if end == "abort" {
                "fresh commit"
            } else {
                "kept commit"
            })
        );
    }
}

#[test]
fn rpc_cancels_codex_refresh_before_model_commit_and_does_not_rotate_credentials() {
    let fixture = Fixture::new();
    let store = auth_store_for(&fixture.0.join("auth"));
    let access="e30.eyJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnsiY2hhdGdwdF9hY2NvdW50X2lkIjoiZml4dHVyZS1hY2NvdW50In19.fixture";
    store.write("openai-codex",json!({"type":"oauth","access":access,"refresh":"offline-refresh","expires":0,"accountId":"fixture-account"}),&CancellationToken::new()).unwrap();
    let before = fs::read(store.path()).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let (accepted, ready) = mpsc::channel();
    let server = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline);
                    thread::sleep(Duration::from_millis(2));
                }
                Err(error) => panic!("{error}"),
            }
        };
        stream.set_nonblocking(false).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_millis(20)))
            .unwrap();
        let mut bytes = vec![];
        let mut expected = None;
        loop {
            let mut buffer = [0; 4096];
            match stream.read(&mut buffer) {
                Ok(0) => panic!("Request ended early"),
                Ok(count) => bytes.extend_from_slice(&buffer[..count]),
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) => {}
                Err(error) => panic!("{error}"),
            };
            if expected.is_none() {
                if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&bytes[..end]);
                    let count = headers
                        .lines()
                        .find_map(|line| {
                            line.split_once(':')
                                .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                                .map(|(_, value)| value.trim().parse::<usize>().unwrap())
                        })
                        .unwrap();
                    expected = Some(end + 4 + count);
                }
            }
            if expected.is_some_and(|count| bytes.len() >= count) {
                break;
            }
            assert!(Instant::now() < deadline);
        }
        assert!(String::from_utf8_lossy(&bytes).contains("grant_type=refresh_token"));
        accepted.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            match stream.read(&mut [0]) {
                Ok(0) => break,
                Ok(_) => panic!("Unexpected bytes"),
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::ConnectionReset | io::ErrorKind::ConnectionAborted
                    ) =>
                {
                    break
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) =>
                {
                    assert!(Instant::now() < deadline);
                }
                Err(error) => panic!("{error}"),
            }
        }
    });
    let mut client = fixture.client();
    client.provider_id = "openai-codex".into();
    client.settings.mode = ProviderMode::OpenAiCodex;
    client.settings.model = "old".into();
    client.settings.options.codex_oauth = CodexOAuthConfig {
        token_url: format!("http://{address}/oauth/token"),
        ..Default::default()
    };
    let mut rpc = Rpc::new(client);
    rpc.send(json!({"type":"set_model","id":"choose","provider":"openai-codex","modelId":"new"}));
    ready.recv_timeout(Duration::from_secs(10)).unwrap();
    assert_eq!(rpc.state()["model"]["id"], "old");
    let start = Instant::now();
    rpc.send(json!({"type":"abort","id":"abort"}));
    assert_eq!(rpc.settled()["success"], false);
    assert!(start.elapsed() < Duration::from_secs(2));
    assert_eq!(rpc.state()["model"]["id"], "old");
    rpc.finish();
    server.join().unwrap();
    assert_eq!(fs::read(store.path()).unwrap(), before);
    assert!(stored_model_selection(&fixture.sessions()[0])
        .unwrap()
        .is_none());
    assert!(fixture.sessions()[0].context.messages.is_empty());
}

#[test]
fn rpc_branch_restore_and_clone_bind_the_selected_native_model() {
    let fixture = Fixture::new();
    let mut client = fixture.client();
    client
        .choose_model("demo", "branch-a", &CancellationToken::new())
        .unwrap();
    client.session.append_message(Message::user("a")).unwrap();
    let leaf = client.session.get_leaf_id().unwrap().unwrap();
    client
        .choose_model("demo", "branch-b", &CancellationToken::new())
        .unwrap();
    client.session.append_message(Message::user("b")).unwrap();
    client.save().unwrap();
    let mut rpc = Rpc::new(client);
    rpc.send(json!({"type":"navigate","id":"navigate","entryId":leaf}));
    assert_eq!(rpc.response("navigate")["success"], true);
    rpc.settled();
    assert_eq!(rpc.state()["model"]["id"], "branch-a");
    rpc.send(json!({"type":"new_session","id":"new"}));
    assert_eq!(rpc.response("new")["success"], true);
    rpc.settled();
    assert_eq!(rpc.state()["model"]["id"], "branch-a");
    rpc.send(json!({"type":"clone_session","id":"clone"}));
    assert_eq!(rpc.response("clone")["success"], true);
    rpc.settled();
    assert_eq!(rpc.state()["model"]["id"], "branch-a");
    rpc.finish();
    assert!(fixture
        .sessions()
        .iter()
        .all(|session| stored_model_selection(session).unwrap().unwrap().model == "branch-a"));
}
