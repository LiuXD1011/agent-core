use agent_core::agent_loop::Provider;
use agent_core::tools::AgentTool;
use agent_core::types::{
    AssistantMessage, CancellationToken, Content, Context, Event, Message, StopReason, ToolCall,
    ToolResult,
};
use agent_runtime::automatic::{AutomationHandle, RuntimeEvent};
use agent_runtime::provider::{ModelProvider, ProviderMode, ProviderSettings};
use agent_runtime::retry::{
    is_context_overflow, is_transient_error, RetryOptions, RetryingProvider,
};
use agent_runtime::runtime::{run_with_provider, MessageQueues, RunOptions};
use agent_runtime::session::Session;
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use std::thread;
use std::time::{Duration, Instant};

struct Fixed {
    responses: VecDeque<Result<AssistantMessage, String>>,
    calls: usize,
    partial: Option<AssistantMessage>,
}
impl Provider for Fixed {
    fn complete(
        &mut self,
        _: &Context,
        _: &CancellationToken,
        update: &mut dyn FnMut(AssistantMessage),
    ) -> Result<AssistantMessage, String> {
        self.calls += 1;
        if let Some(partial) = self.partial.take() {
            update(partial);
        }
        self.responses
            .pop_front()
            .expect("unexpected retry request")
    }
}
fn fixed(responses: Vec<Result<AssistantMessage, String>>) -> Fixed {
    Fixed {
        responses: responses.into(),
        calls: 0,
        partial: None,
    }
}
fn enabled() -> RunOptions {
    let mut options = RunOptions::default();
    options.automation.retry = RetryOptions {
        enabled: true,
        max_retries: 2,
        base_delay_ms: 0,
        max_delay_ms: 0,
    };
    options
}

#[test]
fn classifier_only_retries_explicit_transient_failures_and_excludes_limits_and_context() {
    for error in [
        "HTTP 429",
        "HTTP 503",
        "HTTP 529",
        "EAI_AGAIN",
        "ENOTFOUND",
        "connection lost",
        "request timed out",
        "overloaded",
        "stream ended before a terminal response event",
    ] {
        assert!(is_transient_error(error), "{error}");
    }
    for error in [
        "HTTP 400",
        "HTTP 401",
        "HTTP 403",
        "HTTP 429 insufficient_quota",
        "HTTP 503 billing exhausted",
        "HTTP 500 Invalid tool arguments",
        "HTTP 400 (context_length_exceeded)",
        "Unknown error",
        "failed at offset 500",
        "unsupported stream event",
    ] {
        assert!(!is_transient_error(error), "{error}");
    }
    for code in [
        "context_length_exceeded",
        "context_window_exceeded",
        "too_many_tokens",
        "max_context_length_exceeded",
    ] {
        assert!(is_context_overflow(&format!("HTTP 400 ({code})")));
    }
    assert!(!is_context_overflow("HTTP 400"));
    assert!(!is_context_overflow("HTTP 413"));
    assert!(!is_context_overflow("rate limit: too many tokens"));
}

#[test]
fn bounded_retry_runs_single_prompt_and_commits_only_final_assistant() {
    let mut provider = fixed(vec![
        Err("HTTP 429".into()),
        Err("HTTP 503".into()),
        Ok(AssistantMessage::text("answer")),
    ]);
    let options = enabled();
    let mut session = Session::default();
    let prompt = Message::user("one prompt");
    let mut events = vec![];
    run_with_provider(
        &mut session,
        prompt.clone(),
        &mut provider,
        &[],
        &options,
        &MessageQueues::default(),
        &CancellationToken::new(),
        &mut |event| events.push(event),
    )
    .unwrap();
    assert_eq!(provider.calls, 3);
    assert_eq!(session.context.messages.len(), 2);
    assert_eq!(session.context.messages[0], prompt);
    assert_eq!(session.context.messages[1].text(), "answer");
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, Event::MessageEnd { message } if message == &prompt))
            .count(),
        1
    );
    let report = options.automation_state.snapshot();
    assert_eq!(report.retries, 2);
    assert_eq!(report.state.request_count, 3);
    assert!(report.events.iter().any(|event| matches!(
        event,
        RuntimeEvent::RetryFinished {
            success: true,
            attempt: 2,
            ..
        }
    )));
}

