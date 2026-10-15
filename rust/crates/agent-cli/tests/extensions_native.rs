//! Actual native host/extension executables. All sessions, manifests and logs are temporary fixtures.
use agent_runtime::session::{Session, SessionStore};
use serde_json::{json, Value};
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
#[cfg(windows)]
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::{mpsc, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const WAIT: Duration = Duration::from_secs(10);
const CANCEL: Duration = Duration::from_secs(2);
fn fixture_exe() -> &'static Path {
    static EXE: OnceLock<PathBuf> = OnceLock::new();
    EXE.get_or_init(|| {
        let path = std::env::temp_dir().join(format!(
            "cli-native-fixture-compiler-{}",
            Session::default().id
        ));
        fs::create_dir(&path).unwrap();
        let output = path.join(if cfg!(windows) {
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
            .arg(&output)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        output
    })
}
struct Fixture {
    path: PathBuf,
    manifest: PathBuf,
    executable: PathBuf,
    record: PathBuf,
}
impl Fixture {
    fn new(flags: &[&str]) -> Self {
        let path =
            std::env::temp_dir().join(format!("cli-native-extension-{}", Session::default().id));
        fs::create_dir(&path).unwrap();
        let fixture = Self {
            manifest: path.join("extension.json"),
            executable: path.join(if cfg!(windows) {
                "native extension.exe"
            } else {
                "native extension"
            }),
            record: path.join("extension.requests.jsonl"),
            path,
        };
        fs::copy(fixture_exe(), &fixture.executable).unwrap();
        fixture.configure(flags, "old");
        fixture
    }
    fn configure(&self, flags: &[&str], suffix: &str) {
        let mut args = flags
            .iter()
            .map(|flag| (*flag).to_owned())
            .collect::<Vec<_>>();
        args.push(format!("--record={}", self.record.display()));
        args.push(format!(
            "--pid={}",
            self.path.join(format!("{suffix}.pid")).display()
        ));
        fs::write(&self.manifest,json!({"id":"fixture","executable":self.executable,"args":args,"requestTimeoutMs":30_000,"maxFrameBytes":1_048_576}).to_string()).unwrap();
    }
    fn command(&self, rpc: bool) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_agent-core"));
        command
            .arg("--cwd")
            .arg(&self.path)
            .arg("--agent-dir")
            .arg(self.path.join("agent"))
            .arg("--session-dir")
            .arg(self.path.join("sessions"))
            .arg("--extension")
            .arg(&self.manifest)
            .args(["--no-context-files", "--no-skills", "--no-prompt-templates"]);
        if rpc {
            command.args(["--mode", "rpc"]);
        } else {
            command.arg("--print");
        }
        #[cfg(windows)]
        command.creation_flags(0x0800_0000);
        command
    }
    fn sessions(&self) -> Vec<Session> {
        SessionStore::new(self.path.join("sessions"))
            .list()
            .unwrap()
    }
    fn requests(&self) -> Vec<Value> {
        let source = fs::read_to_string(&self.record).unwrap_or_default();
        let complete = source.rfind('\n').map_or("", |index| &source[..=index]);
        complete
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
    fn wait_method(&self, method: &str, after: usize) {
        let start = Instant::now();
        loop {
            if self
                .requests()
                .iter()
                .skip(after)
                .any(|request| request["method"] == method)
            {
                return;
            }
            assert!(start.elapsed() < WAIT, "Extension did not receive {method}");
            thread::sleep(Duration::from_millis(5));
        }
    }
    fn pid(&self, suffix: &str) -> u32 {
        let start = Instant::now();
        loop {
            if let Ok(pid) = fs::read_to_string(self.path.join(format!("{suffix}.pid"))) {
                if let Ok(pid) = pid.parse() {
                    return pid;
                }
            }
            assert!(start.elapsed() < WAIT);
            thread::sleep(Duration::from_millis(5));
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        assert!(self.path.starts_with(std::env::temp_dir()));
        let _ = fs::remove_dir_all(&self.path);
    }
}
fn assert_gone(pid: u32) {
    #[cfg(windows)]
    {
        let start = Instant::now();
        loop {
            let mut command = Command::new("tasklist.exe");
            command.creation_flags(0x0800_0000);
            let output = command
                .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
                .output()
                .unwrap();
            assert!(output.status.success());
            if !String::from_utf8_lossy(&output.stdout).contains(&format!("\"{pid}\"")) {
                return;
            }
            assert!(start.elapsed() < CANCEL, "Extension PID {pid} remains live");
            thread::sleep(Duration::from_millis(10));
        }
    }
    #[cfg(unix)]
    {
        let start = Instant::now();
        while Path::new(&format!("/proc/{pid}")).exists() {
            if fs::read_to_string(format!("/proc/{pid}/stat"))
                .is_ok_and(|value| value.split_whitespace().nth(2) == Some("Z"))
            {
                return;
            }
            assert!(start.elapsed() < CANCEL);
            thread::sleep(Duration::from_millis(10));
        }
    }
}
struct Rpc {
    child: Child,
    input: Option<ChildStdin>,
    records: mpsc::Receiver<Result<Value, String>>,
    readers: Vec<JoinHandle<()>>,
}
impl Rpc {
    fn spawn(mut command: Command, read_output: bool) -> Self {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let input = child.stdin.take();
        let output = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        let (send, records) = mpsc::channel();
        let mut readers = vec![];
        if read_output {
            readers.push(thread::spawn(move || {
                for line in BufReader::new(output).lines() {
                    let record = line.map_err(|error| error.to_string()).and_then(|line| {
                        serde_json::from_str(&line).map_err(|error| format!("{error}: {line}"))
                    });
                    if send.send(record).is_err() {
                        return;
                    }
                }
            }));
        } else {
            drop(output);
            drop(send);
        }
        readers.push(thread::spawn(move || {
            let mut reader = BufReader::new(stderr);
            let mut buffer = [0u8; 4096];
            while matches!(reader.read(&mut buffer),Ok(count) if count>0) {}
        }));
        Self {
            child,
            input,
            records,
            readers,
        }
    }
    fn send(&mut self, value: Value) {
        writeln!(self.input.as_mut().unwrap(), "{value}").unwrap();
        self.input.as_mut().unwrap().flush().unwrap();
    }
    fn until(&self, predicate: impl Fn(&Value) -> bool, timeout: Duration) -> Vec<Value> {
        let start = Instant::now();
        let mut values = vec![];
        loop {
            let value = self
                .records
                .recv_timeout(timeout.saturating_sub(start.elapsed()))
                .expect("RPC response deadline")
                .expect("Native JSON framing");
            let done = predicate(&value);
            values.push(value);
            if done {
                return values;
            }
        }
    }
    fn response(&mut self, command: Value) -> Value {
        let id = command["id"].clone();
        self.send(command);
        self.until(|value| value["id"] == id, WAIT).pop().unwrap()
    }
    fn command(&mut self, id: &str) -> Value {
        self.send(json!({"id":id,"type":"extension_command","name":"fixture_echo","args":"fixture arguments"}));
        let values = self.until(|value| value["type"] == "agent_settled", WAIT);
        let response = values.iter().find(|value| value["id"] == id).unwrap();
        assert_eq!(response["success"], true, "{response}");
        response["data"].clone()
    }
    fn wait_exit(&mut self, timeout: Duration) -> ExitStatus {
        let start = Instant::now();
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            assert!(start.elapsed() < timeout, "CLI process did not exit");
            thread::sleep(Duration::from_millis(10));
        }
    }
    fn shutdown(&mut self) {
        assert_eq!(
            self.response(json!({"id":"shutdown","type":"shutdown"}))["success"],
            true
        );
        self.input.take();
        assert!(self.wait_exit(WAIT).success());
    }
}
impl Drop for Rpc {
    fn drop(&mut self) {
        self.input.take();
        let _ = self.child.kill();
        let _ = self.child.wait();
        for reader in self.readers.drain(..) {
            let _ = reader.join();
        }
    }
}

#[test]
fn print_and_resume_restore_real_branch_state_without_sending_a_model_prompt() {
    let fixture = Fixture::new(&[]);
    let first = fixture
        .command(false)
        .arg("/fixture_echo first")
        .output()
        .unwrap();
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert_eq!(
        String::from_utf8(first.stdout).unwrap().trim(),
        "native command"
    );
    assert_gone(fixture.pid("old"));
    let first = fixture.sessions().remove(0);
    assert_eq!(
        first.runtime_state("extension:fixture").unwrap(),
        Some(json!({"count":1}))
    );
    assert_eq!(first.context.messages.len(), 1);
    let first_leaf = first.get_leaf_id().unwrap().unwrap();
    fixture.configure(&[], "resume");
    let second = fixture
        .command(false)
        .args(["--continue", "--mode", "json", "/fixture_echo second"])
        .output()
        .unwrap();
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    let output: Value = serde_json::from_slice(&second.stdout).unwrap();
    assert_eq!(output["type"], "extension_command_result");
    assert_eq!(output["result"]["state"]["count"], 2);
    assert_gone(fixture.pid("resume"));
    let second = fixture.sessions().remove(0);
    assert_eq!(second.id, first.id);
    assert_eq!(second.context.messages.len(), 2);
    assert_eq!(second.name, "Native Fixture");
    assert_eq!(
        second.runtime_state("extension:fixture").unwrap(),
        Some(json!({"count":2}))
    );
    let mut command = fixture.command(true);
    command.arg("--continue");
    let mut rpc = Rpc::spawn(command, true);
    let navigate = rpc.response(json!({"id":"navigate","type":"navigate","entryId":first_leaf}));
    assert_eq!(navigate["success"], true);
    assert_eq!(rpc.command("branched")["state"]["count"], 2);
    rpc.shutdown();
    let requests = fixture.requests();
    let commands: Vec<_> = requests
        .iter()
        .filter(|request| request["method"] == "command")
        .collect();
    assert_eq!(commands.len(), 3);
    assert_eq!(
        commands[1]["params"]["context"]["states"]["fixture"]["count"],
        1
    );
    assert_eq!(
        commands[2]["params"]["context"]["states"]["fixture"]["count"],
        1
    );
    assert!(requests
        .iter()
        .all(|request| request["method"] != "before_agent_start"));
}

#[test]
fn rpc_lists_routes_commands_and_reloads_with_saved_state_and_lifecycle_handoff() {
    let fixture = Fixture::new(&[]);
    let mut rpc = Rpc::spawn(fixture.command(true), true);
    let listing = rpc.response(json!({"id":"extensions","type":"get_extensions"}));
    assert_eq!(listing["data"]["extensions"], json!(["fixture"]));
    let commands = rpc.response(json!({"id":"commands","type":"get_commands"}));
    assert!(commands["data"]["commands"]
        .as_array()
        .unwrap()
        .iter()
        .any(|command| command["name"] == "fixture_echo" && command["source"] == "extension"));
    assert_eq!(rpc.command("first")["state"]["count"], 1);
    rpc.send(json!({"id":"slash","type":"prompt","message":"/fixture_echo slash"}));
    let values = rpc.until(|value| value["type"] == "agent_settled", WAIT);
    assert!(values
        .iter()
        .any(|value| value["id"] == "slash" && value["success"] == true));
    assert!(values
        .iter()
        .any(|value| value["type"] == "extension_command_result"
            && value["result"]["state"]["count"] == 2));
    assert!(!values.iter().any(|value| value["type"] == "agent_start"));
    let old = fixture.pid("old");
    fixture.configure(&[], "reload");
    rpc.send(json!({"id":"reload","type":"reload_extensions"}));
    let values = rpc.until(|value| value["type"] == "agent_settled", WAIT);
    assert!(values
        .iter()
        .any(|value| value["id"] == "reload" && value["success"] == true));
    assert_gone(old);
    assert_eq!(rpc.command("after-reload")["state"]["count"], 3);
    rpc.shutdown();
    assert_gone(fixture.pid("reload"));
    let requests = fixture.requests();
    assert!(requests
        .iter()
        .any(|request| request["method"] == "lifecycle"
            && request["params"]["event"]["type"] == "session_start"
            && request["params"]["event"]["reason"] == "reload"
            && request["params"]["context"]["states"]["fixture"]["count"] == 2));
}

#[test]
fn rpc_extension_busy_state_is_explicit_and_mutations_and_command_queues_are_rejected() {
    let fixture = Fixture::new(&["--slow-command-ms=600"]);
    let mut rpc = Rpc::spawn(fixture.command(true), true);
    rpc.send(json!({"id":"slow","type":"extension_command","name":"fixture_echo"}));
    fixture.wait_method("command", 0);
    let state = rpc.response(json!({"id":"state","type":"get_state"}));
    assert_eq!(state["data"]["isExtensionRunning"], true);
    assert_eq!(state["data"]["isStreaming"], false);
    let tree = rpc.response(json!({"id":"tree","type":"get_tree"}));
    assert_eq!(tree["data"]["committed"], true);
    assert_eq!(tree["data"]["isRunning"], true);
    for (index, command) in [
        json!({"type":"reload_extensions"}),
        json!({"type":"new_session"}),
        json!({"type":"extension_command","name":"fixture_echo"}),
        json!({"type":"prompt","message":"/fixture_echo queued","streamingBehavior":"followUp"}),
        json!({"type":"steer","message":"/fixture_echo steer"}),
        json!({"type":"follow_up","message":"/fixture_echo follow"}),
    ]
    .into_iter()
    .enumerate()
    {
        let mut command = command;
        command["id"] = json!(format!("busy-{index}"));
        assert_eq!(rpc.response(command)["success"], false);
    }
    let values = rpc.until(|value| value["type"] == "agent_settled", WAIT);
    assert!(values
        .iter()
        .any(|value| value["id"] == "slow" && value["success"] == true));
    rpc.shutdown();
    assert_eq!(
        fixture.sessions()[0]
            .runtime_state("extension:fixture")
            .unwrap(),
        Some(json!({"count":1}))
    );
}

#[test]
fn rpc_abort_hanging_extension_command_settles_and_preserves_the_committed_session() {
    let fixture = Fixture::new(&["--hang-command"]);
    let mut rpc = Rpc::spawn(fixture.command(true), true);
    rpc.send(json!({"id":"hang","type":"extension_command","name":"fixture_echo"}));
    fixture.wait_method("command", 0);
    let start = Instant::now();
    rpc.send(json!({"id":"abort","type":"abort"}));
    let values = rpc.until(|value| value["type"] == "agent_settled", CANCEL);
    assert!(start.elapsed() < CANCEL);
    assert!(values
        .iter()
        .any(|value| value["id"] == "abort" && value["success"] == true));
    assert!(values
        .iter()
        .any(|value| value["id"] == "hang" && value["success"] == false));
    assert_eq!(values.last().unwrap()["success"], false);
    assert_gone(fixture.pid("old"));
    rpc.shutdown();
    let session = fixture.sessions().remove(0);
    assert!(session.context.messages.is_empty());
    assert_eq!(session.runtime_state("extension:fixture").unwrap(), None);
}

#[test]
fn rpc_eof_hanging_extension_command_closes_extension_and_exits_promptly() {
    let fixture = Fixture::new(&["--hang-command"]);
    let mut rpc = Rpc::spawn(fixture.command(true), true);
    rpc.send(json!({"id":"hang","type":"extension_command","name":"fixture_echo"}));
    fixture.wait_method("command", 0);
    let start = Instant::now();
    rpc.input.take();
    let values = rpc.until(|value| value["type"] == "agent_settled", CANCEL);
    assert_eq!(values.last().unwrap()["success"], false);
    assert!(rpc.wait_exit(CANCEL).success());
    assert!(start.elapsed() < CANCEL);
    assert_gone(fixture.pid("old"));
    assert!(fixture.sessions()[0].context.messages.is_empty());
}

#[test]
fn lost_rpc_stdout_cancels_a_hanging_slash_command_and_closes_process_tree() {
    let fixture = Fixture::new(&["--hang-command"]);
    let mut rpc = Rpc::spawn(fixture.command(true), false);
    rpc.send(json!({"id":"hang","type":"prompt","message":"/fixture_echo"}));
    let start = Instant::now();
    assert!(!rpc.wait_exit(CANCEL).success());
    assert!(start.elapsed() < CANCEL);
    assert_gone(fixture.pid("old"));
    let sessions = fixture.sessions();
    assert_eq!(sessions.len(), 1);
    assert!(sessions[0].context.messages.is_empty());
}

#[test]
fn cancelled_reload_initialize_preserves_old_registry_and_command_state() {
    let fixture = Fixture::new(&[]);
    let mut rpc = Rpc::spawn(fixture.command(true), true);
    assert_eq!(rpc.command("first")["state"]["count"], 1);
    let after = fixture.requests().len();
    fixture.configure(&["--hang"], "candidate");
    rpc.send(json!({"id":"reload","type":"reload_extensions"}));
    fixture.wait_method("initialize", after);
    rpc.send(json!({"id":"abort","type":"abort"}));
    let values = rpc.until(|value| value["type"] == "agent_settled", CANCEL);
    assert!(values
        .iter()
        .any(|value| value["id"] == "reload" && value["success"] == false));
    assert_gone(fixture.pid("candidate"));
    assert_eq!(rpc.command("old-still-works")["state"]["count"], 2);
    rpc.shutdown();
    assert_gone(fixture.pid("old"));
}

#[test]
fn reload_candidate_session_start_is_cancellable_before_old_registry_commit() {
    for eof in [false, true] {
        let fixture = Fixture::new(&[]);
        let mut rpc = Rpc::spawn(fixture.command(true), true);
        assert_eq!(rpc.command("first")["state"]["count"], 1);
        let after = fixture.requests().len();
        fixture.configure(&["--hang-lifecycle"], "candidate");
        rpc.send(json!({"id":"reload","type":"reload_extensions"}));
        fixture.wait_method("lifecycle", after);
        let start = Instant::now();
        if eof {
            rpc.input.take();
        } else {
            rpc.send(json!({"id":"abort","type":"abort"}));
        }
        let values = rpc.until(|value| value["type"] == "agent_settled", CANCEL);
        assert!(start.elapsed() < CANCEL);
        assert!(values
            .iter()
            .any(|value| value["id"] == "reload" && value["success"] == false));
        assert_gone(fixture.pid("candidate"));
        if eof {
            assert!(rpc.wait_exit(CANCEL).success());
        } else {
            assert_eq!(rpc.command("old-still-works")["state"]["count"], 2);
            rpc.shutdown();
        }
        assert_gone(fixture.pid("old"));
    }
}

#[test]
fn rpc_session_mutation_loading_is_cancellable_and_never_commits_a_failed_candidate() {
    for kind in ["new_session", "clone", "clone_session", "switch_session"] {
        let fixture = Fixture::new(&[]);
        let mut rpc = Rpc::spawn(fixture.command(true), true);
        assert_eq!(rpc.command("first")["state"]["count"], 1);
        let original =
            rpc.response(json!({"id":"state","type":"get_state"}))["data"]["sessionId"].clone();
        let after = fixture.requests().len();
        fixture.configure(&["--hang"], "candidate");
        let target = fixture.path.join("target.json");
        let target_session = Session {
            workspace: fixture.path.clone(),
            ..Default::default()
        };
        fs::write(&target, serde_json::to_vec(&target_session).unwrap()).unwrap();
        rpc.send(json!({"id":"transition","type":kind,"sessionPath":target}));
        fixture.wait_method("initialize", after);
        let state = rpc.response(json!({"id":"busy-state","type":"get_state"}));
        assert_eq!(state["data"]["sessionId"], original);
        let start = Instant::now();
        rpc.send(json!({"id":"abort","type":"abort"}));
        let values = rpc.until(|value| value["type"] == "agent_settled", CANCEL);
        assert!(start.elapsed() < CANCEL);
        assert!(values
            .iter()
            .any(|value| value["id"] == "transition" && value["success"] == false));
        assert_gone(fixture.pid("candidate"));
        assert_eq!(
            rpc.response(json!({"id":"after-state","type":"get_state"}))["data"]["sessionId"],
            original
        );
        assert_eq!(rpc.command("old-still-works")["state"]["count"], 2);
        rpc.shutdown();
        assert_eq!(fixture.sessions().len(), 1);
    }
}

#[test]
fn rpc_failed_reload_manifest_keeps_the_usable_old_registry() {
    let fixture = Fixture::new(&[]);
    let mut rpc = Rpc::spawn(fixture.command(true), true);
    assert_eq!(rpc.command("first")["state"]["count"], 1);
    fs::write(&fixture.manifest, "not-json").unwrap();
    rpc.send(json!({"id":"reload","type":"reload_extensions"}));
    let values = rpc.until(|value| value["type"] == "agent_settled", WAIT);
    assert!(values
        .iter()
        .any(|value| value["id"] == "reload" && value["success"] == false));
    assert_eq!(rpc.command("old-still-works")["state"]["count"], 2);
    rpc.shutdown();
}

#[test]
fn rpc_session_candidate_startup_abort_or_eof_keeps_the_old_session_and_registry() {
    for eof in [false, true] {
        let fixture = Fixture::new(&[]);
        let mut rpc = Rpc::spawn(fixture.command(true), true);
        assert_eq!(rpc.command("first")["state"]["count"], 1);
        let original =
            rpc.response(json!({"id":"state","type":"get_state"}))["data"]["sessionId"].clone();
        let after = fixture.requests().len();
        fixture.configure(&["--hang-lifecycle"], "candidate");
        rpc.send(json!({"id":"transition","type":"new_session"}));
        fixture.wait_method("lifecycle", after);
        let start = Instant::now();
        if eof {
            rpc.input.take();
        } else {
            rpc.send(json!({"id":"abort","type":"abort"}));
        }
        let values = rpc.until(|value| value["type"] == "agent_settled", CANCEL);
        assert!(start.elapsed() < CANCEL);
        assert!(values
            .iter()
            .any(|value| value["id"] == "transition" && value["success"] == false));
        assert_gone(fixture.pid("candidate"));
        if eof {
            assert!(rpc.wait_exit(CANCEL).success());
        } else {
            assert_eq!(
                rpc.response(json!({"id":"after","type":"get_state"}))["data"]["sessionId"],
                original
            );
            assert_eq!(rpc.command("old-still-works")["state"]["count"], 2);
            rpc.shutdown();
        }
        assert_gone(fixture.pid("old"));
        let sessions = fixture.sessions();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, original.as_str().unwrap());
        assert_eq!(
            sessions[0].runtime_state("extension:fixture").unwrap(),
            Some(json!({"count":if eof {1}else{2}}))
        );
    }
}

#[test]
fn initial_cli_startup_rejects_a_native_process_that_exits_after_initialize() {
    let fixture = Fixture::new(&["--exit-after-init"]);
    let output = fixture
        .command(false)
        .arg("/fixture_echo")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(!String::from_utf8_lossy(&output.stdout).contains("native command"));
    assert!(!output.stderr.is_empty());
    assert_gone(fixture.pid("old"));
    assert!(fixture.sessions().is_empty());
}

#[test]
fn reload_startup_process_failure_does_not_replace_the_healthy_old_registry() {
    let fixture = Fixture::new(&[]);
    let mut rpc = Rpc::spawn(fixture.command(true), true);
    assert_eq!(rpc.command("first")["state"]["count"], 1);
    fixture.configure(&["--exit-after-init"], "candidate");
    rpc.send(json!({"id":"reload","type":"reload_extensions"}));
    let values = rpc.until(|value| value["type"] == "agent_settled", WAIT);
    assert!(values
        .iter()
        .any(|value| value["id"] == "reload" && value["success"] == false));
    assert_eq!(values.last().unwrap()["success"], false);
    assert_gone(fixture.pid("candidate"));
    assert_eq!(rpc.command("old-still-works")["state"]["count"], 2);
    rpc.shutdown();
    assert_gone(fixture.pid("old"));
}

#[test]
fn session_transition_startup_process_failure_preserves_the_old_identity_and_state() {
    let fixture = Fixture::new(&[]);
    let mut rpc = Rpc::spawn(fixture.command(true), true);
    assert_eq!(rpc.command("first")["state"]["count"], 1);
    let original =
        rpc.response(json!({"id":"state","type":"get_state"}))["data"]["sessionId"].clone();
    fixture.configure(&["--exit-after-init"], "candidate");
    rpc.send(json!({"id":"transition","type":"new_session"}));
    let values = rpc.until(|value| value["type"] == "agent_settled", WAIT);
    assert!(values
        .iter()
        .any(|value| value["id"] == "transition" && value["success"] == false));
    assert_gone(fixture.pid("candidate"));
    assert_eq!(
        rpc.response(json!({"id":"after","type":"get_state"}))["data"]["sessionId"],
        original
    );
    assert_eq!(rpc.command("old-still-works")["state"]["count"], 2);
    rpc.shutdown();
    let sessions = fixture.sessions();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].id, original.as_str().unwrap());
    assert_eq!(
        sessions[0].runtime_state("extension:fixture").unwrap(),
        Some(json!({"count":2}))
    );
}
