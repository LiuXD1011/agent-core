//! Real HTTP transport fixtures; every socket is bound to loopback on a random port.

use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use agent_core::agent_loop::{run_agent_loop, LoopConfig, NoopLoopHooks, Provider};
use agent_core::tools::NoopToolHooks;
use agent_core::types::{timestamp, CancellationToken, Context, Event, Message, StopReason};
use agent_runtime::builtins::{create_tools, tool_definitions};
use agent_runtime::provider::{ModelProvider, ProviderMode, ProviderSettings};
use serde_json::{json, Value};

const FIXTURE_KEY: &str = "offline-fixture-key-not-a-credential";
const FIXTURE_MODEL: &str = "offline-fixture-model";
const IO_DEADLINE: Duration = Duration::from_secs(5);

struct ObservedRequest {
    method: String,
    path: String,
    headers: BTreeMap<String, String>,
    body: Value,
}

struct TemporaryWorkspace(PathBuf);

impl TemporaryWorkspace {
    fn new() -> Self {
        static NEXT_ID: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "agent-desktop-http-{}-{}-{}",
            std::process::id(),
            timestamp(),
            NEXT_ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        fs::write(path.join("sample.txt"), "离线中文样本\n").unwrap();
        Self(path)
    }
}

impl Drop for TemporaryWorkspace {
    fn drop(&mut self) {
        let _ = fs::remove_file(self.0.join("sample.txt"));
        let _ = fs::remove_dir(&self.0);
    }
}

fn listener() -> (TcpListener, SocketAddr) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    (listener, address)
}

fn accept(listener: &TcpListener) -> Result<TcpStream, String> {
    let start = Instant::now();
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                stream
                    .set_nonblocking(false)
                    .map_err(|error| error.to_string())?;
                stream
                    .set_read_timeout(Some(Duration::from_millis(100)))
                    .map_err(|error| error.to_string())?;
                stream
                    .set_write_timeout(Some(Duration::from_secs(2)))
                    .map_err(|error| error.to_string())?;
                return Ok(stream);
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if start.elapsed() >= IO_DEADLINE {
                    return Err("Fixture server timed out accepting a connection".to_owned());
                }
                thread::sleep(Duration::from_millis(5));
            }
            Err(error) => return Err(error.to_string()),
        }
    }
}

fn read_request(stream: &mut TcpStream) -> Result<ObservedRequest, String> {
    let start = Instant::now();
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 4096];
    let mut expected_size = None;
    let mut header_end = None;
    loop {
        if start.elapsed() >= IO_DEADLINE {
            return Err("Fixture server timed out reading a request".to_owned());
        }
        match stream.read(&mut buffer) {
            Ok(0) => return Err("Client closed an incomplete request".to_owned()),
            Ok(count) => bytes.extend_from_slice(&buffer[..count]),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                continue
            }
            Err(error) => return Err(error.to_string()),
        }
        if bytes.len() > 524_288 {
            return Err("Fixture request exceeds its bound".to_owned());
        }
        if header_end.is_none() {
            if let Some(index) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                let end = index + 4;
                let headers =
                    std::str::from_utf8(&bytes[..index]).map_err(|error| error.to_string())?;
                let content_length = headers
                    .lines()
                    .skip(1)
                    .filter_map(|line| line.split_once(':'))
                    .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                    .ok_or("Fixture request requires Content-Length")?
                    .1
                    .trim()
                    .parse::<usize>()
                    .map_err(|error| error.to_string())?;
                header_end = Some(end);
                expected_size = Some(end + content_length);
            }
        }
        if expected_size.is_some_and(|size| bytes.len() >= size) {
            break;
        }
    }
    let end = header_end.ok_or("Request headers missing")?;
    let size = expected_size.ok_or("Request length missing")?;
    let header_text = std::str::from_utf8(&bytes[..end - 4]).map_err(|error| error.to_string())?;
    let mut lines = header_text.lines();
    let mut request_line = lines
        .next()
        .ok_or("Request line missing")?
        .split_whitespace();
    let method = request_line
        .next()
        .ok_or("Request method missing")?
        .to_owned();
    let path = request_line
        .next()
        .ok_or("Request path missing")?
        .to_owned();
    let mut headers = BTreeMap::new();
    for line in lines {
        let (name, value) = line.split_once(':').ok_or("Malformed request header")?;
        headers.insert(name.to_ascii_lowercase(), value.trim().to_owned());
    }
    let body = serde_json::from_slice(&bytes[end..size]).map_err(|error| error.to_string())?;
    Ok(ObservedRequest {
        method,
        path,
        headers,
        body,
    })
}

