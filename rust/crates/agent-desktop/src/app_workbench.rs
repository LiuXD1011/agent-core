//! Native task navigation and inspectable results. All commands still enter the
//! existing transactional workers; artifact reads use a cancellable worker.
use super::*;
use agent_runtime::artifacts::{read_recorded_artifact, ArtifactError, ArtifactPreview};
#[cfg(test)]
use agent_runtime::workbench::TaskProjection;
use agent_runtime::workbench::{
    matches_query, BranchSnapshot, HistoryError, StepOutcome, TaskFilter, TaskPhase,
};
use task_management_ui::GroupCounts;

/// One task row, owned so the list can be drawn while interface state changes.
struct TaskRow {
    id: String,
    name: String,
    workspace: String,
    phase: TaskPhase,
    group: Option<String>,
    archived: bool,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum WorkbenchPage {
    #[default]
    Conversation,
    Tasks,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum InspectorTab {
    #[default]
    Artifacts,
    Execution,
    Overview,
}

#[derive(Default)]
pub(super) struct WorkbenchState {
    pub page: WorkbenchPage,
    inspector_open: bool,
    tab: InspectorTab,
    query: String,
    filter: TaskFilter,
    history_key: Option<HistoryKey>,
    history: Option<Result<BranchSnapshot, HistoryError>>,
    preview: Option<PreviewState>,
    preview_worker: Option<PreviewWorker>,
    queue_open: bool,
    queue_session: String,
    session_queues: BTreeMap<String, SessionQueueState>,
    queue_snapshot: Option<(
        String,
        std::sync::Arc<agent_runtime::runtime::QueueSnapshot>,
    )>,
    pub(super) durable: bool,
    queue_ready: bool,
    queue_load_error: Option<String>,
    queue_io: Option<QueueIo>,
}

struct QueueIo {
    session: String,
    signal: CancellationToken,
    handle: JoinHandle<Result<QueueOutcome, String>>,
}
enum QueueOutcome {
    Loaded(MessageQueues),
    Changed,
    Edited(Message),
    Reserved(Option<Box<(agent_runtime::runtime::QueueItem, Option<Message>)>>),
}
enum QueueAction {
    Pause(bool),
    Resume,
    Remove(QueueItemId),
    Edit(QueueItemId),
    Promote(QueueItemId),
    Reorder(Vec<QueueItemId>),
    Clear(Vec<QueueItemId>),
    Retry(QueueItemId),
    Discard(QueueItemId),
    Reserve,
}

#[derive(Default)]
struct SessionQueueState {
    queues: MessageQueues,
    drafts: Vec<(QueueItemId, Message, String)>,
    halted: bool,
    ready: bool,
}

/// Session history is append-only. Navigating changes leaf_id; flat legacy
/// updates change context length. Explicit refresh also handles external edits.
#[derive(Clone, PartialEq, Eq)]
struct HistoryKey {
    session: String,
    workspace: PathBuf,
    leaf: Option<String>,
    entries: usize,
    messages: usize,
    compactions: usize,
    updated_at: u64,
    tree_version: u32,
}

struct PreviewState {
    key: HistoryKey,
    entry_id: String,
    result: Option<Result<ArtifactPreview, ArtifactError>>,
}

struct PreviewWorker {
    signal: CancellationToken,
    handle: JoinHandle<Result<ArtifactPreview, ArtifactError>>,
}

impl Drop for WorkbenchState {
    fn drop(&mut self) {
        self.cancel_preview();
        if let Some(worker) = &self.queue_io {
            worker.signal.cancel();
        }
    }
}

impl HistoryKey {
    fn new(session: &Session) -> Self {
        Self {
            session: session.id.clone(),
            workspace: session.workspace.clone(),
            leaf: session.leaf_id.clone(),
            entries: session.entries.len(),
            messages: session.context.messages.len(),
            compactions: session.compactions.len(),
            updated_at: session.updated_at,
            tree_version: session.tree_version,
        }
    }
}

impl WorkbenchState {
    fn cancel_preview(&mut self) {
        if let Some(worker) = &self.preview_worker {
            worker.signal.cancel();
        }
        self.preview = None;
    }

    fn poll_preview(&mut self, session: &Session, ctx: &egui::Context) {
        let key = HistoryKey::new(session);
        if !self.inspector_open
            || self.page != WorkbenchPage::Conversation
            || self
                .preview
                .as_ref()
                .is_some_and(|preview| preview.key != key)
        {
            self.cancel_preview();
        }
        if self
            .preview_worker
            .as_ref()
            .is_some_and(|worker| worker.handle.is_finished())
        {
            if let Some(worker) = self.preview_worker.take() {
                let cancelled = worker.signal.is_cancelled();
                let result = worker.handle.join().unwrap_or(Err(ArtifactError::Io));
                if !cancelled {
                    if let Some(preview) = &mut self.preview {
                        preview.result = Some(result);
                    }
                }
            }
        }
        if self.preview_worker.is_some() {
            ctx.request_repaint_after(Duration::from_millis(25));
        }
    }

    fn start_preview(&mut self, session: &Session, entry_id: String, ctx: &egui::Context) {
        // A cancelled worker retains its slot until completion; repeated clicks
        // cannot create an unbounded collection of detached filesystem reads.
        if self.preview_worker.is_some() {
            return;
        }
        self.preview = Some(PreviewState {
            key: HistoryKey::new(session),
            entry_id: entry_id.clone(),
            result: None,
        });
        let session = session.clone();
        let signal = CancellationToken::new();
        let worker_signal = signal.clone();
        let repaint = ctx.clone();
        match std::thread::Builder::new()
            .name("artifact-preview".into())
            .spawn(move || {
                let result = read_recorded_artifact(&session, &entry_id, &worker_signal);
                repaint.request_repaint();
                result
            }) {
            Ok(handle) => self.preview_worker = Some(PreviewWorker { signal, handle }),
            Err(_) => {
                if let Some(preview) = &mut self.preview {
                    preview.result = Some(Err(ArtifactError::Io));
                }
            }
        }
    }

    fn refresh_history(&mut self, session: &Session) {
        let key = HistoryKey::new(session);
        if self.history_key.as_ref() != Some(&key) {
            // Replace errors as well as successful snapshots: never show another
            // branch's stale artifacts after a failed history validation.
            self.history = Some(BranchSnapshot::from_session(session));
            self.history_key = Some(key);
        }
    }
}

impl DesktopApp {
    pub(super) fn queue_store_failed(&mut self, error: String) {
        self.queues.halt_in_memory();
        self.queue_halted = true;
        self.workbench.queue_ready = false;
        self.workbench.queue_load_error = Some(error.clone());
        self.workbench.queue_open = true;
        self.status =
            format!("本地队列保存未确认：{error}。可能已经写入，请重新读取并检查后再操作。");
    }
    pub(super) fn cancel_queue_io(&self) {
        if let Some(worker) = &self.workbench.queue_io {
            worker.signal.cancel();
        }
        if self.workbench.durable {
            self.queues.halt_in_memory();
        }
    }
    pub(super) fn halt_queue(&mut self) {
        if self.workbench.durable {
            self.queues.halt_in_memory();
        } else {
            self.queues.set_paused(true);
        }
    }
    pub(super) fn queue_io_busy(&self) -> bool {
        self.workbench.queue_io.is_some()
            || (self.workbench.durable
                && !self.workbench.queue_ready
                && self.workbench.queue_load_error.is_none())
    }

    pub(super) fn queue_available(&self) -> bool {
        !self.workbench.durable || self.workbench.queue_ready
    }

    pub(super) fn queue_workspace_matches(&self) -> bool {
        fn normalized(path: &str) -> String {
            #[cfg(windows)]
            {
                path.trim()
                    .trim_start_matches(r"\\?\")
                    .replace('/', "\\")
                    .trim_end_matches('\\')
                    .to_lowercase()
            }
            #[cfg(not(windows))]
            {
                path.trim().trim_end_matches('/').to_owned()
            }
        }
        let snapshot = self.queues.snapshot();
        let origin = snapshot
            .workspace
            .as_ref()
            .unwrap_or(&self.session.workspace);
        normalized(&self.workspace) == normalized(&origin.to_string_lossy())
    }

    pub(super) fn enable_durable_queues(&mut self, ctx: &egui::Context) {
        self.workbench.durable = true;
        self.workbench.queue_ready = false;
        self.poll_queue_io(ctx);
    }

    pub(super) fn poll_queue_io(&mut self, ctx: &egui::Context) {
        if self
            .workbench
            .queue_io
            .as_ref()
            .is_some_and(|w| w.handle.is_finished())
        {
            if let Some(worker) = self.workbench.queue_io.take() {
                let result = worker
                    .handle
                    .join()
                    .unwrap_or_else(|_| Err("队列工作线程意外结束".into()));
                if worker.session == self.session.id {
                    match result {
                        Ok(QueueOutcome::Loaded(queues)) => {
                            self.queues = queues;
                            self.queues.set_track_admissions(!self.workbench.durable);
                            self.workbench.queue_ready = true;
                            let snapshot = self.queues.snapshot();
                            self.queue_halted = snapshot.recovery_required;
                            if snapshot.recovery_required {
                                self.workbench.queue_open = true;
                                self.status = "已恢复本地队列；请检查待发送和状态不确定的消息。尚未自动发送。".into();
                            }
                            // A session switch can defer model restoration while
                            // its queue is loading; resume the existing worker path.
                            self.start_model_restore(ctx);
                        }
                        Ok(QueueOutcome::Changed) => {
                            self.status = "队列变更已保存".into();
                        }
                        Ok(QueueOutcome::Edited(message)) => {
                            self.restore_draft(Some(message.text()));
                            if let Message::User(user) = message {
                                self.input_images.extend(
                                    user.content
                                        .into_iter()
                                        .filter(|p| matches!(p, Content::Image { .. })),
                                );
                            }
                            self.workbench.queue_open = false;
                            self.status =
                                "消息已移回输入框；重新发送将重新处理输入。其余队列已暂停。".into();
                        }
                        Ok(QueueOutcome::Reserved(Some(reservation))) => {
                            let (item, raw) = *reservation;
                            if worker.signal.is_cancelled()
                                || self.closing
                                || self.queue_halted
                                || self.signal.is_cancelled()
                                || !self.task_execution_allowed()
                            {
                                self.queues.halt_in_memory();
                                self.queue_halted = true;
                                self.status =
                                    "队列派发已停止；已保留恢复记录，请确认后再重试。".into();
                                self.workbench.queue_snapshot = None;
                                return;
                            }
                            let original = raw.unwrap_or_else(|| item.message.clone());
                            self.pending_draft = Some(original.text());
                            if let Message::User(user) = original {
                                self.pending_images = user
                                    .content
                                    .into_iter()
                                    .filter(|p| matches!(p, Content::Image { .. }))
                                    .collect();
                            }
                            self.pending_queue_id = Some(item.id);
                            self.start_prompt_worker(ctx, item.message, true);
                        }
                        Ok(QueueOutcome::Reserved(None)) => {}
                        Err(error) => {
                            self.queue_store_failed(error);
                        }
                    }
                    self.workbench.queue_snapshot = None;
                } else if let Ok(QueueOutcome::Loaded(queues)) = result {
                    self.workbench.session_queues.insert(
                        worker.session,
                        SessionQueueState {
                            queues,
                            ready: true,
                            halted: true,
                            ..Default::default()
                        },
                    );
                }
            }
        }
        if self.workbench.durable
            && !self.workbench.queue_ready
            && self.workbench.queue_io.is_none()
            && self.workbench.queue_load_error.is_none()
        {
            let path = match self.store.path(&self.session.id) {
                Ok(path) => path,
                Err(error) => {
                    self.status = error.clone();
                    self.queue_halted = true;
                    self.workbench.queue_load_error = Some(error);
                    return;
                }
            };
            let directory = path.with_extension("queue");
            let store = self.store.clone();
            let candidate = self.session.clone();
            let session = self.session.id.clone();
            let owner = session.clone();
            let signal = CancellationToken::new();
            let cancel = signal.clone();
            let repaint = ctx.clone();
            let handle = std::thread::spawn(move || {
                if cancel.is_cancelled() {
                    return Err("队列恢复已取消".into());
                }
                if !path.exists() {
                    store.save(&candidate)?;
                }
                let result = MessageQueues::open_durable(&directory, &session)
                    .map(QueueOutcome::Loaded)
                    .map_err(|e| e.to_string());
                repaint.request_repaint();
                result
            });
            self.workbench.queue_io = Some(QueueIo {
                session: owner,
                signal,
                handle,
            });
        }
        if self.workbench.queue_io.is_some() {
            ctx.request_repaint_after(Duration::from_millis(25));
        }
    }

    fn queue_action(&mut self, ctx: &egui::Context, action: QueueAction) {
        if self.queue_io_busy() || self.closing || !self.queue_available() {
            return;
        }
        if matches!(action, QueueAction::Retry(_) | QueueAction::Discard(_))
            && (self.running || self.queue_receiver.is_some())
        {
            return;
        }
        if matches!(
            action,
            QueueAction::Reserve
                | QueueAction::Promote(_)
                | QueueAction::Resume
                | QueueAction::Pause(false)
        ) && !self.task_execution_allowed()
        {
            self.status = "请先恢复任务或重新读取任务状态，再派发队列".into();
            return;
        }
        if matches!(
            action,
            QueueAction::Resume
                | QueueAction::Promote(_)
                | QueueAction::Reserve
                | QueueAction::Pause(false)
        ) && !self.queue_workspace_matches()
        {
            self.status =
                "队列工作区与当前输入不一致；请移回编辑，在当前工作区重新处理输入。".into();
            return;
        }
        let queues = self.queues.clone();
        let session = self.session.id.clone();
        let signal = CancellationToken::new();
        let cancel = signal.clone();
        let repaint = ctx.clone();
        let handle = std::thread::spawn(move || {
            if cancel.is_cancelled() {
                return Err("队列操作已取消".into());
            }
            let result = (|| {
                match action {
                    QueueAction::Pause(paused) => queues.try_set_paused(paused)?,
                    QueueAction::Resume => queues.resume_recovered()?,
                    QueueAction::Remove(id) => {
                        queues.remove(id)?;
                    }
                    QueueAction::Edit(id) => {
                        let raw = queues.raw_input(id);
                        queues.try_set_paused(true)?;
                        let message = queues.remove(id)?;
                        return Ok(QueueOutcome::Edited(raw.unwrap_or(message)));
                    }
                    QueueAction::Promote(id) => queues.promote_for_dispatch(id)?,
                    QueueAction::Reorder(ids) => queues.reorder(&ids)?,
                    QueueAction::Clear(ids) => {
                        for id in ids {
                            match queues.remove(id) {
                                Ok(_) | Err(agent_runtime::runtime::QueueError::MissingItem) => {}
                                Err(error) => return Err(error),
                            }
                        }
                    }
                    QueueAction::Retry(id) => queues.retry_uncertain(id)?,
                    QueueAction::Discard(id) => {
                        queues.remove_uncertain(id)?;
                    }
                    QueueAction::Reserve => {
                        let item = queues.try_take_next()?;
                        return Ok(QueueOutcome::Reserved(item.map(|item| {
                            let raw = queues.raw_input(item.id);
                            Box::new((item, raw))
                        })));
                    }
                }
                Ok(QueueOutcome::Changed)
            })()
            .map_err(|e: agent_runtime::runtime::QueueError| e.to_string());
            repaint.request_repaint();
            result
        });
        self.workbench.queue_io = Some(QueueIo {
            session,
            signal,
            handle,
        });
    }

    pub(super) fn dispatch_durable_queue(&mut self, ctx: &egui::Context) {
        if !self.queue_available() || !self.task_execution_allowed() {
            return;
        }
        if !self
            .workbench
            .queue_snapshot
            .as_ref()
            .is_some_and(|(session, snapshot)| {
                session == &self.session.id && snapshot.revision == self.queues.revision()
            })
        {
            self.workbench.queue_snapshot = Some((
                self.session.id.clone(),
                std::sync::Arc::new(self.queues.snapshot()),
            ));
        }
        let Some((_, snapshot)) = self.workbench.queue_snapshot.as_ref() else {
            return;
        };
        if snapshot.recovery_required || snapshot.items.is_empty() {
            return;
        }
        if snapshot.paused
            && !snapshot
                .items
                .iter()
                .any(|item| item.lane == QueueLane::Immediate)
        {
            return;
        }
        self.queue_action(ctx, QueueAction::Reserve);
    }

    pub(super) fn restore_pending_draft(&mut self) {
        self.input_images.append(&mut self.pending_images);
        let draft = self.pending_draft.take();
        self.restore_draft(draft);
    }
    pub(super) fn composer_message(&self) -> Message {
        let mut prompt = Message::user(self.input.trim());
        if let Message::User(user) = &mut prompt {
            user.content.extend(self.input_images.clone());
        }
        prompt
    }

    pub(super) fn sync_queue_session(&mut self) {
        if self.workbench.queue_session == self.session.id {
            return;
        }
        if !self.workbench.queue_session.is_empty() {
            self.workbench.session_queues.insert(
                self.workbench.queue_session.clone(),
                SessionQueueState {
                    queues: std::mem::take(&mut self.queues),
                    drafts: std::mem::take(&mut self.queued_drafts),
                    halted: self.queue_halted,
                    ready: self.workbench.queue_ready,
                },
            );
        }
        let saved = self
            .workbench
            .session_queues
            .remove(&self.session.id)
            .unwrap_or_default();
        self.queues = saved.queues;
        self.queues.set_track_admissions(!self.workbench.durable);
        self.queued_drafts = saved.drafts;
        self.queue_halted = saved.halted;
        self.workbench.queue_ready = saved.ready || !self.workbench.durable;
        self.workbench.queue_load_error = None;
        self.workbench.queue_session = self.session.id.clone();
    }

    fn pop_queue_for_edit(&mut self, session: &str, id: QueueItemId) {
        if session != self.session.id
            || !self.input.is_empty()
            || !self.input_images.is_empty()
            || self.queue_receiver.is_some()
        {
            return;
        }
        if self.workbench.durable {
            self.queue_action(&egui::Context::default(), QueueAction::Edit(id));
            return;
        }
        match self.queues.remove(id) {
            Ok(message) => {
                let raw = self
                    .queued_drafts
                    .iter()
                    .position(|(queued_id, _, _)| *queued_id == id)
                    .map(|index| self.queued_drafts.remove(index).2);
                self.input = raw.unwrap_or_else(|| message.text());
                if let Message::User(user) = message {
                    self.input_images = user
                        .content
                        .into_iter()
                        .filter(|part| matches!(part, Content::Image { .. }))
                        .collect();
                }
                self.queues.set_paused(true);
                self.workbench.queue_open = false;
                self.status =
                    "消息已移回输入框，附件保留；重新发送将重新处理输入。其余队列已暂停。".into();
            }
            Err(error) => self.status = error.to_string(),
        }
    }

    pub(super) fn queue_window(&mut self, ctx: &egui::Context) {
        if !self.workbench.queue_open {
            return;
        }
        if !self
            .workbench
            .queue_snapshot
            .as_ref()
            .is_some_and(|(session, snapshot)| {
                session == &self.session.id && snapshot.revision == self.queues.revision()
            })
        {
            self.workbench.queue_snapshot = Some((
                self.session.id.clone(),
                std::sync::Arc::new(self.queues.snapshot()),
            ));
        }
        let Some((_, snapshot)) = self.workbench.queue_snapshot.as_ref() else {
            return;
        };
        let snapshot = snapshot.clone();
        let session = self.session.id.clone();
        let mut open = true;
        let mut edit = None;
        egui::Window::new("消息队列")
            .id(egui::Id::new("managed-message-queue"))
            .open(&mut open).default_width(480.0)
            .max_height((ctx.screen_rect().height() - 120.0).max(200.0)).vscroll(true)
            .show(ctx, |ui| {
                ui.label("当前会话的待发送消息；移出队列表示已交给运行时，不代表模型完成。本地队列在保存后可恢复；状态不确定的消息不会自动重试。");
                ui.label(format!("队列按会话保存，不按分支隔离。原工作区：{}", snapshot.workspace.as_ref().unwrap_or(&self.session.workspace).display()));
                if let Some(error) = &self.workbench.queue_load_error {
                    ui.label(format!("队列恢复失败：{error}"));
                    if ui.add_enabled(!self.running && self.queue_receiver.is_none(), egui::Button::new("重试读取本地队列")).clicked() { self.queues = MessageQueues::default(); self.workbench.queue_load_error = None; }
                }
                ui.horizontal(|ui| {
                    if ui.add_enabled(self.task_execution_allowed() && !self.input.trim().is_empty() && self.queue_receiver.is_none() && !self.compacting && !self.extension_operation, egui::Button::new("将输入加入队列")).clicked() {
                        self.start_queued_input(ctx, QueueLane::FollowUp);
                    }
                    if ui.add_enabled(!self.queue_io_busy() && (!(snapshot.paused || self.queue_halted) || self.task_execution_allowed()), egui::Button::new(if snapshot.paused || self.queue_halted { "恢复队列" } else { "暂停队列" })).clicked() {
                        if self.workbench.durable { self.queue_action(ctx, if snapshot.recovery_required { QueueAction::Resume } else { QueueAction::Pause(!(snapshot.paused || self.queue_halted)) }); } else { self.queues.set_paused(!snapshot.paused); }
                        if !self.workbench.durable || self.workbench.queue_io.is_some() {
                            if snapshot.paused || self.queue_halted { self.queue_halted = false; }
                            if !self.running { self.signal = CancellationToken::new(); }
                        }
                    }
                    if ui.add_enabled(!snapshot.items.is_empty(), egui::Button::new("清空待发送")).clicked() {
                        if self.workbench.durable { self.queue_action(ctx, QueueAction::Clear(snapshot.items.iter().map(|item| item.id).collect())); } else { for item in &snapshot.items {
                            if self.queues.remove(item.id).is_ok() {
                                self.queued_drafts.retain(|(id, _, _)| *id != item.id);
                            }
                        }
                        }
                        self.status = if self.workbench.durable { "正在保存队列清空操作…" } else { "当前列表中仍待发送的消息已清空；已交给运行时的消息不受影响。" }.into();
                    }
                });
                if snapshot.recovery_required { ui.label("重启恢复已暂停所有通道。请检查工作区与任务后再恢复队列。"); }
                for item in &snapshot.uncertain {
                    ui.group(|ui| {
                        ui.label(format!("{}：{}", if self.running { "执行中，等待保存确认" } else { "状态不确定" }, item.message.text().chars().take(200).collect::<String>()));
                        ui.label("上次可能已经执行。仅在确认后重试，以免重复修改文件或调用服务。");
                        ui.add_enabled_ui(!self.running && self.queue_receiver.is_none() && !self.queue_io_busy(), |ui| {
                            if ui.button("确认重试此消息").clicked() { self.queue_action(ctx, QueueAction::Retry(item.id)); }
                            if ui.button("移除此恢复记录").clicked() { self.queue_action(ctx, QueueAction::Discard(item.id)); }
                        });
                    });
                }
                if snapshot.items.is_empty() { ui.label("没有待发送消息。"); }
                for (index, item) in snapshot.items.iter().enumerate() {
                    ui.push_id(item.id.0, |ui| {
                        ui.group(|ui| {
                            ui.label(format!("{} · 待发送", index + 1));
                            let text = item.message.text();
                            let preview: String = text.chars().take(500).collect();
                            ui.add(egui::Label::new(&preview).wrap());
                            if preview.len() < text.len() { ui.small("预览已截断；移回编辑可查看完整输入。"); }
                            if let Message::User(user) = &item.message {
                                let images = user.content.iter().filter(|part| matches!(part, Content::Image { .. })).count();
                                if images > 0 { ui.label(format!("{images} 个图片附件")); }
                            }
                            ui.horizontal_wrapped(|ui| {
                                if ui.add_enabled(self.input.is_empty() && self.input_images.is_empty() && self.queue_receiver.is_none(), egui::Button::new("移回编辑")).on_hover_text("保留已有草稿；输入框为空时可移回原始输入重新编辑、重新处理。").clicked() { edit = Some(item.id); }
                                if ui.button("移除").clicked() {
                                    if self.workbench.durable { self.queue_action(ctx, QueueAction::Remove(item.id)); } else { match self.queues.remove(item.id) {
                                        Ok(_) => { if let Some(index) = self.queued_drafts.iter().position(|(queued_id, _, _)| *queued_id == item.id) { self.queued_drafts.remove(index); } }
                                        Err(error) => self.status = error.to_string(),
                                    } }
                                }
                                for (label, offset) in [("上移", -1isize), ("下移", 1)] {
                                    let target = index.checked_add_signed(offset).filter(|target| *target < snapshot.items.len() && snapshot.items[*target].lane == item.lane);
                                    if ui.add_enabled(target.is_some(), egui::Button::new(label)).clicked() {
                                        if let Some(target) = target {
                                            let mut ids: Vec<_> = snapshot.items.iter().map(|item| item.id).collect(); ids.swap(index, target);
                                            if self.workbench.durable { self.queue_action(ctx, QueueAction::Reorder(ids)); } else if let Err(error) = self.queues.reorder(&ids) { self.status = error.to_string(); }
                                        }
                                    }
                                }
                                if ui.add_enabled(self.task_execution_allowed() && !self.queue_io_busy() && !self.compacting && !self.extension_operation && self.model_receiver.is_none(), egui::Button::new("立即发送")).on_hover_text("优先在下一次运行时消息边界发送，不中断正在执行的工具。").clicked() {
                                    if self.workbench.durable { self.queue_action(ctx, QueueAction::Promote(item.id)); if self.workbench.queue_io.is_some() { self.queue_halted = false; if !self.running { self.signal = CancellationToken::new(); } } } else { match self.queues.promote(item.id) {
                                        Ok(()) => { self.queue_halted = false; if !self.running { self.signal = CancellationToken::new(); } self.status = "已优先排队；等待运行时接收，不代表任务完成。".into(); }
                                        Err(error) => self.status = error.to_string(),
                                    } }
                                }
                            });
                        });
                    });
                }
            });
        self.workbench.queue_open = open;
        if let Some(id) = edit {
            if self.workbench.durable {
                self.queue_action(ctx, QueueAction::Edit(id));
            } else {
                self.pop_queue_for_edit(&session, id);
            }
        }
    }

    pub(super) fn workbench_navigation(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.selectable_value(
                &mut self.workbench.page,
                WorkbenchPage::Conversation,
                "当前对话",
            );
            ui.selectable_value(&mut self.workbench.page, WorkbenchPage::Tasks, "任务中心");
        });
    }

    pub(super) fn workbench_header(&mut self, ui: &mut egui::Ui) {
        if ui
            .button(format!("消息队列 ({})", self.queues.len()))
            .clicked()
        {
            self.workbench.queue_open = !self.workbench.queue_open;
        }
        self.workbench.poll_preview(&self.session, ui.ctx());
        if ui
            .button("任务详情")
            .on_hover_text("产物、执行记录与任务概览")
            .clicked()
        {
            if self.workbench.page == WorkbenchPage::Tasks {
                self.workbench.page = WorkbenchPage::Conversation;
                self.workbench.inspector_open = true;
            } else {
                self.workbench.inspector_open = !self.workbench.inspector_open;
                if !self.workbench.inspector_open {
                    self.workbench.cancel_preview();
                }
            }
        }
    }

    /// Scope, search text and phase — everything except the project filter.
    pub(super) fn task_matches_scope_and_query(&self, session: &Session) -> bool {
        self.task_scope_matches(&session.id)
            && matches_query(session, &self.workbench.query)
            && self.workbench.filter.matches(TaskPhase::from_messages(
                &session.context.messages,
                self.running
                    && !self.extension_operation
                    && !self.compacting
                    && session.id == self.session.id,
            ))
    }

    pub(super) fn task_matches_filters(&self, session: &Session) -> bool {
        self.task_matches_scope_and_query(session) && self.task_group_matches(&session.id)
    }

    /// Per-project counts under the current scope, search and phase filters, so a
    /// chip never promises rows the list would hide.
    pub(super) fn task_group_counts(&self) -> GroupCounts {
        let mut counts = GroupCounts::default();
        for session in std::iter::once(&self.session)
            .chain(self.sessions.iter().filter(|s| s.id != self.session.id))
        {
            if !self.task_matches_scope_and_query(session) {
                continue;
            }
            match self.task_state(&session.id).and_then(|state| state.group) {
                Some(id) => *counts.by_group.entry(id).or_default() += 1,
                None => counts.ungrouped += 1,
            }
        }
        counts
    }

    /// Snapshot of the rows to draw. The live session replaces its disk snapshot
    /// without modifying either.
    fn task_rows(&self) -> Vec<TaskRow> {
        let mut sessions: Vec<&Session> = std::iter::once(&self.session)
            .chain(self.sessions.iter().filter(|s| s.id != self.session.id))
            .collect();
        sessions.sort_by_key(|s| self.task_sort_key(s));
        sessions
            .into_iter()
            .filter(|s| self.task_matches_filters(s))
            .map(|s| TaskRow {
                id: s.id.clone(),
                name: s.name.clone(),
                workspace: s.workspace.to_string_lossy().into_owned(),
                phase: TaskPhase::from_messages(
                    &s.context.messages,
                    self.running
                        && !self.extension_operation
                        && !self.compacting
                        && s.id == self.session.id,
                ),
                group: self.task_state(&s.id).and_then(|state| state.group),
                archived: self.task_state(&s.id).is_some_and(|state| state.archived),
            })
            .collect()
    }

    pub(super) fn tasks_ui(&mut self, ctx: &egui::Context) {
        let palette = shell::palette(ctx);
        let mut selected = None;
        let mut management_action = None;
        let mut toggle: Option<String> = None;
        let mut shown = 0;
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(palette.background).inner_margin(24))
            .show(ctx, |ui| {
                ui.heading("任务中心");
                ui.label(
                    egui::RichText::new("按工作区查找任务，回到上次的执行记录和产物。")
                        .color(palette.muted),
                );
                ui.add_space(16.0);
                ui.add(
                    egui::TextEdit::singleline(&mut self.workbench.query)
                        .hint_text("搜索任务名称或工作区…")
                        .desired_width(f32::INFINITY),
                );
                ui.horizontal(|ui| {
                    for (filter, label) in [
                        (TaskFilter::All, "全部"),
                        (TaskFilter::Completed, "已完成"),
                        (TaskFilter::Attention, "待处理"),
                    ] {
                        ui.selectable_value(&mut self.workbench.filter, filter, label);
                    }
                });
                self.task_management_toolbar(ui);
                if let Some(action) = self.task_batch_bar(ui) {
                    management_action = Some(action);
                }
                ui.add_space(12.0);
                let selection = self.tasks.selection.clone();
                let rows = self.task_rows();
                egui::ScrollArea::vertical()
                    .id_salt("task-center")
                    .show(ui, |ui| {
                        for row in &rows {
                            shown += 1;
                            ui.push_id(&row.id, |ui| {
                                egui::Frame::new()
                                    .fill(palette.surface)
                                    .corner_radius(12)
                                    .inner_margin(14)
                                    .show(ui, |ui| {
                                        ui.set_min_width((ui.available_width() - 1.0).max(0.0));
                                        ui.horizontal(|ui| {
                                            let mut checked = selection.contains(&row.id);
                                            if ui
                                                .add(egui::Checkbox::without_text(&mut checked))
                                                .on_hover_text("选择此任务以进行批量操作")
                                                .changed()
                                            {
                                                toggle = Some(row.id.clone());
                                            }
                                            let width = (ui.available_width() - 100.0).max(40.0);
                                            if ui
                                                .add_enabled_ui(
                                                    !self.busy() || row.id == self.session.id,
                                                    |ui| {
                                                        ui.add_sized(
                                                            [width, 28.0],
                                                            egui::Button::new(&row.name)
                                                                .frame(false)
                                                                .truncate(),
                                                        )
                                                    },
                                                )
                                                .inner
                                                .on_hover_text(&row.name)
                                                .clicked()
                                            {
                                                selected = Some(row.id.clone());
                                            }
                                            ui.label(
                                                RichText::new(row.phase.label())
                                                    .size(11.0)
                                                    .color(palette.muted),
                                            );
                                            if row.archived {
                                                ui.label(
                                                    RichText::new("已归档")
                                                        .size(11.0)
                                                        .color(palette.muted),
                                                );
                                            }
                                        });
                                        ui.horizontal(|ui| {
                                            ui.add(
                                                egui::Label::new(
                                                    RichText::new(&row.workspace)
                                                        .size(12.0)
                                                        .color(palette.muted),
                                                )
                                                .truncate(),
                                            );
                                            if let Some(name) = row
                                                .group
                                                .as_deref()
                                                .and_then(|id| self.task_group_name(id))
                                            {
                                                ui.label(
                                                    RichText::new(format!("· {name}"))
                                                        .size(11.0)
                                                        .color(palette.accent),
                                                );
                                            }
                                        });
                                        if let Some(action) = self.task_row_controls(ui, &row.id) {
                                            management_action = Some(action);
                                        }
                                    });
                                ui.add_space(8.0);
                            });
                        }
                        if shown == 0 {
                            ui.add_space(32.0);
                            ui.label("没有匹配的任务。试试其他名称、工作区、状态或分组筛选。");
                        }
                    });
            });
        if let Some(id) = selected {
            self.resume_workbench_task(ctx, &id);
        }
        if let Some(id) = toggle {
            self.toggle_task_selection(&id);
        }
        if let Some(action) = management_action {
            self.start_task_action(ctx, action);
        }
    }

