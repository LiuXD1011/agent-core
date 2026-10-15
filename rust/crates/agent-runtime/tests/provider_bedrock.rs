//! AWS SDK-generated offline oracles plus real loopback binary HTTP; no cloud credentials.
use agent_core::agent_loop::{run_agent_loop, LoopConfig, NoopLoopHooks, Provider};
use agent_core::tools::{AgentTool, NoopToolHooks};
use agent_core::types::{
    AssistantMessage, CancellationToken, Content, Context, Event, Message, StopReason, ToolResult,
    ToolResultMessage, UserMessage,
};
use agent_runtime::provider::{
    aws_timestamp, bedrock_request_payload, parse_bedrock_stream, read_eventstream_frame,
    sign_aws_v4, AwsCredentials, BedrockCacheRetention, BedrockOptions, EventStreamHeader,
    ModelProvider, ProviderMode, ProviderOptions, ProviderSettings, SigV4Parameters,
};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::{Cursor, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    mpsc, Arc,
};
use std::thread;
use std::time::{Duration, Instant};

const SDK_STREAM: &[u8] = include_bytes!("fixtures/bedrock-converse.eventstream");
const SDK_HEADERS: &[u8] = include_bytes!("fixtures/aws-eventstream-headers.bin");
const ORACLE: &str = include_str!("fixtures/aws-sigv4.json");
const ACCESS: &str = "AKIDEXAMPLE";
const SECRET: &str = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY";
const SESSION: &str = "offline-session-fixture";
const BEARER: &str = "offline-bedrock-bearer-fixture";
const MODEL: &str = "us.anthropic.claude-sonnet-4-fixture-v1:0";
fn credentials() -> AwsCredentials {
    AwsCredentials {
        access_key_id: ACCESS.into(),
        secret_access_key: SECRET.into(),
        session_token: SESSION.into(),
    }
}
fn options() -> BedrockOptions {
    BedrockOptions {
        region: "us-west-2".into(),
        access_key_id: ACCESS.into(),
        secret_access_key: SECRET.into(),
        session_token: SESSION.into(),
        max_tokens: Some(512),
        cache_retention: BedrockCacheRetention::None,
        ..Default::default()
    }
}
fn tools() -> Vec<Value> {
    vec![
        json!({"type":"function","function":{"name":"read","description":"Offline fixture","parameters":{"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}}}),
    ]
}
fn image() -> Content {
    Content::Image {
        data: "aW1hZ2U=".into(),
        mime_type: "image/png".into(),
    }
}
fn context() -> Context {
    Context {
        system_prompt: "系统".into(),
        messages: vec![Message::User(UserMessage {
            content: vec![Content::text("读取图片"), image()],
            timestamp: 0,
        })],
    }
}
fn provider(url: String, bearer: bool) -> ModelProvider {
    ModelProvider {
        settings: ProviderSettings {
            mode: ProviderMode::AmazonBedrock,
            base_url: url,
            model: MODEL.into(),
            api_key: if bearer { BEARER.into() } else { String::new() },
            options: ProviderOptions {
                bedrock: options(),
                ..Default::default()
            },
        },
        tool_definitions: tools(),
    }
}
fn header_bytes(name: &str, value: &str) -> Vec<u8> {
    let mut bytes = vec![name.len() as u8];
    bytes.extend(name.as_bytes());
    bytes.push(7);
    bytes.extend((value.len() as u16).to_be_bytes());
    bytes.extend(value.as_bytes());
    bytes
}
fn raw_frame(headers: &[u8], payload: &[u8]) -> Vec<u8> {
    let mut frame = ((16 + headers.len() + payload.len()) as u32)
        .to_be_bytes()
        .to_vec();
    frame.extend((headers.len() as u32).to_be_bytes());
    frame.extend(crc32fast::hash(&frame).to_be_bytes());
    frame.extend(headers);
    frame.extend(payload);
    frame.extend(crc32fast::hash(&frame).to_be_bytes());
    frame
}
fn frame(kind: &str, payload: Value) -> Vec<u8> {
    raw_frame(
        &[
            header_bytes(":message-type", "event"),
            header_bytes(":event-type", kind),
            header_bytes(":content-type", "application/json"),
        ]
        .concat(),
        payload.to_string().as_bytes(),
    )
}
fn stream(events: Vec<(&str, Value)>) -> Vec<u8> {
    events
        .into_iter()
        .flat_map(|(kind, event)| frame(kind, event))
        .collect()
}
fn final_stream() -> Vec<u8> {
    stream(vec![
        ("messageStart", json!({"role":"assistant"})),
        (
            "contentBlockDelta",
            json!({"contentBlockIndex":0,"delta":{"text":"最终中文回答"}}),
        ),
        ("contentBlockStop", json!({"contentBlockIndex":0})),
        ("messageStop", json!({"stopReason":"end_turn"})),
        (
            "metadata",
            json!({"usage":{"inputTokens":10,"outputTokens":4,"totalTokens":14}}),
        ),
    ])
}
fn parse(bytes: &[u8]) -> Result<AssistantMessage, String> {
    parse_bedrock_stream(Cursor::new(bytes), &CancellationToken::new(), &mut |_| {})
}

#[test]
fn native_sigv4_exactly_matches_three_installed_sdk_oracles() {
    let oracle: Value = serde_json::from_str(ORACLE).unwrap();
    assert_eq!(oracle["cases"].as_array().unwrap().len(), 3);
    for case in oracle["cases"].as_array().unwrap() {
        let headers: BTreeMap<String, String> =
            serde_json::from_value(case["headers"].clone()).unwrap();
        let credentials = AwsCredentials {
            access_key_id: case["credentials"]["accessKeyId"].as_str().unwrap().into(),
            secret_access_key: case["credentials"]["secretAccessKey"]
                .as_str()
                .unwrap()
                .into(),
            session_token: case["credentials"]["sessionToken"]
                .as_str()
                .unwrap_or("")
                .into(),
        };
        let result = sign_aws_v4(SigV4Parameters {
            method: case["method"].as_str().unwrap(),
            url: &reqwest::Url::parse(case["url"].as_str().unwrap()).unwrap(),
            body: case["body"].as_str().unwrap().as_bytes(),
            headers: &headers,
            credentials: &credentials,
            region: case["region"].as_str().unwrap(),
            service: case["service"].as_str().unwrap(),
            timestamp: case["timestamp"].as_str().unwrap(),
            checksum_header: case["checksum_header"].as_bool().unwrap(),
        })
        .unwrap();
        assert_eq!(
            result.canonical_request,
            case["canonical_request"].as_str().unwrap(),
            "{}",
            case["name"]
        );
        assert_eq!(
            result.string_to_sign,
            case["string_to_sign"].as_str().unwrap()
        );
        assert_eq!(result.headers["authorization"], case["authorization"]);
    }
}
#[test]
fn sigv4_binds_actual_body_region_headers_and_rejects_credential_header_injection() {
    let url = reqwest::Url::parse("http://127.0.0.1:8080/model/fixture/converse-stream").unwrap();
    let headers = BTreeMap::from([("content-type".into(), "application/json".into())]);
    let creds = credentials();
    let sign = |body: &[u8], region: &str| {
        sign_aws_v4(SigV4Parameters {
            method: "POST",
            url: &url,
            body,
            headers: &headers,
            credentials: &creds,
            region,
            service: "bedrock",
            timestamp: "20240229T235959Z",
            checksum_header: true,
        })
        .unwrap()
    };
    let original = sign(b"{}", "us-east-1");
    assert_ne!(
        original.signature,
        sign(b"{\"x\":1}", "us-east-1").signature
    );
    assert_ne!(original.signature, sign(b"{}", "us-west-2").signature);
    assert_eq!(
        original.headers["x-amz-content-sha256"],
        format!("{:x}", Sha256::digest(b"{}"))
    );
    let bad = AwsCredentials {
        session_token: "secret\r\ninjected: true".into(),
        ..credentials()
    };
    let error = sign_aws_v4(SigV4Parameters {
        method: "POST",
        url: &url,
        body: b"{}",
        headers: &headers,
        credentials: &bad,
        region: "us-east-1",
        service: "bedrock",
        timestamp: "20240229T235959Z",
        checksum_header: true,
    })
    .err()
    .unwrap();
    assert!(!error.contains("secret"));
    assert_eq!(aws_timestamp(0).unwrap(), "19700101T000000Z");
    assert_eq!(aws_timestamp(1_709_251_199).unwrap(), "20240229T235959Z");
    assert!(aws_timestamp(u64::MAX).is_err());
}

#[test]
fn native_eventstream_decodes_all_ten_header_types_from_installed_codec() {
    let frame = read_eventstream_frame(&mut Cursor::new(SDK_HEADERS), &CancellationToken::new())
        .unwrap()
        .unwrap();
    assert_eq!(frame.payload, b"{}");
    assert_eq!(frame.headers.len(), 10);
    assert!(matches!(
        frame.headers["truth"],
        EventStreamHeader::Bool(true)
    ));
    assert!(matches!(
        frame.headers["falsehood"],
        EventStreamHeader::Bool(false)
    ));
    assert!(matches!(frame.headers["byte"], EventStreamHeader::Byte(-2)));
    assert!(matches!(
        frame.headers["short"],
        EventStreamHeader::Short(-300)
    ));
    assert!(matches!(
        frame.headers["integer"],
        EventStreamHeader::Integer(-100000)
    ));
    assert!(matches!(
        frame.headers["long"],
        EventStreamHeader::Long(-9000000000)
    ));
    assert!(
        matches!(&frame.headers["bytes"],EventStreamHeader::Bytes(bytes) if bytes==&[0,255,17])
    );
    assert!(matches!(&frame.headers["string"],EventStreamHeader::String(value) if value=="中文"));
    assert!(matches!(
        frame.headers["timestamp"],
        EventStreamHeader::Timestamp(1704164645000)
    ));
    assert!(
        matches!(frame.headers["uuid"],EventStreamHeader::Uuid(value) if value==[0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15])
    );
}
#[test]
fn eventstream_rejects_corruption_truncation_duplicate_headers_and_invalid_lengths() {
    for index in [0, 4, 8, 12, SDK_HEADERS.len() - 5, SDK_HEADERS.len() - 1] {
        let mut bytes = SDK_HEADERS.to_vec();
        bytes[index] ^= 1;
        assert!(
            read_eventstream_frame(&mut Cursor::new(bytes), &CancellationToken::new()).is_err()
        );
    }
    for length in 1..SDK_HEADERS.len() {
        assert!(read_eventstream_frame(
            &mut Cursor::new(&SDK_HEADERS[..length]),
            &CancellationToken::new()
        )
        .is_err());
    }
    let duplicate = raw_frame(
        &[
            header_bytes("duplicate", "one"),
            header_bytes("duplicate", "two"),
        ]
        .concat(),
        b"{}",
    );
    assert!(
        read_eventstream_frame(&mut Cursor::new(duplicate), &CancellationToken::new())
            .err()
            .unwrap()
            .contains("Duplicate")
    );
    for header in [
        vec![0, 7, 0, 0],
        vec![1, b'x', 10],
        vec![1, b'x', 7, 0, 5, b'a'],
    ] {
        assert!(read_eventstream_frame(
            &mut Cursor::new(raw_frame(&header, b"{}")),
            &CancellationToken::new()
        )
        .is_err());
    }
    let mut invalid = vec![];
    invalid.extend(15u32.to_be_bytes());
    invalid.extend(0u32.to_be_bytes());
    invalid.extend(crc32fast::hash(&invalid).to_be_bytes());
    assert!(read_eventstream_frame(&mut Cursor::new(invalid), &CancellationToken::new()).is_err());
}
#[test]
fn installed_sdk_binary_converse_stream_preserves_text_reasoning_tools_cache_usage() {
    let mut updates = vec![];
    let message = parse_bedrock_stream(
        Cursor::new(SDK_STREAM),
        &CancellationToken::new(),
        &mut |message| updates.push(message),
    )
    .unwrap();
    assert_eq!(message.stop_reason, StopReason::ToolUse);
    assert_eq!(Message::Assistant(message.clone()).text(), "中文回答");
    assert_eq!(
        message.tool_calls()[0].arguments,
        json!({"path":"image.png"})
    );
    assert_eq!(
        message.protocol_metadata.as_ref().unwrap()["blocks"]["0"]["signature"],
        "offline-reasoning-signature"
    );
    assert_eq!(
        message.protocol_metadata.as_ref().unwrap()["cache_details"][0]["ttl"],
        "1h"
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
        (5, 7, 2, 3, 12)
    );
    assert_eq!(updates.len(), 12);
}
#[test]
fn bedrock_redacted_and_image_deltas_join_raw_bytes_and_keep_opaque_replay() {
    let bytes = stream(vec![
        ("messageStart", json!({"role":"assistant"})),
        (
            "contentBlockDelta",
            json!({"contentBlockIndex":0,"delta":{"reasoningContent":{"redactedContent":"AAE="}}}),
        ),
        (
            "contentBlockDelta",
            json!({"contentBlockIndex":0,"delta":{"reasoningContent":{"redactedContent":"/w=="}}}),
        ),
        ("contentBlockStop", json!({"contentBlockIndex":0})),
        (
            "contentBlockStart",
            json!({"contentBlockIndex":1,"start":{"image":{"format":"png"}}}),
        ),
        (
            "contentBlockDelta",
            json!({"contentBlockIndex":1,"delta":{"image":{"source":{"bytes":"aW0="}}}}),
        ),
        (
            "contentBlockDelta",
            json!({"contentBlockIndex":1,"delta":{"image":{"source":{"bytes":"YWdl"}}}}),
        ),
        ("contentBlockStop", json!({"contentBlockIndex":1})),
        ("messageStop", json!({"stopReason":"end_turn"})),
    ]);
    let mut assistant = parse(&bytes).unwrap();
    assert!(
        matches!(&assistant.content[0],Content::Thinking{thinking} if thinking=="[Reasoning redacted]")
    );
    assert!(
        matches!(&assistant.content[1],Content::Image{data,..} if STANDARD.decode(data).unwrap()==b"image")
    );
    assert_eq!(
        assistant.protocol_metadata.as_ref().unwrap()["blocks"]["0"]["data"],
        "AAH/"
    );
    assistant.protocol_metadata.as_mut().unwrap()["model"] = json!("openai.fixture");
    let history = Context {
        messages: vec![Message::Assistant(assistant)],
        ..Default::default()
    };
    let payload = bedrock_request_payload(&history, "openai.fixture", &[], &options()).unwrap();
    assert_eq!(
        payload["messages"][0]["content"][0]["reasoningContent"]["redactedContent"],
        "AAH/"
    );
    assert_eq!(
        payload["messages"][0]["content"].as_array().unwrap().len(),
        1
    );
    assert!(
        !bedrock_request_payload(&history, "other-model", &[], &options())
            .unwrap()
            .to_string()
            .contains("AAH/")
    );
}
#[test]
fn bedrock_payload_pairs_consecutive_results_normalizes_ids_filters_empty_and_sanitizes_documents()
{
    let mut assistant = parse(SDK_STREAM).unwrap();
    assistant.protocol_metadata.as_mut().unwrap()["model"] = json!(MODEL);
    let mut second = assistant.tool_calls()[0].clone();
    second.id = "other|call/id".into();
    second.arguments = json!({"":"dropped","nested":{"":"dropped","ok":1}});
    assistant.content.push(Content::ToolCall(second));
    let result = |id: &str, is_error| {
        Message::ToolResult(ToolResultMessage {
            tool_call_id: id.into(),
            tool_name: "read".into(),
            content: vec![Content::text("  "), image()],
            details: json!({}),
            usage: None,
            added_tool_names: vec![],
            is_error,
            timestamp: 0,
        })
    };
    let history = Context {
        system_prompt: "系统".into(),
        messages: vec![
            Message::user(" "),
            Message::Assistant(AssistantMessage::error("offline", false)),
            Message::Assistant(assistant),
            result("call_fixture", false),
            result("other|call/id", true),
        ],
    };
    let payload = bedrock_request_payload(&history, MODEL, &tools(), &options()).unwrap();
    assert_eq!(payload["messages"].as_array().unwrap().len(), 3);
    assert_eq!(payload["messages"][0]["content"][0]["text"], "<empty>");
    assert_eq!(
        payload["messages"][1]["content"][0]["reasoningContent"]["reasoningText"]["signature"],
        "offline-reasoning-signature"
    );
    assert_eq!(
        payload["messages"][1]["content"][3]["toolUse"]["toolUseId"],
        "other_call_id"
    );
    assert_eq!(
        payload["messages"][1]["content"][3]["toolUse"]["input"],
        json!({"nested":{"ok":1}})
    );
    assert_eq!(
        payload["messages"][2]["content"].as_array().unwrap().len(),
        2
    );
    assert_eq!(
        payload["messages"][2]["content"][1]["toolResult"]["toolUseId"],
        "other_call_id"
    );
    assert_eq!(
        payload["messages"][2]["content"][1]["toolResult"]["status"],
        "error"
    );
    assert_eq!(
        payload["messages"][2]["content"][0]["toolResult"]["content"][0]["image"]["source"]
            ["bytes"],
        "aW1hZ2U="
    );
    assert_eq!(
        payload["toolConfig"]["tools"][0]["toolSpec"]["inputSchema"]["json"],
        tools()[0]["function"]["parameters"]
    );
}
#[test]
fn bedrock_cache_ttls_known_catalog_caps_and_explicit_sampling_match_request_budget() {
    let known = agent_runtime::catalog::models()
        .iter()
        .find(|model| {
            model.provider == "amazon-bedrock"
                && model.id.contains("claude-sonnet-4")
                && model.max_tokens < model.context_window - 4096
        })
        .unwrap();
    let known_other = agent_runtime::catalog::models()
        .iter()
        .find(|model| {
            model.provider == "amazon-bedrock"
                && !model.id.contains("claude")
                && model.max_tokens < model.context_window - 4096
        })
        .unwrap();
    for model in [known, known_other] {
        let payload =
            bedrock_request_payload(&context(), &model.id, &[], &BedrockOptions::default())
                .unwrap();
        assert_eq!(payload["inferenceConfig"]["maxTokens"], model.max_tokens);
    }
    let long = BedrockOptions {
        max_tokens: Some(1024),
        temperature: Some(0.25),
        cache_retention: BedrockCacheRetention::Long,
        ..Default::default()
    };
    let payload = bedrock_request_payload(&context(), &known.id, &[], &long).unwrap();
    assert_eq!(
        payload["inferenceConfig"],
        json!({"maxTokens":1024,"temperature":0.25})
    );
    assert_eq!(payload["system"][1]["cachePoint"]["ttl"], "1h");
    assert_eq!(
        payload["messages"][0]["content"][2]["cachePoint"]["ttl"],
        "1h"
    );
    assert!(
        bedrock_request_payload(&context(), "unknown-model", &[], &BedrockOptions::default())
            .unwrap()
            .get("inferenceConfig")
            .is_none()
    );
    let huge = Context {
        messages: vec![Message::user(
            "a".repeat((known.context_window as usize) * 4),
        )],
        ..Default::default()
    };
    assert_eq!(
        bedrock_request_payload(&huge, &known.id, &[], &BedrockOptions::default()).unwrap()
            ["inferenceConfig"]["maxTokens"],
        1
    );
}
#[test]
fn bedrock_stop_reasons_are_strict_and_length_never_salvages_tool_arguments() {
    for reason in ["max_tokens", "model_context_window_exceeded"] {
        let bytes = stream(vec![
            ("messageStart", json!({"role":"assistant"})),
            (
                "contentBlockStart",
                json!({"contentBlockIndex":0,"start":{"toolUse":{"toolUseId":"call","name":"read"}}}),
            ),
            (
                "contentBlockDelta",
                json!({"contentBlockIndex":0,"delta":{"toolUse":{"input":"{"}}}),
            ),
            ("contentBlockStop", json!({"contentBlockIndex":0})),
            ("messageStop", json!({"stopReason":reason})),
        ]);
        let message = parse(&bytes).unwrap();
        assert_eq!(message.stop_reason, StopReason::Length);
        assert_eq!(message.tool_calls()[0].arguments, Value::Null);
    }
    for reason in [
        "content_filtered",
        "guardrail_intervened",
        "unknown",
        "tool_use",
    ] {
        assert!(parse(&stream(vec![
            ("messageStart", json!({"role":"assistant"})),
            ("messageStop", json!({"stopReason":reason}))
        ]))
        .is_err());
    }
    for arguments in ["{", "[]", "null", "\"raw-secret-fixture\""] {
        let bytes = stream(vec![
            ("messageStart", json!({"role":"assistant"})),
            (
                "contentBlockStart",
                json!({"contentBlockIndex":0,"start":{"toolUse":{"toolUseId":"call","name":"read"}}}),
            ),
            (
                "contentBlockDelta",
                json!({"contentBlockIndex":0,"delta":{"toolUse":{"input":arguments}}}),
            ),
            ("messageStop", json!({"stopReason":"tool_use"})),
        ]);
        let error = parse(&bytes).unwrap_err();
        assert!(!error.contains("raw-secret-fixture"));
    }
}
#[test]
fn terminal_message_stop_finalizes_open_blocks_and_metadata_is_optional_like_original_ts() {
    // Original TS finalizesStreamingBlock again after settlement: blockStop is optional.
    let events = vec![
        ("messageStart", json!({"role":"assistant"})),
        (
            "contentBlockDelta",
            json!({"contentBlockIndex":0,"delta":{"text":"完整文字"}}),
        ),
        (
            "contentBlockDelta",
            json!({"contentBlockIndex":1,"delta":{"reasoningContent":{"redactedContent":"AAE="}}}),
        ),
        (
            "contentBlockStart",
            json!({"contentBlockIndex":2,"start":{"toolUse":{"toolUseId":"complete","name":"read"}}}),
        ),
        (
            "contentBlockDelta",
            json!({"contentBlockIndex":2,"delta":{"toolUse":{"input":"{\"path\":\"fixture\"}"}}}),
        ),
        ("messageStop", json!({"stopReason":"tool_use"})),
    ];
    let message = parse(&stream(events.clone())).unwrap();
    assert_eq!(Message::Assistant(message.clone()).text(), "完整文字");
    assert_eq!(message.tool_calls()[0].arguments, json!({"path":"fixture"}));
    assert_eq!(
        message.protocol_metadata.as_ref().unwrap()["blocks"]["1"]["data"],
        "AAE="
    );
    assert!(message.usage.is_none());
    for late in [
        ("contentBlockStop", json!({"contentBlockIndex":0})),
        (
            "contentBlockDelta",
            json!({"contentBlockIndex":0,"delta":{"text":"late"}}),
        ),
        ("messageStop", json!({"stopReason":"end_turn"})),
    ] {
        let mut events = events.clone();
        events.push(late);
        assert!(parse(&stream(events)).is_err());
    }
    assert!(parse(&stream(vec![
        ("messageStart", json!({"role":"assistant"})),
        ("metadata", json!({"usage":{}}))
    ]))
    .is_err());
}
#[test]
fn binary_response_limit_counts_header_bytes_and_content_start_unions_are_exclusive() {
    assert!(parse(&stream(vec![("messageStart",json!({"role":"assistant"})),("contentBlockStart",json!({"contentBlockIndex":0,"start":{"toolUse":{"toolUseId":"call","name":"read"},"image":{"format":"png"}}}))])).is_err());
    let mut bytes = frame("messageStart", json!({"role":"assistant"}));
    let mut headers = [
        header_bytes(":message-type", "event"),
        header_bytes(":event-type", "contentBlockDelta"),
    ]
    .concat();
    for index in 0..16 {
        headers.extend(header_bytes(
            &format!("padding-{index}"),
            &"x".repeat(60_000),
        ));
    }
    let delta = raw_frame(&headers, br#"{"contentBlockIndex":0,"delta":{"text":"x"}}"#);
    for _ in 0..9 {
        bytes.extend_from_slice(&delta);
    }
    assert!(parse(&bytes)
        .unwrap_err()
        .contains("response exceeds limit"));
}
#[test]
fn malformed_binary_event_sequences_are_rejected_before_execution() {
    let start = ("messageStart", json!({"role":"assistant"}));
    let text = (
        "contentBlockDelta",
        json!({"contentBlockIndex":0,"delta":{"text":"partial"}}),
    );
    for events in [
        vec![text.clone()],
        vec![("messageStart", json!({"role":"user"}))],
        vec![start.clone(), start.clone()],
        vec![start.clone(), text.clone()],
        vec![
            start.clone(),
            (
                "contentBlockDelta",
                json!({"contentBlockIndex":0,"delta":{"text":"x","reasoningContent":{"text":"y"}}}),
            ),
        ],
        vec![
            start.clone(),
            text.clone(),
            ("contentBlockStop", json!({"contentBlockIndex":0})),
            text.clone(),
        ],
        vec![
            start.clone(),
            (
                "contentBlockDelta",
                json!({"contentBlockIndex":0,"delta":{"toolUse":{"input":"{}"}}}),
            ),
        ],
    ] {
        assert!(parse(&stream(events)).is_err());
    }
    let duplicate = stream(vec![
        start,
        (
            "contentBlockStart",
            json!({"contentBlockIndex":0,"start":{"toolUse":{"toolUseId":"same","name":"read"}}}),
        ),
        (
            "contentBlockStart",
            json!({"contentBlockIndex":1,"start":{"toolUse":{"toolUseId":"same","name":"read"}}}),
        ),
    ]);
    assert!(parse(&duplicate).unwrap_err().contains("Duplicate"));
}
#[test]
fn bedrock_exception_frames_and_error_headers_never_echo_server_secrets() {
    for (kind, status) in [
        ("ThrottlingException", "429"),
        ("InternalServerException", "500"),
        ("ServiceUnavailableException", "503"),
        ("ValidationException", "400"),
        ("unknown-secret-name", "error"),
    ] {
        let bytes = raw_frame(
            &[
                header_bytes(":message-type", "exception"),
                header_bytes(":exception-type", kind),
            ]
            .concat(),
            json!({"message":format!("{SECRET} {BEARER} raw-secret-body")})
                .to_string()
                .as_bytes(),
        );
        let error = parse(&bytes).unwrap_err();
        assert!(error.contains(status));
        for secret in [SECRET, BEARER, "raw-secret-body", "unknown-secret-name"] {
            assert!(!error.contains(secret));
        }
    }
}
#[test]
fn bedrock_options_validation_is_local_and_serialization_excludes_signing_material() {
    let mut settings = provider("http://127.0.0.1:1".into(), false).settings;
    settings.options.bedrock.profile = "unread-fixture-profile".into();
    settings.options.bedrock.credentials_file = Some(PathBuf::from("unread-credentials"));
    settings.validate().unwrap();
    let serialized = serde_json::to_string(&settings.options.bedrock).unwrap();
    for secret in [ACCESS, SECRET, SESSION] {
        assert!(!serialized.contains(secret));
    }
    assert!(!serialized.contains("accessKeyId"));
    for model in [
        "../model",
        "model?key=secret",
        "arn:aws:bedrock:us-east-1:123456789012:prompt/fixture",
    ] {
        settings.model = model.into();
        assert!(settings.validate().is_err());
    }
}
#[test]
fn arn_and_explicit_endpoint_routes_preserve_the_effective_region_and_encoded_model_identity() {
    let arn = "arn:aws:bedrock:eu-west-1:123456789012:inference-profile/fixture";
    let options = options();
    assert_eq!(
        options
            .preview_base_url(ProviderMode::AmazonBedrock.default_base_url(), arn)
            .unwrap()
            .host_str()
            .unwrap(),
        "bedrock-runtime.eu-west-1.amazonaws.com"
    );
    let url = options
        .request_url("http://127.0.0.1:1/proxy", arn, "eu-west-1")
        .unwrap();
    assert!(url
        .path()
        .contains("arn%3Aaws%3Abedrock%3Aeu-west-1%3A123456789012%3Ainference-profile%2Ffixture"));
    assert!(options
        .route_identity(&url, "eu-west-1")
        .ends_with("#region=eu-west-1"));
    let explicit = BedrockOptions {
        endpoint_explicit: true,
        ..options
    };
    assert_eq!(
        explicit
            .resolved_base_url(ProviderMode::AmazonBedrock.default_base_url(), "eu-west-1")
            .unwrap()
            .host_str()
            .unwrap(),
        "bedrock-runtime.us-east-1.amazonaws.com"
    );
    assert_eq!(
        BedrockOptions::default()
            .resolved_base_url("", "cn-north-1")
            .unwrap()
            .host_str()
            .unwrap(),
        "bedrock-runtime.cn-north-1.amazonaws.com.cn"
    );
}

struct Request {
    path: String,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
}
fn accept(listener: &TcpListener) -> TcpStream {
    listener.set_nonblocking(true).unwrap();
    let start = Instant::now();
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
                    && start.elapsed() < Duration::from_secs(3) =>
            {
                thread::sleep(Duration::from_millis(5))
            }
            Err(error) => panic!("Loopback accept: {error}"),
        }
    }
}
fn request(socket: &mut TcpStream) -> Request {
    let mut bytes = vec![];
    let mut buffer = [0u8; 4096];
    let (end, length) = loop {
        let count = socket.read(&mut buffer).unwrap();
        assert!(count > 0);
        bytes.extend_from_slice(&buffer[..count]);
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
                .unwrap();
            break (index + 4, length);
        }
    };
    while bytes.len() < end + length {
        let count = socket.read(&mut buffer).unwrap();
        assert!(count > 0);
        bytes.extend_from_slice(&buffer[..count]);
    }
    let head = String::from_utf8_lossy(&bytes[..end]);
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
                    .map(|(name, value)| (name.to_ascii_lowercase(), value.trim().to_owned()))
            })
            .collect(),
        body: bytes[end..end + length].to_vec(),
    }
}
struct Server {
    address: SocketAddr,
    records: mpsc::Receiver<Request>,
    worker: thread::JoinHandle<()>,
}
impl Server {
    fn new(responses: Vec<(u16, Vec<u8>)>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (sender, records) = mpsc::channel();
        let worker = thread::spawn(move || {
            for (status, body) in responses {
                let mut socket = accept(&listener);
                let record = request(&mut socket);
                sender.send(record).unwrap();
                write!(socket,"HTTP/1.1 {status} Fixture\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",if status==200{"application/vnd.amazon.eventstream"}else{"application/json"},body.len()).unwrap();
                for chunk in body.chunks(7) {
                    if socket.write_all(chunk).is_err() {
                        break;
                    }
                }
                let _ = socket.flush();
            }
        });
        Self {
            address,
            records,
            worker,
        }
    }
    fn url(&self) -> String {
        format!("http://{}", self.address)
    }
    fn finish(self) -> Vec<Request> {
        self.worker.join().unwrap();
        self.records.into_iter().collect()
    }
}
struct ReadFixture(Arc<AtomicUsize>);
impl AgentTool for ReadFixture {
    fn name(&self) -> &str {
        "read"
    }
    fn execute(
        &self,
        _: &str,
        args: &Value,
        _: &CancellationToken,
        _: &mut dyn FnMut(ToolResult),
    ) -> Result<ToolResult, String> {
        self.0.fetch_add(1, Ordering::Relaxed);
        assert_eq!(args["path"], "image.png");
        Ok(ToolResult {
            content: vec![Content::text("image fixture"), image()],
            ..Default::default()
        })
    }
}
fn verify_http_signature(request: &Request, base: &str) {
    let signed = request.headers["authorization"]
        .split("SignedHeaders=")
        .nth(1)
        .unwrap()
        .split(',')
        .next()
        .unwrap();
    let headers = request
        .headers
        .iter()
        .filter(|(name, _)| {
            signed.split(';').any(|part| part == *name)
                && !matches!(
                    name.as_str(),
                    "x-amz-date" | "x-amz-content-sha256" | "x-amz-security-token" | "host"
                )
        })
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect();
    let url = reqwest::Url::parse(&(base.to_owned() + &request.path)).unwrap();
    let signature = sign_aws_v4(SigV4Parameters {
        method: "POST",
        url: &url,
        body: &request.body,
        headers: &headers,
        credentials: &credentials(),
        region: "us-west-2",
        service: "bedrock",
        timestamp: &request.headers["x-amz-date"],
        checksum_header: true,
    })
    .unwrap();
    assert_eq!(
        request.headers["authorization"],
        signature.headers["authorization"]
    );
    assert_eq!(
        request.headers["x-amz-content-sha256"],
        signature.headers["x-amz-content-sha256"]
    );
    assert_eq!(request.headers["x-amz-security-token"], SESSION);
}

