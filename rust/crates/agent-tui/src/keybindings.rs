use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Submit,
    NewLine,
    Left,
    Right,
    Up,
    Down,
    WordLeft,
    WordRight,
    Home,
    End,
    Backspace,
    Delete,
    KillWordLeft,
    KillWordRight,
    KillStart,
    KillEnd,
    Yank,
    YankPop,
    Undo,
    Redo,
    SelectAll,
    Copy,
    Paste,
    Complete,
    Cancel,
    Quit,
    DeleteOrQuit,
    NewSession,
    Sessions,
    Compact,
    Theme,
    ToggleTools,
    Search,
    PageUp,
    PageDown,
    Top,
    Bottom,
}

pub struct Binding {
    pub name: &'static str,
    pub action: Action,
    pub keys: &'static [&'static str],
}

pub const DEFAULT_EDITOR_KEYBINDINGS: &[Binding] = &[
    Binding {
        name: "submit",
        action: Action::Submit,
        keys: &["enter"],
    },
    Binding {
        name: "newline",
        action: Action::NewLine,
        keys: &["shift+enter", "ctrl+j", "alt+enter"],
    },
    Binding {
        name: "left",
        action: Action::Left,
        keys: &["left", "ctrl+b", "shift+left"],
    },
    Binding {
        name: "right",
        action: Action::Right,
        keys: &["right", "ctrl+f", "shift+right"],
    },
    Binding {
        name: "up",
        action: Action::Up,
        keys: &["up", "shift+up"],
    },
    Binding {
        name: "down",
        action: Action::Down,
        keys: &["down", "shift+down"],
    },
    Binding {
        name: "word_left",
        action: Action::WordLeft,
        keys: &["ctrl+left", "alt+b"],
    },
    Binding {
        name: "word_right",
        action: Action::WordRight,
        keys: &["ctrl+right", "alt+f"],
    },
    Binding {
        name: "home",
        action: Action::Home,
        keys: &["home", "ctrl+a", "shift+home"],
    },
    Binding {
        name: "end",
        action: Action::End,
        keys: &["end", "ctrl+e", "shift+end"],
    },
    Binding {
        name: "backspace",
        action: Action::Backspace,
        keys: &["backspace"],
    },
    Binding {
        name: "delete",
        action: Action::Delete,
        keys: &["delete"],
    },
    Binding {
        name: "kill_word_left",
        action: Action::KillWordLeft,
        keys: &["ctrl+w", "alt+backspace"],
    },
    Binding {
        name: "kill_word_right",
        action: Action::KillWordRight,
        keys: &["alt+d", "alt+delete"],
    },
    Binding {
        name: "kill_start",
        action: Action::KillStart,
        keys: &["ctrl+u"],
    },
    Binding {
        name: "kill_end",
        action: Action::KillEnd,
        keys: &["ctrl+k"],
    },
    Binding {
        name: "yank",
        action: Action::Yank,
        keys: &["ctrl+y"],
    },
    Binding {
        name: "yank_pop",
        action: Action::YankPop,
        keys: &["alt+y"],
    },
    Binding {
        name: "undo",
        action: Action::Undo,
        keys: &["ctrl+-", "ctrl+z"],
    },
    Binding {
        name: "redo",
        action: Action::Redo,
        keys: &["ctrl+shift+z"],
    },
    Binding {
        name: "select_all",
        action: Action::SelectAll,
        keys: &["ctrl+shift+a"],
    },
    Binding {
        name: "copy",
        action: Action::Copy,
        keys: &["ctrl+c"],
    },
    Binding {
        name: "paste",
        action: Action::Paste,
        keys: &["ctrl+v"],
    },
    Binding {
        name: "complete",
        action: Action::Complete,
        keys: &["tab"],
    },
];
pub const DEFAULT_APP_KEYBINDINGS: &[Binding] = &[
    Binding {
        name: "cancel",
        action: Action::Cancel,
        keys: &["escape"],
    },
    Binding {
        name: "quit",
        action: Action::Quit,
        keys: &["ctrl+q"],
    },
    Binding {
        name: "delete_or_quit",
        action: Action::DeleteOrQuit,
        keys: &["ctrl+d"],
    },
    Binding {
        name: "new_session",
        action: Action::NewSession,
        keys: &["ctrl+n"],
    },
    Binding {
        name: "sessions",
        action: Action::Sessions,
        keys: &["ctrl+r"],
    },
    Binding {
        name: "compact",
        action: Action::Compact,
        keys: &["ctrl+alt+c"],
    },
    Binding {
        name: "theme",
        action: Action::Theme,
        keys: &["ctrl+t"],
    },
    Binding {
        name: "toggle_tools",
        action: Action::ToggleTools,
        keys: &["ctrl+o"],
    },
    Binding {
        name: "search",
        action: Action::Search,
        keys: &["ctrl+shift+f"],
    },
    Binding {
        name: "page_up",
        action: Action::PageUp,
        keys: &["pageup"],
    },
    Binding {
        name: "page_down",
        action: Action::PageDown,
        keys: &["pagedown"],
    },
    Binding {
        name: "top",
        action: Action::Top,
        keys: &["ctrl+home"],
    },
    Binding {
        name: "bottom",
        action: Action::Bottom,
        keys: &["ctrl+end"],
    },
];

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Keybindings {
    pub overrides: BTreeMap<String, Vec<String>>,
}