#[test]
fn retry_exhaustion_preserves_final_error_once_and_reports_finished_failure() {
    let mut provider = fixed(vec![
        Err("HTTP 503".into()),
        Err("HTTP 503".into()),
        Err("HTTP 503".into()),
    ]);
    let options = enabled();
    let mut session = Session::default();
    assert_eq!(
        run_with_provider(
            &mut session,
            Message::user("prompt"),
            &mut provider,
            &[],
            &options,
            &MessageQueues::default(),
            &CancellationToken::new(),
            &mut |_| {}
        )
        .unwrap_err(),
        "HTTP 503"
    );
    assert_eq!(provider.calls, 3);
    assert_eq!(session.context.messages.len(), 2);
    assert!(
        matches!(&session.context.messages[1], Message::Assistant(message) if message.stop_reason == StopReason::Error)
    );
    assert!(options
        .automation_state
        .drain_events()
        .iter()
        .any(|event| matches!(
            event,
            RuntimeEvent::RetryFinished {
                success: false,
                attempt: 2,
                ..
            }
        )));
}

#[test]
fn backoff_is_cancelled_promptly_and_never_retries_after_cancel() {
    let signal = CancellationToken::new();
    let cancel = signal.clone();
    let worker = thread::spawn(move || {
        thread::sleep(Duration::from_millis(30));
        cancel.cancel();
    });
    let mut provider = fixed(vec![Err("HTTP 503".into())]);
    let mut options = enabled();
    options.automation.retry.base_delay_ms = 2000;
    options.automation.retry.max_delay_ms = 2000;
    let mut session = Session::default();
    let started = Instant::now();
    assert!(run_with_provider(
        &mut session,
        Message::user("prompt"),
        &mut provider,
        &[],
        &options,
        &MessageQueues::default(),
        &signal,
        &mut |_| {}
    )
    .unwrap_err()
    .contains("Retry cancelled"));
    worker.join().unwrap();
    assert!(started.elapsed() < Duration::from_millis(500));
    assert_eq!(provider.calls, 1);
    assert!(
        matches!(&session.context.messages[1], Message::Assistant(message) if message.stop_reason == StopReason::Aborted)
    );
}

struct Count(Arc<AtomicUsize>);
impl AgentTool for Count {
    fn name(&self) -> &str {
        "count"
    }
    fn execute(
        &self,
        _: &str,
        _: &Value,
        _: &CancellationToken,
        _: &mut dyn FnMut(ToolResult),
    ) -> Result<ToolResult, String> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(ToolResult::text("done"))
    }
}

#[test]
fn failed_partial_tool_call_is_not_retried_or_executed_and_its_pair_is_closed() {
    let mut partial = AssistantMessage::text("visible partial");
    partial.content.push(Content::ToolCall(ToolCall {
        id: "call-1".into(),
        name: "count".into(),
        arguments: json!({}),
    }));
    let mut provider = fixed(vec![Err("HTTP 503".into())]);
    provider.partial = Some(partial);
    let calls = Arc::new(AtomicUsize::new(0));
    let tools: Vec<Box<dyn AgentTool>> = vec![Box::new(Count(calls.clone()))];
    let options = enabled();
    let mut session = Session::default();
    let mut events = vec![];
    assert!(run_with_provider(
        &mut session,
        Message::user("prompt"),
        &mut provider,
        &tools,
        &options,
        &MessageQueues::default(),
        &CancellationToken::new(),
        &mut |event| events.push(event)
    )
    .is_err());
    assert_eq!(provider.calls, 1);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(session.context.messages[1].text(), "visible partial");
    assert!(
        matches!(&session.context.messages[2], Message::ToolResult(result) if result.tool_call_id == "call-1" && result.is_error)
    );
    agent_runtime::automatic::validate_request_boundary(&session.context.messages).unwrap();
    assert!(!events
        .iter()
        .any(|event| matches!(event, Event::ToolExecutionStart { .. })));
    assert_eq!(options.automation_state.snapshot().retries, 0);
}

#[test]
fn retry_of_next_request_does_not_repeat_completed_tool_batch() {
    let mut call = AssistantMessage::text("");
    call.stop_reason = StopReason::ToolUse;
    call.content = vec![Content::ToolCall(ToolCall {
        id: "call-1".into(),
        name: "count".into(),
        arguments: json!({}),
    })];
    let mut provider = fixed(vec![
        Ok(call),
        Err("HTTP 503".into()),
        Ok(AssistantMessage::text("finished")),
    ]);
    let calls = Arc::new(AtomicUsize::new(0));
    let tools: Vec<Box<dyn AgentTool>> = vec![Box::new(Count(calls.clone()))];
    let mut session = Session::default();
    run_with_provider(
        &mut session,
        Message::user("prompt"),
        &mut provider,
        &tools,
        &enabled(),
        &MessageQueues::default(),
        &CancellationToken::new(),
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(provider.calls, 3);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        session
            .context
            .messages
            .iter()
            .filter(|message| matches!(message, Message::ToolResult(_)))
            .count(),
        1
    );
}

