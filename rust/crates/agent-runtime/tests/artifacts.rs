use agent_core::types::{
    AssistantMessage, CancellationToken, Content, Message, StopReason, ToolCall, ToolResultMessage,
};
use agent_runtime::{
    artifacts::{read_recorded_artifact, ArtifactError, MAX_PREVIEW_BYTES},
    session::Session,
};
use serde_json::json;
use std::path::PathBuf;

struct Fixture {
    root: PathBuf,
    session: Session,
}
impl Fixture {
    fn new() -> Self {
        let mut random = [0u8; 16];
        getrandom::fill(&mut random).unwrap();
        let root =
            std::env::temp_dir().join(format!("agent-artifact-{:x}", u128::from_le_bytes(random)));
        std::fs::create_dir_all(&root).unwrap();
        let session = Session {
            workspace: root.clone(),
            ..Session::default()
        };
        Self { root, session }
    }
    fn record(&mut self, path: &str, error: bool) -> String {
        let id = format!("call-{}", self.session.entries.len());
        self.session
            .append_message(Message::user("fixture"))
            .unwrap();
        self.session
            .append_message(Message::Assistant(AssistantMessage {
                content: vec![Content::ToolCall(ToolCall {
                    id: id.clone(),
                    name: "write".into(),
                    arguments: json!({"path":path}),
                })],
                stop_reason: StopReason::ToolUse,
                ..AssistantMessage::text("")
            }))
            .unwrap();
        self.session
            .append_message(Message::ToolResult(ToolResultMessage {
                tool_call_id: id,
                tool_name: "write".into(),
                content: vec![],
                details: json!({}),
                usage: None,
                added_tool_names: vec![],
                is_error: error,
                timestamp: 1,
            }))
            .unwrap()
    }
    fn read(
        &self,
        entry: &str,
    ) -> Result<agent_runtime::artifacts::ArtifactPreview, ArtifactError> {
        read_recorded_artifact(&self.session, entry, &CancellationToken::new())
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[test]
fn actual_unicode_disk_bytes_hash_and_updates_not_historical_content() {
    use sha2::{Digest, Sha256};
    let mut f = Fixture::new();
    let id = f.record("中文.md", false);
    std::fs::write(f.root.join("中文.md"), "真实内容\n").unwrap();
    let before = serde_json::to_value(&f.session).unwrap();
    let read = f.read(&id).unwrap();
    assert_eq!(read.text, "真实内容\n");
    assert_eq!(read.byte_len, 13);
    assert_eq!(
        read.sha256,
        format!("{:x}", Sha256::digest(read.text.as_bytes()))
    );
    assert_eq!(read.entry_id, id);
    assert_eq!(read.path, "中文.md");
    std::fs::write(f.root.join("中文.md"), "changed").unwrap();
    assert_eq!(f.read(&id).unwrap().text, "changed");
    assert_eq!(serde_json::to_value(&f.session).unwrap(), before);
    std::fs::remove_file(f.root.join("中文.md")).unwrap();
    assert_eq!(f.read(&id).err(), Some(ArtifactError::Missing));
}

#[test]
fn invalid_failed_and_foreign_branch_records_do_not_read() {
    let mut f = Fixture::new();
    std::fs::write(f.root.join("x"), "data").unwrap();
    let failed = f.record("x", true);
    assert_eq!(f.read(&failed).err(), Some(ArtifactError::InvalidRecord));
    assert_eq!(f.read("x").err(), Some(ArtifactError::InvalidRecord));
    let base = f.session.leaf_id.clone();
    let left = f.record("x", false);
    assert!(f.read(&left).is_ok());
    f.session.navigate(base.as_deref()).unwrap();
    f.record("x", false);
    assert_eq!(f.read(&left).err(), Some(ArtifactError::InvalidRecord));
}

#[test]
fn directory_binary_large_and_cancel_are_explicit() {
    let mut f = Fixture::new();
    std::fs::create_dir(f.root.join("dir")).unwrap();
    std::fs::write(f.root.join("bin"), [0, 1, 2, 255]).unwrap();
    std::fs::write(f.root.join("control"), b"a\0b").unwrap();
    std::fs::write(
        f.root.join("large"),
        vec![b'x'; MAX_PREVIEW_BYTES as usize + 1],
    )
    .unwrap();
    for (path, expected) in [
        ("dir", ArtifactError::Directory),
        ("bin", ArtifactError::Unsupported),
        ("control", ArtifactError::Unsupported),
        ("large", ArtifactError::TooLarge),
    ] {
        let id = f.record(path, false);
        assert_eq!(f.read(&id).err(), Some(expected));
    }
    let signal = CancellationToken::new();
    signal.cancel();
    assert_eq!(
        read_recorded_artifact(&f.session, "anything", &signal).err(),
        Some(ArtifactError::Cancelled)
    );
}

#[test]
fn outside_device_ads_and_parent_paths_are_rejected() {
    let mut f = Fixture::new();
    for path in [
        "../outside",
        "C:\\outside",
        "\\\\server\\share\\file",
        "\\\\?\\C:\\secret",
        "x:stream",
        "CON.txt",
        "LPT1",
        "x.",
    ] {
        let id = f.record(path, false);
        assert_eq!(
            f.read(&id).err(),
            Some(ArtifactError::OutsideWorkspace),
            "{path}"
        );
    }
}

#[test]
fn old_record_remains_readable_after_compaction_and_serialization() {
    use agent_core::types::UserMessage;
    use agent_runtime::compaction::NativeCompaction;
    let mut f = Fixture::new();
    std::fs::write(f.root.join("old"), "still on disk").unwrap();
    let id = f.record("old", false);
    f.session.record_compaction(NativeCompaction { id:"c".into(), timestamp:10, summary:"fixture".into(), archived_messages:f.session.context.messages.clone(), kept_message_count:0, usage:None }, vec![Message::User(UserMessage { timestamp:10, content:vec![Content::text("The conversation history before this point was compacted into the following summary:\n\n<summary>\nfixture\n</summary>")] })]).unwrap();
    f.session = serde_json::from_value(serde_json::to_value(&f.session).unwrap()).unwrap();
    assert_eq!(f.read(&id).unwrap().text, "still on disk");
}

#[cfg(windows)]
#[test]
fn canonical_workspace_and_windows_write_sharing_are_checked() {
    use std::os::windows::fs::OpenOptionsExt;
    let mut f = Fixture::new();
    std::fs::write(f.root.join("x"), "data").unwrap();
    f.session.workspace = f.root.canonicalize().unwrap();
    let id = f.record("x", false);
    assert!(f.read(&id).is_ok());
    let ordinary_absolute = f.root.join("x").to_string_lossy().into_owned();
    let absolute_id = f.record(&ordinary_absolute.to_ascii_uppercase(), false);
    assert!(f.read(&absolute_id).is_ok());
    let canonical_absolute = f.root.join("x").canonicalize().unwrap();
    f.session.workspace = f.root.clone();
    let canonical_id = f.record(&canonical_absolute.to_string_lossy(), false);
    assert!(f.read(&canonical_id).is_ok());
    let writer = std::fs::OpenOptions::new()
        .write(true)
        .share_mode(7)
        .open(f.root.join("x"))
        .unwrap();
    assert_eq!(f.read(&id).err(), Some(ArtifactError::Changed));
    drop(writer);
    assert!(f.read(&id).is_ok());
}

#[test]
fn exact_limit_empty_and_renamed_files_are_observed() {
    let mut f = Fixture::new();
    std::fs::write(
        f.root.join("bounded"),
        vec![b'x'; MAX_PREVIEW_BYTES as usize],
    )
    .unwrap();
    let id = f.record("bounded", false);
    assert_eq!(f.read(&id).unwrap().byte_len, MAX_PREVIEW_BYTES);
    std::fs::write(f.root.join("bounded"), []).unwrap();
    assert!(f.read(&id).unwrap().text.is_empty());
    std::fs::rename(f.root.join("bounded"), f.root.join("moved")).unwrap();
    assert_eq!(f.read(&id).err(), Some(ArtifactError::Missing));
}

#[cfg(windows)]
#[test]
fn junction_to_external_directory_is_never_followed() {
    let mut f = Fixture::new();
    let outside = Fixture::new();
    std::fs::write(outside.root.join("secret"), "outside text").unwrap();
    let junction = f.root.join("linked");
    let status = std::process::Command::new("cmd.exe")
        .args(["/c", "mklink", "/J"])
        .arg(&junction)
        .arg(&outside.root)
        .status()
        .unwrap();
    assert!(status.success());
    let id = f.record("linked/secret", false);
    let result = f.read(&id).err();
    std::fs::remove_dir(&junction).unwrap();
    assert_eq!(result, Some(ArtifactError::Link));
    assert_eq!(
        std::fs::read_to_string(outside.root.join("secret")).unwrap(),
        "outside text"
    );
}
