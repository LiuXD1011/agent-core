use agent_core::agent_loop::Provider;
use agent_core::types::{timestamp, CancellationToken, Context, Message};
use agent_runtime::auth::{NativeOAuthClient, NativeOAuthConfig, OAuthCredential};
use agent_runtime::auth_storage::AuthStorage;
use agent_runtime::provider::{ModelProvider, ProviderMode, ProviderOptions, ProviderSettings};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

type CapturedRequest = (String, BTreeMap<String, String>, String);
type ServerWorker = std::thread::JoinHandle<Vec<CapturedRequest>>;

fn request(stream: &mut TcpStream) -> CapturedRequest {
    stream.set_nonblocking(false).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let mut bytes = Vec::new();
    let mut byte = [0];
    while !bytes.ends_with(b"\r\n\r\n") {
        assert!(bytes.len() < 32768);
        stream.read_exact(&mut byte).unwrap();
        bytes.push(byte[0]);
    }
    let headers = String::from_utf8(bytes).unwrap();
    let mut lines = headers.lines();
    let first = lines.next().unwrap().to_owned();
    let headers: BTreeMap<String, String> = lines
        .filter_map(|line| {
            line.split_once(':')
                .map(|(key, value)| (key.to_lowercase(), value.trim().into()))
        })
        .collect();
    let size = headers
        .get("content-length")
        .map(|value| value.parse::<usize>().unwrap())
        .unwrap_or(0);
    let mut body = vec![0; size];
    stream.read_exact(&mut body).unwrap();
    (first, headers, String::from_utf8(body).unwrap())
}
fn reply(stream: &mut TcpStream, status: u16, body: Value) {
    let body = body.to_string();
    write!(stream,"HTTP/1.1 {status} Fixture\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();
}
fn server(responses: Vec<(u16, Value)>) -> (String, ServerWorker) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let worker = std::thread::spawn(move || {
        let mut requests = vec![];
        for (status, body) in responses {
            let started = Instant::now();
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(
                            started.elapsed() < Duration::from_secs(12),
                            "missing OAuth fixture request"
                        );
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("{error}"),
                }
            };
            requests.push(request(&mut stream));
            reply(&mut stream, status, body);
        }
        requests
    });
    (base, worker)
}
fn config(provider: &str, base: &str) -> NativeOAuthConfig {
    let mut config = NativeOAuthConfig::for_provider(provider).unwrap();
    config.device_url = format!("{base}/device");
    config.token_url = format!("{base}/token");
    config.copilot_token_url = format!("{base}/copilot-token");
    config.models_url = format!("{base}/models");
    config.authorize_url = format!("{base}/authorize");
    config.callback_bind = "127.0.0.1:0".parse().unwrap();
    config.redirect_uri = "http://localhost:0/callback".into();
    config.request_timeout = Duration::from_secs(3);
    config.timeout = Duration::from_secs(10);
    config.retry_delay = Duration::from_millis(1);
    config
}
fn credential() -> OAuthCredential {
    OAuthCredential {
        credential_type: "oauth".into(),
        access: "fixture-old-access".into(),
        refresh: "fixture-refresh".into(),
        expires: 0,
        account_id: String::new(),
        extra: BTreeMap::from([("unknown".into(), json!({"preserved":true}))]),
    }
}
fn temp() -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../../target/auth-native-fixtures")
        .join(format!(
            "{}-{}-{}",
            std::process::id(),
            timestamp(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
    fs::create_dir_all(&path).unwrap();
    path
}

#[test]
fn five_flow_constants_and_credential_rules_are_provider_specific() {
    let copilot = NativeOAuthConfig::for_provider("github-copilot").unwrap();
    assert_eq!(copilot.device_url, "https://github.com/login/device/code");
    assert_eq!(
        copilot.copilot_token_url,
        "https://api.github.com/copilot_internal/v2/token"
    );
    let enterprise = copilot.with_enterprise_domain("company.ghe.com").unwrap();
    assert_eq!(
        enterprise.copilot_token_url,
        "https://api.company.ghe.com/copilot_internal/v2/token"
    );
    let kimi = NativeOAuthConfig::for_provider("kimi-coding").unwrap();
    assert_eq!(kimi.token_url, "https://auth.kimi.com/api/oauth/token");
    let xai = NativeOAuthConfig::for_provider("xai").unwrap();
    assert_eq!(xai.token_url, "https://auth.x.ai/oauth2/token");
    let anthropic = NativeOAuthConfig::for_provider("anthropic").unwrap();
    assert_eq!(anthropic.callback_bind.port(), 53692);
    assert_eq!(
        anthropic.token_url,
        "https://platform.claude.com/v1/oauth/token"
    );
    let openrouter = NativeOAuthConfig::for_provider("openrouter").unwrap();
    assert_eq!(
        openrouter.token_url,
        "https://openrouter.ai/api/v1/auth/keys"
    );
    let mut permanent = credential();
    permanent.refresh.clear();
    assert!(permanent.validate_for_provider("openrouter").is_ok());
    assert!(permanent.validate_for_provider("xai").is_err());
    assert!(credential().validate().is_err());
}
#[test]
fn kimi_and_xai_real_device_grants_send_distinct_clients_scopes_and_tokens() {
    for provider in ["kimi-coding", "xai"] {
        let device = json!({"device_code":"fixture-device","user_code":"CODE","verification_uri":"https://fixture.invalid/verify","verification_uri_complete":"https://fixture.invalid/verify?code=CODE","interval":1,"expires_in":60});
        let (base, worker) = server(vec![
            (200, device),
            (
                200,
                json!({"access_token":"fixture-access","refresh_token":"fixture-refresh","expires_in":3600}),
            ),
        ]);
        let client = NativeOAuthClient::new(config(provider, &base));
        let login = client.begin_device(&CancellationToken::new()).unwrap();
        assert_eq!(login.user_code, "CODE");
        let result = login.finish(&CancellationToken::new()).unwrap();
        assert_eq!(result.access, "fixture-access");
        result.validate_for_provider(provider).unwrap();
        let requests = worker.join().unwrap();
        assert!(requests[0].0.starts_with("POST /device"));
        assert!(requests[1]
            .2
            .contains("grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code"));
        if provider == "xai" {
            assert!(requests[0].2.contains("grok-cli%3Aaccess"));
            assert!(requests[0].2.contains("referrer=pi"));
            assert!(result.expires < timestamp() + 3_400_000);
        } else {
            assert_eq!(
                requests[0].2,
                "client_id=17e5f671-d194-4dfb-9706-5516cb48c098"
            );
            assert!(result.expires > timestamp() + 3_500_000);
        }
    }
}
#[test]
fn copilot_device_exchange_and_models_use_github_token_only_for_exchange() {
    let (base, worker) = server(vec![
        (
            200,
            json!({"device_code":"fixture-device","user_code":"CODE","verification_uri":"https://github.com/login/device","interval":1,"expires_in":60}),
        ),
        (200, json!({"access_token":"fixture-github-token"})),
        (
            200,
            json!({"token":"tid=fixture;proxy-ep=proxy.individual.githubcopilot.com","expires_at":timestamp()/1000+3600}),
        ),
        (
            200,
            json!({"data":[{"id":"gpt-5.5","model_picker_enabled":true,"policy":{"state":"enabled"}},{"id":"disabled","model_picker_enabled":true,"policy":{"state":"disabled"}},{"id":"no-tools","model_picker_enabled":true,"capabilities":{"supports":{"tool_calls":false}}}]}),
        ),
    ]);
    let client = NativeOAuthClient::new(config("github-copilot", &base));
    let result = client
        .begin_device(&CancellationToken::new())
        .unwrap()
        .finish(&CancellationToken::new())
        .unwrap();
    assert_eq!(result.refresh, "fixture-github-token");
    assert_eq!(result.extra["availableModelIds"], json!(["gpt-5.5"]));
    let requests = worker.join().unwrap();
    assert_eq!(
        requests[0].2,
        "client_id=Iv1.b507a08c87ecfe98&scope=read%3Auser"
    );
    assert!(requests[2].0.starts_with("GET /copilot-token"));
    assert_eq!(
        requests[2].1["authorization"],
        "Bearer fixture-github-token"
    );
    assert!(requests[3].1["authorization"].contains("tid=fixture;"));
    assert_eq!(requests[3].1["x-github-api-version"], "2026-06-01");
}
#[test]
fn copilot_policy_enable_is_real_and_retains_only_successful_available_ids() {
    let (base, worker) = server(vec![
        (
            200,
            json!({"device_code":"fixture-device","user_code":"CODE","verification_uri":"https://github.com/login/device","interval":1,"expires_in":60}),
        ),
        (200, json!({"access_token":"fixture-github-token"})),
        (
            200,
            json!({"token":"fixture-copilot-token","expires_at":timestamp()/1000+3600}),
        ),
        (
            200,
            json!({"data":[{"id":"gpt-5.5","model_picker_enabled":true,"policy":{"state":"unconfigured"}}]}),
        ),
        (200, json!({})),
    ]);
    let client = NativeOAuthClient::new(config("github-copilot", &base));
    let result = client
        .begin_device(&CancellationToken::new())
        .unwrap()
        .finish(&CancellationToken::new())
        .unwrap();
    assert_eq!(result.extra["availableModelIds"], json!(["gpt-5.5"]));
    let requests = worker.join().unwrap();
    assert!(requests[4].0.starts_with("POST /models/gpt-5.5/policy"));
    assert_eq!(
        serde_json::from_str::<Value>(&requests[4].2).unwrap(),
        json!({"state":"enabled"})
    );
    assert_eq!(requests[4].1["openai-intent"], "chat-policy");
}
#[test]
fn device_pending_slow_down_and_cancel_are_real_waits_not_success_placeholders() {
    let (base, worker) = server(vec![
        (
            200,
            json!({"device_code":"fixture-device","user_code":"CODE","verification_uri":"https://fixture.invalid","interval":1,"expires_in":60}),
        ),
        (400, json!({"error":"authorization_pending"})),
        (400, json!({"error":"slow_down","interval":1})),
        (
            200,
            json!({"access_token":"fixture-access","refresh_token":"fixture-refresh","expires_in":3600}),
        ),
    ]);
    let client = NativeOAuthClient::new(config("xai", &base));
    let result = client
        .begin_device(&CancellationToken::new())
        .unwrap()
        .finish(&CancellationToken::new())
        .unwrap();
    assert_eq!(result.access, "fixture-access");
    assert_eq!(worker.join().unwrap().len(), 4);
    let (base, worker) = server(vec![(
        200,
        json!({"device_code":"fixture-device","user_code":"CODE","verification_uri":"https://fixture.invalid","interval":60,"expires_in":120}),
    )]);
    let client = NativeOAuthClient::new(config("xai", &base));
    let login = client.begin_device(&CancellationToken::new()).unwrap();
    let signal = CancellationToken::new();
    let copy = signal.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(50));
        copy.cancel();
    });
    let started = Instant::now();
    assert_eq!(login.finish(&signal).err().unwrap(), "Operation aborted");
    assert!(started.elapsed() < Duration::from_secs(1));
    assert_eq!(worker.join().unwrap().len(), 1);
}
#[test]
fn refresh_rotation_retry_and_permanent_openrouter_semantics() {
    for provider in ["anthropic", "kimi-coding", "xai"] {
        let response = if provider == "xai" {
            json!({"access_token":"fixture-new","expires_in":3600})
        } else {
            json!({"access_token":"fixture-new","refresh_token":"fixture-rotated","expires_in":3600})
        };
        let responses = if provider == "kimi-coding" {
            vec![
                (429, json!({"error":"fixture-secret"})),
                (503, json!({})),
                (200, response),
            ]
        } else {
            vec![(200, response)]
        };
        let (base, worker) = server(responses);
        let client = NativeOAuthClient::new(config(provider, &base));
        let result = client
            .refresh(&credential(), &CancellationToken::new())
            .unwrap();
        assert_eq!(
            result.refresh,
            if provider == "xai" {
                "fixture-refresh"
            } else {
                "fixture-rotated"
            }
        );
        assert_eq!(result.extra["unknown"], json!({"preserved":true}));
        let requests = worker.join().unwrap();
        assert_eq!(
            requests.len(),
            if provider == "kimi-coding" { 3 } else { 1 }
        );
        if provider == "anthropic" {
            let body: Value = serde_json::from_str(&requests[0].2).unwrap();
            assert_eq!(body["client_id"], "9d1c250a-e61b-44d9-88ed-5944d1962f5e");
            assert_eq!(body["grant_type"], "refresh_token");
        }
    }
    let mut old = credential();
    old.refresh.clear();
    old.expires = 9007199254740991;
    let client = NativeOAuthClient::new(NativeOAuthConfig::for_provider("openrouter").unwrap());
    let refreshed = client.refresh(&old, &CancellationToken::new()).unwrap();
    assert!(refreshed == old);
}
#[test]
fn anthropic_and_openrouter_pkce_manual_exchange_use_actual_native_json() {
    for provider in ["anthropic", "openrouter"] {
        let response = if provider == "openrouter" {
            json!({"key":"fixture-permanent-key"})
        } else {
            json!({"access_token":"sk-ant-oat-fixture","refresh_token":"fixture-rotated","expires_in":3600})
        };
        let (base, worker) = server(vec![(200, response)]);
        let client = NativeOAuthClient::new(config(provider, &base));
        let login = client.begin_pkce().unwrap();
        let authorization = reqwest::Url::parse(&login.authorization_url).unwrap();
        let query: BTreeMap<_, _> = authorization
            .query_pairs()
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect();
        assert_eq!(query["code_challenge_method"], "S256");
        let result = login
            .finish(Some("fixture-code"), &CancellationToken::new())
            .unwrap();
        let requests = worker.join().unwrap();
        let body: Value = serde_json::from_str(&requests[0].2).unwrap();
        assert_eq!(body["code"], "fixture-code");
        assert_eq!(
            agent_runtime::auth::pkce_challenge(body["code_verifier"].as_str().unwrap()),
            query["code_challenge"]
        );
        if provider == "anthropic" {
            assert_eq!(body["state"], query["state"]);
            assert!(query["scope"].contains("user:inference"));
        } else {
            assert!(result.refresh.is_empty());
            assert_eq!(result.expires, 9007199254740991);
            assert!(query["callback_url"].contains("/oauth/callback/"));
        }
    }
}
#[test]
fn pkce_callback_checks_state_path_and_releases_listener_after_real_exchange() {
    let (base, worker) = server(vec![(
        200,
        json!({"access_token":"sk-ant-oat-fixture","refresh_token":"fixture-rotated","expires_in":3600}),
    )]);
    let client = NativeOAuthClient::new(config("anthropic", &base));
    let login = client.begin_pkce().unwrap();
    let address = login.callback_addr().unwrap();
    let authorize = reqwest::Url::parse(&login.authorization_url).unwrap();
    let state = authorize
        .query_pairs()
        .find(|(key, _)| key == "state")
        .unwrap()
        .1
        .to_string();
    let child = std::thread::spawn(move || login.finish(None, &CancellationToken::new()));
    for (query, expected) in [
        ("/wrong?code=x".into(), "404"),
        ("/callback?code=x&state=wrong".into(), "400"),
        (format!("/callback?code=fixture-code&state={state}"), "200"),
    ] {
        let mut stream = TcpStream::connect(address).unwrap();
        write!(stream, "GET {query} HTTP/1.1\r\nHost: localhost\r\n\r\n").unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        assert!(response.starts_with(&format!("HTTP/1.1 {expected}")));
    }
    assert_eq!(child.join().unwrap().unwrap().access, "sk-ant-oat-fixture");
    assert_eq!(worker.join().unwrap().len(), 1);
    assert!(TcpListener::bind(address).is_ok());
}
#[test]
fn native_storage_refresh_is_singleflight_encrypted_and_preserves_unknown_entries() {
    let directory = temp();
    let path = directory.join("credentials.dat");
    let storage = AuthStorage::new(&path);
    let signal = CancellationToken::new();
    storage
        .write("xai", serde_json::to_value(credential()).unwrap(), &signal)
        .unwrap();
    storage
        .write("unknown", json!({"type":"custom","fixture":true}), &signal)
        .unwrap();
    let (base, worker) = server(vec![(
        200,
        json!({"access_token":"fixture-new","refresh_token":"fixture-rotated","expires_in":3600}),
    )]);
    let client = NativeOAuthClient::new(config("xai", &base));
    let joins: Vec<_> = (0..3)
        .map(|_| {
            let storage = storage.clone();
            let client = client.clone();
            std::thread::spawn(move || {
                storage
                    .ensure_oauth("xai", &client, &CancellationToken::new())
                    .unwrap()
                    .access
            })
        })
        .collect();
    for join in joins {
        assert_eq!(join.join().unwrap(), "fixture-new");
    }
    assert_eq!(worker.join().unwrap().len(), 1);
    assert_eq!(
        storage.read("unknown", &signal).unwrap().unwrap()["fixture"],
        true
    );
    #[cfg(windows)]
    assert!(!String::from_utf8_lossy(&fs::read(&path).unwrap()).contains("fixture-rotated"));
}
#[test]
fn provider_stored_api_key_and_anthropic_oauth_send_native_headers_identity_and_tools() {
    for provider in ["openai", "anthropic"] {
        let directory = temp();
        let storage = AuthStorage::new(directory.join("credentials.dat"));
        let signal = CancellationToken::new();
        let credential = if provider == "openai" {
            json!({"type":"api_key","key":"fixture-api-key"})
        } else {
            json!({"type":"oauth","access":"sk-ant-oat-fixture","refresh":"fixture-refresh","expires":timestamp()+3600000})
        };
        storage.write(provider, credential, &signal).unwrap();
        let (base, worker) = server(vec![(
            403,
            json!({"error":{"message":"fixture-secret-do-not-echo"}}),
        )]);
        let settings = ProviderSettings {
            mode: if provider == "openai" {
                ProviderMode::OpenAiCompatible
            } else {
                ProviderMode::Anthropic
            },
            base_url: base,
            model: "offline-model".into(),
            api_key: String::new(),
            options: ProviderOptions {
                provider_id: provider.into(),
                auth_storage_path: Some(storage.path().into()),
                ..Default::default()
            },
        };
        let mut model = ModelProvider {
            settings,
            tool_definitions: vec![
                json!({"type":"function","function":{"name":"read","parameters":{"type":"object"}}}),
            ],
        };
        let result = model.complete(
            &Context {
                system_prompt: "custom system".into(),
                messages: vec![Message::user("fixture")],
            },
            &signal,
            &mut |_| {},
        );
        let error = result.err().unwrap();
        assert!(error.contains("HTTP 403"));
        assert!(!error.contains("fixture-secret"));
        let requests = worker.join().unwrap();
        if provider == "anthropic" {
            assert_eq!(requests[0].1["authorization"], "Bearer sk-ant-oat-fixture");
            assert!(!requests[0].1.contains_key("x-api-key"));
            assert_eq!(requests[0].1["user-agent"], "claude-cli/2.1.251");
            assert!(requests[0].1["anthropic-beta"].contains("oauth-2025-04-20"));
            let body: Value = serde_json::from_str(&requests[0].2).unwrap();
            assert_eq!(body["tools"][0]["name"], "Read");
            assert_eq!(
                body["system"][0]["text"],
                "You are Claude Code, Anthropic's official CLI for Claude."
            );
        } else {
            assert_eq!(requests[0].1["authorization"], "Bearer fixture-api-key");
        }
    }
}

