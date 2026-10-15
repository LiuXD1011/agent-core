use agent_core::types::{
    AssistantMessage, Content, Message, StopReason, ToolCall, ToolResultMessage, UserMessage,
};
use agent_runtime::compaction::NativeCompaction;
use agent_runtime::legacy_session::{parse_legacy_session, LeafSelection};
use agent_runtime::session::{Session, SessionStore};
use agent_runtime::session_tree::SessionEntry;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::PathBuf;

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("tree-fixture-{}", Session::default().id));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}
fn user(text: &str, time: u64) -> Message {
    Message::User(UserMessage {
        content: vec![Content::text(text)],
        timestamp: time,
    })
}
fn answer(text: &str, time: u64) -> Message {
    Message::Assistant(AssistantMessage {
        timestamp: time,
        ..AssistantMessage::text(text)
    })
}
fn calls(ids: &[&str]) -> Message {
    Message::Assistant(AssistantMessage {
        content: ids
            .iter()
            .map(|id| {
                Content::ToolCall(ToolCall {
                    id: (*id).into(),
                    name: "read".into(),
                    arguments: json!({"path":"fixture"}),
                })
            })
            .collect(),
        stop_reason: StopReason::ToolUse,
        error_message: None,
        usage: None,
        timestamp: 3,
        protocol_metadata: None,
    })
}
fn result(id: &str) -> Message {
    Message::ToolResult(ToolResultMessage {
        tool_call_id: id.into(),
        tool_name: "read".into(),
        content: vec![Content::text("result")],
        details: json!({"diff":"-old\n+new"}),
        usage: None,
        added_tool_names: vec![],
        is_error: false,
        timestamp: 4,
    })
}
fn hash(session: &Session) -> String {
    format!("{:x}", Sha256::digest(serde_json::to_vec(session).unwrap()))
}
fn checkpoint(session: &mut Session, cut: usize, summary: &str, time: u64) -> String {
    let record = NativeCompaction {
        id: format!("compact-{time}"),
        timestamp: time,
        summary: summary.into(),
        archived_messages: session.context.messages[..cut].to_vec(),
        kept_message_count: session.context.messages.len() - cut,
        usage: None,
    };
    let mut active=vec![user(&format!("The conversation history before this point was compacted into the following summary:\n\n<summary>\n{summary}\n</summary>"),time)];
    active.extend_from_slice(&session.context.messages[cut..]);
    session.record_compaction(record, active).unwrap()
}

#[test]
fn navigation_never_isolates_tool_calls_and_failed_targets_are_atomic() {
    let mut session = Session::default();
    session.append_message(user("start", 1)).unwrap();
    let call = session.append_message(calls(&["a", "b"])).unwrap();
    let partial = session.append_message(result("b")).unwrap();
    let complete = session.append_message(result("a")).unwrap();
    session.append_message(answer("done", 5)).unwrap();
    let original = hash(&session);
    for target in [&call, &partial, &"missing".to_owned()] {
        assert!(session.navigate(Some(target)).is_err());
        assert_eq!(hash(&session), original);
    }
    session.navigate(Some(&complete)).unwrap();
    assert_eq!(session.context.messages.len(), 4);
    session.navigate(None).unwrap();
    assert!(session.context.messages.is_empty());
    assert_eq!(session.get_entries().unwrap().len(), 5);
}

#[test]
fn fork_positions_before_user_and_preserves_the_abandoned_branch() {
    let mut session = Session::default();
    let first = session.append_message(user("first", 1)).unwrap();
    let response = session.append_message(answer("response", 2)).unwrap();
    let second = session.append_message(user("second", 3)).unwrap();
    session.append_message(answer("old", 4)).unwrap();
    assert_eq!(session.fork(&second).unwrap(), "second");
    assert_eq!(session.get_leaf_id().unwrap(), Some(response.clone()));
    assert_eq!(session.context.messages.len(), 2);
    let replacement = session.append_message(user("edited", 5)).unwrap();
    assert_eq!(session.entries.last().unwrap().parent_id, Some(response));
    assert_eq!(session.get_tree().unwrap()[0].children[0].children.len(), 2);
    assert!(session.fork(&replacement).is_ok());
    assert!(session.fork("missing").is_err());
    assert_eq!(session.fork(&first).unwrap(), "first");
    assert!(session.get_leaf_id().unwrap().is_none());
    assert!(session.context.messages.is_empty());
}

