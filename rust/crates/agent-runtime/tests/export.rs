use agent_core::types::{AssistantMessage, Content, Message, ToolResultMessage, UserMessage};
use agent_runtime::html_export::{collect_archives, generate_session_html};
use agent_runtime::legacy_session::{load_legacy_session, parse_legacy_session, LeafSelection};
use agent_runtime::session::{Session, SessionStore};
use agent_runtime::session_tree::SessionEntry;
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::PathBuf;

const PNG: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+jXioAAAAASUVORK5CYII=";
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("export-fixture-{}", Session::default().id));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}
fn archive(id: &str, parent: Option<&str>, name: &str, bytes: &[u8]) -> Value {
    json!({"type":"custom","id":id,"parentId":parent,"customType":"export:context-archives","data":{"schema":"agent-session-archives/1","files":{name:STANDARD.encode(bytes)},"checksums":{name:format!("{:x}",Sha256::digest(bytes))},"sourceRoot":"/old/context"}})
}

#[test]
fn self_contained_html_includes_all_branches_text_images_thinking_tool_diff_and_unknown_metadata() {
    let mut session = Session {
        name: "离线会话".into(),
        ..Session::default()
    };
    let root = session
        .append_message(Message::User(UserMessage {
            content: vec![
                Content::text("中文 😀 <literal>"),
                Content::Image {
                    data: PNG.into(),
                    mime_type: "image/png".into(),
                },
            ],
            timestamp: 1,
        }))
        .unwrap();
    session
        .append_message(Message::Assistant(AssistantMessage {
            content: vec![
                Content::Thinking {
                    thinking: "private reasoning".into(),
                },
                Content::text("answer A"),
            ],
            timestamp: 2,
            ..AssistantMessage::text("")
        }))
        .unwrap();
    session
        .append_message(Message::ToolResult(ToolResultMessage {
            tool_call_id: "fixture".into(),
            tool_name: "edit".into(),
            content: vec![Content::text("tool output")],
            details: json!({"diff":"-old <tag>\n+new 中文","unknown":{"value":42}}),
            usage: None,
            added_tool_names: vec![],
            is_error: false,
            timestamp: 3,
        }))
        .unwrap();
    session.navigate(Some(&root)).unwrap();
    session.append_message(Message::user("sibling B")).unwrap();
    session
        .metadata
        .insert("future_meta".into(), json!({"opaque":"preserved"}));
    let html = generate_session_html(&session).unwrap();
    assert!(html.starts_with("<!DOCTYPE html>"));
    assert!(html.ends_with("</html>"));
    for text in [
        "中文 😀 &lt;literal&gt;",
        "answer A",
        "sibling B",
        "private reasoning",
        "tool output",
        "diff",
        "future_meta",
        "preserved",
    ] {
        assert!(html.contains(text), "missing {text}");
    }
    assert!(html.contains(&format!("src=\"data:image/png;base64,{PNG}\"")));
    assert!(html.contains("class=added>+new 中文"));
    assert!(html.contains("class=removed>-old &lt;tag&gt;"));
    assert!(!html.contains("<script"));
    assert!(!html.contains("<link"));
    assert!(!html.contains("src=\"http"));
}

