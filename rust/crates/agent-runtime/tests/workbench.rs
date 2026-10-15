use agent_core::types::{
    AssistantMessage, Content, Message, StopReason, ToolCall, ToolResultMessage,
};
use agent_runtime::session::Session;
use agent_runtime::workbench::{matches_query, StepOutcome, TaskFilter, TaskPhase, TaskProjection};
use agent_runtime::workbench::{BranchSnapshot, HistoryError};
use serde_json::json;

fn record_file(session: &mut Session, id: &str, path: &str) -> String {
    record_file_with_diff(session, id, path, &format!("+ {path}"))
}

fn record_file_with_diff(session: &mut Session, id: &str, path: &str, diff: &str) -> String {
    session.append_message(Message::user("edit file")).unwrap();
    session.append_message(call(id, "edit", path)).unwrap();
    let mut output = result(id, "edit", false);
    if let Message::ToolResult(result) = &mut output {
        result.details = json!({"diff":diff});
    }
    let entry = session.append_message(output).unwrap();
    session
        .append_message(Message::Assistant(AssistantMessage::text("done")))
        .unwrap();
    entry
}

fn compact_all(session: &mut Session) {
    use agent_core::types::UserMessage;
    use agent_runtime::compaction::NativeCompaction;
    let summary = "The conversation history before this point was compacted into the following summary:\n\n<summary>\nfixture\n</summary>";
    session
        .record_compaction(
            NativeCompaction {
                id: format!("checkpoint-{}", session.compactions.len()),
                timestamp: 10,
                summary: "fixture".into(),
                archived_messages: session.context.messages.clone(),
                kept_message_count: 0,
                usage: None,
            },
            vec![Message::User(UserMessage {
                content: vec![Content::text(summary)],
                timestamp: 10,
            })],
        )
        .unwrap();
}

#[test]
fn archived_artifacts_and_diffs_survive_compaction_and_round_trip() {
    let mut s = Session::default();
    let original_entry = record_file(&mut s, "first", "old.md");
    compact_all(&mut s);
    record_file(&mut s, "second", "new.md");
    compact_all(&mut s);
    assert!(TaskProjection::from_session(&s, false).artifacts.is_empty());
    let before = serde_json::to_value(&s).unwrap();
    let loaded: Session = serde_json::from_value(before.clone()).unwrap();
    let snapshot = BranchSnapshot::from_session(&loaded).unwrap();
    assert_eq!(snapshot.projection.artifacts.len(), 2);
    assert_eq!(snapshot.projection.user_turns, 2); // synthetic summaries are excluded
    assert_eq!(snapshot.projection.phase, TaskPhase::Completed);
    let detail = snapshot
        .artifact_detail(snapshot.projection.artifacts[0].result_message)
        .unwrap();
    assert_eq!(detail.entry_id, original_entry);
    assert_eq!(detail.diff.as_deref(), Some("+ old.md"));
    assert_eq!(serde_json::to_value(&s).unwrap(), before);
}

#[test]
fn only_selected_ancestors_are_visible_after_navigation_and_clone() {
    let mut s = Session::default();
    record_file(&mut s, "base", "shared.md");
    let base = s.leaf_id.clone();
    record_file(&mut s, "left", "left.md");
    let left = s.leaf_id.clone();
    s.navigate(base.as_deref()).unwrap();
    record_file(&mut s, "right", "right.md");
    let right = BranchSnapshot::from_session(&s).unwrap();
    assert_eq!(
        right
            .projection
            .artifacts
            .iter()
            .map(|a| a.path.as_str())
            .collect::<Vec<_>>(),
        ["shared.md", "right.md"]
    );
    s.navigate(left.as_deref()).unwrap();
    let clone = s.clone_current_branch().unwrap();
    let snapshot = BranchSnapshot::from_session(&clone).unwrap();
    assert_eq!(
        snapshot
            .projection
            .artifacts
            .iter()
            .map(|a| a.path.as_str())
            .collect::<Vec<_>>(),
        ["shared.md", "left.md"]
    );
    s.navigate(None).unwrap();
    assert!(BranchSnapshot::from_session(&s)
        .unwrap()
        .projection
        .artifacts
        .is_empty());
}

#[test]
fn history_errors_are_typed_and_do_not_echo_corrupt_payloads() {
    let mut s = Session::default();
    record_file(&mut s, "one", "a");
    compact_all(&mut s);
    s.entries[0]
        .fields
        .insert("message".into(), json!({"role":"SECRET-CORRUPT"}));
    let error = BranchSnapshot::from_session(&s).err().unwrap();
    assert_eq!(error, HistoryError::InvalidMessage);
    assert!(!format!("{error:?} {error}").contains("SECRET"));
    s.entries[0].parent_id = Some("SECRET-PARENT".into());
    assert_eq!(
        BranchSnapshot::from_session(&s).err(),
        Some(HistoryError::InvalidBranch)
    );
}

#[test]
fn flat_legacy_messages_use_existing_decoder_and_unicode_diff_is_bounded() {
    let mut s = Session::default();
    record_file_with_diff(&mut s, "one", "中文.md", &"界".repeat(24_001));
    let snapshot = BranchSnapshot::from_session(&s).unwrap();
    let detail = snapshot
        .artifact_detail(snapshot.projection.artifacts[0].result_message)
        .unwrap();
    assert_eq!(detail.diff.as_ref().unwrap().chars().count(), 24_000);
    assert!(detail.diff_truncated);
    for entry in &mut s.entries {
        entry.fields.remove("native");
    }
    use agent_runtime::legacy_session::{parse_legacy_session, LeafSelection};
    let mut lines = vec![json!({"type":"session","version":3,"id":"history-fixture","cwd":"C:/fixture","timestamp":"2026-10-15T00:00:00Z"}).to_string()];
    lines.extend(s.entries.iter().map(|entry| entry.to_value().to_string()));
    let legacy = parse_legacy_session(&lines.join("\n"), "fixture.jsonl".into())
        .unwrap()
        .import(LeafSelection::Latest)
        .unwrap();
    let imported = BranchSnapshot::from_session(&legacy).unwrap();
    assert_eq!(imported.projection.artifacts.len(), 1);
    let flat = Session {
        context: s.context.clone(),
        ..Session::default()
    };
    assert_eq!(
        BranchSnapshot::from_session(&flat)
            .unwrap()
            .projection
            .artifacts
            .len(),
        1
    );
}

