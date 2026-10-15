//! Native-process fixtures: JSONL framing, cancellation and failure persistence.

use agent_core::types::{AssistantMessage, Message, StopReason, Usage};
use agent_runtime::session::{Session, SessionStore};
use serde_json::{json, Value};
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
#[cfg(target_os = "windows")]
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const RECORD_WAIT: Duration = Duration::from_secs(10);
const CANCEL_WAIT: Duration = Duration::from_secs(2);
const FIXTURE_KEY: &str = "offline-rpc-cancel-fixture";

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("agent-cli-rpc-control-{}", Session::default().id));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_agent-core"));
        command
            .arg("--cwd")
            .arg(&self.0)
            .arg("--agent-dir")
            .arg(self.0.join("agent"))
            .arg("--session-dir")
            .arg(self.0.join("sessions"))
            .args([
                "--no-context-files",
                "--no-skills",
                "--no-prompt-templates",
                "--mode",
                "rpc",
            ]);
        #[cfg(target_os = "windows")]
        command.creation_flags(0x0800_0000);
        command
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

struct RpcProcess {
    child: Child,
    input: Option<ChildStdin>,
    records: Receiver<Result<Value, String>>,
    readers: Vec<JoinHandle<()>>,
}
impl RpcProcess {
    fn spawn(mut command: Command) -> Self {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let input = child.stdin.take();
        let output = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        let (sender, records) = mpsc::channel();
        let stdout_reader = thread::spawn(move || {
            for line in BufReader::new(output).lines() {
                let value = line.map_err(|error| error.to_string()).and_then(|line| {
                    serde_json::from_str(&line)
                        .map_err(|error| format!("Invalid native JSON record: {error}: {line}"))
                });
                if sender.send(value).is_err() {
                    return;
                }
            }
        });
        let stderr_reader = thread::spawn(move || {
            let mut reader = BufReader::new(stderr);
            let mut buffer = [0u8; 4096];
            while matches!(reader.read(&mut buffer),Ok(count) if count>0) {}
        });
        Self {
            child,
            input,
            records,
            readers: vec![stdout_reader, stderr_reader],
        }
    }
    fn write(&mut self, bytes: &[u8]) {
        let input = self.input.as_mut().expect("RPC stdin is open");
        input.write_all(bytes).unwrap();
        input.flush().unwrap();
    }
    fn send(&mut self, command: Value) {
        self.write(format!("{command}\n").as_bytes());
    }
    fn record(&self, timeout: Duration) -> Value {
        self.records
            .recv_timeout(timeout)
            .expect("Native RPC response deadline")
            .expect("Native RPC JSON framing")
    }
    fn until(&self, timeout: Duration, predicate: impl Fn(&Value) -> bool) -> Vec<Value> {
        let start = Instant::now();
        let mut records = Vec::new();
        loop {
            let value = self.record(timeout.saturating_sub(start.elapsed()));
            let done = predicate(&value);
            records.push(value);
            if done {
                return records;
            }
        }
    }
    fn wait_exit(&mut self, timeout: Duration) -> ExitStatus {
        let start = Instant::now();
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            assert!(start.elapsed() < timeout, "Native CLI exit deadline");
            thread::sleep(Duration::from_millis(10));
        }
    }
    fn shutdown(&mut self) {
        self.send(json!({"id":"shutdown","type":"shutdown"}));
        let records = self.until(RECORD_WAIT, |value| value["id"] == "shutdown");
        assert_eq!(records.last().unwrap()["success"], true);
        self.input.take();
        assert!(self.wait_exit(RECORD_WAIT).success());
    }
}
impl Drop for RpcProcess {
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
fn oversized_record_with_utf8_crossing_the_byte_boundary_is_rejected_then_resynchronized() {
    let fixture = Fixture::new();
    let mut process = RpcProcess::spawn(fixture.command());
    let prefix = br#"{"id":"oversized","type":"prompt","message":""#;
    let mut record = prefix.to_vec();
    record.extend(std::iter::repeat_n(b'a', 1_048_576 - prefix.len()));
    record.extend_from_slice("😀\"}\n".as_bytes());
    // The first byte of the emoji is the last byte returned by Take(1_048_577).
    assert_eq!(record[1_048_576], 0xf0);
    process.write(&record);
    process.send(json!({"id":"initialize","type":"initialize"}));
    process.send(json!({"id":"state","type":"get_state"}));
    let records = process.until(RECORD_WAIT, |value| value["id"] == "state");
    assert_eq!(records[0]["command"], "parse");
    assert_eq!(records[0]["success"], false);
    assert!(records[0]["error"].as_str().unwrap().contains("1MB"));
    assert!(records
        .iter()
        .any(|value| value["id"] == "initialize" && value["success"] == true));
    assert_eq!(records.last().unwrap()["success"], true);
    process.shutdown();
}

#[test]
fn invalid_utf8_only_rejects_its_own_record_without_closing_rpc() {
    let fixture = Fixture::new();
    let mut process = RpcProcess::spawn(fixture.command());
    process.write(&[b'{', 0xff, 0xfe, b'}', b'\n']);
    process.send(json!({"id":"state","type":"get_state"}));
    let records = process.until(RECORD_WAIT, |value| value["id"] == "state");
    assert_eq!(records[0]["success"], false);
    assert!(records[0]["error"].as_str().unwrap().contains("UTF-8"));
    assert_eq!(records.last().unwrap()["success"], true);
    process.shutdown();
}

#[test]
fn extreme_native_usage_metadata_saturates_statistics_without_panicking() {
    let fixture = Fixture::new();
    let mut session = Session {
        workspace: fixture.0.clone(),
        ..Session::default()
    };
    for _ in 0..2 {
        let mut assistant = AssistantMessage::text("fixture");
        assistant.usage = Some(Usage {
            input: u64::MAX,
            output: u64::MAX,
            cache_read: u64::MAX,
            cache_write: u64::MAX,
            total_tokens: u64::MAX,
        });
        session.context.messages.push(Message::Assistant(assistant));
    }
    SessionStore::new(fixture.0.join("sessions"))
        .save(&session)
        .unwrap();
    let mut command = fixture.command();
    command.arg("--session").arg(&session.id);
    let mut process = RpcProcess::spawn(command);
    process.send(json!({"id":"stats","type":"get_session_stats"}));
    let records = process.until(RECORD_WAIT, |value| value["id"] == "stats");
    let stats = records.last().unwrap();
    assert_eq!(stats["success"], true);
    for field in ["input", "output", "cacheRead", "cacheWrite"] {
        assert_eq!(stats["data"]["tokens"][field].as_u64(), Some(u64::MAX));
    }
    process.shutdown();
}

struct StalledServer {
    address: SocketAddr,
    request: Receiver<Result<Value, String>>,
    closed: Receiver<Result<(), String>>,
    worker: Option<JoinHandle<()>>,
}
impl StalledServer {
    fn new() -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let (request_sender, request) = mpsc::channel();
        let (closed_sender, closed) = mpsc::channel();
        let worker = thread::spawn(move || {
            let result = (|| -> Result<(), String> {
                let start = Instant::now();
                let mut stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            if start.elapsed() >= RECORD_WAIT {
                                return Err("Loopback accept deadline".into());
                            }
                            thread::sleep(Duration::from_millis(5));
                        }
                        Err(error) => return Err(error.to_string()),
                    }
                };
                // Windows accepted sockets can inherit the listener's nonblocking
                // mode. A read timeout alone does not switch them back to blocking.
                stream
                    .set_nonblocking(false)
                    .map_err(|error| error.to_string())?;
                stream
                    .set_read_timeout(Some(Duration::from_millis(100)))
                    .map_err(|error| error.to_string())?;
                let payload = read_request(&mut stream)?;
                let _ = request_sender.send(Ok(payload));
                stream
                    .set_read_timeout(Some(Duration::from_millis(100)))
                    .map_err(|error| error.to_string())?;
                let start = Instant::now();
                let mut byte = [0u8; 1];
                loop {
                    match stream.read(&mut byte) {
                        Ok(0) => return Ok(()),
                        Ok(_) => return Err("Unexpected bytes after the fixture request".into()),
                        Err(error)
                            if matches!(
                                error.kind(),
                                std::io::ErrorKind::ConnectionReset
                                    | std::io::ErrorKind::ConnectionAborted
                                    | std::io::ErrorKind::BrokenPipe
                            ) =>
                        {
                            return Ok(())
                        }
                        Err(error)
                            if matches!(
                                error.kind(),
                                std::io::ErrorKind::TimedOut
                                    | std::io::ErrorKind::WouldBlock
                                    | std::io::ErrorKind::Interrupted
                            ) =>
                        {
                            if start.elapsed() >= Duration::from_secs(8) {
                                return Err("Cancelled HTTP socket remained open".into());
                            }
                        }
                        Err(error) => return Err(error.to_string()),
                    }
                }
            })();
            if let Err(error) = &result {
                let _ = request_sender.send(Err(error.clone()));
            }
            let _ = closed_sender.send(result);
        });
        Self {
            address,
            request,
            closed,
            worker: Some(worker),
        }
    }
    fn wait_request(&self) -> Value {
        self.request.recv_timeout(RECORD_WAIT).unwrap().unwrap()
    }
    fn wait_closed(&self) {
        self.closed
            .recv_timeout(CANCEL_WAIT)
            .expect("Cancelled socket close deadline")
            .expect("Cancelled socket closed");
    }
}
impl Drop for StalledServer {
    fn drop(&mut self) {
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn read_request(stream: &mut TcpStream) -> Result<Value, String> {
    let start = Instant::now();
    let mut bytes = Vec::new();
    let mut buffer = [0u8; 8192];
    let mut header_end = None;
    let mut request_end = None;
    loop {
        if start.elapsed() >= RECORD_WAIT {
            return Err("Fixture HTTP request deadline".into());
        }
        match stream.read(&mut buffer) {
            Ok(0) => return Err("Incomplete HTTP request".into()),
            Ok(count) => bytes.extend_from_slice(&buffer[..count]),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock
                        | std::io::ErrorKind::TimedOut
                        | std::io::ErrorKind::Interrupted
                ) =>
            {
                // Retain every byte already received, including partial header
                // lines or a body split across multiple timeout intervals.
                thread::sleep(Duration::from_millis(1));
                continue;
            }
            Err(error) => return Err(error.to_string()),
        }
        if bytes.len() > 16_384 + 524_288 {
            return Err("Fixture HTTP request exceeded bound".into());
        }
        if header_end.is_none() {
            if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                if end + 4 > 16_384 {
                    return Err("Fixture HTTP headers exceeded bound".into());
                }
                let headers =
                    std::str::from_utf8(&bytes[..end]).map_err(|error| error.to_string())?;
                let length = headers
                    .lines()
                    .skip(1)
                    .filter_map(|line| line.split_once(':'))
                    .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                    .ok_or("HTTP fixture requires Content-Length")?
                    .1
                    .trim()
                    .parse::<usize>()
                    .map_err(|error| error.to_string())?;
                if length > 524_288 {
                    return Err("Fixture request body exceeded bound".into());
                }
                header_end = Some(end + 4);
                request_end = Some(end + 4 + length);
            } else if bytes.len() > 16_384 {
                return Err("Fixture HTTP headers exceeded bound".into());
            }
        }
        if let (Some(header_end), Some(request_end)) = (header_end, request_end) {
            if bytes.len() >= request_end {
                return serde_json::from_slice(&bytes[header_end..request_end])
                    .map_err(|error| error.to_string());
            }
        }
    }
}