#[test]
fn html_escapes_every_untrusted_value_and_never_activates_markdown_or_metadata() {
    let attack="</pre><script>alert(1)</script><img src=x onerror=\"boom\"> & ' \" [x](javascript:alert(1))";
    let mut session = Session {
        name: attack.into(),
        ..Session::default()
    };
    session.context.system_prompt = attack.into();
    session.append_message(Message::user(attack)).unwrap();
    session
        .metadata
        .insert("unknown".into(), json!({"payload":attack}));
    let html = generate_session_html(&session).unwrap();
    assert!(!html.contains("<script"));
    assert!(!html.contains("<img src=x"));
    assert!(!html.contains("onerror=\""));
    assert!(!html.contains("href=\"javascript:"));
    assert!(html.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
    assert!(html.contains("&amp;"));
    assert!(html.contains("&#39;"));
    assert!(html.contains("&quot;"));
}

#[test]
fn archive_bytes_hash_and_unknown_source_records_survive_import_native_save_and_html_export() {
    let fixture = Fixture::new();
    let bytes = b"offline evidence\0\xff\n";
    let records = [
        json!({"type":"session","version":3,"id":"original","cwd":"fixture","timestamp":"2025-01-01T00:00:00Z","future_header":{"keep":true}}),
        json!({"type":"message","id":"u","parentId":null,"message":{"role":"user","content":"source","timestamp":1}}),
        json!({"type":"future_extension","id":"future","parentId":"u","payload":{"arbitrary":[1,2,3]}}),
        archive("evidence", Some("future"), "files/evidence.bin", bytes),
    ];
    let source = fixture.0.join("original.jsonl");
    let original = records
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(&source, &original).unwrap();
    let before = Sha256::digest(original.as_bytes());
    let session = load_legacy_session(&source)
        .unwrap()
        .import(LeafSelection::Latest)
        .unwrap();
    let store = SessionStore::new(fixture.0.join("native"));
    store.save(&session).unwrap();
    let restored = store.list().unwrap().pop().unwrap();
    let html_path = fixture.0.join("session.html");
    SessionStore::export_html(&restored, &html_path).unwrap();
    let html = fs::read_to_string(html_path).unwrap();
    assert!(html.contains(&format!(
        "href=\"data:application/octet-stream;base64,{}\"",
        STANDARD.encode(bytes)
    )));
    assert!(html.contains("future_extension"));
    assert!(html.contains("future_header"));
    assert_eq!(restored.imported.as_ref().unwrap().records, records);
    assert_eq!(Sha256::digest(fs::read(source).unwrap()), before);
    let archives = collect_archives(&restored.get_entries().unwrap()).unwrap();
    assert_eq!(
        STANDARD.decode(&archives["files/evidence.bin"]).unwrap(),
        bytes
    );
}

#[test]
fn malformed_archive_or_image_fails_before_replacing_existing_export() {
    let fixture = Fixture::new();
    let path = fixture.0.join("kept.html");
    fs::write(&path, b"existing").unwrap();
    for mut entry in [
        archive("archive", None, "../escape", b"bytes"),
        archive("archive", None, "good", b"bytes"),
    ] {
        if entry["data"]["files"].get("good").is_some() {
            entry["data"]["checksums"]["good"] = json!("incorrect");
        }
        let session = Session {
            entries: vec![SessionEntry::from_value(entry).unwrap()],
            leaf_id: Some("archive".into()),
            tree_version: 1,
            ..Session::default()
        };
        assert!(SessionStore::export_html(&session, &path).is_err());
        assert_eq!(fs::read(&path).unwrap(), b"existing");
    }
    let mut session = Session::default();
    session
        .append_message(Message::User(UserMessage {
            content: vec![Content::Image {
                data: "invalid?".into(),
                mime_type: "image/png".into(),
            }],
            timestamp: 1,
        }))
        .unwrap();
    assert!(SessionStore::export_html(&session, &path).is_err());
    assert_eq!(fs::read(path).unwrap(), b"existing");
}

#[test]
fn duplicate_equal_archives_coalesce_and_conflicting_payloads_are_errors() {
    let first = SessionEntry::from_value(archive("a", None, "file", b"one")).unwrap();
    let duplicate = SessionEntry::from_value(archive("b", Some("a"), "file", b"one")).unwrap();
    let conflict = SessionEntry::from_value(archive("c", Some("b"), "file", b"two")).unwrap();
    assert_eq!(
        collect_archives(&[first.clone(), duplicate.clone()])
            .unwrap()
            .len(),
        1
    );
    assert!(collect_archives(&[first, duplicate, conflict])
        .unwrap_err()
        .contains("Conflicting"));
}

#[test]
fn resumable_jsonl_exports_selected_branch_without_sibling_and_retains_off_branch_evidence() {
    let fixture = Fixture::new();
    let records = [
        json!({"type":"session","version":3,"id":"source","cwd":"fixture"}),
        json!({"type":"message","id":"root","parentId":null,"message":{"role":"user","content":"root","timestamp":1}}),
        json!({"type":"message","id":"A","parentId":"root","message":{"role":"user","content":"A","timestamp":2}}),
        json!({"type":"message","id":"B","parentId":"root","message":{"role":"user","content":"B","timestamp":3}}),
        archive("archive", Some("B"), "file", b"portable"),
    ];
    let raw = records
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    let session = parse_legacy_session(&raw, PathBuf::from("original.jsonl"))
        .unwrap()
        .import(LeafSelection::Entry("A".into()))
        .unwrap();
    let path = fixture.0.join("branch.jsonl");
    SessionStore::export_branch_jsonl(&session, &path).unwrap();
    let exported = load_legacy_session(&path)
        .unwrap()
        .import(LeafSelection::Latest)
        .unwrap();
    assert_eq!(
        exported
            .context
            .messages
            .iter()
            .map(Message::text)
            .collect::<Vec<_>>(),
        ["root", "A"]
    );
    assert_eq!(exported.get_entries().unwrap().len(), 3);
    assert_eq!(
        collect_archives(&exported.get_entries().unwrap()).unwrap()["file"],
        STANDARD.encode(b"portable")
    );
}

#[test]
fn native_message_protocol_metadata_images_and_compaction_projection_survive_branch_export() {
    let fixture = Fixture::new();
    let mut session = Session::default();
    session
        .append_message(Message::user("native user"))
        .unwrap();
    session
        .append_message(Message::Assistant(AssistantMessage {
            protocol_metadata: Some(json!({"protocol":"opaque","unknown":"kept"})),
            ..AssistantMessage::text("native answer")
        }))
        .unwrap();
    session.append_message(Message::user("keep")).unwrap();
    let record = agent_runtime::compaction::NativeCompaction {
        id: "compaction".into(),
        timestamp: 5,
        summary: "summary".into(),
        archived_messages: session.context.messages[..2].to_vec(),
        kept_message_count: 1,
        usage: None,
    };
    let active=vec![Message::User(UserMessage{content:vec![Content::text("The conversation history before this point was compacted into the following summary:\n\n<summary>\nsummary\n</summary>")],timestamp:5}),session.context.messages[2].clone()];
    session.record_compaction(record, active).unwrap();
    let path = fixture.0.join("native.jsonl");
    SessionStore::export_branch_jsonl(&session, &path).unwrap();
    let imported = load_legacy_session(&path)
        .unwrap()
        .import(LeafSelection::Latest)
        .unwrap();
    assert_eq!(imported.context, session.context);
    assert_eq!(
        imported.get_entries().unwrap(),
        session.get_entries().unwrap()
    );
    assert!(generate_session_html(&imported).unwrap().contains("opaque"));
}

#[test]
fn html_token_statistics_saturate_instead_of_panicking() {
    let mut session = Session::default();
    for _ in 0..2 {
        session
            .append_message(Message::Assistant(AssistantMessage {
                usage: Some(agent_core::types::Usage {
                    total_tokens: u64::MAX,
                    ..Default::default()
                }),
                ..AssistantMessage::text("fixture")
            }))
            .unwrap();
    }
    assert!(generate_session_html(&session)
        .unwrap()
        .contains(&format!("Total tokens {}", u64::MAX)));
}