fn key_id(key: KeyEvent) -> String {
    let mut parts = vec![];
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        parts.push("ctrl".to_string());
    }
    if key.modifiers.contains(KeyModifiers::ALT) {
        parts.push("alt".to_string());
    }
    if key.modifiers.contains(KeyModifiers::SHIFT) {
        parts.push("shift".to_string());
    }
    parts.push(match key.code {
        KeyCode::Char(character) => character.to_lowercase().to_string(),
        KeyCode::Enter => "enter".into(),
        KeyCode::Esc => "escape".into(),
        KeyCode::Tab => "tab".into(),
        KeyCode::BackTab => "backtab".into(),
        KeyCode::Left => "left".into(),
        KeyCode::Right => "right".into(),
        KeyCode::Up => "up".into(),
        KeyCode::Down => "down".into(),
        KeyCode::Home => "home".into(),
        KeyCode::End => "end".into(),
        KeyCode::Delete => "delete".into(),
        KeyCode::Backspace => "backspace".into(),
        KeyCode::PageUp => "pageup".into(),
        KeyCode::PageDown => "pagedown".into(),
        KeyCode::F(number) => format!("f{number}"),
        _ => return String::new(),
    });
    parts.join("+")
}

impl Keybindings {
    pub fn label(&self, name: &str) -> String {
        if let Some(keys) = self.overrides.get(name) {
            return keys.first().cloned().unwrap_or_else(|| "未绑定".into());
        }
        DEFAULT_EDITOR_KEYBINDINGS
            .iter()
            .chain(DEFAULT_APP_KEYBINDINGS)
            .find(|binding| binding.name == name)
            .and_then(|binding| binding.keys.first())
            .copied()
            .unwrap_or("未绑定")
            .to_owned()
    }
    pub fn validate(&self) -> Result<(), String> {
        let mut claimed = BTreeMap::new();
        for (name, keys) in &self.overrides {
            if !DEFAULT_EDITOR_KEYBINDINGS
                .iter()
                .chain(DEFAULT_APP_KEYBINDINGS)
                .any(|binding| binding.name == name)
            {
                return Err(format!("Unknown keybinding: {name}"));
            }
            for key in keys {
                let key = key.to_lowercase();
                let mut code = key.as_str();
                for modifier in ["ctrl+", "alt+", "shift+"] {
                    if let Some(rest) = code.strip_prefix(modifier) {
                        code = rest;
                    }
                }
                let named = matches!(
                    code,
                    "enter"
                        | "escape"
                        | "tab"
                        | "backtab"
                        | "left"
                        | "right"
                        | "up"
                        | "down"
                        | "home"
                        | "end"
                        | "delete"
                        | "backspace"
                        | "pageup"
                        | "pagedown"
                ) || code
                    .strip_prefix('f')
                    .and_then(|number| number.parse::<u8>().ok())
                    .is_some_and(|number| (1..=24).contains(&number));
                if !named && (code.chars().count() != 1 || code.chars().any(char::is_control)) {
                    return Err(format!(
                        "Invalid keybinding: {key}; use ctrl+alt+shift+KEY order"
                    ));
                }
                if let Some(previous) = claimed.insert(key.clone(), name) {
                    if previous != name {
                        return Err(format!("Keybinding conflict: {key} ({previous}, {name})"));
                    }
                }
            }
        }
        Ok(())
    }
    pub fn resolve(&self, key: KeyEvent) -> Option<Action> {
        let id = key_id(key);
        let definitions = || {
            DEFAULT_EDITOR_KEYBINDINGS
                .iter()
                .chain(DEFAULT_APP_KEYBINDINGS)
        };
        // User claims take precedence over defaults, but conflicting user claims are rejected.
        for (name, keys) in &self.overrides {
            if keys.iter().any(|key| key.to_lowercase() == id) {
                return definitions()
                    .find(|binding| binding.name == name)
                    .map(|binding| binding.action);
            }
        }
        definitions()
            .find(|binding| {
                !self.overrides.contains_key(binding.name) && binding.keys.contains(&id.as_str())
            })
            .map(|binding| binding.action)
    }
}
