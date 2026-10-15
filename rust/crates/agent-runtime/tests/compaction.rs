use agent_core::agent_loop::Provider;
use agent_core::types::{
    AssistantMessage, CancellationToken, Content, Context, Message, StopReason, ToolCall,
    ToolResultMessage, Usage,
};
use agent_runtime::compaction::{compact_session, serialize_conversation};
use agent_runtime::legacy_session::{parse_legacy_session, LeafSelection};
use agent_runtime::session::{Session, SessionStore};
use serde_json::{json, Value};
use std::fs;

struct SummaryProvider {
    response: Result<AssistantMessage, String>,
    calls: usize,
    cancel_on_call: bool,
    request: Option<Context>,
}

impl Provider for SummaryProvider {
    fn complete(
        &mut self,
        context: &Context,
        signal: &CancellationToken,
        _on_update: &mut dyn FnMut(AssistantMessage),
    ) -> Result<AssistantMessage, String> {
        self.calls += 1;
        self.request = Some(context.clone());
        if self.cancel_on_call {
            signal.cancel();
        }
        self.response.clone()
    }
}

fn provider(response: Result<AssistantMessage, String>) -> SummaryProvider {
    SummaryProvider {
        response,
        calls: 0,
        cancel_on_call: false,
        request: None,
    }
}

fn session() -> Session {
    Session {
        context: Context {
            system_prompt: "system".into(),
            messages: vec![
                Message::user("old question"),
                Message::Assistant(AssistantMessage::text("old answer")),
                Message::user("latest question"),
                Message::Assistant(AssistantMessage::text("latest answer")),
            ],
        },
        ..Default::default()
    }
}

fn tool_call(id: &str) -> Message {
    let mut assistant = AssistantMessage::text("");
    assistant.stop_reason = StopReason::ToolUse;
    assistant.content = vec![Content::ToolCall(ToolCall {
        id: id.into(),
        name: "read".into(),
        arguments: json!({"path":"README.md"}),
    })];
    Message::Assistant(assistant)
}

fn tool_result(id: &str) -> Message {
    Message::ToolResult(ToolResultMessage {
        tool_call_id: id.into(),
        tool_name: "read".into(),
        content: vec![Content::text("file content")],
        details: Value::Null,
        usage: None,
        added_tool_names: vec![],
        is_error: false,
        timestamp: 1,
    })
}

#[test]
fn checkpoints_completed_history_and_keeps_latest_user_turn_exactly() {
    let mut session = session();
    let original = session.context.messages.clone();
    let mut model = provider(Ok(AssistantMessage::text("## Goal\nContinue work")));
    let result = compact_session(&mut session, &mut model, 1, &CancellationToken::new()).unwrap();
    assert_eq!(result.archived_message_count, 2);
    assert_eq!(result.kept_message_count, 2);
    assert_eq!(&session.context.messages[1..], &original[2..]);
    assert!(session.context.messages[0]
        .text()
        .contains("## Goal\nContinue work"));
    assert_eq!(session.compactions[0].archived_messages, original[..2]);
    assert_eq!(session.context.system_prompt, "system");
    let request = model.request.unwrap();
    assert_eq!(request.messages.len(), 1);
    assert!(matches!(&request.messages[0], Message::User(_)));
    assert!(request.messages[0].text().contains("[User]: old question"));
    assert!(!request.messages[0].text().contains("latest question"));
}

#[test]
fn all_failure_modes_preserve_the_complete_session() {
    let mut truncated = AssistantMessage::text("incomplete");
    truncated.stop_reason = StopReason::Length;
    let mut tool = AssistantMessage::text("bad");
    tool.content.push(Content::ToolCall(ToolCall {
        id: "call".into(),
        name: "write".into(),
        arguments: json!({}),
    }));
    for response in [
        Err("network drop".into()),
        Ok(AssistantMessage::error("failed", false)),
        Ok(AssistantMessage::error("cancelled", true)),
        Ok(truncated),
        Ok(AssistantMessage::text(" \n ")),
        Ok(tool),
    ] {
        let mut session = session();
        let before = serde_json::to_value(&session).unwrap();
        assert!(compact_session(
            &mut session,
            &mut provider(response),
            1,
            &CancellationToken::new()
        )
        .is_err());
        assert_eq!(serde_json::to_value(&session).unwrap(), before);
    }
}

#[test]
fn cancellation_before_and_during_summary_is_transactional() {
    let mut session = session();
    let before = serde_json::to_value(&session).unwrap();
    let signal = CancellationToken::new();
    signal.cancel();
    let mut model = provider(Ok(AssistantMessage::text("summary")));
    assert!(compact_session(&mut session, &mut model, 1, &signal).is_err());
    assert_eq!(model.calls, 0);
    assert_eq!(serde_json::to_value(&session).unwrap(), before);
    model.cancel_on_call = true;
    assert!(compact_session(&mut session, &mut model, 1, &CancellationToken::new()).is_err());
    assert_eq!(serde_json::to_value(&session).unwrap(), before);
}

#[test]
fn never_splits_tool_call_result_pairs() {
    let mut session = session();
    session.context.messages = vec![
        Message::user("old"),
        tool_call("old-call"),
        tool_result("old-call"),
        Message::user("keep"),
        tool_call("kept-call"),
        tool_result("kept-call"),
    ];
    let kept = session.context.messages[3..].to_vec();
    let result = compact_session(
        &mut session,
        &mut provider(Ok(AssistantMessage::text("summary"))),
        1,
        &CancellationToken::new(),
    )
    .unwrap();
    assert_eq!(result.archived_message_count, 3);
    assert_eq!(&session.context.messages[1..], kept);
    assert!(result
        .summary
        .contains("<read-files>\nREADME.md\n</read-files>"));
}