#[test]
fn in_memory_oauth_rejects_mismatched_refresh_identity_before_any_http() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    for expires in [0, timestamp() + 3600000] {
        let mut original = credential();
        original.expires = expires;
        let before = serde_json::to_value(&original).unwrap();
        let mut provider = ModelProvider {
            settings: ProviderSettings {
                mode: ProviderMode::OpenAiCompatible,
                model: "offline-xai".into(),
                api_key: String::new(),
                base_url: base.clone(),
                options: ProviderOptions {
                    provider_id: "xai".into(),
                    oauth: Some(original),
                    oauth_config: Some(config("anthropic", &base)),
                    ..Default::default()
                },
            },
            tool_definitions: vec![],
        };
        let error = provider
            .complete(&Context::default(), &CancellationToken::new(), &mut |_| {
                panic!("Mismatched OAuth must not stream")
            })
            .unwrap_err();
        assert_eq!(error, "OAuth configuration provider identity mismatch");
        assert_eq!(
            serde_json::to_value(provider.settings.options.oauth.as_ref().unwrap()).unwrap(),
            before
        );
        assert!(matches!(
            listener.accept(),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock
        ));
    }
}

#[test]
fn refresh_failure_and_precommit_cancel_leave_original_storage_bytes_unchanged() {
    let directory = temp();
    let path = directory.join("credentials.dat");
    let storage = AuthStorage::new(&path);
    let signal = CancellationToken::new();
    storage
        .write(
            "anthropic",
            serde_json::to_value(credential()).unwrap(),
            &signal,
        )
        .unwrap();
    let before = fs::read(&path).unwrap();
    let (base, worker) = server(vec![(
        401,
        json!({"error_description":"fixture-sensitive-refresh"}),
    )]);
    let client = NativeOAuthClient::new(config("anthropic", &base));
    let error = storage
        .ensure_oauth("anthropic", &client, &signal)
        .err()
        .unwrap();
    assert!(!error.contains("fixture-sensitive"));
    assert_eq!(fs::read(&path).unwrap(), before);
    worker.join().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let (notify, received) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        request(&mut stream);
        notify.send(()).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        let started = Instant::now();
        let mut byte = [0];
        loop {
            match stream.read(&mut byte) {
                Ok(0) => break,
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                    ) =>
                {
                    assert!(started.elapsed() < Duration::from_secs(3))
                }
                Err(_) => break,
                _ => {}
            }
        }
    });
    let client = NativeOAuthClient::new(config("anthropic", &base));
    let cloned = storage.clone();
    let copy = signal.clone();
    let caller = std::thread::spawn(move || cloned.ensure_oauth("anthropic", &client, &copy));
    received.recv_timeout(Duration::from_secs(3)).unwrap();
    let started = Instant::now();
    signal.cancel();
    assert_eq!(caller.join().unwrap().err().unwrap(), "Operation aborted");
    assert!(started.elapsed() < Duration::from_secs(1));
    worker.join().unwrap();
    assert_eq!(fs::read(&path).unwrap(), before);
}
#[test]
fn callback_wait_and_manual_receiver_are_cancelled_without_network_or_listener_leak() {
    let client = NativeOAuthClient::new(config("openrouter", "http://127.0.0.1:1"));
    let login = client.begin_pkce().unwrap();
    let address = login.callback_addr().unwrap();
    let signal = CancellationToken::new();
    let copy = signal.clone();
    let caller = std::thread::spawn(move || login.finish_with_manual(None, &copy));
    std::thread::sleep(Duration::from_millis(30));
    signal.cancel();
    assert_eq!(caller.join().unwrap().err().unwrap(), "Operation aborted");
    assert!(TcpListener::bind(address).is_ok());
}

