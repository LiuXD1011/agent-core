//! Real RPC pipes and native extension dialog frames; all data is offline.
use agent_cli::args::Args;
use agent_cli::client::Client;
use agent_cli::terminal_client::TerminalClient;
use agent_core::types::CancellationToken;
use agent_runtime::extensions::{ExtensionMode, UiDialog, UiReply};
use agent_runtime::session::{Session, SessionStore};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use serde_json::{json, Value};
use std::fs;
use std::io::{BufRead, BufReader, Write};
#[cfg(windows)]
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{mpsc, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

fn executable() -> &'static Path {
    static EXE: OnceLock<PathBuf> = OnceLock::new();
    EXE.get_or_init(|| {
        let path =
            std::env::temp_dir().join(format!("extension-ui-compiler-{}", Session::default().id));
        fs::create_dir(&path).unwrap();
        let executable = path.join(if cfg!(windows) {
            "fixture.exe"
        } else {
            "fixture"
        });
        let mut compiler =
            Command::new(std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into()));
        #[cfg(windows)]
        compiler.creation_flags(0x0800_0000);
        let result = compiler
            .args(["--edition=2021", "-O"])
            .arg(
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../agent-runtime/examples/native_extension_fixture.rs"),
            )
            .arg("-o")
            .arg(&executable)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        executable
    })
}
struct Fixture {
    path: PathBuf,
    manifest: PathBuf,
}
impl Fixture {
    fn startup_ui(&self) {
        let mut manifest: Value =
            serde_json::from_slice(&fs::read(&self.manifest).unwrap()).unwrap();
        manifest["args"]
            .as_array_mut()
            .unwrap()
            .push(json!("--ui-startup"));
        fs::write(&self.manifest, manifest.to_string()).unwrap();
    }
    fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("extension-ui-host-{}", Session::default().id));
        fs::create_dir(&path).unwrap();
        let manifest = path.join("extension.json");
        fs::write(&manifest,json!({"id":"fixture","executable":executable(),"args":["--ui-command",format!("--record={}",path.join("requests").display())],"requestTimeoutMs":30000,"maxFrameBytes":1048576}).to_string()).unwrap();
        Self { path, manifest }
    }
    fn args(&self) -> Args {
        Args {
            cwd: Some(self.path.clone()),
            provider: Some("demo".into()),
            auth_dir: Some(self.path.join("auth")),
            agent_dir: Some(self.path.join("agent")),
            session_dir: Some(self.path.join("sessions")),
            extensions: vec![self.manifest.clone()],
            no_context_files: true,
            no_skills: true,
            no_prompts: true,
            ..Default::default()
        }
    }
    fn records(&self) -> Vec<Value> {
        fs::read_to_string(self.path.join("requests"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_agent-core"));
        command
            .args([
                "--mode",
                "rpc",
                "--provider",
                "demo",
                "--no-context-files",
                "--no-skills",
                "--no-prompt-templates",
            ])
            .arg("--cwd")
            .arg(&self.path)
            .arg("--agent-dir")
            .arg(self.path.join("agent"))
            .arg("--session-dir")
            .arg(self.path.join("sessions"))
            .arg("--auth-dir")
            .arg(self.path.join("auth"))
            .arg("--extension")
            .arg(&self.manifest);
        #[cfg(windows)]
        command.creation_flags(0x0800_0000);
        command
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}
struct Rpc {
    child: Child,
    input: Option<ChildStdin>,
    output: mpsc::Receiver<Value>,
}
impl Rpc {
    fn new(fixture: &Fixture) -> Self {
        let mut child = fixture
            .command()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let input = child.stdin.take();
        let stdout = child.stdout.take().unwrap();
        let (send, output) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else {
                    break;
                };
                let value = serde_json::from_str(&line).unwrap();
                if send.send(value).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            input,
            output,
        }
    }
    fn send(&mut self, value: Value) {
        writeln!(self.input.as_mut().unwrap(), "{value}").unwrap();
        self.input.as_mut().unwrap().flush().unwrap();
    }
    fn until(&self, predicate: impl Fn(&Value) -> bool) -> Vec<Value> {
        let deadline = Instant::now() + Duration::from_secs(8);
        let mut records = vec![];
        loop {
            let value = self
                .output
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .expect("RPC UI deadline");
            let done = predicate(&value);
            records.push(value);
            if done {
                return records;
            }
        }
    }
    fn request(&self) -> Value {
        self.until(|value| value["type"] == "extension_ui_request")
            .pop()
            .unwrap()
    }
    fn exit(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert!(status.success(), "{status}");
                break;
            }
            assert!(
                Instant::now() < deadline,
                "RPC did not exit after UI shutdown"
            );
            thread::sleep(Duration::from_millis(5));
        }
    }
}
impl Drop for Rpc {
    fn drop(&mut self) {
        self.input = None;
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn wait_dialog(host: &mut TerminalClient) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while host.pending_ui().is_none() {
        host.poll();
        assert!(Instant::now() < deadline, "{}", host.model.status);
        thread::sleep(Duration::from_millis(5));
    }
}
fn drain(host: &mut TerminalClient) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while host.model.running {
        host.poll();
        assert!(Instant::now() < deadline, "{}", host.model.status);
        thread::sleep(Duration::from_millis(5));
    }
}
fn key(host: &mut TerminalClient, code: KeyCode) {
    assert!(host
        .handle_event(Event::Key(KeyEvent::new(code, KeyModifiers::NONE)))
        .is_none());
}