#[test]
fn summary_calls_use_the_same_retry_budget_but_do_not_count_as_plan_requests() {
    let mut provider = fixed(vec![
        Err("HTTP 503".into()),
        Ok(AssistantMessage::text("summary")),
    ]);
    let handle = AutomationHandle::default();
    let options = enabled();
    let mut retry = RetryingProvider::new(
        &mut provider,
        options.automation.retry,
        handle.clone(),
        "compaction",
        None,
    );
    let response = retry
        .complete_summary(&Context::default(), &CancellationToken::new(), &mut |_| {})
        .unwrap();
    assert_eq!(response.content, vec![Content::text("summary")]);
    assert_eq!(provider.calls, 2);
    assert_eq!(handle.state().request_count, 0);
    assert!(handle.drain_events().iter().any(|event| matches!(event, RuntimeEvent::RetryScheduled { source, .. } if source == "compaction")));
}

#[test]
fn exponential_wait_is_bounded_and_configuration_is_validated() {
    let options = RetryOptions {
        base_delay_ms: 10,
        max_delay_ms: 50,
        ..Default::default()
    };
    assert_eq!(
        (1..=5)
            .map(|attempt| options.delay_ms(attempt))
            .collect::<Vec<_>>(),
        [10, 20, 40, 50, 50]
    );
    assert!(RetryOptions {
        max_retries: 17,
        ..Default::default()
    }
    .validate()
    .is_err());
}

#[test]
fn real_loopback_http_503_retries_identical_payload_without_duplicate_prompt() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        let mut observed = vec![];
        for response in [
            "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                .to_owned(),
            {
                let body = "data: {\"choices\":[{\"delta\":{\"content\":\"offline answer\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
                format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())
            },
        ] {
            let start = Instant::now();
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error)
                        if error.kind() == std::io::ErrorKind::WouldBlock
                            && start.elapsed() < Duration::from_secs(5) =>
                    {
                        thread::sleep(Duration::from_millis(5))
                    }
                    Err(error) => panic!("loopback accept: {error}"),
                }
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut bytes = vec![];
            let mut buffer = [0_u8; 4096];
            loop {
                let count = stream.read(&mut buffer).unwrap();
                assert!(count > 0);
                bytes.extend_from_slice(&buffer[..count]);
                if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                    let header = String::from_utf8_lossy(&bytes[..end]);
                    let length: usize = header
                        .lines()
                        .filter_map(|line| line.split_once(':'))
                        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                        .unwrap()
                        .1
                        .trim()
                        .parse()
                        .unwrap();
                    if bytes.len() >= end + 4 + length {
                        observed.push(
                            serde_json::from_slice::<Value>(&bytes[end + 4..end + 4 + length])
                                .unwrap(),
                        );
                        break;
                    }
                }
            }
            stream.write_all(response.as_bytes()).unwrap();
            stream.flush().unwrap();
        }
        observed
    });
    let mut provider = ModelProvider {
        settings: ProviderSettings {
            mode: ProviderMode::OpenAiCompatible,
            base_url: format!("http://{address}/v1"),
            model: "offline-fixture-model".into(),
            api_key: "offline-fixture-not-a-credential".into(),
            ..Default::default()
        },
        tool_definitions: vec![],
    };
    let mut session = Session::default();
    run_with_provider(
        &mut session,
        Message::user("one prompt"),
        &mut provider,
        &[],
        &enabled(),
        &MessageQueues::default(),
        &CancellationToken::new(),
        &mut |_| {},
    )
    .unwrap();
    let observed = server.join().unwrap();
    assert_eq!(observed.len(), 2);
    assert_eq!(observed[0], observed[1]);
    assert_eq!(session.context.messages.len(), 2);
    assert_eq!(session.context.messages[1].text(), "offline answer");
}

#[test]
fn opaque_reasoning_partial_without_visible_text_is_not_retried() {
    let mut partial = AssistantMessage::text("");
    partial.content = vec![Content::Thinking {
        thinking: String::new(),
    }];
    partial.protocol_metadata = Some(
        json!({"protocol":"openai-responses","items":{"0":{"id":"rs_1","type":"reasoning","summary":[],"encrypted_content":"opaque"}}}),
    );
    let mut provider = fixed(vec![Err("HTTP 503".into())]);
    provider.partial = Some(partial);
    let options = enabled();
    let mut session = Session::default();
    assert!(run_with_provider(
        &mut session,
        Message::user("prompt"),
        &mut provider,
        &[],
        &options,
        &MessageQueues::default(),
        &CancellationToken::new(),
        &mut |_| {}
    )
    .is_err());
    assert_eq!(provider.calls, 1);
    assert_eq!(options.automation_state.snapshot().retries, 0);
    assert!(
        matches!(&session.context.messages[1], Message::Assistant(message) if message.protocol_metadata.as_ref().unwrap()["items"]["0"]["encrypted_content"] == "opaque")
    );
}
