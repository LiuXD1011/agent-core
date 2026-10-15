use agent_core::tools::AgentTool;
use agent_core::types::{CancellationToken, Content, ExecutionMode, ToolResult};
use agent_runtime::builtins::{create_tools, tool_definitions};
use base64::{engine::general_purpose::STANDARD, Engine};
use image::{DynamicImage, ImageFormat};
use serde_json::{json, Value};
use std::fs;
use std::io::Cursor;
#[cfg(target_os = "windows")]
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
#[cfg(target_os = "windows")]
use std::process::{Command, Stdio};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Barrier,
};
use std::thread;
use std::time::Duration;

static NEXT: AtomicUsize = AtomicUsize::new(0);
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "agent-runtime-builtins-{}-{}-{}",
            std::process::id(),
            agent_core::types::timestamp(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn write(&self, path: &str, bytes: impl AsRef<[u8]>) {
        let path = self.0.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }
    fn tool(&self, name: &str) -> Box<dyn AgentTool> {
        create_tools(&self.0, true, true)
            .into_iter()
            .find(|tool| tool.name() == name)
            .unwrap()
    }
    fn run(&self, name: &str, args: Value) -> Result<ToolResult, String> {
        let tool = self.tool(name);
        let args = tool.prepare_arguments(&args)?;
        tool.validate_arguments(&args)?;
        tool.execute("fixture", &args, &CancellationToken::new(), &mut |_| {})
    }
    fn image(&self, path: &str, width: u32, height: u32, format: ImageFormat) {
        let image = if format == ImageFormat::Bmp {
            DynamicImage::new_rgb8(width, height)
        } else {
            DynamicImage::new_rgba8(width, height)
        };
        let mut buffer = Cursor::new(Vec::new());
        image.write_to(&mut buffer, format).unwrap();
        self.write(path, buffer.into_inner());
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        assert!(self.0.starts_with(std::env::temp_dir()));
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn text(result: &ToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|block| match block {
            Content::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn shared_tool_set_has_seven_tools_and_read_only_search_is_parallel() {
    let fixture = Fixture::new();
    let readonly = create_tools(&fixture.0, false, false);
    assert_eq!(
        readonly.iter().map(|tool| tool.name()).collect::<Vec<_>>(),
        vec!["read", "ls", "grep", "find"]
    );
    assert!(readonly
        .iter()
        .all(|tool| tool.execution_mode() == Some(ExecutionMode::Parallel)));
    let all = create_tools(&fixture.0, true, true);
    assert_eq!(all.len(), 7);
    assert_eq!(tool_definitions(&all).len(), 7);
    assert!(all
        .iter()
        .filter(|tool| matches!(tool.name(), "edit" | "write" | "run_command"))
        .all(|tool| tool.execution_mode() == Some(ExecutionMode::Sequential)));
}

#[test]
fn read_preserves_crlf_trailing_newline_and_rejects_out_of_bounds_offsets() {
    let fixture = Fixture::new();
    fixture.write("text.txt", "one\r\n中文\r\nthree\r\n");
    let full = fixture.run("read", json!({"path":"text.txt"})).unwrap();
    assert_eq!(text(&full), "one\r\n中文\r\nthree\r\n");
    assert_eq!(full.details["totalLines"], 4);
    let limited = fixture
        .run("read", json!({"path":"text.txt","offset":2,"limit":1}))
        .unwrap();
    assert!(text(&limited).starts_with("中文\r"));
    assert!(text(&limited).contains("offset=3"));
    assert!(fixture
        .run("read", json!({"path":"text.txt","offset":5}))
        .unwrap_err()
        .contains("beyond end"));
    fixture.write("empty.txt", "");
    assert_eq!(
        text(&fixture.run("read", json!({"path":"empty.txt"})).unwrap()),
        ""
    );
}

#[test]
fn read_enforces_line_and_byte_caps_using_complete_lines_and_continuation() {
    let fixture = Fixture::new();
    fixture.write(
        "many.txt",
        (1..=2100)
            .map(|line| format!("line-{line}\n"))
            .collect::<String>(),
    );
    let result = fixture.run("read", json!({"path":"many.txt"})).unwrap();
    assert_eq!(result.details["shownLines"], 2000);
    assert!(text(&result).contains("offset=2001"));
    assert!(!text(&result).contains("line-2001"));
    fixture.write(
        "bytes.txt",
        (0..30)
            .map(|_| format!("{}\n", "中".repeat(1000)))
            .collect::<String>(),
    );
    let result = fixture.run("read", json!({"path":"bytes.txt"})).unwrap();
    assert_eq!(result.details["truncated"], true);
    assert!(
        result.details["truncation"]["outputBytes"]
            .as_u64()
            .unwrap()
            <= 51_200
    );
    fixture.write("huge-line.txt", "x".repeat(60_000));
    assert!(text(
        &fixture
            .run("read", json!({"path":"huge-line.txt"}))
            .unwrap()
    )
    .contains("50KB"));
}

#[test]
fn read_uses_content_sniffing_and_attaches_png_and_converted_bmp() {
    let fixture = Fixture::new();
    for (name, format) in [
        ("no-extension", ImageFormat::Png),
        ("picture.bmp", ImageFormat::Bmp),
    ] {
        fixture.image(name, 4, 3, format);
        let result = fixture.run("read", json!({"path":name})).unwrap();
        let (data, mime) = result
            .content
            .iter()
            .find_map(|block| match block {
                Content::Image { data, mime_type } => Some((data, mime_type)),
                _ => None,
            })
            .unwrap();
        assert_eq!(mime, "image/png");
        assert!(STANDARD.decode(data).unwrap().starts_with(b"\x89PNG"));
        assert_eq!(result.details["width"], 4);
        assert_eq!(result.details["height"], 3);
    }
}

#[test]
fn read_resizes_images_and_reports_the_coordinate_scale() {
    let fixture = Fixture::new();
    fixture.image("wide.png", 2001, 1, ImageFormat::Png);
    let result = fixture.run("read", json!({"path":"wide.png"})).unwrap();
    assert_eq!(result.details["originalWidth"], 2001);
    assert_eq!(result.details["width"], 2000);
    assert!(text(&result).contains("Multiply coordinates"));
}

#[test]
fn paths_support_at_prefix_unicode_spaces_and_curly_quote_read_variants() {
    let fixture = Fixture::new();
    fixture.write("a b.txt", "spaces");
    fixture.write("capture d’écran.txt", "quote");
    assert_eq!(
        text(
            &fixture
                .run("read", json!({"path":"@a\u{00a0}b.txt"}))
                .unwrap()
        ),
        "spaces"
    );
    assert_eq!(
        text(
            &fixture
                .run("read", json!({"path":"capture d'écran.txt"}))
                .unwrap()
        ),
        "quote"
    );
    fixture
        .run("write", json!({"path":"@nested/new.txt","content":"new"}))
        .unwrap();
    assert_eq!(
        fs::read_to_string(fixture.0.join("nested/new.txt")).unwrap(),
        "new"
    );
}

#[test]
fn ls_defaults_to_workspace_includes_hidden_and_obeys_entry_limits() {
    let fixture = Fixture::new();
    fixture.write("B.txt", "");
    fixture.write("a.txt", "");
    fixture.write(".hidden", "");
    fs::create_dir(fixture.0.join("dir")).unwrap();
    assert_eq!(
        text(&fixture.run("ls", json!({})).unwrap()),
        ".hidden\na.txt\nB.txt\ndir/"
    );
    let result = fixture.run("ls", json!({"limit":2})).unwrap();
    assert_eq!(result.details["entryLimitReached"], 2);
    assert!(text(&result).contains("Use limit=4"));
    assert!(!text(&result).contains("B.txt"));
    assert!(fixture
        .run("ls", json!({"path":"a.txt"}))
        .unwrap_err()
        .contains("Not a directory"));
}

#[test]
fn find_respects_ignore_files_in_non_git_workspaces_and_includes_hidden_entries() {
    let fixture = Fixture::new();
    fixture.write(".gitignore", "ignored.rs\nvendor/\nnode_modules/\n");
    fixture.write(".ignore", "local.rs\n");
    for path in [
        "src/main.rs",
        "src/ignored.rs",
        "local.rs",
        "vendor/a.rs",
        "node_modules/a.rs",
        ".hidden/config.rs",
        "notes.txt",
    ] {
        fixture.write(path, "needle");
    }
    let result = fixture.run("find", json!({"pattern":"*.rs"})).unwrap();
    assert_eq!(text(&result), ".hidden/config.rs\nsrc/main.rs");
    let result = fixture
        .run("find", json!({"pattern":"src/**/*.rs"}))
        .unwrap();
    assert_eq!(text(&result), "src/main.rs");
    assert_eq!(
        text(&fixture.run("find", json!({"pattern":"missing*"})).unwrap()),
        "No files found matching pattern"
    );
}

#[test]
fn find_handles_glob_braces_directories_nested_paths_and_result_limits() {
    let fixture = Fixture::new();
    for path in ["src/a.rs", "src/b.txt", "nested/src/c.rs"] {
        fixture.write(path, "");
    }
    assert!(text(
        &fixture
            .run("find", json!({"pattern":"src/**/*.rs"}))
            .unwrap()
    )
    .contains("nested/src/c.rs"));
    let result = fixture
        .run("find", json!({"pattern":"{*.rs,*.txt}"}))
        .unwrap();
    assert_eq!(text(&result).lines().count(), 3);
    assert!(text(&fixture.run("find", json!({"pattern":"src"})).unwrap()).contains("src/"));
    let result = fixture
        .run("find", json!({"pattern":"*.rs","limit":1}))
        .unwrap();
    assert_eq!(result.details["resultLimitReached"], 1);
    assert!(text(&result).contains("Use limit=2"));
    assert!(fixture
        .run("find", json!({"pattern":"["}))
        .unwrap_err()
        .contains("Invalid glob"));
}

#[test]
fn nested_repository_boundaries_stop_parent_gitignore_rules() {
    let fixture = Fixture::new();
    fs::create_dir(fixture.0.join(".git")).unwrap();
    fixture.write(".gitignore", "ignored.txt\n");
    fixture.write("ignored.txt", "parent");
    fixture.write("nested/ignored.txt", "nested");
    fs::create_dir(fixture.0.join("nested/.git")).unwrap();
    fixture.write(".git/internal.txt", "internal");
    let result = fixture.run("find", json!({"pattern":"*.txt"})).unwrap();
    assert_eq!(text(&result), "nested/ignored.txt");
}

#[test]
fn grep_supports_regex_literal_case_glob_context_and_single_file_search() {
    let fixture = Fixture::new();
    fixture.write("src/file.rs", "before\nVersion42\na.b\naxb\nafter\n");
    fixture.write("other.txt", "Version99\n");
    let result = fixture
        .run(
            "grep",
            json!({"pattern":"^version\\d+$","ignoreCase":true,"glob":"*.rs","context":1}),
        )
        .unwrap();
    assert_eq!(
        text(&result),
        "src/file.rs-1- before\nsrc/file.rs:2: Version42\nsrc/file.rs-3- a.b"
    );
    let result = fixture
        .run(
            "grep",
            json!({"pattern":"a.b","literal":true,"path":"src/file.rs"}),
        )
        .unwrap();
    assert_eq!(text(&result), "file.rs:3: a.b");
    let result = fixture
        .run("grep", json!({"pattern":"Version","glob":"!*.txt"}))
        .unwrap();
    assert!(!text(&result).contains("other.txt"));
    assert!(fixture
        .run("grep", json!({"pattern":"["}))
        .unwrap_err()
        .contains("Invalid regex"));
}

#[test]
fn grep_respects_gitignore_hidden_files_and_binary_exclusion() {
    let fixture = Fixture::new();
    fs::create_dir(fixture.0.join(".git")).unwrap();
    fixture.write(".gitignore", "ignore.txt\n");
    fixture.write("ignore.txt", "needle");
    fixture.write("binary.txt", b"needle\0binary");
    fixture.write(".hidden/data.txt", "needle\n");
    fixture.write(".git/internal.txt", "needle");
    let result = fixture.run("grep", json!({"pattern":"needle"})).unwrap();
    assert_eq!(text(&result), ".hidden/data.txt:1: needle");
    assert_eq!(
        text(&fixture.run("grep", json!({"pattern":"absent"})).unwrap()),
        "No matches found"
    );
}

#[test]
fn grep_caps_matches_bytes_and_long_lines_with_actionable_details() {
    let fixture = Fixture::new();
    fixture.write("matches.txt", "needle\nneedle\nneedle\n");
    let result = fixture
        .run("grep", json!({"pattern":"needle","limit":2}))
        .unwrap();
    assert_eq!(result.details["matchLimitReached"], 2);
    assert!(text(&result).contains("Use limit=4"));
    assert!(!text(&result).contains("matches.txt:3"));
    fixture.write("long.txt", format!("needle{}", "中".repeat(600)));
    let result = fixture
        .run("grep", json!({"path":"long.txt","pattern":"needle"}))
        .unwrap();
    assert_eq!(result.details["linesTruncated"], true);
    assert!(text(&result).contains("[truncated]"));
    fixture.write(
        "large.txt",
        (0..100)
            .map(|_| format!("needle{}\n", "中".repeat(490)))
            .collect::<String>(),
    );
    let result = fixture
        .run(
            "grep",
            json!({"path":"large.txt","pattern":"needle","limit":1000}),
        )
        .unwrap();
    assert_eq!(result.details["byteLimitReached"], true);
    assert!(text(&result).split("\n\n[").next().unwrap().len() <= 51_200);
}

#[cfg(target_os = "windows")]
fn directory_link(link: &Path, target: &Path) {
    let status = Command::new("cmd.exe")
        .args(["/d", "/c", "mklink", "/J"])
        .arg(link)
        .arg(target)
        .creation_flags(0x0800_0000)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success());
}
#[cfg(unix)]
fn directory_link(link: &Path, target: &Path) {
    std::os::unix::fs::symlink(target, link).unwrap();
}

#[test]
fn traversal_does_not_follow_directory_symlinks_or_junctions() {
    let fixture = Fixture::new();
    fixture.write("real/file.txt", "needle");
    directory_link(&fixture.0.join("linked"), &fixture.0.join("real"));
    assert_eq!(
        text(&fixture.run("find", json!({"pattern":"*.txt"})).unwrap()),
        "real/file.txt"
    );
    assert_eq!(
        text(&fixture.run("grep", json!({"pattern":"needle"})).unwrap()),
        "real/file.txt:1: needle"
    );
}

#[test]
fn all_tools_honor_pre_cancellation_without_mutating_files() {
    let fixture = Fixture::new();
    fixture.write("file.txt", "original");
    let signal = CancellationToken::new();
    signal.cancel();
    for (name, args) in [
        ("read", json!({"path":"file.txt"})),
        ("ls", json!({})),
        ("find", json!({"pattern":"*"})),
        ("grep", json!({"pattern":"original"})),
        ("write", json!({"path":"file.txt","content":"new"})),
        (
            "edit",
            json!({"path":"file.txt","oldText":"original","newText":"new"}),
        ),
        ("run_command", json!({"command":"exit 0"})),
    ] {
        assert_eq!(
            fixture
                .tool(name)
                .execute("cancel", &args, &signal, &mut |_| {})
                .unwrap_err(),
            "Operation aborted"
        );
    }
    assert_eq!(
        fs::read_to_string(fixture.0.join("file.txt")).unwrap(),
        "original"
    );
}

#[test]
fn grep_observes_cancellation_while_processing_a_large_local_fixture() {
    let fixture = Fixture::new();
    fixture.write("large.txt", vec![b'a'; 8 * 1024 * 1024]);
    let signal = CancellationToken::new();
    let cancel = signal.clone();
    let ready = Arc::new(Barrier::new(2));
    let notify = ready.clone();
    let worker = thread::spawn(move || {
        notify.wait();
        thread::sleep(Duration::from_millis(1));
        cancel.cancel();
    });
    ready.wait();
    let error = fixture
        .tool("grep")
        .execute(
            "cancel",
            &json!({"pattern":"absent","path":"large.txt"}),
            &signal,
            &mut |_| {},
        )
        .unwrap_err();
    worker.join().unwrap();
    assert_eq!(error, "Operation aborted");
}

#[test]
fn edits_are_matched_against_original_content_and_preserve_bom_and_crlf() {
    let fixture = Fixture::new();
    fixture.write("file.txt", "\u{feff}one\r\ntwo\r\nkeep\r\n");
    let result = fixture.run("edit",json!({"path":"file.txt","edits":[{"oldText":"one\ntwo","newText":"first\ntwo"},{"oldText":"keep","newText":"last"}]})).unwrap();
    assert_eq!(
        fs::read_to_string(fixture.0.join("file.txt")).unwrap(),
        "\u{feff}first\r\ntwo\r\nlast\r\n"
    );
    assert!(result.details["patch"]
        .as_str()
        .unwrap()
        .contains("--- file.txt"));
    assert!(result.details["diff"]
        .as_str()
        .unwrap()
        .contains("+1 first"));
    assert_eq!(result.details["firstChangedLine"], 1);
    fixture.write("file.txt", "one\ntwo\n");
    fixture.run("edit",json!({"path":"file.txt","edits":[{"oldText":"one","newText":"two"},{"oldText":"two","newText":"three"}]})).unwrap();
    assert_eq!(
        fs::read_to_string(fixture.0.join("file.txt")).unwrap(),
        "two\nthree\n"
    );
}

#[test]
fn edit_rejects_duplicate_overlap_cascading_missing_and_identical_changes_atomically() {
    let fixture = Fixture::new();
    let original = "abc abc\nother\n";
    for edits in [
        json!([{"oldText":"abc","newText":"x"}]),
        json!([{"oldText":"abc abc","newText":"x"},{"oldText":"abc abc\nother","newText":"y"}]),
        json!([{"oldText":"other","newText":"new"},{"oldText":"new","newText":"after"}]),
        json!([{"oldText":"other","newText":"other"}]),
        json!([{"oldText":"","newText":"x"}]),
    ] {
        fixture.write("file.txt", original);
        assert!(fixture
            .run("edit", json!({"path":"file.txt","edits":edits}))
            .is_err());
        assert_eq!(
            fs::read_to_string(fixture.0.join("file.txt")).unwrap(),
            original
        );
    }
}

#[test]
fn fuzzy_edits_preserve_untouched_lines_and_detect_normalized_duplicate_matches() {
    let fixture = Fixture::new();
    fixture.write("file.txt", "keep  \t\n“value” — old  \nend \t\n");
    fixture
        .run(
            "edit",
            json!({"path":"file.txt","oldText":"\"value\" - old","newText":"changed"}),
        )
        .unwrap();
    assert_eq!(
        fs::read_to_string(fixture.0.join("file.txt")).unwrap(),
        "keep  \t\nchanged\nend \t\n"
    );
    fixture.write("file.txt", "“same”\n\"same\"\n");
    assert!(fixture
        .run(
            "edit",
            json!({"path":"file.txt","oldText":"\"same\"","newText":"new"})
        )
        .unwrap_err()
        .contains("2 occurrences"));
}

#[test]
fn edit_preparation_accepts_legacy_single_object_and_json_string_shapes() {
    let fixture = Fixture::new();
    for input in [
        json!({"path":"file.txt","oldText":"old","newText":"new"}),
        json!({"path":"file.txt","edits":{"oldText":"old","newText":"new"}}),
        json!({"path":"file.txt","edits":"[{\"oldText\":\"old\",\"newText\":\"new\"}]"}),
    ] {
        fixture.write("file.txt", "old");
        fixture.run("edit", input).unwrap();
        assert_eq!(
            fs::read_to_string(fixture.0.join("file.txt")).unwrap(),
            "new"
        );
    }
    let tool = fixture.tool("edit");
    assert!(tool
        .validate_arguments(
            &tool
                .prepare_arguments(&json!({"path":"file.txt","edits":"invalid"}))
                .unwrap()
        )
        .is_err());
    assert!(tool
        .validate_arguments(&json!({"path":"file.txt","edits":[]}))
        .is_err());
}

#[test]
fn concurrent_disjoint_edits_on_the_same_file_are_serialized_across_tool_instances() {
    let fixture = Fixture::new();
    fixture.write("file.txt", "left\nright\n");
    let barrier = Arc::new(Barrier::new(2));
    let workers: Vec<_> = [("left", "LEFT"), ("right", "RIGHT")]
        .into_iter()
        .map(|(old, new)| {
            let tool = fixture.tool("edit");
            let barrier = barrier.clone();
            thread::spawn(move || {
                barrier.wait();
                tool.execute(
                    "concurrent",
                    &json!({"path":"file.txt","oldText":old,"newText":new}),
                    &CancellationToken::new(),
                    &mut |_| {},
                )
                .unwrap();
            })
        })
        .collect();
    for worker in workers {
        worker.join().unwrap();
    }
    assert_eq!(
        fs::read_to_string(fixture.0.join("file.txt")).unwrap(),
        "LEFT\nRIGHT\n"
    );
}

#[test]
fn grep_does_not_invent_an_empty_match_after_the_terminal_newline() {
    let fixture = Fixture::new();
    fixture.write("terminal.txt", "line\n");
    fixture.write("empty.txt", "");
    assert_eq!(
        text(
            &fixture
                .run("grep", json!({"pattern":"^$","path":"terminal.txt"}))
                .unwrap()
        ),
        "No matches found"
    );
    assert_eq!(
        text(
            &fixture
                .run("grep", json!({"pattern":"^$","path":"empty.txt"}))
                .unwrap()
        ),
        "No matches found"
    );
    assert!(fixture
        .run("find", json!({"pattern":"*","path":"terminal.txt"}))
        .unwrap_err()
        .contains("Not a directory"));
}

#[test]
fn jpeg_exif_orientation_is_applied_to_reported_dimensions() {
    let fixture = Fixture::new();
    fixture.image("raw.jpg", 4, 2, ImageFormat::Jpeg);
    let raw = fs::read(fixture.0.join("raw.jpg")).unwrap();
    let exif = b"Exif\0\0II\x2a\0\x08\0\0\0\x01\0\x12\x01\x03\0\x01\0\0\0\x06\0\0\0\0\0\0\0";
    let mut oriented = raw[..2].to_vec();
    oriented.extend_from_slice(&[0xff, 0xe1]);
    oriented.extend_from_slice(&((exif.len() + 2) as u16).to_be_bytes());
    oriented.extend_from_slice(exif);
    oriented.extend_from_slice(&raw[2..]);
    fixture.write("oriented.jpg", oriented);
    let result = fixture.run("read", json!({"path":"oriented.jpg"})).unwrap();
    assert_eq!(result.details["width"], 2);
    assert_eq!(result.details["height"], 4);
}