#[test]
fn tui_native_dialogs_use_keyboard_paste_and_preserve_the_original_draft() {
    let fixture = Fixture::new();
    let mut host = TerminalClient::new(Client::from_args(&fixture.args()).unwrap());
    host.model.editor.set_text("original raw draft").unwrap();
    host.model
        .keybindings
        .overrides
        .insert("submit".into(), vec!["f2".into()]);
    host.submit("/fixture_echo", CancellationToken::new())
        .unwrap();
    wait_dialog(&mut host);
    assert!(matches!(
        host.pending_ui().unwrap().dialog,
        UiDialog::Select { .. }
    ));
    assert!(host
        .submit("must not queue during modal", CancellationToken::new())
        .is_err());
    key(&mut host, KeyCode::Down);
    key(&mut host, KeyCode::F(2));
    wait_dialog(&mut host);
    assert!(matches!(
        host.pending_ui().unwrap().dialog,
        UiDialog::Confirm { .. }
    ));
    key(&mut host, KeyCode::F(2));
    wait_dialog(&mut host);
    assert!(matches!(
        host.pending_ui().unwrap().dialog,
        UiDialog::Input { .. }
    ));
    host.handle_event(Event::Paste("中文🙂 typed".into()));
    key(&mut host, KeyCode::F(2));
    wait_dialog(&mut host);
    assert!(matches!(
        host.pending_ui().unwrap().dialog,
        UiDialog::Editor { .. }
    ));
    assert_eq!(host.display_model().editor.text(), "original\ntext");
    host.handle_event(Event::Paste("\nappended🙂".into()));
    key(&mut host, KeyCode::F(2));
    drain(&mut host);
    assert!(host.model.messages.is_empty());
    assert_eq!(host.model.editor.text(), "original raw draft");
    let results: Value = serde_json::from_str(&host.model.status).unwrap();
    assert_eq!(
        results,
        json!([{"value":"蓝🙂"},{"confirmed":true},{"value":"中文🙂 typed"},{"value":"original\ntext\nappended🙂"}])
    );
    assert_eq!(
        fixture
            .records()
            .iter()
            .filter(|frame| frame["type"] == "host_result")
            .count(),
        4
    );
}

#[test]
fn tui_cancel_and_drop_close_a_blocking_native_dialog_in_under_two_seconds() {
    for drop_host in [false, true] {
        let fixture = Fixture::new();
        let mut host = TerminalClient::new(Client::from_args(&fixture.args()).unwrap());
        host.submit("/fixture_echo", CancellationToken::new())
            .unwrap();
        wait_dialog(&mut host);
        let start = Instant::now();
        if drop_host {
            drop(host);
        } else {
            host.cancel();
            drain(&mut host);
            assert!(host.model.messages.is_empty());
        }
        assert!(start.elapsed() < Duration::from_secs(2));
    }
}

#[test]
fn rpc_real_bidirectional_ui_frames_validate_replies_without_provider_or_fake_messages() {
    let fixture = Fixture::new();
    let mut rpc = Rpc::new(&fixture);
    rpc.send(json!({"type":"initialize","id":"initialize"}));
    let initialized = rpc
        .until(|value| value["type"] == "response" && value["id"] == "initialize")
        .pop()
        .unwrap();
    assert_eq!(initialized["success"], true);
    for command in [
        "extension_ui_response",
        "get_available_thinking_levels",
        "set_thinking_level",
        "cycle_thinking_level",
    ] {
        assert!(initialized["data"]["commands"]
            .as_array()
            .unwrap()
            .contains(&json!(command)));
    }
    for provider in ["mistral", "pi-messages"] {
        assert!(initialized["data"]["providers"]
            .as_array()
            .unwrap()
            .contains(&json!(provider)));
    }
    rpc.send(json!({"type":"extension_command","id":"command","name":"fixture_echo"}));
    let methods = ["select", "confirm", "input", "editor"];
    let replies = [
        json!({"value":"蓝🙂"}),
        json!({"confirmed":true}),
        json!({"value":"RPC 中文🙂"}),
        json!({"value":"edit\nmultiline🙂"}),
    ];
    for (method, reply) in methods.iter().zip(&replies) {
        let request = rpc.request();
        assert_eq!(request["method"], *method);
        rpc.send(json!({"type":"extension_ui_response","id":request["id"],"confirmed":false,"value":"ambiguous"}));
        let rejected = rpc
            .until(|value| value["type"] == "response" && value["id"] == request["id"])
            .pop()
            .unwrap();
        assert_eq!(rejected["success"], false);
        let mut command = reply.clone();
        command["type"] = json!("extension_ui_response");
        command["id"] = request["id"].clone();
        rpc.send(command);
        let response = rpc
            .until(|value| value["type"] == "response" && value["id"] == request["id"])
            .pop()
            .unwrap();
        assert_eq!(response["success"], true);
    }
    let records = rpc.until(|value| value["type"] == "agent_settled");
    let command = records
        .iter()
        .find(|value| value["id"] == "command")
        .unwrap();
    assert_eq!(command["success"], true);
    assert_eq!(
        serde_json::from_str::<Value>(command["data"]["text"].as_str().unwrap()).unwrap(),
        json!(replies)
    );
    rpc.send(json!({"type":"shutdown","id":"quit"}));
    rpc.exit();
    assert!(SessionStore::new(fixture.path.join("sessions"))
        .list()
        .unwrap()
        .iter()
        .all(|session| session.context.messages.is_empty()));
    let frames = fixture.records();
    assert_eq!(
        frames
            .iter()
            .filter(|frame| frame["type"] == "host_result")
            .count(),
        4
    );
    assert!(!frames.iter().any(|frame| frame["method"] == "input"
        || frame["method"] == "before_agent_start"
        || frame["method"] == "execute"));
}

