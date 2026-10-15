//! Actual loopback HTTP, native callback sockets and temporary DPAPI files only.
use agent_core::agent_loop::{run_agent_loop, LoopConfig, NoopLoopHooks, Provider};
use agent_core::tools::NoopToolHooks;
use agent_core::types::{timestamp, CancellationToken, Content, Context, Message, StopReason};
use agent_runtime::auth::{
    codex_account_id, pkce_challenge, CodexOAuthClient, CodexOAuthConfig, OAuthCredential,
    CODEX_CLIENT_ID, CODEX_PROVIDER,
};
use agent_runtime::auth_storage::AuthStorage;
use agent_runtime::automatic::AutomationHandle;
use agent_runtime::builtins::{create_tools, tool_definitions};
use agent_runtime::provider::{
    parse_codex_sse, parse_responses_sse, ModelProvider, ProviderMode, ProviderOptions,
    ProviderSettings,
};
use agent_runtime::retry::{RetryOptions, RetryingProvider};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::io::{Cursor, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::{mpsc, Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};

const REFRESH: &str = "temporary-offline-refresh-fixture";
fn jwt(account: &str) -> String {
    format!(
        "e30.{}.fixture",
        URL_SAFE_NO_PAD.encode(
            json!({"https://api.openai.com/auth":{"chatgpt_account_id":account}}).to_string()
        )
    )
}
fn credential(expired: bool) -> OAuthCredential {
    OAuthCredential {
        credential_type: "oauth".into(),
        access: jwt("offline-account"),
        refresh: REFRESH.into(),
        expires: if expired { 0 } else { timestamp() + 3_600_000 },
        account_id: "offline-account".into(),
        extra: BTreeMap::from([("preserved".into(), json!({"unknown":true}))]),
    }
}
fn token_response() -> String {
    json!({"access_token":jwt("offline-account"),"refresh_token":"rotated-offline-fixture","expires_in":3600}).to_string()
}

struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "rust-auth-{}-{}",
            std::process::id(),
            URL_SAFE_NO_PAD.encode({
                let mut bytes = [0u8; 16];
                getrandom::fill(&mut bytes).unwrap();
                bytes
            })
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn store(&self) -> AuthStorage {
        AuthStorage::new(self.0.join("native-auth.dat"))
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        for file in fs::read_dir(&self.0).unwrap().flatten() {
            let _ = fs::remove_file(file.path());
        }
        let _ = fs::remove_dir(&self.0);
    }
}