    fn resume_workbench_task(&mut self, ctx: &egui::Context, id: &str) {
        if id == self.session.id {
            self.workbench.page = WorkbenchPage::Conversation;
        } else if !self.busy() {
            if let Some(session) = self
                .sessions
                .iter()
                .find(|session| session.id == id)
                .cloned()
            {
                self.workbench.page = WorkbenchPage::Conversation;
                self.start_extension_action(
                    ctx,
                    ExtensionAction::Replace(Box::new(session), SessionStartReason::Resume),
                );
            }
        }
    }

    /// Below 1200px the inspector becomes a floating window, so the composer
    /// never loses the space needed by model selection, permissions and Stop.
    pub(super) fn inspector_panel(&mut self, ctx: &egui::Context) {
        if !self.workbench.inspector_open
            || self.workbench.page != WorkbenchPage::Conversation
            || ctx.screen_rect().width() < 1200.0
        {
            return;
        }
        let palette = shell::palette(ctx);
        egui::SidePanel::right("task-inspector")
            .resizable(false)
            .exact_width(300.0)
            .frame(egui::Frame::new().fill(palette.sidebar).inner_margin(16))
            .show(ctx, |ui| self.inspector_contents(ui));
    }

    pub(super) fn inspector_window(&mut self, ctx: &egui::Context) {
        if !self.workbench.inspector_open
            || self.workbench.page != WorkbenchPage::Conversation
            || ctx.screen_rect().width() >= 1200.0
        {
            return;
        }
        let mut open = true;
        egui::Window::new("任务详情")
            .id(egui::Id::new("task-inspector-window"))
            .open(&mut open)
            .default_width(340.0)
            .max_width(420.0)
            .max_height((ctx.screen_rect().height() - 120.0).max(200.0))
            .vscroll(true)
            .show(ctx, |ui| self.inspector_contents(ui));
        self.workbench.inspector_open &= open;
        if !open {
            self.workbench.cancel_preview();
        }
    }

