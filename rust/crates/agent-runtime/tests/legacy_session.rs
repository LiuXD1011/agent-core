use agent_core::types::Message;
use agent_runtime::legacy_session::{load_legacy_session, parse_legacy_session, LeafSelection};
use agent_runtime::provider::{
    anthropic_request_payload, google_request_payload, responses_request_payload,
};
use agent_runtime::session::{Session, SessionStore};
use serde_json::{json, Value};
use std::fs;
use std::path::PathBuf;

const TIMESTAMP: &str = "2025-01-01T00:00:00.000Z";

fn header(version: u64) -> Value {
    json!({"type":"session","version":version,"id":"old-session","timestamp":TIMESTAMP,"cwd":"/original/project","unknownHeader":{"kept":true}})
}

fn user(id: &str, parent: Option<&str>, text: &str) -> Value {
    json!({"type":"message","id":id,"parentId":parent,"timestamp":TIMESTAMP,"message":{"role":"user","content":text,"timestamp":1}})
}

fn assistant(id: &str, parent: &str, text: &str) -> Value {
    json!({"type":"message","id":id,"parentId":parent,"timestamp":TIMESTAMP,"message":{"role":"assistant","content":[{"type":"text","text":text}],"api":"anthropic-messages","provider":"anthropic","model":"fixture-model","usage":{"input":1,"output":1,"cacheRead":0,"cacheWrite":0,"totalTokens":2,"cost":{"total":0}},"stopReason":"stop","timestamp":2}})
}

fn jsonl(records: &[Value]) -> String {
    records
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n")
}

fn parsed(records: &[Value]) -> agent_runtime::legacy_session::LegacySession {
    parse_legacy_session(&jsonl(records), PathBuf::from("fixture.jsonl")).unwrap()
}

#[test]
fn selects_only_requested_branch_and_never_silently_falls_back() {
    let records = vec![
        header(3),
        user("root", None, "start"),
        assistant("answer", "root", "response"),
        user("branch-a", Some("answer"), "A"),
        user("branch-b", Some("answer"), "B"),
    ];
    let source = parsed(&records);
    assert_eq!(
        source
            .leaves()
            .iter()
            .map(|leaf| leaf.id.as_str())
            .collect::<Vec<_>>(),
        ["branch-a", "branch-b"]
    );
    let session_a = source
        .import(LeafSelection::Entry("branch-a".into()))
        .unwrap();
    let session_b = source.import(LeafSelection::Latest).unwrap();
    assert_eq!(
        session_a
            .context
            .messages
            .iter()
            .map(Message::text)
            .collect::<Vec<_>>(),
        ["start", "response", "A"]
    );
    assert_eq!(
        session_b
            .context
            .messages
            .iter()
            .map(Message::text)
            .collect::<Vec<_>>(),
        ["start", "response", "B"]
    );
    assert!(source
        .import(LeafSelection::Entry("missing".into()))
        .unwrap_err()
        .contains("not found"));
    let root = source.import(LeafSelection::Root).unwrap();
    assert!(root.context.messages.is_empty());
    assert_eq!(root.imported.unwrap().selected_leaf_id, None);
}

#[test]
fn latest_compaction_keeps_referenced_entries_and_settings_from_full_branch() {
    let mut records = vec![
        header(3),
        user("1", None, "summarized"),
        json!({"type":"thinking_level_change","id":"2","parentId":"1","timestamp":TIMESTAMP,"thinkingLevel":"high"}),
        assistant("3", "2", "old answer"),
        user("4", Some("3"), "kept"),
        assistant("5", "4", "kept answer"),
    ];
    records.push(json!({"type":"compaction","id":"6","parentId":"5","timestamp":TIMESTAMP,"summary":"old summary","firstKeptEntryId":"1","tokensBefore":1000}));
    records.push(user("7", Some("6"), "new kept"));
    records.push(json!({"type":"custom","id":"8","parentId":"7","customType":"state","data":{"secret":"metadata only"}}));
    records.push(json!({"type":"compaction","id":"9","parentId":"8","timestamp":TIMESTAMP,"summary":"latest summary","firstKeptEntryId":"7","tokensBefore":1200,"unknownCompaction":{"keep":true}}));
    records.push(user("10", Some("9"), "after"));
    let session = parsed(&records).import(LeafSelection::Latest).unwrap();
    assert_eq!(session.context.messages.len(), 3);
    assert!(session.context.messages[0]
        .text()
        .contains("latest summary"));
    assert!(!session.context.messages[0].text().contains("old summary"));
    assert_eq!(session.context.messages[1].text(), "new kept");
    assert_eq!(session.context.messages[2].text(), "after");
    assert_eq!(session.context.messages[0].text(), "The conversation history before this point was compacted into the following summary:\n\n<summary>\nlatest summary\n</summary>");
    let metadata = session.imported.unwrap();
    assert_eq!(metadata.context_entry_ids, ["9", "7", "8", "10"]);
    assert_eq!(metadata.thinking_level, "high");
    assert_eq!(metadata.model.unwrap().model_id, "fixture-model");
    assert_eq!(metadata.records, records);
}