struct Request {
    path: String,
    headers: BTreeMap<String, String>,
    body: String,
}
fn accept(listener: &TcpListener) -> TcpStream {
    listener.set_nonblocking(true).unwrap();
    let started = Instant::now();
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                return stream;
            }
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock
                    && started.elapsed() < Duration::from_secs(5) =>
            {
                thread::sleep(Duration::from_millis(5))
            }
            Err(error) => panic!("Fixture accept: {error}"),
        }
    }
}
fn request(stream: &mut TcpStream) -> Request {
    let mut bytes = Vec::new();
    while !bytes.ends_with(b"\r\n\r\n") {
        assert!(bytes.len() < 65_536);
        let mut byte = [0u8];
        stream.read_exact(&mut byte).unwrap();
        bytes.push(byte[0]);
    }
    let text = String::from_utf8(bytes).unwrap();
    let mut lines = text.lines();
    let path = lines
        .next()
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .to_owned();
    let headers: BTreeMap<_, _> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(key, value)| (key.to_ascii_lowercase(), value.trim().to_owned()))
        .collect();
    let length = headers["content-length"].parse::<usize>().unwrap();
    assert!(length < 524_288);
    let mut body = vec![0u8; length];
    stream.read_exact(&mut body).unwrap();
    Request {
        path,
        headers,
        body: String::from_utf8(body).unwrap(),
    }
}
fn respond(stream: &mut TcpStream, status: &str, body: &str) {
    write!(stream,"HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();
}
fn fixture_server(body: String, status: &'static str) -> (SocketAddr, thread::JoinHandle<Request>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let address = listener.local_addr().unwrap();
    (
        address,
        thread::spawn(move || {
            let mut stream = accept(&listener);
            let request = request(&mut stream);
            respond(&mut stream, status, &body);
            request
        }),
    )
}
fn client(address: SocketAddr) -> CodexOAuthClient {
    CodexOAuthClient::new(CodexOAuthConfig {
        token_url: format!("http://{address}/oauth/token"),
        authorize_url: format!("http://{address}/oauth/authorize"),
        redirect_uri: "http://127.0.0.1:0/auth/callback".into(),
        callback_bind: "127.0.0.1:0".parse().unwrap(),
        timeout: Duration::from_secs(3),
        ..CodexOAuthConfig::default()
    })
}
fn form(body: &str) -> BTreeMap<String, String> {
    let url = reqwest::Url::parse(&format!("http://fixture.invalid?{body}")).unwrap();
    url.query_pairs()
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect()
}
fn callback(address: SocketAddr, path: &str) -> String {
    let mut stream = TcpStream::connect(address).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut reply = String::new();
    stream.read_to_string(&mut reply).unwrap();
    reply
}

#[test]
fn pkce_matches_rfc7636_and_login_generates_fresh_state_and_verifier_challenge() {
    assert_eq!(
        pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
        "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
    );
    let config = CodexOAuthConfig::default();
    assert_eq!(config.client_id, CODEX_CLIENT_ID);
    assert_eq!(config.redirect_uri, "http://localhost:1455/auth/callback");
    let client = client("127.0.0.1:1".parse().unwrap());
    let first = client.begin_login().unwrap();
    let second = client.begin_login().unwrap();
    let first_url = reqwest::Url::parse(&first.authorization_url).unwrap();
    let first = form(first_url.query().unwrap());
    let second_url = reqwest::Url::parse(&second.authorization_url).unwrap();
    let second = form(second_url.query().unwrap());
    assert_eq!(first["client_id"], CODEX_CLIENT_ID);
    assert_eq!(first["scope"], "openid profile email offline_access");
    assert_eq!(first["code_challenge_method"], "S256");
    assert_eq!(first["codex_cli_simplified_flow"], "true");
    assert_eq!(first["id_token_add_organizations"], "true");
    assert_eq!(first["originator"], "pi");
    assert_ne!(first["state"], second["state"]);
    assert_ne!(first["code_challenge"], second["code_challenge"]);
    assert_eq!(first["state"].len(), 32);
    assert_eq!(first["code_challenge"].len(), 43);
}

#[test]
fn callback_state_and_route_are_checked_before_actual_pkce_token_exchange() {
    let (address, server) = fixture_server(token_response(), "200 OK");
    let login = client(address).begin_login().unwrap();
    let callback_address = login.callback_addr().unwrap();
    let redirect = login.redirect_uri.clone();
    let url = reqwest::Url::parse(&login.authorization_url).unwrap();
    let authorization = form(url.query().unwrap());
    let state = authorization["state"].clone();
    let challenge = authorization["code_challenge"].clone();
    let worker = thread::spawn(move || login.finish(&CancellationToken::new()));
    assert!(callback(
        callback_address,
        &format!("/wrong?code=fixture&state={state}")
    )
    .starts_with("HTTP/1.1 404"));
    assert!(
        callback(callback_address, "/auth/callback?code=fixture&state=wrong")
            .starts_with("HTTP/1.1 400")
    );
    assert!(callback(
        callback_address,
        &format!("/auth/callback?code=fixture&state={state}&state={state}")
    )
    .starts_with("HTTP/1.1 400"));
    let reply = callback(
        callback_address,
        &format!("/auth/callback?code=fixture-code%2Bvalue&state={state}"),
    );
    assert!(reply.starts_with("HTTP/1.1 200"));
    assert!(!reply.contains("authentication completed"));
    let result = worker.join().unwrap().unwrap();
    assert_eq!(result.account_id, "offline-account");
    assert_eq!(result.refresh, "rotated-offline-fixture");
    assert!(result.expires > timestamp());
    let request = server.join().unwrap();
    assert_eq!(request.path, "/oauth/token");
    assert_eq!(
        request.headers["content-type"],
        "application/x-www-form-urlencoded"
    );
    assert!(!request.headers.contains_key("authorization"));
    let body = form(&request.body);
    assert_eq!(body["grant_type"], "authorization_code");
    assert_eq!(body["client_id"], CODEX_CLIENT_ID);
    assert_eq!(body["code"], "fixture-code+value");
    assert_eq!(body["redirect_uri"], redirect);
    assert_eq!(pkce_challenge(&body["code_verifier"]), challenge);
    assert_eq!(
        URL_SAFE_NO_PAD
            .decode(&body["code_verifier"])
            .unwrap()
            .len(),
        32
    );
    assert!(TcpListener::bind(callback_address).is_ok());
}

#[test]
fn callback_wait_cancel_and_timeout_close_the_listener() {
    let client = client("127.0.0.1:1".parse().unwrap());
    let login = client.begin_login().unwrap();
    let address = login.callback_addr().unwrap();
    let signal = CancellationToken::new();
    let worker_signal = signal.clone();
    let worker = thread::spawn(move || login.finish(&worker_signal));
    thread::sleep(Duration::from_millis(75));
    let start = Instant::now();
    signal.cancel();
    let result = worker.join().unwrap();
    assert!(result.err().unwrap().contains("aborted"));
    assert!(start.elapsed() < Duration::from_secs(2));
    assert!(TcpListener::bind(address).is_ok());
    let mut client = client;
    client.config.timeout = Duration::from_millis(50);
    let login = client.begin_login().unwrap();
    let address = login.callback_addr().unwrap();
    assert!(login
        .finish(&CancellationToken::new())
        .err()
        .unwrap()
        .contains("timed out"));
    assert!(TcpListener::bind(address).is_ok());
}

#[test]
fn refresh_rotates_the_token_preserves_extra_metadata_and_uses_the_exact_form() {
    let (address, server) = fixture_server(token_response(), "200 OK");
    let old = credential(true);
    let result = client(address)
        .refresh(&old, &CancellationToken::new())
        .unwrap();
    assert_eq!(result.refresh, "rotated-offline-fixture");
    assert_eq!(result.extra, old.extra);
    result.validate().unwrap();
    let request = server.join().unwrap();
    let body = form(&request.body);
    assert_eq!(body["grant_type"], "refresh_token");
    assert_eq!(body["refresh_token"], REFRESH);
    assert_eq!(body["client_id"], CODEX_CLIENT_ID);
    assert!(!body.contains_key("code_verifier"));
}

#[test]
fn token_errors_and_invalid_success_bodies_do_not_expose_secrets() {
    for (body, status) in [
        (format!("reflected {REFRESH}"), "401 Unauthorized"),
        (
            json!({"access_token":jwt("offline-account"),"refresh_token":REFRESH}).to_string(),
            "200 OK",
        ),
        (
            json!({"access_token":jwt("offline-account"),"refresh_token":REFRESH,"expires_in":-1})
                .to_string(),
            "200 OK",
        ),
        (
            json!({"access_token":"invalid","refresh_token":REFRESH,"expires_in":3600}).to_string(),
            "200 OK",
        ),
        ("{".to_owned(), "200 OK"),
    ] {
        let (address, server) = fixture_server(body, status);
        let result = client(address).refresh(&credential(true), &CancellationToken::new());
        let error = result.err().unwrap();
        assert!(!error.contains(REFRESH));
        assert!(!error.contains("reflected"));
        server.join().unwrap();
    }
    assert!(codex_account_id("not-a-jwt").is_err());
    let mut invalid = credential(false);
    invalid.account_id = "different".into();
    assert!(invalid.validate().is_err());
}

#[test]
fn token_exchange_and_refresh_cancel_without_waiting_for_headers() {
    for exchange in [false, true] {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let (accepted_tx, accepted_rx) = mpsc::channel();
        let server = thread::spawn(move || {
            let mut stream = accept(&listener);
            request(&mut stream);
            stream
                .set_read_timeout(Some(Duration::from_millis(100)))
                .unwrap();
            accepted_tx.send(()).unwrap();
            let start = Instant::now();
            loop {
                let mut buffer = [0u8];
                match stream.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(_) => {}
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                        ) => {}
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::ConnectionReset
                                | std::io::ErrorKind::ConnectionAborted
                        ) =>
                    {
                        break
                    }
                    Err(error) => panic!("{error}"),
                };
                assert!(start.elapsed() < Duration::from_secs(5));
            }
        });
        let signal = CancellationToken::new();
        let worker_signal = signal.clone();
        let (result_tx, result_rx) = mpsc::channel();
        let worker = if exchange {
            let login = client(address).begin_login().unwrap();
            let cb = login.callback_addr().unwrap();
            let url = reqwest::Url::parse(&login.authorization_url).unwrap();
            let state = form(url.query().unwrap())["state"].clone();
            let worker =
                thread::spawn(move || result_tx.send(login.finish(&worker_signal)).unwrap());
            assert!(
                callback(cb, &format!("/auth/callback?code=fixture&state={state}"))
                    .starts_with("HTTP/1.1 200")
            );
            worker
        } else {
            thread::spawn(move || {
                result_tx
                    .send(client(address).refresh(&credential(true), &worker_signal))
                    .unwrap()
            })
        };
        accepted_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        thread::sleep(Duration::from_millis(75));
        let start = Instant::now();
        signal.cancel();
        let result = result_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(result.err().unwrap().contains("aborted"));
        assert!(start.elapsed() < Duration::from_secs(2));
        worker.join().unwrap();
        server.join().unwrap();
    }
}

