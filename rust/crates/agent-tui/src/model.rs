use crate::editor::Editor;
use crate::keybindings::{Action, Keybindings};
use agent_core::types::{AgentEvent, AssistantMessage, Message};
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers, MouseEventKind};
use unicode_segmentation::UnicodeSegmentation;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Theme {
    #[default]
    Dark,
    Light,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AppAction {
    Submit(String),
    Cancel,
    Quit,
    NewSession,
    Compact,
    SelectSession(String),
    Copy(String),
    Paste,
}
#[derive(Clone, Debug)]
pub struct SessionChoice {
    pub id: String,
    pub name: String,
}

pub struct TuiModel {
    pub editor: Editor,
    pub keybindings: Keybindings,
    pub title: String,
    pub model: String,
    pub status: String,
    pub messages: Vec<Message>,
    pub partial: Option<AssistantMessage>,
    pub running: bool,
    pub closing: bool,
    pub show_tools: bool,
    pub theme: Theme,
    pub sessions: Vec<SessionChoice>,
    pub session_selector: Option<usize>,
    pub commands: Vec<String>,
    pub scroll_offset: Option<usize>,
    pub transcript_height: usize,
    pub viewport_height: usize,
    pub search_input: Option<String>,
    pub search_query: String,
    pub activity: String,
}
impl Default for TuiModel {
    fn default() -> Self {
        Self {
            editor: Editor::default(),
            keybindings: Keybindings::default(),
            title: "Agent Core · Rust TUI".into(),
            model: "offline demo".into(),
            status: "就绪".into(),
            messages: vec![],
            partial: None,
            running: false,
            closing: false,
            show_tools: true,
            theme: Theme::Dark,
            sessions: vec![],
            session_selector: None,
            commands: vec![
                "/exit".into(),
                "/new".into(),
                "/sessions".into(),
                "/compact".into(),
                "/tree".into(),
                "/theme".into(),
            ],
            scroll_offset: None,
            transcript_height: 0,
            viewport_height: 0,
            search_input: None,
            search_query: String::new(),
            activity: String::new(),
        }
    }
}

impl TuiModel {
    pub fn apply(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::AgentStart => {
                self.running = true;
                self.activity = "生成中".into();
            }
            AgentEvent::MessageStart {
                message: Message::Assistant(message),
            }
            | AgentEvent::MessageUpdate {
                message: Message::Assistant(message),
            } => self.partial = Some(message),
            AgentEvent::MessageEnd { message } => {
                if matches!(&message, Message::Assistant(_)) {
                    self.partial = None;
                }
                self.messages.push(message);
            }
            AgentEvent::ToolExecutionStart { tool_name, .. } => {
                self.activity = format!("执行 {tool_name}")
            }
            AgentEvent::ToolExecutionEnd {
                tool_name,
                is_error,
                ..
            } => self.activity = format!("{tool_name}: {}", if is_error { "错误" } else { "完成" }),
            AgentEvent::AgentEnd { .. } => self.partial = None,
            _ => {}
        }
    }
    pub fn finish(&mut self, messages: Vec<Message>, result: Result<(), String>) {
        self.messages = messages;
        self.partial = None;
        self.running = false;
        self.activity.clear();
        self.status = result.map_or_else(|error| error, |_| "完成 · 已保存".into());
    }
    fn scroll(&mut self, delta: isize) {
        let max = self.transcript_height.saturating_sub(self.viewport_height);
        let current = self.scroll_offset.unwrap_or(max);
        self.scroll_offset = Some(current.saturating_add_signed(delta).min(max));
    }
    fn complete(&mut self) -> Result<(), String> {
        let prefix = self.editor.text();
        if !prefix.starts_with('/') || prefix.contains(char::is_whitespace) {
            return self.editor.insert("\t");
        }
        let candidates: Vec<_> = self
            .commands
            .iter()
            .filter(|command| command.starts_with(prefix))
            .collect();
        if candidates.len() == 1 {
            self.editor.set_text(format!("{} ", candidates[0]))?;
        } else if !candidates.is_empty() {
            self.status = candidates
                .into_iter()
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join("  ");
        }
        Ok(())
    }
    pub fn handle(&mut self, event: Event) -> Option<AppAction> {
        if self.closing {
            return None;
        }
        match event {
            Event::Paste(text) => {
                if let Some(search) = &mut self.search_input {
                    if search.len() + text.len() <= 4096 {
                        search.push_str(&text.replace(['\r', '\n'], " "));
                    }
                } else if let Err(error) = self.editor.insert(&text) {
                    self.status = error;
                }
                None
            }
            Event::Mouse(mouse) => {
                match mouse.kind {
                    MouseEventKind::ScrollUp => self.scroll(-3),
                    MouseEventKind::ScrollDown => self.scroll(3),
                    _ => {}
                }
                None
            }
            Event::Resize(..) => None,
            Event::Key(key) if key.kind != KeyEventKind::Release => {
                let action = self.keybindings.resolve(key);
                if let Some(selected) = self.session_selector {
                    match action {
                        Some(Action::Cancel | Action::Quit) => self.session_selector = None,
                        Some(Action::Up) => {
                            self.session_selector = Some(selected.saturating_sub(1))
                        }
                        Some(Action::Down) => {
                            self.session_selector =
                                Some((selected + 1).min(self.sessions.len().saturating_sub(1)))
                        }
                        Some(Action::Submit) => {
                            self.session_selector = None;
                            return self
                                .sessions
                                .get(selected)
                                .map(|session| AppAction::SelectSession(session.id.clone()));
                        }
                        _ => {}
                    }
                    return None;
                }
                if let Some(search) = &mut self.search_input {
                    match action {
                        Some(Action::Cancel | Action::Quit) => self.search_input = None,
                        Some(Action::Submit) => {
                            self.search_query = search.clone();
                            self.search_input = None;
                            self.scroll_offset = Some(0);
                        }
                        Some(Action::Backspace) => {
                            let previous = search
                                .grapheme_indices(true)
                                .next_back()
                                .map_or(0, |(index, _)| index);
                            search.truncate(previous);
                        }
                        _ => {
                            if let KeyCode::Char(character) = key.code {
                                if !key
                                    .modifiers
                                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                                    && search.len() + character.len_utf8() <= 4096
                                {
                                    search.push(character);
                                }
                            }
                        }
                    }
                    return None;
                }
                let select = key.modifiers.contains(KeyModifiers::SHIFT);
                let result = match action {
                    Some(Action::Submit) => {
                        let text = self.editor.submit();
                        if !text.trim().is_empty() {
                            return Some(AppAction::Submit(text));
                        }
                        Ok(())
                    }
                    Some(Action::NewLine) => self.editor.insert("\n"),
                    Some(Action::Left) => {
                        self.editor.left(select);
                        Ok(())
                    }
                    Some(Action::Right) => {
                        self.editor.right(select);
                        Ok(())
                    }
                    Some(Action::Up) => {
                        if !self.editor.vertical(false, select) && !select {
                            self.editor.history_previous();
                        }
                        Ok(())
                    }
                    Some(Action::Down) => {
                        if !self.editor.vertical(true, select) && !select {
                            self.editor.history_next();
                        }
                        Ok(())
                    }
                    Some(Action::WordLeft) => {
                        self.editor.word_left(select);
                        Ok(())
                    }
                    Some(Action::WordRight) => {
                        self.editor.word_right(select);
                        Ok(())
                    }
                    Some(Action::Home) => {
                        self.editor.home(select);
                        Ok(())
                    }
                    Some(Action::End) => {
                        self.editor.end(select);
                        Ok(())
                    }
                    Some(Action::Backspace) => {
                        self.editor.backspace();
                        Ok(())
                    }
                    Some(Action::Delete) => {
                        self.editor.delete();
                        Ok(())
                    }
                    Some(Action::KillWordLeft) => {
                        self.editor.kill_word_left();
                        Ok(())
                    }
                    Some(Action::KillWordRight) => {
                        self.editor.kill_word_right();
                        Ok(())
                    }
                    Some(Action::KillStart) => {
                        self.editor.kill_to_start();
                        Ok(())
                    }
                    Some(Action::KillEnd) => {
                        self.editor.kill_to_end();
                        Ok(())
                    }
                    Some(Action::Yank) => self.editor.yank(),
                    Some(Action::YankPop) => self.editor.yank_pop(),
                    Some(Action::Undo) => {
                        self.editor.undo();
                        Ok(())
                    }
                    Some(Action::Redo) => {
                        self.editor.redo();
                        Ok(())
                    }
                    Some(Action::SelectAll) => {
                        self.editor.select_all();
                        Ok(())
                    }
                    Some(Action::Copy) => {
                        if let Some(text) = self.editor.selected_text() {
                            return Some(AppAction::Copy(text.into()));
                        }
                        if self.running {
                            return Some(AppAction::Cancel);
                        }
                        if !self.editor.text().is_empty() {
                            self.editor.select_all();
                            self.editor.delete();
                            Ok(())
                        } else {
                            return Some(AppAction::Quit);
                        }
                    }
                    Some(Action::Paste) => return Some(AppAction::Paste),
                    Some(Action::Complete) => self.complete(),
                    Some(Action::Cancel) => {
                        if self.running {
                            return Some(AppAction::Cancel);
                        }
                        self.editor.select_all();
                        self.editor.delete();
                        Ok(())
                    }
                    Some(Action::DeleteOrQuit) => {
                        if !self.editor.text().is_empty() {
                            self.editor.delete();
                            Ok(())
                        } else {
                            return Some(AppAction::Quit);
                        }
                    }
                    Some(Action::Quit) => return Some(AppAction::Quit),
                    Some(Action::NewSession) => return Some(AppAction::NewSession),
                    Some(Action::Compact) => return Some(AppAction::Compact),
                    Some(Action::Sessions) => {
                        self.session_selector = Some(0);
                        Ok(())
                    }
                    Some(Action::Theme) => {
                        self.theme = if self.theme == Theme::Dark {
                            Theme::Light
                        } else {
                            Theme::Dark
                        };
                        Ok(())
                    }
                    Some(Action::ToggleTools) => {
                        self.show_tools = !self.show_tools;
                        Ok(())
                    }
                    Some(Action::Search) => {
                        self.search_input = Some(self.search_query.clone());
                        Ok(())
                    }
                    Some(Action::PageUp) => {
                        self.scroll(-(self.viewport_height as isize));
                        Ok(())
                    }
                    Some(Action::PageDown) => {
                        self.scroll(self.viewport_height as isize);
                        Ok(())
                    }
                    Some(Action::Top) => {
                        self.scroll_offset = Some(0);
                        Ok(())
                    }
                    Some(Action::Bottom) => {
                        self.scroll_offset = None;
                        Ok(())
                    }
                    None => {
                        if let KeyCode::Char(character) = key.code {
                            if !key
                                .modifiers
                                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                            {
                                self.editor.insert(&character.to_string())
                            } else {
                                Ok(())
                            }
                        } else {
                            Ok(())
                        }
                    }
                };
                if let Err(error) = result {
                    self.status = error;
                }
                None
            }
            _ => None,
        }
    }
}