#[test]
fn real_binary_http_sigv4_runs_tool_then_replays_reasoning_and_image_result() {
    let server = Server::new(vec![(200, SDK_STREAM.to_vec()), (200, final_stream())]);
    let base = server.url();
    let mut model = provider(base.clone(), false);
    let calls = Arc::new(AtomicUsize::new(0));
    let ctx = context();
    let messages = run_agent_loop(
        ctx.messages.clone(),
        &Context {
            system_prompt: ctx.system_prompt,
            messages: vec![],
        },
        LoopConfig::default(),
        &mut model,
        &[Box::new(ReadFixture(calls.clone()))],
        &mut NoopLoopHooks,
        &NoopToolHooks,
        &CancellationToken::new(),
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(messages.last().unwrap().text(), "最终中文回答");
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    let requests = server.finish();
    assert_eq!(requests.len(), 2);
    for request in &requests {
        assert_eq!(
            request.path,
            "/model/us.anthropic.claude-sonnet-4-fixture-v1%3A0/converse-stream"
        );
        assert_eq!(
            request.headers["accept"],
            "application/vnd.amazon.eventstream"
        );
        verify_http_signature(request, &base);
        for secret in [ACCESS, SECRET, SESSION] {
            assert!(!String::from_utf8_lossy(&request.body).contains(secret));
        }
    }
    let first: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(
        first["messages"][0]["content"][1]["image"]["source"]["bytes"],
        "aW1hZ2U="
    );
    assert_eq!(first["inferenceConfig"]["maxTokens"], 512);
    let second: Value = serde_json::from_slice(&requests[1].body).unwrap();
    assert_eq!(
        second["messages"][1]["content"][0]["reasoningContent"]["reasoningText"]["signature"],
        "offline-reasoning-signature"
    );
    assert_eq!(
        second["messages"][2]["content"][0]["toolResult"]["toolUseId"],
        "call_fixture"
    );
    assert_eq!(
        second["messages"][2]["content"][0]["toolResult"]["content"][1]["image"]["format"],
        "png"
    );
    let saved = serde_json::to_string(&messages).unwrap();
    for secret in [ACCESS, SECRET, SESSION] {
        assert!(!saved.contains(secret));
    }
}
#[test]
fn bedrock_bearer_transport_does_not_emit_sigv4_or_session_token_headers() {
    let server = Server::new(vec![(200, final_stream())]);
    let mut model = provider(server.url(), true);
    model
        .complete(&context(), &CancellationToken::new(), &mut |_| {})
        .unwrap();
    let request = server.finish().remove(0);
    assert_eq!(request.headers["authorization"], format!("Bearer {BEARER}"));
    for name in ["x-amz-date", "x-amz-content-sha256", "x-amz-security-token"] {
        assert!(!request.headers.contains_key(name));
    }
}
#[test]
fn bedrock_signature_replay_gate_includes_provider_model_region_and_endpoint() {
    let server = Server::new(vec![
        (200, SDK_STREAM.to_vec()),
        (200, final_stream()),
        (200, final_stream()),
        (200, final_stream()),
        (200, final_stream()),
    ]);
    let mut model = provider(server.url(), false);
    let assistant = model
        .complete(&context(), &CancellationToken::new(), &mut |_| {})
        .unwrap();
    let ctx = Context {
        messages: vec![Message::Assistant(assistant)],
        ..Default::default()
    };
    model.settings.options.bedrock.region = "eu-west-1".into();
    model
        .complete(&ctx, &CancellationToken::new(), &mut |_| {})
        .unwrap();
    model.settings.options.bedrock.region = "us-west-2".into();
    model.settings.base_url.push_str("/other");
    model
        .complete(&ctx, &CancellationToken::new(), &mut |_| {})
        .unwrap();
    model.settings.base_url = server.url();
    model.settings.options.provider_id = "offline-alternate-provider".into();
    model
        .complete(&ctx, &CancellationToken::new(), &mut |_| {})
        .unwrap();
    model.settings.options.provider_id.clear();
    model.settings.model = "other-model".into();
    model
        .complete(&ctx, &CancellationToken::new(), &mut |_| {})
        .unwrap();
    let requests = server.finish();
    assert_eq!(requests.len(), 5);
    for request in requests.iter().skip(1) {
        assert!(!String::from_utf8_lossy(&request.body).contains("offline-reasoning-signature"));
    }
}
#[test]
fn bedrock_http_and_binary_exception_errors_do_not_reveal_credentials_or_response_bodies() {
    for binary in [false, true] {
        let body = if binary {
            raw_frame(
                &[
                    header_bytes(":message-type", "exception"),
                    header_bytes(":exception-type", "ThrottlingException"),
                ]
                .concat(),
                json!({"message":format!("secret-body {SECRET} {BEARER}")})
                    .to_string()
                    .as_bytes(),
            )
        } else {
            json!({"message":format!("secret-body {SECRET} {BEARER}")})
                .to_string()
                .into_bytes()
        };
        let server = Server::new(vec![(if binary { 200 } else { 403 }, body)]);
        let mut model = provider(server.url(), false);
        let error = model
            .complete(&context(), &CancellationToken::new(), &mut |_| {})
            .unwrap_err();
        assert!(error.contains(if binary { "429" } else { "403" }));
        for secret in [SECRET, BEARER, "secret-body"] {
            assert!(!error.contains(secret));
        }
        server.finish();
    }
}

fn stalled(binary_partial: bool) -> (String, mpsc::Receiver<()>, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let (send, accepted) = mpsc::channel();
    let worker = thread::spawn(move || {
        let mut socket = accept(&listener);
        request(&mut socket);
        if binary_partial {
            write!(socket,"HTTP/1.1 200 Fixture\r\nContent-Type: application/vnd.amazon.eventstream\r\nContent-Length: 9999\r\n\r\n").unwrap();
            socket.write_all(&SDK_STREAM[..20]).unwrap();
            socket.flush().unwrap();
        }
        send.send(()).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_millis(25)))
            .unwrap();
        let start = Instant::now();
        let mut byte = [0];
        loop {
            match socket.read(&mut byte) {
                Ok(0) => return,
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => return,
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                    ) && start.elapsed() < Duration::from_secs(2) => {}
                other => panic!("Cancelled Bedrock connection did not close: {other:?}"),
            }
        }
    });
    (format!("http://{address}"), accepted, worker)
}
#[test]
fn native_bedrock_cancellation_closes_waiting_headers_and_partial_binary_frame_connections() {
    for partial in [false, true] {
        let (url, accepted, server) = stalled(partial);
        let mut model = provider(url, false);
        let signal = CancellationToken::new();
        let worker_signal = signal.clone();
        let worker = thread::spawn(move || {
            model
                .complete(&context(), &worker_signal, &mut |_| {})
                .unwrap_err()
        });
        accepted.recv_timeout(Duration::from_secs(3)).unwrap();
        thread::sleep(Duration::from_millis(50));
        let start = Instant::now();
        signal.cancel();
        assert!(worker.join().unwrap().contains("aborted"));
        assert!(start.elapsed() < Duration::from_secs(2));
        server.join().unwrap();
    }
}
#[test]
fn bedrock_partial_cancellation_preserves_text_and_pairs_unexecuted_call_for_resume() {
    let server = Server::new(vec![(200, SDK_STREAM.to_vec())]);
    let mut model = provider(server.url(), false);
    let calls = Arc::new(AtomicUsize::new(0));
    let signal = CancellationToken::new();
    let messages = run_agent_loop(
        vec![Message::user("fixture")],
        &Context::default(),
        LoopConfig::default(),
        &mut model,
        &[Box::new(ReadFixture(calls.clone()))],
        &mut NoopLoopHooks,
        &NoopToolHooks,
        &signal,
        &mut |event| {
            if let Event::MessageUpdate {
                message: Message::Assistant(assistant),
            } = event
            {
                if !assistant.tool_calls().is_empty() {
                    signal.cancel();
                }
            }
        },
    )
    .unwrap();
    assert_eq!(calls.load(Ordering::Relaxed), 0);
    let assistant = messages
        .iter()
        .find_map(|message| {
            if let Message::Assistant(assistant) = message {
                Some(assistant)
            } else {
                None
            }
        })
        .unwrap();
    assert_eq!(assistant.stop_reason, StopReason::Aborted);
    assert_eq!(Message::Assistant(assistant.clone()).text(), "中文回答");
    assert!(messages.iter().any(|message|matches!(message,Message::ToolResult(result) if result.tool_call_id=="call_fixture"&&result.is_error)));
    let history = Context {
        messages,
        ..Default::default()
    };
    let payload = bedrock_request_payload(&history, MODEL, &[], &options()).unwrap();
    assert!(payload.to_string().contains("Operation aborted"));
    assert_eq!(server.finish().len(), 1);
}