#[test]
fn branch_summary_custom_and_bash_messages_use_observed_llm_projection() {
    let records = vec![
        header(3),
        user("1", None, "start"),
        json!({"type":"branch_summary","id":"2","parentId":"1","timestamp":TIMESTAMP,"fromId":"abandoned","summary":"try another route"}),
        json!({"type":"custom_message","id":"3","parentId":"2","timestamp":TIMESTAMP,"customType":"extension","content":"injected","display":false,"details":{"kept":true}}),
        json!({"type":"message","id":"4","parentId":"3","timestamp":TIMESTAMP,"message":{"role":"bashExecution","command":"echo hello","output":"hello","exitCode":0,"cancelled":false,"truncated":false,"timestamp":4}}),
        json!({"type":"message","id":"5","parentId":"4","timestamp":TIMESTAMP,"message":{"role":"bashExecution","command":"hidden","output":"not in context","excludeFromContext":true,"timestamp":5}}),
    ];
    let session = parsed(&records).import(LeafSelection::Latest).unwrap();
    assert_eq!(session.context.messages.len(), 4);
    assert_eq!(session.context.messages[1].text(), "The following is a summary of a branch that this conversation came back from:\n\n<summary>\ntry another route</summary>");
    assert_eq!(session.context.messages[2].text(), "injected");
    assert_eq!(
        session.context.messages[3].text(),
        "Ran `echo hello`\n```\nhello\n```"
    );
}

#[test]
fn old_versions_migrate_in_memory_without_rewriting_original_records() {
    let records = vec![
        header(1),
        json!({"type":"message","timestamp":TIMESTAMP,"message":{"role":"user","content":"old","timestamp":1}}),
        json!({"type":"message","timestamp":TIMESTAMP,"message":{"role":"hookMessage","customType":"hook","content":"hook context","timestamp":2}}),
        json!({"type":"compaction","timestamp":TIMESTAMP,"summary":"v1 summary","firstKeptEntryIndex":2,"tokensBefore":50}),
    ];
    let session = parsed(&records).import(LeafSelection::Latest).unwrap();
    assert_eq!(session.context.messages.len(), 2);
    assert_eq!(session.context.messages[1].text(), "hook context");
    let metadata = session.imported.unwrap();
    assert_eq!(metadata.context_entry_ids, ["legacy-3", "legacy-2"]);
    assert_eq!(metadata.records, records);
    let records = vec![
        header(2),
        json!({"type":"message","id":"hook","parentId":null,"timestamp":TIMESTAMP,"message":{"role":"hookMessage","content":"v2 hook","timestamp":1}}),
    ];
    assert_eq!(
        parsed(&records)
            .import(LeafSelection::Latest)
            .unwrap()
            .context
            .messages[0]
            .text(),
        "v2 hook"
    );
}

#[test]
fn preserves_unknown_records_name_cwd_and_native_save_roundtrip() {
    let dir = std::env::temp_dir().join(format!("legacy-import-{}", Session::default().id));
    fs::create_dir(&dir).unwrap();
    let source_path = dir.join("original.jsonl");
    let records = vec![
        header(3),
        user("1", None, "first"),
        json!({"type":"future_extension","id":"2","parentId":"1","payload":{"untouched":[1,2,3]}}),
        json!({"type":"session_info","id":"3","parentId":"2","name":" Named session "}),
    ];
    let original_bytes = jsonl(&records).into_bytes();
    fs::write(&source_path, &original_bytes).unwrap();
    let source = load_legacy_session(&source_path).unwrap();
    let session = source.import(LeafSelection::Latest).unwrap();
    assert_eq!(session.name, "Named session");
    assert_eq!(session.workspace, PathBuf::from("/original/project"));
    assert_eq!(session.context.messages.len(), 1);
    let store = SessionStore::new(dir.join("native"));
    store.save(&session).unwrap();
    let restored = store.list().unwrap().pop().unwrap();
    assert_eq!(restored.imported.unwrap().records, records);
    assert_eq!(fs::read(&source_path).unwrap(), original_bytes);
    fs::remove_file(source_path).unwrap();
    fs::remove_file(dir.join("native").join(format!("{}.json", session.id))).unwrap();
    fs::remove_dir(dir.join("native")).unwrap();
    fs::remove_dir(dir).unwrap();
}

