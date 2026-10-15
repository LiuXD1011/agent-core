//! Protocol fixtures use fixed SSE bytes and local-only HTTP, never live services.
use agent_core::agent_loop::{run_agent_loop, LoopConfig, NoopLoopHooks, Provider};
use agent_core::tools::{AgentTool, NoopToolHooks};
use agent_core::types::{
    AssistantMessage, CancellationToken, Content, Context, Event, Message, StopReason, ToolResult,
    ToolResultMessage, UserMessage,
};
use agent_runtime::provider::{
    anthropic_request_payload, google_request_payload, parse_anthropic_sse, parse_google_sse,
    parse_responses_sse, parse_sse, request_payload, responses_request_payload, ModelProvider,
    ProviderMode, ProviderSettings,
};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::{Cursor, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    mpsc, Arc,
};
use std::thread;
use std::time::{Duration, Instant};

const FIXTURE_KEY: &str = "offline-protocol-fixture-not-a-real-key";

fn stream(frames: Vec<Value>) -> String {
    frames
        .into_iter()
        .map(|frame| format!("data: {frame}\r\n\r\n"))
        .collect()
}

fn anthropic_frames() -> Vec<Value> {
    vec![
        json!({"type":"message_start","message":{"usage":{"input_tokens":5,"cache_read_input_tokens":2,"cache_creation_input_tokens":3}}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"分析"}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"signature-fixture"}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"content_block_start","index":1,"content_block":{"type":"text","text":"中文"}}),
        json!({"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"回答"}}),
        json!({"type":"content_block_stop","index":1}),
        json!({"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"call-fixture","name":"read","input":{}}}),
        json!({"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"pa"}}),
        json!({"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"th\":\"image.png\"}"}}),
        json!({"type":"content_block_stop","index":2}),
        json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":7}}),
        json!({"type":"message_stop"}),
    ]
}

fn google_frames() -> Vec<Value> {
    vec![
        json!({"candidates":[{"content":{"parts":[{"text":"分析","thought":true,"thoughtSignature":"c2ln"}]}}]}),
        json!({"candidates":[{"content":{"parts":[{"text":"中文"}]}}]}),
        json!({"candidates":[{"content":{"parts":[{"text":"回答","thoughtSignature":"dGV4dA=="}]}}]}),
        json!({"candidates":[{"content":{"parts":[{"functionCall":{"id":"call-fixture","name":"read","args":{"path":"image.png"}},"thoughtSignature":"dG9vbA=="}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":10,"cachedContentTokenCount":2,"candidatesTokenCount":3,"thoughtsTokenCount":4,"totalTokenCount":17}}),
    ]
}

fn image() -> Content {
    Content::Image {
        data: "aW1hZ2U=".to_owned(),
        mime_type: "image/png".to_owned(),
    }
}

fn context_with_result(mut assistant: AssistantMessage, protocol: &str) -> Context {
    assistant.protocol_metadata.as_mut().unwrap()["model"] = json!("fixture-model");
    assert_eq!(
        assistant.protocol_metadata.as_ref().unwrap()["protocol"],
        protocol
    );
    Context {
        system_prompt: "系统提示".to_owned(),
        messages: vec![
            Message::User(UserMessage {
                content: vec![Content::text("查看图片"), image()],
                timestamp: 0,
            }),
            Message::Assistant(assistant),
            Message::ToolResult(ToolResultMessage {
                tool_call_id: "call-fixture".to_owned(),
                tool_name: "read".to_owned(),
                content: vec![Content::text("图片结果"), image()],
                details: json!({}),
                usage: None,
                added_tool_names: Vec::new(),
                is_error: false,
                timestamp: 0,
            }),
        ],
    }
}

fn tools() -> Vec<Value> {
    vec![
        json!({"type":"function","function":{"name":"read","description":"Read a fixture","parameters":{"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}}}),
    ]
}

#[test]
fn anthropic_stream_preserves_thinking_signature_tool_fragments_and_usage() {
    let mut updates = Vec::new();
    let message = parse_anthropic_sse(
        Cursor::new(stream(anthropic_frames())),
        &CancellationToken::new(),
        &mut |message| updates.push(message),
    )
    .unwrap();
    assert_eq!(message.stop_reason, StopReason::ToolUse);
    assert_eq!(Message::Assistant(message.clone()).text(), "中文回答");
    assert!(matches!(&message.content[0], Content::Thinking { thinking } if thinking == "分析"));
    assert_eq!(
        message.tool_calls()[0].arguments,
        json!({"path":"image.png"})
    );
    assert_eq!(
        message.protocol_metadata.as_ref().unwrap()["blocks"]["0"]["signature"],
        "signature-fixture"
    );
    let usage = message.usage.unwrap();
    assert_eq!(
        (
            usage.input,
            usage.output,
            usage.cache_read,
            usage.cache_write,
            usage.total_tokens
        ),
        (5, 7, 2, 3, 17)
    );
    assert!(updates.len() >= 6);
}

#[test]
fn anthropic_payload_replays_signed_thinking_and_multimodal_tool_results() {
    let assistant = parse_anthropic_sse(
        Cursor::new(stream(anthropic_frames())),
        &CancellationToken::new(),
        &mut |_| {},
    )
    .unwrap();
    let context = context_with_result(assistant, "anthropic");
    let payload = anthropic_request_payload(&context, "fixture-model", &tools());
    assert_eq!(payload["system"], "系统提示");
    assert_eq!(payload["max_tokens"], 8192);
    assert_eq!(
        payload["tools"][0]["input_schema"],
        tools()[0]["function"]["parameters"]
    );
    assert_eq!(
        payload["messages"][0]["content"][1]["source"]["data"],
        "aW1hZ2U="
    );
    assert_eq!(payload["messages"][1]["content"][0]["type"], "thinking");
    assert_eq!(
        payload["messages"][1]["content"][0]["signature"],
        "signature-fixture"
    );
    assert_eq!(payload["messages"][1]["content"][2]["type"], "tool_use");
    assert_eq!(payload["messages"][2]["role"], "user");
    assert_eq!(
        payload["messages"][2]["content"][0]["tool_use_id"],
        "call-fixture"
    );
    assert_eq!(
        payload["messages"][2]["content"][0]["content"][1]["type"],
        "image"
    );
    let different_model = anthropic_request_payload(&context, "other-model", &[]);
    assert_eq!(different_model["messages"][1]["content"][0]["type"], "text");
}

#[test]
fn google_stream_preserves_thought_markers_signatures_tools_and_usage() {
    let message = parse_google_sse(
        Cursor::new(stream(google_frames())),
        &CancellationToken::new(),
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(message.stop_reason, StopReason::ToolUse);
    assert_eq!(Message::Assistant(message.clone()).text(), "中文回答");
    assert!(matches!(&message.content[0], Content::Thinking { thinking } if thinking == "分析"));
    assert_eq!(
        message.protocol_metadata.as_ref().unwrap()["signatures"]["0"],
        "c2ln"
    );
    assert_eq!(
        message.protocol_metadata.as_ref().unwrap()["signatures"]["1"],
        "dGV4dA=="
    );
    assert_eq!(
        message.protocol_metadata.as_ref().unwrap()["signatures"]["2"],
        "dG9vbA=="
    );
    assert_eq!(message.tool_calls()[0].id, "call-fixture");
    let usage = message.usage.unwrap();
    assert_eq!(
        (
            usage.input,
            usage.output,
            usage.cache_read,
            usage.total_tokens
        ),
        (8, 7, 2, 17)
    );
}

#[test]
fn google_payload_preserves_native_parts_and_function_response_pairing() {
    let assistant = parse_google_sse(
        Cursor::new(stream(google_frames())),
        &CancellationToken::new(),
        &mut |_| {},
    )
    .unwrap();
    let context = context_with_result(assistant, "google");
    let payload = google_request_payload(&context, "fixture-model", &tools());
    assert_eq!(payload["systemInstruction"]["parts"][0]["text"], "系统提示");
    assert_eq!(
        payload["contents"][0]["parts"][1]["inlineData"]["mimeType"],
        "image/png"
    );
    assert_eq!(payload["contents"][1]["role"], "model");
    assert_eq!(payload["contents"][1]["parts"][0]["thought"], true);
    assert_eq!(
        payload["contents"][1]["parts"][2]["thoughtSignature"],
        "dG9vbA=="
    );
    assert_eq!(
        payload["contents"][2]["parts"][0]["functionResponse"]["id"],
        "call-fixture"
    );
    assert_eq!(
        payload["contents"][2]["parts"][0]["functionResponse"]["response"]["output"],
        "图片结果"
    );
    assert_eq!(
        payload["contents"][2]["parts"][0]["functionResponse"]["parts"][0]["inlineData"]["data"],
        "aW1hZ2U="
    );
    assert_eq!(
        payload["tools"][0]["functionDeclarations"][0]["parametersJsonSchema"],
        tools()[0]["function"]["parameters"]
    );
    let switched = google_request_payload(&context, "other-model", &[]);
    assert!(switched["contents"][1]["parts"][0]
        .get("thoughtSignature")
        .is_none());
    assert!(switched["contents"][1]["parts"][0].get("thought").is_none());
}

#[test]
fn anthropic_rejects_missing_terminal_events_and_invalid_tool_arguments() {
    let original = anthropic_frames();
    for drop_index in [0, 4, 13] {
        let mut frames = original.clone();
        frames.remove(drop_index);
        assert!(parse_anthropic_sse(
            Cursor::new(stream(frames)),
            &CancellationToken::new(),
            &mut |_| {}
        )
        .is_err());
    }
    for args in ["{", "[]", "null", "\"text\""] {
        let mut frames = original.clone();
        frames[9]["delta"]["partial_json"] = json!(args);
        frames[10]["delta"]["partial_json"] = json!("");
        assert!(parse_anthropic_sse(
            Cursor::new(stream(frames)),
            &CancellationToken::new(),
            &mut |_| {}
        )
        .is_err());
    }
}

#[test]
fn google_rejects_incomplete_blocked_bad_arguments_and_duplicate_call_streams() {
    let invalid = [
        vec![json!({"candidates":[{"content":{"parts":[{"text":"partial"}]}}]})],
        vec![json!({"promptFeedback":{"blockReason":"SAFETY"}})],
        vec![json!({"error":{"message":FIXTURE_KEY}})],
        vec![json!({"candidates":[{"finishReason":"SAFETY"}]})],
        vec![
            json!({"candidates":[{"content":{"parts":[{"functionCall":{"name":"read","args":[]}}]},"finishReason":"STOP"}]}),
        ],
        vec![
            json!({"candidates":[{"content":{"parts":[{"functionCall":{"id":"duplicate","name":"read","args":{}}},{"functionCall":{"id":"duplicate","name":"read","args":{}}}]},"finishReason":"STOP"}]}),
        ],
    ];
    for frames in invalid {
        let error = parse_google_sse(
            Cursor::new(stream(frames)),
            &CancellationToken::new(),
            &mut |_| {},
        )
        .unwrap_err();
        assert!(!error.contains(FIXTURE_KEY));
    }
}

#[test]
fn native_length_stops_never_salvage_tool_arguments_for_execution() {
    let mut anthropic = anthropic_frames();
    anthropic[9]["delta"]["partial_json"] = json!("{");
    anthropic[10]["delta"]["partial_json"] = json!("");
    anthropic[12]["delta"]["stop_reason"] = json!("max_tokens");
    let message = parse_anthropic_sse(
        Cursor::new(stream(anthropic)),
        &CancellationToken::new(),
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(message.stop_reason, StopReason::Length);
    assert_eq!(message.tool_calls()[0].arguments, Value::Null);
    let mut google = google_frames();
    google[3]["candidates"][0]["finishReason"] = json!("MAX_TOKENS");
    let message = parse_google_sse(
        Cursor::new(stream(google)),
        &CancellationToken::new(),
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(message.stop_reason, StopReason::Length);
    assert_eq!(message.tool_calls()[0].arguments, Value::Null);
}

#[test]
fn native_protocols_honor_cancellation_during_fixed_stream_updates() {
    let signal = CancellationToken::new();
    assert!(parse_anthropic_sse(
        Cursor::new(stream(anthropic_frames())),
        &signal,
        &mut |_| signal.cancel()
    )
    .unwrap_err()
    .contains("aborted"));
    let signal = CancellationToken::new();
    assert!(
        parse_google_sse(Cursor::new(stream(google_frames())), &signal, &mut |_| {
            signal.cancel()
        })
        .unwrap_err()
        .contains("aborted")
    );
}

#[test]
fn google_without_call_ids_generates_unique_stable_result_pair_ids() {
    let frames = vec![
        json!({"candidates":[{"content":{"parts":[{"functionCall":{"name":"read","args":{}}},{"functionCall":{"name":"read","args":{}}}]},"finishReason":"STOP"}]}),
    ];
    let message = parse_google_sse(
        Cursor::new(stream(frames)),
        &CancellationToken::new(),
        &mut |_| {},
    )
    .unwrap();
    let calls = message.tool_calls();
    assert!(!calls[0].id.is_empty());
    assert_ne!(calls[0].id, calls[1].id);
}

#[test]
fn openai_payload_and_stream_now_preserve_images_and_reasoning() {
    let frames = vec![
        json!({"choices":[{"delta":{"reasoning_content":"分析","content":"回答"},"finish_reason":"stop"}]}),
    ];
    let message = parse_sse(
        Cursor::new(stream(frames) + "data: [DONE]\n\n"),
        &CancellationToken::new(),
        &mut |_| {},
    )
    .unwrap();
    assert!(matches!(&message.content[0], Content::Thinking { thinking } if thinking == "分析"));
    let context = Context {
        system_prompt: String::new(),
        messages: vec![
            Message::User(UserMessage {
                content: vec![image()],
                timestamp: 0,
            }),
            Message::Assistant(message),
        ],
    };
    let payload = request_payload(&context, "fixture", &[]);
    assert_eq!(
        payload["messages"][0]["content"][0]["image_url"]["url"],
        "data:image/png;base64,aW1hZ2U="
    );
    assert_eq!(payload["messages"][1]["reasoning_content"], "分析");
    assert!(parse_sse(
        Cursor::new("data: [DONE]\n\n"),
        &CancellationToken::new(),
        &mut |_| {}
    )
    .is_err());
}

struct Request {
    path: String,
    headers: BTreeMap<String, String>,
    body: Value,
}

fn serve_request(listener: TcpListener, response: &str) -> Result<Request, String> {
    listener
        .set_nonblocking(true)
        .map_err(|error| error.to_string())?;
    let deadline = Instant::now();
    let mut stream = loop {
        match listener.accept() {
            Ok((stream, _)) => break stream,
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock
                    && deadline.elapsed() < Duration::from_secs(5) =>
            {
                thread::sleep(Duration::from_millis(5))
            }
            Err(error) => return Err(error.to_string()),
        }
    };
    stream
        .set_nonblocking(false)
        .map_err(|error| error.to_string())?;
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .map_err(|error| error.to_string())?;
    stream
        .set_write_timeout(Some(Duration::from_secs(2)))
        .map_err(|error| error.to_string())?;
    let request = read_request(&mut stream)?;
    write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}", response.len()).map_err(|error| error.to_string())?;
    Ok(request)
}

fn read_request(stream: &mut TcpStream) -> Result<Request, String> {
    let mut headers = Vec::new();
    while !headers.ends_with(b"\r\n\r\n") {
        if headers.len() > 65_536 {
            return Err("Fixture headers too large".into());
        }
        let mut byte = [0_u8];
        stream
            .read_exact(&mut byte)
            .map_err(|error| error.to_string())?;
        headers.push(byte[0]);
    }
    let headers = String::from_utf8(headers).map_err(|error| error.to_string())?;
    let mut lines = headers.lines();
    let path = lines
        .next()
        .ok_or("Request line missing")?
        .split_whitespace()
        .nth(1)
        .ok_or("Path missing")?
        .to_owned();
    let headers: BTreeMap<_, _> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.to_ascii_lowercase(), value.trim().to_owned()))
        .collect();
    let length = headers
        .get("content-length")
        .ok_or("Content length missing")?
        .parse::<usize>()
        .map_err(|error| error.to_string())?;
    if length > 524_288 {
        return Err("Fixture request too large".into());
    }
    let mut body = vec![0; length];
    stream
        .read_exact(&mut body)
        .map_err(|error| error.to_string())?;
    Ok(Request {
        path,
        headers,
        body: serde_json::from_slice(&body).map_err(|error| error.to_string())?,
    })
}

fn model_provider(address: SocketAddr, mode: ProviderMode) -> ModelProvider {
    ModelProvider {
        settings: ProviderSettings {
            mode,
            base_url: format!("http://{address}/v1"),
            model: "fixture-model".to_owned(),
            api_key: FIXTURE_KEY.to_owned(),
            options: Default::default(),
        },
        tool_definitions: tools(),
    }
}

#[test]
fn loopback_native_http_routes_auth_and_stream_protocols_without_real_services() {
    for mode in [ProviderMode::Anthropic, ProviderMode::Google] {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let response = if mode == ProviderMode::Anthropic {
            stream(anthropic_frames())
        } else {
            stream(google_frames())
        };
        let server = thread::spawn(move || serve_request(listener, &response));
        let message = model_provider(address, mode.clone())
            .complete(
                &Context {
                    system_prompt: "system fixture".to_owned(),
                    messages: vec![Message::user("fixture prompt")],
                },
                &CancellationToken::new(),
                &mut |_| {},
            )
            .unwrap();
        let request = server.join().unwrap().unwrap();
        assert_eq!(message.stop_reason, StopReason::ToolUse);
        assert_eq!(
            message.protocol_metadata.as_ref().unwrap()["model"],
            "fixture-model"
        );
        assert!(!request.path.contains(FIXTURE_KEY));
        assert!(!request.body.to_string().contains(FIXTURE_KEY));
        if mode == ProviderMode::Anthropic {
            assert_eq!(request.path, "/v1/messages");
            assert_eq!(request.headers["x-api-key"], FIXTURE_KEY);
            assert_eq!(request.headers["anthropic-version"], "2023-06-01");
            assert_eq!(request.body["model"], "fixture-model");
            assert_eq!(request.body["tools"][0]["name"], "read");
        } else {
            assert_eq!(
                request.path,
                "/v1/models/fixture-model:streamGenerateContent?alt=sse"
            );
            assert_eq!(request.headers["x-goog-api-key"], FIXTURE_KEY);
            assert_eq!(
                request.body["tools"][0]["functionDeclarations"][0]["name"],
                "read"
            );
            assert_eq!(request.body["contents"][0]["role"], "user");
        }
        assert!(!request.headers.contains_key("authorization"));
    }
}

#[test]
fn native_http_modes_cancel_before_response_headers() {
    for mode in [
        ProviderMode::Anthropic,
        ProviderMode::Google,
        ProviderMode::OpenAiResponses,
    ] {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let (accepted_tx, accepted_rx) = mpsc::channel();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_millis(100)))
                .unwrap();
            read_request(&mut stream).unwrap();
            accepted_tx.send(()).unwrap();
            let start = Instant::now();
            let mut byte = [0];
            loop {
                assert!(
                    start.elapsed() < Duration::from_secs(5),
                    "Cancelled socket did not close"
                );
                match stream.read(&mut byte) {
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
                    Err(error) => panic!("Fixture read failed: {error}"),
                }
            }
        });
        let signal = CancellationToken::new();
        let worker_signal = signal.clone();
        let (done_tx, done_rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            done_tx
                .send(model_provider(address, mode).complete(
                    &Context::default(),
                    &worker_signal,
                    &mut |_| {},
                ))
                .unwrap()
        });
        accepted_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        thread::sleep(Duration::from_millis(75));
        let start = Instant::now();
        signal.cancel();
        let error = done_rx
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap_err();
        assert!(start.elapsed() < Duration::from_secs(2));
        assert!(error.contains("aborted"));
        assert!(!error.contains(FIXTURE_KEY));
        worker.join().unwrap();
        server.join().unwrap();
    }
}

#[test]
fn provider_mode_defaults_and_environment_keys_are_protocol_specific() {
    assert_eq!(
        ProviderMode::OpenAiResponses.api_key_env(),
        "OPENAI_API_KEY"
    );
    assert_eq!(
        ProviderMode::OpenAiResponses.default_base_url(),
        "https://api.openai.com/v1"
    );
    assert_eq!(
        ProviderMode::Anthropic.default_base_url(),
        "https://api.anthropic.com"
    );
    assert_eq!(ProviderMode::Anthropic.api_key_env(), "ANTHROPIC_API_KEY");
    assert_eq!(
        ProviderMode::Google.default_base_url(),
        "https://generativelanguage.googleapis.com/v1beta"
    );
    assert_eq!(ProviderMode::Google.api_key_env(), "GEMINI_API_KEY");
    assert_eq!(
        ProviderMode::OpenAiCompatible.api_key_env(),
        "OPENAI_API_KEY"
    );
}

#[test]
fn settings_preflight_and_complete_share_local_configuration_validation() {
    for mode in [
        ProviderMode::OpenAiCompatible,
        ProviderMode::OpenAiResponses,
        ProviderMode::Anthropic,
        ProviderMode::Google,
    ] {
        for base_url in [
            "ftp://fixture.invalid",
            "https://user:password@fixture.invalid",
            "https://fixture.invalid?key=fixture",
            "https://fixture.invalid#fragment",
            "not a URL",
        ] {
            let settings = ProviderSettings {
                mode: mode.clone(),
                base_url: base_url.to_owned(),
                model: "fixture-model".to_owned(),
                api_key: FIXTURE_KEY.to_owned(),
                options: Default::default(),
            };
            let validation = settings.validate().unwrap_err();
            let error = ModelProvider {
                settings,
                tool_definitions: Vec::new(),
            }
            .complete(&Context::default(), &CancellationToken::new(), &mut |_| {
                panic!("Validation must not stream or contact a provider")
            })
            .unwrap_err();
            assert_eq!(validation, error);
            assert!(!error.contains(FIXTURE_KEY));
        }
    }
    let invalid_model = ProviderSettings {
        mode: ProviderMode::Google,
        base_url: ProviderMode::Google.default_base_url().to_owned(),
        model: "../model?key=fixture".to_owned(),
        api_key: String::new(),
        options: Default::default(),
    };
    assert!(invalid_model.validate().is_err());
    let empty_model = ProviderSettings {
        mode: ProviderMode::Anthropic,
        model: "  ".to_owned(),
        ..ProviderSettings::default()
    };
    assert!(empty_model.validate().is_err());
    assert!(ProviderSettings::default().validate().is_ok());
}

#[test]
fn legacy_gemini_tool_images_use_separate_user_parts_without_call_ids() {
    let assistant = parse_google_sse(
        Cursor::new(stream(google_frames())),
        &CancellationToken::new(),
        &mut |_| {},
    )
    .unwrap();
    let context = context_with_result(assistant, "google");
    let payload = google_request_payload(&context, "gemini-2.5-flash", &[]);
    assert!(payload["contents"][1]["parts"][2]["functionCall"]
        .get("id")
        .is_none());
    assert!(payload["contents"][2]["parts"][0]["functionResponse"]
        .get("id")
        .is_none());
    assert!(payload["contents"][2]["parts"][0]["functionResponse"]
        .get("parts")
        .is_none());
    assert_eq!(
        payload["contents"][3]["parts"][0]["inlineData"]["mimeType"],
        "image/png"
    );
}

#[test]
fn anthropic_redacted_thinking_is_preserved_only_for_its_original_model() {
    let frames = vec![
        json!({"type":"message_start","message":{"usage":{}}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"redacted_thinking","data":"opaque-fixture"}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_delta","delta":{"stop_reason":"end_turn"}}),
        json!({"type":"message_stop"}),
    ];
    let mut assistant = parse_anthropic_sse(
        Cursor::new(stream(frames)),
        &CancellationToken::new(),
        &mut |_| {},
    )
    .unwrap();
    assistant.protocol_metadata.as_mut().unwrap()["model"] = json!("fixture-model");
    let context = Context {
        system_prompt: String::new(),
        messages: vec![Message::Assistant(assistant)],
    };
    let payload = anthropic_request_payload(&context, "fixture-model", &[]);
    assert_eq!(
        payload["messages"][0]["content"][0],
        json!({"type":"redacted_thinking","data":"opaque-fixture"})
    );
    let switched = anthropic_request_payload(&context, "other-model", &[]);
    assert_eq!(switched["messages"][0]["content"][0]["type"], "text");
    assert!(!switched.to_string().contains("opaque-fixture"));
}

#[test]
fn openai_rejects_duplicate_ids_and_non_object_tool_arguments() {
    for arguments in ["[]", "null", "\"text\""] {
        let body = stream(vec![
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call","function":{"name":"read","arguments":arguments}}]},"finish_reason":"tool_calls"}]}),
        ]) + "data: [DONE]\n\n";
        assert!(parse_sse(Cursor::new(body), &CancellationToken::new(), &mut |_| {}).is_err());
    }
    let body = stream(vec![
        json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"duplicate","function":{"name":"read","arguments":"{}"}},{"index":1,"id":"duplicate","function":{"name":"read","arguments":"{}"}}]},"finish_reason":"tool_calls"}]}),
    ]) + "data: [DONE]\n\n";
    assert!(
        parse_sse(Cursor::new(body), &CancellationToken::new(), &mut |_| {})
            .unwrap_err()
            .contains("Duplicate")
    );
}

#[test]
fn protocol_metadata_survives_serialization_without_changing_existing_messages() {
    let assistant = parse_google_sse(
        Cursor::new(stream(google_frames())),
        &CancellationToken::new(),
        &mut |_| {},
    )
    .unwrap();
    let context = context_with_result(assistant, "google");
    let stored = serde_json::to_string(&context).unwrap();
    let restored: Context = serde_json::from_str(&stored).unwrap();
    assert_eq!(
        google_request_payload(&context, "fixture-model", &tools()),
        google_request_payload(&restored, "fixture-model", &tools())
    );
    assert_eq!(serde_json::to_string(&context).unwrap(), stored);
    let legacy = json!({"role":"assistant","content":[{"type":"text","text":"legacy"}],"stopReason":"stop","errorMessage":null,"usage":null,"timestamp":0});
    let message: Message = serde_json::from_value(legacy).unwrap();
    assert!(
        matches!(message, Message::Assistant(assistant) if assistant.protocol_metadata.is_none())
    );
}

fn responses_items(arguments: &str) -> Vec<Value> {
    vec![
        json!({"type":"reasoning","id":"rs_fixture","summary":[{"type":"summary_text","text":"推理"}],"encrypted_content":"opaque-reasoning-fixture","status":"completed"}),
        json!({"type":"message","id":"msg_fixture","role":"assistant","status":"completed","phase":"commentary","content":[{"type":"output_text","text":"读取","annotations":[]}]}),
        json!({"type":"function_call","id":"fc_fixture","call_id":"call_fixture","name":"read","arguments":arguments,"status":"completed"}),
    ]
}

fn responses_frames() -> Vec<Value> {
    let items = responses_items("{\"path\":\"sample.txt\"}");
    let mut unsigned_reasoning = items[0].clone();
    unsigned_reasoning
        .as_object_mut()
        .unwrap()
        .remove("encrypted_content");
    vec![
        json!({"type":"response.created","sequence_number":0,"response":{"id":"resp_fixture","status":"in_progress"}}),
        json!({"type":"response.output_item.added","sequence_number":1,"output_index":0,"item":{"type":"reasoning","id":"rs_fixture","summary":[]}}),
        json!({"type":"response.reasoning_summary_text.delta","sequence_number":2,"output_index":0,"item_id":"rs_fixture","summary_index":0,"delta":"推理"}),
        json!({"type":"response.output_item.done","sequence_number":3,"output_index":0,"item":unsigned_reasoning}),
        json!({"type":"response.output_item.added","sequence_number":4,"output_index":1,"item":{"type":"message","id":"msg_fixture","role":"assistant","status":"in_progress","content":[]}}),
        json!({"type":"response.output_text.delta","sequence_number":5,"output_index":1,"item_id":"msg_fixture","content_index":0,"delta":"读取"}),
        json!({"type":"response.output_text.done","sequence_number":6,"output_index":1,"item_id":"msg_fixture","content_index":0,"text":"读取"}),
        json!({"type":"response.output_item.done","sequence_number":7,"output_index":1,"item":items[1]}),
        json!({"type":"response.output_item.added","sequence_number":8,"output_index":2,"item":{"type":"function_call","id":"fc_fixture","call_id":"call_fixture","name":"read","arguments":"","status":"in_progress"}}),
        json!({"type":"response.function_call_arguments.delta","sequence_number":9,"output_index":2,"item_id":"fc_fixture","delta":"{\"path\":"}),
        json!({"type":"response.function_call_arguments.done","sequence_number":10,"output_index":2,"item_id":"fc_fixture","name":"read","arguments":"{\"path\":\"sample.txt\"}"}),
        json!({"type":"response.output_item.done","sequence_number":11,"output_index":2,"item":items[2]}),
        json!({"type":"response.completed","sequence_number":12,"response":{"id":"resp_fixture","status":"completed","output":items,"usage":{"input_tokens":10,"input_tokens_details":{"cached_tokens":2,"cache_write_tokens":1},"output_tokens":7,"output_tokens_details":{"reasoning_tokens":4},"total_tokens":17}}}),
    ]
}

#[test]
fn responses_stream_parses_items_arguments_usage_phase_and_encrypted_backfill() {
    let mut updates = Vec::new();
    let message = parse_responses_sse(
        Cursor::new(stream(responses_frames())),
        &CancellationToken::new(),
        &mut |message| updates.push(message),
    )
    .unwrap();
    assert_eq!(message.stop_reason, StopReason::ToolUse);
    assert_eq!(Message::Assistant(message.clone()).text(), "读取");
    assert!(matches!(&message.content[0], Content::Thinking { thinking } if thinking == "推理"));
    assert_eq!(message.tool_calls()[0].id, "call_fixture|fc_fixture");
    assert_eq!(
        message.tool_calls()[0].arguments,
        json!({"path":"sample.txt"})
    );
    let metadata = message.protocol_metadata.as_ref().unwrap();
    assert_eq!(
        metadata["items"]["0"]["encrypted_content"],
        "opaque-reasoning-fixture"
    );
    assert_eq!(metadata["items"]["1"]["phase"], "commentary");
    assert_eq!(metadata["response_id"], "resp_fixture");
    assert_eq!(
        metadata["usage_details"]["output_tokens_details"]["reasoning_tokens"],
        4
    );
    let usage = message.usage.unwrap();
    assert_eq!(
        (
            usage.input,
            usage.output,
            usage.cache_read,
            usage.cache_write,
            usage.total_tokens
        ),
        (7, 7, 2, 1, 17)
    );
    assert!(updates.len() >= 7);
}

#[test]
fn responses_payload_replays_native_items_and_pairs_multimodal_function_outputs() {
    let assistant = parse_responses_sse(
        Cursor::new(stream(responses_frames())),
        &CancellationToken::new(),
        &mut |_| {},
    )
    .unwrap();
    let mut context = context_with_result(assistant, "openai-responses");
    if let Message::ToolResult(result) = &mut context.messages[2] {
        result.tool_call_id = "call_fixture|fc_fixture".to_owned();
    }
    let payload = responses_request_payload(&context, "fixture-model", &tools());
    assert_eq!(payload["instructions"], "系统提示");
    assert_eq!(payload["stream"], true);
    assert_eq!(payload["store"], false);
    assert_eq!(payload["include"], json!(["reasoning.encrypted_content"]));
    assert_eq!(payload["tools"][0]["type"], "function");
    assert_eq!(payload["tools"][0]["strict"], false);
    assert_eq!(
        payload["tools"][0]["parameters"],
        tools()[0]["function"]["parameters"]
    );
    assert_eq!(payload["input"][0]["content"][1]["type"], "input_image");
    assert_eq!(payload["input"][1]["type"], "reasoning");
    assert_eq!(
        payload["input"][1]["encrypted_content"],
        "opaque-reasoning-fixture"
    );
    assert_eq!(payload["input"][2]["phase"], "commentary");
    assert_eq!(payload["input"][3]["id"], "fc_fixture");
    assert_eq!(payload["input"][3]["call_id"], "call_fixture");
    assert_eq!(payload["input"][4]["type"], "function_call_output");
    assert_eq!(payload["input"][4]["call_id"], "call_fixture");
    assert_eq!(
        payload["input"][4]["output"][1]["image_url"],
        "data:image/png;base64,aW1hZ2U="
    );
    let switched = responses_request_payload(&context, "other-model", &[]);
    assert!(!switched.to_string().contains("opaque-reasoning-fixture"));
    assert!(switched["input"][3].get("id").is_none());
}

#[test]
fn responses_requires_terminal_event_and_rejects_failed_or_filtered_status() {
    let mut frames = responses_frames();
    frames.pop();
    assert!(parse_responses_sse(
        Cursor::new(stream(frames)),
        &CancellationToken::new(),
        &mut |_| {}
    )
    .unwrap_err()
    .contains("terminal"));
    for terminal in [
        json!({"type":"response.failed","response":{"status":"failed","error":{"message":FIXTURE_KEY}}}),
        json!({"type":"error","message":FIXTURE_KEY}),
        json!({"type":"response.incomplete","response":{"status":"incomplete","incomplete_details":{"reason":"content_filter"}}}),
        json!({"type":"response.completed","response":{"status":"in_progress"}}),
    ] {
        let error = parse_responses_sse(
            Cursor::new(stream(vec![terminal])),
            &CancellationToken::new(),
            &mut |_| {},
        )
        .unwrap_err();
        assert!(!error.contains(FIXTURE_KEY));
    }
}

#[test]
fn responses_incomplete_max_tokens_keeps_text_but_never_salvages_call_arguments() {
    let mut frames = responses_frames();
    frames[10]["arguments"] = json!("{");
    frames[11]["item"]["arguments"] = json!("{");
    frames[12] = json!({"type":"response.incomplete","response":{"id":"resp_fixture","status":"incomplete","incomplete_details":{"reason":"max_output_tokens"},"output":responses_items("{")}});
    let message = parse_responses_sse(
        Cursor::new(stream(frames)),
        &CancellationToken::new(),
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(message.stop_reason, StopReason::Length);
    assert_eq!(message.tool_calls()[0].arguments, Value::Null);
    assert_eq!(Message::Assistant(message).text(), "读取");
}

#[test]
fn responses_rejects_non_object_arguments_identity_conflicts_and_duplicate_call_ids() {
    for arguments in ["[]", "null", "{", "\"text\""] {
        let mut frames = responses_frames();
        frames[12]["response"]["output"][2]["arguments"] = json!(arguments);
        assert!(parse_responses_sse(
            Cursor::new(stream(frames)),
            &CancellationToken::new(),
            &mut |_| {}
        )
        .is_err());
    }
    let mut frames = responses_frames();
    frames[9]["item_id"] = json!("wrong-item");
    assert!(parse_responses_sse(
        Cursor::new(stream(frames)),
        &CancellationToken::new(),
        &mut |_| {}
    )
    .is_err());
    let mut duplicate = responses_items("{}");
    let mut second = duplicate[2].clone();
    second["id"] = json!("fc_second");
    duplicate.push(second);
    let frames = vec![
        json!({"type":"response.completed","response":{"status":"completed","output":duplicate}}),
    ];
    assert!(parse_responses_sse(
        Cursor::new(stream(frames)),
        &CancellationToken::new(),
        &mut |_| {}
    )
    .unwrap_err()
    .contains("Duplicate"));
}

#[test]
fn responses_terminal_output_can_reconstruct_missing_individual_item_events() {
    let frames = vec![
        json!({"type":"response.completed","response":{"id":"resp_fixture","status":"completed","output":responses_items("{}")}}),
    ];
    let message = parse_responses_sse(
        Cursor::new(stream(frames)),
        &CancellationToken::new(),
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(message.content.len(), 3);
    assert_eq!(message.tool_calls()[0].arguments, json!({}));
    let signal = CancellationToken::new();
    assert!(parse_responses_sse(
        Cursor::new(stream(responses_frames())),
        &signal,
        &mut |_| signal.cancel()
    )
    .unwrap_err()
    .contains("aborted"));
}

#[test]
fn responses_loopback_http_uses_native_endpoint_bearer_and_payload() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let response = stream(responses_frames());
    let server = thread::spawn(move || serve_request(listener, &response));
    let message = model_provider(address, ProviderMode::OpenAiResponses)
        .complete(
            &Context {
                system_prompt: "system fixture".to_owned(),
                messages: vec![Message::user("fixture prompt")],
            },
            &CancellationToken::new(),
            &mut |_| {},
        )
        .unwrap();
    let request = server.join().unwrap().unwrap();
    assert_eq!(request.path, "/v1/responses");
    assert_eq!(
        request.headers["authorization"],
        format!("Bearer {FIXTURE_KEY}")
    );
    assert_eq!(request.body["instructions"], "system fixture");
    assert_eq!(request.body["tools"][0]["name"], "read");
    assert_eq!(request.body["tools"][0]["type"], "function");
    assert_eq!(request.body["input"][0]["role"], "user");
    assert_eq!(
        message.protocol_metadata.as_ref().unwrap()["model"],
        "fixture-model"
    );
    assert_eq!(message.tool_calls()[0].id, "call_fixture|fc_fixture");
}

struct OfflineRead(Arc<AtomicUsize>);

impl AgentTool for OfflineRead {
    fn name(&self) -> &str {
        "read"
    }

    fn parameters(&self) -> Value {
        tools()[0]["function"]["parameters"].clone()
    }

    fn execute(
        &self,
        call_id: &str,
        arguments: &Value,
        signal: &CancellationToken,
        _on_update: &mut dyn FnMut(ToolResult),
    ) -> Result<ToolResult, String> {
        assert_eq!(call_id, "call_fixture|fc_fixture");
        assert_eq!(arguments, &json!({"path":"sample.txt"}));
        assert!(!signal.is_cancelled());
        self.0.fetch_add(1, Ordering::Relaxed);
        Ok(ToolResult {
            content: vec![Content::text("离线中文样本"), image()],
            ..ToolResult::default()
        })
    }
}

#[test]
fn responses_loopback_runs_tool_then_replays_native_items_and_pairs_result_for_final_answer() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let first = stream(responses_frames());
    let final_item = json!({"type":"message","id":"msg_final","role":"assistant","status":"completed","phase":"final_answer","content":[{"type":"output_text","text":"文件内容是：离线中文样本","annotations":[]}]});
    let second = stream(vec![
        json!({"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"msg_final","role":"assistant","content":[]}}),
        json!({"type":"response.output_text.delta","output_index":0,"item_id":"msg_final","content_index":0,"delta":"文件内容是：离线中文样本"}),
        json!({"type":"response.output_item.done","output_index":0,"item":final_item}),
        json!({"type":"response.completed","response":{"id":"resp_final","status":"completed","output":[final_item]}}),
    ]);
    let server = thread::spawn(move || -> Result<Vec<Request>, String> {
        Ok(vec![
            serve_request(
                listener.try_clone().map_err(|error| error.to_string())?,
                &first,
            )?,
            serve_request(listener, &second)?,
        ])
    });
    let count = Arc::new(AtomicUsize::new(0));
    let agent_tools: Vec<Box<dyn AgentTool>> = vec![Box::new(OfflineRead(count.clone()))];
    let mut events = Vec::new();
    let messages = run_agent_loop(
        vec![Message::user("读取 sample.txt 并用中文回答")],
        &Context {
            system_prompt: "离线系统提示".into(),
            messages: Vec::new(),
        },
        LoopConfig::default(),
        &mut model_provider(address, ProviderMode::OpenAiResponses),
        &agent_tools,
        &mut NoopLoopHooks,
        &NoopToolHooks,
        &CancellationToken::new(),
        &mut |event| events.push(event),
    )
    .unwrap();
    let requests = server.join().unwrap().unwrap();
    assert_eq!(count.load(Ordering::Relaxed), 1);
    assert_eq!(requests.len(), 2);
    for request in &requests {
        assert_eq!(request.path, "/v1/responses");
        assert_eq!(
            request.headers["authorization"],
            format!("Bearer {FIXTURE_KEY}")
        );
        assert_eq!(request.body["model"], "fixture-model");
        assert_eq!(request.body["instructions"], "离线系统提示");
        assert_eq!(request.body["tools"][0]["name"], "read");
        assert_eq!(request.body["tools"][0]["strict"], false);
        assert!(!request.body.to_string().contains(FIXTURE_KEY));
    }
    assert_eq!(requests[0].body["input"].as_array().unwrap().len(), 1);
    let replay = &requests[1].body["input"];
    assert_eq!(replay.as_array().unwrap().len(), 5);
    assert_eq!(replay[0]["role"], "user");
    assert_eq!(replay[1]["type"], "reasoning");
    assert_eq!(replay[1]["encrypted_content"], "opaque-reasoning-fixture");
    assert_eq!(replay[2]["phase"], "commentary");
    assert_eq!(replay[3]["type"], "function_call");
    assert_eq!(replay[3]["id"], "fc_fixture");
    assert_eq!(replay[3]["call_id"], "call_fixture");
    assert_eq!(replay[4]["type"], "function_call_output");
    assert_eq!(replay[4]["call_id"], "call_fixture");
    assert_eq!(replay[4]["output"][0]["text"], "离线中文样本");
    assert_eq!(replay[4]["output"][1]["type"], "input_image");
    assert_eq!(
        messages.iter().map(Message::role).collect::<Vec<_>>(),
        ["user", "assistant", "toolResult", "assistant"]
    );
    assert_eq!(messages.last().unwrap().text(), "文件内容是：离线中文样本");
    assert!(
        matches!(messages.last(), Some(Message::Assistant(message)) if message.stop_reason == StopReason::Stop && message.protocol_metadata.as_ref().unwrap()["items"]["0"]["phase"] == "final_answer")
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, Event::ToolExecutionStart { .. }))
            .count(),
        1
    );
    assert!(matches!(events.last(), Some(Event::AgentEnd { .. })));
}

#[test]
fn responses_empty_signed_text_and_refusal_parts_are_preserved() {
    let frames = vec![
        json!({"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"msg_refusal","role":"assistant","content":[]}}),
        json!({"type":"response.refusal.delta","output_index":0,"item_id":"msg_refusal","content_index":0,"delta":"fixture refusal"}),
        json!({"type":"response.refusal.done","output_index":0,"item_id":"msg_refusal","content_index":0,"refusal":"fixture refusal"}),
        json!({"type":"response.completed","response":{"status":"completed","output":[{"type":"message","id":"msg_refusal","role":"assistant","status":"completed","content":[{"type":"refusal","refusal":"fixture refusal"}]}]}}),
    ];
    let message = parse_responses_sse(
        Cursor::new(stream(frames)),
        &CancellationToken::new(),
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(Message::Assistant(message).text(), "fixture refusal");
    let empty_item = json!({"id":"msg_empty","type":"message","role":"assistant","status":"completed","phase":"commentary","content":[{"type":"output_text","text":"","annotations":[]}]});
    let mut message = AssistantMessage::text("");
    message.protocol_metadata = Some(
        json!({"protocol":"openai-responses","model":"fixture-model","items":{"0":empty_item}}),
    );
    let payload = responses_request_payload(
        &Context {
            system_prompt: String::new(),
            messages: vec![Message::Assistant(message)],
        },
        "fixture-model",
        &[],
    );
    assert_eq!(payload["input"][0], empty_item);
}
