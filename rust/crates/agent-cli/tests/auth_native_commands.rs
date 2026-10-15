use agent_cli::auth_commands::{auth_store_for, execute_auth_command_with_clients};
use agent_core::types::{timestamp, CancellationToken};
use agent_runtime::auth::{CodexOAuthClient, NativeOAuthClient, NativeOAuthConfig};
use serde_json::{json, Value};
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "native-cli-auth-{}",
            agent_runtime::session::Session::default().id
        ));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
    fn args(&self, provider: &str) -> Vec<String> {
        vec![
            "auth".into(),
            "login".into(),
            provider.into(),
            "--auth-dir".into(),
            self.0.to_string_lossy().into_owned(),
        ]
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn request(stream: &mut TcpStream) -> (String, String) {
    stream.set_nonblocking(false).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let mut bytes = vec![];
    let mut byte = [0];
    while !bytes.ends_with(b"\r\n\r\n") {
        assert!(bytes.len() < 32768);
        stream.read_exact(&mut byte).unwrap();
        bytes.push(byte[0]);
    }
    let headers = String::from_utf8(bytes).unwrap();
    let size = headers
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(key, _)| key.eq_ignore_ascii_case("content-length"))
        .map(|(_, value)| value.trim().parse::<usize>().unwrap())
        .unwrap_or(0);
    let mut body = vec![0; size];
    stream.read_exact(&mut body).unwrap();
    (headers, String::from_utf8(body).unwrap())
}
fn server(responses: Vec<Value>) -> (String, thread::JoinHandle<Vec<(String, String)>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let worker = thread::spawn(move || {
        let mut result = vec![];
        for body in responses {
            let start = Instant::now();
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(start.elapsed() < Duration::from_secs(8));
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("{error}"),
                }
            };
            result.push(request(&mut stream));
            let body = body.to_string();
            write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();
        }
        result
    });
    (base, worker)
}
fn client(provider: &str, base: &str) -> NativeOAuthClient {
    let mut config = NativeOAuthConfig::for_provider(provider).unwrap();
    config.authorize_url = format!("{base}/authorize");
    config.token_url = format!("{base}/token");
    config.device_url = format!("{base}/device");
    config.copilot_token_url = format!("{base}/copilot");
    config.models_url = format!("{base}/models");
    config.callback_bind = "127.0.0.1:0".parse().unwrap();
    config.redirect_uri = "http://localhost:0/callback".into();
    config.timeout = Duration::from_secs(5);
    config.request_timeout = Duration::from_secs(3);
    NativeOAuthClient::new(config)
}
#[test]
fn five_native_login_commands_exchange_and_commit_only_the_selected_provider() {
    for provider in [
        "anthropic",
        "openrouter",
        "github-copilot",
        "kimi-coding",
        "xai",
    ] {
        let fixture = Fixture::new();
        let store = auth_store_for(&fixture.0);
        store
            .write(
                "unrelated",
                json!({"type":"api_key","key":"unrelated-secret"}),
                &CancellationToken::new(),
            )
            .unwrap();
        let responses = match provider {
            "anthropic" => vec![
                json!({"access_token":"native-access","refresh_token":"native-refresh","expires_in":3600}),
            ],
            "openrouter" => vec![json!({"key":"native-permanent-key"})],
            "github-copilot" => vec![
                json!({"device_code":"private-device","user_code":"PUBLIC-CODE","verification_uri":"https://github.com/login/device","interval":1,"expires_in":60}),
                json!({"access_token":"private-github-token"}),
                json!({"token":"tid=fixture;proxy-ep=proxy.individual.githubcopilot.com","expires_at":timestamp()/1000+3600}),
                json!({"data":[{"id":"gpt-4o","model_picker_enabled":true,"policy":{"state":"enabled"},"capabilities":{"supports":{"tool_calls":true}}}]}),
            ],
            _ => vec![
                json!({"device_code":"private-device","user_code":"PUBLIC-CODE","verification_uri":"https://fixture.invalid/verify","verification_uri_complete":"https://fixture.invalid/verify?code=PUBLIC-CODE","interval":1,"expires_in":60}),
                json!({"access_token":"native-access","refresh_token":"native-refresh","expires_in":3600}),
            ],
        };
        let (base, worker) = server(responses);
        let native = client(provider, &base);
        let (sender, manual) = mpsc::channel();
        sender.send("manual-once-code".into()).unwrap();
        let mut output = vec![];
        assert!(execute_auth_command_with_clients(
            &fixture.args(provider),
            &CodexOAuthClient::default(),
            Some(&native),
            Some(&manual),
            &CancellationToken::new(),
            &mut output
        )
        .unwrap());
        let text = String::from_utf8(output).unwrap();
        let events: Vec<Value> = text
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0]["type"], "auth_url");
        assert_eq!(
            events[1],
            json!({"type":"auth_complete","provider":provider})
        );
        if !matches!(provider, "anthropic" | "openrouter") {
            assert_eq!(events[0]["userCode"], "PUBLIC-CODE");
        }
        for secret in [
            "native-access",
            "native-refresh",
            "native-permanent-key",
            "private-device",
            "private-github-token",
            "unrelated-secret",
            "manual-once-code",
        ] {
            assert!(!text.contains(secret));
        }
        let credential = store
            .read(provider, &CancellationToken::new())
            .unwrap()
            .unwrap();
        assert_eq!(credential["type"], "oauth");
        if provider == "openrouter" {
            assert_eq!(credential["access"], "native-permanent-key");
            assert_eq!(credential["refresh"], "");
        }
        assert_eq!(
            store
                .read("unrelated", &CancellationToken::new())
                .unwrap()
                .unwrap()["key"],
            "unrelated-secret"
        );
        let requests = worker.join().unwrap();
        assert!(requests.iter().all(|(headers, _)| !headers
            .to_lowercase()
            .contains("authorization: bearer native-access")));
        assert!(requests.iter().any(|(_, body)| body.contains(
            if matches!(provider, "anthropic" | "openrouter") {
                "code_verifier"
            } else {
                "device_code"
            }
        )));
        #[cfg(windows)]
        {
            let bytes = fs::read(store.path()).unwrap();
            assert!(!String::from_utf8_lossy(&bytes).contains("native-refresh"));
        }
        assert!(!fixture.0.join("sessions").exists());
    }
}
#[test]
fn mismatched_oauth_client_and_enterprise_options_fail_before_network_or_store() {
    let fixture = Fixture::new();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let native = client("xai", &format!("http://{}", listener.local_addr().unwrap()));
    let mut output = vec![];
    let error = execute_auth_command_with_clients(
        &fixture.args("kimi-coding"),
        &CodexOAuthClient::default(),
        Some(&native),
        None,
        &CancellationToken::new(),
        &mut output,
    )
    .unwrap_err();
    assert!(error.contains("identity mismatch"));
    let mut args = fixture.args("xai");
    args.extend(["--enterprise-domain".into(), "company.ghe.com".into()]);
    assert!(execute_auth_command_with_clients(
        &args,
        &CodexOAuthClient::default(),
        None,
        None,
        &CancellationToken::new(),
        &mut output
    )
    .unwrap_err()
    .contains("belongs"));
    assert!(output.is_empty());
    assert!(!auth_store_for(&fixture.0).path().exists());
    assert!(matches!(listener.accept(),Err(error) if error.kind()==std::io::ErrorKind::WouldBlock));
}
#[test]
fn cancelled_native_login_keeps_prior_credentials_and_outputs_no_completion() {
    let fixture = Fixture::new();
    let store = auth_store_for(&fixture.0);
    store
        .write(
            "xai",
            json!({"type":"api_key","key":"prior-fixture"}),
            &CancellationToken::new(),
        )
        .unwrap();
    let before = fs::read(store.path()).unwrap();
    let signal = CancellationToken::new();
    signal.cancel();
    let native = client("xai", "http://127.0.0.1:1");
    let mut output = vec![];
    assert!(execute_auth_command_with_clients(
        &fixture.args("xai"),
        &CodexOAuthClient::default(),
        Some(&native),
        None,
        &signal,
        &mut output
    )
    .unwrap_err()
    .contains("aborted"));
    assert!(output.is_empty());
    assert_eq!(fs::read(store.path()).unwrap(), before);
}
