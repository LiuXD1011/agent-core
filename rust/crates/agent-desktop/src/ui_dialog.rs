//! Real modal presentation for a worker's synchronous extension UI request.
use agent_runtime::extensions::{UiBroker, UiDialog, UiDialogRequest, UiReply};
use eframe::egui;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

struct ActiveDialog {
    request: UiDialogRequest,
    text: String,
    focus: bool,
    received: Instant,
}

pub struct DesktopDialogs {
    broker: UiBroker,
    queued: VecDeque<UiDialogRequest>,
    active: Option<ActiveDialog>,
}
impl Default for DesktopDialogs {
    fn default() -> Self {
        Self {
            broker: UiBroker::new(),
            queued: VecDeque::new(),
            active: None,
        }
    }
}
impl DesktopDialogs {
    pub fn broker(&self) -> UiBroker {
        self.broker.clone()
    }
    pub fn has_pending(&self) -> bool {
        !self.broker.pending_ids().is_empty()
    }
    pub fn active_request(&self) -> Option<&UiDialogRequest> {
        self.active.as_ref().map(|active| &active.request)
    }
    pub fn respond(&mut self, id: &str, reply: UiReply) -> Result<(), String> {
        self.broker.respond(id, reply)?;
        if self
            .active
            .as_ref()
            .is_some_and(|active| active.request.id == id)
        {
            self.active = None;
        }
        Ok(())
    }
    pub fn close(&mut self) {
        self.broker.close();
        self.queued.clear();
        self.active = None;
    }
    fn poll(&mut self) {
        self.queued.extend(self.broker.drain_requests());
        self.queued
            .retain(|request| self.broker.is_pending(&request.id));
        if self
            .active
            .as_ref()
            .is_some_and(|dialog| !self.broker.is_pending(&dialog.request.id))
        {
            self.active = None;
        }
        if self.active.is_none() {
            if let Some(request) = self.queued.pop_front() {
                let text = match &request.dialog {
                    UiDialog::Editor { prefill, .. } => prefill.clone().unwrap_or_default(),
                    _ => String::new(),
                };
                self.active = Some(ActiveDialog {
                    request,
                    text,
                    focus: true,
                    received: Instant::now(),
                });
            }
        }
    }
    pub fn show(
        &mut self,
        ctx: &egui::Context,
        submit: egui::KeyboardShortcut,
    ) -> Result<(), String> {
        self.poll();
        let Some(active) = &mut self.active else {
            return Ok(());
        };
        let mut reply = None;
        let modal = egui::Modal::new(egui::Id::new(("extension-dialog", &active.request.id))).show(
            ctx,
            |ui| {
                ui.set_min_width(360.0_f32.min(ui.available_width()));
                ui.set_max_width(640.0);
                ui.heading(active.request.dialog.title());
                if let Some(timeout) = active.request.dialog.timeout() {
                    let remaining =
                        Duration::from_millis(timeout).saturating_sub(active.received.elapsed());
                    ui.label(format!("自动关闭: {:.1}s", remaining.as_secs_f32()));
                }
                match &active.request.dialog {
                    UiDialog::Select { options, .. } => {
                        egui::ScrollArea::vertical()
                            .max_height(300.0)
                            .show(ui, |ui| {
                                for option in options {
                                    if ui.button(option).clicked() {
                                        reply = Some(UiReply::Value {
                                            value: option.clone(),
                                        });
                                    }
                                }
                            });
                    }
                    UiDialog::Confirm { message, .. } => {
                        ui.label(message);
                        ui.horizontal(|ui| {
                            if ui.button("是").clicked() {
                                reply = Some(UiReply::Confirmed { confirmed: true });
                            }
                            if ui.button("否").clicked() {
                                reply = Some(UiReply::Confirmed { confirmed: false });
                            }
                        });
                    }
                    UiDialog::Input { placeholder, .. }
                    | UiDialog::Editor {
                        prefill: placeholder,
                        ..
                    } => {
                        let before = active.text.clone();
                        let edit = if matches!(active.request.dialog, UiDialog::Editor { .. }) {
                            egui::TextEdit::multiline(&mut active.text).desired_rows(10)
                        } else {
                            egui::TextEdit::singleline(&mut active.text)
                        }
                        .desired_width(600.0)
                        .hint_text(placeholder.as_deref().unwrap_or(""))
                        .char_limit(1_048_576);
                        let response = ui.add(edit);
                        if active.text.len() > 1_048_576 {
                            active.text = before;
                            ui.label("输入上限为 1MiB");
                        }
                        if active.focus {
                            response.request_focus();
                            active.focus = false;
                        }
                        if ui.button("确定").clicked()
                            || ui.input_mut(|input| input.consume_shortcut(&submit))
                        {
                            reply = Some(UiReply::Value {
                                value: active.text.clone(),
                            });
                        }
                    }
                }
                if ui.button("取消").clicked() {
                    reply = Some(UiReply::cancelled());
                }
            },
        );
        if modal.should_close() {
            reply = Some(UiReply::cancelled());
        }
        ctx.request_repaint_after(Duration::from_millis(50));
        if let Some(reply) = reply {
            let id = active.request.id.clone();
            self.broker.respond(&id, reply)?;
            self.active = None;
        }
        Ok(())
    }
}
impl Drop for DesktopDialogs {
    fn drop(&mut self) {
        self.close();
    }
}