#[test]
fn expands_cut_back_to_preserve_cross_turn_pairs_and_rejects_unfinished_archive() {
    let mut session = session();
    session.context.messages = vec![
        Message::user("old"),
        tool_call("cross"),
        Message::user("latest"),
        tool_result("cross"),
    ];
    let before = serde_json::to_value(&session).unwrap();
    let mut model = provider(Ok(AssistantMessage::text("summary")));
    assert!(compact_session(&mut session, &mut model, 1, &CancellationToken::new()).is_err());
    assert_eq!(model.calls, 0);
    assert_eq!(serde_json::to_value(&session).unwrap(), before);
    session.context.messages = vec![
        Message::user("old"),
        tool_call("unfinished"),
        Message::user("latest"),
    ];
    assert!(
        compact_session(&mut session, &mut model, 1, &CancellationToken::new())
            .unwrap_err()
            .contains("unfinished")
    );
}

#[test]
fn preserves_at_least_one_turn_and_rejects_empty_history_without_model_call() {
    let mut session = session();
    let kept = session.context.messages[2..].to_vec();
    compact_session(
        &mut session,
        &mut provider(Ok(AssistantMessage::text("summary"))),
        0,
        &CancellationToken::new(),
    )
    .unwrap();
    assert_eq!(&session.context.messages[1..], kept);
    let mut model = provider(Ok(AssistantMessage::text("unused")));
    assert!(compact_session(&mut session, &mut model, 10, &CancellationToken::new()).is_err());
    assert_eq!(model.calls, 0);
}

#[test]
fn successive_checkpoints_keep_cumulative_archive_and_summary_context() {
    let mut session = session();
    compact_session(
        &mut session,
        &mut provider(Ok(AssistantMessage::text("first summary"))),
        1,
        &CancellationToken::new(),
    )
    .unwrap();
    session.context.messages.push(Message::user("newest"));
    let mut model = provider(Ok(AssistantMessage::text("second summary")));
    compact_session(&mut session, &mut model, 1, &CancellationToken::new()).unwrap();
    assert_eq!(session.compactions.len(), 2);
    assert_eq!(
        session.compactions[0].archived_messages[0].text(),
        "old question"
    );
    assert!(session.compactions[1].archived_messages[0]
        .text()
        .contains("first summary"));
    assert!(model.request.unwrap().messages[0]
        .text()
        .contains("first summary"));
    assert_eq!(session.context.messages[1].text(), "newest");
}

#[test]
fn archive_and_summary_usage_survive_native_session_roundtrip() {
    let directory =
        std::env::temp_dir().join(format!("compaction-store-{}", Session::default().id));
    let store = SessionStore::new(&directory);
    let mut session = session();
    let mut summary = AssistantMessage::text("summary");
    summary.usage = Some(Usage {
        input: 10,
        output: 5,
        total_tokens: 15,
        ..Default::default()
    });
    compact_session(
        &mut session,
        &mut provider(Ok(summary)),
        1,
        &CancellationToken::new(),
    )
    .unwrap();
    store.save(&session).unwrap();
    let restored = store.list().unwrap().pop().unwrap();
    assert_eq!(restored.compactions, session.compactions);
    assert_eq!(
        restored.compactions[0].usage.as_ref().unwrap().total_tokens,
        15
    );
    assert_eq!(restored.context, session.context);
    fs::remove_file(directory.join(format!("{}.json", session.id))).unwrap();
    fs::remove_dir(directory).unwrap();
}

#[test]
fn serialized_tool_output_is_bounded_without_changing_archived_message() {
    let mut message = tool_result("id");
    if let Message::ToolResult(result) = &mut message {
        result.content = vec![Content::text("中".repeat(2100))];
    }
    let serialized = serialize_conversation(&[message.clone()]);
    assert!(serialized.contains("100 more characters truncated"));
    assert_eq!(message.text().chars().count(), 2100);
}

#[test]
fn deterministic_file_tracking_survives_successive_checkpoint_generation() {
    let mut session = session();
    session.context.messages = vec![
        Message::user("old"),
        tool_call("read-file"),
        tool_result("read-file"),
        Message::user("kept"),
    ];
    compact_session(
        &mut session,
        &mut provider(Ok(AssistantMessage::text("first"))),
        1,
        &CancellationToken::new(),
    )
    .unwrap();
    session.context.messages.push(Message::user("newest"));
    let result = compact_session(
        &mut session,
        &mut provider(Ok(AssistantMessage::text("second"))),
        1,
        &CancellationToken::new(),
    )
    .unwrap();
    assert!(result
        .summary
        .contains("<read-files>\nREADME.md\n</read-files>"));
}

#[test]
fn imported_checkpoints_do_not_count_as_an_additional_recent_user_turn() {
    let records = [
        json!({"type":"session","version":3,"id":"old","cwd":"/project"}),
        json!({"type":"compaction","id":"summary","parentId":null,"timestamp":"2025-01-01T00:00:00Z","summary":"previous","firstKeptEntryId":"absent","tokensBefore":1}),
        json!({"type":"message","id":"user","parentId":"summary","message":{"role":"user","content":"keep","timestamp":1}}),
        json!({"type":"message","id":"latest","parentId":"user","message":{"role":"user","content":"latest","timestamp":2}}),
    ];
    let text = records
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    let mut session = parse_legacy_session(&text, "fixture.jsonl".into())
        .unwrap()
        .import(LeafSelection::Latest)
        .unwrap();
    let before = serde_json::to_value(&session).unwrap();
    let mut model = provider(Ok(AssistantMessage::text("unused")));
    assert!(compact_session(&mut session, &mut model, 2, &CancellationToken::new()).is_err());
    assert_eq!(model.calls, 0);
    assert_eq!(serde_json::to_value(&session).unwrap(), before);
}