#[test]
fn native_storage_encrypts_on_windows_and_lists_only_metadata() {
    let temp = Temp::new();
    let store = temp.store();
    let signal = CancellationToken::new();
    assert!(store.read(CODEX_PROVIDER, &signal).unwrap().is_none());
    store
        .write(
            CODEX_PROVIDER,
            serde_json::to_value(credential(false)).unwrap(),
            &signal,
        )
        .unwrap();
    let reopened = AuthStorage::new(store.path());
    let restored = reopened.read(CODEX_PROVIDER, &signal).unwrap().unwrap();
    assert_eq!(restored["refresh"], REFRESH);
    assert_eq!(restored["preserved"]["unknown"], true);
    let listing = serde_json::to_string(&reopened.list(&signal).unwrap()).unwrap();
    assert!(listing.contains("openai-codex"));
    assert!(!listing.contains(REFRESH));
    assert!(!listing.contains(&jwt("offline-account")));
    #[cfg(windows)]
    {
        let bytes = fs::read(store.path()).unwrap();
        assert!(bytes.starts_with(b"AGENT_CORE_DPAPI_V1\n"));
        assert!(!bytes
            .windows(REFRESH.len())
            .any(|part| part == REFRESH.as_bytes()));
        assert!(!bytes
            .windows("offline-account".len())
            .any(|part| part == b"offline-account"));
    }
    store
        .write(
            "unknown-provider",
            json!({"type":"future","nested":{"preserve":1}}),
            &signal,
        )
        .unwrap();
    store.delete(CODEX_PROVIDER, &signal).unwrap();
    assert_eq!(
        store.read("unknown-provider", &signal).unwrap().unwrap()["nested"]["preserve"],
        1
    );
    assert!(store.read(CODEX_PROVIDER, &signal).unwrap().is_none());
}