#[test]
fn validates_tree_cycles_duplicates_missing_parent_and_explicit_workspace_override() {
    let source = parsed(&[
        header(3),
        user("a", Some("b"), "A"),
        user("b", Some("a"), "B"),
    ]);
    assert!(source
        .import(LeafSelection::Latest)
        .unwrap_err()
        .contains("Cycle"));
    let source = parsed(&[header(3), user("a", Some("missing"), "A")]);
    assert!(source
        .import(LeafSelection::Latest)
        .unwrap_err()
        .contains("Missing parent"));
    assert!(parse_legacy_session(
        &jsonl(&[header(3), user("a", None, "A"), user("a", None, "B")]),
        PathBuf::new()
    )
    .unwrap_err()
    .contains("Duplicate"));
    let source = parsed(&[header(3), user("a", None, "A")]);
    let override_path = PathBuf::from("explicit-project");
    assert_eq!(
        source
            .import_with_workspace(LeafSelection::Latest, Some(&override_path))
            .unwrap()
            .workspace,
        override_path
    );
    let mut missing_cwd = header(3);
    missing_cwd.as_object_mut().unwrap().remove("cwd");
    assert!(parsed(&[missing_cwd])
        .import(LeafSelection::Root)
        .unwrap_err()
        .contains("explicit workspace"));
}

#[test]
fn skips_malformed_lines_with_diagnostics_and_rejects_unknown_versions() {
    let content = format!(
        "garbage\n{}\n{{unfinished\n{}",
        header(3),
        user("1", None, "hello")
    );
    let source = parse_legacy_session(&content, PathBuf::new()).unwrap();
    assert_eq!(source.diagnostics.len(), 2);
    assert_eq!(
        source
            .import(LeafSelection::Latest)
            .unwrap()
            .context
            .messages[0]
            .text(),
        "hello"
    );
    assert!(parse_legacy_session(&header(4).to_string(), PathBuf::new())
        .unwrap_err()
        .contains("Unsupported"));
    assert!(
        parse_legacy_session(&user("1", None, "not header").to_string(), PathBuf::new()).is_err()
    );
}

#[test]
fn normalizes_missing_content_and_preserves_tool_result_usage() {
    let records = vec![
        header(3),
        json!({"type":"message","id":"1","parentId":null,"timestamp":TIMESTAMP,"message":{"role":"user","timestamp":1}}),
        json!({"type":"message","id":"2","parentId":"1","timestamp":TIMESTAMP,"message":{"role":"toolResult","toolCallId":"call-1","toolName":"read","content":[{"type":"text","text":"result"}],"isError":false,"usage":{"input":10,"output":20,"totalTokens":30},"timestamp":2}}),
    ];
    let session = parsed(&records).import(LeafSelection::Latest).unwrap();
    assert_eq!(session.context.messages[0].text(), "");
    match &session.context.messages[1] {
        Message::ToolResult(result) => assert_eq!(result.usage.as_ref().unwrap().total_tokens, 30),
        _ => panic!("expected tool result"),
    }
}

#[test]
fn utc_and_offset_summary_timestamps_match_epoch_milliseconds() {
    for timestamp in ["2025-01-01T00:00:00Z", "2025-01-01T08:00:00.000+08:00"] {
        let source = parsed(&[
            header(3),
            json!({"type":"compaction","id":"1","parentId":null,"timestamp":timestamp,"summary":"summary","firstKeptEntryId":"absent","tokensBefore":0}),
        ]);
        match &source
            .import(LeafSelection::Latest)
            .unwrap()
            .context
            .messages[0]
        {
            Message::User(user) => assert_eq!(user.timestamp, 1_735_689_600_000),
            _ => panic!("expected summary user message"),
        }
    }
}

#[test]
fn future_message_roles_remain_metadata_without_inventing_timestamp_or_content() {
    let records = vec![
        header(3),
        user("1", None, "hello"),
        json!({"type":"message","id":"2","parentId":"1","message":{"role":"future-message","arbitrary":{"kept":true}}}),
    ];
    let session = parsed(&records).import(LeafSelection::Latest).unwrap();
    assert_eq!(session.context.messages.len(), 1);
    let imported = session.imported.unwrap();
    assert_eq!(imported.records, records);
    assert!(imported
        .diagnostics
        .iter()
        .any(|diagnostic| diagnostic.contains("Unknown message role")));
}