#[test]
fn clone_extracts_current_leaf_not_the_latest_written_sibling() {
    let mut session = Session::default();
    let root = session.append_message(user("root", 1)).unwrap();
    let branch_a = session.append_message(answer("A", 2)).unwrap();
    session.navigate(Some(&root)).unwrap();
    session.append_message(answer("B", 3)).unwrap();
    session.navigate(Some(&branch_a)).unwrap();
    session
        .metadata
        .insert("unknown".into(), json!({"preserved":true}));
    let original = hash(&session);
    let cloned = session.clone_current_branch().unwrap();
    assert_ne!(cloned.id, session.id);
    assert_eq!(cloned.context.messages[1].text(), "A");
    assert_eq!(cloned.get_entries().unwrap().len(), 2);
    assert_eq!(cloned.leaf_id, Some(branch_a));
    assert_eq!(cloned.metadata["unknown"], json!({"preserved":true}));
    assert_eq!(hash(&session), original);
}

#[test]
fn native_checkpoints_keep_full_pairs_and_branch_context_across_save_reload() {
    let fixture = Fixture::new();
    let store = SessionStore::new(&fixture.0);
    let mut session = Session::default();
    session.append_message(user("old", 1)).unwrap();
    session.append_message(calls(&["call"])).unwrap();
    session.append_message(result("call")).unwrap();
    let before = session.append_message(answer("old done", 5)).unwrap();
    session.append_message(user("kept", 6)).unwrap();
    session.append_message(answer("kept done", 7)).unwrap();
    let compact = checkpoint(&mut session, 4, "summary", 8);
    session.append_message(user("after", 9)).unwrap();
    let after = session.leaf_id.clone();
    session.navigate(Some(&before)).unwrap();
    assert_eq!(session.context.messages[0].text(), "old");
    assert_eq!(session.context.messages.len(), 4);
    session.navigate(Some(&compact)).unwrap();
    assert_eq!(session.context.messages.len(), 3);
    assert_eq!(session.context.messages[1].text(), "kept");
    session.navigate(after.as_deref()).unwrap();
    store.save(&session).unwrap();
    let mut loaded = store.list().unwrap().pop().unwrap();
    assert_eq!(loaded.entries, session.entries);
    assert_eq!(loaded.leaf_id, session.leaf_id);
    assert_eq!(loaded.compactions, session.compactions);
    assert_eq!(loaded.context, session.context);
    loaded.navigate(Some(&before)).unwrap();
    assert_eq!(loaded.context.messages[2], result("call"));
}

#[test]
fn invalid_compaction_cuts_or_context_replacements_leave_history_unchanged() {
    let mut session = Session::default();
    session.append_message(user("old", 1)).unwrap();
    session.append_message(calls(&["a"])).unwrap();
    session.append_message(result("a")).unwrap();
    session.append_message(user("keep", 5)).unwrap();
    let original = hash(&session);
    let record = NativeCompaction {
        id: "bad".into(),
        timestamp: 6,
        summary: "bad".into(),
        archived_messages: session.context.messages[..2].to_vec(),
        kept_message_count: 2,
        usage: None,
    };
    let active = vec![user("wrong summary", 6), result("a"), user("keep", 5)];
    assert!(session.record_compaction(record, active).is_err());
    assert_eq!(hash(&session), original);
    let mut invalid = session.clone();
    invalid.context.messages = vec![user("rewritten", 9)];
    assert!(invalid.ensure_tree().unwrap_err().contains("diverges"));
}

