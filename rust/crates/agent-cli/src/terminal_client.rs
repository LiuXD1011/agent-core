//! Native terminal host. Worker owns and saves the authoritative session even on UI I/O loss.
use crate::client::{prepare_request_input, Client};
use crate::interrupt::Interrupt;
use crate::terminal_dialog::{DialogAction, TerminalDialog};
use agent_core::types::{timestamp, AgentEvent, CancellationToken, Content, Message, UserMessage};
use agent_runtime::automatic::AutomationHandle;
use agent_runtime::extensions::{
    ExtensionMode, InputSource, InputStreamingBehavior, UiBroker, UiDialogRequest, UiReply,
};
use agent_runtime::resources::ResourceLoad;
use agent_runtime::runtime::{MessageQueues, RunOptions};
use agent_runtime::session::{Session, SessionStore};
use agent_tui::keybindings::Keybindings;
use agent_tui::model::{AppAction, SessionChoice, Theme, TuiModel};
use agent_tui::render::render;
use agent_tui::terminal::{NativeClipboard, NativeTerminal, TerminalMode};
use crossterm::event::Event;
use ratatui::backend::TestBackend;
use ratatui::Terminal;
use std::collections::VecDeque;
use std::fs;
use std::path::Path;
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

pub struct TerminalClient {
    pub model: TuiModel,
    client: Option<Client>,
    worker: Option<JoinHandle<(Client, Result<(), String>)>>,
    events: Option<Receiver<AgentEvent>>,
    signal: Option<CancellationToken>,
    queues: MessageQueues,
    resources: ResourceLoad,
    automation: AutomationHandle,
    recovered_inputs: VecDeque<String>,
    snapshot: Session,
    options: RunOptions,
    preparing: Option<JoinHandle<Result<Option<Message>, String>>>,
    prepare_signal: Option<CancellationToken>,
    preparing_raw: Option<Message>,
    pending_raw: VecDeque<Message>,
    prepared_inputs: VecDeque<(Message, Message)>,
    active_raw: Option<Message>,
    worker_accepts_input: bool,
    stopping: bool,
    ui: UiBroker,
    ui_pending: VecDeque<UiDialogRequest>,
    ui_dialog: Option<TerminalDialog>,
    startup_worker: bool,
    startup_failed: bool,
}

fn send_event(sender: &SyncSender<AgentEvent>, mut event: AgentEvent, signal: &CancellationToken) {
    loop {
        match sender.try_send(event) {
            Ok(()) => break,
            Err(TrySendError::Disconnected(_)) => {
                signal.cancel();
                break;
            }
            Err(TrySendError::Full(returned)) => {
                if signal.is_cancelled() {
                    break;
                }
                event = returned;
                thread::sleep(Duration::from_millis(2));
            }
        }
    }
}

