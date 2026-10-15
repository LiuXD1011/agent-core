use agent_cli::args::parse;
use agent_cli::client::Client;
use agent_cli::terminal_client::{smoke_test, TerminalClient};
use agent_core::types::{CancellationToken, Message};
use agent_runtime::session::SessionStore;
use std::fs;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

fn workspace() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../agent-core")
}
fn fixture(directory: Option<&std::path::Path>) -> Client {
    let mut args = vec![
        "--cwd".into(),
        workspace().to_string_lossy().into_owned(),
        "--no-skills".into(),
        "--no-prompt-templates".into(),
        "--no-context-files".into(),
    ];
    if let Some(directory) = directory {
        args.extend([
            "--session-dir".into(),
            directory.to_string_lossy().into_owned(),
        ]);
    } else {
        args.push("--no-session".into());
    }
    Client::from_args(&parse(args).unwrap()).unwrap()
}
fn drain(host: &mut TerminalClient) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while host.model.running {
        host.poll();
        assert!(Instant::now() < deadline, "Terminal worker did not finish");
        thread::sleep(Duration::from_millis(2));
    }
}

#[test]
fn terminal_worker_runs_shared_loop_and_reconciles_authoritative_messages() {
    let mut host = TerminalClient::new(fixture(None));
    host.submit("offline fixture", CancellationToken::new())
        .unwrap();
    assert!(host.model.running);
    drain(&mut host);
    assert_eq!(
        host.model
            .messages
            .iter()
            .filter(|message| matches!(message, Message::User(_)))
            .count(),
        1
    );
    assert!(host
        .model
        .messages
        .iter()
        .any(|message| matches!(message, Message::Assistant(_))));
    assert!(host.model.status.contains("完成"));
    assert!(host.model.status.contains("仅保存在内存"));
    host.save().unwrap();
}
#[test]
fn busy_session_mutation_does_not_switch_the_workers_session() {
    let mut host = TerminalClient::new(fixture(None));
    host.start(Some(Message::user("first")), CancellationToken::new())
        .unwrap();
    assert!(host.submit("/new", CancellationToken::new()).is_err());
    drain(&mut host);
    assert!(host
        .model
        .messages
        .iter()
        .any(|message| message.text() == "first"));
    host.submit("/new", CancellationToken::new()).unwrap();
    assert!(host.model.messages.is_empty());
}
#[test]
fn queued_terminal_input_is_preserved_across_worker_finish_race() {
    let mut host = TerminalClient::new(fixture(None));
    host.start(Some(Message::user("first")), CancellationToken::new())
        .unwrap();
    host.submit("second", CancellationToken::new()).unwrap();
    drain(&mut host);
    if host.has_queued() {
        host.start_queued(CancellationToken::new()).unwrap();
        drain(&mut host);
    }
    assert_eq!(
        host.model
            .messages
            .iter()
            .filter(|message| message.text() == "first")
            .count(),
        1
    );
    assert_eq!(
        host.model
            .messages
            .iter()
            .filter(|message| message.text() == "second")
            .count(),
        1
    );
}
#[test]
fn dropping_terminal_cancels_joins_and_saves_without_waiting_for_ui_events() {
    let directory = std::env::temp_dir().join(format!(
        "agent-tui-drop-{}-{}",
        std::process::id(),
        agent_core::types::timestamp()
    ));
    let mut host = TerminalClient::new(fixture(Some(&directory)));
    host.start(
        Some(Message::user("persist fixture")),
        CancellationToken::new(),
    )
    .unwrap();
    drop(host);
    let sessions = SessionStore::new(directory.clone()).list().unwrap();
    assert_eq!(sessions.len(), 1);
    assert!(sessions[0]
        .context
        .messages
        .iter()
        .any(|message| message.text() == "persist fixture"));
    for entry in fs::read_dir(&directory).unwrap() {
        fs::remove_file(entry.unwrap().path()).unwrap();
    }
    fs::remove_dir(directory).unwrap();
}
#[test]
fn terminal_branch_commands_fork_draft_clone_and_html_export() {
    let mut client = fixture(None);
    let user = client
        .session
        .append_message(Message::user("draft to recover"))
        .unwrap();
    client
        .session
        .append_message(Message::Assistant(
            agent_core::types::AssistantMessage::text("fixture answer"),
        ))
        .unwrap();
    let mut host = TerminalClient::new(client);
    host.submit(&format!("/fork {user}"), CancellationToken::new())
        .unwrap();
    assert_eq!(host.model.editor.text(), "draft to recover");
    assert!(host.model.messages.is_empty());
    host.submit(&format!("/navigate {user}"), CancellationToken::new())
        .unwrap();
    assert_eq!(host.model.messages.len(), 1);
    host.submit("/clone", CancellationToken::new()).unwrap();
    assert_eq!(host.model.messages.len(), 1);
    let path = std::env::temp_dir().join(format!(
        "agent-tui-export-{}-{}.html",
        std::process::id(),
        agent_core::types::timestamp()
    ));
    host.submit(
        &format!("/export-html {}", path.display()),
        CancellationToken::new(),
    )
    .unwrap();
    assert!(fs::read_to_string(&path)
        .unwrap()
        .contains("draft to recover"));
    fs::remove_file(path).unwrap();
}
#[test]
fn headless_terminal_smoke_has_native_frame_and_no_ansi_input_execution() {
    let report = smoke_test().unwrap();
    assert!(report.starts_with("PASS: offline Rust TUI"));
    assert!(report.contains("原生 Rust editor/render/host"));
}

#[test]
fn stopping_restores_unconsumed_input_and_does_not_restart_the_queue() {
    let client = fixture(None);
    let queues = client.queues.clone();
    let mut host = TerminalClient::new(client);
    host.model.editor.insert("current draft").unwrap();
    queues.steer(Message::user("queued fixture"));
    host.start(None, CancellationToken::new()).unwrap();
    host.cancel();
    assert!(!host.has_queued());
    assert_eq!(host.model.editor.text(), "queued fixture\n\ncurrent draft");
    drain(&mut host);
    assert!(!host.model.running);
    assert!(host.model.messages.is_empty());
    assert_eq!(host.model.editor.text(), "queued fixture\n\ncurrent draft");
}
