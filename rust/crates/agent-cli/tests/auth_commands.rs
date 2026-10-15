use agent_cli::args::Args;
use agent_cli::auth_commands::{auth_store_for, execute_auth_command_with};
use agent_cli::client::{settings_for, Client};
use agent_core::types::{timestamp, CancellationToken};
use agent_runtime::auth::{CodexOAuthClient, CodexOAuthConfig, CODEX_CLIENT_ID};
use agent_runtime::provider::{ProviderMode, ProviderOptions, ProviderSettings};
use serde_json::{json, Value};
use std::fs;
use std::io::{BufRead, Read, Write};
use std::net::{TcpListener, TcpStream};
#[cfg(target_os = "windows")]
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

struct Temp(PathBuf);
struct RpcInput {
    receiver: mpsc::Receiver<Vec<u8>>,
    buffer: Vec<u8>,
    position: usize,
}
impl BufRead for RpcInput {
    fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
        if self.position == self.buffer.len() {
            self.buffer = self.receiver.recv().unwrap_or_default();
            self.position = 0;
        }
        Ok(&self.buffer[self.position..])
    }
    fn consume(&mut self, size: usize) {
        self.position = (self.position + size).min(self.buffer.len());
    }
}
impl Read for RpcInput {
    fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
        let buffer = self.fill_buf()?;
        let size = buffer.len().min(output.len());
        output[..size].copy_from_slice(&buffer[..size]);
        self.consume(size);
        Ok(size)
    }
}