fn replay_assistant(api: &str, model: &str, content: Value) -> Value {
    let mut entry = assistant("answer", "root", "");
    entry["message"]["api"] = json!(api);
    entry["message"]["provider"] = json!(match api {
        "anthropic-messages" => "anthropic",
        "google-generative-ai" => "google",
        "google-vertex" => "google-vertex",
        "openai-responses" => "openai",
        "azure-openai-responses" => "azure-openai-responses",
        "openai-codex-responses" => "openai-codex",
        _ => "fixture-provider",
    });
    entry["message"]["model"] = json!(model);
    entry["message"]["content"] = content;
    entry
}

fn persisted_import(records: &[Value]) -> Session {
    let directory = std::env::temp_dir().join(format!("legacy-replay-{}", Session::default().id));
    fs::create_dir(&directory).unwrap();
    let path = directory.join("original.jsonl");
    let original = jsonl(records).into_bytes();
    fs::write(&path, &original).unwrap();
    let imported = load_legacy_session(&path)
        .unwrap()
        .import(LeafSelection::Latest)
        .unwrap();
    let store = SessionStore::new(directory.join("native"));
    store.save(&imported).unwrap();
    let restored = store.list().unwrap().pop().unwrap();
    assert_eq!(restored.context.messages, imported.context.messages);
    assert_eq!(&restored.imported.as_ref().unwrap().records, records);
    assert_eq!(fs::read(&path).unwrap(), original);
    fs::remove_file(path).unwrap();
    fs::remove_file(
        directory
            .join("native")
            .join(format!("{}.json", imported.id)),
    )
    .unwrap();
    fs::remove_dir(directory.join("native")).unwrap();
    fs::remove_dir(directory).unwrap();
    restored
}

fn replay_metadata(session: &Session) -> &Value {
    match &session.context.messages[1] {
        Message::Assistant(message) => message.protocol_metadata.as_ref().unwrap(),
        _ => panic!("expected imported assistant"),
    }
}

#[test]
fn anthropic_signed_and_redacted_thinking_survive_import_native_save_and_payload_replay() {
    let records = vec![
        header(3),
        user("root", None, "question"),
        replay_assistant(
            "anthropic-messages",
            "claude-fixture",
            json!([
                {"type":"text","text":"before"},
                {"type":"thinking","thinking":"reasoning","thinkingSignature":"opaque-signed-payload","futureField":{"keep":true}},
                {"type":"thinking","thinking":"[Reasoning redacted]","redacted":true,"thinkingSignature":"opaque-redacted-payload"},
                {"type":"toolCall","id":"tool-1","name":"read","arguments":{"path":"a.txt"}},
            ]),
        ),
        json!({"type":"message","id":"result","parentId":"answer","timestamp":TIMESTAMP,"message":{"role":"toolResult","toolCallId":"tool-1","toolName":"read","content":[{"type":"text","text":"contents"}],"timestamp":3}}),
    ];
    let session = persisted_import(&records);
    assert_eq!(
        replay_metadata(&session),
        &json!({"protocol":"anthropic","model":"claude-fixture","blocks":{
            "1":{"kind":"thinking","signature":"opaque-signed-payload"},
            "2":{"kind":"redacted","signature":"opaque-redacted-payload"},
        }})
    );
    let payload = anthropic_request_payload(&session.context, "claude-fixture", &[]);
    assert_eq!(
        payload["messages"][1]["content"],
        json!([
            {"type":"text","text":"before"},
            {"type":"thinking","thinking":"reasoning","signature":"opaque-signed-payload"},
            {"type":"redacted_thinking","data":"opaque-redacted-payload"},
            {"type":"tool_use","id":"tool-1","name":"read","input":{"path":"a.txt"}},
        ])
    );
    assert_eq!(
        payload["messages"][2]["content"][0]["tool_use_id"],
        "tool-1"
    );
    let changed = anthropic_request_payload(&session.context, "other-model", &[]);
    assert!(changed["messages"][1]["content"]
        .as_array()
        .unwrap()
        .iter()
        .all(|block| block.get("signature").is_none() && block.get("data").is_none()));
}

