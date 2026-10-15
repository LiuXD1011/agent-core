use super::*;
use std::time::Instant;

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let root =
            std::env::temp_dir().join(format!("desktop-task-management-{}", Session::default().id));
        std::fs::create_dir_all(root.join("workspace")).unwrap();
        Self(root)
    }
    fn app(&self, ctx: &egui::Context) -> DesktopApp {
        let mut app = DesktopApp::with_store(ctx, SessionStore::new(self.0.join("sessions")));
        app.settings.api_key.clear();
        app.auth_path = self.0.join("auth/credentials.dat");
        app.settings.options.auth_storage_path = Some(app.auth_path.clone());
        app.workspace = self.0.join("workspace").to_string_lossy().into_owned();
        if app.sessions.is_empty() {
            app.session.workspace = std::fs::canonicalize(self.0.join("workspace")).unwrap();
        }
        app.enable_task_management(ctx);
        settle(&mut app, ctx);
        app
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn settle(app: &mut DesktopApp, ctx: &egui::Context) {
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        app.poll(ctx);
        if !app.task_management_busy() && !app.busy() {
            break;
        }
        assert!(Instant::now() < deadline, "{}", app.status);
        std::thread::sleep(Duration::from_millis(2));
    }
}
fn render(app: &mut DesktopApp, ctx: &egui::Context, events: Vec<egui::Event>) -> egui::FullOutput {
    ctx.run(
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1280.0, 900.0),
            )),
            events,
            ..Default::default()
        },
        |ctx| app.show(ctx),
    )
}
fn click(app: &mut DesktopApp, ctx: &egui::Context, label: &str) {
    let mut position = None;
    for _ in 0..3 {
        let output = render(app, ctx, vec![]);
        position = output.shapes.iter().find_map(|shape| match &shape.shape {
            egui::Shape::Text(text) if text.galley.text() == label => {
                Some(text.pos + text.galley.size() * 0.5)
            }
            _ => None,
        });
    }
    let pos = position.unwrap_or_else(|| panic!("missing task action: {label}"));
    for pressed in [true, false] {
        render(
            app,
            ctx,
            vec![
                egui::Event::PointerMoved(pos),
                egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
        );
    }
}
fn saved_other(app: &mut DesktopApp, name: &str) -> Session {
    let session = Session {
        name: name.into(),
        workspace: app.session.workspace.clone(),
        ..Default::default()
    };
    app.store.save(&session).unwrap();
    app.sessions.push(session.clone());
    session
}
fn change(app: &mut DesktopApp, ctx: &egui::Context, id: &str, change: TaskChange) {
    app.start_task_action(ctx, TaskAction::Change(id.into(), change));
    settle(app, ctx);
}

#[test]
fn task_pin_click_persists_new_session_and_unpin_survives_reopen() {
    let f = Fixture::new();
    let ctx = egui::Context::default();
    let mut app = f.app(&ctx);
    let id = app.session.id.clone();
    app.input = "unsent draft".into();
    click(&mut app, &ctx, "任务中心");
    click(&mut app, &ctx, "置顶");
    settle(&mut app, &ctx);
    assert!(app.store.path(&id).unwrap().is_file());
    assert!(app.task_state(&id).unwrap().pinned);
    assert_eq!(app.input, "unsent draft");
    assert!(app.session.context.messages.is_empty());
    drop(app);
    let mut reopened = f.app(&ctx);
    assert_eq!(reopened.session.id, id);
    assert!(reopened.task_state(&id).unwrap().pinned);
    click(&mut reopened, &ctx, "任务中心");
    click(&mut reopened, &ctx, "取消置顶");
    settle(&mut reopened, &ctx);
    assert!(!reopened.store.task_store().load(&id).unwrap().pinned);
}

#[test]
fn task_archive_click_preserves_history_draft_and_pending_queue_until_explicit_restore() {
    let f = Fixture::new();
    let ctx = egui::Context::default();
    let mut app = f.app(&ctx);
    app.queues.set_paused(true);
    let queued = app
        .queues
        .enqueue(Message::user("pending"), QueueLane::FollowUp);
    app.input = "draft stays".into();
    let id = app.session.id.clone();
    let original = serde_json::to_value(&app.session).unwrap();
    click(&mut app, &ctx, "任务中心");
    click(&mut app, &ctx, "归档任务");
    settle(&mut app, &ctx);
    assert!(app.task_state(&id).unwrap().archived);
    assert_eq!(serde_json::to_value(&app.session).unwrap(), original);
    assert_eq!(app.input, "draft stays");
    assert_eq!(app.queues.snapshot().items[0].id, queued);
    app.start(&ctx);
    app.start_queued_input(&ctx, QueueLane::Immediate);
    assert!(app.worker.is_none() && app.queue_worker.is_none());
    assert_eq!(app.input, "draft stays");
    click(&mut app, &ctx, "已归档");
    click(&mut app, &ctx, "恢复任务");
    settle(&mut app, &ctx);
    assert!(!app.task_state(&id).unwrap().archived);
    assert!(app.queues.snapshot().paused);
    assert_eq!(app.queues.snapshot().items[0].id, queued);
    assert!(app.session.context.messages.is_empty());
}

#[test]
fn task_catalog_failure_blocks_sending_until_refresh_and_conflict_never_claims_success() {
    let f = Fixture::new();
    let ctx = egui::Context::default();
    let mut app = f.app(&ctx);
    let id = app.session.id.clone();
    app.store.save(&app.session).unwrap();
    app.store
        .task_store()
        .update(
            &id,
            0,
            TaskChange::SetArchived(true),
            &CancellationToken::new(),
        )
        .unwrap();
    app.start_task_action(
        &ctx,
        TaskAction::Change(id.clone(), TaskChange::SetPinned(true)),
    );
    settle(&mut app, &ctx);
    assert!(app.tasks.error.is_some());
    assert!(app.tasks.catalog.is_none());
    assert!(!app.task_execution_allowed());
    app.input = "do not send".into();
    app.start(&ctx);
    assert!(app.worker.is_none());
    click(&mut app, &ctx, "重新读取任务状态");
    settle(&mut app, &ctx);
    assert!(app.task_state(&id).unwrap().archived);
    assert!(!app.task_state(&id).unwrap().pinned);
    std::fs::write(f.0.join("sessions/task-catalog.state"), b"broken").unwrap();
    app.reload_task_management(&ctx);
    settle(&mut app, &ctx);
    assert!(app.tasks.catalog.is_none());
    assert!(!app.task_execution_allowed());
    assert_eq!(
        std::fs::read(f.0.join("sessions/task-catalog.state")).unwrap(),
        b"broken"
    );
}

#[test]
fn task_committed_update_is_consumed_even_when_cancel_arrives_before_poll() {
    let f = Fixture::new();
    let ctx = egui::Context::default();
    let mut app = f.app(&ctx);
    let id = app.session.id.clone();
    app.start_task_action(
        &ctx,
        TaskAction::Change(id.clone(), TaskChange::SetPinned(true)),
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while !app.tasks.worker.as_ref().unwrap().handle.is_finished() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(2));
    }
    app.cancel_task_management();
    app.poll(&ctx);
    assert!(app.task_state(&id).unwrap().pinned);
    assert!(app.tasks.error.is_none());
    assert!(app.status.contains("已保存"));
}