#[test]
fn imported_all_branches_unknown_entries_labels_and_settings_remain_navigable() {
    let records = [
        json!({"type":"session","id":"source","version":3,"timestamp":"2025-01-01T00:00:00Z","cwd":"fixture"}),
        json!({"type":"message","id":"root","parentId":null,"message":{"role":"user","content":"root","timestamp":1}}),
        json!({"type":"thinking_level_change","id":"setting","parentId":"root","thinkingLevel":"high"}),
        json!({"type":"message","id":"A","parentId":"setting","message":{"role":"user","content":"A","timestamp":2}}),
        json!({"type":"future","id":"future","parentId":"A","unknown":{"preserved":[1,2,3]}}),
        json!({"type":"message","id":"B","parentId":"root","message":{"role":"user","content":"B","timestamp":3}}),
        json!({"type":"label","id":"bookmark","parentId":"B","targetId":"A","label":"marked","timestamp":"2025-01-02T00:00:00Z"}),
    ];
    let text = records
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    let source = parse_legacy_session(&text, PathBuf::from("source.jsonl")).unwrap();
    let mut session = source.import(LeafSelection::Latest).unwrap();
    assert_eq!(session.get_entries().unwrap().len(), 6);
    session.navigate(Some("future")).unwrap();
    assert_eq!(session.context.messages[1].text(), "A");
    assert_eq!(session.imported.as_ref().unwrap().thinking_level, "high");
    assert_eq!(session.imported.as_ref().unwrap().records, records);
    assert_eq!(
        session.get_tree().unwrap()[0].children[0].children[0]
            .label
            .as_deref(),
        Some("marked")
    );
    session.navigate(Some("B")).unwrap();
    assert_eq!(session.imported.as_ref().unwrap().thinking_level, "off");
}

#[test]
fn old_flat_native_and_imported_snapshots_upgrade_without_erasing_unknown_payloads() {
    let mut original = Session::default();
    original.context.messages = vec![user("old flat", 1), answer("answer", 2)];
    let mut raw = serde_json::to_value(&original).unwrap();
    let object = raw.as_object_mut().unwrap();
    for field in [
        "entries",
        "leaf_id",
        "tree_version",
        "imported",
        "compactions",
    ] {
        object.remove(field);
    }
    raw["futureTop"] = json!({"kept":true});
    raw["context"]["messages"][1]["futureProvider"] = json!({"signature":"opaque"});
    let mut loaded: Session = serde_json::from_value(raw).unwrap();
    loaded.ensure_tree().unwrap();
    assert_eq!(loaded.entries.len(), 2);
    assert_eq!(loaded.metadata["futureTop"], json!({"kept":true}));
    assert_eq!(
        loaded.metadata["preserved_context"]["messages"][1]["futureProvider"]["signature"],
        "opaque"
    );
    let entries = loaded.entries.clone();
    loaded.ensure_tree().unwrap();
    assert_eq!(loaded.entries, entries);
    let first_ids = original.get_entries().unwrap();
    assert_eq!(first_ids, original.get_entries().unwrap());
    assert_eq!(
        original.get_leaf_id().unwrap(),
        first_ids.last().map(|entry| entry.id.clone())
    );
}

#[test]
fn flat_compaction_archives_upgrade_to_complete_ordered_history() {
    let mut session = Session::default();
    session.context.messages = vec![
        user("one", 1),
        answer("one answer", 2),
        user("two", 3),
        answer("two answer", 4),
        user("three", 5),
    ];
    checkpoint(&mut session, 2, "first", 6);
    checkpoint(&mut session, 3, "second", 7);
    let expected = session.context.messages.clone();
    let archived = session.compactions.clone();
    session.entries.clear();
    session.leaf_id = None;
    session.tree_version = 0;
    session.ensure_tree().unwrap();
    assert_eq!(session.context.messages, expected);
    assert_eq!(session.compactions, archived);
    assert_eq!(
        session
            .entries
            .iter()
            .filter(|entry| entry.entry_type == "message")
            .count(),
        5
    );
    assert_eq!(
        session
            .entries
            .iter()
            .filter(|entry| entry.entry_type == "compaction")
            .count(),
        2
    );
    assert!(session.get_tree().is_ok());
}

#[test]
fn invalid_ids_cycles_missing_parents_and_off_branch_corruption_are_errors() {
    for entries in [
        vec![json!({"id":"a","type":"future","parentId":"missing"})],
        vec![json!({"id":"a","type":"future","parentId":"a"})],
        vec![
            json!({"id":"a","type":"future","parentId":null}),
            json!({"id":"a","type":"future","parentId":null}),
        ],
        vec![
            json!({"id":"good","type":"future","parentId":null}),
            json!({"id":"a","type":"future","parentId":"b"}),
            json!({"id":"b","type":"future","parentId":"a"}),
        ],
    ] {
        let session = Session {
            entries: entries
                .into_iter()
                .map(|entry| SessionEntry::from_value(entry).unwrap())
                .collect(),
            tree_version: 1,
            ..Session::default()
        };
        assert!(session.get_tree().is_err());
        assert!(session.get_entries().is_err());
    }
    let session = Session {
        leaf_id: Some("missing".into()),
        tree_version: 1,
        ..Session::default()
    };
    assert!(session.get_entries().is_err());
}

