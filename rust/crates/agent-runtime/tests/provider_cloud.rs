//! Synthetic cloud credentials and real loopback HTTP only; no external model calls.
use agent_core::agent_loop::{run_agent_loop, LoopConfig, NoopLoopHooks, Provider};
use agent_core::tools::{AgentTool, NoopToolHooks};
use agent_core::types::{CancellationToken, Content, Context, Message, ToolResult, UserMessage};
use agent_runtime::provider::{
    azure_request_payload, parse_azure_sse, parse_vertex_sse, service_account_assertion,
    vertex_request_payload, AzureOptions, ModelProvider, ProviderMode, ProviderOptions,
    ProviderSettings, VertexOptions,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use ring::signature::{UnparsedPublicKey, RSA_PKCS1_2048_8192_SHA256};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fs;
use std::io::{Cursor, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    mpsc,
};
use std::thread;
use std::time::{Duration, Instant};

const KEY: &str = "offline-cloud-fixture-key";
const TOKEN: &str = "offline-cloud-fixture-access-token";
const SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform";
const PRIVATE_KEY: &str = include_str!("fixtures/vertex-service-account.private.pem");
const PUBLIC_KEY: &[u8] = include_bytes!("fixtures/vertex-service-account.public.der");

fn sse(frames: Vec<Value>) -> String {
    frames
        .into_iter()
        .map(|frame| format!("data: {frame}\r\n\r\n"))
        .collect()
}
fn azure_stream(tool: bool) -> String {
    let output = if tool {
        vec![
            json!({"type":"reasoning","id":"rs_fixture","summary":[{"type":"summary_text","text":"思考"}],"encrypted_content":"opaque-azure-fixture"}),
            json!({"type":"function_call","id":"fc_fixture","call_id":"call_fixture","name":"read","arguments":"{\"path\":\"image.png\"}"}),
        ]
    } else {
        vec![
            json!({"type":"message","id":"msg_fixture","role":"assistant","content":[{"type":"output_text","text":"中文完成","annotations":[]}]}),
        ]
    };
    sse(vec![
        json!({"type":"response.completed","response":{"status":"completed","output":output,"usage":{"input_tokens":10,"input_tokens_details":{"cached_tokens":2},"output_tokens":7,"total_tokens":17}}}),
    ])
}
fn vertex_stream(tool: bool) -> String {
    let parts = if tool {
        vec![
            json!({"text":"思考","thought":true,"thoughtSignature":"dGhpbms="}),
            json!({"functionCall":{"id":"call_fixture","name":"read","args":{"path":"image.png"}},"thoughtSignature":"Y2FsbA=="}),
        ]
    } else {
        vec![json!({"text":"中文完成"})]
    };
    sse(vec![
        json!({"candidates":[{"content":{"role":"model","parts":parts},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":10,"cachedContentTokenCount":2,"candidatesTokenCount":3,"thoughtsTokenCount":4,"totalTokenCount":17}}),
    ])
}
fn image() -> Content {
    Content::Image {
        data: "aW1hZ2U=".into(),
        mime_type: "image/png".into(),
    }
}
fn initial() -> Context {
    Context {
        system_prompt: "系统".into(),
        messages: vec![Message::User(UserMessage {
            content: vec![Content::text("读取图片"), image()],
            timestamp: 0,
        })],
    }
}
fn tools() -> Vec<Value> {
    vec![
        json!({"type":"function","function":{"name":"read","description":"Fixture","parameters":{"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}}}),
    ]
}

struct ReadFixture;
impl AgentTool for ReadFixture {
    fn name(&self) -> &str {
        "read"
    }
    fn execute(
        &self,
        _id: &str,
        args: &Value,
        _signal: &CancellationToken,
        _update: &mut dyn FnMut(ToolResult),
    ) -> Result<ToolResult, String> {
        assert_eq!(args["path"], "image.png");
        Ok(ToolResult {
            content: vec![Content::text("图片结果"), image()],
            details: json!({}),
            usage: None,
            added_tool_names: vec![],
            terminate: false,
        })
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
            Ok((socket, _)) => {
                socket.set_nonblocking(false).unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                socket
                    .set_write_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                return socket;
            }
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock
                    && started.elapsed() < Duration::from_secs(3) =>
            {
                thread::sleep(Duration::from_millis(5))
            }
            Err(error) => panic!("Loopback accept: {error}"),
        }
    }
}
fn read_request(socket: &mut TcpStream) -> Request {
    let mut bytes = Vec::new();
    let mut chunk = [0; 4096];
    let (head_end, length) = loop {
        let count = socket.read(&mut chunk).unwrap();
        assert!(count > 0);
        bytes.extend_from_slice(&chunk[..count]);
        assert!(bytes.len() < 2_000_000);
        if let Some(index) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&bytes[..index]);
            let length = head
                .lines()
                .find_map(|line| {
                    line.split_once(':')
                        .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                        .map(|(_, value)| value.trim().parse::<usize>().unwrap())
                })
                .unwrap_or(0);
            break (index + 4, length);
        }
    };
    while bytes.len() < head_end + length {
        let count = socket.read(&mut chunk).unwrap();
        assert!(count > 0);
        bytes.extend_from_slice(&chunk[..count]);
    }
    let head = String::from_utf8_lossy(&bytes[..head_end]);
    Request {
        path: head
            .lines()
            .next()
            .unwrap()
            .split_whitespace()
            .nth(1)
            .unwrap()
            .into(),
        headers: head
            .lines()
            .skip(1)
            .filter_map(|line| {
                line.split_once(':')
                    .map(|(name, value)| (name.to_ascii_lowercase(), value.trim().into()))
            })
            .collect(),
        body: String::from_utf8(bytes[head_end..head_end + length].to_vec()).unwrap(),
    }
}
fn respond(socket: &mut TcpStream, status: u16, kind: &str, body: &str) {
    write!(socket,"HTTP/1.1 {status} Fixture\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();
    socket.flush().unwrap();
}
struct Server {
    address: SocketAddr,
    requests: mpsc::Receiver<Request>,
    handle: thread::JoinHandle<()>,
}
impl Server {
    fn new(responses: Vec<(u16, String)>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (send, requests) = mpsc::channel();
        let handle = thread::spawn(move || {
            for (status, body) in responses {
                let mut socket = accept(&listener);
                let request = read_request(&mut socket);
                respond(
                    &mut socket,
                    status,
                    if body.starts_with("data:") {
                        "text/event-stream"
                    } else {
                        "application/json"
                    },
                    &body,
                );
                send.send(request).unwrap();
            }
        });
        Self {
            address,
            requests,
            handle,
        }
    }
    fn url(&self) -> String {
        format!("http://{}", self.address)
    }
    fn finish(self) -> Vec<Request> {
        self.handle.join().unwrap();
        self.requests.into_iter().collect()
    }
}
struct Temp(PathBuf);
impl Temp {
    fn new(value: Value) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "vertex-adc-fixture-{}-{}-{}.json",
            std::process::id(),
            agent_core::types::timestamp(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::write(&path, value.to_string()).unwrap();
        Self(path)
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}
fn user_adc() -> Value {
    json!({"type":"authorized_user","client_id":"offline-client","client_secret":"offline-client-secret","refresh_token":"offline-refresh","quota_project_id":"fixture-quota"})
}
fn service_adc() -> Value {
    json!({"type":"service_account","client_email":"fixture@offline.invalid","private_key_id":"offline-key-id","private_key":PRIVATE_KEY})
}
fn vertex_options() -> VertexOptions {
    VertexOptions {
        project: "fixture-project".into(),
        location: "us-central1".into(),
        ..Default::default()
    }
}
fn provider(mode: ProviderMode, base: String) -> ModelProvider {
    ModelProvider {
        settings: ProviderSettings {
            mode,
            base_url: base,
            model: "gemini-3-fixture".into(),
            api_key: KEY.into(),
            options: ProviderOptions {
                vertex: vertex_options(),
                ..Default::default()
            },
        },
        tool_definitions: tools(),
    }
}
fn form(body: &str) -> BTreeMap<String, String> {
    reqwest::Url::parse(&format!("http://fixture.invalid/?{body}"))
        .unwrap()
        .query_pairs()
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect()
}
fn verify_assertion(assertion: &str, audience: &str) -> Value {
    let parts: Vec<_> = assertion.split('.').collect();
    assert_eq!(parts.len(), 3);
    let header: Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[0]).unwrap()).unwrap();
    assert_eq!(
        header,
        json!({"alg":"RS256","typ":"JWT","kid":"offline-key-id"})
    );
    let claims: Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[1]).unwrap()).unwrap();
    assert_eq!(claims["iss"], "fixture@offline.invalid");
    assert_eq!(claims["scope"], SCOPE);
    assert_eq!(claims["aud"], audience);
    assert_eq!(
        claims["exp"].as_u64().unwrap() - claims["iat"].as_u64().unwrap(),
        3600
    );
    UnparsedPublicKey::new(&RSA_PKCS1_2048_8192_SHA256, PUBLIC_KEY)
        .verify(
            format!("{}.{}", parts[0], parts[1]).as_bytes(),
            &URL_SAFE_NO_PAD.decode(parts[2]).unwrap(),
        )
        .unwrap();
    claims
}

#[test]
fn azure_native_two_turn_loop_preserves_deployment_auth_images_signed_reasoning_and_usage() {
    let server = Server::new(vec![(200, azure_stream(true)), (200, azure_stream(false))]);
    let mut model = provider(ProviderMode::AzureOpenAiResponses, server.url());
    model.settings.model = "gpt-catalog-model".into();
    model.settings.options.azure.deployment = "fixture-deployment".into();
    let context = initial();
    let messages = run_agent_loop(
        context.messages.clone(),
        &Context {
            system_prompt: context.system_prompt,
            messages: vec![],
        },
        LoopConfig::default(),
        &mut model,
        &[Box::new(ReadFixture)],
        &mut NoopLoopHooks,
        &NoopToolHooks,
        &CancellationToken::new(),
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(messages.last().unwrap().text(), "中文完成");
    let requests = server.finish();
    assert_eq!(requests.len(), 2);
    for request in &requests {
        assert_eq!(request.path, "/openai/v1/responses?api-version=v1");
        assert_eq!(request.headers["api-key"], KEY);
        assert!(!request.headers.contains_key("authorization"));
        assert!(!request.path.contains("deployments"));
        let body: Value = serde_json::from_str(&request.body).unwrap();
        assert_eq!(body["model"], "fixture-deployment");
        assert_eq!(body["tools"][0]["name"], "read");
    }
    let first: Value = serde_json::from_str(&requests[0].body).unwrap();
    assert_eq!(first["input"][0]["content"][1]["type"], "input_image");
    let second: Value = serde_json::from_str(&requests[1].body).unwrap();
    assert_eq!(
        second["input"][1]["encrypted_content"],
        "opaque-azure-fixture"
    );
    assert_eq!(second["input"][2]["call_id"], "call_fixture");
    assert_eq!(second["input"][3]["call_id"], "call_fixture");
    assert_eq!(second["input"][3]["output"][1]["type"], "input_image");
    let Message::Assistant(last) = messages.last().unwrap() else {
        panic!("Assistant required")
    };
    let metadata = last.protocol_metadata.as_ref().unwrap();
    assert_eq!(metadata["model"], "fixture-deployment");
    assert_eq!(metadata["configured_model"], "gpt-catalog-model");
    assert_eq!(metadata["model_identity_verified"], false);
    assert!(metadata["route"]
        .as_str()
        .unwrap()
        .ends_with("#deployment=fixture-deployment"));
    assert_eq!(last.usage.as_ref().unwrap().input, 8);
}

#[test]
fn azure_explicit_bearer_preview_and_endpoint_normalization_match_native_sdk() {
    let server = Server::new(vec![(200, azure_stream(false))]);
    let mut model = provider(
        ProviderMode::AzureOpenAiResponses,
        server.url() + "/openai/",
    );
    model.settings.api_key.clear();
    model.settings.options.azure = AzureOptions {
        bearer_token: TOKEN.into(),
        api_version: "preview".into(),
        ..Default::default()
    };
    model
        .complete(&initial(), &CancellationToken::new(), &mut |_| {})
        .unwrap();
    let request = server.finish().remove(0);
    assert_eq!(request.path, "/openai/v1/responses?api-version=preview");
    assert_eq!(request.headers["authorization"], format!("Bearer {TOKEN}"));
    assert!(!request.headers.contains_key("api-key"));
    let options = AzureOptions {
        resource_name: "fixture-resource".into(),
        ..Default::default()
    };
    assert_eq!(
        options.resolved_base_url("").unwrap().as_str(),
        "https://fixture-resource.openai.azure.com/openai/v1"
    );
    assert_eq!(
        options
            .resolved_base_url("https://fixture.openai.azure.com/openai/v1/responses")
            .unwrap()
            .path(),
        "/openai/v1"
    );
}

#[test]
fn azure_signature_replay_requires_same_effective_deployment_and_route() {
    let server = Server::new(vec![
        (200, azure_stream(true)),
        (200, azure_stream(false)),
        (200, azure_stream(false)),
    ]);
    let mut model = provider(ProviderMode::AzureOpenAiResponses, server.url());
    model.settings.options.azure.deployment = "first-deployment".into();
    let assistant = model
        .complete(&initial(), &CancellationToken::new(), &mut |_| {})
        .unwrap();
    let history = Context {
        system_prompt: String::new(),
        messages: vec![Message::Assistant(assistant.clone())],
    };
    model.settings.options.azure.deployment = "second-deployment".into();
    model
        .complete(&history, &CancellationToken::new(), &mut |_| {})
        .unwrap();
    model.settings.options.azure.deployment = "first-deployment".into();
    model.settings.base_url.push_str("/other-route");
    model
        .complete(&history, &CancellationToken::new(), &mut |_| {})
        .unwrap();
    let requests = server.finish();
    for request in requests.iter().skip(1) {
        assert!(!request.body.contains("opaque-azure-fixture"));
        assert!(!request.body.contains("fc_fixture"));
    }
    let mut options = AzureOptions::default();
    options
        .deployment_map
        .insert("catalog-id".into(), "mapped-deployment".into());
    assert_eq!(
        options.effective_deployment("catalog-id"),
        "mapped-deployment"
    );
    options.deployment = "override".into();
    assert_eq!(options.effective_deployment("catalog-id"), "override");
    let mut assistant = assistant;
    assistant.protocol_metadata.as_mut().unwrap()["model"] = json!("override");
    let payload = azure_request_payload(
        &Context {
            messages: vec![Message::Assistant(assistant)],
            ..Default::default()
        },
        "catalog-id",
        &[],
        &options,
    );
    assert_eq!(payload["model"], "override");
}

#[test]
fn vertex_express_key_loop_uses_publisher_route_and_native_function_images() {
    let server = Server::new(vec![
        (200, vertex_stream(true)),
        (200, vertex_stream(false)),
    ]);
    let mut model = provider(ProviderMode::GoogleVertex, server.url());
    let context = initial();
    let messages = run_agent_loop(
        context.messages.clone(),
        &Context {
            system_prompt: context.system_prompt,
            messages: vec![],
        },
        LoopConfig::default(),
        &mut model,
        &[Box::new(ReadFixture)],
        &mut NoopLoopHooks,
        &NoopToolHooks,
        &CancellationToken::new(),
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(messages.last().unwrap().text(), "中文完成");
    let requests = server.finish();
    for request in &requests {
        assert_eq!(
            request.path,
            "/v1/publishers/google/models/gemini-3-fixture:streamGenerateContent?alt=sse"
        );
        assert_eq!(request.headers["x-goog-api-key"], KEY);
        assert!(!request.headers.contains_key("authorization"));
    }
    let second: Value = serde_json::from_str(&requests[1].body).unwrap();
    assert_eq!(second["contents"][1]["parts"][0]["thought"], true);
    assert_eq!(
        second["contents"][1]["parts"][1]["thoughtSignature"],
        "Y2FsbA=="
    );
    assert_eq!(
        second["contents"][2]["parts"][0]["functionResponse"]["id"],
        "call_fixture"
    );
    assert_eq!(
        second["contents"][2]["parts"][0]["functionResponse"]["parts"][0]["inlineData"]["data"],
        "aW1hZ2U="
    );
    let Message::Assistant(last) = messages.last().unwrap() else {
        panic!("Assistant required")
    };
    assert_eq!(
        last.protocol_metadata.as_ref().unwrap()["protocol"],
        "google-vertex"
    );
    assert_eq!(last.usage.as_ref().unwrap().output, 7);
}

#[test]
fn vertex_explicit_bearer_uses_full_project_location_route_and_identity() {
    let server = Server::new(vec![(200, vertex_stream(false))]);
    let mut model = provider(
        ProviderMode::GoogleVertex,
        server.url() + "/v1beta1/projects/fixture-project/locations/us-central1",
    );
    model.settings.api_key = "gcp-vertex-credentials".into();
    model.settings.options.vertex.access_token = TOKEN.into();
    let message = model
        .complete(&initial(), &CancellationToken::new(), &mut |_| {})
        .unwrap();
    let request = server.finish().remove(0);
    assert_eq!(request.path,"/v1beta1/projects/fixture-project/locations/us-central1/publishers/google/models/gemini-3-fixture:streamGenerateContent?alt=sse");
    assert_eq!(request.headers["authorization"], format!("Bearer {TOKEN}"));
    assert!(!request.headers.contains_key("x-goog-api-key"));
    assert!(message.protocol_metadata.as_ref().unwrap()["route"]
        .as_str()
        .unwrap()
        .contains("projects/fixture-project/locations/us-central1/"));
    for (location, host) in [
        ("global", "aiplatform.googleapis.com"),
        ("us", "aiplatform.us.rep.googleapis.com"),
        ("eu", "aiplatform.eu.rep.googleapis.com"),
        ("us-central1", "us-central1-aiplatform.googleapis.com"),
    ] {
        let options = VertexOptions {
            location: location.into(),
            ..vertex_options()
        };
        assert_eq!(
            options
                .resolved_base_url("https://aiplatform.googleapis.com", false)
                .unwrap()
                .host_str()
                .unwrap(),
            host
        );
        assert_eq!(
            options
                .resolved_base_url("", true)
                .unwrap()
                .host_str()
                .unwrap(),
            "aiplatform.googleapis.com"
        );
    }
}

#[test]
fn vertex_adc_authorized_user_refreshes_caches_quota_header_without_modifying_credentials() {
    let token_server = Server::new(vec![(
        200,
        json!({"access_token":TOKEN,"expires_in":3600,"token_type":"Bearer"}).to_string(),
    )]);
    let model_server = Server::new(vec![
        (200, vertex_stream(false)),
        (200, vertex_stream(false)),
    ]);
    let adc = Temp::new(user_adc());
    let original = fs::read(&adc.0).unwrap();
    let mut model = provider(ProviderMode::GoogleVertex, model_server.url());
    model.settings.api_key.clear();
    model.settings.options.vertex.adc_path = Some(adc.0.clone());
    model.settings.options.vertex.token_url = token_server.url() + "/token";
    for _ in 0..2 {
        model
            .complete(&initial(), &CancellationToken::new(), &mut |_| {})
            .unwrap();
    }
    let request = token_server.finish().remove(0);
    assert_eq!(request.path, "/token");
    assert_eq!(
        request.headers["content-type"],
        "application/x-www-form-urlencoded"
    );
    assert_eq!(
        form(&request.body),
        BTreeMap::from([
            ("grant_type".into(), "refresh_token".into()),
            ("client_id".into(), "offline-client".into()),
            ("client_secret".into(), "offline-client-secret".into()),
            ("refresh_token".into(), "offline-refresh".into())
        ])
    );
    for request in model_server.finish() {
        assert_eq!(request.headers["authorization"], format!("Bearer {TOKEN}"));
        assert_eq!(request.headers["x-goog-user-project"], "fixture-quota");
        assert!(!request.body.contains("offline-client-secret"));
    }
    assert_eq!(fs::read(&adc.0).unwrap(), original);
}

#[test]
fn vertex_service_account_signed_jwt_is_real_rs256_with_observed_claims() {
    let assertion =
        service_account_assertion(&service_adc(), "https://oauth2.googleapis.com/token", 1234)
            .unwrap();
    let claims = verify_assertion(&assertion, "https://oauth2.googleapis.com/token");
    assert_eq!(claims["iat"], 1234);
    assert_eq!(claims["exp"], 4834);
    let parts: Vec<_> = assertion.split('.').collect();
    let changed = format!("{}.{}x", parts[0], parts[1]);
    assert!(
        UnparsedPublicKey::new(&RSA_PKCS1_2048_8192_SHA256, PUBLIC_KEY)
            .verify(
                changed.as_bytes(),
                &URL_SAFE_NO_PAD.decode(parts[2]).unwrap()
            )
            .is_err()
    );
}

#[test]
fn vertex_service_account_exchanges_assertion_and_completes_model_request() {
    let token_server = Server::new(vec![(
        200,
        json!({"access_token":TOKEN,"expires_in":3600}).to_string(),
    )]);
    let model_server = Server::new(vec![(200, vertex_stream(false))]);
    let adc = Temp::new(service_adc());
    let original = fs::read(&adc.0).unwrap();
    let mut model = provider(ProviderMode::GoogleVertex, model_server.url());
    model.settings.api_key.clear();
    model.settings.options.vertex = VertexOptions {
        adc_path: Some(adc.0.clone()),
        token_url: token_server.url() + "/token",
        ..vertex_options()
    };
    let message = model
        .complete(&initial(), &CancellationToken::new(), &mut |_| {})
        .unwrap();
    assert_eq!(Message::Assistant(message).text(), "中文完成");
    let request = token_server.finish().remove(0);
    let fields = form(&request.body);
    assert_eq!(
        fields["grant_type"],
        "urn:ietf:params:oauth:grant-type:jwt-bearer"
    );
    verify_assertion(
        &fields["assertion"],
        &model.settings.options.vertex.token_url,
    );
    assert_eq!(
        model_server.finish().remove(0).headers["authorization"],
        format!("Bearer {TOKEN}")
    );
    assert_eq!(fs::read(&adc.0).unwrap(), original);
}

#[test]
fn cloud_options_do_not_serialize_secrets_or_read_adc_during_validation() {
    let azure = AzureOptions {
        bearer_token: TOKEN.into(),
        resource_name: "fixture".into(),
        ..Default::default()
    };
    let serialized = serde_json::to_string(&azure).unwrap();
    assert!(!serialized.contains(TOKEN));
    assert!(!serialized.contains("bearerToken"));
    let parsed: AzureOptions = serde_json::from_value(json!({"bearerToken":TOKEN})).unwrap();
    assert_eq!(parsed.bearer_token, TOKEN);
    let vertex = VertexOptions {
        access_token: TOKEN.into(),
        adc_path: Some(PathBuf::from("missing-offline-fixture.json")),
        ..vertex_options()
    };
    assert!(!serde_json::to_string(&vertex).unwrap().contains(TOKEN));
    vertex
        .validate("http://127.0.0.1:1", "gemini-fixture", "")
        .unwrap();
    assert!(VertexOptions::default().adc_path.is_none());
    assert!(VertexOptions::default().access_token.is_empty());
}

#[test]
fn cloud_validation_rejects_ambiguous_credentials_path_traversal_and_header_injection() {
    let azure = AzureOptions {
        resource_name: "fixture".into(),
        bearer_token: TOKEN.into(),
        ..Default::default()
    };
    assert!(azure.validate("", "model", KEY).is_err());
    assert!(azure.validate("", "../deployment", "").is_err());
    assert!(AzureOptions {
        bearer_token: "x\r\nsecret".into(),
        ..azure
    }
    .validate("", "model", "")
    .is_err());
    let vertex = VertexOptions {
        access_token: TOKEN.into(),
        ..vertex_options()
    };
    assert!(vertex.validate("", "model", KEY).is_err());
    assert!(vertex.validate("", "../model", "").is_err());
    assert!(VertexOptions {
        project: "../project".into(),
        ..vertex.clone()
    }
    .validate("", "model", "")
    .is_err());
    assert!(VertexOptions {
        token_url: "http://remote.invalid/token".into(),
        ..vertex
    }
    .validate("", "model", "")
    .is_err());
}

#[test]
fn vertex_adc_unsupported_types_and_invalid_service_account_keys_fail_without_network() {
    for value in [
        json!({"type":"external_account","refresh_token":"raw-secret-material-fixture"}),
        json!({"type":"impersonated_service_account"}),
        json!({"type":"unknown"}),
        json!({"type":"service_account","private_key":"raw-secret-material-fixture","client_email":"fixture@offline.invalid"}),
        json!({"type":"authorized_user","client_id":"offline"}),
    ] {
        let adc = Temp::new(value);
        let options = VertexOptions {
            adc_path: Some(adc.0.clone()),
            token_url: "http://127.0.0.1:1/token".into(),
            ..vertex_options()
        };
        let error = options
            .authorize("", &CancellationToken::new())
            .err()
            .unwrap();
        assert!(!error.contains("raw-secret-material-fixture"));
        assert!(!error.contains(PRIVATE_KEY));
    }
    assert!(service_account_assertion(
        &service_adc(),
        "https://oauth2.googleapis.com/token",
        u64::MAX
    )
    .is_err());
}

#[test]
fn vertex_adc_token_errors_redact_server_body_and_credentials() {
    for (status, response) in [
        (
            400,
            json!({"error":"invalid_grant","error_description":"offline-refresh offline-client-secret secret-body"}),
        ),
        (200, json!({"access_token":TOKEN,"expires_in":0})),
        (
            200,
            json!({"access_token":TOKEN,"expires_in":3600,"token_type":"not-bearer"}),
        ),
        (
            200,
            json!({"access_token":"x\r\nsecret-body","expires_in":3600}),
        ),
    ] {
        let server = Server::new(vec![(status, response.to_string())]);
        let adc = Temp::new(user_adc());
        let options = VertexOptions {
            adc_path: Some(adc.0.clone()),
            token_url: server.url() + "/token",
            ..vertex_options()
        };
        let error = options
            .authorize("", &CancellationToken::new())
            .err()
            .unwrap();
        for secret in [
            "offline-refresh",
            "offline-client-secret",
            "secret-body",
            TOKEN,
        ] {
            assert!(!error.contains(secret));
        }
        assert_eq!(server.finish().len(), 1);
    }
}

#[test]
fn cloud_sse_strictly_rejects_truncation_invalid_arguments_and_cancellation() {
    for body in [
        "".to_owned(),
        sse(vec![
            json!({"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"fc_x","call_id":"call_x","name":"read","arguments":""}}),
        ]),
        sse(vec![
            json!({"type":"response.completed","response":{"status":"completed","output":[{"type":"function_call","id":"fc_x","call_id":"call_x","name":"read","arguments":"[]"}]}}),
        ]),
    ] {
        assert!(
            parse_azure_sse(Cursor::new(body), &CancellationToken::new(), &mut |_| {}).is_err()
        );
    }
    for body in [
        "".to_owned(),
        sse(vec![
            json!({"candidates":[{"content":{"parts":[{"text":"partial"}]}}]}),
        ]),
        sse(vec![
            json!({"candidates":[{"content":{"parts":[{"functionCall":{"name":"read","args":[]}}]},"finishReason":"STOP"}]}),
        ]),
    ] {
        assert!(
            parse_vertex_sse(Cursor::new(body), &CancellationToken::new(), &mut |_| {}).is_err()
        );
    }
    let signal = CancellationToken::new();
    signal.cancel();
    assert!(
        parse_vertex_sse(Cursor::new(vertex_stream(false)), &signal, &mut |_| {})
            .unwrap_err()
            .contains("aborted")
    );
    assert!(
        parse_azure_sse(Cursor::new(azure_stream(false)), &signal, &mut |_| {})
            .unwrap_err()
            .contains("aborted")
    );
    let mut assistant = parse_vertex_sse(
        Cursor::new(vertex_stream(true)),
        &CancellationToken::new(),
        &mut |_| {},
    )
    .unwrap();
    assistant.protocol_metadata.as_mut().unwrap()["model"] = json!("gemini-3-fixture");
    let context = Context {
        messages: vec![Message::Assistant(assistant)],
        ..Default::default()
    };
    assert!(vertex_request_payload(&context, "gemini-3-fixture", &[])
        .to_string()
        .contains("dGhpbms="));
    assert!(!vertex_request_payload(&context, "other-model", &[])
        .to_string()
        .contains("dGhpbms="));
}

fn stall_server() -> (String, mpsc::Receiver<()>, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let (send, accepted) = mpsc::channel();
    let handle = thread::spawn(move || {
        let mut socket = accept(&listener);
        read_request(&mut socket);
        send.send(()).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_millis(25)))
            .unwrap();
        let start = Instant::now();
        let mut byte = [0];
        loop {
            match socket.read(&mut byte) {
                Ok(0) => break,
                Ok(_) => {}
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                    ) && start.elapsed() < Duration::from_secs(2) => {}
                Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => break,
                other => panic!("Cancelled client did not close socket: {other:?}"),
            }
        }
    });
    (format!("http://{address}"), accepted, handle)
}
#[test]
fn vertex_adc_refresh_cancellation_closes_stalled_token_socket_without_model_request() {
    let (url, accepted, server) = stall_server();
    let adc = Temp::new(user_adc());
    let original = fs::read(&adc.0).unwrap();
    let options = VertexOptions {
        adc_path: Some(adc.0.clone()),
        token_url: url + "/token",
        ..vertex_options()
    };
    let model_listener = TcpListener::bind("127.0.0.1:0").unwrap();
    model_listener.set_nonblocking(true).unwrap();
    let mut model = provider(
        ProviderMode::GoogleVertex,
        format!("http://{}", model_listener.local_addr().unwrap()),
    );
    model.settings.api_key.clear();
    model.settings.options.vertex = options;
    let signal = CancellationToken::new();
    let worker_signal = signal.clone();
    let worker = thread::spawn(move || {
        model
            .complete(&initial(), &worker_signal, &mut |_| {})
            .unwrap_err()
    });
    accepted.recv_timeout(Duration::from_secs(3)).unwrap();
    let start = Instant::now();
    thread::sleep(Duration::from_millis(50));
    signal.cancel();
    assert!(worker.join().unwrap().contains("aborted"));
    assert!(start.elapsed() < Duration::from_secs(2));
    server.join().unwrap();
    assert_eq!(fs::read(&adc.0).unwrap(), original);
    assert!(
        matches!(model_listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock)
    );
}

#[test]
fn azure_and_vertex_model_cancellation_close_stalled_header_sockets() {
    for mode in [
        ProviderMode::AzureOpenAiResponses,
        ProviderMode::GoogleVertex,
    ] {
        let (url, accepted, server) = stall_server();
        let mut model = provider(mode, url);
        let signal = CancellationToken::new();
        let worker_signal = signal.clone();
        let worker = thread::spawn(move || {
            model
                .complete(&initial(), &worker_signal, &mut |_| {})
                .unwrap_err()
        });
        accepted.recv_timeout(Duration::from_secs(3)).unwrap();
        let start = Instant::now();
        thread::sleep(Duration::from_millis(50));
        signal.cancel();
        assert!(worker.join().unwrap().contains("aborted"));
        assert!(start.elapsed() < Duration::from_secs(2));
        server.join().unwrap();
    }
}

#[test]
fn cloud_http_errors_preserve_only_status_and_known_codes() {
    for mode in [
        ProviderMode::AzureOpenAiResponses,
        ProviderMode::GoogleVertex,
    ] {
        let server = Server::new(vec![(
            429,
            json!({"error":{"code":"insufficient_quota","message":format!("secret-body {KEY}")}})
                .to_string(),
        )]);
        let mut model = provider(mode, server.url());
        let error = model
            .complete(&initial(), &CancellationToken::new(), &mut |_| {})
            .unwrap_err();
        assert!(error.contains("HTTP 429"));
        assert!(error.contains("insufficient_quota"));
        assert!(!error.contains(KEY));
        assert!(!error.contains("secret-body"));
        server.finish();
    }
    let server = Server::new(vec![(
        400,
        json!({"code":"context_length_exceeded","message":format!("secret-body {KEY}")})
            .to_string(),
    )]);
    let mut model = provider(ProviderMode::AzureOpenAiResponses, server.url());
    let error = model
        .complete(&initial(), &CancellationToken::new(), &mut |_| {})
        .unwrap_err();
    assert!(error.contains("context_length_exceeded"));
    assert!(!error.contains("secret-body"));
    assert!(!error.contains(KEY));
    server.finish();
}

#[test]
fn vertex_identity_gate_prevents_signature_replay_between_projects() {
    let server = Server::new(vec![
        (200, vertex_stream(true)),
        (200, vertex_stream(false)),
    ]);
    let mut model = provider(ProviderMode::GoogleVertex, server.url());
    model.settings.api_key.clear();
    model.settings.options.vertex.access_token = TOKEN.into();
    let assistant = model
        .complete(&initial(), &CancellationToken::new(), &mut |_| {})
        .unwrap();
    model.settings.options.vertex.project = "different-project".into();
    model
        .complete(
            &Context {
                messages: vec![Message::Assistant(assistant)],
                ..Default::default()
            },
            &CancellationToken::new(),
            &mut |_| {},
        )
        .unwrap();
    let requests = server.finish();
    assert!(!requests[1].body.contains("dGhpbms="));
    assert!(!requests[1].body.contains("Y2FsbA=="));
    assert!(model
        .settings
        .options
        .vertex
        .request_url("", &model.settings.model, "")
        .unwrap()
        .path()
        .contains("projects/different-project/"));
}

#[test]
fn vertex_adc_refreshes_near_expiry_and_changed_file_fingerprints() {
    let server = Server::new(vec![
        (
            200,
            json!({"access_token":"first-fixture","expires_in":30}).to_string(),
        ),
        (
            200,
            json!({"access_token":"second-fixture","expires_in":3600}).to_string(),
        ),
        (
            200,
            json!({"access_token":"third-fixture","expires_in":3600}).to_string(),
        ),
    ]);
    let adc = Temp::new(user_adc());
    let options = VertexOptions {
        adc_path: Some(adc.0.clone()),
        token_url: server.url() + "/token",
        ..vertex_options()
    };
    assert_eq!(
        options
            .authorize("", &CancellationToken::new())
            .unwrap()
            .access_token
            .as_deref(),
        Some("first-fixture")
    );
    assert_eq!(
        options
            .authorize("", &CancellationToken::new())
            .unwrap()
            .access_token
            .as_deref(),
        Some("second-fixture")
    );
    let mut changed = user_adc();
    changed["refresh_token"] = json!("rotated-fixture-refresh");
    fs::write(&adc.0, changed.to_string()).unwrap();
    assert_eq!(
        options
            .authorize("", &CancellationToken::new())
            .unwrap()
            .access_token
            .as_deref(),
        Some("third-fixture")
    );
    let requests = server.finish();
    assert_eq!(requests.len(), 3);
    assert_eq!(
        form(&requests[2].body)["refresh_token"],
        "rotated-fixture-refresh"
    );
}

#[test]
fn vertex_concurrent_adc_requests_coalesce_one_refresh() {
    let server = Server::new(vec![(
        200,
        json!({"access_token":TOKEN,"expires_in":3600}).to_string(),
    )]);
    let adc = Temp::new(user_adc());
    let options = VertexOptions {
        adc_path: Some(adc.0.clone()),
        token_url: server.url() + "/token",
        ..vertex_options()
    };
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
    let workers: Vec<_> = (0..2)
        .map(|_| {
            let options = options.clone();
            let barrier = barrier.clone();
            thread::spawn(move || {
                barrier.wait();
                options
                    .authorize("", &CancellationToken::new())
                    .unwrap()
                    .access_token
                    .unwrap()
            })
        })
        .collect();
    barrier.wait();
    for worker in workers {
        assert_eq!(worker.join().unwrap(), TOKEN);
    }
    assert_eq!(server.finish().len(), 1);
}

#[test]
fn vertex_cancellation_also_interrupts_waiting_for_another_refresh() {
    let (url, accepted, server) = stall_server();
    let adc = Temp::new(user_adc());
    let options = VertexOptions {
        adc_path: Some(adc.0.clone()),
        token_url: url + "/token",
        ..vertex_options()
    };
    let first_signal = CancellationToken::new();
    let worker_signal = first_signal.clone();
    let first_options = options.clone();
    let first = thread::spawn(move || first_options.authorize("", &worker_signal).err().unwrap());
    accepted.recv_timeout(Duration::from_secs(3)).unwrap();
    let second_signal = CancellationToken::new();
    let worker_signal = second_signal.clone();
    let second = thread::spawn(move || options.authorize("", &worker_signal).err().unwrap());
    thread::sleep(Duration::from_millis(50));
    let start = Instant::now();
    second_signal.cancel();
    assert!(second.join().unwrap().contains("aborted"));
    assert!(start.elapsed() < Duration::from_secs(2));
    first_signal.cancel();
    assert!(first.join().unwrap().contains("aborted"));
    server.join().unwrap();
}

#[test]
fn vertex_default_project_routes_and_custom_collection_routes_match_sdk() {
    let options = vertex_options();
    assert_eq!(options.request_url("https://{location}-aiplatform.googleapis.com","gemini-fixture","").unwrap().as_str(),"https://us-central1-aiplatform.googleapis.com/v1/projects/fixture-project/locations/us-central1/publishers/google/models/gemini-fixture:streamGenerateContent?alt=sse");
    assert_eq!(
        options
            .request_url("http://127.0.0.1:1/proxy/v1beta", "gemini-fixture", "")
            .unwrap()
            .path(),
        "/proxy/v1beta/publishers/google/models/gemini-fixture:streamGenerateContent"
    );
    assert_eq!(options.request_url("http://127.0.0.1:1/v1/projects/fixture-project/locations/us-central1","publishers/google/models/gemini-fixture","").unwrap().path(),"/v1/projects/fixture-project/locations/us-central1/publishers/google/models/gemini-fixture:streamGenerateContent");
}

#[test]
fn vertex_publisher_prefixed_legacy_models_do_not_replay_unsupported_function_ids() {
    let mut assistant = parse_vertex_sse(
        Cursor::new(vertex_stream(true)),
        &CancellationToken::new(),
        &mut |_| {},
    )
    .unwrap();
    assistant.protocol_metadata.as_mut().unwrap()["model"] =
        json!("publishers/google/models/gemini-2.5-flash");
    let payload = vertex_request_payload(
        &Context {
            messages: vec![Message::Assistant(assistant)],
            ..Default::default()
        },
        "publishers/google/models/gemini-2.5-flash",
        &[],
    );
    assert!(payload["contents"][0]["parts"][1]["functionCall"]
        .get("id")
        .is_none());
    assert_eq!(
        payload["contents"][0]["parts"][1]["thoughtSignature"],
        "Y2FsbA=="
    );
}