#[test]
fn openrouter_denial_callback_settles_without_exchange_or_listener_leak() {
    let token_listener = TcpListener::bind("127.0.0.1:0").unwrap();
    token_listener.set_nonblocking(true).unwrap();
    let base = format!("http://{}", token_listener.local_addr().unwrap());
    let client = NativeOAuthClient::new(config("openrouter", &base));
    let login = client.begin_pkce().unwrap();
    let address = login.callback_addr().unwrap();
    let path = reqwest::Url::parse(&login.redirect_uri)
        .unwrap()
        .path()
        .to_owned();
    let (sender, receiver) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        sender
            .send(login.finish(None, &CancellationToken::new()))
            .unwrap();
    });
    let started = Instant::now();
    let mut stream = TcpStream::connect(address).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    write!(stream, "GET {path}?error=access_denied&error_description=fixture-secret HTTP/1.1\r\nHost: localhost\r\n\r\n").unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    assert!(response.starts_with("HTTP/1.1 400"));
    assert!(!response.contains("fixture-secret"));
    assert_eq!(
        receiver
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .err()
            .unwrap(),
        "OAuth authorization was denied"
    );
    assert!(started.elapsed() < Duration::from_secs(2));
    worker.join().unwrap();
    assert!(TcpListener::bind(address).is_ok());
    assert!(matches!(
        token_listener.accept(),
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock
    ));
}