#[test]
fn legacy_import_is_read_only_preserves_unknown_entries_and_never_overwrites_native_login() {
    let temp = Temp::new();
    let store = temp.store();
    let legacy = temp.0.join("auth.json");
    let signal = CancellationToken::new();
    let source = format!(
        "\u{feff}{}",
        json!({"openai-codex":serde_json::to_value(credential(false)).unwrap(),"unknown-provider":{"type":"custom","unknown":[1,2]},"command-provider":{"type":"api_key","key":"!never_execute_fixture_command","env":{"FIXTURE":"value"}}})
    );
    fs::write(&legacy, source.as_bytes()).unwrap();
    let original = Sha256::digest(fs::read(&legacy).unwrap());
    store
        .write(
            CODEX_PROVIDER,
            json!({"type":"oauth","access":"current-native","refresh":"kept"}),
            &signal,
        )
        .unwrap();
    let report = store.import_legacy(&legacy, &signal).unwrap();
    assert_eq!(report.imported, ["command-provider", "unknown-provider"]);
    assert_eq!(report.skipped, [CODEX_PROVIDER]);
    assert_eq!(
        store.read(CODEX_PROVIDER, &signal).unwrap().unwrap()["access"],
        "current-native"
    );
    assert_eq!(
        store.read("unknown-provider", &signal).unwrap().unwrap()["unknown"],
        json!([1, 2])
    );
    assert_eq!(
        store.read("command-provider", &signal).unwrap().unwrap()["key"],
        "!never_execute_fixture_command"
    );
    assert_eq!(Sha256::digest(fs::read(&legacy).unwrap()), original);
}

#[test]
fn native_storage_rejects_corruption_and_cancelled_updates_without_losing_existing_bytes() {
    let temp = Temp::new();
    let store = temp.store();
    let signal = CancellationToken::new();
    store
        .write(
            "fixture",
            json!({"type":"api_key","key":"initial"}),
            &signal,
        )
        .unwrap();
    let initial = fs::read(store.path()).unwrap();
    let aborted = CancellationToken::new();
    aborted.cancel();
    assert!(store
        .write(
            "fixture",
            json!({"type":"api_key","key":"changed"}),
            &aborted
        )
        .unwrap_err()
        .contains("aborted"));
    assert_eq!(fs::read(store.path()).unwrap(), initial);
    let mut damaged = initial.clone();
    let last = damaged.len() - 1;
    damaged[last] ^= 0xff;
    fs::write(store.path(), &damaged).unwrap();
    assert!(store.read("fixture", &signal).is_err());
    assert_eq!(fs::read(store.path()).unwrap(), damaged);
    fs::write(store.path(), initial).unwrap();
    assert_eq!(
        store.read("fixture", &signal).unwrap().unwrap()["key"],
        "initial"
    );
}