impl TerminalClient {
    pub fn new(mut client: Client) -> Self {
        client.options.extension_mode = ExtensionMode::Tui;
        let ui = UiBroker::new();
        let binding = client.options.extensions.bind_ui(ui.handle());
        let mut host = Self {
            model: TuiModel::default(),
            queues: client.queues.clone(),
            resources: client.resources.clone(),
            automation: client.options.automation_state.clone(),
            recovered_inputs: VecDeque::new(),
            snapshot: client.session.clone(),
            options: client.options.clone(),
            preparing: None,
            prepare_signal: None,
            preparing_raw: None,
            pending_raw: VecDeque::new(),
            prepared_inputs: VecDeque::new(),
            active_raw: None,
            worker_accepts_input: false,
            stopping: false,
            ui,
            ui_pending: VecDeque::new(),
            ui_dialog: None,
            startup_worker: false,
            startup_failed: false,
            client: Some(client),
            worker: None,
            events: None,
            signal: None,
        };
        host.sync_model();
        if let Err(error) = binding {
            host.model.status = error;
        }
        host
    }
    fn poll_dialogs(&mut self) {
        self.ui_pending.extend(self.ui.drain_requests());
        self.ui_pending
            .retain(|request| self.ui.is_pending(&request.id));
        if self
            .ui_dialog
            .as_ref()
            .is_some_and(|dialog| !self.ui.is_pending(&dialog.request.id))
        {
            self.ui_dialog = None;
        }
        if self.ui_dialog.is_none() {
            if let Some(request) = self.ui_pending.pop_front() {
                let id = request.id.clone();
                match TerminalDialog::new(request, self.model.keybindings.clone()) {
                    Ok(dialog) => self.ui_dialog = Some(dialog),
                    Err(error) => {
                        let _ = self.ui.respond(&id, UiReply::cancelled());
                        self.model.status = error;
                    }
                }
            }
        }
        if let Some(dialog) = &mut self.ui_dialog {
            dialog.update_countdown();
        }
    }
    pub fn pending_ui(&self) -> Option<&UiDialogRequest> {
        self.ui_dialog.as_ref().map(|dialog| &dialog.request)
    }
    pub fn respond_ui(&mut self, id: &str, reply: UiReply) -> Result<(), String> {
        self.ui.respond(id, reply)?;
        if self
            .ui_dialog
            .as_ref()
            .is_some_and(|dialog| dialog.request.id == id)
        {
            self.ui_dialog = None;
        }
        Ok(())
    }
    pub fn display_model(&mut self) -> &mut TuiModel {
        self.ui_dialog
            .as_mut()
            .map_or(&mut self.model, |dialog| &mut dialog.model)
    }
    pub fn handle_event(&mut self, event: Event) -> Option<AppAction> {
        if let Some(dialog) = &mut self.ui_dialog {
            let id = dialog.request.id.clone();
            match dialog.handle(event) {
                Some(DialogAction::Reply(reply)) => {
                    if let Err(error) = self.respond_ui(&id, reply) {
                        self.model.status = error;
                    }
                    None
                }
                Some(DialogAction::Clipboard(action)) => Some(action),
                Some(DialogAction::Quit) => Some(AppAction::Quit),
                None => None,
            }
        } else {
            self.model.handle(event)
        }
    }
    fn sync_model(&mut self) {
        if let Some(client) = &self.client {
            self.model.messages = client.session.context.messages.clone();
            self.model.title = format!("Agent Core · {}", client.session.name);
            self.model.model = format!(
                "{}/{}",
                client.provider_id,
                if client.settings.model.is_empty() {
                    "(default)"
                } else {
                    &client.settings.model
                }
            );
            self.resources = client.resources.clone();
            self.queues = client.queues.clone();
            self.automation = client.options.automation_state.clone();
            self.snapshot = client.session.clone();
            self.options = client.options.clone();
            match client.store.as_ref().map(SessionStore::list).transpose() {
                Ok(sessions) => {
                    self.model.sessions = sessions
                        .unwrap_or_default()
                        .into_iter()
                        .map(|session| SessionChoice {
                            id: session.id,
                            name: session.name,
                        })
                        .collect()
                }
                Err(error) => self.model.status = error,
            }
            self.model.commands = vec![
                "/exit",
                "/new",
                "/sessions",
                "/resume",
                "/compact",
                "/tree",
                "/navigate",
                "/fork",
                "/clone",
                "/export-html",
                "/export-jsonl",
                "/theme",
                "/help",
                "/queued",
                "/extensions",
                "/reload",
                "/model",
                "/name",
                "/thinking",
            ]
            .into_iter()
            .map(String::from)
            .chain(
                client
                    .resources
                    .prompts
                    .iter()
                    .map(|prompt| format!("/{}", prompt.name)),
            )
            .chain(
                client
                    .options
                    .extensions
                    .commands()
                    .iter()
                    .map(|command| format!("/{}", command.invocation_name)),
            )
            .collect();
        }
    }
    pub fn start(
        &mut self,
        prompt: Option<Message>,
        signal: CancellationToken,
    ) -> Result<(), String> {
        if self.model.running {
            return Err("A request is already running".into());
        }
        self.start_internal(prompt, signal, false)
    }
    pub fn startup_pending(&self) -> bool {
        self.client
            .as_ref()
            .is_some_and(|client| client.session_start_pending || client.model_notification_pending)
    }
    pub fn start_startup(&mut self, signal: CancellationToken) -> Result<(), String> {
        if !self.startup_pending() {
            return Ok(());
        }
        if self.worker.is_some() {
            return Err("A request is already running".into());
        }
        let mut client = self.client.take().ok_or("Client is not idle")?;
        let cancellation = signal.clone();
        self.worker = Some(thread::spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                client.complete_startup_notifications(&cancellation)
            }))
            .unwrap_or_else(|_| Err("Startup notification worker panicked".into()));
            (client, result)
        }));
        self.signal = Some(signal);
        self.model.running = true;
        self.model.status = "正在初始化扩展…".into();
        self.worker_accepts_input = false;
        self.startup_worker = true;
        Ok(())
    }
    fn start_internal(
        &mut self,
        prompt: Option<Message>,
        signal: CancellationToken,
        prepared: bool,
    ) -> Result<(), String> {
        if self.worker.is_some() {
            return Err("A request is already running".into());
        }
        let accepts_input = prepared
            || prompt.as_ref().is_some_and(|prompt| match prompt {
                Message::User(user) => match user.content.as_slice() {
                    [Content::Text { text }] => !self.is_command(text),
                    _ => true,
                },
                _ => true,
            });
        let mut client = self.client.take().ok_or("Client is not idle")?;
        self.active_raw = if prepared {
            prompt.as_ref().and_then(|prompt| {
                let index = self
                    .prepared_inputs
                    .iter()
                    .position(|(prepared, _)| prepared == prompt)?;
                Some(self.prepared_inputs.remove(index).unwrap().1)
            })
        } else {
            prompt.clone()
        };
        self.snapshot = client.session.clone();
        self.options = client.options.clone();
        let (sender, receiver) = mpsc::sync_channel(64);
        let cancellation = signal.clone();
        let worker = thread::spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match prompt {
                Some(prompt) => {
                    let mut emit = |event| send_event(&sender, event, &cancellation);
                    if prepared {
                        client.run_prepared(prompt, &cancellation, &mut emit)
                    } else {
                        client.run(prompt, &cancellation, &mut emit)
                    }
                }
                None => client.compact(2, &cancellation).map(|_| ()),
            }))
            .unwrap_or_else(|_| Err("Request worker panicked".into()));
            // Metadata/session/model commands commit themselves. A veto must not
            // cause this outer worker to materialize an otherwise unsaved session.
            let result = if accepts_input {
                result.and(client.save())
            } else {
                result
            };
            (client, result)
        });
        self.worker = Some(worker);
        self.worker_accepts_input = accepts_input;
        self.events = Some(receiver);
        if self.signal.is_none() {
            self.signal = Some(signal);
        }
        self.stopping = false;
        self.model.running = true;
        self.model.status = "处理中".into();
        Ok(())
    }
    fn observe(&mut self, event: AgentEvent) {
        if let AgentEvent::MessageEnd { message } = &event {
            if matches!(message, Message::User(_)) {
                self.active_raw = None;
                if let Some(index) = self
                    .prepared_inputs
                    .iter()
                    .position(|(prepared, _)| prepared == message)
                {
                    self.prepared_inputs.remove(index);
                }
            }
        }
        self.model.apply(event);
    }
    fn start_preparation(&mut self) -> Result<(), String> {
        if self.preparing.is_some() || self.stopping || self.pending_raw.is_empty() {
            return Ok(());
        }
        let raw = self.pending_raw.front().unwrap().clone();
        let mut snapshot = self.snapshot.clone();
        snapshot.context.messages = self.model.messages.clone();
        if let Some(partial) = &self.model.partial {
            snapshot
                .context
                .messages
                .push(Message::Assistant(partial.clone()));
        }
        let options = self.options.clone();
        let resources = self.resources.clone();
        let signal = CancellationToken::new();
        let cancellation = signal.clone();
        let prompt = raw.clone();
        let worker = thread::Builder::new()
            .name("agent-tui-input".into())
            .spawn(move || {
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    prepare_request_input(
                        &snapshot,
                        &options,
                        &resources,
                        prompt,
                        InputSource::Interactive,
                        Some(InputStreamingBehavior::Steer),
                        &cancellation,
                    )
                }))
                .unwrap_or_else(|_| Err("Input preparation worker panicked".into()))
            })
            .map_err(|error| error.to_string())?;
        self.pending_raw.pop_front();
        self.preparing_raw = Some(raw);
        self.prepare_signal = Some(signal);
        self.preparing = Some(worker);
        self.model.running = true;
        Ok(())
    }
    pub fn poll(&mut self) -> bool {
        let was_running = self.model.running;
        self.poll_dialogs();
        if !self.stopping
            && self
                .signal
                .as_ref()
                .is_some_and(CancellationToken::is_cancelled)
        {
            self.cancel();
        }
        let events = self
            .events
            .as_ref()
            .map(|events| events.try_iter().take(256).collect::<Vec<_>>())
            .unwrap_or_default();
        for event in events {
            self.observe(event);
        }
        for event in self.automation.drain_events() {
            self.model.status =
                serde_json::to_string(&event).unwrap_or_else(|_| "运行状态更新".into());
        }
        if self.worker.as_ref().is_some_and(JoinHandle::is_finished) {
            // Drain after the completion check so the final events are not lost to a race.
            let events = self
                .events
                .as_ref()
                .map(|events| events.try_iter().collect::<Vec<_>>())
                .unwrap_or_default();
            for event in events {
                self.observe(event);
            }
            match self.worker.take().unwrap().join() {
                Ok((client, result)) => {
                    if self.startup_worker {
                        self.startup_failed = result.is_err() || self.stopping;
                    }
                    // No late runtime event may overwrite the final saved-session status.
                    let _ = self.automation.drain_events();
                    let in_memory = result.is_ok() && client.store.is_none();
                    let selection_committed = result.is_err()
                        && self.options.extension_model != client.options.extension_model;
                    let fork_draft = result.is_ok()
                        && self.active_raw.as_ref().is_some_and(|message| {
                            message.text().split_whitespace().next() == Some("/fork")
                        });
                    if result.is_err() {
                        if let Some(raw) = self.active_raw.take() {
                            self.recover(raw.text());
                        }
                    } else {
                        self.active_raw = None;
                    }
                    let handled = client.input_handled;
                    self.model
                        .finish(client.session.context.messages.clone(), result);
                    if selection_committed {
                        self.model.status =
                            format!("模型选择已保存，通知未完成: {}", self.model.status);
                    }
                    if in_memory {
                        self.model.status = "完成 · 仅保存在内存".into();
                    }
                    if handled {
                        self.model.status = "输入已由扩展处理".into();
                    }
                    if let Some(command) = &client.last_command {
                        self.model.status = command.text.clone();
                        if fork_draft {
                            if let Err(error) = self.model.editor.set_text(command.text.clone()) {
                                self.recover(command.text.clone());
                                self.model.status = error;
                            }
                        }
                        for ui in &command.ui {
                            match ui {
                                agent_runtime::extensions::UiRequest::Notification {
                                    text, ..
                                } => self.model.status.push_str(&format!(" | {text}")),
                                agent_runtime::extensions::UiRequest::SetStatus { key, text } => {
                                    self.model.status.push_str(&format!(
                                        " | {key}: {}",
                                        text.as_deref().unwrap_or("")
                                    ))
                                }
                            }
                        }
                    }
                    let diagnostics = client.options.extensions.drain_diagnostics();
                    if !diagnostics.is_empty() {
                        self.model.status.push_str(&format!(
                            " | {}",
                            diagnostics
                                .iter()
                                .map(|d| format!("{}: {}", d.extension_id, d.error))
                                .collect::<Vec<_>>()
                                .join(" | ")
                        ));
                    }
                    self.client = Some(client);
                    self.sync_model();
                }
                Err(_) => {
                    self.model.running = false;
                    self.model.status = "Request worker failed".into();
                }
            }
            self.events = None;
            self.worker_accepts_input = false;
            self.startup_worker = false;
        }
        if self.preparing.as_ref().is_some_and(JoinHandle::is_finished) {
            let raw = self.preparing_raw.take();
            let result = self
                .preparing
                .take()
                .unwrap()
                .join()
                .unwrap_or_else(|_| Err("Input preparation worker failed".into()));
            let cancelled = self.stopping
                || self
                    .prepare_signal
                    .as_ref()
                    .is_some_and(CancellationToken::is_cancelled);
            self.prepare_signal = None;
            match result {
                Ok(Some(prepared)) if !cancelled => {
                    self.prepared_inputs
                        .push_back((prepared.clone(), raw.unwrap()));
                    self.queues.steer(prepared);
                    self.model.status = "指令已排队".into();
                }
                Ok(None) if !cancelled => self.model.status = "输入已由扩展处理".into(),
                result => {
                    if let Some(raw) = raw {
                        self.recover(raw.text());
                    }
                    if let Err(error) = result {
                        self.model.status = error;
                    }
                }
            }
        }
        if !self.stopping && !self.model.closing {
            if let Err(error) = self.start_preparation() {
                self.model.status = error;
            }
            if self.worker.is_none() {
                if let Some(prompt) = self.queues.pop_next() {
                    let signal = self.signal.clone().unwrap_or_default();
                    if let Err(error) = self.start_internal(Some(prompt), signal, true) {
                        self.model.status = error;
                    }
                }
            }
        }
        let running = self.worker.is_some()
            || self.preparing.is_some()
            || (!self.stopping && !self.pending_raw.is_empty());
        self.model.running = running;
        if !running {
            self.signal = None;
            self.stopping = false;
        }
        was_running && !running
    }
    pub fn cancel(&mut self) {
        if self.signal.is_some() || self.preparing.is_some() || !self.pending_raw.is_empty() {
            self.stopping = true;
            if let Some(signal) = &self.signal {
                signal.cancel();
            }
            if let Some(signal) = &self.prepare_signal {
                signal.cancel();
            }
            let (steering, follow_up) = self.queues.clear();
            for message in steering.into_iter().chain(follow_up) {
                let text = if let Some(index) = self
                    .prepared_inputs
                    .iter()
                    .position(|(prepared, _)| prepared == &message)
                {
                    self.prepared_inputs.remove(index).unwrap().1.text()
                } else {
                    message.text()
                };
                self.recovered_inputs.push_back(text);
            }
            if let Some(raw) = self.preparing_raw.take() {
                self.recovered_inputs.push_back(raw.text());
            }
            self.recovered_inputs
                .extend(self.pending_raw.drain(..).map(|message| message.text()));
            if !self.recovered_inputs.is_empty() {
                if !self.model.editor.text().is_empty() {
                    self.recovered_inputs
                        .push_back(self.model.editor.text().to_owned());
                }
                self.restore_inputs();
            }
            self.model.status = "正在停止并保存".into();
        }
    }
    fn recover(&mut self, text: String) {
        if !self.model.editor.text().is_empty() {
            self.recovered_inputs
                .push_back(self.model.editor.text().to_owned());
        }
        self.recovered_inputs.push_front(text);
        self.restore_inputs();
    }
    fn restore_inputs(&mut self) {
        let mut text = String::new();
        while let Some(next) = self.recovered_inputs.front() {
            let extra = if text.is_empty() { 0 } else { 2 };
            if text.len() + extra + next.len() > agent_tui::editor::MAX_INPUT_BYTES {
                break;
            }
            if extra > 0 {
                text.push_str("\n\n");
            }
            text.push_str(&self.recovered_inputs.pop_front().unwrap());
        }
        if !text.is_empty() {
            if let Err(error) = self.model.editor.set_text(text.clone()) {
                self.recovered_inputs.push_front(text);
                self.model.status = error;
            }
        }
    }
    pub fn has_queued(&self) -> bool {
        !self.queues.is_empty() || !self.pending_raw.is_empty() || self.preparing.is_some()
    }
    pub fn start_queued(&mut self, signal: CancellationToken) -> Result<(), String> {
        if self.worker.is_some() {
            return Err("A request is already running".into());
        }
        if let Some(prompt) = self.queues.pop_next() {
            self.start_internal(Some(prompt), signal, true)
        } else {
            if self.signal.is_none() {
                self.signal = Some(signal);
            }
            self.start_preparation()
        }
    }
    pub fn submit(&mut self, text: &str, signal: CancellationToken) -> Result<(), String> {
        if !self.ui.pending_ids().is_empty() {
            return Err("Respond to the active extension dialog first".into());
        }
        if self.stopping {
            return Err("Wait for cancellation and session save".into());
        }
        if self.model.running {
            if self.worker.is_some() && !self.worker_accepts_input {
                return Err("A model/session operation is running; wait for completion".into());
            }
            if self.is_command(text) {
                return Err("Commands wait until the active request has finished".into());
            }
            if self.queues.len() + self.pending_raw.len() + usize::from(self.preparing.is_some())
                >= 32
            {
                return Err("Terminal pending queue contains 32 prompts".into());
            }
            if text.len() > agent_tui::editor::MAX_INPUT_BYTES {
                return Err("Queued prompt exceeds 1MB".into());
            }
            self.pending_raw.push_back(Message::User(UserMessage {
                content: vec![Content::text(text)],
                timestamp: timestamp(),
            }));
            self.start_preparation()?;
            self.model.status = "正在准备排队指令".into();
            return Ok(());
        }
        let command = text.split_whitespace().next().unwrap_or("");
        let worker_command = matches!(command, "/model" | "/resume" | "/name" | "/thinking")
            || matches!(command, "/new" | "/clone" | "/navigate" | "/fork")
                && self.client.as_ref().is_some_and(|client| {
                    !client.options.extensions.ids().is_empty()
                        || client.session.entries.iter().any(|entry| {
                            entry.entry_type == "thinking_level_change"
                                || (entry.entry_type == "runtime_state"
                                    && entry.field("key").as_str() == Some("model-selection"))
                        })
                });
        if text.trim_start().starts_with('/') && !worker_command && self.command(text)? {
            return Ok(());
        }
        let prompt = self
            .client
            .as_ref()
            .ok_or("Client is not idle")?
            .message(text, &[])?;
        self.start(Some(prompt), signal)
    }
    fn is_command(&self, text: &str) -> bool {
        let Some(text) = text.trim_start().strip_prefix('/') else {
            return false;
        };
        let name = text.split_whitespace().next().unwrap_or("");
        self.options.extensions.contains_command(name)
            || matches!(
                name,
                "exit"
                    | "quit"
                    | "new"
                    | "sessions"
                    | "resume"
                    | "compact"
                    | "tree"
                    | "navigate"
                    | "fork"
                    | "clone"
                    | "export-html"
                    | "export-jsonl"
                    | "theme"
                    | "help"
                    | "queued"
                    | "extensions"
                    | "reload"
                    | "model"
                    | "name"
                    | "thinking"
            )
    }
    fn command(&mut self, text: &str) -> Result<bool, String> {
        let (command, value) = text
            .trim()
            .split_once(char::is_whitespace)
            .unwrap_or((text.trim(), ""));
        let value = value.trim();
        let client = self.client.as_mut().ok_or("Client is not idle")?;
        match command {
            "/exit"|"/quit"=>self.model.closing=true,
            "/new"=>client.new_session()?,
            "/sessions"=>self.model.session_selector=Some(0),
            "/resume"=>client.resume(value)?,
            "/theme"=>self.model.theme=if self.model.theme==Theme::Dark{Theme::Light}else{Theme::Dark},
            "/tree"=>{
                let entries=client.session.get_entries()?;
                self.model.status=entries.iter().map(|entry|format!("{} [{}] parent={}",entry.id,entry.entry_type,entry.parent_id.as_deref().unwrap_or("root"))).collect::<Vec<_>>().join(" | ");
            },
            "/navigate"=>{let mut session=client.session.clone();session.navigate((!value.is_empty()&&value!="root").then_some(value))?;client.replace_selected_branch(session,&CancellationToken::new())?;},
            "/fork"=>{let mut session=client.session.clone();let draft=session.fork(value)?;client.replace_selected_branch(session,&CancellationToken::new())?;self.model.editor.set_text(draft)?;},
            "/clone"=>{let session=client.session.clone_current_branch()?;client.replace_session(session,agent_runtime::extensions::SessionStartReason::Fork,agent_runtime::extensions::SessionShutdownReason::Fork)?;client.save()?;},
            "/export-html"=>{SessionStore::export_html(&client.session,Path::new(value))?;self.model.status=format!("HTML: {value}");},
            "/export-jsonl"=>{SessionStore::export_branch_jsonl(&client.session,Path::new(value))?;self.model.status=format!("JSONL: {value}");},
            "/help"=>self.model.status="/model PROVIDER MODEL /new /sessions /resume ID /tree /navigate ID /fork USER_ID /clone /compact /export-html PATH /export-jsonl PATH /theme /exit".into(),
            "/compact"=>return Ok(false),
            "/queued"=>{self.restore_inputs();self.model.status=format!("待取回草稿: {}",self.recovered_inputs.len());},
            _=>return Ok(false),
        }
        self.sync_model();
        Ok(true)
    }
    pub fn save(&self) -> Result<(), String> {
        self.client.as_ref().map_or(Ok(()), Client::save)
    }
}
impl Drop for TerminalClient {
    fn drop(&mut self) {
        self.ui.close();
        self.cancel();
        self.events = None;
        if let Some(preparing) = self.preparing.take() {
            let _ = preparing.join();
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

pub fn run_terminal(
    client: Client,
    mut initial: Option<Message>,
    mode: TerminalMode,
    keybindings: Option<&Path>,
    interrupt: &Interrupt,
) -> Result<(), String> {
    let mut host = TerminalClient::new(client);
    if let Some(path) = keybindings {
        if fs::metadata(path).map_err(|error| error.to_string())?.len() > 65_536 {
            return Err("Keybinding file exceeds 64KB".into());
        }
        host.model.keybindings = serde_json::from_slice::<Keybindings>(
            &fs::read(path).map_err(|error| error.to_string())?,
        )
        .map_err(|error| error.to_string())?;
        host.model.keybindings.validate()?;
    }
    let mut terminal = NativeTerminal::open(mode).map_err(|error| error.to_string())?;
    let mut clipboard = NativeClipboard::default();
    if host.startup_pending() {
        host.start_startup(interrupt.begin())?;
    } else if let Some(prompt) = initial.take() {
        host.start(Some(prompt), interrupt.begin())?;
    }
    let result = (|| {
        loop {
            if host.poll() {
                interrupt.end();
            }
            if !host.model.running && !host.model.closing {
                if host.startup_failed {
                    if let Some(prompt) = initial.take() {
                        host.recover(prompt.text());
                    }
                } else if let Some(prompt) = initial.take() {
                    host.start(Some(prompt), interrupt.begin())?;
                }
            }
            if !host.model.running && !host.model.closing && host.has_queued() {
                host.start_queued(interrupt.begin())?;
            }
            if host.model.closing && !host.model.running {
                break;
            }
            terminal
                .draw(host.display_model())
                .map_err(|error| error.to_string())?;
            if let Some(event) = NativeTerminal::next_event(Duration::from_millis(30))
                .map_err(|error| error.to_string())?
            {
                if let Some(action) = host.handle_event(event) {
                    let submitted = if let AppAction::Submit(text) = &action {
                        Some(text.clone())
                    } else {
                        None
                    };
                    let result = match action {
                        AppAction::Submit(text) => {
                            if text.trim() == "/compact" {
                                if host.model.running {
                                    Err("Wait for the active request".into())
                                } else {
                                    host.start(None, interrupt.begin())
                                }
                            } else {
                                let was_running = host.model.running;
                                let signal = if was_running {
                                    CancellationToken::new()
                                } else {
                                    interrupt.begin()
                                };
                                let result = host.submit(&text, signal);
                                if !host.model.running {
                                    interrupt.end();
                                }
                                result
                            }
                        }
                        AppAction::Cancel => {
                            host.cancel();
                            Ok(())
                        }
                        AppAction::Quit => {
                            host.model.closing = true;
                            host.cancel();
                            Ok(())
                        }
                        AppAction::NewSession => {
                            if host.model.running {
                                Err("Commands wait until the active request has finished".into())
                            } else {
                                let result = host.submit("/new", interrupt.begin());
                                if !host.model.running {
                                    interrupt.end();
                                }
                                result
                            }
                        }
                        AppAction::Compact => {
                            if host.model.running {
                                Err("Wait for the active request".into())
                            } else {
                                host.start(None, interrupt.begin())
                            }
                        }
                        AppAction::SelectSession(id) => {
                            if host.model.running {
                                Err("Commands wait until the active request has finished".into())
                            } else {
                                let result =
                                    host.submit(&format!("/resume {id}"), interrupt.begin());
                                if !host.model.running {
                                    interrupt.end();
                                }
                                result
                            }
                        }
                        AppAction::Copy(text) => clipboard.copy(&text),
                        AppAction::Paste => clipboard
                            .paste()
                            .and_then(|text| host.display_model().editor.insert(&text)),
                    };
                    if let Err(error) = result {
                        if let Some(text) = submitted {
                            if host.model.editor.text().is_empty() {
                                let _ = host.model.editor.set_text(text);
                            }
                        }
                        host.model.status = error;
                    }
                }
            }
        }
        host.save()
    })();
    host.cancel();
    drop(host);
    interrupt.end();
    terminal.restore();
    result
}

pub fn smoke_test() -> Result<String, String> {
    let mut model = TuiModel::default();
    model
        .editor
        .insert("Rust 原生终端\n组合字符 e\u{301} + 👩‍💻")?;
    model.messages = vec![
        Message::user("离线终端回归"),
        Message::Assistant(agent_core::types::AssistantMessage::text(
            "原生 Rust editor/render/host",
        )),
    ];
    let mut terminal =
        Terminal::new(TestBackend::new(100, 28)).map_err(|error| error.to_string())?;
    terminal
        .draw(|frame| render(frame, &mut model))
        .map_err(|error| error.to_string())?;
    let buffer = terminal.backend().buffer();
    let text = (0..buffer.area.height)
        .map(|y| {
            let mut row = String::new();
            let mut x = 0;
            while x < buffer.area.width {
                let symbol = buffer[(x, y)].symbol();
                row.push_str(symbol);
                x = x.saturating_add(agent_tui::editor::cell_width(symbol).max(1) as u16);
            }
            row
        })
        .collect::<Vec<_>>()
        .join("\n");
    if !text.contains("Agent Core") || !text.contains("Rust") {
        return Err("Terminal frame verification failed".into());
    }
    Ok(format!(
        "PASS: offline Rust TUI, grapheme editor, native 100x28 frame\n{text}"
    ))
}
