//! Offline RPC input-worker lifecycle and fresh Codex authentication regressions.
use agent_cli::args::Args;
use agent_cli::client::Client;
use agent_cli::rpc::run_rpc;
use agent_core::types::{timestamp, AssistantMessage, CancellationToken, Message, StopReason};
use agent_runtime::auth_storage::AuthStorage;
use agent_runtime::extensions::{
    CommandResult, Extension, ExtensionCommand, ExtensionContext, InputEvent, InputResult,
    InputSource,
};
use agent_runtime::provider::ProviderMode;
use agent_runtime::session::{Session, SessionStore};
use serde_json::{json, Value};
use std::fs;
use std::io::{self, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

fn wait_for(mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !predicate() {
        assert!(
            Instant::now() < deadline,
            "Offline fixture deadline elapsed"
        );
        thread::sleep(Duration::from_millis(2));
    }
}

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("rpc-input-edge-{}", Session::default().id));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn args(&self) -> Args {
        Args {
            cwd: Some(self.0.clone()),
            agent_dir: Some(self.0.join("agent")),
            auth_dir: Some(self.0.join("auth")),
            session_dir: Some(self.0.join("sessions")),
            no_context_files: true,
            no_skills: true,
            no_prompts: true,
            ..Default::default()
        }
    }
    fn client(&self, server: &Server, hook: Arc<Hook>) -> Client {
        let mut client = Client::from_args(&self.args()).unwrap();
        client.settings.mode = ProviderMode::OpenAiCompatible;
        client.settings.base_url = format!("http://{}/v1", server.address);
        client.settings.model = "fixture-model".into();
        client.settings.api_key = "fixture-key".into();
        client.settings.options.provider_id = "openai-compatible".into();
        client.options.extensions.register(hook).unwrap();
        client
    }
    fn codex_client(&self, server: &Server) -> Client {
        let args = Args {
            provider: Some("openai-codex".into()),
            model: Some("fixture-model".into()),
            api_key: Some(String::new()),
            base_url: Some(format!("http://{}/backend-api", server.address)),
            ..self.args()
        };
        let client = Client::from_args(&args).unwrap();
        assert!(client.settings.options.oauth.is_none());
        client
    }
    fn sessions(&self) -> Vec<Session> {
        SessionStore::new(self.0.join("sessions")).list().unwrap()
    }
    fn write_codex_credential(&self) -> Vec<u8> {
        let store = AuthStorage::new(self.0.join("auth/credentials.dat"));
        store
            .write(
                "openai-codex",
                json!({
                    "type":"oauth", "access":ACCESS, "refresh":"offline-refresh",
                    "expires":timestamp()+3_600_000,"accountId":"fixture-account"
                }),
                &CancellationToken::new(),
            )
            .unwrap();
        fs::read(store.path()).unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        assert!(self.0.starts_with(std::env::temp_dir()));
        let _ = fs::remove_dir_all(&self.0);
    }
}

const ACCESS: &str = "e30.eyJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnsiY2hhdGdwdF9hY2NvdW50X2lkIjoiZml4dHVyZS1hY2NvdW50In19.fixture";

