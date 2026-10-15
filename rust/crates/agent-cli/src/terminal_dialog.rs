//! Modal state is separate from the submitted prompt editor and transcript.
use agent_runtime::extensions::{UiDialog, UiDialogRequest, UiReply};
use agent_tui::keybindings::{Action, Keybindings};
use agent_tui::model::{AppAction, SessionChoice, TuiModel};
use crossterm::event::{Event, KeyEventKind};
use std::time::{Duration, Instant};

pub enum DialogAction {
    Reply(UiReply),
    Clipboard(AppAction),
    Quit,
}
pub struct TerminalDialog {
    pub request: UiDialogRequest,
    pub model: TuiModel,
    received: Instant,
}
impl TerminalDialog {
    pub fn new(request: UiDialogRequest, keybindings: Keybindings) -> Result<Self, String> {
        let mut model = TuiModel {
            title: request.dialog.title().into(),
            model: "扩展交互".into(),
            keybindings,
            ..Default::default()
        };
        match &request.dialog {
            UiDialog::Select { options, .. } => {
                model.sessions = options
                    .iter()
                    .enumerate()
                    .map(|(index, name)| SessionChoice {
                        id: index.to_string(),
                        name: name.clone(),
                    })
                    .collect();
                model.session_selector = Some(0);
                model.status = "选择一个选项，或取消".into();
            }
            UiDialog::Confirm { message, .. } => {
                model.sessions = vec![
                    SessionChoice {
                        id: "true".into(),
                        name: "是".into(),
                    },
                    SessionChoice {
                        id: "false".into(),
                        name: "否".into(),
                    },
                ];
                model.session_selector = Some(0);
                model.status = message.clone();
            }
            UiDialog::Input { placeholder, .. } => {
                model.status = placeholder.clone().unwrap_or_default()
            }
            UiDialog::Editor { prefill, .. } => {
                model.editor.set_text(prefill.clone().unwrap_or_default())?;
                model.status = format!(
                    "{} 确认 · {} 换行",
                    model.keybindings.label("submit"),
                    model.keybindings.label("newline")
                );
            }
        }
        Ok(Self {
            request,
            model,
            received: Instant::now(),
        })
    }
    pub fn update_countdown(&mut self) {
        if let Some(timeout) = self.request.dialog.timeout() {
            let remaining = Duration::from_millis(timeout).saturating_sub(self.received.elapsed());
            self.model.activity = format!("{:.1}s", remaining.as_secs_f32());
            // Render uses activity only during a running operation, but modal
            // editing must not acquire model-run cancellation/queue semantics.
            self.model.model = format!("扩展交互 · {:.1}s", remaining.as_secs_f32());
        }
    }
    pub fn handle(&mut self, event: Event) -> Option<DialogAction> {
        if let Event::Key(key) = &event {
            if key.kind != KeyEventKind::Release {
                match self.model.keybindings.resolve(*key) {
                    Some(Action::Quit) => return Some(DialogAction::Quit),
                    Some(Action::Cancel) => return Some(DialogAction::Reply(UiReply::cancelled())),
                    Some(Action::Submit)
                        if matches!(
                            self.request.dialog,
                            UiDialog::Input { .. } | UiDialog::Editor { .. }
                        ) =>
                    {
                        return Some(DialogAction::Reply(UiReply::Value {
                            value: self.model.editor.text().into(),
                        }));
                    }
                    _ => {}
                }
            }
        }
        match self.model.handle(event)? {
            AppAction::SelectSession(id) => match &self.request.dialog {
                UiDialog::Select { options, .. } => id
                    .parse::<usize>()
                    .ok()
                    .and_then(|index| options.get(index))
                    .map(|value| {
                        DialogAction::Reply(UiReply::Value {
                            value: value.clone(),
                        })
                    }),
                UiDialog::Confirm { .. } => Some(DialogAction::Reply(UiReply::Confirmed {
                    confirmed: id == "true",
                })),
                _ => None,
            },
            AppAction::Copy(text) => Some(DialogAction::Clipboard(AppAction::Copy(text))),
            AppAction::Paste => Some(DialogAction::Clipboard(AppAction::Paste)),
            AppAction::Quit | AppAction::Cancel => Some(DialogAction::Reply(UiReply::cancelled())),
            // Session/model shortcuts never escape a modal into the host.
            _ => None,
        }
    }
}