#[test]
fn task_reorder_click_preserves_hidden_archived_pins_and_persists_order() {
    let f = Fixture::new();
    let ctx = egui::Context::default();
    let mut app = f.app(&ctx);
    app.session.name = "first visible".into();
    let first = app.session.id.clone();
    let hidden = saved_other(&mut app, "hidden archived");
    let last = saved_other(&mut app, "last visible");
    change(&mut app, &ctx, &last.id, TaskChange::SetPinned(true));
    change(&mut app, &ctx, &hidden.id, TaskChange::SetPinned(true));
    change(&mut app, &ctx, &first, TaskChange::SetPinned(true));
    change(&mut app, &ctx, &hidden.id, TaskChange::SetArchived(true));
    click(&mut app, &ctx, "任务中心");
    click(&mut app, &ctx, "置顶下移");
    settle(&mut app, &ctx);
    assert_eq!(
        app.tasks.catalog.as_ref().unwrap().pinned_ids,
        vec![last.id.clone(), hidden.id.clone(), first.clone()]
    );
    assert_eq!(
        app.store.task_store().load_catalog().unwrap().pinned_ids,
        vec![last.id, hidden.id, first]
    );
}

struct RunningGate(std::sync::Arc<std::sync::atomic::AtomicBool>);
impl agent_runtime::extensions::Extension for RunningGate {
    fn id(&self) -> &str {
        "task-archive-gate"
    }
    fn before_agent_start(
        &self,
        _: &agent_runtime::extensions::BeforeAgentStartEvent,
        _: &agent_runtime::extensions::ExtensionContext,
        signal: &CancellationToken,
    ) -> Result<agent_runtime::extensions::BeforeAgentStartResult, String> {
        while !self.0.load(std::sync::atomic::Ordering::SeqCst) {
            if signal.is_cancelled() {
                return Err("fixture cancelled".into());
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        Ok(Default::default())
    }
}
#[test]
fn task_archive_during_running_keeps_worker_alive_and_completion_does_not_clear_archive() {
    let f = Fixture::new();
    let ctx = egui::Context::default();
    let mut app = f.app(&ctx);
    app.store.save(&app.session).unwrap();
    let id = app.session.id.clone();
    let gate = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    app.extensions
        .register(std::sync::Arc::new(RunningGate(gate.clone())))
        .unwrap();
    app.input = "keep running".into();
    app.start(&ctx);
    assert!(app.running);
    click(&mut app, &ctx, "任务中心");
    click(&mut app, &ctx, "归档任务");
    let deadline = Instant::now() + Duration::from_secs(5);
    while app.task_management_busy() {
        app.poll(&ctx);
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(app.running);
    assert!(!app.signal.is_cancelled());
    assert!(app.task_state(&id).unwrap().archived);
    gate.store(true, std::sync::atomic::Ordering::SeqCst);
    settle(&mut app, &ctx);
    assert!(app.store.task_store().load(&id).unwrap().archived);
    assert!(app
        .store
        .list()
        .unwrap()
        .iter()
        .find(|s| s.id == id)
        .unwrap()
        .context
        .messages
        .iter()
        .any(|m| matches!(m, Message::Assistant(_))));
}

#[test]
fn task_startup_catalog_load_blocks_immediate_queue_and_archived_task_never_auto_dispatches() {
    let f = Fixture::new();
    let ctx = egui::Context::default();
    let mut app = DesktopApp::with_store(&ctx, SessionStore::new(f.0.join("sessions")));
    app.store.save(&app.session).unwrap();
    app.store
        .task_store()
        .update(
            &app.session.id,
            0,
            TaskChange::SetArchived(true),
            &CancellationToken::new(),
        )
        .unwrap();
    app.queues
        .enqueue(Message::user("not sent"), QueueLane::Immediate);
    app.enable_task_management(&ctx);
    assert!(!app.task_execution_allowed());
    settle(&mut app, &ctx);
    assert!(app.worker.is_none());
    assert_eq!(app.queues.len(), 1);
    assert!(app.session.context.messages.is_empty());
}

#[test]
fn task_durable_restart_keeps_archived_queue_isolated_until_both_explicit_recoveries() {
    let f = Fixture::new();
    let ctx = egui::Context::default();
    let mut app = f.app(&ctx);
    app.enable_durable_queues(&ctx);
    settle(&mut app, &ctx);
    let id = app.session.id.clone();
    change(&mut app, &ctx, &id, TaskChange::SetArchived(true));
    app.queues.bind_workspace(&app.session.workspace).unwrap();
    app.queues
        .try_enqueue(
            Message::user("restart archived input"),
            QueueLane::Immediate,
            None,
        )
        .unwrap();
    drop(app);
    let mut recovered = f.app(&ctx);
    recovered.enable_durable_queues(&ctx);
    settle(&mut recovered, &ctx);
    assert!(recovered.task_state(&id).unwrap().archived);
    assert!(recovered.queues.snapshot().recovery_required);
    assert!(recovered.session.context.messages.is_empty());
    assert!(recovered.worker.is_none());
    assert_eq!(recovered.queues.len(), 1);
    recovered.signal.cancel();
    let halted = recovered.queue_halted;
    click(&mut recovered, &ctx, "恢复队列");
    click(&mut recovered, &ctx, "立即发送");
    assert_eq!(recovered.queue_halted, halted);
    assert!(recovered.signal.is_cancelled());
    assert!(recovered.queues.snapshot().recovery_required);
    click(&mut recovered, &ctx, "消息队列 (1)");
    click(&mut recovered, &ctx, "恢复当前任务");
    settle(&mut recovered, &ctx);
    assert!(!recovered.task_state(&id).unwrap().archived);
    assert!(recovered.queues.snapshot().recovery_required);
    assert!(recovered.session.context.messages.is_empty());
    click(&mut recovered, &ctx, "消息队列 (1)");
    click(&mut recovered, &ctx, "恢复队列");
    settle(&mut recovered, &ctx);
    assert_eq!(
        recovered
            .session
            .context
            .messages
            .iter()
            .filter(|m| matches!(m, Message::User(_)))
            .count(),
        1
    );
    assert!(recovered.queues.is_empty());
}

#[test]
fn task_search_filtered_pin_move_preserves_hidden_unarchived_position() {
    let f = Fixture::new();
    let ctx = egui::Context::default();
    let mut app = f.app(&ctx);
    app.session.name = "match first".into();
    let first = app.session.id.clone();
    let hidden = saved_other(&mut app, "hidden unarchived");
    let last = saved_other(&mut app, "match last");
    change(&mut app, &ctx, &last.id, TaskChange::SetPinned(true));
    change(&mut app, &ctx, &hidden.id, TaskChange::SetPinned(true));
    change(&mut app, &ctx, &first, TaskChange::SetPinned(true));
    click(&mut app, &ctx, "任务中心");
    click(&mut app, &ctx, "搜索任务名称或工作区…");
    render(&mut app, &ctx, vec![egui::Event::Text("match".into())]);
    assert!(!app.task_matches_filters(&hidden));
    assert!(app.task_matches_filters(&last));
    click(&mut app, &ctx, "置顶下移");
    settle(&mut app, &ctx);
    assert_eq!(
        app.store.task_store().load_catalog().unwrap().pinned_ids,
        vec![last.id, hidden.id, first]
    );
}

#[test]
fn task_existing_session_removed_externally_is_not_recreated_from_ui_snapshot() {
    let f = Fixture::new();
    let ctx = egui::Context::default();
    let mut app = f.app(&ctx);
    app.store.save(&app.session).unwrap();
    app.sessions.push(app.session.clone());
    let id = app.session.id.clone();
    let path = app.store.path(&id).unwrap();
    std::fs::remove_file(&path).unwrap();
    change(&mut app, &ctx, &id, TaskChange::SetPinned(true));
    assert!(!path.exists());
    assert!(app.tasks.error.is_some());
    assert!(app.tasks.catalog.is_none());
}

#[test]
fn task_pin_worker_does_not_interrupt_reserved_durable_message_handoff() {
    let f = Fixture::new();
    let ctx = egui::Context::default();
    let mut app = f.app(&ctx);
    app.enable_durable_queues(&ctx);
    settle(&mut app, &ctx);
    // This queue-only session has been saved by the queue worker, but is not
    // necessarily in the recent-session snapshot yet.
    assert!(app.sessions.is_empty());
    app.queues.bind_workspace(&app.session.workspace).unwrap();
    app.queues
        .try_enqueue(
            Message::user("reserved during pin"),
            QueueLane::Immediate,
            None,
        )
        .unwrap();
    app.dispatch_durable_queue(&ctx);
    let deadline = Instant::now() + Duration::from_secs(5);
    while app.queues.snapshot().uncertain.is_empty() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(2));
    }
    let id = app.session.id.clone();
    app.start_task_action(
        &ctx,
        TaskAction::Change(id.clone(), TaskChange::SetPinned(true)),
    );
    let worker = app.tasks.worker.take().unwrap();
    assert!(!worker.block_dispatch);
    let (release, wait) = mpsc::sync_channel::<()>(1);
    app.tasks.worker = Some(TaskWorker {
        signal: worker.signal,
        target: worker.target,
        block_dispatch: worker.block_dispatch,
        handle: std::thread::spawn(move || {
            let result = worker.handle.join().unwrap();
            let _ = wait.recv();
            result
        }),
    });
    assert!(app.task_execution_allowed());
    loop {
        app.poll(&ctx);
        if app
            .session
            .context
            .messages
            .iter()
            .any(|m| matches!(m, Message::Assistant(_)))
            && !app.busy()
        {
            break;
        }
        assert!(Instant::now() < deadline, "{}", app.status);
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(app.task_management_busy());
    assert!(!app.queue_halted);
    assert!(app.queues.snapshot().uncertain.is_empty());
    assert_eq!(
        app.session
            .context
            .messages
            .iter()
            .filter(|m| matches!(m, Message::User(_)))
            .count(),
        1
    );
    release.send(()).unwrap();
    settle(&mut app, &ctx);
    assert!(app.task_state(&id).unwrap().pinned);
    assert!(app.queues.is_empty());
}