#[derive(Default)]
struct Hook {
    entered: AtomicUsize,
    released: AtomicBool,
    cancelled: AtomicBool,
    handled: bool,
    handle_primary: bool,
    calls: Mutex<Vec<(String, ExtensionContext)>>,
}
impl Hook {
    fn ready() -> Arc<Self> {
        Arc::new(Self {
            released: AtomicBool::new(true),
            ..Default::default()
        })
    }
}
impl Extension for Hook {
    fn id(&self) -> &str {
        "input-edge"
    }
    fn input(
        &self,
        event: &InputEvent,
        context: &ExtensionContext,
        signal: &CancellationToken,
    ) -> Result<InputResult, String> {
        assert_eq!(event.source, InputSource::Rpc);
        self.calls
            .lock()
            .unwrap()
            .push((event.text.clone(), context.clone()));
        if event.streaming_behavior.is_some() {
            self.entered.fetch_add(1, Ordering::SeqCst);
            while !self.released.load(Ordering::SeqCst) {
                if signal.is_cancelled() {
                    self.cancelled.store(true, Ordering::SeqCst);
                    return Err("Offline input cancelled".into());
                }
                thread::sleep(Duration::from_millis(2));
            }
            if self.handled {
                return Ok(InputResult::Handled);
            }
            return Ok(InputResult::Transform {
                text: format!("prepared {}", event.text),
                images: None,
            });
        }
        Ok(if self.handle_primary {
            InputResult::Handled
        } else {
            InputResult::Continue
        })
    }
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
                "Offline closed RPC output",
            ));
        }
        self.pending.extend_from_slice(bytes);
        while let Some(end) = self.pending.iter().position(|byte| *byte == b'\n') {
            let frame: Vec<u8> = self.pending.drain(..=end).collect();
            self.sender
                .send(serde_json::from_slice(&frame).unwrap())
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
    records: Vec<Value>,
    broken: Arc<AtomicBool>,
    worker: Option<JoinHandle<Result<(), String>>>,
}
impl Rpc {
    fn new(client: Client) -> Self {
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
                &CancellationToken::new(),
            )
        });
        Self {
            sender: Some(sender),
            receiver,
            records: vec![],
            broken,
            worker: Some(worker),
        }
    }
    fn send(&self, command: Value) {
        self.sender
            .as_ref()
            .unwrap()
            .send(format!("{command}\n").into_bytes())
            .unwrap();
    }
    fn until(&mut self, predicate: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let value = self
                .receiver
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .unwrap_or_else(|error| panic!("RPC fixture: {error}; records={:?}", self.records));
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
        let deadline = Instant::now() + Duration::from_secs(2);
        while !self.worker.as_ref().unwrap().is_finished() {
            assert!(
                Instant::now() < deadline,
                "RPC shutdown did not join within 2 seconds"
            );
            thread::sleep(Duration::from_millis(2));
        }
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

struct Server {
    address: SocketAddr,
    release: Arc<AtomicBool>,
    stopped: Arc<AtomicBool>,
    closed: Arc<AtomicUsize>,
    requests: Arc<Mutex<Vec<Vec<u8>>>>,
    worker: Option<JoinHandle<()>>,
}
impl Server {
    fn new(fail_first: bool, codex: bool) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let release = Arc::new(AtomicBool::new(false));
        let stopped = Arc::new(AtomicBool::new(false));
        let closed = Arc::new(AtomicUsize::new(0));
        let requests = Arc::new(Mutex::new(vec![]));
        let worker_release = release.clone();
        let worker_stopped = stopped.clone();
        let worker_closed = closed.clone();
        let worker_requests = requests.clone();
        let worker = thread::spawn(move || {
            while !worker_stopped.load(Ordering::SeqCst) {
                let mut stream = match listener.accept() {
                    Ok((stream, _)) => stream,
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                        continue;
                    }
                    Err(error) => panic!("Fixture accept: {error}"),
                };
                // Windows accepted sockets may inherit listener nonblocking mode.
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_millis(20)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let request = read_request(&mut stream);
                let index = {
                    let mut requests = worker_requests.lock().unwrap();
                    requests.push(request);
                    requests.len()
                };
                if index == 1 {
                    let mut disconnected = false;
                    while !worker_release.load(Ordering::SeqCst)
                        && !worker_stopped.load(Ordering::SeqCst)
                    {
                        match stream.read(&mut [0]) {
                            Ok(0) => {
                                worker_closed.fetch_add(1, Ordering::SeqCst);
                                disconnected = true;
                                break;
                            }
                            Err(error)
                                if matches!(
                                    error.kind(),
                                    io::ErrorKind::WouldBlock
                                        | io::ErrorKind::TimedOut
                                        | io::ErrorKind::Interrupted
                                ) => {}
                            Err(_) => {
                                worker_closed.fetch_add(1, Ordering::SeqCst);
                                disconnected = true;
                                break;
                            }
                            Ok(_) => panic!("Unexpected bytes after HTTP request"),
                        }
                    }
                    if disconnected || worker_stopped.load(Ordering::SeqCst) {
                        continue;
                    }
                }
                let (status, body) = if fail_first && index == 1 {
                    (
                        "500 Internal Server Error",
                        "{\"error\":\"offline first request failed\"}".into(),
                    )
                } else {
                    ("200 OK", sse(index, codex))
                };
                let response=format!("HTTP/1.1 {status}\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len());
                let _ = stream.write_all(response.as_bytes());
            }
        });
        Self {
            address,
            release,
            stopped,
            closed,
            requests,
            worker: Some(worker),
        }
    }
    fn count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
    fn wait_request(&self) {
        wait_for(|| self.count() > 0);
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::SeqCst);
        if let Some(worker) = self.worker.take() {
            worker.join().unwrap();
        }
    }
}
fn read_request(stream: &mut TcpStream) -> Vec<u8> {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut bytes = vec![];
    let mut expected = None;
    while expected.is_none_or(|expected| bytes.len() < expected) {
        assert!(Instant::now() < deadline, "HTTP request deadline");
        let mut buffer = [0; 4096];
        match stream.read(&mut buffer) {
            Ok(0) => panic!("HTTP client closed before request was complete"),
            Ok(size) => bytes.extend_from_slice(&buffer[..size]),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::Interrupted
                ) =>
            {
                continue
            }
            Err(error) => panic!("HTTP read: {error}"),
        }
        assert!(bytes.len() < 1_048_576);
        if expected.is_none() {
            if let Some(end) = bytes.windows(4).position(|value| value == b"\r\n\r\n") {
                let headers = String::from_utf8_lossy(&bytes[..end]);
                let length = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().unwrap())
                    })
                    .unwrap();
                expected = Some(end + 4 + length);
            }
        }
    }
    bytes
}
fn sse(index: usize, codex: bool) -> String {
    let text = format!("reply {index}");
    if codex {
        let item = json!({"id":"fixture-output","type":"message","role":"assistant","content":[{"type":"output_text","text":text}]});
        let events = [
            json!({"type":"response.output_item.added","output_index":0,"item":{"id":"fixture-output","type":"message","role":"assistant","content":[]}}),
            json!({"type":"response.output_text.delta","output_index":0,"item_id":"fixture-output","delta":text}),
            json!({"type":"response.completed","response":{"id":"fixture-response","status":"completed","output":[item],"usage":{"input_tokens":1,"output_tokens":2,"total_tokens":3}}}),
        ];
        return events
            .iter()
            .map(|event| format!("data: {event}\n\n"))
            .collect();
    }
    format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        json!({"choices":[{"delta":{"content":text},"finish_reason":null}]}),
        json!({"choices":[{"delta":{},"finish_reason":"stop"}]})
    )
}
fn start_primary(rpc: &mut Rpc, server: &Server) {
    rpc.send(json!({"type":"prompt","id":"first","message":"first"}));
    assert_eq!(rpc.response("first")["success"], true);
    server.wait_request();
}
fn busy_prompt(rpc: &Rpc, id: &str) {
    rpc.send(json!({"type":"prompt","id":id,"message":id,"streamingBehavior":"followUp"}));
}

