use agent_core::agent_loop::{run_agent_loop, LoopConfig, NoopLoopHooks, Provider};
use agent_core::tools::{AgentTool, NoopToolHooks};
use agent_core::types::{
    AssistantMessage, CancellationToken, Content, Context, Message, StopReason, ToolCall,
    ToolResult, ToolResultMessage, Usage,
};
use agent_runtime::provider::{
    mistral_request_payload, parse_mistral_sse, parse_pi_messages_sse, pi_messages_request_payload,
    CloudflareOptions, CopilotOptions, MistralOptions, ModelProvider, PiMessagesOptions,
    ProviderMode, ProviderOptions, ProviderSettings,
};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::{Cursor, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

fn events(values: &[Value]) -> String {
    values
        .iter()
        .map(|value| format!("data: {value}\n\n"))
        .collect()
}
fn context() -> Context {
    Context {
        system_prompt: "offline system".into(),
        messages: vec![Message::user("你好")],
    }
}
fn usage() -> Value {
    json!({"input":16,"output":3,"cacheRead":8,"cacheWrite":0,"totalTokens":27,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}})
}
fn pi_stream() -> String {
    events(&[
        json!({"type":"start"}),
        json!({"type":"thinking_start","contentIndex":0}),
        json!({"type":"thinking_delta","contentIndex":0,"delta":"思考"}),
        json!({"type":"thinking_end","contentIndex":0,"content":"思考","contentSignature":"opaque-fixture"}),
        json!({"type":"text_start","contentIndex":1}),
        json!({"type":"text_delta","contentIndex":1,"delta":"中文回答"}),
        json!({"type":"text_end","contentIndex":1,"content":"中文回答"}),
        json!({"type":"done","reason":"stop","usage":usage(),"responseId":"pi-fixture","rewrite":{"policyId":"offline","policyVersion":1,"changed":true,"tokenCountChange":-1,"messageCountChange":0,"systemPromptChanged":false}}),
    ])
}
fn mistral_stream() -> String {
    events(&[
        json!({"id":"mistral-fixture","choices":[{"delta":{"content":[{"type":"thinking","thinking":[{"type":"text","text":"思考"}]}]},"finish_reason":null}]}),
        json!({"choices":[{"delta":{"content":[{"type":"text","text":"中文回答"}]},"finish_reason":"stop"}],"usage":{"prompt_tokens":24,"completion_tokens":3,"total_tokens":27,"num_cached_tokens":8}}),
    ])
}
fn tool_result(id: &str) -> Message {
    Message::ToolResult(ToolResultMessage {
        tool_call_id: id.into(),
        tool_name: "read".into(),
        content: vec![
            Content::text("tool output"),
            Content::Image {
                data: "aA==".into(),
                mime_type: "image/png".into(),
            },
        ],
        details: json!({}),
        usage: None,
        added_tool_names: vec![],
        is_error: false,
        timestamp: 1,
    })
}