fn write_response(stream: &mut TcpStream, status: &str, body: &str) -> Result<(), String> {
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: text/event-stream; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .map_err(|error| error.to_string())?;
    stream.flush().map_err(|error| error.to_string())
}

fn sse(frames: Vec<Value>) -> String {
    let mut body = frames
        .into_iter()
        .map(|frame| format!("data: {frame}\r\n\r\n"))
        .collect::<String>();
    body.push_str("data: [DONE]\r\n\r\n");
    body
}

fn provider(address: SocketAddr, tools: Vec<Value>) -> ModelProvider {
    ModelProvider {
        settings: ProviderSettings {
            mode: ProviderMode::OpenAiCompatible,
            base_url: format!("http://{address}/v1"),
            model: FIXTURE_MODEL.to_owned(),
            api_key: FIXTURE_KEY.to_owned(),
            options: Default::default(),
        },
        tool_definitions: tools,
    }
}

#[test]
fn loopback_http_executes_read_then_sends_paired_result_and_answers_in_chinese() {
    let workspace = TemporaryWorkspace::new();
    let tools = create_tools(&workspace.0, false, false);
    let definitions = tool_definitions(&tools);
    let (listener, address) = listener();
    let server = thread::spawn(move || -> Result<Vec<ObservedRequest>, String> {
        let responses = [
            sse(vec![
                json!({"choices":[{"delta":{"content":"正在读取文件。","tool_calls":[{"index":0,"id":"fixture-read-1","type":"function","function":{"name":"read","arguments":"{\"pa"}}]},"finish_reason":null}]}),
                json!({"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"th\":\"sample.txt\"}"}}]},"finish_reason":"tool_calls"}]}),
                json!({"choices":[],"usage":{"prompt_tokens":4,"completion_tokens":6,"total_tokens":10}}),
            ]),
            sse(vec![
                json!({"choices":[{"delta":{"content":"文件内容是："},"finish_reason":null}]}),
                json!({"choices":[{"delta":{"content":"离线中文样本"},"finish_reason":"stop"}]}),
            ]),
        ];
        let mut requests = Vec::new();
        for response in responses {
            let mut stream = accept(&listener)?;
            requests.push(read_request(&mut stream)?);
            write_response(&mut stream, "200 OK", &response)?;
        }
        Ok(requests)
    });
    let mut provider = provider(address, definitions.clone());
    let mut events = Vec::new();
    let messages = run_agent_loop(
        vec![Message::user("读取 sample.txt 并用中文回答")],
        &Context {
            system_prompt: "离线测试系统提示".to_owned(),
            messages: Vec::new(),
        },
        LoopConfig::default(),
        &mut provider,
        &tools,
        &mut NoopLoopHooks,
        &NoopToolHooks,
        &CancellationToken::new(),
        &mut |event| events.push(event),
    )
    .unwrap();
    let requests = server.join().unwrap().unwrap();
    assert_eq!(requests.len(), 2);
    for request in &requests {
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/v1/chat/completions");
        assert_eq!(
            request.headers["authorization"],
            format!("Bearer {FIXTURE_KEY}")
        );
        assert!(request.headers["content-type"].starts_with("application/json"));
        assert_eq!(request.body["model"], FIXTURE_MODEL);
        assert_eq!(request.body["stream"], true);
        assert_eq!(request.body["tools"], json!(definitions));
    }
    assert_eq!(requests[0].body["messages"][0]["role"], "system");
    assert_eq!(requests[0].body["messages"][1]["role"], "user");
    let second = requests[1].body["messages"].as_array().unwrap();
    assert_eq!(second.len(), 4);
    assert_eq!(second[2]["role"], "assistant");
    assert_eq!(second[2]["tool_calls"][0]["id"], "fixture-read-1");
    assert_eq!(second[2]["tool_calls"][0]["function"]["name"], "read");
    let arguments: Value = serde_json::from_str(
        second[2]["tool_calls"][0]["function"]["arguments"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(arguments, json!({"path":"sample.txt"}));
    assert_eq!(second[3]["role"], "tool");
    assert_eq!(second[3]["tool_call_id"], "fixture-read-1");
    assert_eq!(second[3]["content"], "离线中文样本\n");
    assert_eq!(
        messages.iter().map(Message::role).collect::<Vec<_>>(),
        ["user", "assistant", "toolResult", "assistant"]
    );
    assert!(
        matches!(&messages[2], Message::ToolResult(result) if !result.is_error && result.tool_call_id == "fixture-read-1")
    );
    assert_eq!(messages.last().unwrap().text(), "文件内容是：离线中文样本");
    assert!(
        matches!(messages.last(), Some(Message::Assistant(message)) if message.stop_reason == StopReason::Stop)
    );
    if let Some(Message::Assistant(message)) = messages.last() {
        let identity = message.protocol_metadata.as_ref().unwrap();
        assert_eq!(identity["protocol"], "openai-completions");
        assert_eq!(identity["provider"], "openai");
        assert_eq!(identity["model"], FIXTURE_MODEL);
        assert_eq!(
            identity["route"],
            format!("http://{address}/v1/chat/completions")
        );
    }
    assert!(events
        .iter()
        .any(|event| matches!(event, Event::MessageUpdate { .. })));
    assert!(matches!(events.last(), Some(Event::AgentEnd { .. })));
}

#[test]
fn cancellation_interrupts_a_request_before_response_headers_arrive() {
    let (listener, address) = listener();
    let (accepted_sender, accepted_receiver) = mpsc::channel();
    let server = thread::spawn(move || -> Result<(), String> {
        let mut stream = accept(&listener)?;
        let _request = read_request(&mut stream)?;
        accepted_sender
            .send(())
            .map_err(|error| error.to_string())?;
        let start = Instant::now();
        let mut buffer = [0_u8; 1024];
        // Deliberately send no headers. A dropped transport must close this socket.
        loop {
            if start.elapsed() >= IO_DEADLINE {
                return Err("Cancelled client did not close its socket".to_owned());
            }
            match stream.read(&mut buffer) {
                Ok(0) => return Ok(()),
                Ok(_) => {}
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) => {}
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted
                    ) =>
                {
                    return Ok(())
                }
                Err(error) => return Err(error.to_string()),
            }
        }
    });
    let signal = CancellationToken::new();
    let worker_signal = signal.clone();
    let (result_sender, result_receiver) = mpsc::channel();
    let worker = thread::spawn(move || {
        let result = provider(address, Vec::new()).complete(
            &Context {
                system_prompt: String::new(),
                messages: vec![Message::user("stall fixture")],
            },
            &worker_signal,
            &mut |_| {},
        );
        let _ = result_sender.send(result);
    });
    accepted_receiver.recv_timeout(IO_DEADLINE).unwrap();
    thread::sleep(Duration::from_millis(75));
    let cancelled_at = Instant::now();
    signal.cancel();
    let result = result_receiver.recv_timeout(Duration::from_secs(2));
    assert!(
        result.is_ok(),
        "Provider did not settle within 2 seconds of cancellation"
    );
    let error = result.unwrap().unwrap_err();
    assert!(
        error.to_ascii_lowercase().contains("aborted"),
        "Unexpected cancellation result: {error}"
    );
    assert!(cancelled_at.elapsed() < Duration::from_secs(2));
    assert!(!error.contains(FIXTURE_KEY));
    worker.join().unwrap();
    server.join().unwrap().unwrap();
}

#[test]
fn non_success_http_returns_status_without_exposing_the_fixture_key() {
    let (listener, address) = listener();
    let server = thread::spawn(move || -> Result<ObservedRequest, String> {
        let mut stream = accept(&listener)?;
        let request = read_request(&mut stream)?;
        // Even a server reflecting Authorization in its body must not expose it.
        write_response(
            &mut stream,
            "403 Forbidden",
            &format!("reflected {FIXTURE_KEY}"),
        )?;
        Ok(request)
    });
    let error = provider(address, Vec::new())
        .complete(
            &Context {
                system_prompt: String::new(),
                messages: vec![Message::user("error fixture")],
            },
            &CancellationToken::new(),
            &mut |_| {},
        )
        .unwrap_err();
    let request = server.join().unwrap().unwrap();
    assert_eq!(
        request.headers["authorization"],
        format!("Bearer {FIXTURE_KEY}")
    );
    assert!(error.contains("HTTP 403"), "Unexpected HTTP error: {error}");
    assert!(!error.contains(FIXTURE_KEY));
    assert!(!error.contains("reflected"));
}
