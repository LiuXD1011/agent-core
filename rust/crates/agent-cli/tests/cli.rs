use serde_json::{json, Value};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "agent-cli-{}",
            agent_runtime::session::Session::default().id
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_agent-core"));
        command
            .args(["--cwd"])
            .arg(&self.0)
            .args(["--agent-dir"])
            .arg(self.0.join("agent"))
            .args(["--session-dir"])
            .arg(self.0.join("sessions"))
            .args(["--no-context-files", "--no-skills", "--no-prompt-templates"]);
        command
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        if self.0.starts_with(std::env::temp_dir()) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
}
struct Process(Child);
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn native_cli_prints_chinese_offline_answer_without_node_and_persists_resume() {
    let fixture = Fixture::new();
    let output = fixture
        .command()
        .args(["--print", "你好Rust"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8(output.stdout)
        .unwrap()
        .contains("Rust 核心已收到：你好Rust"));
    let output = fixture
        .command()
        .args(["--continue", "--mode", "json", "-p", "继续"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let events: Vec<Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(events.last().unwrap()["type"], "agent_end");
    let sessions = agent_runtime::session::SessionStore::new(fixture.0.join("sessions"))
        .list()
        .unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].context.messages.len(), 4);
}

#[test]
fn invalid_arguments_fail_before_creating_a_session() {
    let fixture = Fixture::new();
    let output = fixture
        .command()
        .args(["--mode", "unknown"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("--mode must be"));
    assert!(!fixture.0.join("sessions").exists());
}

#[test]
fn rpc_ack_stream_query_rename_and_shutdown_are_correlated_and_keys_stay_private() {
    let fixture = Fixture::new();
    let mut child = Process(
        fixture
            .command()
            .args(["--mode", "rpc", "--api-key", "offline-test-key"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let mut input = child.0.stdin.take().unwrap();
    let output = child.0.stdout.take().unwrap();
    let (sender, receiver) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(output).lines() {
            if sender.send(line.unwrap()).is_err() {
                break;
            }
        }
    });
    writeln!(
        input,
        "{}",
        json!({"id":"prompt-1","type":"prompt","message":"离线RPC\u{2028}内容"})
    )
    .unwrap();
    input.flush().unwrap();
    let mut records = vec![];
    loop {
        let line = receiver.recv_timeout(Duration::from_secs(10)).unwrap();
        assert!(!line.contains("offline-test-key"));
        let value: Value = serde_json::from_str(&line).unwrap();
        let done = value["type"] == "agent_settled";
        records.push(value);
        if done {
            break;
        }
    }
    assert_eq!(records[0]["id"], "prompt-1");
    assert_eq!(records[0]["success"], true);
    assert_eq!(records.last().unwrap()["success"], true);
    for command in [
        json!({"id":"query","type":"get_messages"}),
        json!({"id":"rename","type":"set_session_name","name":"RPC会话"}),
        json!({"id":"state","type":"get_state"}),
        json!({"id":"unknown","type":"unimplemented_command"}),
        json!({"id":"shutdown","type":"shutdown"}),
    ] {
        writeln!(input, "{command}").unwrap();
    }
    input.flush().unwrap();
    for id in ["query", "rename", "state", "unknown", "shutdown"] {
        let line = receiver.recv_timeout(Duration::from_secs(10)).unwrap();
        let value: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["id"], id);
        if id == "query" {
            assert_eq!(value["data"]["messages"].as_array().unwrap().len(), 2);
        }
        if id == "state" {
            assert_eq!(value["data"]["sessionName"], "RPC会话");
        }
        if id == "unknown" {
            assert_eq!(value["success"], false);
        }
    }
    drop(input);
    assert!(child.0.wait().unwrap().success());
    reader.join().unwrap();
    let sessions = agent_runtime::session::SessionStore::new(fixture.0.join("sessions"))
        .list()
        .unwrap();
    let persisted = serde_json::to_string(&sessions).unwrap();
    assert!(!persisted.contains("offline-test-key"));
    assert_eq!(sessions[0].name, "RPC会话");
}

#[test]
fn rpc_metadata_without_extensions_preserves_pipelined_commit_query_order_and_clear() {
    let fixture = Fixture::new();
    let mut child = Process(
        fixture
            .command()
            .args(["--mode", "rpc"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let mut input = child.0.stdin.take().unwrap();
    let output = child.0.stdout.take().unwrap();
    let (sender, receiver) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(output).lines() {
            if sender.send(line.unwrap()).is_err() {
                break;
            }
        }
    });
    let commands = [
        json!({"id":"rename","type":"set_session_name","name":"  Global\r\nname  "}),
        json!({"id":"state","type":"get_state"}),
        json!({"id":"clear","type":"set_session_name","name":" \n\r "}),
        json!({"id":"cleared","type":"get_state"}),
        json!({"id":"entries","type":"get_entries"}),
        json!({"id":"shutdown","type":"shutdown"}),
    ];
    for command in &commands {
        writeln!(input, "{command}").unwrap();
    }
    input.flush().unwrap();
    let mut records = vec![];
    for command in &commands {
        let line = receiver.recv_timeout(Duration::from_secs(10)).unwrap();
        let record: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(record["type"], "response");
        assert_eq!(record["id"], command["id"]);
        assert_eq!(record["success"], true);
        records.push(record);
    }
    assert_eq!(records[0]["data"]["sessionName"], "Global name");
    assert_eq!(records[0]["data"]["committed"], true);
    assert_eq!(records[1]["data"]["sessionName"], "Global name");
    assert_eq!(records[2]["data"]["sessionName"], Value::Null);
    assert_eq!(records[2]["data"]["committed"], true);
    assert_eq!(records[3]["data"]["sessionName"], "");
    let entries = records[4]["data"]["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 2);
    assert!(entries.iter().all(|entry| entry["type"] == "session_info"));
    assert_eq!(entries[1]["name"], "");
    assert_eq!(records[4]["data"]["leafId"], entries[1]["id"]);
    assert_eq!(records[4]["data"]["committed"], true);
    drop(input);
    assert!(child.0.wait().unwrap().success());
    reader.join().unwrap();
    let sessions = agent_runtime::session::SessionStore::new(fixture.0.join("sessions"))
        .list()
        .unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].get_session_name().unwrap(), None);
    assert!(sessions[0].context.messages.is_empty());
}
