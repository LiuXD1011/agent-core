use agent_core::types::{AgentEvent, AssistantMessage, Content, Message};
use agent_tui::keybindings::{Action, Keybindings};
use agent_tui::model::{AppAction, TuiModel};
use agent_tui::render::{render, transcript, wrap_editor};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::backend::TestBackend;
use ratatui::Terminal;

fn key(code: KeyCode, modifiers: KeyModifiers) -> Event {
    Event::Key(KeyEvent::new(code, modifiers))
}

#[test]
fn default_and_configurable_shortcuts_resolve_without_hardcoding() {
    let mut bindings = Keybindings::default();
    assert_eq!(
        bindings.resolve(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        Some(Action::Submit)
    );
    bindings
        .overrides
        .insert("submit".into(), vec!["ctrl+s".into()]);
    bindings.overrides.insert("newline".into(), vec![]);
    bindings.validate().unwrap();
    assert_eq!(
        bindings.resolve(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        None
    );
    assert_eq!(
        bindings.resolve(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL)),
        Some(Action::Submit)
    );
    assert_eq!(
        bindings.resolve(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL)),
        None
    );
    bindings
        .overrides
        .insert("copy".into(), vec!["ctrl+s".into()]);
    assert!(bindings.validate().is_err());
    bindings.overrides.insert("unknown".into(), vec![]);
    assert!(bindings.validate().is_err());
}
#[test]
fn multiline_paste_submit_history_and_key_release() {
    let mut model = TuiModel::default();
    model.handle(Event::Paste("你好\r\n世界".into()));
    let mut release = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE);
    release.kind = KeyEventKind::Release;
    model.handle(Event::Key(release));
    assert_eq!(model.editor.text(), "你好\n世界");
    assert_eq!(
        model.handle(key(KeyCode::Enter, KeyModifiers::NONE)),
        Some(AppAction::Submit("你好\n世界".into()))
    );
    model.handle(key(KeyCode::Up, KeyModifiers::NONE));
    assert_eq!(model.editor.text(), "你好\n世界");
    model.handle(key(KeyCode::Enter, KeyModifiers::SHIFT));
    assert!(model.editor.text().ends_with('\n'));
}
#[test]
fn cancel_copy_clear_and_eof_have_distinct_actions() {
    let mut model = TuiModel::default();
    model.editor.insert("draft").unwrap();
    assert_eq!(
        model.handle(key(KeyCode::Char('c'), KeyModifiers::CONTROL)),
        None
    );
    assert_eq!(model.editor.text(), "");
    assert_eq!(
        model.handle(key(KeyCode::Char('c'), KeyModifiers::CONTROL)),
        Some(AppAction::Quit)
    );
    model.editor.insert("中文").unwrap();
    model.editor.select_all();
    assert_eq!(
        model.handle(key(KeyCode::Char('c'), KeyModifiers::CONTROL)),
        Some(AppAction::Copy("中文".into()))
    );
    model.editor.end(false);
    model.running = true;
    assert_eq!(
        model.handle(key(KeyCode::Esc, KeyModifiers::NONE)),
        Some(AppAction::Cancel)
    );
    model.running = false;
    model.editor.home(false);
    assert_eq!(
        model.handle(key(KeyCode::Char('d'), KeyModifiers::CONTROL)),
        None
    );
    assert_eq!(model.editor.text(), "文");
}
#[test]
fn streaming_snapshots_do_not_duplicate_final_messages() {
    let mut model = TuiModel::default();
    model.apply(AgentEvent::AgentStart);
    model.apply(AgentEvent::MessageUpdate {
        message: Message::Assistant(AssistantMessage::text("partial")),
    });
    assert!(model.partial.is_some());
    let final_message = Message::Assistant(AssistantMessage::text("final"));
    model.apply(AgentEvent::MessageEnd {
        message: final_message.clone(),
    });
    model.finish(vec![Message::user("request"), final_message], Ok(()));
    assert_eq!(model.messages.len(), 2);
    assert!(!model.running);
    assert!(model.partial.is_none());
}
#[test]
fn completion_search_session_selector_and_scroll_are_native() {
    let mut model = TuiModel::default();
    model.editor.insert("/comp").unwrap();
    model.handle(key(KeyCode::Tab, KeyModifiers::NONE));
    assert_eq!(model.editor.text(), "/compact ");
    model.handle(key(KeyCode::Char('r'), KeyModifiers::CONTROL));
    assert_eq!(model.session_selector, Some(0));
    model.handle(key(KeyCode::Esc, KeyModifiers::NONE));
    model.handle(key(
        KeyCode::Char('f'),
        KeyModifiers::CONTROL | KeyModifiers::SHIFT,
    ));
    model.handle(Event::Paste("needle".into()));
    model.handle(key(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(model.search_query, "needle");
    model.transcript_height = 100;
    model.viewport_height = 20;
    model.handle(key(KeyCode::End, KeyModifiers::CONTROL));
    model.handle(key(KeyCode::PageUp, KeyModifiers::NONE));
    assert_eq!(model.scroll_offset, Some(60));
}
#[test]
fn wraps_graphemes_and_tabs_with_original_byte_offsets() {
    let text = "a\t中e\u{301}\n👩‍💻";
    let lines = wrap_editor(text, 6);
    assert_eq!(
        lines
            .iter()
            .map(|line| line.text.as_str())
            .collect::<Vec<_>>(),
        vec!["a   中", "e\u{301}", "👩‍💻"]
    );
    for line in &lines {
        assert!(text.is_char_boundary(line.start));
        assert!(text.is_char_boundary(line.end));
    }
    assert_eq!(lines[1].start, "a\t中".len());
    assert_eq!(wrap_editor("", 0).len(), 1);
}
#[test]
fn renderer_handles_tiny_resize_unicode_long_scroll_and_controls() {
    let mut model = TuiModel::default();
    model.editor.insert("x\t中\n👩‍💻 e\u{301}").unwrap();
    model.messages = vec![Message::user(
        "line\u{1b}[2J\n# heading\n```rust\nlet a = 1;\n```\nneedle",
    )];
    for (width, height) in [(1, 1), (8, 4), (8, 5), (20, 8), (100, 30)] {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| render(frame, &mut model)).unwrap();
    }
    let text = transcript(&model)
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(!text.contains('\u{1b}'));
    assert!(text.contains("␛[2J"));
    assert!(text.contains("heading"));
}
#[test]
fn thinking_images_tool_results_and_error_content_remain_visible() {
    let mut assistant = AssistantMessage::text("text");
    assistant.content.push(Content::Thinking {
        thinking: "reasoning".into(),
    });
    assistant.content.push(Content::Image {
        mime_type: "image/png".into(),
        data: "fixture".into(),
    });
    assistant.error_message = Some("fixture error".into());
    let model = TuiModel {
        messages: vec![Message::Assistant(assistant)],
        ..Default::default()
    };
    let text = transcript(&model)
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    for expected in ["text", "reasoning", "image/png", "fixture error"] {
        assert!(text.contains(expected));
    }
    assert!(!text.contains("fixture\n"));
}

#[test]
fn modal_navigation_and_footer_use_the_same_configured_shortcuts() {
    let mut model = TuiModel::default();
    model
        .keybindings
        .overrides
        .insert("submit".into(), vec!["ctrl+s".into()]);
    model
        .keybindings
        .overrides
        .insert("cancel".into(), vec!["ctrl+x".into()]);
    model
        .keybindings
        .overrides
        .insert("up".into(), vec!["ctrl+p".into()]);
    model.keybindings.validate().unwrap();
    model.search_input = Some("needle".into());
    model.handle(key(KeyCode::Enter, KeyModifiers::NONE));
    assert!(model.search_input.is_some());
    model.handle(key(KeyCode::Char('s'), KeyModifiers::CONTROL));
    assert!(model.search_input.is_none());
    assert_eq!(model.search_query, "needle");
    model.sessions = vec![
        agent_tui::model::SessionChoice {
            id: "one".into(),
            name: "One".into(),
        },
        agent_tui::model::SessionChoice {
            id: "two".into(),
            name: "Two".into(),
        },
    ];
    model.session_selector = Some(1);
    model.handle(key(KeyCode::Char('p'), KeyModifiers::CONTROL));
    assert_eq!(model.session_selector, Some(0));
    assert_eq!(
        model.handle(key(KeyCode::Char('s'), KeyModifiers::CONTROL)),
        Some(AppAction::SelectSession("one".into()))
    );
    let mut terminal = Terminal::new(TestBackend::new(120, 24)).unwrap();
    terminal.draw(|frame| render(frame, &mut model)).unwrap();
    let text = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(text.contains("ctrl+s"));
}
#[test]
fn invalid_key_syntax_is_rejected_instead_of_disabling_an_action_silently() {
    for value in ["", "ctrl+ctrl+s", "alt+ctrl+s", "control+s", "f25"] {
        let mut bindings = Keybindings::default();
        bindings
            .overrides
            .insert("submit".into(), vec![value.into()]);
        assert!(bindings.validate().is_err(), "{value}");
    }
}
#[test]
fn selected_editor_graphemes_have_visible_highlight() {
    let mut model = TuiModel::default();
    model.editor.insert("中文 e\u{301}").unwrap();
    model.editor.left(true);
    let mut terminal = Terminal::new(TestBackend::new(40, 12)).unwrap();
    terminal.draw(|frame| render(frame, &mut model)).unwrap();
    let buffer = terminal.backend().buffer();
    assert!(buffer
        .content
        .iter()
        .any(|cell| cell.symbol() == "e\u{301}" && cell.bg == ratatui::style::Color::Blue));
}

#[test]
fn bottom_and_search_use_actual_word_wrapping_not_cell_width_division() {
    let mut model = TuiModel {
        messages: vec![Message::user(
            "000001 000002 000003 000004 000005 000006 000007 000008",
        )],
        ..Default::default()
    };
    let mut terminal = Terminal::new(TestBackend::new(10, 12)).unwrap();
    terminal.draw(|frame| render(frame, &mut model)).unwrap();
    let text = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(text.contains("000008"), "{text}");
    assert_eq!(
        model.transcript_height,
        ratatui::widgets::Paragraph::new(transcript(&model))
            .wrap(ratatui::widgets::Wrap { trim: false })
            .line_count(10)
    );
    model.search_query = "000008".into();
    model.scroll_offset = Some(0);
    terminal.draw(|frame| render(frame, &mut model)).unwrap();
    let text = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(text.contains("000008"));
}