#[test]
fn mistral_native_thinking_cache_usage_and_terminal_reason() {
    let result = parse_mistral_sse(
        Cursor::new(mistral_stream()),
        &CancellationToken::new(),
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(result.stop_reason, StopReason::Stop);
    assert!(matches!(&result.content[0],Content::Thinking{thinking}if thinking=="思考"));
    assert_eq!(
        result.usage,
        Some(Usage {
            input: 16,
            output: 3,
            cache_read: 8,
            cache_write: 0,
            total_tokens: 27
        })
    );
}
#[test]
fn actual_original_ts_oracle_payload_and_parsed_usage_agree_with_native() {
    let oracle: Value =
        serde_json::from_str(include_str!("fixtures/native-protocols.json")).unwrap();
    for case in oracle["cases"].as_array().unwrap() {
        let context: Context = serde_json::from_value(case["context"].clone()).unwrap();
        let tools = case["context"]["tools"].as_array().unwrap();
        let options = MistralOptions {
            temperature: Some(0.2),
            max_tokens: Some(100),
            session_id: Some("session-fixture".into()),
            cache_retention: Some("short".into()),
            ..MistralOptions::default()
        };
        let (payload, result) = if case["model"]["api"] == "mistral-conversations" {
            (
                mistral_request_payload(&context, "fixture-model", tools, &options).unwrap(),
                parse_mistral_sse(
                    Cursor::new(case["sse"].as_str().unwrap()),
                    &CancellationToken::new(),
                    &mut |_| {},
                )
                .unwrap(),
            )
        } else {
            let options = PiMessagesOptions {
                temperature: Some(0.2),
                max_tokens: Some(100),
                session_id: Some("session-fixture".into()),
                cache_retention: Some("short".into()),
                ..PiMessagesOptions::default()
            };
            (
                pi_messages_request_payload(&context, "fixture-model", tools, &options).unwrap(),
                parse_pi_messages_sse(
                    Cursor::new(case["sse"].as_str().unwrap()),
                    &CancellationToken::new(),
                    &mut |_| {},
                )
                .unwrap(),
            )
        };
        assert_eq!(payload, case["request"]["payload"]);
        let usage = serde_json::to_value(result.usage.unwrap()).unwrap();
        for name in ["input", "output", "cacheRead", "cacheWrite", "totalTokens"] {
            assert_eq!(usage[name], case["result"]["usage"][name]);
        }
    }
}
#[test]
fn mistral_native_tool_json_round_trip_uses_nine_character_paired_ids() {
    let stream = events(&[
        json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"Abcd12345","function":{"name":"read","arguments":"{\"path\":"}}]}}]}),
        json!({"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"fixture\"}"}}]},"finish_reason":"tool_calls"}]}),
    ]);
    let result =
        parse_mistral_sse(Cursor::new(stream), &CancellationToken::new(), &mut |_| {}).unwrap();
    let mut context = context();
    context.messages.push(Message::Assistant(result));
    context.messages.push(tool_result("Abcd12345"));
    let payload =
        mistral_request_payload(&context, "fixture", &[], &MistralOptions::default()).unwrap();
    let calls = &payload["messages"][2]["tool_calls"];
    assert_eq!(calls[0]["id"], payload["messages"][3]["tool_call_id"]);
    assert_eq!(calls[0]["id"].as_str().unwrap().len(), 9);
    assert_eq!(payload["messages"][3]["content"][1]["type"], "image_url");
}
#[test]
fn mistral_long_unicode_call_ids_remain_paired_and_distinct() {
    let mut context = context();
    let mut assistant = AssistantMessage::text("prior");
    assistant.content.extend([
        Content::ToolCall(ToolCall {
            id: "call|長い/id-a".into(),
            name: "read".into(),
            arguments: json!({}),
        }),
        Content::ToolCall(ToolCall {
            id: "call|長い/id-b".into(),
            name: "read".into(),
            arguments: json!({}),
        }),
    ]);
    context.messages.push(Message::Assistant(assistant));
    context.messages.push(tool_result("call|長い/id-a"));
    context.messages.push(tool_result("call|長い/id-b"));
    let payload =
        mistral_request_payload(&context, "fixture", &[], &MistralOptions::default()).unwrap();
    let ids = &payload["messages"][2]["tool_calls"];
    assert_eq!(ids[0]["id"], payload["messages"][3]["tool_call_id"]);
    assert_eq!(ids[1]["id"], payload["messages"][4]["tool_call_id"]);
    assert_ne!(ids[0]["id"], ids[1]["id"]);
}
#[test]
fn mistral_options_are_real_wire_fields_not_unused_dto() {
    let options = MistralOptions {
        temperature: Some(0.2),
        max_tokens: Some(100),
        tool_choice: Some(json!("any")),
        reasoning_effort: Some("high".into()),
        prompt_mode: Some("reasoning".into()),
        session_id: Some("session-fixture".into()),
        cache_retention: Some("short".into()),
    };
    let payload = mistral_request_payload(&context(), "fixture", &[], &options).unwrap();
    assert_eq!(payload["max_tokens"], 100);
    assert_eq!(payload["tool_choice"], "any");
    assert_eq!(payload["reasoning_effort"], "high");
    assert_eq!(payload["prompt_cache_key"], "session-fixture");
}
#[test]
fn mistral_truncation_malformed_arguments_and_conflicting_identity_fail() {
    for stream in [
        events(&[json!({"choices":[{"delta":{"content":"partial"}}]})]),
        events(&[
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"Abcd12345","function":{"name":"read","arguments":"[1]"}}]},"finish_reason":"tool_calls"}]}),
        ]),
        events(&[
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"Abcd12345","function":{"name":"read","arguments":"{"}}]}}]}),
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"Other1234","function":{"arguments":"}"}}]},"finish_reason":"tool_calls"}]}),
        ]),
    ] {
        assert!(
            parse_mistral_sse(Cursor::new(stream), &CancellationToken::new(), &mut |_| {}).is_err()
        );
    }
}
#[test]
fn pi_messages_native_thinking_signature_usage_and_rewrite_round_trip() {
    let result = parse_pi_messages_sse(
        Cursor::new(pi_stream()),
        &CancellationToken::new(),
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(result.usage.as_ref().unwrap().input, 16);
    assert_eq!(
        result.protocol_metadata.as_ref().unwrap()["rewrite"]["changed"],
        true
    );
    let mut context = context();
    context.messages.push(Message::Assistant(result));
    let payload=pi_messages_request_payload(&context,"fixture",&[json!({"type":"function","function":{"name":"read","description":"read","parameters":{"type":"object"}}})],&PiMessagesOptions::default()).unwrap();
    assert_eq!(
        payload["context"]["messages"][1]["content"][0]["thinkingSignature"],
        "opaque-fixture"
    );
    assert_eq!(payload["context"]["tools"][0]["name"], "read");
    assert_eq!(payload["context"]["messages"][1]["usage"]["cacheRead"], 8);
}
#[test]
fn pi_messages_tool_completion_validates_graph_arguments_and_metadata() {
    let stream = events(&[
        json!({"type":"start"}),
        json!({"type":"toolcall_start","contentIndex":0,"id":"tool-fixture","toolName":"read"}),
        json!({"type":"toolcall_delta","contentIndex":0,"delta":"{\"path\":\"fixture\"}"}),
        json!({"type":"toolcall_end","contentIndex":0,"toolCall":{"type":"toolCall","id":"tool-fixture","name":"read","arguments":{"path":"fixture"},"namespace":"fixture_ns","thoughtSignature":"tool-signature"}}),
        json!({"type":"done","reason":"toolUse","usage":usage()}),
    ]);
    let result = parse_pi_messages_sse(
        Cursor::new(stream.clone()),
        &CancellationToken::new(),
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(result.stop_reason, StopReason::ToolUse);
    let mut context = context();
    context.messages.push(Message::Assistant(result));
    context.messages.push(tool_result("tool-fixture"));
    let payload =
        pi_messages_request_payload(&context, "fixture", &[], &PiMessagesOptions::default())
            .unwrap();
    assert_eq!(
        payload["context"]["messages"][1]["content"][0]["namespace"],
        "fixture_ns"
    );
    assert_eq!(
        payload["context"]["messages"][2]["content"][1]["type"],
        "image"
    );
    let bad = stream.replace(
        "\"arguments\":{\"path\":\"fixture\"}",
        "\"arguments\":{\"path\":\"conflict\"}",
    );
    assert!(
        parse_pi_messages_sse(Cursor::new(bad), &CancellationToken::new(), &mut |_| {}).is_err()
    );
}
#[test]
fn pi_messages_terminal_error_retains_partial_and_usage_without_secret_echo() {
    let stream = events(&[
        json!({"type":"start"}),
        json!({"type":"text_start","contentIndex":0}),
        json!({"type":"text_delta","contentIndex":0,"delta":"partial"}),
        json!({"type":"error","reason":"error","usage":usage(),"errorMessage":"secret-fixture-token"}),
    ]);
    let result =
        parse_pi_messages_sse(Cursor::new(stream), &CancellationToken::new(), &mut |_| {}).unwrap();
    assert_eq!(result.stop_reason, StopReason::Error);
    assert!(matches!(&result.content[0],Content::Text{text}if text=="partial"));
    assert!(!result
        .error_message
        .unwrap()
        .contains("secret-fixture-token"));
}
#[test]
fn pi_messages_invalid_event_order_and_missing_terminal_are_errors() {
    for stream in [
        events(&[json!({"type":"text_delta","contentIndex":0,"delta":"bad"})]),
        events(&[
            json!({"type":"start"}),
            json!({"type":"text_start","contentIndex":0}),
        ]),
        events(&[
            json!({"type":"start"}),
            json!({"type":"done","reason":"stop","usage":{"input":0}}),
        ]),
    ] {
        assert!(
            parse_pi_messages_sse(Cursor::new(stream), &CancellationToken::new(), &mut |_| {})
                .is_err()
        );
    }
}

struct Request {
    method: String,
    path: String,
    headers: BTreeMap<String, String>,
    body: Value,
}
fn accept(listener: &TcpListener) -> TcpStream {
    listener.set_nonblocking(true).unwrap();
    let now = Instant::now();
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                return stream;
            }
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock
                    && now.elapsed() < Duration::from_secs(3) =>
            {
                thread::sleep(Duration::from_millis(5))
            }
            Err(error) => panic!("fixture accept: {error}"),
        }
    }
}
fn read_request(stream: &mut TcpStream) -> Request {
    let mut bytes = vec![];
    let mut chunk = [0; 4096];
    let (end, length) = loop {
        let count = stream.read(&mut chunk).unwrap();
        assert!(count > 0);
        bytes.extend_from_slice(&chunk[..count]);
        if let Some(index) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
            let length = String::from_utf8_lossy(&bytes[..index])
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
    while bytes.len() < end + length {
        let count = stream.read(&mut chunk).unwrap();
        assert!(count > 0);
        bytes.extend_from_slice(&chunk[..count]);
    }
    let head = String::from_utf8_lossy(&bytes[..end]);
    let mut first = head.lines().next().unwrap().split_whitespace();
    Request {
        method: first.next().unwrap().into(),
        path: first.next().unwrap().into(),
        headers: head
            .lines()
            .skip(1)
            .filter_map(|line| line.split_once(':'))
            .map(|(name, value)| (name.to_lowercase(), value.trim().into()))
            .collect(),
        body: serde_json::from_slice(&bytes[end..end + length]).unwrap(),
    }
}
fn serve(body: String, status: u16) -> (String, thread::JoinHandle<Request>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = thread::spawn(move || {
        let mut stream = accept(&listener);
        let request = read_request(&mut stream);
        write!(stream,"HTTP/1.1 {status} Fixture\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();
        request
    });
    (url, server)
}
fn provider(mode: ProviderMode, url: &str, identity: &str) -> ModelProvider {
    ModelProvider {
        settings: ProviderSettings {
            mode,
            base_url: url.into(),
            model: "fixture-model".into(),
            api_key: "secret-fixture-token".into(),
            options: ProviderOptions {
                provider_id: identity.into(),
                cloudflare: CloudflareOptions {
                    account_id: "account_fixture".into(),
                    gateway_id: "gateway_fixture".into(),
                },
                copilot: CopilotOptions::default(),
                ..ProviderOptions::default()
            },
        },
        tool_definitions: vec![],
    }
}
fn openai_stream() -> String {
    events(&[
        json!({"choices":[{"delta":{"content":"中文回答"},"finish_reason":"stop"}],"usage":{"prompt_tokens":16,"completion_tokens":3}}),
    ]) + "data: [DONE]\n\n"
}
fn anthropic_stream() -> String {
    events(&[
        json!({"type":"message_start","message":{"usage":{"input_tokens":16}}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"中文回答"}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":3}}),
        json!({"type":"message_stop"}),
    ])
}

#[test]
fn actual_mistral_and_pi_http_have_native_paths_auth_payload_and_identity() {
    for (mode, identity, stream, path) in [
        (
            ProviderMode::Mistral,
            "mistral",
            mistral_stream(),
            "/v1/chat/completions",
        ),
        (
            ProviderMode::PiMessages,
            "pi-fixture",
            pi_stream(),
            "/messages?debug=1",
        ),
    ] {
        let (url, server) = serve(stream, 200);
        let mut provider = provider(mode, &url, identity);
        provider.settings.options.pi_messages.debug = true;
        provider.settings.options.mistral.session_id = Some("affinity-fixture".into());
        let result = provider
            .complete(&context(), &CancellationToken::new(), &mut |_| {})
            .unwrap();
        let request = server.join().unwrap();
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, path);
        assert_eq!(
            request.headers["authorization"],
            "Bearer secret-fixture-token"
        );
        assert_eq!(
            result.protocol_metadata.as_ref().unwrap()["provider"],
            identity
        );
        if identity == "mistral" {
            assert_eq!(request.headers["x-affinity"], "affinity-fixture");
            assert!(request.body.get("messages").is_some());
        } else {
            assert!(request.body.get("context").is_some());
        }
    }
}
#[test]
fn actual_cloudflare_gateway_owns_auth_and_materializes_account_gateway_route() {
    let (url, server) = serve(openai_stream(), 200);
    let template = format!("{url}/v1/{{CLOUDFLARE_ACCOUNT_ID}}/{{CLOUDFLARE_GATEWAY_ID}}/compat");
    let mut provider = provider(
        ProviderMode::OpenAiCompatible,
        &template,
        "cloudflare-ai-gateway",
    );
    provider
        .settings
        .options
        .headers
        .insert("Authorization".into(), Some("stale-provider-secret".into()));
    let result = provider
        .complete(&context(), &CancellationToken::new(), &mut |_| {})
        .unwrap();
    let request = server.join().unwrap();
    assert_eq!(
        request.path,
        "/v1/account_fixture/gateway_fixture/compat/chat/completions"
    );
    assert_eq!(
        request.headers["cf-aig-authorization"],
        "Bearer secret-fixture-token"
    );
    assert!(!request.headers.contains_key("authorization"));
    assert!(!request.headers.contains_key("x-api-key"));
    assert!(result.protocol_metadata.unwrap()["route"]
        .as_str()
        .unwrap()
        .contains("gateway_fixture"));
}
#[test]
fn actual_cloudflare_workers_ai_and_copilot_anthropic_use_bearer_not_xapikey() {
    for (identity, mode, stream) in [
        (
            "cloudflare-workers-ai",
            ProviderMode::OpenAiCompatible,
            openai_stream(),
        ),
        (
            "github-copilot",
            ProviderMode::Anthropic,
            anthropic_stream(),
        ),
    ] {
        let (url, server) = serve(stream, 200);
        let mut provider = provider(mode, &url, identity);
        let mut context = context();
        context.messages.push(tool_result("prior-fixture"));
        provider
            .complete(&context, &CancellationToken::new(), &mut |_| {})
            .unwrap();
        let request = server.join().unwrap();
        assert_eq!(
            request.headers["authorization"],
            "Bearer secret-fixture-token"
        );
        assert!(!request.headers.contains_key("x-api-key"));
        if identity == "github-copilot" {
            assert_eq!(request.headers["x-initiator"], "agent");
            assert_eq!(request.headers["copilot-vision-request"], "true");
            assert_eq!(request.headers["editor-version"], "vscode/1.107.0");
        }
    }
}
#[test]
fn copilot_account_token_route_and_explicit_proxy_have_correct_precedence() {
    let options = CopilotOptions::default();
    assert_eq!(
        options
            .resolve_base(
                "https://api.individual.githubcopilot.com",
                "tid=fixture;proxy-ep=proxy.business.githubcopilot.com;"
            )
            .unwrap()
            .host_str(),
        Some("api.business.githubcopilot.com")
    );
    assert_eq!(
        options
            .resolve_base(
                "http://127.0.0.1:9",
                "proxy-ep=proxy.business.githubcopilot.com;"
            )
            .unwrap()
            .host_str(),
        Some("127.0.0.1")
    );
    assert!(options
        .resolve_base(
            "https://api.individual.githubcopilot.com",
            "proxy-ep=bad.invalid;"
        )
        .is_err());
}
#[test]
fn pi_requires_explicit_base_and_routes_reject_missing_credentials_config() {
    let mut settings = provider(ProviderMode::PiMessages, "", "pi-fixture").settings;
    assert!(settings.validate().is_err());
    settings.base_url = "http://127.0.0.1:9".into();
    settings.api_key.clear();
    assert!(settings.validate().is_err());
    let mut settings = provider(
        ProviderMode::OpenAiCompatible,
        "http://127.0.0.1:9",
        "cloudflare-ai-gateway",
    )
    .settings;
    settings.options.cloudflare.gateway_id.clear();
    assert!(settings.validate().is_err());
}
#[test]
fn native_http_errors_never_echo_remote_secret_body() {
    for mode in [ProviderMode::Mistral, ProviderMode::PiMessages] {
        let (url, server) = serve("remote secret-fixture-token".into(), 403);
        let mut provider = provider(mode, &url, "offline");
        let error = provider
            .complete(&context(), &CancellationToken::new(), &mut |_| {})
            .err()
            .unwrap();
        assert!(error.contains("403"));
        assert!(!error.contains("secret-fixture-token"));
        server.join().unwrap();
    }
}
#[test]
fn both_native_routes_cancel_stalled_headers_and_close_socket() {
    for mode in [ProviderMode::Mistral, ProviderMode::PiMessages] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (ready, receiver) = mpsc::channel();
        let server = thread::spawn(move || {
            let mut stream = accept(&listener);
            read_request(&mut stream);
            ready.send(()).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut byte = [0];
            assert!(matches!(stream.read(&mut byte), Ok(0) | Err(_)));
        });
        let signal = CancellationToken::new();
        let cancel = signal.clone();
        let worker = thread::spawn(move || {
            provider(mode, &url, "offline").complete(&context(), &cancel, &mut |_| {})
        });
        receiver.recv_timeout(Duration::from_secs(3)).unwrap();
        let started = Instant::now();
        signal.cancel();
        assert!(worker.join().unwrap().err().unwrap().contains("aborted"));
        assert!(started.elapsed() < Duration::from_secs(2));
        server.join().unwrap();
    }
}

struct FixtureTool;
impl AgentTool for FixtureTool {
    fn name(&self) -> &str {
        "read"
    }
    fn execute(
        &self,
        _id: &str,
        args: &Value,
        signal: &CancellationToken,
        _updates: &mut dyn FnMut(ToolResult),
    ) -> Result<ToolResult, String> {
        assert!(!signal.is_cancelled());
        assert_eq!(args["path"], "fixture");
        Ok(ToolResult {
            content: vec![
                Content::text("tool output"),
                Content::Image {
                    data: "aA==".into(),
                    mime_type: "image/png".into(),
                },
            ],
            ..ToolResult::default()
        })
    }
}
#[test]
fn both_native_http_protocols_complete_model_tool_image_final_multiround() {
    for mode in [ProviderMode::Mistral, ProviderMode::PiMessages] {
        let first = if mode == ProviderMode::Mistral {
            events(&[
                json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"Abcd12345","function":{"name":"read","arguments":"{\"path\":\"fixture\"}"}}]},"finish_reason":"tool_calls"}]}),
            ])
        } else {
            events(&[
                json!({"type":"start"}),
                json!({"type":"toolcall_start","contentIndex":0,"id":"Abcd12345","toolName":"read"}),
                json!({"type":"toolcall_end","contentIndex":0,"toolCall":{"type":"toolCall","id":"Abcd12345","name":"read","arguments":{"path":"fixture"}}}),
                json!({"type":"done","reason":"toolUse","usage":usage()}),
            ])
        };
        let second = if mode == ProviderMode::Mistral {
            mistral_stream()
        } else {
            pi_stream()
        };
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = thread::spawn(move || {
            [first,second].iter().map(|body|{let mut stream=accept(&listener);let request=read_request(&mut stream);write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();request}).collect::<Vec<_>>()
        });
        let mistral = mode == ProviderMode::Mistral;
        let mut provider = provider(mode, &url, if mistral { "mistral" } else { "pi-fixture" });
        provider.tool_definitions = vec![
            json!({"type":"function","function":{"name":"read","description":"fixture","parameters":{"type":"object"}}}),
        ];
        let tools: Vec<Box<dyn AgentTool>> = vec![Box::new(FixtureTool)];
        let messages = run_agent_loop(
            vec![Message::user("你好")],
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
        assert!(messages
            .iter()
            .any(|message| matches!(message, Message::ToolResult(_))));
        assert!(messages.last().unwrap().text().contains("中文回答"));
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 2);
        let second = if mistral {
            &requests[1].body["messages"]
        } else {
            &requests[1].body["context"]["messages"]
        };
        let tool = second
            .as_array()
            .unwrap()
            .iter()
            .find(|message| matches!(message["role"].as_str(), Some("tool" | "toolResult")))
            .unwrap();
        assert!(tool["content"]
            .as_array()
            .unwrap()
            .iter()
            .any(|part| matches!(part["type"].as_str(), Some("image_url" | "image"))));
    }
}
fn responses_stream() -> String {
    events(&[
        json!({"type":"response.created","response":{"id":"resp-fixture","status":"in_progress","output":[]}}),
        json!({"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"msg_fixture","role":"assistant","content":[]}}),
        json!({"type":"response.output_text.delta","item_id":"msg_fixture","output_index":0,"content_index":0,"delta":"中文回答"}),
        json!({"type":"response.output_item.done","output_index":0,"item":{"type":"message","id":"msg_fixture","role":"assistant","content":[{"type":"output_text","text":"中文回答"}]}}),
        json!({"type":"response.completed","response":{"id":"resp-fixture","status":"completed","output":[{"type":"message","id":"msg_fixture","role":"assistant","content":[{"type":"output_text","text":"中文回答"}]}],"usage":{"input_tokens":16,"output_tokens":3,"total_tokens":19}}}),
    ])
}
#[test]
fn actual_copilot_and_cloudflare_responses_have_native_auth_route_and_sse() {
    for identity in ["github-copilot", "cloudflare-ai-gateway"] {
        let (url, server) = serve(responses_stream(), 200);
        let mut provider = provider(
            ProviderMode::OpenAiResponses,
            &format!("{url}/native-proxy"),
            identity,
        );
        let result = provider
            .complete(&context(), &CancellationToken::new(), &mut |_| {})
            .unwrap();
        assert!(matches!(&result.content[0],Content::Text{text}if text=="中文回答"));
        let request = server.join().unwrap();
        assert_eq!(request.path, "/native-proxy/responses");
        if identity == "github-copilot" {
            assert_eq!(
                request.headers["authorization"],
                "Bearer secret-fixture-token"
            );
        } else {
            assert!(!request.headers.contains_key("authorization"));
            assert_eq!(
                request.headers["cf-aig-authorization"],
                "Bearer secret-fixture-token"
            );
        }
        assert_eq!(result.protocol_metadata.unwrap()["provider"], identity);
    }
}
#[test]
fn cloudflare_anthropic_gateway_route_is_not_replaced_by_anthropic_default_auth() {
    let (url, server) = serve(anthropic_stream(), 200);
    let mut provider = provider(
        ProviderMode::Anthropic,
        &format!("{url}/account/gateway/anthropic"),
        "cloudflare-ai-gateway",
    );
    provider
        .complete(&context(), &CancellationToken::new(), &mut |_| {})
        .unwrap();
    let request = server.join().unwrap();
    assert_eq!(request.path, "/account/gateway/anthropic/v1/messages");
    assert_eq!(
        request.headers["cf-aig-authorization"],
        "Bearer secret-fixture-token"
    );
    assert!(!request.headers.contains_key("authorization"));
    assert!(!request.headers.contains_key("x-api-key"));
}