#[test]
fn rpc_abort_and_eof_close_pending_ui_without_an_abandoned_command_commit() {
    for eof in [false, true] {
        let fixture = Fixture::new();
        let mut rpc = Rpc::new(&fixture);
        rpc.send(json!({"type":"extension_command","id":"command","name":"fixture_echo"}));
        let request = rpc.request();
        let start = Instant::now();
        if eof {
            rpc.input = None;
            rpc.exit();
        } else {
            rpc.send(json!({"type":"abort","id":"abort"}));
            let records = rpc.until(|value| value["type"] == "agent_settled");
            assert_eq!(records.last().unwrap()["success"], false);
            rpc.send(json!({"type":"extension_ui_response","id":request["id"],"value":"late"}));
            assert_eq!(
                rpc.until(|value| value["type"] == "response" && value["id"] == request["id"])
                    .last()
                    .unwrap()["success"],
                false
            );
            rpc.send(json!({"type":"shutdown","id":"quit"}));
            rpc.exit();
        }
        assert!(start.elapsed() < Duration::from_secs(2));
        assert!(SessionStore::new(fixture.path.join("sessions"))
            .list()
            .unwrap()
            .iter()
            .all(|session| session.context.messages.is_empty()));
    }
}

#[test]
fn tui_deferred_startup_has_live_dialogs_and_never_runs_a_provider() {
    let fixture = Fixture::new();
    fixture.startup_ui();
    let client = Client::from_args_for_mode(
        &fixture.args(),
        ExtensionMode::Tui,
        &CancellationToken::new(),
    )
    .unwrap();
    assert!(!fixture
        .records()
        .iter()
        .any(|frame| frame["params"]["event"]["type"] == "session_start"));
    let mut host = TerminalClient::new(client);
    assert!(host.startup_pending());
    host.start_startup(CancellationToken::new()).unwrap();
    wait_dialog(&mut host);
    assert_eq!(
        host.pending_ui().unwrap().dialog.title(),
        "Extension startup input"
    );
    assert!(host.model.messages.is_empty());
    let id = host.pending_ui().unwrap().id.clone();
    host.respond_ui(
        &id,
        UiReply::Value {
            value: "startup answer".into(),
        },
    )
    .unwrap();
    drain(&mut host);
    let records = fixture.records();
    let starts: Vec<_> = records
        .iter()
        .filter(|frame| frame["params"]["event"]["type"] == "session_start")
        .collect();
    assert_eq!(starts.len(), 1);
    assert_eq!(starts[0]["params"]["context"]["hasUi"], true);
    assert!(host.model.messages.is_empty());
    assert!(!records
        .iter()
        .any(|frame| frame["method"] == "input" || frame["method"] == "before_agent_start"));
}

#[test]
fn rpc_deferred_startup_dialog_can_be_answered_before_pending_commands() {
    let fixture = Fixture::new();
    fixture.startup_ui();
    let mut rpc = Rpc::new(&fixture);
    rpc.send(json!({"type":"get_messages","id":"messages"}));
    let request = rpc.request();
    assert_eq!(request["title"], "Extension startup input");
    rpc.send(json!({"type":"extension_ui_response","id":request["id"],"value":"RPC startup"}));
    let _ = rpc.until(|value| value["type"] == "response" && value["id"] == request["id"]);
    rpc.send(json!({"type":"get_state","id":"ready"}));
    let records = rpc.until(|value| value["id"] == "ready");
    assert_eq!(records.last().unwrap()["data"]["messageCount"], 0);
    rpc.send(json!({"type":"shutdown","id":"quit"}));
    rpc.exit();
    let starts: Vec<_> = fixture
        .records()
        .into_iter()
        .filter(|frame| frame["params"]["event"]["type"] == "session_start")
        .collect();
    assert_eq!(starts.len(), 1);
    assert_eq!(starts[0]["params"]["context"]["hasUi"], true);
}
