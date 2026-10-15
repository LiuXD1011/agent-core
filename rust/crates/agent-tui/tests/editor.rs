use agent_tui::editor::{cell_width, Editor, MAX_INPUT_BYTES};
use unicode_segmentation::UnicodeSegmentation;

fn boundary(editor: &Editor) {
    assert!(
        editor.cursor() == editor.text().len()
            || editor
                .text()
                .grapheme_indices(true)
                .any(|(index, _)| index == editor.cursor())
    );
}

#[test]
fn grapheme_navigation_selection_and_deletion() {
    let mut editor = Editor::default();
    editor.insert("a中e\u{301}👩‍💻").unwrap();
    editor.left(true);
    assert_eq!(editor.selected_text(), Some("👩‍💻"));
    editor.backspace();
    assert_eq!(editor.text(), "a中e\u{301}");
    editor.left(false);
    assert_eq!(editor.cursor(), 4);
    editor.delete();
    assert_eq!(editor.text(), "a中");
    editor.backspace();
    assert_eq!(editor.text(), "a");
    boundary(&editor);
}
#[test]
fn insert_and_delete_joined_graphemes_keep_valid_cursor() {
    let mut editor = Editor::default();
    editor.insert("e\n\u{301}").unwrap();
    editor.home(false);
    editor.left(false);
    editor.delete();
    assert_eq!(editor.text(), "e\u{301}");
    boundary(&editor);
    editor.backspace();
    assert_eq!(editor.text(), "");
    editor.insert("👩💻").unwrap();
    editor.left(false);
    editor.insert("\u{200d}").unwrap();
    assert_eq!(editor.text(), "👩‍💻");
    boundary(&editor);
    editor.backspace();
    assert_eq!(editor.text(), "");
}
#[test]
fn paste_normalization_and_limit_are_atomic() {
    let mut editor = Editor::default();
    editor.insert("a\r\nb\rc").unwrap();
    assert_eq!(editor.text(), "a\nb\nc");
    let before = editor.text().to_owned();
    assert!(editor.insert(&"x".repeat(MAX_INPUT_BYTES)).is_err());
    assert_eq!(editor.text(), before);
    editor.select_all();
    editor.insert(&"x".repeat(MAX_INPUT_BYTES)).unwrap();
    assert_eq!(editor.text().len(), MAX_INPUT_BYTES);
    assert!(editor.insert("a").is_err());
}
#[test]
fn undo_redo_preserve_unicode_selection_and_new_edit_invalidates_redo() {
    let mut editor = Editor::default();
    editor.insert("中文").unwrap();
    editor.left(true);
    editor.insert("字").unwrap();
    assert_eq!(editor.text(), "中字");
    editor.undo();
    assert_eq!(editor.selected_text(), Some("文"));
    editor.redo();
    assert_eq!(editor.text(), "中字");
    editor.undo();
    editor.insert("词").unwrap();
    editor.redo();
    assert_eq!(editor.text(), "中词");
    boundary(&editor);
}
#[test]
fn vertical_motion_retains_terminal_column_with_tabs_and_wide_text() {
    let mut editor = Editor::default();
    editor.insert("x\t中a\nx  中a\nx\t中a").unwrap();
    assert_eq!(cell_width("x\t中a"), 7);
    assert!(editor.vertical(false, false));
    assert_eq!(editor.cursor(), "x\t中a\nx  中a".len());
    assert!(editor.vertical(false, false));
    assert_eq!(editor.cursor(), "x\t中a".len());
    assert!(!editor.vertical(false, false));
    boundary(&editor);
}
#[test]
fn kill_ring_yank_rotation_and_undo() {
    let mut editor = Editor::default();
    editor.insert("one two").unwrap();
    editor.kill_word_left();
    assert_eq!(editor.text(), "one ");
    editor.kill_to_start();
    assert_eq!(editor.text(), "");
    editor.yank().unwrap();
    assert_eq!(editor.text(), "one ");
    editor.yank_pop().unwrap();
    assert_eq!(editor.text(), "two");
    editor.undo();
    assert_eq!(editor.text(), "one ");
    boundary(&editor);
}
#[test]
fn prompt_history_restores_unsubmitted_draft() {
    let mut editor = Editor::default();
    editor.insert("第一条").unwrap();
    assert_eq!(editor.submit(), "第一条");
    editor.insert("第二条").unwrap();
    editor.submit();
    editor.insert("未提交\n草稿").unwrap();
    editor.history_previous();
    assert_eq!(editor.text(), "第二条");
    editor.history_previous();
    assert_eq!(editor.text(), "第一条");
    editor.history_next();
    assert_eq!(editor.text(), "第二条");
    editor.history_next();
    assert_eq!(editor.text(), "未提交\n草稿");
    boundary(&editor);
}
#[test]
fn word_and_line_motion_remain_grapheme_aligned() {
    let mut editor = Editor::default();
    editor.insert("alpha e\u{301}\n中文 👩‍💻").unwrap();
    for _ in 0..8 {
        editor.word_left(false);
        boundary(&editor);
    }
    for _ in 0..8 {
        editor.word_right(false);
        boundary(&editor);
    }
    editor.home(true);
    assert!(editor.selected_text().is_some());
    editor.end(false);
    boundary(&editor);
}

#[test]
fn history_navigation_invalidates_stale_yank_range() {
    let mut editor = Editor::default();
    editor.insert("x").unwrap();
    editor.submit();
    editor.insert("one two three").unwrap();
    editor.kill_word_left();
    editor.kill_to_start();
    editor.yank().unwrap();
    editor.history_previous();
    assert_eq!(editor.text(), "x");
    editor.yank_pop().unwrap();
    assert_eq!(editor.text(), "x");
    boundary(&editor);
    editor.history_next();
    let draft = editor.text().to_owned();
    editor.yank_pop().unwrap();
    assert_eq!(editor.text(), draft);
    boundary(&editor);
}

#[test]
fn programmatic_draft_normalizes_line_endings() {
    let mut editor = Editor::default();
    editor.set_text("a\r\nb\rc").unwrap();
    assert_eq!(editor.text(), "a\nb\nc");
    boundary(&editor);
}