#[test]
fn failed_model_cancels_late_preparation_and_does_not_restart_queued_input() {
    let fixture = Fixture::new();
    let server = Server::new(true, false);
    let hook = Arc::new(Hook::default());
    let mut rpc = Rpc::new(fixture.client(&server, hook.clone()));
    start_primary(&mut rpc, &server);
    busy_prompt(&rpc, "late");
    wait_for(|| hook.entered.load(Ordering::SeqCst) == 1);
    busy_prompt(&rpc, "pending");
    server.release.store(true, Ordering::SeqCst);
    let settled = rpc.settled();
    assert_eq!(settled["success"], false);
    assert!(
        settled["error"].as_str().unwrap().contains("500"),
        "{settled}"
    );
    assert!(hook.cancelled.load(Ordering::SeqCst));
    assert!(rpc
        .records
        .iter()
        .any(|value| value["id"] == "late" && value["success"] == false));
    assert!(rpc
        .records
        .iter()
        .any(|value| value["id"] == "pending" && value["success"] == false));
    assert_eq!(rpc.state()["pendingMessageCount"], 0);
    assert_eq!(server.count(), 1);
    rpc.finish();
    let sessions = fixture.sessions();
    assert_eq!(sessions[0].context.messages.len(), 2);
    assert_eq!(sessions[0].context.messages[0].text(), "first");
    assert_eq!(hook.calls.lock().unwrap().len(), 2);
}