fn cancellation_fixture(eof: bool) {
    let fixture = Fixture::new();
    let server = StalledServer::new();
    let mut command = fixture.command();
    command
        .args([
            "--provider",
            "openai",
            "--model",
            "offline-fixture-model",
            "--api-key",
            FIXTURE_KEY,
            "--base-url",
        ])
        .arg(format!("http://{}/v1", server.address));
    let mut process = RpcProcess::spawn(command);
    process.send(json!({"id":"prompt","type":"prompt","message":"header stall"}));
    let payload = server.wait_request();
    assert!(
        payload["messages"].as_array().unwrap().last().unwrap()["content"]
            .to_string()
            .contains("header stall")
    );
    let start = Instant::now();
    if eof {
        process.input.take();
    } else {
        process.send(json!({"id":"abort","type":"abort"}));
    }
    let records = process.until(CANCEL_WAIT, |value| value["type"] == "agent_settled");
    assert!(start.elapsed() < CANCEL_WAIT);
    assert_eq!(records.last().unwrap()["success"], false);
    if !eof {
        assert!(records
            .iter()
            .any(|value| value["id"] == "abort" && value["success"] == true));
    }
    assert!(records.iter().any(|value| value["type"] == "message_end"
        && value["message"]["role"] == "assistant"
        && value["message"]["stopReason"] == "aborted"));
    server.wait_closed();
    if eof {
        assert!(process.wait_exit(CANCEL_WAIT).success());
    } else {
        process.shutdown();
    }
    let sessions = fixture.sessions();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].context.messages.len(), 2);
    assert!(
        matches!(sessions[0].context.messages.last(),Some(Message::Assistant(message)) if message.stop_reason==StopReason::Aborted)
    );
    assert!(!serde_json::to_string(&sessions)
        .unwrap()
        .contains(FIXTURE_KEY));
}

