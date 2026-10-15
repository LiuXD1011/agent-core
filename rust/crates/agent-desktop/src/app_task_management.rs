//! Durable task organization is independent of conversation execution history.
use super::*;
use agent_runtime::task_store::{TaskCatalog, TaskChange, TaskState};

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum TaskScope {
    #[default]
    Active,
    Pinned,
    Archived,
    All,
}
#[derive(Default)]
pub(super) struct TaskManagement {
    enabled: bool,
    catalog: Option<TaskCatalog>,
    worker: Option<TaskWorker>,
    error: Option<String>,
    pub scope: TaskScope,
}
struct TaskWorker {
    signal: CancellationToken,
    target: Option<String>,
    block_dispatch: bool,
    handle: JoinHandle<Result<TaskCatalog, String>>,
}
#[derive(Clone)]
pub(super) enum TaskAction {
    Change(String, TaskChange),
    Move(String, bool),
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
    pub(super) fn task_list_matches(&self, id: &str) -> bool {
        let Some(state) = self.task_state(id) else {
            return self.tasks.scope == TaskScope::All;
        };
        match self.tasks.scope {
            TaskScope::Active => !state.archived,
            TaskScope::Pinned => state.pinned && !state.archived,
            TaskScope::Archived => state.archived,
            TaskScope::All => true,
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
        self.tasks.enabled
            && self.tasks.catalog.is_some()
            && self.tasks.worker.is_none()
            && !self.closing
    }
    pub(super) fn start_task_action(&mut self, ctx: &egui::Context, action: TaskAction) {
        let (id, archive) = match &action {
            TaskAction::Change(id, change) => {
                (id.clone(), matches!(change, TaskChange::SetArchived(true)))
            }
            TaskAction::Move(id, _) => (id.clone(), false),
        };
        if !self.task_action_allowed(&id, archive) {
            return;
        }
        let Some(catalog) = self.tasks.catalog.as_ref() else {
            return;
        };
        let revision = catalog.revision;
        let mut order = catalog.pinned_ids.clone();
        if let TaskAction::Move(_, down) = &action {
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
            let Some(index) = visible.iter().position(|key| key == &id) else {
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
            let Some(a) = order.iter().position(|key| key == &id) else {
                return;
            };
            let Some(b) = order.iter().position(|key| key == &visible[next]) else {
                return;
            };
            order.swap(a, b);
        }
        let candidate = if id == self.session.id {
            Some(self.session.clone())
        } else {
            self.sessions.iter().find(|s| s.id == id).cloned()
        };
        let Some(candidate) = candidate else {
            return;
        };
        // A loaded durable queue has already saved its containing session, even
        // before that new queue-only session reaches the recent-session list.
        let known_saved = self.sessions.iter().any(|session| session.id == id)
            || (id == self.session.id && self.workbench.durable && self.queue_available());
        let may_save_new = id == self.session.id && !known_saved && !self.busy();
        let store = self.store.clone();
        let signal = CancellationToken::new();
        let cancel = signal.clone();
        let repaint = ctx.clone();
        self.tasks.error = None;
        self.status = "正在保存任务管理状态…".into();
        self.tasks.worker = Some(TaskWorker {
            signal,
            target: Some(id.clone()),
            block_dispatch: archive || may_save_new,
            handle: std::thread::spawn(move || {
                let result = (|| {
                    if cancel.is_cancelled() {
                        return Err("任务管理操作已取消".into());
                    }
                    if !store.path(&id)?.exists() {
                        if !may_save_new {
                            return Err(
                                "任务会话尚未保存或已不存在，请等待保存或重新打开任务列表".into()
                            );
                        }
                        store.save(&candidate)?;
                    }
                    let tasks = store.task_store();
                    match action {
                        TaskAction::Change(_, change) => {
                            tasks
                                .update(&id, revision, change, &cancel)
                                .map_err(|e| e.to_string())?;
                            tasks
                                .load_catalog()
                                .map_err(|e| format!("任务变更已保存，但重新读取失败：{e}"))
                        }
                        TaskAction::Move(_, _) => tasks
                            .reorder_pinned(revision, order, &cancel)
                            .map_err(|e| e.to_string()),
                    }
                })();
                repaint.request_repaint();
                result
            }),
        });
    }
    fn finish_task_worker(&mut self, worker: TaskWorker) {
        let mutation = worker.target.is_some();
        let target = worker.target.clone();
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
                if mutation {
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
        if let Some(error) = &self.tasks.error {
            ui.label(format!("任务状态读取失败：{error}"));
        }
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
}

#[cfg(test)]
#[path = "app_task_management_tests.rs"]
mod tests;