#[test]
fn google_signatures_stay_attached_to_exact_thinking_text_and_tool_positions() {
    for api in ["google-generative-ai", "google-vertex"] {
        let records = vec![
            header(3),
            user("root", None, "question"),
            replay_assistant(
                api,
                "gemini-3-fixture",
                json!([
                    {"type":"thinking","thinking":"","thinkingSignature":"c2ln"},
                    {"type":"text","text":"answer","textSignature":"dGV4dA=="},
                    {"type":"toolCall","id":"call-1","name":"read","arguments":{"path":"a.txt"},"thoughtSignature":"dG9vbA=="},
                    {"type":"text","text":"","textSignature":"ZW1wdHk="},
                ]),
            ),
        ];
        let session = persisted_import(&records);
        assert_eq!(
            replay_metadata(&session),
            &json!({"protocol":"google","model":"gemini-3-fixture","signatures":{
                "0":"c2ln","1":"dGV4dA==","2":"dG9vbA==","3":"ZW1wdHk=",
            }})
        );
        let payload = google_request_payload(&session.context, "gemini-3-fixture", &[]);
        assert_eq!(
            payload["contents"][1]["parts"],
            json!([
                {"text":"","thought":true,"thoughtSignature":"c2ln"},
                {"text":"answer","thoughtSignature":"dGV4dA=="},
                {"functionCall":{"id":"call-1","name":"read","args":{"path":"a.txt"}},"thoughtSignature":"dG9vbA=="},
                {"text":"","thoughtSignature":"ZW1wdHk="},
            ])
        );
        let changed = google_request_payload(&session.context, "gemini-3-other", &[]);
        assert!(changed["contents"][1]["parts"]
            .as_array()
            .unwrap()
            .iter()
            .all(|part| part.get("thoughtSignature").is_none() && part.get("thought").is_none()));
        let foreign = anthropic_request_payload(&session.context, "gemini-3-fixture", &[]);
        assert!(foreign["messages"][1]["content"]
            .as_array()
            .unwrap()
            .iter()
            .all(|block| block.get("signature").is_none()));
    }
}

#[test]
fn responses_reasoning_phase_and_namespaced_tool_ids_survive_native_save_and_replay() {
    for api in [
        "openai-responses",
        "azure-openai-responses",
        "openai-codex-responses",
    ] {
        let reasoning = json!({"type":"reasoning","id":"rs_opaque","summary":[],"content":[{"type":"reasoning_text","text":"private"}],"encrypted_content":"encrypted-opaque","status":"completed","futureMetadata":{"keep":true}});
        let mut answer = replay_assistant(
            api,
            "gpt-fixture",
            json!([
                {"type":"thinking","thinking":"","thinkingSignature":reasoning.to_string()},
                {"type":"text","text":"working","textSignature":json!({"v":1,"id":"msg_comment","phase":"commentary"}).to_string()},
                {"type":"text","text":"legacy","textSignature":"msg_legacy"},
                {"type":"text","text":"","textSignature":json!({"v":1,"id":"msg_empty","phase":"final_answer"}).to_string()},
                {"type":"toolCall","id":"call_a|fc_native","name":"read","arguments":{"path":"a.txt"},"namespace":"filesystem"},
            ]),
        );
        answer["message"]["responseId"] = json!("resp_original");
        let records = vec![
            header(3),
            user("root", None, "question"),
            answer,
            json!({"type":"message","id":"result","parentId":"answer","timestamp":TIMESTAMP,"message":{"role":"toolResult","toolCallId":"call_a|fc_native","toolName":"read","content":[{"type":"text","text":"contents"}],"timestamp":3}}),
        ];
        let session = persisted_import(&records);
        let metadata = replay_metadata(&session);
        assert_eq!(metadata["protocol"], "openai-responses");
        assert_eq!(metadata["response_id"], "resp_original");
        assert_eq!(metadata["items"]["0"], reasoning);
        assert_eq!(metadata["items"]["1"]["phase"], "commentary");
        assert_eq!(metadata["items"]["2"]["id"], "msg_legacy");
        let payload = responses_request_payload(&session.context, "gpt-fixture", &[]);
        assert_eq!(payload["input"][1], reasoning);
        assert_eq!(
            payload["input"][2],
            json!({"type":"message","role":"assistant","id":"msg_comment","status":"completed","phase":"commentary","content":[{"type":"output_text","text":"working","annotations":[]}]})
        );
        assert_eq!(payload["input"][3]["id"], "msg_legacy");
        assert_eq!(payload["input"][4]["phase"], "final_answer");
        assert_eq!(payload["input"][4]["content"][0]["text"], "");
        assert_eq!(
            payload["input"][5],
            json!({"type":"function_call","call_id":"call_a","id":"fc_native","name":"read","arguments":"{\"path\":\"a.txt\"}","namespace":"filesystem"})
        );
        assert_eq!(payload["input"][6]["call_id"], "call_a");
        let changed = responses_request_payload(&session.context, "gpt-other", &[]);
        assert!(changed["input"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item["type"] != "reasoning"
                && item.get("phase").is_none()
                && item.get("namespace").is_none()
                && item.get("id").is_none()));
    }
}