#[test]
fn concurrent_refresh_is_serialized_and_persisted_before_any_request() {
    let temp = Temp::new();
    let store = temp.store();
    let signal = CancellationToken::new();
    store
        .write(
            CODEX_PROVIDER,
            serde_json::to_value(credential(true)).unwrap(),
            &signal,
        )
        .unwrap();
    store
        .write("unknown", json!({"type":"future","keep":true}), &signal)
        .unwrap();
    let (address, server) = fixture_server(token_response(), "200 OK");
    let barrier = Arc::new(Barrier::new(3));
    let workers: Vec<_> = (0..2)
        .map(|_| {
            let store = store.clone();
            let barrier = barrier.clone();
            thread::spawn(move || {
                barrier.wait();
                store.ensure_codex(&client(address), &CancellationToken::new())
            })
        })
        .collect();
    barrier.wait();
    for worker in workers {
        assert_eq!(
            worker.join().unwrap().unwrap().refresh,
            "rotated-offline-fixture"
        );
    }
    let request = server.join().unwrap();
    assert_eq!(form(&request.body)["refresh_token"], REFRESH);
    assert_eq!(
        store.read(CODEX_PROVIDER, &signal).unwrap().unwrap()["refresh"],
        "rotated-offline-fixture"
    );
    assert_eq!(
        store.read("unknown", &signal).unwrap().unwrap()["keep"],
        true
    );
}

#[test]
fn cancelled_storage_lock_wait_keeps_lock_and_original_credentials() {
    let temp = Temp::new();
    let store = temp.store();
    let signal = CancellationToken::new();
    store
        .write(
            "fixture",
            json!({"type":"api_key","key":"initial"}),
            &signal,
        )
        .unwrap();
    let initial = fs::read(store.path()).unwrap();
    let mut lock = store.path().as_os_str().to_os_string();
    lock.push(".lock");
    let lock = PathBuf::from(lock);
    fs::write(&lock, b"fixture owns lock").unwrap();
    let worker_signal = signal.clone();
    let worker_store = store.clone();
    let worker = thread::spawn(move || worker_store.delete("fixture", &worker_signal));
    thread::sleep(Duration::from_millis(75));
    signal.cancel();
    assert!(worker.join().unwrap().unwrap_err().contains("aborted"));
    assert_eq!(fs::read(store.path()).unwrap(), initial);
    assert_eq!(fs::read(&lock).unwrap(), b"fixture owns lock");
}

fn sse_output() -> String {
    let item = json!({"type":"message","id":"msg_offline","role":"assistant","status":"completed","content":[{"type":"output_text","text":"离线Codex回答","annotations":[]}]});
    format!(
        "data: {}\n\n",
        json!({"type":"response.done","response":{"id":"resp_offline","status":"completed","output":[item],"end_turn":true}})
    )
}
fn fixture_provider(address: SocketAddr) -> ModelProvider {
    ModelProvider {
        settings: ProviderSettings {
            mode: ProviderMode::OpenAiCodex,
            base_url: format!("http://{address}/backend-api"),
            model: "codex-fixture-model".into(),
            api_key: "must-not-send-api-key-as-oauth".into(),
            options: ProviderOptions {
                oauth: Some(credential(false)),
                ..ProviderOptions::default()
            },
        },
        tool_definitions: vec![
            json!({"type":"function","function":{"name":"fixture","description":"fixture","parameters":{"type":"object"}}}),
        ],
    }
}

#[test]
fn codex_provider_uses_special_route_oauth_account_headers_and_native_payload() {
    let (address, server) = fixture_server(sse_output(), "200 OK");
    let message = fixture_provider(address)
        .complete(
            &Context {
                system_prompt: String::new(),
                messages: vec![Message::user("offline prompt")],
            },
            &CancellationToken::new(),
            &mut |_| {},
        )
        .unwrap();
    assert_eq!(Message::Assistant(message.clone()).text(), "离线Codex回答");
    let metadata = message.protocol_metadata.as_ref().unwrap();
    assert_eq!(metadata["protocol"], "openai-codex-responses");
    assert_eq!(metadata["provider"], CODEX_PROVIDER);
    assert_eq!(metadata["end_turn"], true);
    let request = server.join().unwrap();
    assert_eq!(request.path, "/backend-api/codex/responses");
    assert_eq!(
        request.headers["authorization"],
        format!("Bearer {}", jwt("offline-account"))
    );
    assert_eq!(request.headers["chatgpt-account-id"], "offline-account");
    assert_eq!(request.headers["originator"], "pi");
    assert_eq!(request.headers["openai-beta"], "responses=experimental");
    assert_eq!(request.headers["accept"], "text/event-stream");
    assert!(!request.headers.contains_key("x-api-key"));
    let body: Value = serde_json::from_str(&request.body).unwrap();
    assert_eq!(body["instructions"], "You are a helpful assistant.");
    assert_eq!(body["store"], false);
    assert_eq!(body["text"]["verbosity"], "low");
    assert_eq!(body["tools"][0]["strict"], Value::Null);
    assert_eq!(body["parallel_tool_calls"], true);
    assert_eq!(body["tool_choice"], "auto");
    assert!(!request.body.contains(REFRESH));
    assert!(!request.body.contains("must-not-send-api-key-as-oauth"));
}