#[test]
fn abort_during_http_header_stall_is_prompt_settles_and_closes_socket() {
    cancellation_fixture(false);
}

#[test]
fn stdin_eof_cancels_http_header_stall_settles_persists_and_closes_socket() {
    cancellation_fixture(true);
}

struct ChildGuard(Child);
#[test]
fn rpc_emits_real_runtime_retry_events_and_reports_the_shared_handle_state() {
    let fixture = Fixture::new();
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let address = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();
    let server = thread::spawn(move || {
        let mut requests = Vec::new();
        for (status,body) in [("429 Too Many Requests","{\"error\":{\"code\":\"rate_limit_exceeded\"}}"),("200 OK","data: {\"choices\":[{\"delta\":{\"content\":\"真实重试后的离线回答\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n")] {let start=Instant::now();let mut stream=loop {match listener.accept() {Ok((stream,_))=>break stream,Err(error) if error.kind()==std::io::ErrorKind::WouldBlock && start.elapsed()<RECORD_WAIT=>thread::sleep(Duration::from_millis(5)),Err(error)=>panic!("fixture accept: {error}")}};stream.set_nonblocking(false).unwrap();stream.set_read_timeout(Some(Duration::from_millis(100))).unwrap();stream.set_write_timeout(Some(Duration::from_secs(2))).unwrap();requests.push(read_request(&mut stream).unwrap());write!(stream,"HTTP/1.1 {status}\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();}
        requests
    });
    let mut command = fixture.command();
    command
        .args([
            "--retry",
            "--provider",
            "openai-compatible",
            "--model",
            "offline-fixture-model",
            "--base-url",
        ])
        .arg(format!("http://{address}/v1"));
    let mut process = RpcProcess::spawn(command);
    process.send(json!({"id":"prompt","type":"prompt","message":"retry fixture"}));
    let records = process.until(RECORD_WAIT, |value| value["type"] == "agent_settled");
    assert_eq!(records.last().unwrap()["success"], true);
    assert!(records
        .iter()
        .any(|event| event["type"] == "retry_scheduled" && event["attempt"] == 1));
    assert!(records
        .iter()
        .any(|event| event["type"] == "retry_attempt_start"));
    assert!(records
        .iter()
        .any(|event| event["type"] == "retry_finished" && event["success"] == true));
    process.send(json!({"id":"automation","type":"get_automation_state"}));
    let result = process
        .until(RECORD_WAIT, |value| value["id"] == "automation")
        .pop()
        .unwrap();
    assert_eq!(result["data"]["retries"], 1);
    assert!(result["data"]["state"]["requestCount"].as_u64().unwrap() >= 1);
    assert_eq!(server.join().unwrap().len(), 2);
    process.shutdown();
}
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn closed_rpc_stdout_cancels_header_stall_and_persists_the_accepted_prompt() {
    let fixture = Fixture::new();
    let server = StalledServer::new();
    let mut command = fixture.command();
    command
        .args([
            "--provider",
            "openai",
            "--model",
            "offline-fixture-model",
            "--api-key",
            FIXTURE_KEY,
            "--base-url",
        ])
        .arg(format!("http://{}/v1", server.address));
    let mut process = ChildGuard(
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let mut input = process.0.stdin.take().unwrap();
    let output = process.0.stdout.take().unwrap();
    writeln!(
        input,
        "{}",
        json!({"id":"prompt","type":"prompt","message":"broken output fixture"})
    )
    .unwrap();
    input.flush().unwrap();
    assert!(server.wait_request()["messages"]
        .to_string()
        .contains("broken output fixture"));
    // The request is now waiting for HTTP response headers. A later command
    // requires another RPC write and exposes the disconnected output consumer.
    drop(output);
    let start = Instant::now();
    writeln!(input, "{}", json!({"id":"state","type":"get_state"})).unwrap();
    input.flush().unwrap();
    let status = loop {
        if let Some(status) = process.0.try_wait().unwrap() {
            break status;
        }
        assert!(
            start.elapsed() < CANCEL_WAIT,
            "Broken-output settlement deadline"
        );
        thread::sleep(Duration::from_millis(10));
    };
    assert!(!status.success());
    server.wait_closed();
    let sessions = fixture.sessions();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].context.messages.len(), 2);
    assert!(
        matches!(sessions[0].context.messages.first(), Some(Message::User(message)) if message.content.iter().any(|content|matches!(content,agent_core::types::Content::Text { text } if text == "broken output fixture")))
    );
    assert!(
        matches!(sessions[0].context.messages.last(), Some(Message::Assistant(message)) if message.stop_reason == StopReason::Aborted)
    );
}

#[test]
fn session_tree_queries_navigation_fork_clone_and_exports_use_real_entry_ids() {
    let fixture = Fixture::new();
    let mut session = Session {
        workspace: fixture.0.clone(),
        ..Session::default()
    };
    let user = session.append_message(Message::user("第一轮中文")).unwrap();
    let first_answer = session
        .append_message(Message::Assistant(AssistantMessage::text("第一轮回答")))
        .unwrap();
    session.append_message(Message::user("第二轮中文")).unwrap();
    session
        .append_message(Message::Assistant(AssistantMessage::text("第二轮回答")))
        .unwrap();
    let entries = session.entries.clone();
    SessionStore::new(fixture.0.join("sessions"))
        .save(&session)
        .unwrap();
    let mut command = fixture.command();
    command.arg("--session").arg(&session.id);
    let mut process = RpcProcess::spawn(command);
    process.send(json!({"id":"entries","type":"get_entries"}));
    let result = process
        .until(RECORD_WAIT, |value| value["id"] == "entries")
        .pop()
        .unwrap();
    assert_eq!(result["success"], true);
    assert_eq!(result["data"]["entries"], json!(entries));
    assert_eq!(result["data"]["committed"], true);
    assert_eq!(result["data"]["isRunning"], false);
    process.send(json!({"id":"tree","type":"get_tree"}));
    let result = process
        .until(RECORD_WAIT, |value| value["id"] == "tree")
        .pop()
        .unwrap();
    assert_eq!(result["data"]["tree"][0]["entry"]["id"], user);
    process.send(json!({"id":"branch","type":"get_branch","entryId":first_answer}));
    let result = process
        .until(RECORD_WAIT, |value| value["id"] == "branch")
        .pop()
        .unwrap();
    assert_eq!(result["data"]["branch"].as_array().unwrap().len(), 2);
    process.send(json!({"id":"fork","type":"fork","entryId":user}));
    let result = process
        .until(RECORD_WAIT, |value| value["id"] == "fork")
        .pop()
        .unwrap();
    assert_eq!(result["data"]["text"], "第一轮中文");
    assert_eq!(result["data"]["leafId"], Value::Null);
    process.send(json!({"id":"navigate","type":"navigate","entryId":first_answer}));
    assert_eq!(
        process
            .until(RECORD_WAIT, |value| value["id"] == "navigate")
            .pop()
            .unwrap()["success"],
        true
    );
    process.send(json!({"id":"clone","type":"clone_session"}));
    let cloned = process
        .until(RECORD_WAIT, |value| value["id"] == "clone")
        .pop()
        .unwrap();
    assert_eq!(cloned["success"], true);
    assert_ne!(cloned["data"]["sessionId"], session.id);
    let html = fixture.0.join("export.html");
    let jsonl = fixture.0.join("branch.jsonl");
    process.send(json!({"id":"html","type":"export_html","outputPath":html}));
    assert_eq!(
        process
            .until(RECORD_WAIT, |value| value["id"] == "html")
            .pop()
            .unwrap()["success"],
        true
    );
    process.send(json!({"id":"jsonl","type":"export_branch_jsonl","outputPath":jsonl}));
    assert_eq!(
        process
            .until(RECORD_WAIT, |value| value["id"] == "jsonl")
            .pop()
            .unwrap()["success"],
        true
    );
    process.send(json!({"id":"messages","type":"get_messages"}));
    let result = process
        .until(RECORD_WAIT, |value| value["id"] == "messages")
        .pop()
        .unwrap();
    assert_eq!(result["data"]["messages"].as_array().unwrap().len(), 2);
    process.shutdown();
    assert_eq!(fixture.sessions().len(), 2);
    assert!(fs::read_to_string(html).unwrap().contains("第一轮中文"));
    let imported = agent_runtime::legacy_session::load_legacy_session(&jsonl)
        .unwrap()
        .import(agent_runtime::legacy_session::LeafSelection::Latest)
        .unwrap();
    assert_eq!(imported.context.messages.len(), 2);
    assert_eq!(imported.context.messages[0].text(), "第一轮中文");
}

#[test]
fn busy_tree_queries_are_committed_snapshots_and_tree_mutations_are_rejected() {
    let fixture = Fixture::new();
    let mut session = Session {
        workspace: fixture.0.clone(),
        ..Session::default()
    };
    session
        .append_message(Message::user("committed fixture"))
        .unwrap();
    session
        .append_message(Message::Assistant(AssistantMessage::text(
            "committed answer",
        )))
        .unwrap();
    let entries = session.entries.clone();
    SessionStore::new(fixture.0.join("sessions"))
        .save(&session)
        .unwrap();
    let server = StalledServer::new();
    let mut command = fixture.command();
    command
        .arg("--session")
        .arg(&session.id)
        .args([
            "--provider",
            "openai",
            "--model",
            "offline-fixture-model",
            "--api-key",
            FIXTURE_KEY,
            "--base-url",
        ])
        .arg(format!("http://{}/v1", server.address));
    let mut process = RpcProcess::spawn(command);
    process.send(json!({"id":"prompt","type":"prompt","message":"new uncommitted fixture"}));
    server.wait_request();
    for kind in ["get_entries", "get_tree", "get_branch"] {
        process.send(json!({"id":kind,"type":kind}));
        let result = process
            .until(RECORD_WAIT, |value| value["id"] == kind)
            .pop()
            .unwrap();
        assert_eq!(result["success"], true);
        assert_eq!(result["data"]["committed"], true);
        assert_eq!(result["data"]["isRunning"], true);
        if kind == "get_entries" {
            assert_eq!(result["data"]["entries"], json!(entries));
        }
        assert!(!result.to_string().contains("new uncommitted fixture"));
    }
    for kind in [
        "navigate",
        "fork",
        "clone_session",
        "export_html",
        "export_branch_jsonl",
    ] {
        process.send(json!({"id":kind,"type":kind,"entryId":entries[0].id,"outputPath":fixture.0.join("must-not-exist") }));
        let result = process
            .until(RECORD_WAIT, |value| value["id"] == kind)
            .pop()
            .unwrap();
        assert_eq!(result["success"], false);
        assert!(result["error"].as_str().unwrap().contains("running"));
    }
    assert!(!fixture.0.join("must-not-exist").exists());
    process.send(json!({"id":"abort","type":"abort"}));
    process.until(CANCEL_WAIT, |value| value["type"] == "agent_settled");
    server.wait_closed();
    process.send(json!({"id":"settled-entries","type":"get_entries"}));
    let result = process
        .until(RECORD_WAIT, |value| value["id"] == "settled-entries")
        .pop()
        .unwrap();
    assert_eq!(result["data"]["isRunning"], false);
    assert_eq!(result["data"]["entries"].as_array().unwrap().len(), 4);
    process.shutdown();
}

#[test]
fn invalid_tree_targets_do_not_mutate_the_active_session() {
    let fixture = Fixture::new();
    let mut session = Session {
        workspace: fixture.0.clone(),
        ..Session::default()
    };
    session
        .append_message(Message::user("kept fixture"))
        .unwrap();
    session
        .append_message(Message::Assistant(AssistantMessage::text("kept answer")))
        .unwrap();
    SessionStore::new(fixture.0.join("sessions"))
        .save(&session)
        .unwrap();
    let mut command = fixture.command();
    command.arg("--session").arg(&session.id);
    let mut process = RpcProcess::spawn(command);
    for command in [
        json!({"id":"bad","type":"navigate","entryId":"missing"}),
        json!({"id":"bad","type":"navigate","entryId":42}),
        json!({"id":"bad","type":"fork","entryId":session.leaf_id}),
    ] {
        process.send(command);
        assert_eq!(
            process
                .until(RECORD_WAIT, |value| value["id"] == "bad")
                .pop()
                .unwrap()["success"],
            false
        );
    }
    process.send(json!({"id":"kept","type":"get_messages"}));
    assert_eq!(
        process
            .until(RECORD_WAIT, |value| value["id"] == "kept")
            .pop()
            .unwrap()["data"]["messages"],
        json!(session.context.messages)
    );
    process.shutdown();
}