#[test]
fn abort_clears_already_prepared_inputs_before_the_next_independent_prompt() {
    let fixture = Fixture::new();
    let server = Server::new(false, false);
    let hook = Hook::ready();
    let mut rpc = Rpc::new(fixture.client(&server, hook.clone()));
    start_primary(&mut rpc, &server);
    busy_prompt(&rpc, "discarded");
    assert_eq!(rpc.response("discarded")["success"], true);
    assert_eq!(rpc.state()["pendingMessageCount"], 1);
    let start = Instant::now();
    rpc.send(json!({"type":"abort","id":"abort"}));
    rpc.settled();
    assert!(start.elapsed() < Duration::from_secs(2));
    wait_for(|| server.closed.load(Ordering::SeqCst) == 1);
    assert_eq!(rpc.state()["pendingMessageCount"], 0);
    rpc.send(json!({"type":"prompt","id":"next","message":"next"}));
    assert_eq!(rpc.response("next")["success"], true);
    assert_eq!(rpc.settled()["success"], true);
    rpc.finish();
    assert_eq!(server.count(), 2);
    assert!(!fixture.sessions()[0]
        .context
        .messages
        .iter()
        .any(|message| message.text().contains("discarded")));
    assert_eq!(
        hook.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(text, _)| text == "discarded")
            .count(),
        1
    );
}

fn completed_request_waits_for_late_preparation(handled: bool) {
    let fixture = Fixture::new();
    let server = Server::new(false, false);
    let hook = Arc::new(Hook {
        handled,
        ..Default::default()
    });
    let mut rpc = Rpc::new(fixture.client(&server, hook.clone()));
    start_primary(&mut rpc, &server);
    busy_prompt(&rpc, "late");
    wait_for(|| hook.entered.load(Ordering::SeqCst) == 1);
    server.release.store(true, Ordering::SeqCst);
    wait_for(|| rpc.state()["isStreaming"] == false);
    assert_eq!(rpc.state()["isPreparingInput"], true);
    assert!(!rpc
        .records
        .iter()
        .any(|value| value["type"] == "agent_settled"));
    hook.released.store(true, Ordering::SeqCst);
    assert_eq!(rpc.settled()["success"], true);
    assert_eq!(
        rpc.records
            .iter()
            .filter(|value| value["type"] == "agent_settled")
            .count(),
        1
    );
    if handled {
        assert!(rpc
            .records
            .iter()
            .any(|value| value["type"] == "input_handled"));
    }
    assert_eq!(server.count(), if handled { 1 } else { 2 });
    rpc.finish();
    let sessions = fixture.sessions();
    assert_eq!(
        sessions[0].context.messages.len(),
        if handled { 2 } else { 4 }
    );
    let calls = hook.calls.lock().unwrap();
    assert_eq!(calls.iter().filter(|(text, _)| text == "late").count(), 1);
    assert!(calls[1]
        .1
        .context
        .messages
        .iter()
        .any(|message| message.text() == "first"));
}
#[test]
fn completed_request_waits_for_transformed_late_input_without_reapplying_hooks() {
    completed_request_waits_for_late_preparation(false);
}
#[test]
fn completed_request_waits_for_handled_late_input_without_fake_messages() {
    completed_request_waits_for_late_preparation(true);
}

#[test]
fn clear_queue_cancels_raw_preparation_and_pending_inputs_but_not_the_active_request() {
    let fixture = Fixture::new();
    let server = Server::new(false, false);
    let hook = Arc::new(Hook::default());
    let mut rpc = Rpc::new(fixture.client(&server, hook.clone()));
    start_primary(&mut rpc, &server);
    busy_prompt(&rpc, "late");
    wait_for(|| hook.entered.load(Ordering::SeqCst) == 1);
    busy_prompt(&rpc, "pending");
    rpc.send(json!({"type":"clear_queue","id":"clear"}));
    assert_eq!(rpc.response("clear")["success"], true);
    wait_for(|| rpc.state()["isPreparingInput"] == false);
    let state = rpc.state();
    assert_eq!(state["pendingMessageCount"], 0);
    assert_eq!(state["isStreaming"], true);
    assert_eq!(server.closed.load(Ordering::SeqCst), 0);
    server.release.store(true, Ordering::SeqCst);
    rpc.settled();
    rpc.finish();
    assert_eq!(server.count(), 1);
    assert_eq!(fixture.sessions()[0].context.messages.len(), 2);
}