#[test]
fn codex_provider_refreshes_under_storage_lock_then_uses_the_rotated_token() {
    let temp = Temp::new();
    let store = temp.store();
    let signal = CancellationToken::new();
    store
        .write(
            CODEX_PROVIDER,
            serde_json::to_value(credential(true)).unwrap(),
            &signal,
        )
        .unwrap();
    let (auth_address, auth_server) = fixture_server(token_response(), "200 OK");
    let (model_address, model_server) = fixture_server(sse_output(), "200 OK");
    let mut provider = fixture_provider(model_address);
    provider.settings.options.oauth = Some(credential(true));
    provider.settings.options.auth_storage_path = Some(store.path().to_owned());
    provider.settings.options.codex_oauth = client(auth_address).config;
    provider
        .complete(
            &Context {
                system_prompt: "offline system".into(),
                messages: vec![Message::user("offline prompt")],
            },
            &signal,
            &mut |_| {},
        )
        .unwrap();
    assert_eq!(
        provider.settings.options.oauth.unwrap().refresh,
        "rotated-offline-fixture"
    );
    assert_eq!(
        store.read(CODEX_PROVIDER, &signal).unwrap().unwrap()["refresh"],
        "rotated-offline-fixture"
    );
    auth_server.join().unwrap();
    assert_eq!(
        model_server.join().unwrap().headers["authorization"],
        format!("Bearer {}", jwt("offline-account"))
    );
}

#[test]
fn summary_provider_omits_tools_and_signature_replay_requires_provider_and_route() {
    let (address, server) = fixture_server(sse_output(), "200 OK");
    let mut provider = fixture_provider(address);
    let message = provider
        .complete_summary(
            &Context {
                system_prompt: "summary".into(),
                messages: vec![Message::user("summary fixture")],
            },
            &CancellationToken::new(),
            &mut |_| {},
        )
        .unwrap();
    let request = server.join().unwrap();
    let body: Value = serde_json::from_str(&request.body).unwrap();
    assert!(body.get("tools").is_none());
    assert_eq!(provider.tool_definitions.len(), 1);
    for mismatch in ["provider", "route"] {
        let (address, server) = fixture_server(sse_output(), "200 OK");
        let mut provider = fixture_provider(address);
        let mut previous = message.clone();
        let metadata = previous.protocol_metadata.as_mut().unwrap();
        metadata["route"] = json!(format!("http://{address}/backend-api/codex/responses"));
        metadata[mismatch] = json!("different-fixture");
        previous.content.insert(
            0,
            Content::Thinking {
                thinking: "visible reasoning".into(),
            },
        );
        metadata["items"] = json!({"0":{"type":"reasoning","id":"rs_fixture","summary":[],"encrypted_content":"must-not-replay-signed-fixture"}});
        provider
            .complete(
                &Context {
                    system_prompt: String::new(),
                    messages: vec![Message::Assistant(previous), Message::user("next")],
                },
                &CancellationToken::new(),
                &mut |_| {},
            )
            .unwrap();
        let request = server.join().unwrap();
        assert!(!request.body.contains("must-not-replay-signed-fixture"));
        assert!(!request.body.contains("rs_fixture"));
    }
}

#[test]
fn codex_preflight_never_treats_api_key_as_oauth_or_contacts_model_for_missing_login() {
    let mut settings = ProviderSettings {
        mode: ProviderMode::OpenAiCodex,
        base_url: "http://127.0.0.1:1/backend-api".into(),
        model: "fixture".into(),
        api_key: "fixture-api-key".into(),
        options: ProviderOptions::default(),
    };
    assert!(settings.validate().unwrap_err().contains("login"));
    settings.options.oauth = Some(credential(false));
    settings.validate().unwrap();
    settings.options.oauth.as_mut().unwrap().account_id = "wrong".into();
    assert!(settings.validate().is_err());
    assert_eq!(
        ProviderMode::OpenAiCodex.default_base_url(),
        "https://chatgpt.com/backend-api"
    );
    assert_eq!(ProviderMode::OpenAiCodex.api_key_env(), "");
}