fn call(id: &str, name: &str, path: &str) -> Message {
    Message::Assistant(AssistantMessage {
        content: vec![Content::ToolCall(ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: json!({"path": path, "content":"not retained"}),
        })],
        stop_reason: StopReason::ToolUse,
        ..AssistantMessage::text("")
    })
}
fn result(id: &str, name: &str, error: bool) -> Message {
    Message::ToolResult(ToolResultMessage {
        tool_call_id: id.into(),
        tool_name: name.into(),
        content: vec![],
        details: json!({}),
        usage: None,
        added_tool_names: vec![],
        is_error: error,
        timestamp: 1,
    })
}

#[test]
fn terminal_phases_are_exact_and_running_is_transient() {
    assert_eq!(
        TaskProjection::from_messages(&[], false).phase,
        TaskPhase::Draft
    );
    let mut messages = vec![Message::user("task")];
    assert_eq!(
        TaskProjection::from_messages(&messages, false).phase,
        TaskPhase::Waiting
    );
    for (stop, phase) in [
        (StopReason::Stop, TaskPhase::Completed),
        (StopReason::Length, TaskPhase::Limited),
        (StopReason::Error, TaskPhase::Failed),
        (StopReason::Aborted, TaskPhase::Interrupted),
        (StopReason::ToolUse, TaskPhase::Waiting),
    ] {
        messages.truncate(1);
        messages.push(Message::Assistant(AssistantMessage {
            stop_reason: stop,
            ..AssistantMessage::text("result")
        }));
        messages.push(Message::Custom {
            kind: "view".into(),
            data: json!({}),
            timestamp: 2,
        });
        assert_eq!(TaskProjection::from_messages(&messages, false).phase, phase);
        assert_eq!(
            TaskProjection::from_messages(&messages, true).phase,
            TaskPhase::Running
        );
    }
}

#[test]
fn only_successful_exactly_matched_mutations_produce_artifacts() {
    let messages = vec![
        Message::user("task"),
        call("1", "write", "one.txt"),
        call("2", "edit", "two.txt"),
        result("2", "edit", true),
        result("1", "write", false),
        call("3", "read", "three.txt"),
        result("3", "read", false),
        call("4", "write", "unfinished.txt"),
    ];
    let p = TaskProjection::from_messages(&messages, false);
    assert_eq!(p.artifacts.len(), 1);
    assert_eq!(p.artifacts[0].path, "one.txt");
    assert_eq!(p.artifacts[0].result_message, 4);
    assert_eq!(p.steps[0].outcome, StepOutcome::Succeeded);
    assert_eq!(p.steps[1].outcome, StepOutcome::Failed);
    assert_eq!(p.steps[3].outcome, StepOutcome::Unfinished);
    assert!(!format!("{p:?}").contains("not retained"));
}

#[test]
fn ambiguous_or_foreign_results_never_claim_file_changes() {
    let messages = vec![
        call("1", "write", "a"),
        call("1", "write", "b"),
        result("1", "write", false),
        result("orphan", "write", false),
        call("2", "write", "c"),
        result("2", "edit", false),
        Message::user("new turn"),
        result("2", "write", false),
    ];
    let p = TaskProjection::from_messages(&messages, false);
    assert!(p.artifacts.is_empty());
    assert_eq!(
        p.steps
            .iter()
            .filter(|s| s.outcome == StepOutcome::Unmatched)
            .count(),
        4
    );
}

#[test]
fn malformed_paths_and_opaque_results_are_not_opened_or_retained() {
    let mut messages = vec![];
    for (i, path) in ["", "bad\npath", &"x".repeat(4097)].iter().enumerate() {
        messages.extend([
            call(&i.to_string(), "write", path),
            result(&i.to_string(), "write", false),
        ]);
    }
    assert!(TaskProjection::from_messages(&messages, false)
        .artifacts
        .is_empty());
    messages.extend([
        call("ok", "edit", "../outside.txt"),
        result("ok", "edit", false),
    ]);
    // Merely records a string; does not resolve, inspect, or execute it.
    assert_eq!(
        TaskProjection::from_messages(&messages, false).artifacts[0].path,
        "../outside.txt"
    );
}

#[test]
fn search_and_filters_never_use_message_content_or_modify_sessions() {
    let mut s = Session {
        name: "Rust 重构".into(),
        workspace: "C:/Projects/Sample".into(),
        ..Session::default()
    };
    s.context.messages.push(Message::user("private query"));
    let before = serde_json::to_value(&s).unwrap();
    assert!(matches_query(&s, "rust"));
    assert!(matches_query(&s, " 重构 "));
    assert!(matches_query(&s, "PROJECTS"));
    assert!(matches_query(&s, "  "));
    assert!(!matches_query(&s, "private query"));
    assert!(TaskFilter::Attention.matches(TaskProjection::from_session(&s, false).phase));
    assert!(!TaskFilter::Completed.matches(TaskPhase::Limited));
    assert!(!TaskFilter::Attention.matches(TaskPhase::Running));
    assert_eq!(serde_json::to_value(&s).unwrap(), before);
}