    fn inspector_contents(&mut self, ui: &mut egui::Ui) {
        let palette = shell::palette(ui.ctx());
        ui.horizontal(|ui| {
            ui.strong("任务详情");
            if ui.small_button("刷新记录").clicked() {
                self.workbench.history_key = None;
                self.workbench.cancel_preview();
            }
            if ui.small_button("关闭").clicked() {
                self.workbench.inspector_open = false;
                self.workbench.cancel_preview();
            }
        });
        ui.horizontal(|ui| {
            for (tab, label) in [
                (InspectorTab::Artifacts, "产物"),
                (InspectorTab::Execution, "执行"),
                (InspectorTab::Overview, "概览"),
            ] {
                ui.selectable_value(&mut self.workbench.tab, tab, label);
            }
        });
        ui.separator();
        self.workbench.refresh_history(&self.session);
        let snapshot = match self.workbench.history.as_ref() {
            Some(Ok(snapshot)) => snapshot,
            Some(Err(error)) => {
                ui.label(error.to_string());
                return;
            }
            None => return,
        };
        let projection = &snapshot.projection;
        let phase = if self.running && !self.extension_operation && !self.compacting {
            TaskPhase::Running
        } else {
            projection.phase
        };
        if snapshot.skipped_messages > 0 {
            ui.label(format!(
                "{} 条非标准历史消息未纳入统计。",
                snapshot.skipped_messages
            ));
        }
        let mut preview_entry = None;
        let mut close_preview = false;
        egui::ScrollArea::vertical().id_salt("task-inspector-body").show(ui, |ui| {
            match self.workbench.tab {
                InspectorTab::Artifacts => {
                    ui.label(RichText::new("当前分支完整历史，包含压缩前记录；仅表示工具报告成功，不是磁盘实时快照。").size(11.0).color(palette.muted));
                    if projection.artifacts.is_empty() {
                        ui.add_space(24.0);
                        ui.strong("尚无文件产物");
                        ui.label("工具成功写入或编辑文件后，会在这里显示记录与代码差异。");
                    }
                    for artifact in projection.artifacts.iter().rev().take(100) {
                        let detail = snapshot.artifact_detail(artifact.result_message);
                        ui.push_id(detail.map(|detail| detail.entry_id.as_str()), |ui| {
                            egui::Frame::new().fill(palette.surface).corner_radius(12).inner_margin(10).show(ui, |ui| {
                                ui.add(egui::Label::new(RichText::new(&artifact.path).strong()).wrap());
                                ui.label(RichText::new(format!("{} · 已记录", artifact.operation)).size(11.0).color(palette.muted));
                                if ui.small_button("复制路径").clicked() {
                                    ui.ctx().copy_text(artifact.path.clone());
                                }
                                if let Some(detail) = detail {
                                    if ui.add_enabled(self.workbench.preview_worker.is_none(), egui::Button::new("查看当前文件").small()).clicked() {
                                        preview_entry = Some(detail.entry_id.clone());
                                    }
                                    if let Some(preview) = self.workbench.preview.as_ref().filter(|p| p.entry_id == detail.entry_id) {
                                        ui.separator();
                                        ui.label("磁盘文件快照（非历史版本，非实时监听）");
                                        ui.horizontal(|ui| {
                                            if ui.add_enabled(self.workbench.preview_worker.is_none(), egui::Button::new("刷新预览").small()).clicked() { preview_entry = Some(detail.entry_id.clone()); }
                                            if ui.small_button(if preview.result.is_none() {"取消预览"} else {"关闭预览"}).clicked() { close_preview = true; }
                                        });
                                        match &preview.result {
                                            None => { ui.spinner(); ui.label("正在读取文件…"); }
                                            Some(Err(error)) => { ui.label(error.to_string()); }
                                            Some(Ok(file)) => {
                                                ui.label(format!("{} 字节 · SHA-256 {}", file.byte_len, file.sha256));
                                                egui::ScrollArea::vertical().id_salt(("artifact-preview", &detail.entry_id)).max_height(220.0).show(ui, |ui| {
                                                    ui.add(egui::Label::new(RichText::new(&file.text).monospace().size(11.0)).wrap());
                                                });
                                            }
                                        }
                                    }
                                    ui.add(egui::Label::new(RichText::new(format!("记录：{}", detail.entry_id)).size(10.0).color(palette.muted)).truncate())
                                        .on_hover_text(&detail.entry_id);
                                    if let Some(diff) = &detail.diff {
                                        ui.collapsing("查看差异", |ui| {
                                            ui.add(egui::Label::new(RichText::new(diff).monospace().size(11.0)).wrap());
                                            if detail.diff_truncated { ui.label("差异预览已截断；完整记录见会话树或导出。"); }
                                        });
                                    }
                                }
                            });
                            ui.add_space(8.0);
                        });
                    }
                    if projection.artifacts.len() > 100 { ui.label("显示最近 100 条；完整记录见对话或导出。"); }
                }
                InspectorTab::Execution => {
                    ui.label(format!("{} · {} 次用户输入", phase.label(), projection.user_turns));
                    if projection.steps.is_empty() { ui.label("尚无工具执行记录。"); }
                    for (index, step) in projection.steps.iter().enumerate().rev().take(100) {
                        let outcome = match step.outcome {
                            StepOutcome::Unfinished => "未收到结果",
                            StepOutcome::Succeeded => "成功",
                            StepOutcome::Failed => "失败",
                            StepOutcome::Unmatched => "结果未匹配调用",
                        };
                        ui.group(|ui| {
                            ui.label(format!("{:02}  {} · {outcome}", index + 1, step.tool_name));
                            if let Some(path) = &step.path { ui.add(egui::Label::new(RichText::new(path).size(11.0).color(palette.muted)).wrap()); }
                        });
                    }
                    if projection.steps.len() > 100 { ui.label("显示最近 100 条；完整记录见对话或导出。"); }
                    if self.running && !self.runtime_activity.is_empty() { ui.label(&self.runtime_activity); }
                }
                InspectorTab::Overview => {
                    ui.strong(&self.session.name);
                    ui.label(phase.label());
                    ui.label(format!("{} 次输入 · {} 个工具调用/结果 · {} 条文件变更", projection.user_turns, projection.steps.len(), projection.artifacts.len()));
                    ui.separator();
                    ui.label("工作区");
                    ui.add(egui::Label::new(self.session.workspace.to_string_lossy()).wrap());
                    ui.label(format!("当前模型：{} / {}", self.provider_id(), self.settings.model));
                    ui.label(format!("编辑：{} · 命令：{}", if self.allow_changes {"开启"} else {"关闭"}, if self.allow_commands {"开启"} else {"关闭"}));
                    ui.label(RichText::new("状态来自当前分支的最后消息；后台运行状态不会写入历史。登录和模型配置继续由独立工作线程处理。").size(11.0).color(palette.muted));
                }
            }
        });
        if close_preview {
            self.workbench.cancel_preview();
        } else if let Some(entry) = preview_entry {
            self.workbench.start_preview(&self.session, entry, ui.ctx());
        }
    }
}

#[cfg(test)]
#[path = "app_workbench_tests.rs"]
mod tests;