#[test]
fn model_http_errors_only_expose_whitelisted_structural_context_codes() {
    for code in [
        "context_length_exceeded",
        "context_window_exceeded",
        "too_many_tokens",
        "max_context_length_exceeded",
        "unknown_code",
    ] {
        let body=json!({"error":{"code":code,"message":format!("context_length_exceeded reflected {REFRESH}")}}).to_string();
        let (address, server) = fixture_server(body, "400 Bad Request");
        let error = fixture_provider(address)
            .complete(
                &Context {
                    system_prompt: String::new(),
                    messages: vec![Message::user("fixture")],
                },
                &CancellationToken::new(),
                &mut |_| panic!("An HTTP error must not emit a partial response"),
            )
            .unwrap_err();
        assert!(!error.contains(REFRESH));
        assert!(!error.contains("reflected"));
        assert!(error.contains("HTTP 400"));
        if code == "unknown_code" {
            assert!(!error.contains("context_length_exceeded"));
            assert!(!error.contains("unknown_code"));
        } else {
            assert!(error.contains(&format!("({code})")));
        }
        server.join().unwrap();
    }
    let (address, server) = fixture_server(
        json!({"error":{"type":"context_window_exceeded","code":"other","message":REFRESH}})
            .to_string(),
        "400 Bad Request",
    );
    let error = fixture_provider(address)
        .complete(&Context::default(), &CancellationToken::new(), &mut |_| {})
        .unwrap_err();
    assert!(error.contains("(context_window_exceeded)"));
    assert!(!error.contains(REFRESH));
    server.join().unwrap();
}

#[test]
fn model_http_error_body_read_has_a_short_deadline_and_is_cancellable() {
    for cancel in [false, true] {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let (accepted_tx, accepted_rx) = mpsc::channel();
        let server = thread::spawn(move || {
            let mut stream = accept(&listener);
            request(&mut stream);
            write!(stream,"HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: 100000\r\nConnection: close\r\n\r\n").unwrap();
            accepted_tx.send(()).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_millis(100)))
                .unwrap();
            let started = Instant::now();
            loop {
                let mut byte = [0u8];
                match stream.read(&mut byte) {
                    Ok(0) => break,
                    Ok(_) => {}
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                        ) => {}
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::ConnectionReset
                                | std::io::ErrorKind::ConnectionAborted
                        ) =>
                    {
                        break
                    }
                    Err(error) => panic!("{error}"),
                };
                assert!(started.elapsed() < Duration::from_secs(5));
            }
        });
        let signal = CancellationToken::new();
        let worker_signal = signal.clone();
        let (result_tx, result_rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            result_tx
                .send(fixture_provider(address).complete(
                    &Context::default(),
                    &worker_signal,
                    &mut |_| {},
                ))
                .unwrap()
        });
        accepted_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        let started = Instant::now();
        if cancel {
            thread::sleep(Duration::from_millis(75));
            signal.cancel();
        }
        let error = result_rx
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(error.contains(if cancel { "aborted" } else { "HTTP 400" }));
        worker.join().unwrap();
        server.join().unwrap();
    }
}