fn end_with_parallel_preparation(output_loss: bool) {
    let fixture = Fixture::new();
    let server = Server::new(false, false);
    let hook = Arc::new(Hook::default());
    let mut rpc = Rpc::new(fixture.client(&server, hook.clone()));
    start_primary(&mut rpc, &server);
    busy_prompt(&rpc, "late");
    wait_for(|| hook.entered.load(Ordering::SeqCst) == 1);
    busy_prompt(&rpc, "pending");
    let start = Instant::now();
    if output_loss {
        rpc.broken.store(true, Ordering::SeqCst);
        rpc.send(json!({"type":"get_state","id":"broken"}));
        let error = rpc.join().unwrap_err();
        assert!(error.contains("RPC output failed"));
        rpc.sender.take();
    } else {
        rpc.sender.take();
        rpc.join().unwrap();
    }
    assert!(start.elapsed() < Duration::from_secs(2));
    wait_for(|| server.closed.load(Ordering::SeqCst) == 1);
    assert!(hook.cancelled.load(Ordering::SeqCst));
    assert_eq!(server.count(), 1);
    let sessions = fixture.sessions();
    assert_eq!(sessions[0].context.messages.len(), 2);
    assert_eq!(sessions[0].context.messages[0].text(), "first");
    let Message::Assistant(assistant) = &sessions[0].context.messages[1] else {
        panic!("Missing cancelled assistant")
    };
    assert_eq!(assistant.stop_reason, StopReason::Aborted);
}
#[test]
fn eof_cancels_both_workers_and_saves_only_the_active_request() {
    end_with_parallel_preparation(false);
}
#[test]
fn output_loss_cancels_both_workers_and_saves_only_the_active_request() {
    end_with_parallel_preparation(true);
}

#[test]
fn fresh_codex_prompt_loads_only_the_temporary_credential_store_after_input() {
    let fixture = Fixture::new();
    let before = fixture.write_codex_credential();
    let server = Server::new(false, true);
    server.release.store(true, Ordering::SeqCst);
    let mut rpc = Rpc::new(fixture.codex_client(&server));
    rpc.send(json!({"type":"prompt","id":"run","message":"fresh Codex"}));
    assert_eq!(rpc.response("run")["success"], true);
    assert_eq!(rpc.settled()["success"], true);
    rpc.finish();
    assert_eq!(server.count(), 1);
    let request = String::from_utf8(server.requests.lock().unwrap()[0].clone()).unwrap();
    assert!(request.starts_with("POST /backend-api/codex/responses "));
    assert!(request
        .to_lowercase()
        .contains("chatgpt-account-id: fixture-account"));
    assert!(request.contains(&format!("Bearer {ACCESS}")));
    assert_eq!(
        fs::read(fixture.0.join("auth/credentials.dat")).unwrap(),
        before
    );
    assert_eq!(fixture.sessions()[0].context.messages[1].text(), "reply 1");
}

#[test]
fn fresh_codex_compaction_loads_auth_in_the_worker_and_preserves_archived_turns() {
    let fixture = Fixture::new();
    let before = fixture.write_codex_credential();
    let server = Server::new(false, true);
    server.release.store(true, Ordering::SeqCst);
    let mut client = fixture.codex_client(&server);
    for text in ["old complete turn", "latest complete turn"] {
        client.session.append_message(Message::user(text)).unwrap();
        client
            .session
            .append_message(Message::Assistant(AssistantMessage::text(format!(
                "answer {text}"
            ))))
            .unwrap();
    }
    let mut rpc = Rpc::new(client);
    rpc.send(json!({"type":"compact","id":"compact","keepRecentUserTurns":1}));
    let response = rpc.response("compact");
    assert_eq!(response["success"], true, "{response}");
    assert_eq!(rpc.settled()["success"], true);
    rpc.finish();
    assert_eq!(server.count(), 1);
    assert_eq!(
        fs::read(fixture.0.join("auth/credentials.dat")).unwrap(),
        before
    );
    let sessions = fixture.sessions();
    assert_eq!(sessions[0].compactions.len(), 1);
    assert_eq!(sessions[0].compactions[0].summary, "reply 1");
    assert_eq!(
        sessions[0].compactions[0].archived_messages[0].text(),
        "old complete turn"
    );
    assert_eq!(
        sessions[0].context.messages.last().unwrap().text(),
        "answer latest complete turn"
    );
}