#[test]
fn validated_http_rotation_survives_late_cancel_after_response_consumption() {
    let directory = temp();
    let path = directory.join("credentials.dat");
    let storage = AuthStorage::new(&path);
    storage
        .write(
            "anthropic",
            serde_json::to_value(credential()).unwrap(),
            &CancellationToken::new(),
        )
        .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let signal = CancellationToken::new();
    let copy = signal.clone();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        request(&mut stream);
        reply(
            &mut stream,
            200,
            json!({"access_token":"sk-ant-oat-fixture-new","refresh_token":"fixture-rotated-after-close","expires_in":3600}),
        );
        // The request has no cancellation until the peer consumes the complete
        // Connection:close response and releases its socket. Cancel only then.
        stream
            .set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        let start = Instant::now();
        let mut byte = [0];
        loop {
            match stream.read(&mut byte) {
                Ok(0) => break,
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                    ) =>
                {
                    assert!(start.elapsed() < Duration::from_secs(3))
                }
                Err(_) => break,
                _ => {}
            }
        }
        copy.cancel();
    });
    let client = NativeOAuthClient::new(config("anthropic", &base));
    let result = storage.ensure_oauth("anthropic", &client, &signal);
    server.join().unwrap();
    assert!(
        result.is_ok()
            || result
                .err()
                .is_some_and(|error| error == "Operation aborted")
    );
    let stored = storage
        .read("anthropic", &CancellationToken::new())
        .unwrap()
        .unwrap();
    assert_eq!(stored["refresh"], "fixture-rotated-after-close");
    assert_eq!(stored["access"], "sk-ant-oat-fixture-new");
}