#[test]
fn codex_loopback_reads_a_real_fixture_then_replays_reasoning_and_pairs_tool_output() {
    let temp = Temp::new();
    fs::write(temp.0.join("sample.txt"), "离线文件样本\n").unwrap();
    let tools = create_tools(&temp.0, false, false);
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        let first = format!(
            "data: {}\n\n",
            json!({"type":"response.done","response":{"id":"resp_read","status":"completed","output":[{"type":"reasoning","id":"rs_read","summary":[{"type":"summary_text","text":"读取文件"}],"encrypted_content":"opaque-codex-reasoning-fixture"},{"type":"function_call","id":"fc_read","call_id":"call_read","name":"read","arguments":"{\"path\":\"sample.txt\"}","status":"completed"}]}})
        );
        let mut requests = Vec::new();
        for response in [first, sse_output()] {
            let mut stream = accept(&listener);
            requests.push(request(&mut stream));
            respond(&mut stream, "200 OK", &response);
        }
        requests
    });
    let mut provider = fixture_provider(address);
    provider.tool_definitions = tool_definitions(&tools);
    let messages = run_agent_loop(
        vec![Message::user("读取 sample.txt")],
        &Context::default(),
        LoopConfig::default(),
        &mut provider,
        &tools,
        &mut NoopLoopHooks,
        &NoopToolHooks,
        &CancellationToken::new(),
        &mut |_| {},
    )
    .unwrap();
    let requests = server.join().unwrap();
    assert_eq!(requests.len(), 2);
    let replay: Value = serde_json::from_str(&requests[1].body).unwrap();
    assert_eq!(replay["input"][1]["type"], "reasoning");
    assert_eq!(
        replay["input"][1]["encrypted_content"],
        "opaque-codex-reasoning-fixture"
    );
    assert_eq!(replay["input"][2]["id"], "fc_read");
    assert_eq!(replay["input"][2]["call_id"], "call_read");
    assert_eq!(replay["input"][3]["type"], "function_call_output");
    assert_eq!(replay["input"][3]["call_id"], "call_read");
    assert_eq!(replay["input"][3]["output"], "离线文件样本\n");
    assert_eq!(
        messages.iter().map(Message::role).collect::<Vec<_>>(),
        ["user", "assistant", "toolResult", "assistant"]
    );
    assert_eq!(messages.last().unwrap().text(), "离线Codex回答");
}

#[test]
fn codex_terminal_alias_keeps_length_and_rejects_failed_or_missing_terminal_status() {
    let body = format!(
        "data: {}\n\n",
        json!({"type":"response.done","response":{"status":"incomplete","incomplete_details":{"reason":"max_output_tokens"},"output":[{"type":"function_call","id":"fc_fixture","call_id":"call_fixture","name":"read","arguments":"{"}]}})
    );
    let message =
        parse_codex_sse(Cursor::new(&body), &CancellationToken::new(), &mut |_| {}).unwrap();
    assert_eq!(message.stop_reason, StopReason::Length);
    assert_eq!(message.tool_calls()[0].arguments, Value::Null);
    assert!(
        parse_responses_sse(Cursor::new(&body), &CancellationToken::new(), &mut |_| {}).is_err()
    );
    for response in [
        json!({"type":"response.done","response":{"status":"failed","error":{"message":REFRESH}}}),
        json!({"type":"response.done","response":{"output":[]}}),
        json!({"type":"response.created","response":{"status":"in_progress"}}),
    ] {
        let error = parse_codex_sse(
            Cursor::new(format!("data: {response}\n\n")),
            &CancellationToken::new(),
            &mut |_| {},
        )
        .unwrap_err();
        assert!(!error.contains(REFRESH));
    }
}

#[test]
fn model_http_quota_codes_remain_nonretryable_without_exposing_error_messages() {
    for mode in [ProviderMode::OpenAiCompatible, ProviderMode::OpenAiCodex] {
        for code in [
            "insufficient_quota",
            "billing_hard_limit_reached",
            "GoUsageLimitError",
            "FreeUsageLimitError",
        ] {
            let (address, server) = fixture_server(
                json!({"error":{"code":code,"message":format!("reflected {REFRESH}")}}).to_string(),
                "429 Too Many Requests",
            );
            let mut provider = fixture_provider(address);
            provider.settings.mode = mode.clone();
            if mode == ProviderMode::OpenAiCompatible {
                provider.settings.options.oauth = None;
                provider.settings.options.api_key_explicit = true;
                provider.settings.api_key = "offline-compatible-quota-fixture".into();
            }
            let handle = AutomationHandle::default();
            let mut retry = RetryingProvider::new(
                &mut provider,
                RetryOptions {
                    enabled: true,
                    max_retries: 2,
                    base_delay_ms: 0,
                    max_delay_ms: 0,
                },
                handle.clone(),
                "agent",
                None,
            );
            let error = retry
                .complete(&Context::default(), &CancellationToken::new(), &mut |_| {
                    panic!("Quota HTTP error must not stream")
                })
                .unwrap_err();
            assert!(
                error.contains(&format!("({code})")),
                "mode={mode:?} code={code} error={error}"
            );
            assert!(
                error.contains("HTTP 429"),
                "mode={mode:?} code={code} error={error}"
            );
            assert!(!error.contains(REFRESH));
            assert!(!error.contains("reflected"));
            assert_eq!(handle.snapshot().retries, 0);
            assert_eq!(
                server.join().unwrap().path,
                if mode == ProviderMode::OpenAiCodex {
                    "/backend-api/codex/responses"
                } else {
                    "/backend-api/chat/completions"
                }
            );
        }
    }
}