#[test]
fn handled_codex_input_skips_missing_auth_and_provider_validation() {
    let fixture = Fixture::new();
    let server = Server::new(false, true);
    let mut client = fixture.codex_client(&server);
    client.settings.model.clear();
    client
        .options
        .extensions
        .register(Arc::new(Hook {
            handle_primary: true,
            ..Default::default()
        }))
        .unwrap();
    let mut rpc = Rpc::new(client);
    rpc.send(json!({"type":"prompt","id":"handled","message":"handled"}));
    assert_eq!(rpc.settled()["success"], true);
    assert!(rpc
        .records
        .iter()
        .any(|value| value["type"] == "input_handled"));
    assert!(!rpc
        .records
        .iter()
        .any(|value| value["type"] == "agent_start" || value["type"] == "message_end"));
    rpc.finish();
    assert_eq!(server.count(), 0);
    assert!(!fixture.0.join("auth").exists());
    assert!(fixture.sessions()[0].context.messages.is_empty());
}

struct SlowCommand(Arc<AtomicBool>);
impl ExtensionCommand for SlowCommand {
    fn name(&self) -> &str {
        "slow_edge"
    }
    fn execute(
        &self,
        _: &str,
        _: &ExtensionContext,
        signal: &CancellationToken,
    ) -> Result<CommandResult, String> {
        self.0.store(true, Ordering::SeqCst);
        while !signal.is_cancelled() {
            thread::sleep(Duration::from_millis(2));
        }
        Err("Offline command cancelled".into())
    }
}
struct Commands(Arc<AtomicBool>);
impl Extension for Commands {
    fn id(&self) -> &str {
        "commands-edge"
    }
    fn commands(&self) -> Vec<Arc<dyn ExtensionCommand>> {
        vec![Arc::new(SlowCommand(self.0.clone()))]
    }
}
#[test]
fn busy_extension_and_compaction_reject_input_instead_of_preparing_stale_snapshots() {
    for compaction in [false, true] {
        let fixture = Fixture::new();
        let server = Server::new(false, false);
        let hook = Hook::ready();
        let entered = Arc::new(AtomicBool::new(false));
        let mut client = fixture.client(&server, hook.clone());
        client
            .options
            .extensions
            .register(Arc::new(Commands(entered.clone())))
            .unwrap();
        for text in ["old", "latest"] {
            client.session.append_message(Message::user(text)).unwrap();
            client
                .session
                .append_message(Message::Assistant(AssistantMessage::text(text)))
                .unwrap();
        }
        let mut rpc = Rpc::new(client);
        rpc.send(if compaction {
            json!({"type":"compact","id":"busy","keepRecentUserTurns":1})
        } else {
            json!({"type":"extension_command","id":"busy","name":"slow_edge"})
        });
        if compaction {
            server.wait_request();
        } else {
            wait_for(|| entered.load(Ordering::SeqCst));
        }
        for kind in ["steer", "follow_up", "prompt"] {
            rpc.send(json!({"type":kind,"id":kind,"message":"not accepted","streamingBehavior":"followUp"}));
            let response = rpc.response(kind);
            assert_eq!(response["success"], false);
            assert!(response["error"]
                .as_str()
                .unwrap()
                .contains("operation is running"));
        }
        assert!(hook.calls.lock().unwrap().is_empty());
        rpc.send(json!({"type":"abort","id":"abort"}));
        rpc.settled();
        rpc.finish();
        assert_eq!(fixture.sessions()[0].context.messages.len(), 4);
    }
}