#[test]
fn prefix_writes_from_previous_native_clients_append_without_duplicate_entries() {
    let mut session = Session::default();
    session.append_message(user("one", 1)).unwrap();
    session.context.messages.push(answer("two", 2));
    session.ensure_tree().unwrap();
    assert_eq!(session.entries.len(), 2);
    session.ensure_tree().unwrap();
    assert_eq!(session.entries.len(), 2);
}

#[test]
fn deep_linear_history_is_traversed_iteratively() {
    let mut session = Session::default();
    session.context.messages = (0..1200).map(|index| user("fixture", index)).collect();
    session.ensure_tree().unwrap();
    assert_eq!(session.get_branch(None).unwrap().len(), 1200);
    assert_eq!(session.get_tree().unwrap().len(), 1);
}

#[test]
fn runtime_state_tracks_branch_ancestry_without_adding_model_messages() {
    let fixture = Fixture::new();
    let store = SessionStore::new(&fixture.0);
    let mut session = Session::default();
    let root = session.append_message(user("root", 1)).unwrap();
    let first = session
        .record_runtime_state("automation", json!({"budget":10}))
        .unwrap();
    session.append_message(answer("first branch", 2)).unwrap();
    let second = session
        .record_runtime_state("automation", json!({"budget":20}))
        .unwrap();
    assert_eq!(session.context.messages.len(), 2);
    assert_eq!(
        session.runtime_state("automation").unwrap(),
        Some(json!({"budget":20}))
    );
    session.navigate(Some(&first)).unwrap();
    assert_eq!(
        session.runtime_state("automation").unwrap(),
        Some(json!({"budget":10}))
    );
    assert_eq!(session.context.messages.len(), 1);
    session.navigate(Some(&root)).unwrap();
    assert!(session.runtime_state("automation").unwrap().is_none());
    session.append_message(answer("new branch", 3)).unwrap();
    session
        .record_runtime_state("automation", json!({"budget":5}))
        .unwrap();
    store.save(&session).unwrap();
    let mut loaded = store.list().unwrap().pop().unwrap();
    assert_eq!(
        loaded.runtime_state("automation").unwrap(),
        Some(json!({"budget":5}))
    );
    loaded.navigate(Some(&second)).unwrap();
    assert_eq!(
        loaded.runtime_state("automation").unwrap(),
        Some(json!({"budget":20}))
    );
    assert!(loaded.record_runtime_state(" ", Value::Null).is_err());
    assert!(loaded.runtime_state("").is_err());
}

#[test]
fn cloned_branch_rechains_through_label_entries_and_keeps_resolved_global_bookmarks() {
    let records = [
        json!({"type":"session","version":3,"id":"source","cwd":"fixture"}),
        json!({"type":"message","id":"root","parentId":null,"message":{"role":"user","content":"root","timestamp":1}}),
        json!({"type":"label","id":"label","parentId":"root","targetId":"root","label":"initial"}),
        json!({"type":"message","id":"A","parentId":"label","message":{"role":"user","content":"A","timestamp":2}}),
        json!({"type":"message","id":"B","parentId":"root","message":{"role":"user","content":"B","timestamp":3}}),
        json!({"type":"label","id":"offbranch","parentId":"B","targetId":"A","label":"bookmarked A","futureLabel":"kept"}),
        json!({"type":"label","id":"clear","parentId":"offbranch","targetId":"root","label":""}),
    ];
    let raw = records
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    let session = parse_legacy_session(&raw, PathBuf::new())
        .unwrap()
        .import(LeafSelection::Entry("A".into()))
        .unwrap();
    let clone = session.clone_current_branch().unwrap();
    assert_eq!(clone.context.messages, session.context.messages);
    assert_eq!(clone.entries[1].parent_id.as_deref(), Some("root"));
    assert_eq!(clone.entries.len(), 3);
    assert_eq!(clone.entries[2].field("futureLabel"), "kept");
    let tree = clone.get_tree().unwrap();
    assert!(tree[0].label.is_none());
    assert_eq!(tree[0].children[0].label.as_deref(), Some("bookmarked A"));
}