fn cancelled_rpc_refresh(eof: bool, compaction: bool) {
    let temp = Temp::new();
    let mut expired = credential();
    expired["expires"] = json!(0);
    let store = auth_store_for(&temp.0);
    store
        .write("openai-codex", expired.clone(), &CancellationToken::new())
        .unwrap();
    let before = fs::read(store.path()).unwrap();
    let args = Args {
        cwd: Some(temp.0.clone()),
        auth_dir: Some(temp.0.clone()),
        session_dir: Some(temp.0.join("sessions")),
        no_skills: true,
        no_prompts: true,
        no_context_files: true,
        ..Default::default()
    };
    let mut client = Client::from_args(&args).unwrap();
    if compaction {
        for text in ["first", "second"] {
            client
                .session
                .append_message(agent_core::types::Message::user(text))
                .unwrap();
            client
                .session
                .append_message(agent_core::types::Message::Assistant(
                    agent_core::types::AssistantMessage::text(format!("answer {text}")),
                ))
                .unwrap();
        }
    }
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let (accepted_sender, accepted) = mpsc::channel();
    let server = thread::spawn(move || {
        let start = Instant::now();
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        && start.elapsed() < Duration::from_secs(3) =>
                {
                    thread::sleep(Duration::from_millis(5))
                }
                Err(error) => panic!("OAuth fixture accept: {error}"),
            }
        };
        stream.set_nonblocking(false).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        let mut bytes = Vec::new();
        let mut expected = None;
        while expected.is_none_or(|expected| bytes.len() < expected) {
            let mut buffer = [0u8; 4096];
            match stream.read(&mut buffer) {
                Ok(0) => panic!("Incomplete OAuth fixture request"),
                Ok(size) => bytes.extend_from_slice(&buffer[..size]),
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) => {}
                Err(error) => panic!("{error}"),
            };
            if expected.is_none() {
                if let Some(end) = bytes.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                    let header = std::str::from_utf8(&bytes[..end]).unwrap();
                    let length = header
                        .lines()
                        .filter_map(|line| line.split_once(':'))
                        .find(|(key, _)| key.eq_ignore_ascii_case("content-length"))
                        .unwrap()
                        .1
                        .trim()
                        .parse::<usize>()
                        .unwrap();
                    expected = Some(end + 4 + length);
                }
            }
            assert!(start.elapsed() < Duration::from_secs(3));
        }
        assert!(String::from_utf8_lossy(&bytes).contains("grant_type=refresh_token"));
        accepted_sender.send(()).unwrap();
        let start = Instant::now();
        loop {
            let mut byte = [0u8];
            match stream.read(&mut byte) {
                Ok(0) => break,
                Ok(_) => panic!("Unexpected OAuth bytes"),
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                    ) => {}
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::ConnectionAborted | std::io::ErrorKind::ConnectionReset
                    ) =>
                {
                    break
                }
                Err(error) => panic!("{error}"),
            };
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "Cancelled refresh did not close socket"
            );
        }
    });
    client.provider_id = "openai-codex".into();
    client.settings = ProviderSettings {
        mode: ProviderMode::OpenAiCodex,
        base_url: "http://127.0.0.1:1/backend-api".into(),
        model: "offline-model".into(),
        api_key: String::new(),
        options: ProviderOptions {
            bedrock: Default::default(),
            azure: Default::default(),
            vertex: Default::default(),
            provider_id: "openai-codex".into(),
            oauth: Some(serde_json::from_value(expired).unwrap()),
            auth_storage_path: Some(store.path().to_owned()),
            codex_oauth: CodexOAuthConfig {
                token_url: format!("http://{address}/oauth/token"),
                ..Default::default()
            },
            ..Default::default()
        },
    };
    let (input_sender, input) = mpsc::channel();
    let (output_sender, output) = mpsc::channel();
    let (result_sender, result) = mpsc::channel();
    let worker = thread::spawn(move || {
        result_sender
            .send(agent_cli::rpc::run_rpc(
                RpcInput {
                    receiver: input,
                    buffer: Vec::new(),
                    position: 0,
                },
                &mut RecordOutput {
                    sender: output_sender,
                    buffer: Vec::new(),
                },
                client,
                &CancellationToken::new(),
            ))
            .unwrap()
    });
    let command = if compaction {
        json!({"id":"run","type":"compact","keepRecentUserTurns":1})
    } else {
        json!({"id":"run","type":"prompt","message":"expired refresh fixture"})
    };
    input_sender
        .send(format!("{command}\n").into_bytes())
        .unwrap();
    accepted.recv_timeout(Duration::from_secs(3)).unwrap();
    let start = Instant::now();
    let mut input_sender = Some(input_sender);
    if eof {
        input_sender.take();
    } else {
        input_sender
            .as_ref()
            .unwrap()
            .send(b"{\"id\":\"abort\",\"type\":\"abort\"}\n".to_vec())
            .unwrap();
    }
    let mut records = Vec::new();
    loop {
        let record = output
            .recv_timeout(Duration::from_secs(2).saturating_sub(start.elapsed()))
            .unwrap();
        let settled = record["type"] == "agent_settled";
        records.push(record);
        if settled {
            break;
        }
    }
    assert!(start.elapsed() < Duration::from_secs(2));
    assert_eq!(records.last().unwrap()["success"], false);
    if !eof {
        assert!(records
            .iter()
            .any(|record| record["id"] == "abort" && record["success"] == true));
        input_sender
            .as_ref()
            .unwrap()
            .send(b"{\"id\":\"shutdown\",\"type\":\"shutdown\"}\n".to_vec())
            .unwrap();
        input_sender.take();
    }
    result
        .recv_timeout(Duration::from_secs(2))
        .unwrap()
        .unwrap();
    worker.join().unwrap();
    server.join().unwrap();
    assert_eq!(fs::read(store.path()).unwrap(), before);
    assert_eq!(
        store
            .read("openai-codex", &CancellationToken::new())
            .unwrap()
            .unwrap()["refresh"],
        "offline-command-refresh"
    );
    let sessions = agent_runtime::session::SessionStore::new(temp.0.join("sessions"))
        .list()
        .unwrap();
    assert_eq!(sessions.len(), 1);
    assert!(!serde_json::to_string(&sessions)
        .unwrap()
        .contains("offline-command-refresh"));
    if compaction {
        assert_eq!(sessions[0].context.messages.len(), 4);
    } else {
        assert_eq!(sessions[0].context.messages.len(), 2);
        assert!(
            matches!(sessions[0].context.messages.last(),Some(agent_core::types::Message::Assistant(message)) if message.stop_reason==agent_core::types::StopReason::Aborted)
        );
    }
}

#[test]
fn rpc_abort_cancels_expired_oauth_refresh_and_settles_without_rotating_credentials() {
    cancelled_rpc_refresh(false, false);
}
#[test]
fn rpc_eof_cancels_expired_oauth_refresh_and_closes_the_token_socket() {
    cancelled_rpc_refresh(true, false);
}
#[test]
fn rpc_compaction_abort_cancels_expired_oauth_refresh_and_preserves_history() {
    cancelled_rpc_refresh(false, true);
}