#[test]
fn responses_long_legacy_text_id_uses_exact_typescript_utf16_hash() {
    let signature = "😀".repeat(40) + "legacy-id";
    let session = persisted_import(&[
        header(3),
        user("root", None, "question"),
        replay_assistant(
            "openai-responses",
            "gpt-fixture",
            json!([
                {"type":"text","text":"answer","textSignature":signature},
            ]),
        ),
    ]);
    // Obtained from the original packages/ai/src/utils/hash.ts with Node strip-types.
    assert_eq!(
        replay_metadata(&session)["items"]["0"]["id"],
        "msg_10bp1ktr5eqkc"
    );
    assert_eq!(
        responses_request_payload(&session.context, "gpt-fixture", &[])["input"][1]["id"],
        "msg_10bp1ktr5eqkc"
    );
}

#[test]
fn malformed_or_unreconstructable_replay_fields_produce_diagnostics_and_keep_originals() {
    for (api, model, content, expected) in [
        (
            "anthropic-messages",
            "claude-fixture",
            json!([{"type":"thinking","thinking":"[Reasoning redacted]","redacted":true}]),
            "lacks an opaque payload",
        ),
        (
            "anthropic-messages",
            "claude-fixture",
            json!([{"type":"thinking","thinking":"reasoning","thinkingSignature":42}]),
            "not a string",
        ),
        (
            "google-generative-ai",
            "gemini-fixture",
            json!([{"type":"text","text":"answer","textSignature":"not-base64?"}]),
            "not valid base64",
        ),
        (
            "openai-responses",
            "gpt-fixture",
            json!([{"type":"thinking","thinking":"reasoning","thinkingSignature":"{broken"}]),
            "not a complete Responses reasoning item",
        ),
        (
            "openai-responses",
            "gpt-fixture",
            json!([{"type":"thinking","thinking":"reasoning","thinkingSignature":json!({"type":"reasoning","id":"rs_missing_summary","encrypted_content":"opaque"}).to_string()}]),
            "not a complete Responses reasoning item",
        ),
        (
            "openai-responses",
            "gpt-fixture",
            json!([{"type":"text","text":"answer","textSignature":json!({"v":1,"id":""}).to_string()}]),
            "empty Responses text ID",
        ),
        (
            "openai-responses",
            "gpt-fixture",
            json!([{"type":"toolCall","id":"call|ctc_custom","name":"grammar","arguments":{"input":"original"},"namespace":"tools"}]),
            "original grammar-tool configuration",
        ),
        (
            "future-api",
            "future-model",
            json!([{"type":"thinking","thinking":"reasoning","thinkingSignature":"opaque"}]),
            "unknown or missing API",
        ),
        (
            "anthropic-messages",
            "",
            json!([{"type":"thinking","thinking":"reasoning","thinkingSignature":"opaque"}]),
            "missing model identity",
        ),
    ] {
        let records = [
            header(3),
            user("root", None, "question"),
            replay_assistant(api, model, content),
        ];
        let session = persisted_import(&records);
        assert!(
            session
                .imported
                .as_ref()
                .unwrap()
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.contains(expected)),
            "{api}: missing diagnostic {expected}"
        );
        assert_eq!(&session.imported.unwrap().records, &records);
    }
}

#[test]
fn malformed_v1_text_signature_keeps_observed_legacy_id_fallback_and_reports_it() {
    let session = persisted_import(&[
        header(3),
        user("root", None, "question"),
        replay_assistant(
            "openai-responses",
            "gpt-fixture",
            json!([
                {"type":"text","text":"answer","textSignature":"{not-v1"},
            ]),
        ),
    ]);
    assert_eq!(replay_metadata(&session)["items"]["0"]["id"], "{not-v1");
    assert!(session
        .imported
        .unwrap()
        .diagnostics
        .iter()
        .any(|diagnostic| diagnostic.contains("legacy plain-ID interpretation")));
}