#[test]
fn tree_children_sort_numeric_and_iso_timestamps_chronologically() {
    let entries = [
        json!({"id":"root","parentId":null,"type":"future","timestamp":0}),
        json!({"id":"ten","parentId":"root","type":"future","timestamp":10}),
        json!({"id":"two","parentId":"root","type":"future","timestamp":2}),
        json!({"id":"iso","parentId":"root","type":"future","timestamp":"1970-01-01T00:00:00.003Z"}),
    ];
    let session = Session {
        entries: entries
            .into_iter()
            .map(|entry| SessionEntry::from_value(entry).unwrap())
            .collect(),
        tree_version: 1,
        ..Session::default()
    };
    assert_eq!(
        session.get_tree().unwrap()[0]
            .children
            .iter()
            .map(|child| child.entry.id.as_str())
            .collect::<Vec<_>>(),
        ["two", "iso", "ten"]
    );
}

#[test]
fn old_imported_flat_snapshot_restores_sibling_history_and_appended_native_tail() {
    let records = [
        json!({"type":"session","version":3,"id":"source","cwd":"fixture"}),
        json!({"type":"message","id":"root","parentId":null,"message":{"role":"user","content":"root","timestamp":1}}),
        json!({"type":"message","id":"A","parentId":"root","message":{"role":"user","content":"A","timestamp":2}}),
        json!({"type":"message","id":"B","parentId":"root","message":{"role":"user","content":"B","timestamp":3}}),
    ];
    let raw = records
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    let mut original = parse_legacy_session(&raw, PathBuf::new())
        .unwrap()
        .import(LeafSelection::Entry("A".into()))
        .unwrap();
    original.entries.clear();
    original.leaf_id = None;
    original.tree_version = 0;
    original.context.messages.push(answer("native tail", 4));
    original.ensure_tree().unwrap();
    assert_eq!(original.entries.len(), 4);
    assert_eq!(
        original.context.messages.last().unwrap().text(),
        "native tail"
    );
    assert_eq!(
        original.entries.last().unwrap().parent_id.as_deref(),
        Some("A")
    );
    original.navigate(Some("B")).unwrap();
    assert_eq!(original.context.messages.last().unwrap().text(), "B");
    assert_eq!(original.imported.as_ref().unwrap().records, records);
}

#[test]
fn old_imported_flat_snapshot_with_native_archives_keeps_every_original_branch() {
    let records = [
        json!({"type":"session","version":3,"id":"source","cwd":"fixture"}),
        json!({"type":"message","id":"root","parentId":null,"message":{"role":"user","content":"old","timestamp":1}}),
        json!({"type":"message","id":"answer","parentId":"root","message":{"role":"assistant","content":[{"type":"text","text":"old answer"}],"stopReason":"stop","timestamp":2}}),
        json!({"type":"message","id":"keep","parentId":"answer","message":{"role":"user","content":"keep","timestamp":3}}),
        json!({"type":"message","id":"sibling","parentId":"answer","message":{"role":"user","content":"sibling","timestamp":4}}),
    ];
    let raw = records
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    let mut session = parse_legacy_session(&raw, PathBuf::new())
        .unwrap()
        .import(LeafSelection::Entry("keep".into()))
        .unwrap();
    let old_metadata = session.imported.clone();
    session.append_message(answer("native kept", 5)).unwrap();
    checkpoint(&mut session, 2, "summary", 6);
    let active = session.context.messages.clone();
    session.entries.clear();
    session.leaf_id = None;
    session.tree_version = 0;
    session.imported = old_metadata;
    let deterministic = session.get_entries().unwrap();
    assert_eq!(deterministic, session.get_entries().unwrap());
    session.ensure_tree().unwrap();
    assert_eq!(session.context.messages, active);
    assert_eq!(session.entries.len(), 6);
    assert_eq!(session.compactions.len(), 1);
    session.navigate(Some("sibling")).unwrap();
    assert_eq!(session.context.messages.last().unwrap().text(), "sibling");
    assert_eq!(session.imported.as_ref().unwrap().records, records);
}