fn manual_compaction_fixture(fail_ledger: bool) {
    let temp = Temp::new();
    let args = Args {
        cwd: Some(temp.0.clone()),
        auth_dir: Some(temp.0.clone()),
        session_dir: Some(temp.0.join("sessions")),
        no_skills: true,
        no_prompts: true,
        no_context_files: true,
        ..Default::default()
    };
    let mut client = Client::from_args(&args).unwrap();
    client
        .choose_model("anthropic", "claude-sonnet-4-6", &CancellationToken::new())
        .unwrap();
    // Model selection is its own committed identity-only transaction. A failed
    // compaction must leave that existing file byte-for-byte unchanged.
    let session_path = temp
        .0
        .join("sessions")
        .join(format!("{}.json", client.session.id));
    let saved_before = fs::read(&session_path).unwrap();
    for text in ["first archived turn", "second kept turn"] {
        client
            .session
            .append_message(agent_core::types::Message::user(text))
            .unwrap();
        client
            .session
            .append_message(agent_core::types::Message::Assistant(
                agent_core::types::AssistantMessage::text(format!("answer {text}")),
            ))
            .unwrap();
    }
    if fail_ledger {
        client
            .session
            .record_runtime_state("automation", json!({"requestCount":"invalid-fixture"}))
            .unwrap();
    }
    let before = serde_json::to_value(&client.session).unwrap();
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let address = listener.local_addr().unwrap();
    client.settings.base_url = format!("http://{address}/v1");
    client.settings.api_key = "offline-summary-fixture".into();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut bytes = Vec::new();
        while !bytes.ends_with(b"\r\n\r\n") {
            let mut byte = [0u8];
            stream.read_exact(&mut byte).unwrap();
            bytes.push(byte[0]);
        }
        let headers = String::from_utf8(bytes).unwrap();
        let size = headers
            .lines()
            .filter_map(|line| line.split_once(':'))
            .find(|(key, _)| key.eq_ignore_ascii_case("content-length"))
            .unwrap()
            .1
            .trim()
            .parse::<usize>()
            .unwrap();
        let mut bytes = vec![0u8; size];
        stream.read_exact(&mut bytes).unwrap();
        let request: Value = serde_json::from_slice(&bytes).unwrap();
        let response=[json!({"type":"message_start","message":{"usage":{"input_tokens":120}}}),json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":"离线上下文摘要"}}),json!({"type":"content_block_stop","index":0}),json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":7}}),json!({"type":"message_stop"})].into_iter().map(|value|format!("data: {value}\n\n")).collect::<String>();
        write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",response.len()).unwrap();
        request
    });
    let result = client.compact(1, &CancellationToken::new());
    let request = server.join().unwrap();
    assert!(request.get("tools").is_none());
    if fail_ledger {
        assert!(result.unwrap_err().contains("Automatic runtime state"));
        assert_eq!(serde_json::to_value(&client.session).unwrap(), before);
        assert_eq!(
            client
                .options
                .automation_state
                .snapshot()
                .state
                .native_compaction_count,
            0
        );
        assert_eq!(fs::read(&session_path).unwrap(), saved_before);
        let saved = agent_runtime::session::SessionStore::new(temp.0.join("sessions"))
            .list()
            .unwrap();
        assert_eq!(saved.len(), 1);
        assert!(saved[0].context.messages.is_empty());
        assert!(saved[0].compactions.is_empty());
        return;
    }
    let result = result.unwrap();
    assert_eq!(result.summary, "离线上下文摘要");
    assert_eq!(result.usage.as_ref().unwrap().input, 120);
    assert_eq!(result.usage.as_ref().unwrap().output, 7);
    assert_eq!(
        client
            .options
            .automation_state
            .snapshot()
            .state
            .native_compaction_count,
        1
    );
    assert!(client
        .session
        .runtime_state("automation")
        .unwrap()
        .is_some());
    let reopened = agent_runtime::session::SessionStore::new(temp.0.join("sessions"))
        .list()
        .unwrap();
    assert_eq!(reopened.len(), 1);
    assert_eq!(reopened[0].compactions[0].usage, result.usage);
    assert!(reopened[0].runtime_state("automation").unwrap().is_some());
    assert_eq!(reopened[0].context.messages.len(), 3);
}
#[test]
fn manual_client_compaction_uses_one_summary_request_and_persists_the_shared_ledger() {
    manual_compaction_fixture(false);
}
#[test]
fn manual_compaction_ledger_failure_restores_the_exact_original_session() {
    manual_compaction_fixture(true);
}
#[test]
fn executable_dispatches_auth_before_prompt_argument_parsing() {
    let temp = Temp::new();
    auth_store_for(&temp.0)
        .write(
            "fixture",
            json!({"type":"api_key","key":"not-printed-fixture"}),
            &CancellationToken::new(),
        )
        .unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_agent-core"));
    command.args(temp.args(&["auth", "status"]));
    #[cfg(target_os = "windows")]
    command.creation_flags(0x0800_0000);
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["credentials"][0]["providerId"], "fixture");
    assert!(!String::from_utf8_lossy(&output.stdout).contains("not-printed-fixture"));
    assert!(!temp.0.join("sessions").exists());
}
impl Temp {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "cli-auth-{}",
            agent_runtime::session::Session::default().id
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn args(&self, words: &[&str]) -> Vec<String> {
        words
            .iter()
            .map(|word| word.to_string())
            .chain(["--auth-dir".into(), self.0.to_string_lossy().into_owned()])
            .collect()
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn credential() -> Value {
    let access="e30.eyJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnsiY2hhdGdwdF9hY2NvdW50X2lkIjoiZml4dHVyZS1hY2NvdW50In19.fixture";
    json!({"type":"oauth","access":access,"refresh":"offline-command-refresh","expires":timestamp()+3_600_000,"accountId":"fixture-account"})
}

fn percent_decode(value: &str) -> String {
    let mut output = Vec::new();
    let mut bytes = value.bytes();
    while let Some(byte) = bytes.next() {
        match byte {
            b'%' => {
                let high = (bytes.next().unwrap() as char).to_digit(16).unwrap();
                let low = (bytes.next().unwrap() as char).to_digit(16).unwrap();
                output.push((high * 16 + low) as u8);
            }
            b'+' => output.push(b' '),
            _ => output.push(byte),
        }
    }
    String::from_utf8(output).unwrap()
}

#[test]
fn auth_status_and_logout_are_native_and_never_print_credentials() {
    let temp = Temp::new();
    let store = auth_store_for(&temp.0);
    let signal = CancellationToken::new();
    store.write("openai-codex", credential(), &signal).unwrap();
    store
        .write(
            "unknown",
            json!({"type":"future","key":"do-not-print"}),
            &signal,
        )
        .unwrap();
    let mut output = Vec::new();
    assert!(execute_auth_command_with(
        &temp.args(&["auth", "status"]),
        &CodexOAuthClient::default(),
        &signal,
        &mut output
    )
    .unwrap());
    let text = String::from_utf8(output).unwrap();
    assert!(text.contains("openai-codex"));
    assert!(text.contains("unknown"));
    assert!(!text.contains("offline-command-refresh"));
    assert!(!text.contains("do-not-print"));
    assert!(!text.contains("fixture-account"));
    let mut output = Vec::new();
    execute_auth_command_with(
        &temp.args(&["auth", "status", "unknown"]),
        &CodexOAuthClient::default(),
        &signal,
        &mut output,
    )
    .unwrap();
    let result: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(result["credentials"].as_array().unwrap().len(), 1);
    assert_eq!(result["credentials"][0]["providerId"], "unknown");
    execute_auth_command_with(
        &temp.args(&["auth", "logout", "openai-codex"]),
        &CodexOAuthClient::default(),
        &signal,
        &mut Vec::new(),
    )
    .unwrap();
    assert!(store.read("openai-codex", &signal).unwrap().is_none());
    assert!(store.read("unknown", &signal).unwrap().is_some());
}

#[test]
fn auth_import_preserves_unknown_legacy_entries_without_editing_the_source() {
    let temp = Temp::new();
    let path = temp.0.join("legacy.json");
    let source =
        json!({"openai-codex":credential(),"unknown":{"type":"future","value":[1,2]}}).to_string();
    fs::write(&path, &source).unwrap();
    let words = vec![
        "auth".into(),
        "import".into(),
        path.to_string_lossy().into_owned(),
        "--auth-dir".into(),
        temp.0.to_string_lossy().into_owned(),
    ];
    let mut output = Vec::new();
    execute_auth_command_with(
        &words,
        &CodexOAuthClient::default(),
        &CancellationToken::new(),
        &mut output,
    )
    .unwrap();
    assert_eq!(fs::read_to_string(path).unwrap(), source);
    let result: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(result["report"]["imported"].as_array().unwrap().len(), 2);
    assert_eq!(
        auth_store_for(&temp.0)
            .read("unknown", &CancellationToken::new())
            .unwrap()
            .unwrap()["value"],
        json!([1, 2])
    );
}

#[test]
fn auth_invalid_commands_and_passthrough_do_not_create_credential_files() {
    let temp = Temp::new();
    for words in [
        &["auth", "login", "fixture-unknown-provider"][..],
        &["auth", "unknown"][..],
        &["auth", "status", "x", "extra"][..],
    ] {
        assert!(execute_auth_command_with(
            &temp.args(words),
            &CodexOAuthClient::default(),
            &CancellationToken::new(),
            &mut Vec::new()
        )
        .is_err());
    }
    assert!(!temp.0.join("credentials.dat").exists());
    assert!(!execute_auth_command_with(
        &["--print".into(), "fixture".into()],
        &CodexOAuthClient::default(),
        &CancellationToken::new(),
        &mut Vec::new()
    )
    .unwrap());
}

struct RecordOutput {
    sender: mpsc::Sender<Value>,
    buffer: Vec<u8>,
}
impl Write for RecordOutput {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.buffer.extend_from_slice(bytes);
        while let Some(end) = self.buffer.iter().position(|byte| *byte == b'\n') {
            let line: Vec<_> = self.buffer.drain(..=end).collect();
            self.sender
                .send(serde_json::from_slice(&line).unwrap())
                .unwrap();
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn auth_login_real_callback_and_token_exchange_persist_dpapi_without_printing_tokens() {
    let temp = Temp::new();
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let token = credential();
    let response =
        json!({"access_token":token["access"],"refresh_token":token["refresh"],"expires_in":3600})
            .to_string();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut headers = Vec::new();
        while !headers.ends_with(b"\r\n\r\n") {
            let mut byte = [0u8];
            stream.read_exact(&mut byte).unwrap();
            headers.push(byte[0]);
        }
        let header = String::from_utf8(headers).unwrap();
        let size = header
            .lines()
            .filter_map(|line| line.split_once(':'))
            .find(|(key, _)| key.eq_ignore_ascii_case("content-length"))
            .unwrap()
            .1
            .trim()
            .parse::<usize>()
            .unwrap();
        let mut body = vec![0u8; size];
        stream.read_exact(&mut body).unwrap();
        write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",response.len()).unwrap();
        String::from_utf8(body).unwrap()
    });
    let client = CodexOAuthClient::new(CodexOAuthConfig {
        authorize_url: format!("http://{address}/oauth/authorize"),
        token_url: format!("http://{address}/oauth/token"),
        callback_bind: "127.0.0.1:0".parse().unwrap(),
        redirect_uri: "http://127.0.0.1:0/auth/callback".into(),
        ..Default::default()
    });
    let args = temp.args(&["auth", "login", "openai-codex"]);
    let (sender, records) = mpsc::channel();
    let worker = thread::spawn(move || {
        execute_auth_command_with(
            &args,
            &client,
            &CancellationToken::new(),
            &mut RecordOutput {
                sender,
                buffer: Vec::new(),
            },
        )
    });
    let event = records.recv_timeout(Duration::from_secs(3)).unwrap();
    assert_eq!(event["type"], "auth_url");
    let params: std::collections::BTreeMap<_, _> = event["url"]
        .as_str()
        .unwrap()
        .split_once('?')
        .unwrap()
        .1
        .split('&')
        .map(|pair| {
            let (key, value) = pair.split_once('=').unwrap();
            (percent_decode(key), percent_decode(value))
        })
        .collect();
    assert_eq!(params["client_id"], CODEX_CLIENT_ID);
    let redirect = params["redirect_uri"].strip_prefix("http://").unwrap();
    let (address, path) = redirect.split_once('/').unwrap();
    let mut stream = TcpStream::connect(address).unwrap();
    write!(
        stream,
        "GET /{path}?code=offline-code&state={} HTTP/1.1\r\nHost: localhost\r\n\r\n",
        params["state"]
    )
    .unwrap();
    let mut callback = String::new();
    stream.read_to_string(&mut callback).unwrap();
    assert!(callback.starts_with("HTTP/1.1 200"));
    assert!(worker.join().unwrap().unwrap());
    let complete = records.recv_timeout(Duration::from_secs(3)).unwrap();
    assert_eq!(complete["type"], "auth_complete");
    assert!(!complete.to_string().contains("offline-command-refresh"));
    assert!(!complete.to_string().contains("fixture-account"));
    assert!(server
        .join()
        .unwrap()
        .contains("grant_type=authorization_code"));
    assert_eq!(
        auth_store_for(&temp.0)
            .read("openai-codex", &CancellationToken::new())
            .unwrap()
            .unwrap()["refresh"],
        "offline-command-refresh"
    );
}

#[test]
fn client_model_selection_binds_exact_provider_catalog_budget_and_native_oauth_store() {
    let temp = Temp::new();
    let store = auth_store_for(&temp.0);
    store
        .write("openai-codex", credential(), &CancellationToken::new())
        .unwrap();
    let args = Args {
        cwd: Some(temp.0.clone()),
        auth_dir: Some(temp.0.clone()),
        no_session: true,
        no_skills: true,
        no_prompts: true,
        no_context_files: true,
        ..Default::default()
    };
    let mut client = Client::from_args(&args).unwrap();
    assert!(!client.options.automation.enabled);
    client
        .choose_model("anthropic", "claude-sonnet-4-6", &CancellationToken::new())
        .unwrap();
    assert_eq!(client.settings.options.provider_id, "anthropic");
    let budget = client.options.automation.model.as_ref().unwrap();
    assert_eq!(
        budget.context_window,
        agent_runtime::catalog::find("anthropic", "claude-sonnet-4-6")
            .unwrap()
            .context_window
    );
    assert!(!client.options.automation.enabled);
    let model = agent_runtime::catalog::models()
        .iter()
        .find(|model| model.provider == "openai-codex")
        .unwrap();
    client
        .choose_model("openai-codex", &model.id, &CancellationToken::new())
        .unwrap();
    assert_eq!(client.settings.mode, ProviderMode::OpenAiCodex);
    assert_eq!(
        client.settings.options.auth_storage_path.as_deref(),
        Some(store.path())
    );
    assert!(client.settings.options.oauth.is_some());
    assert_eq!(client.settings.options.provider_id, "openai-codex");
    assert_eq!(
        client.options.automation.model.as_ref().unwrap().id,
        model.id
    );
    assert_eq!(
        settings_for(
            "deepseek",
            &agent_runtime::catalog::models()
                .iter()
                .find(|model| model.provider == "deepseek")
                .unwrap()
                .id
        )
        .unwrap()
        .options
        .provider_id,
        "deepseek"
    );
}

#[test]
fn client_unknown_budget_enabled_selection_and_missing_oauth_fail_without_credential_side_effects()
{
    let temp = Temp::new();
    let args = Args {
        cwd: Some(temp.0.clone()),
        auth_dir: Some(temp.0.clone()),
        no_session: true,
        no_skills: true,
        no_prompts: true,
        no_context_files: true,
        ..Default::default()
    };
    let mut client = Client::from_args(&args).unwrap();
    client.options.automation.enabled = true;
    assert!(client
        .choose_model(
            "openai-compatible",
            "fixture-unknown-model",
            &CancellationToken::new()
        )
        .unwrap_err()
        .contains("metadata"));
    assert_eq!(client.provider_id, "demo");
    client.options.automation.enabled = false;
    assert!(client
        .choose_model("openai-codex", "fixture", &CancellationToken::new())
        .unwrap_err()
        .contains("login"));
    assert_eq!(client.provider_id, "demo");
    assert!(!temp.0.join("credentials.dat").exists());
}
