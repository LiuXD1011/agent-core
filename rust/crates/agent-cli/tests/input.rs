//! Offline native input pipeline, command priority, queue processing and cancellation.
use agent_cli::args::{parse, Args};
use agent_cli::client::Client;
use agent_cli::rpc::run_rpc;
use agent_core::types::{CancellationToken, Content, Message};
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
use std::sync::{mpsc, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

fn executable() -> &'static Path {
    static EXE: OnceLock<PathBuf> = OnceLock::new();
    EXE.get_or_init(|| {
        let dir =
            std::env::temp_dir().join(format!("native-input-compiler-{}", Session::default().id));
        fs::create_dir(&dir).unwrap();
        let output = dir.join(if cfg!(windows) {
            "fixture.exe"
        } else {
            "fixture"
        });
        let mut command = Command::new(std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into()));
        #[cfg(windows)]
        command.creation_flags(0x0800_0000);
        let result = command
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
    record: PathBuf,
}
impl Fixture {
    fn new(flags: &[&str]) -> Self {
        let path = std::env::temp_dir().join(format!("native-cli-input-{}", Session::default().id));
        fs::create_dir(&path).unwrap();
        let manifest = path.join("extension.json");
        let record = path.join("requests.jsonl");
        let mut args: Vec<String> = flags.iter().map(|v| (*v).into()).collect();
        args.push(format!("--record={}", record.display()));
        fs::write(&manifest,json!({"id":"fixture","executable":executable(),"args":args,"requestTimeoutMs":30000,"maxFrameBytes":1048576}).to_string()).unwrap();
        Self {
            path,
            manifest,
            record,
        }
    }
    fn args(&self) -> Vec<String> {
        vec![
            "--cwd".into(),
            self.path.to_string_lossy().into_owned(),
            "--agent-dir".into(),
            self.path.join("agent").to_string_lossy().into_owned(),
            "--session-dir".into(),
            self.path.join("sessions").to_string_lossy().into_owned(),
            "--extension".into(),
            self.manifest.to_string_lossy().into_owned(),
            "--no-context-files".into(),
            "--no-skills".into(),
        ]
    }
    fn client(&self) -> Client {
        Client::from_args(&parse(self.args()).unwrap()).unwrap()
    }
    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_agent-core"));
        command.args(self.args());
        #[cfg(windows)]
        command.creation_flags(0x0800_0000);
        command
    }
    fn requests(&self) -> Vec<Value> {
        let text = fs::read_to_string(&self.record).unwrap_or_default();
        let end = text.rfind('\n').map_or(0, |i| i + 1);
        text[..end]
            .lines()
            .map(|s| serde_json::from_str(s).unwrap())
            .collect()
    }
    fn wait_input(&self) {
        let until = Instant::now() + Duration::from_secs(10);
        while !self.requests().iter().any(|v| v["method"] == "input") {
            assert!(Instant::now() < until);
            thread::sleep(Duration::from_millis(2));
        }
    }
    fn sessions(&self) -> Vec<Session> {
        SessionStore::new(self.path.join("sessions"))
            .list()
            .unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        assert!(self.path.starts_with(std::env::temp_dir()));
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[test]
fn print_handled_skips_bad_provider_credentials_and_does_not_create_history() {
    let fixture = Fixture::new(&["--input-handled"]);
    let output = fixture
        .command()
        .args([
            "--mode",
            "json",
            "--provider",
            "bedrock",
            "--model",
            "fixture",
            "--aws-credentials-file",
        ])
        .arg(fixture.path.join("missing-credentials"))
        .arg("--aws-sso-cache-dir")
        .arg(fixture.path.join("missing SSO cache"))
        .args([
            "--aws-sso-endpoint",
            "not-a-url",
            "--aws-sso-oidc-endpoint",
            "not-an-oidc-url",
        ])
        .args(["-p", "handled raw"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().trim(),
        "{\"type\":\"input_handled\"}"
    );
    assert!(fixture.sessions().is_empty());
    let requests = fixture.requests();
    let input = requests.iter().find(|v| v["method"] == "input").unwrap();
    assert_eq!(input["params"]["event"]["text"], "handled raw");
    assert_eq!(input["params"]["event"]["source"], "interactive");
    assert!(!requests.iter().any(|v| v["method"] == "before_agent_start"));
}
#[test]
fn raw_input_transforms_before_templates_and_only_prepared_text_is_saved() {
    let fixture = Fixture::new(&["--input-prefix=/greet "]);
    let template = fixture.path.join("greet.md");
    fs::write(&template, "Expanded $1").unwrap();
    let output = fixture
        .command()
        .arg("--prompt-template")
        .arg(&template)
        .args(["-p", "Ada"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8(output.stdout)
        .unwrap()
        .contains("Expanded Ada"));
    assert_eq!(
        fixture.sessions()[0].context.messages[0].text(),
        "Expanded Ada"
    );
    let requests = fixture.requests();
    let inputs: Vec<_> = requests.iter().filter(|v| v["method"] == "input").collect();
    assert_eq!(inputs.len(), 1);
    assert_eq!(inputs[0]["params"]["event"]["text"], "Ada");
}
#[test]
fn registered_command_wins_over_input_handled_and_is_not_sent_to_provider() {
    let fixture = Fixture::new(&["--input-handled"]);
    let output = fixture
        .command()
        .args(["--mode", "json", "-p", "/fixture_echo literal"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["type"], "extension_command_result");
    assert!(!fixture.requests().iter().any(|v| v["method"] == "input"));
    assert_eq!(fixture.sessions()[0].context.messages.len(), 1);
}
#[test]
fn transformed_slash_does_not_execute_a_registered_command() {
    let fixture = Fixture::new(&["--input-prefix=/fixture_echo "]);
    let output = fixture.command().args(["-p", "literal"]).output().unwrap();
    assert!(output.status.success());
    assert!(!fixture.requests().iter().any(|v| v["method"] == "command"));
    assert_eq!(
        fixture.sessions()[0].context.messages[0].text(),
        "/fixture_echo literal"
    );
    assert_eq!(fixture.sessions()[0].context.messages.len(), 2);
}
#[test]
fn hook_error_is_diagnosed_and_original_input_continues() {
    let fixture = Fixture::new(&["--input-error"]);
    let output = fixture
        .command()
        .args(["-p", "original raw"])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(String::from_utf8(output.stderr).unwrap().contains("input"));
    assert_eq!(
        fixture.sessions()[0].context.messages[0].text(),
        "original raw"
    );
}
#[test]
fn transformed_images_are_preserved_and_cleared_without_fake_tool_blocks() {
    for clear in [false, true] {
        let fixture = Fixture::new(if clear {
            &["--input-prefix=x ", "--input-clear-images"]
        } else {
            &["--input-prefix=x "]
        });
        let mut client = fixture.client();
        let raw = Message::User(agent_core::types::UserMessage {
            timestamp: 1,
            content: vec![
                Content::text("raw"),
                Content::Image {
                    data: "aW1hZ2U=".into(),
                    mime_type: "image/png".into(),
                },
            ],
        });
        client
            .run(raw, &CancellationToken::new(), &mut |_| {})
            .unwrap();
        let Message::User(user) = &client.session.context.messages[0] else {
            panic!("Missing User")
        };
        assert_eq!(
            user.content
                .iter()
                .filter(|v| matches!(v, Content::Image { .. }))
                .count(),
            usize::from(!clear)
        );
        assert_eq!(user.timestamp, 1);
    }
}

struct ChannelRead {
    receiver: mpsc::Receiver<Vec<u8>>,
    pending: std::io::Cursor<Vec<u8>>,
}
impl Read for ChannelRead {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        loop {
            let n = self.pending.read(buffer)?;
            if n > 0 {
                return Ok(n);
            }
            match self.receiver.recv() {
                Ok(bytes) => self.pending = std::io::Cursor::new(bytes),
                Err(_) => return Ok(0),
            }
        }
    }
}
struct Sink {
    sender: mpsc::Sender<Value>,
    buffer: Vec<u8>,
}
impl Write for Sink {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.buffer.extend_from_slice(bytes);
        while let Some(i) = self.buffer.iter().position(|b| *b == b'\n') {
            let data: Vec<_> = self.buffer.drain(..=i).collect();
            self.sender
                .send(serde_json::from_slice(&data).unwrap())
                .map_err(|_| io::ErrorKind::BrokenPipe)?;
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
struct Rpc {
    sender: Option<mpsc::Sender<Vec<u8>>>,
    records: mpsc::Receiver<Value>,
    worker: Option<JoinHandle<Result<(), String>>>,
}
impl Rpc {
    fn new(client: Client) -> Self {
        let (sender, input) = mpsc::channel();
        let (out, records) = mpsc::channel();
        let worker = thread::spawn(move || {
            run_rpc(
                BufReader::new(ChannelRead {
                    receiver: input,
                    pending: std::io::Cursor::new(vec![]),
                }),
                &mut Sink {
                    sender: out,
                    buffer: vec![],
                },
                client,
                &CancellationToken::new(),
            )
        });
        Self {
            sender: Some(sender),
            records,
            worker: Some(worker),
        }
    }
    fn send(&self, value: Value) {
        self.sender
            .as_ref()
            .unwrap()
            .send(format!("{value}\n").into_bytes())
            .unwrap();
    }
    fn until(&self, predicate: impl Fn(&Value) -> bool) -> Vec<Value> {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut records = vec![];
        loop {
            let value = self
                .records
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .unwrap();
            let done = predicate(&value);
            records.push(value);
            if done {
                return records;
            }
        }
    }
    fn finish(&mut self) {
        self.send(json!({"type":"shutdown","id":"shutdown"}));
        self.until(|v| v["id"] == "shutdown");
        self.sender.take();
        self.worker.take().unwrap().join().unwrap().unwrap();
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
fn rpc_handled_has_no_agent_events_or_model_validation() {
    let fixture = Fixture::new(&["--input-handled"]);
    let mut client = fixture.client();
    client.settings.mode = ProviderMode::OpenAiCompatible;
    client.settings.model.clear();
    client.settings.api_key.clear();
    let mut rpc = Rpc::new(client);
    rpc.send(json!({"type":"prompt","id":"run","message":"handled"}));
    let records = rpc.until(|v| v["type"] == "agent_settled");
    assert!(records.iter().any(|v| v["type"] == "input_handled"));
    assert!(!records
        .iter()
        .any(|v| v["type"] == "message_end" || v["type"] == "agent_start"));
    rpc.finish();
    assert!(fixture.sessions()[0].context.messages.is_empty());
}
#[test]
fn rpc_busy_input_is_prepared_once_and_late_queue_uses_prepared_path() {
    let fixture = Fixture::new(&["--input-prefix=x ", "--slow-input-ms=120"]);
    let mut rpc = Rpc::new(fixture.client());
    rpc.send(json!({"type":"prompt","id":"first","message":"first"}));
    fixture.wait_input();
    rpc.send(
        json!({"type":"prompt","id":"second","message":"second","streamingBehavior":"followUp"}),
    );
    rpc.until(|v| {
        v["type"] == "message_end"
            && v["message"]["role"] == "assistant"
            && v["message"]["content"].as_array().is_some_and(|blocks| {
                blocks
                    .iter()
                    .any(|b| b["text"].as_str().is_some_and(|s| s.contains("x second")))
            })
    });
    rpc.until(|v| v["type"] == "agent_settled");
    rpc.finish();
    let messages = &fixture.sessions()[0].context.messages;
    assert_eq!(messages.iter().filter(|v| v.text() == "x first").count(), 1);
    assert_eq!(
        messages.iter().filter(|v| v.text() == "x second").count(),
        1
    );
    let requests = fixture.requests();
    let inputs: Vec<_> = requests.iter().filter(|v| v["method"] == "input").collect();
    assert_eq!(inputs.len(), 2);
    assert_eq!(
        inputs[1]["params"]["event"]["streamingBehavior"],
        "followUp"
    );
}
fn cancelled_input(eof: bool, queued: bool) {
    let fixture = Fixture::new(&["--hang-input"]);
    let mut rpc = Rpc::new(fixture.client());
    rpc.send(if queued {
        json!({"type":"steer","id":"input","message":"raw"})
    } else {
        json!({"type":"prompt","id":"input","message":"raw"})
    });
    fixture.wait_input();
    rpc.send(json!({"type":"get_state","id":"state"}));
    let records = rpc.until(|v| v["id"] == "state");
    assert!(records.last().unwrap()["success"].as_bool().unwrap());
    let start = Instant::now();
    if eof {
        rpc.sender.take();
    } else {
        rpc.send(json!({"type":"abort","id":"abort"}));
    }
    rpc.until(|v| v["type"] == "agent_settled");
    assert!(start.elapsed() < Duration::from_secs(2));
    if eof {
        rpc.worker.take().unwrap().join().unwrap().unwrap();
    } else {
        rpc.finish();
    }
    assert!(fixture.sessions()[0].context.messages.is_empty());
}
#[test]
fn rpc_initial_input_abort_is_responsive() {
    cancelled_input(false, false);
}
#[test]
fn rpc_initial_input_eof_is_responsive() {
    cancelled_input(true, false);
}
#[test]
fn rpc_idle_steering_input_abort_is_responsive() {
    cancelled_input(false, true);
}
#[test]
fn bedrock_args_and_selected_model_preserve_config_without_printing_credentials() {
    let fixture = Fixture::new(&[]);
    let mut args = fixture.args();
    args.extend([
        "--provider".into(),
        "bedrock".into(),
        "--model".into(),
        "fixture-model".into(),
        "--aws-region".into(),
        "eu-west-1".into(),
        "--aws-profile".into(),
        "fixture".into(),
        "--aws-sso-cache-dir".into(),
        fixture
            .path
            .join("missing SSO cache")
            .to_string_lossy()
            .into_owned(),
        "--aws-sso-endpoint".into(),
        "http://127.0.0.1:1/sso".into(),
        "--aws-sso-oidc-endpoint".into(),
        "http://127.0.0.1:1/oidc".into(),
        "--aws-access-key-id".into(),
        "synthetic-access".into(),
        "--aws-secret-access-key".into(),
        "synthetic-secret".into(),
        "--aws-session-token".into(),
        "synthetic-session".into(),
        "--bedrock-max-tokens".into(),
        "1234".into(),
        "--bedrock-temperature".into(),
        "0.3".into(),
        "--bedrock-cache".into(),
        "none".into(),
    ]);
    let args: Args = parse(args).unwrap();
    assert!(!format!("{args:?}").contains("synthetic"));
    let mut client = Client::from_args(&args).unwrap();
    assert_eq!(client.provider_id, "amazon-bedrock");
    assert_eq!(client.settings.options.bedrock.region, "eu-west-1");
    assert_eq!(client.settings.options.bedrock.max_tokens, Some(1234));
    assert_eq!(
        client.settings.options.bedrock.sso_cache_dir,
        Some(fixture.path.join("missing SSO cache"))
    );
    assert_eq!(
        client.settings.options.bedrock.sso_endpoint,
        "http://127.0.0.1:1/sso"
    );
    assert_eq!(
        client.settings.options.bedrock.sso_oidc_endpoint,
        "http://127.0.0.1:1/oidc"
    );
    client
        .choose_model(
            "bedrock",
            "us.anthropic.claude-sonnet-4-20250514-v1:0",
            &CancellationToken::new(),
        )
        .unwrap();
    assert_eq!(
        client
            .options
            .automation
            .model
            .as_ref()
            .unwrap()
            .max_output_tokens,
        1234
    );
    client
        .choose_model(
            "bedrock",
            "different-fixture-model",
            &CancellationToken::new(),
        )
        .unwrap();
    assert_eq!(client.settings.options.bedrock.profile, "fixture");
    assert_eq!(
        client.settings.options.bedrock.sso_cache_dir,
        Some(fixture.path.join("missing SSO cache"))
    );
    assert_eq!(
        client.settings.options.bedrock.sso_endpoint,
        "http://127.0.0.1:1/sso"
    );
    assert_eq!(
        client.settings.options.bedrock.sso_oidc_endpoint,
        "http://127.0.0.1:1/oidc"
    );
    assert_eq!(
        client.settings.options.bedrock.secret_access_key,
        "synthetic-secret"
    );
    let serialized = serde_json::to_string(&client.settings.options.bedrock).unwrap();
    assert!(!serialized.contains("synthetic"));
    let persisted = serde_json::to_string(&fixture.sessions()[0]).unwrap();
    for value in [
        "missing SSO cache",
        "/sso",
        "/oidc",
        "synthetic",
        "eu-west-1",
    ] {
        assert!(
            !persisted.contains(value),
            "configuration persisted: {value}"
        );
    }
    assert_eq!(
        client
            .session
            .runtime_state("model-selection")
            .unwrap()
            .unwrap(),
        json!({"provider":"amazon-bedrock","model":"different-fixture-model"})
    );
    for input in [
        ["--bedrock-temperature", "NaN"],
        ["--bedrock-temperature", "2"],
        ["--bedrock-max-tokens", "0"],
        ["--bedrock-cache", "bad"],
    ] {
        assert!(parse(input.into_iter().map(String::from)).is_err());
    }
}

#[test]
fn bedrock_selection_is_local_and_does_not_read_sso_cache_run_process_or_request_auth() {
    let fixture = Fixture::new(&[]);
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let cache = fixture.path.join("SSO cache");
    fs::create_dir(&cache).unwrap();
    let malformed = cache.join("session.json");
    fs::write(&malformed, b"unread expired SSO cache fixture").unwrap();
    let marker = fixture.path.join("credential-process-must-not-run.pid");
    let config = fixture.path.join("AWS config");
    fs::write(&config, format!(
        "[profile fixture-process]\nregion=us-east-1\ncredential_process=\"{}\" \"--pid={}\"\n\
         [profile fixture-sso]\nsso_session=fixture-session\nsso_account_id=123456789012\nsso_role_name=fixture-role\nregion=us-east-1\n\
         [sso-session fixture-session]\nsso_start_url=https://fixture.awsapps.com/start\nsso_region=us-east-1\n",
        executable().display(), marker.display())).unwrap();
    let config_before = fs::read(&config).unwrap();
    let mut args = fixture.args();
    args.extend([
        "--provider".into(),
        "bedrock".into(),
        "--model".into(),
        "fixture-model".into(),
        "--aws-region".into(),
        "us-east-1".into(),
        "--aws-config-file".into(),
        config.to_string_lossy().into_owned(),
        "--aws-credentials-file".into(),
        fixture
            .path
            .join("missing credentials")
            .to_string_lossy()
            .into_owned(),
        "--aws-sso-cache-dir".into(),
        cache.to_string_lossy().into_owned(),
        "--aws-sso-endpoint".into(),
        endpoint.clone(),
        "--aws-sso-oidc-endpoint".into(),
        endpoint.clone(),
    ]);
    let mut client = Client::from_args(&parse(args).unwrap()).unwrap();
    for profile in ["fixture-process", "fixture-sso"] {
        client.settings.options.bedrock.profile = profile.into();
        client
            .choose_model("bedrock", "selected-fixture", &CancellationToken::new())
            .unwrap();
        assert_eq!(client.settings.options.bedrock.profile, profile);
        assert_eq!(
            client.settings.options.bedrock.sso_cache_dir.as_ref(),
            Some(&cache)
        );
        assert!(client.session.context.messages.is_empty());
        assert!(!marker.exists());
        assert_eq!(fs::read(&config).unwrap(), config_before);
        assert_eq!(
            fs::read(&malformed).unwrap(),
            b"unread expired SSO cache fixture"
        );
        assert!(matches!(listener.accept(), Err(error) if error.kind()==io::ErrorKind::WouldBlock));
    }
    let persisted = serde_json::to_string(&fixture.sessions()[0]).unwrap();
    for value in [
        "SSO cache",
        "AWS config",
        "credential_process",
        "fixture-process",
        endpoint.as_str(),
    ] {
        assert!(!persisted.contains(value));
    }
}
