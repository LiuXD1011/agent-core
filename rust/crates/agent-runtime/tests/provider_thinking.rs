use agent_core::agent_loop::Provider;
use agent_core::types::{CancellationToken, Context, Message, ThinkingLevel};
use agent_runtime::auth::OAuthCredential;
use agent_runtime::provider::{
    apply_thinking, ModelProvider, ProviderMode, ProviderOptions, ProviderSettings,
    ThinkingCapabilities,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::Duration;

fn capability() -> ThinkingCapabilities {
    ThinkingCapabilities {
        reasoning: true,
        max_tokens: 32768,
        context_window: 200000,
        name: String::new(),
        thinking_level_map: BTreeMap::new(),
        compat: json!({}),
    }
}
fn settings(mode: ProviderMode, model: &str) -> ProviderSettings {
    ProviderSettings {
        mode,
        model: model.into(),
        api_key: "fixture-only".into(),
        base_url: "http://127.0.0.1:1".into(),
        options: ProviderOptions {
            thinking_level: Some(ThinkingLevel::Medium),
            thinking_capabilities: Some(capability()),
            ..Default::default()
        },
    }
}
fn context() -> Context {
    Context {
        system_prompt: "system".into(),
        messages: vec![Message::user("中文fixture")],
    }
}

#[test]
fn catalog_clamp_uses_sparse_null_map_and_never_guesses_custom_model() {
    let mut s = settings(ProviderMode::OpenAiResponses, "custom");
    s.options.thinking_capabilities = None;
    assert_eq!(s.supported_thinking_levels(), [ThinkingLevel::Off]);
    assert!(s.validate().unwrap_err().contains("capabilities"));
    let mut cap = capability();
    cap.thinking_level_map = BTreeMap::from([
        ("off".into(), None),
        ("minimal".into(), None),
        ("low".into(), None),
        ("xhigh".into(), Some("xhigh".into())),
        ("max".into(), None),
    ]);
    s.options.thinking_capabilities = Some(cap);
    assert_eq!(
        s.clamp_thinking_level(ThinkingLevel::Off),
        ThinkingLevel::Medium
    );
    assert_eq!(
        s.clamp_thinking_level(ThinkingLevel::Max),
        ThinkingLevel::Xhigh
    );
    s.options.thinking_capabilities.as_mut().unwrap().reasoning = false;
    assert_eq!(s.supported_thinking_levels(), [ThinkingLevel::Off]);
}

#[test]
fn demo_all_seven_levels_validate_without_capabilities_or_payload_changes() {
    let mut s = settings(ProviderMode::Demo, "offline-demo");
    s.options.thinking_capabilities = None;
    s.options.max_tokens = Some(2048);
    let baseline = json!({"fixture":"unchanged","messages":[]});
    assert_eq!(s.supported_thinking_levels(), ThinkingLevel::ALL);
    for level in ThinkingLevel::ALL {
        s.options.thinking_level = Some(level);
        s.validate_thinking_options().unwrap();
        assert_eq!(s.clamp_thinking_level(level), level);
        let mut payload = baseline.clone();
        apply_thinking(&s, &context(), &[], &mut payload).unwrap();
        assert_eq!(payload, baseline);
    }
    s.options.max_tokens = Some(0);
    assert!(s.validate_thinking_options().is_err());
    s.options.max_tokens = None;
    s.options.thinking_budgets.insert("high".into(), 0);
    assert!(s.validate_thinking_options().is_err());
    s.options.thinking_budgets.clear();
    let mut invalid = capability();
    invalid.context_window = 0;
    s.options.thinking_capabilities = Some(invalid);
    assert!(s.validate_thinking_options().is_err());
    s.options.thinking_capabilities = None;
    s.mode = ProviderMode::OpenAiResponses;
    assert!(s
        .validate_thinking_options()
        .unwrap_err()
        .contains("capabilities"));
}

#[test]
fn none_preserves_payload_and_budget_reserves_answer_room() {
    let mut s = settings(ProviderMode::Anthropic, "custom");
    s.options.thinking_level = None;
    let mut payload = json!({"max_tokens":8192});
    apply_thinking(&s, &context(), &[], &mut payload).unwrap();
    assert_eq!(payload, json!({"max_tokens":8192}));
    s.options.thinking_level = Some(ThinkingLevel::High);
    s.options.max_tokens = Some(2048);
    apply_thinking(&s, &context(), &[], &mut payload).unwrap();
    assert_eq!(payload["thinking"]["budget_tokens"], 1024);
    assert_eq!(payload["max_tokens"], 2048);
    s.options.max_tokens = Some(1024);
    assert!(apply_thinking(&s, &context(), &[], &mut payload).is_err());
}
#[test]
fn native_protocols_map_effort_budget_and_hidden_google_off() {
    let mut s = settings(ProviderMode::Anthropic, "claude-opus-4-6");
    s.options.thinking_capabilities.as_mut().unwrap().compat =
        json!({"forceAdaptiveThinking":true});
    let mut p = json!({});
    apply_thinking(&s, &context(), &[], &mut p).unwrap();
    assert_eq!(p["thinking"]["type"], "adaptive");
    assert_eq!(p["output_config"]["effort"], "medium");
    s = settings(ProviderMode::Google, "gemini-3-pro");
    p = json!({});
    apply_thinking(&s, &context(), &[], &mut p).unwrap();
    assert_eq!(
        p["generationConfig"]["thinkingConfig"],
        json!({"includeThoughts":true,"thinkingLevel":"HIGH"})
    );
    s.options.thinking_level = Some(ThinkingLevel::Off);
    apply_thinking(&s, &context(), &[], &mut p).unwrap();
    assert_eq!(
        p["generationConfig"]["thinkingConfig"],
        json!({"thinkingLevel":"LOW"})
    );
    s = settings(ProviderMode::GoogleVertex, "gemini-2.5-pro");
    s.options.thinking_level = Some(ThinkingLevel::High);
    p = json!({});
    apply_thinking(&s, &context(), &[], &mut p).unwrap();
    assert_eq!(
        p["generationConfig"]["thinkingConfig"]["thinkingBudget"],
        32768
    );
    s = settings(
        ProviderMode::AmazonBedrock,
        "us.anthropic.claude-sonnet-4-6-v1",
    );
    p = json!({});
    apply_thinking(&s, &context(), &[], &mut p).unwrap();
    assert_eq!(
        p["additionalModelRequestFields"]["output_config"]["effort"],
        "medium"
    );
    s.model = "us-gov.anthropic.claude-3-7-sonnet-v1".into();
    s.options.bedrock.region = "us-gov-west-1".into();
    p = json!({});
    apply_thinking(&s, &context(), &[], &mut p).unwrap();
    assert!(p["additionalModelRequestFields"]["thinking"]
        .get("display")
        .is_none());
    assert_eq!(
        p["additionalModelRequestFields"]["thinking"]["budget_tokens"],
        8192
    );
    s = settings(ProviderMode::Mistral, "mistral-small-2603");
    p = json!({});
    apply_thinking(&s, &context(), &[], &mut p).unwrap();
    assert_eq!(p["reasoning_effort"], "high");
    s.model = "magistral-medium-latest".into();
    p = json!({});
    apply_thinking(&s, &context(), &[], &mut p).unwrap();
    assert_eq!(p["prompt_mode"], "reasoning");
    assert!(p.get("reasoning_effort").is_none());
}
#[test]
fn completions_compatibility_formats_are_not_generic_effort_strings() {
    for (format, expected) in [
        (
            "zai",
            json!({"thinking":{"type":"enabled","clear_thinking":false}}),
        ),
        ("qwen", json!({"enable_thinking":true})),
        (
            "qwen-chat-template",
            json!({"chat_template_kwargs":{"enable_thinking":true,"preserve_thinking":true}}),
        ),
        ("deepseek", json!({"thinking":{"type":"enabled"}})),
        ("openrouter", json!({"reasoning":{"effort":"medium"}})),
        ("together", json!({"reasoning":{"enabled":true}})),
        ("string-thinking", json!({"thinking":"medium"})),
    ] {
        let mut s = settings(ProviderMode::OpenAiCompatible, "fixture");
        s.options.thinking_capabilities.as_mut().unwrap().compat = json!({"thinkingFormat":format});
        let mut p = json!({});
        apply_thinking(&s, &context(), &[], &mut p).unwrap();
        for (key, value) in expected.as_object().unwrap() {
            assert_eq!(&p[key], value, "{format}");
        }
    }
    let mut s = settings(ProviderMode::OpenAiCompatible, "custom");
    s.options.max_tokens = Some(2048);
    s.options.thinking_capabilities.as_mut().unwrap().compat = json!({"thinkingFormat":"chat-template","chatTemplateKwargs":{"enable":{"$var":"thinking.enabled"},"effort":{"$var":"thinking.effort","omitWhenOff":true},"budget":{"$var":"thinking.budget"}},"thinkingTokenBudgetField":"reasoning_budget"});
    let mut p = json!({});
    apply_thinking(&s, &context(), &[], &mut p).unwrap();
    assert_eq!(
        p["chat_template_kwargs"],
        json!({"enable":true,"effort":"medium","budget":1024})
    );
    assert_eq!(p["reasoning_budget"], 1024);
}
#[test]
fn managed_anthropic_effort_is_a_real_mid_conversation_message_not_only_top_level_high() {
    let mut s = settings(ProviderMode::Anthropic, "claude-managed");
    s.options.thinking_capabilities.as_mut().unwrap().compat =
        json!({"forceAdaptiveThinking":true,"supportsMidConvoEffort":true});
    let mut p = json!({"messages":[{"role":"user","content":[{"type":"text","text":"fixture"}]}]});
    apply_thinking(&s, &context(), &[], &mut p).unwrap();
    assert_eq!(p["output_config"]["effort"], "high");
    assert_eq!(
        p["messages"][1],
        json!({"role":"system","content":[],"output_config":{"effort":"medium"}})
    );
    assert_eq!(
        p["thinking"]["block_binding"]["prefix_mismatch_behavior"],
        "drop_block"
    );
}
fn read_request(stream: &mut TcpStream) -> Value {
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
    let size = headers
        .lines()
        .find_map(|line| {
            line.to_lowercase()
                .strip_prefix("content-length:")
                .map(|value| value.trim().parse::<usize>().unwrap())
        })
        .unwrap();
    let mut body = vec![0; size];
    stream.read_exact(&mut body).unwrap();
    serde_json::from_slice(&body).unwrap()
}
#[test]
fn all_ten_protocols_send_actual_http_native_thinking_payloads() {
    let cases = [
        (ProviderMode::OpenAiCompatible, "gpt-fixture"),
        (ProviderMode::OpenAiResponses, "gpt-fixture"),
        (ProviderMode::OpenAiCodex, "gpt-fixture"),
        (ProviderMode::AzureOpenAiResponses, "deployment-fixture"),
        (ProviderMode::Anthropic, "claude-3-7-sonnet"),
        (ProviderMode::Google, "gemini-3-flash"),
        (ProviderMode::GoogleVertex, "gemini-3-pro"),
        (
            ProviderMode::AmazonBedrock,
            "us.anthropic.claude-sonnet-4-6-v1",
        ),
        (ProviderMode::Mistral, "mistral-small-2603"),
        (ProviderMode::PiMessages, "pi-fixture"),
    ];
    for (mode, model) in cases {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let worker = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let payload = read_request(&mut stream);
            stream
                .write_all(
                    b"HTTP/1.1 400 Bad Request\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
                )
                .unwrap();
            payload
        });
        let mut s = settings(mode.clone(), model);
        s.base_url = base;
        if mode == ProviderMode::OpenAiCodex {
            let claims = URL_SAFE_NO_PAD.encode(
                br#"{"https://api.openai.com/auth":{"chatgpt_account_id":"offline-account"}}"#,
            );
            s.options.oauth = Some(OAuthCredential {
                credential_type: "oauth".into(),
                access: format!("fixture.{claims}.signature"),
                refresh: "offline-refresh".into(),
                expires: u64::MAX,
                account_id: "offline-account".into(),
                extra: BTreeMap::new(),
            });
        }
        if mode == ProviderMode::GoogleVertex {
            s.options.vertex.project = "fixture-project".into();
            s.options.vertex.location = "us-central1".into();
            s.options.vertex.access_token = "offline-token".into();
            s.api_key.clear();
        }
        if mode == ProviderMode::AmazonBedrock {
            s.options.bedrock.region = "us-east-1".into();
            s.options.bedrock.bearer_token = "offline-token".into();
            s.options.bedrock.endpoint_explicit = true;
        }
        let mut provider = ModelProvider {
            settings: s,
            tool_definitions: vec![],
        };
        let result = provider.complete(&context(), &CancellationToken::new(), &mut |_| {});
        assert!(result.unwrap_err().contains("HTTP 400"));
        let p = worker.join().unwrap();
        match mode {
            ProviderMode::OpenAiCompatible => assert_eq!(p["reasoning_effort"], "medium"),
            ProviderMode::OpenAiResponses
            | ProviderMode::OpenAiCodex
            | ProviderMode::AzureOpenAiResponses => assert_eq!(p["reasoning"]["effort"], "medium"),
            ProviderMode::Anthropic => assert_eq!(p["thinking"]["budget_tokens"], 8192),
            ProviderMode::Google => assert_eq!(
                p["generationConfig"]["thinkingConfig"]["thinkingLevel"],
                "MEDIUM"
            ),
            ProviderMode::GoogleVertex => assert_eq!(
                p["generationConfig"]["thinkingConfig"]["thinkingLevel"],
                "HIGH"
            ),
            ProviderMode::AmazonBedrock => assert_eq!(
                p["additionalModelRequestFields"]["output_config"]["effort"],
                "medium"
            ),
            ProviderMode::Mistral => assert_eq!(p["reasoning_effort"], "high"),
            ProviderMode::PiMessages => assert_eq!(p["options"]["reasoning"], "medium"),
            _ => unreachable!(),
        }
    }
}
