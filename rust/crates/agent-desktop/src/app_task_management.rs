//! Durable task organization is independent of conversation execution history.
use super::*;
use agent_runtime::task_store::{BatchAction, TaskCatalog, TaskChange, TaskState};

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum TaskScope {
    #[default]
    Active,
    Pinned,
    Archived,
    All,
}
/// Which project the list is limited to. Organizing state only: it filters the
/// list and never stops execution, changes messages or touches the queue.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) enum GroupFilter {
    #[default]
    All,
    Ungrouped,
    Group(String),
}
/// Chip counts over the tasks that pass scope, search and phase, so a chip never
/// promises a row the list would hide.
#[derive(Default)]
pub(super) struct GroupCounts {
    pub ungrouped: usize,
    pub by_group: std::collections::BTreeMap<String, usize>,
}
#[derive(Default)]
pub(super) struct TaskManagement {
    enabled: bool,
    catalog: Option<TaskCatalog>,
    worker: Option<TaskWorker>,
    error: Option<String>,
    pub scope: TaskScope,
    /// Tasks ticked for a batch action. Interface state only; never persisted.
    pub selection: std::collections::BTreeSet<String>,
    pub group_filter: GroupFilter,
    pub group_open: bool,
    pub group_name: String,
    /// Set while the group form renames instead of creating.
    pub group_rename: Option<String>,
    pub group_file_selection: bool,
}
struct TaskWorker {
    signal: CancellationToken,
    target: Option<String>,
    block_dispatch: bool,
    /// What the committed operation did, quoted in the status line.
    committed: Option<String>,
    handle: JoinHandle<Result<TaskCatalog, String>>,
}
#[derive(Clone)]
pub(super) enum TaskAction {
    Change(String, TaskChange),
    Move(String, bool),
    Batch(BatchAction),
    CreateGroup { name: String, with_selection: bool },
    RenameGroup { id: String, name: String },
    DeleteGroup(String),
    MoveGroup(String, bool),
}
/// The store call a worker runs, resolved from a `TaskAction` before spawning.
enum TaskJob {
    Update {
        id: String,
        change: TaskChange,
    },
    ReorderPinned {
        order: Vec<String>,
    },
    Batch {
        ids: Vec<String>,
        action: BatchAction,
    },
    CreateGroup {
        name: String,
        ids: Vec<String>,
    },
    RenameGroup {
        id: String,
        name: String,
    },
    DeleteGroup {
        id: String,
    },
    ReorderGroups {
        ids: Vec<String>,
    },
}
fn batch_label(action: &BatchAction, count: usize) -> String {
    match action {
        BatchAction::Archive => format!("已归档 {count} 项任务"),
        BatchAction::Restore => format!("已恢复 {count} 项任务"),
        BatchAction::Pin => format!("已置顶 {count} 项任务"),
        BatchAction::Unpin => format!("已取消置顶 {count} 项任务"),
        BatchAction::MoveToGroup(_) => format!("已把 {count} 项任务移入分组"),
        BatchAction::RemoveFromGroup => format!("已把 {count} 项任务移出分组"),
    }
}
impl DesktopApp {
    pub(super) fn enable_task_management(&mut self, ctx: &egui::Context) {
        self.tasks.enabled = true;
        self.reload_task_management(ctx);
    }
    fn reload_task_management(&mut self, ctx: &egui::Context) {
        if self.tasks.worker.is_some() || self.closing {
            return;
        }
        self.tasks.catalog = None;
        self.tasks.error = None;
        let store = self.store.task_store();
        let signal = CancellationToken::new();
        let cancel = signal.clone();
        let repaint = ctx.clone();
        self.tasks.worker = Some(TaskWorker {
            signal,
            target: None,
            block_dispatch: false,
            committed: None,
            handle: std::thread::spawn(move || {
                let result = if cancel.is_cancelled() {
                    Err("任务状态读取已取消".into())
                } else {
                    store.load_catalog().map_err(|e| e.to_string())
                };
                repaint.request_repaint();
                result
            }),
        });
    }
    pub(super) fn task_execution_allowed(&self) -> bool {
        if !self.tasks.enabled {
            return true;
        }
        self.tasks.catalog.as_ref().is_some_and(|catalog| {
            catalog
                .states
                .get(&self.session.id)
                .is_none_or(|state| !state.archived)
        }) && !self.tasks.worker.as_ref().is_some_and(|job| {
            job.block_dispatch && job.target.as_deref() == Some(&self.session.id)
        })
    }
    pub(super) fn task_state(&self, id: &str) -> Option<TaskState> {
        if !self.tasks.enabled {
            return Some(TaskState::default());
        }
        self.tasks
            .catalog
            .as_ref()
            .map(|catalog| catalog.states.get(id).cloned().unwrap_or_default())
    }
    pub(super) fn task_group_name(&self, id: &str) -> Option<String> {
        self.tasks
            .catalog
            .as_ref()?
            .groups
            .iter()
            .find(|group| group.id == id)
            .map(|group| group.name.clone())
    }
    pub(super) fn task_scope_matches(&self, id: &str) -> bool {
        match self.task_state(id) {
            Some(state) => match self.tasks.scope {
                TaskScope::Active => !state.archived,
                TaskScope::Pinned => state.pinned && !state.archived,
                TaskScope::Archived => state.archived,
                TaskScope::All => true,
            },
            None => self.tasks.scope == TaskScope::All,
        }
    }
    pub(super) fn task_group_matches(&self, id: &str) -> bool {
        match self.task_state(id) {
            Some(state) => match &self.tasks.group_filter {
                GroupFilter::All => true,
                GroupFilter::Ungrouped => state.group.is_none(),
                GroupFilter::Group(group) => state.group.as_deref() == Some(group.as_str()),
            },
            None => matches!(self.tasks.group_filter, GroupFilter::All),
        }
    }
    pub(super) fn task_sort_key(&self, session: &Session) -> (usize, std::cmp::Reverse<u64>) {
        (
            self.task_state(&session.id)
                .and_then(|s| s.pinned_rank)
                .unwrap_or(usize::MAX),
            std::cmp::Reverse(session.updated_at),
        )
    }
    pub(super) fn task_action_allowed(&self, _id: &str, _archive: bool) -> bool {
        self.task_group_action_allowed()
    }
    /// Organizing actions need a loaded catalog and no worker in flight.
    pub(super) fn task_group_action_allowed(&self) -> bool {
        self.tasks.enabled
            && self.tasks.catalog.is_some()
            && self.tasks.worker.is_none()
            && !self.closing
    }
    /// Selected tasks in list order, so a batch pin keeps the order the user sees.
    /// Ids that no longer exist are ignored instead of failing the whole batch.
    pub(super) fn selected_ids_in_order(&self) -> Vec<String> {
        if self.tasks.selection.is_empty() {
            return Vec::new();
        }
        let mut sessions: Vec<&Session> = std::iter::once(&self.session)
            .chain(self.sessions.iter().filter(|s| s.id != self.session.id))
            .collect();
        sessions.sort_by_key(|s| self.task_sort_key(s));
        sessions
            .into_iter()
            .filter(|s| self.tasks.selection.contains(&s.id))
            .map(|s| s.id.clone())
            .collect()
    }
    /// Ticking a row adds it to the batch selection; ticking it again removes it.
    pub(super) fn toggle_task_selection(&mut self, id: &str) {
        if !self.tasks.selection.remove(id) {
            self.tasks.selection.insert(id.into());
        }
    }
    fn prune_task_selection(&mut self) {
        let known: std::collections::BTreeSet<String> = std::iter::once(self.session.id.clone())
            .chain(self.sessions.iter().map(|session| session.id.clone()))
            .collect();
        self.tasks.selection.retain(|id| known.contains(id));
    }
    fn prune_group_filter(&mut self) {
        if let GroupFilter::Group(id) = &self.tasks.group_filter {
            let exists = self
                .tasks
                .catalog
                .as_ref()
                .is_some_and(|catalog| catalog.groups.iter().any(|group| &group.id == id));
            if !exists {
                self.tasks.group_filter = GroupFilter::All;
            }
        }
    }
    pub(super) fn start_task_action(&mut self, ctx: &egui::Context, action: TaskAction) {
        if !self.task_group_action_allowed() {
            return;
        }
        let Some(catalog) = self.tasks.catalog.as_ref() else {
            return;
        };
        let revision = catalog.revision;
        let mut target: Option<String> = None;
        let mut block_dispatch = false;
        let mut committed: Option<String> = None;
        let job = match &action {
            TaskAction::Change(id, change) => {
                target = Some(id.clone());
                block_dispatch = matches!(change, TaskChange::SetArchived(true));
                if matches!(change, TaskChange::SetArchived(true)) {
                    committed = Some("任务已归档".into());
                } else if matches!(change, TaskChange::SetArchived(false)) {
                    committed = Some("任务已恢复".into());
                }
                TaskJob::Update {
                    id: id.clone(),
                    change: *change,
                }
            }
            TaskAction::Move(id, down) => {
                let mut order = catalog.pinned_ids.clone();
                let visible: Vec<_> = order
                    .iter()
                    .filter(|key| {
                        let session = if *key == &self.session.id {
                            Some(&self.session)
                        } else {
                            self.sessions.iter().find(|s| &s.id == *key)
                        };
                        session.is_some_and(|session| self.task_matches_filters(session))
                    })
                    .cloned()
                    .collect();
                let Some(index) = visible.iter().position(|key| key == id) else {
                    return;
                };
                let next = if *down {
                    index.checked_add(1).filter(|&n| n < visible.len())
                } else {
                    index.checked_sub(1)
                };
                let Some(next) = next else {
                    return;
                };
                let Some(a) = order.iter().position(|key| key == id) else {
                    return;
                };
                let Some(b) = order.iter().position(|key| key == &visible[next]) else {
                    return;
                };
                order.swap(a, b);
                committed = Some("已调整置顶顺序".into());
                TaskJob::ReorderPinned { order }
            }
            TaskAction::Batch(batch) => {
                let ids = self.selected_ids_in_order();
                if ids.is_empty() {
                    return;
                }
                let touches_current = ids.contains(&self.session.id);
                target = touches_current.then(|| self.session.id.clone());
                block_dispatch =
                    touches_current && matches!(batch, BatchAction::Archive | BatchAction::Restore);
                committed = Some(batch_label(batch, ids.len()));
                TaskJob::Batch {
                    ids,
                    action: batch.clone(),
                }
            }
            TaskAction::CreateGroup {
                name,
                with_selection,
            } => {
                let ids = if *with_selection {
                    let ids = self.selected_ids_in_order();
                    if ids.is_empty() {
                        return;
                    }
                    ids
                } else {
                    Vec::new()
                };
                let touches_current = ids.contains(&self.session.id);
                target = touches_current.then(|| self.session.id.clone());
                committed = Some(if ids.is_empty() {
                    format!("已创建分组「{}」", name.trim())
                } else {
                    format!("已创建分组「{}」并移入 {} 项任务", name.trim(), ids.len())
                });
                TaskJob::CreateGroup {
                    name: name.clone(),
                    ids,
                }
            }
            TaskAction::RenameGroup { id, name } => {
                committed = Some(format!("已重命名分组「{}」", name.trim()));
                TaskJob::RenameGroup {
                    id: id.clone(),
                    name: name.clone(),
                }
            }
            TaskAction::DeleteGroup(id) => {
                committed = Some("已删除分组；组内任务保留并回到未分组".into());
                TaskJob::DeleteGroup { id: id.clone() }
            }
            TaskAction::MoveGroup(id, down) => {
                let groups = catalog.groups.clone();
                let Some(index) = groups.iter().position(|group| &group.id == id) else {
                    return;
                };
                let next = if *down {
                    index.checked_add(1).filter(|&n| n < groups.len())
                } else {
                    index.checked_sub(1)
                };
                let Some(next) = next else {
                    return;
                };
                let mut order: Vec<String> = groups.iter().map(|group| group.id.clone()).collect();
                order.swap(index, next);
                committed = Some("已调整分组顺序".into());
                TaskJob::ReorderGroups { ids: order }
            }
        };
        // A loaded durable queue has already saved its containing session, even
        // before that new queue-only session reaches the recent-session list.
        let known_saved = self.sessions.iter().any(|s| s.id == self.session.id)
            || (self.workbench.durable && self.queue_available());
        let save_current =
            (target.as_deref() == Some(self.session.id.as_str()) && !known_saved && !self.busy())
                .then(|| self.session.clone());
        let needs_saved_session = matches!(job, TaskJob::Update { .. });
        let store = self.store.clone();
        let signal = CancellationToken::new();
        let cancel = signal.clone();
        let repaint = ctx.clone();
        self.tasks.error = None;
        self.status = "正在保存任务管理状态…".into();
        self.tasks.worker = Some(TaskWorker {
            signal,
            target: target.clone(),
            block_dispatch,
            committed,
            handle: std::thread::spawn(move || {
                let result = (|| {
                    if cancel.is_cancelled() {
                        return Err("任务管理操作已取消".into());
                    }
                    let Some(session) = save_current.as_ref() else {
                        if needs_saved_session {
                            if let Some(id) = target.as_deref() {
                                if !store.path(id)?.exists() {
                                    return Err(
                                        "任务会话尚未保存或已不存在，请等待保存或重新打开任务列表"
                                            .into(),
                                    );
                                }
                            }
                        }
                        return commit_task_job(&store, revision, job, &cancel);
                    };
                    if !store.path(&session.id)?.exists() {
                        store.save(session)?;
                    }
                    commit_task_job(&store, revision, job, &cancel)
                })();
                repaint.request_repaint();
                result
            }),
        });
    }
    fn finish_task_worker(&mut self, worker: TaskWorker) {
        let mutation = worker.target.is_some() || worker.committed.is_some();
        let target = worker.target.clone();
        let committed = worker.committed.clone();
        match worker
            .handle
            .join()
            .unwrap_or_else(|_| Err("任务管理工作线程意外结束".into()))
        {
            Ok(catalog) => {
                self.tasks.catalog = Some(catalog);
                self.tasks.error = None;
                if target.as_deref() == Some(&self.session.id)
                    && !self.sessions.iter().any(|s| s.id == self.session.id)
                {
                    self.sessions.push(self.session.clone());
                }
                self.tasks.selection.clear();
                self.prune_task_selection();
                self.prune_group_filter();
                if let Some(committed) = committed {
                    self.status = format!("{committed}；执行结果与消息队列保持不变");
                } else if mutation {
                    self.status = "任务管理状态已保存；执行结果与消息队列保持不变".into();
                }
            }
            Err(error) => {
                self.tasks.catalog = None;
                self.tasks.error = Some(error.clone());
                self.status = format!("任务管理状态未确认：{error}。请重新读取后再操作。");
            }
        }
    }
    pub(super) fn poll_task_management(&mut self, ctx: &egui::Context) {
        if self
            .tasks
            .worker
            .as_ref()
            .is_some_and(|w| w.handle.is_finished())
        {
            if let Some(worker) = self.tasks.worker.take() {
                self.finish_task_worker(worker);
            }
        }
        if self.tasks.worker.is_some() {
            ctx.request_repaint_after(Duration::from_millis(25));
        }
    }
    pub(super) fn task_management_busy(&self) -> bool {
        self.tasks.worker.is_some()
    }
    pub(super) fn cancel_task_management(&self) {
        if let Some(worker) = &self.tasks.worker {
            worker.signal.cancel();
        }
    }
    pub(super) fn stop_task_management(&mut self) {
        if let Some(worker) = self.tasks.worker.take() {
            worker.signal.cancel();
            // Normal close polls to completion. An abrupt teardown must not block
            // the UI on storage; the next launch reads any committed publication.
            if worker.handle.is_finished() {
                self.finish_task_worker(worker);
            }
        }
    }
    pub(super) fn task_management_toolbar(&mut self, ui: &mut egui::Ui) {
        if !self.tasks.enabled {
            return;
        }
        ui.horizontal_wrapped(|ui| {
            for (scope, label) in [
                (TaskScope::Active, "未归档"),
                (TaskScope::Pinned, "置顶任务"),
                (TaskScope::Archived, "已归档"),
                (TaskScope::All, "所有任务"),
            ] {
                ui.selectable_value(&mut self.tasks.scope, scope, label);
            }
            if ui
                .add_enabled(
                    self.tasks.worker.is_none(),
                    egui::Button::new("刷新任务状态"),
                )
                .clicked()
            {
                self.reload_task_management(ui.ctx());
            }
            if self.tasks.worker.is_some() {
                ui.spinner();
                if ui.button("取消任务管理操作").clicked() {
                    if let Some(worker) = &self.tasks.worker {
                        worker.signal.cancel();
                    }
                }
            }
        });
        let counts = self.task_group_counts();
        let groups = self
            .tasks
            .catalog
            .as_ref()
            .map(|catalog| catalog.groups.clone())
            .unwrap_or_default();
        ui.horizontal_wrapped(|ui| {
            ui.label(egui::RichText::new("分组").size(11.0).weak());
            if ui
                .selectable_label(
                    matches!(self.tasks.group_filter, GroupFilter::All),
                    "全部分组",
                )
                .clicked()
            {
                self.tasks.group_filter = GroupFilter::All;
            }
            if ui
                .selectable_label(
                    matches!(self.tasks.group_filter, GroupFilter::Ungrouped),
                    format!("未分组 ({})", counts.ungrouped),
                )
                .clicked()
            {
                self.tasks.group_filter = GroupFilter::Ungrouped;
            }
            for group in &groups {
                let count = counts.by_group.get(&group.id).copied().unwrap_or(0);
                let selected =
                    matches!(&self.tasks.group_filter, GroupFilter::Group(id) if id == &group.id);
                if ui
                    .selectable_label(selected, format!("{} ({count})", group.name))
                    .clicked()
                {
                    self.tasks.group_filter = GroupFilter::Group(group.id.clone());
                }
            }
            if ui.button("分组管理…").clicked() {
                self.tasks.group_open = true;
            }
        });
        if let Some(error) = &self.tasks.error {
            ui.label(format!("任务状态读取失败：{error}"));
        }
    }
    /// Batch bar for the ticked tasks; returns the action the user asked for.
    pub(super) fn task_batch_bar(&mut self, ui: &mut egui::Ui) -> Option<TaskAction> {
        if !self.tasks.enabled || self.tasks.selection.is_empty() {
            return None;
        }
        let count = self.tasks.selection.len();
        let ready = self.task_group_action_allowed();
        let groups = self
            .tasks
            .catalog
            .as_ref()
            .map(|catalog| catalog.groups.clone())
            .unwrap_or_default();
        let mut action = None;
        egui::Frame::new()
            .fill(ui.visuals().faint_bg_color)
            .corner_radius(8)
            .inner_margin(10)
            .show(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.label(format!("已选 {count} 项"));
                    for (label, batch) in [
                        ("批量归档", BatchAction::Archive),
                        ("批量恢复", BatchAction::Restore),
                        ("批量置顶", BatchAction::Pin),
                        ("批量取消置顶", BatchAction::Unpin),
                        ("移出分组", BatchAction::RemoveFromGroup),
                    ] {
                        if ui.add_enabled(ready, egui::Button::new(label)).clicked() {
                            action = Some(TaskAction::Batch(batch));
                        }
                    }
                    ui.add_enabled_ui(ready && !groups.is_empty(), |ui| {
                        egui::ComboBox::from_id_salt("task-batch-group")
                            .selected_text("移入分组…")
                            .show_ui(ui, |ui| {
                                for group in &groups {
                                    if ui.selectable_label(false, &group.name).clicked() {
                                        action = Some(TaskAction::Batch(BatchAction::MoveToGroup(
                                            group.id.clone(),
                                        )));
                                    }
                                }
                            });
                    });
                    if ui
                        .add_enabled(ready, egui::Button::new("新建分组并移入…"))
                        .clicked()
                    {
                        self.tasks.group_name.clear();
                        self.tasks.group_rename = None;
                        self.tasks.group_file_selection = true;
                        self.tasks.group_open = true;
                    }
                    if ui.button("清除选择").clicked() {
                        self.tasks.selection.clear();
                    }
                });
            });
        action
    }
    pub(super) fn task_row_controls(&self, ui: &mut egui::Ui, id: &str) -> Option<TaskAction> {
        if !self.tasks.enabled {
            return None;
        }
        let Some(state) = self.task_state(id) else {
            ui.label("任务状态尚未读取");
            return None;
        };
        let mut action = None;
        ui.horizontal_wrapped(|ui| {
            if ui
                .add_enabled(
                    self.task_action_allowed(id, false),
                    egui::Button::new(if state.pinned {
                        "取消置顶"
                    } else {
                        "置顶"
                    }),
                )
                .clicked()
            {
                action = Some(TaskAction::Change(
                    id.into(),
                    TaskChange::SetPinned(!state.pinned),
                ));
            }
            if ui
                .add_enabled(
                    self.task_action_allowed(id, !state.archived),
                    egui::Button::new(if state.archived {
                        "恢复任务"
                    } else {
                        "归档任务"
                    }),
                )
                .on_hover_text(
                    "归档不停止当前执行，其输入队列仍可能继续处理。空闲派发与新发送暂停；尚未保存的新任务需等待首次保存。",
                )
                .clicked()
            {
                action = Some(TaskAction::Change(
                    id.into(),
                    TaskChange::SetArchived(!state.archived),
                ));
            }
            if state.pinned {
                for (down, label) in [(false, "置顶上移"), (true, "置顶下移")] {
                    if ui
                        .add_enabled(
                            self.task_action_allowed(id, false),
                            egui::Button::new(label),
                        )
                        .clicked()
                    {
                        action = Some(TaskAction::Move(id.into(), down));
                    }
                }
            }
            if state.archived {
                ui.label("已归档 · 历史保留");
            }
        });
        action
    }
    pub(super) fn task_execution_banner(&mut self, ui: &mut egui::Ui) {
        if !self.tasks.enabled || self.task_execution_allowed() {
            return;
        }
        if self
            .task_state(&self.session.id)
            .is_some_and(|s| s.archived)
        {
            ui.label("当前任务已归档，当前执行不停止；执行中的输入队列仍可能继续处理。空闲派发与新发送暂停。");
            if ui
                .add_enabled(
                    self.task_action_allowed(&self.session.id, false),
                    egui::Button::new("恢复当前任务"),
                )
                .clicked()
            {
                self.start_task_action(
                    ui.ctx(),
                    TaskAction::Change(self.session.id.clone(), TaskChange::SetArchived(false)),
                );
            }
        } else {
            ui.label("任务管理状态正在读取或尚未确认；新的发送和空闲队列派发已暂停。");
            if ui
                .add_enabled(
                    self.tasks.worker.is_none(),
                    egui::Button::new("重新读取任务状态"),
                )
                .clicked()
            {
                self.reload_task_management(ui.ctx());
            }
        }
    }
    /// Projects are created, renamed, reordered and deleted here. Deleting a
    /// project keeps every task: it only moves them back to ungrouped.
    pub(super) fn task_group_window(&mut self, ctx: &egui::Context) {
        if !self.tasks.enabled || !self.tasks.group_open {
            return;
        }
        let mut open = true;
        let mut action: Option<TaskAction> = None;
        let mut reset_form = false;
        let ready = self.task_group_action_allowed();
        let groups = self
            .tasks
            .catalog
            .as_ref()
            .map(|catalog| catalog.groups.clone())
            .unwrap_or_default();
        let loaded = self.tasks.catalog.is_some();
        let counts = self.task_group_counts();
        let selection = self.tasks.selection.len();
        egui::Window::new("分组管理")
            .collapsible(false)
            .resizable(false)
            .open(&mut open)
            .show(ctx, |ui| {
                ui.label(if self.tasks.group_rename.is_some() {
                    "重命名分组"
                } else {
                    "新建分组"
                });
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.tasks.group_name)
                            .hint_text("分组名称…")
                            .desired_width(180.0),
                    );
                    let can_submit = ready && !self.tasks.group_name.trim().is_empty();
                    let label = if self.tasks.group_rename.is_some() {
                        "保存名称"
                    } else {
                        "创建分组"
                    };
                    if ui.add_enabled(can_submit, egui::Button::new(label)).clicked() {
                        let name = self.tasks.group_name.trim().to_string();
                        action = Some(match &self.tasks.group_rename {
                            Some(id) => TaskAction::RenameGroup {
                                id: id.clone(),
                                name,
                            },
                            None => TaskAction::CreateGroup {
                                name,
                                with_selection: self.tasks.group_file_selection && selection > 0,
                            },
                        });
                    }
                    if self.tasks.group_rename.is_some() {
                        if ui.button("取消重命名").clicked() {
                            reset_form = true;
                        }
                    } else if !self.tasks.group_name.is_empty() && ui.button("清空名称").clicked() {
                        reset_form = true;
                    }
                });
                if self.tasks.group_rename.is_none() && selection > 0 {
                    ui.checkbox(
                        &mut self.tasks.group_file_selection,
                        format!("同时把已选的 {selection} 项移入"),
                    );
                }
                ui.separator();
                if !loaded {
                    ui.label("任务状态尚未读取，无法管理分组。");
                } else if groups.is_empty() {
                    ui.label("还没有分组。创建后可在任务行看到所属分组。");
                }
                for (index, group) in groups.iter().enumerate() {
                    ui.horizontal(|ui| {
                        let count = counts.by_group.get(&group.id).copied().unwrap_or(0);
                        ui.add_sized(
                            [160.0, 20.0],
                            egui::Label::new(format!("{} ({count})", group.name)).truncate(),
                        );
                        let first = index == 0;
                        let last = index + 1 == groups.len();
                        if ui
                            .add_enabled(ready && !first, egui::Button::new("上移"))
                            .clicked()
                        {
                            action = Some(TaskAction::MoveGroup(group.id.clone(), false));
                        }
                        if ui
                            .add_enabled(ready && !last, egui::Button::new("下移"))
                            .clicked()
                        {
                            action = Some(TaskAction::MoveGroup(group.id.clone(), true));
                        }
                        if ui.add_enabled(ready, egui::Button::new("重命名")).clicked() {
                            self.tasks.group_rename = Some(group.id.clone());
                            self.tasks.group_name = group.name.clone();
                            self.tasks.group_file_selection = false;
                        }
                        if ui.add_enabled(ready, egui::Button::new("删除分组")).clicked() {
                            action = Some(TaskAction::DeleteGroup(group.id.clone()));
                        }
                    });
                }
                ui.separator();
                ui.label(
                    egui::RichText::new(
                        "删除分组只移除分组本身：任务、置顶、归档和消息队列都保留，组内任务回到未分组。",
                    )
                    .size(11.0)
                    .weak(),
                );
            });
        self.tasks.group_open = open;
        if reset_form {
            self.tasks.group_rename = None;
            self.tasks.group_name.clear();
            self.tasks.group_file_selection = false;
        }
        if let Some(action) = action {
            self.tasks.group_name.clear();
            self.tasks.group_rename = None;
            self.tasks.group_file_selection = false;
            self.start_task_action(ctx, action);
        }
    }
}
fn commit_task_job(
    store: &SessionStore,
    revision: u64,
    job: TaskJob,
    cancel: &CancellationToken,
) -> Result<TaskCatalog, String> {
    let tasks = store.task_store();
    match job {
        TaskJob::Update { id, change } => {
            tasks
                .update(&id, revision, change, cancel)
                .map_err(|e| e.to_string())?;
            tasks
                .load_catalog()
                .map_err(|e| format!("任务变更已保存，但重新读取失败：{e}"))
        }
        TaskJob::ReorderPinned { order } => tasks
            .reorder_pinned(revision, order, cancel)
            .map_err(|e| e.to_string()),
        TaskJob::Batch { ids, action } => tasks
            .apply_batch(revision, &ids, action, cancel)
            .map_err(|e| e.to_string()),
        TaskJob::CreateGroup { name, ids } => tasks
            .create_group_with_tasks(revision, &name, &ids, cancel)
            .map_err(|e| e.to_string()),
        TaskJob::RenameGroup { id, name } => tasks
            .rename_group(revision, &id, &name, cancel)
            .map_err(|e| e.to_string()),
        TaskJob::DeleteGroup { id } => tasks
            .delete_group(revision, &id, cancel)
            .map_err(|e| e.to_string()),
        TaskJob::ReorderGroups { ids } => tasks
            .reorder_groups(revision, ids, cancel)
            .map_err(|e| e.to_string()),
    }
}

#[cfg(test)]
#[path = "app_task_management_tests.rs"]
mod tests;
