use super::*;
use std::time::Instant;

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        Self(std::env::temp_dir().join(format!("desktop-workbench-{}", Session::default().id)))
    }
    fn app(&self, ctx: &egui::Context) -> DesktopApp {
        let mut app = DesktopApp::with_store(ctx, SessionStore::new(self.0.join("sessions")));
        app.auth_path = self.0.join("auth/credentials.dat");
        app.settings.options.auth_storage_path = Some(app.auth_path.clone());
        app.settings.api_key.clear();
        app
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn input(size: egui::Vec2, events: Vec<egui::Event>) -> egui::RawInput {
    egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
        events,
        ..Default::default()
    }
}
fn click(app: &mut DesktopApp, ctx: &egui::Context, label: &str) {
    let size = egui::vec2(1280.0, 840.0);
    let mut position = None;
    for _ in 0..3 {
        let output = ctx.run(input(size, vec![]), |ctx| app.show(ctx));
        position = output.shapes.iter().find_map(|shape| match &shape.shape {
            egui::Shape::Text(text) if text.galley.text() == label => {
                Some(text.pos + text.galley.size() * 0.5)
            }
            _ => None,
        });
    }
    let pos = position.unwrap_or_else(|| panic!("visible action missing: {label}"));
    for pressed in [true, false] {
        let _ = ctx.run(
            input(
                size,
                vec![
                    egui::Event::PointerMoved(pos),
                    egui::Event::PointerButton {
                        pos,
                        button: egui::PointerButton::Primary,
                        pressed,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
            ),
            |ctx| app.show(ctx),
        );
    }
}

#[test]
fn actual_navigation_and_inspector_clicks_preserve_draft_credentials_and_session() {
    let f = Fixture::new();
    let ctx = egui::Context::default();
    let mut app = f.app(&ctx);
    app.input = "unsent task".into();
    let session = serde_json::to_value(&app.session).unwrap();
    click(&mut app, &ctx, "任务中心");
    assert!(app.workbench.page == WorkbenchPage::Tasks);
    click(&mut app, &ctx, "任务详情");
    assert!(app.workbench.page == WorkbenchPage::Conversation);
    assert!(app.workbench.inspector_open);
    assert_eq!(app.input, "unsent task");
    assert_eq!(serde_json::to_value(&app.session).unwrap(), session);
    assert!(app.worker.is_none() && app.auth_worker.is_none());
    assert!(!app.allow_changes && !app.allow_commands);
    assert!(!f.0.exists());
}

#[test]
fn responsive_inspector_does_not_shrink_small_window_composer() {
    let f = Fixture::new();
    for theme in [egui::Theme::Dark, egui::Theme::Light] {
        for size in [egui::vec2(900.0, 640.0), egui::vec2(1280.0, 840.0)] {
            let ctx = egui::Context::default();
            let mut app = f.app(&ctx);
            shell::set_theme(&ctx, theme);
            app.workbench.inspector_open = true;
            app.input = "long draft\n".repeat(100);
            for tab in [
                InspectorTab::Overview,
                InspectorTab::Execution,
                InspectorTab::Artifacts,
            ] {
                app.workbench.tab = tab;
                for _ in 0..3 {
                    let _ = ctx.run(input(size, vec![]), |ctx| app.show(ctx));
                }
                let composer =
                    egui::containers::panel::PanelState::load(&ctx, egui::Id::new("composer"))
                        .unwrap();
                assert!(composer.rect.height() < 240.0);
                assert!(
                    composer.rect.width() > 540.0,
                    "{size:?} {:?}",
                    composer.rect
                );
                assert!(composer.rect.max.y <= size.y);
                assert_eq!(ctx.theme(), theme);
            }
        }
    }
}

#[test]
fn task_center_uses_live_session_and_filters_without_worker_requests() {
    let f = Fixture::new();
    let ctx = egui::Context::default();
    let mut app = f.app(&ctx);
    let mut saved = app.session.clone();
    saved.name = "stale task".into();
    app.sessions.push(saved);
    app.session.name = "live task".into();
    app.workbench.page = WorkbenchPage::Tasks;
    app.workbench.query = "live".into();
    let output = ctx.run(input(egui::vec2(1280.0, 840.0), vec![]), |ctx| {
        app.show(ctx)
    });
    let text = output
        .shapes
        .iter()
        .filter_map(|s| {
            if let egui::Shape::Text(t) = &s.shape {
                Some(t.galley.text())
            } else {
                None
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("live task"));
    app.workbench.filter = TaskFilter::Completed;
    assert!(!TaskFilter::Completed.matches(TaskPhase::from_messages(
        &app.session.context.messages,
        false
    )));
    let _ = ctx.run(input(egui::vec2(1280.0, 840.0), vec![]), |ctx| {
        app.show(ctx)
    });
    assert!(app.worker.is_none());
    assert!(!f.0.exists());
}

#[test]
fn long_task_titles_leave_room_for_status_at_minimum_width() {
    let f = Fixture::new();
    let ctx = egui::Context::default();
    let mut app = f.app(&ctx);
    app.session.name = "very long task name ".repeat(100);
    app.session
        .context
        .messages
        .push(Message::Assistant(AssistantMessage::error(
            "fixture cancellation",
            true,
        )));
    app.workbench.page = WorkbenchPage::Tasks;
    let mut status = None;
    for _ in 0..3 {
        let output = ctx.run(input(egui::vec2(900.0, 640.0), vec![]), |ctx| app.show(ctx));
        status = output.shapes.iter().find_map(|shape| match &shape.shape {
            egui::Shape::Text(text) if text.galley.text() == "已停止" => {
                Some(egui::Rect::from_min_size(text.pos, text.galley.size()))
            }
            _ => None,
        });
    }
    let rect = status.expect("task status visible beside a truncated long title");
    assert!(rect.max.x <= 876.0, "{rect:?}");
    assert!(rect.min.x >= shell::SIDEBAR_WIDTH);
}

#[test]
fn selecting_task_reuses_resume_worker_and_obeys_busy_guard() {
    let f = Fixture::new();
    let ctx = egui::Context::default();
    let mut app = f.app(&ctx);
    let other = Session {
        name: "other task".into(),
        ..Session::default()
    };
    let id = other.id.clone();
    app.store.save(&other).unwrap();
    app.sessions.push(other);
    let before = app.session.id.clone();
    app.running = true;
    app.resume_workbench_task(&ctx, &id);
    assert_eq!(app.session.id, before);
    app.running = false;
    app.resume_workbench_task(&ctx, &id);
    assert!(app.worker.is_some());
    let deadline = Instant::now() + Duration::from_secs(5);
    while app.busy() {
        app.poll(&ctx);
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(2));
    }
    assert_eq!(app.session.id, id);
    assert!(app.workbench.page == WorkbenchPage::Conversation);
    app.workbench.page = WorkbenchPage::Tasks;
    click(&mut app, &ctx, "other task");
    assert!(app.workbench.page == WorkbenchPage::Conversation);
    let deadline = Instant::now() + Duration::from_secs(5);
    while app.busy() {
        app.poll(&ctx);
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[test]
fn demo_execution_still_persists_once_and_inspector_is_read_only() {
    let f = Fixture::new();
    let ctx = egui::Context::default();
    let mut app = f.app(&ctx);
    std::fs::create_dir_all(f.0.join("workspace")).unwrap();
    app.workspace = f.0.join("workspace").to_string_lossy().into_owned();
    app.input = "hello".into();
    app.start(&ctx);
    let deadline = Instant::now() + Duration::from_secs(5);
    while app.busy() {
        app.poll(&ctx);
        assert!(Instant::now() < deadline, "{}", app.status);
        std::thread::sleep(Duration::from_millis(2));
    }
    let before = serde_json::to_value(&app.session).unwrap();
    let p = TaskProjection::from_session(&app.session, false);
    assert_eq!(p.phase, TaskPhase::Completed);
    assert_eq!(p.user_turns, 1);
    app.workbench.inspector_open = true;
    let _ = ctx.run(input(egui::vec2(1280.0, 840.0), vec![]), |ctx| {
        app.show(ctx)
    });
    assert_eq!(serde_json::to_value(&app.session).unwrap(), before);
    assert_eq!(app.store.list().unwrap().len(), 1);
}

fn archived_session() -> Session {
    use agent_core::types::{StopReason, ToolCall, ToolResultMessage, UserMessage};
    use agent_runtime::compaction::NativeCompaction;
    let mut session = Session::default();
    session
        .append_message(Message::user("fixture change"))
        .unwrap();
    session
        .append_message(Message::Assistant(AssistantMessage {
            content: vec![Content::ToolCall(ToolCall {
                id: "call".into(),
                name: "edit".into(),
                arguments: serde_json::json!({"path":"archive.md"}),
            })],
            stop_reason: StopReason::ToolUse,
            ..AssistantMessage::text("")
        }))
        .unwrap();
    session
        .append_message(Message::ToolResult(ToolResultMessage {
            tool_call_id: "call".into(),
            tool_name: "edit".into(),
            content: vec![],
            details: serde_json::json!({"diff":"+ archived change"}),
            usage: None,
            added_tool_names: vec![],
            is_error: false,
            timestamp: 1,
        }))
        .unwrap();
    session
        .append_message(Message::Assistant(AssistantMessage::text("done")))
        .unwrap();
    let archived_messages = session.context.messages.clone();
    session.record_compaction(NativeCompaction{id:"checkpoint".into(),timestamp:2,summary:"summary".into(),archived_messages,kept_message_count:0,usage:None},
        vec![Message::User(UserMessage {content:vec![Content::text("The conversation history before this point was compacted into the following summary:\n\n<summary>\nsummary\n</summary>")],timestamp:2})]).unwrap();
    session
}

#[test]
fn history_cache_reuses_snapshot_and_invalidates_on_append_branch_and_error() {
    let mut state = WorkbenchState::default();
    let mut session = archived_session();
    state.refresh_history(&session);
    assert_eq!(
        state
            .history
            .as_ref()
            .unwrap()
            .as_ref()
            .unwrap()
            .projection
            .artifacts
            .len(),
        1
    );
    state
        .history
        .as_mut()
        .unwrap()
        .as_mut()
        .unwrap()
        .projection
        .user_turns = 999;
    state.refresh_history(&session);
    assert_eq!(
        state
            .history
            .as_ref()
            .unwrap()
            .as_ref()
            .unwrap()
            .projection
            .user_turns,
        999
    );
    session.append_message(Message::user("next task")).unwrap();
    state.refresh_history(&session);
    assert_eq!(
        state
            .history
            .as_ref()
            .unwrap()
            .as_ref()
            .unwrap()
            .projection
            .user_turns,
        2
    );
    session.navigate(None).unwrap();
    state.refresh_history(&session);
    assert!(state
        .history
        .as_ref()
        .unwrap()
        .as_ref()
        .unwrap()
        .projection
        .artifacts
        .is_empty());
    session.leaf_id = Some("missing".into());
    state.refresh_history(&session);
    assert!(state.history.as_ref().unwrap().is_err());
}

#[test]
fn archived_diff_and_copy_path_are_visible_without_touching_disk() {
    let f = Fixture::new();
    let ctx = egui::Context::default();
    let mut app = f.app(&ctx);
    app.session = archived_session();
    app.workbench.inspector_open = true;
    let before = serde_json::to_value(&app.session).unwrap();
    click(&mut app, &ctx, "查看差异");
    let mut copy_pos = None;
    let mut diff_visible = false;
    for _ in 0..4 {
        let output = ctx.run(input(egui::vec2(1280.0, 840.0), vec![]), |ctx| {
            app.show(ctx)
        });
        for shape in output.shapes {
            if let egui::Shape::Text(t) = shape.shape {
                diff_visible |= t.galley.text().contains("+ archived change");
                if t.galley.text() == "复制路径" {
                    copy_pos = Some(t.pos + t.galley.size() * 0.5);
                }
            }
        }
    }
    assert!(diff_visible);
    let pos = copy_pos.unwrap();
    let mut copied = false;
    for pressed in [true, false] {
        let output = ctx.run(
            input(
                egui::vec2(1280.0, 840.0),
                vec![
                    egui::Event::PointerMoved(pos),
                    egui::Event::PointerButton {
                        pos,
                        button: egui::PointerButton::Primary,
                        pressed,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
            ),
            |ctx| app.show(ctx),
        );
        copied |= output.platform_output.commands.iter().any(
            |command| matches!(command,egui::OutputCommand::CopyText(text) if text=="archive.md"),
        );
    }
    assert!(copied);
    assert_eq!(serde_json::to_value(&app.session).unwrap(), before);
    assert!(!f.0.exists());
}

fn preview_app(f: &Fixture, ctx: &egui::Context) -> DesktopApp {
    let mut app = f.app(ctx);
    std::fs::create_dir_all(f.0.join("workspace")).unwrap();
    app.session = archived_session();
    app.session.workspace = f.0.join("workspace");
    app.workbench.inspector_open = true;
    std::fs::write(app.session.workspace.join("archive.md"), "actual file v1").unwrap();
    app
}

fn wait_preview(app: &mut DesktopApp, ctx: &egui::Context) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while app.workbench.preview_worker.is_some() {
        let _ = ctx.run(input(egui::vec2(1280.0, 840.0), vec![]), |ctx| {
            app.show(ctx)
        });
        assert!(Instant::now() < deadline, "preview worker timeout");
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[test]
fn artifact_preview_click_reads_refreshes_and_reports_deleted_file() {
    let f = Fixture::new();
    let ctx = egui::Context::default();
    let mut app = preview_app(&f, &ctx);
    let before = serde_json::to_value(&app.session).unwrap();
    click(&mut app, &ctx, "查看当前文件");
    wait_preview(&mut app, &ctx);
    let file = app
        .workbench
        .preview
        .as_ref()
        .unwrap()
        .result
        .as_ref()
        .unwrap()
        .as_ref()
        .unwrap();
    assert_eq!(file.text, "actual file v1");
    let hash = file.sha256.clone();
    let output = ctx.run(input(egui::vec2(1280.0, 840.0), vec![]), |ctx| {
        app.show(ctx)
    });
    assert!(output.shapes.iter().any(
        |s| matches!(&s.shape, egui::Shape::Text(t) if t.galley.text().contains("actual file v1"))
    ));
    std::fs::write(app.session.workspace.join("archive.md"), "actual file v2").unwrap();
    click(&mut app, &ctx, "刷新预览");
    wait_preview(&mut app, &ctx);
    let file = app
        .workbench
        .preview
        .as_ref()
        .unwrap()
        .result
        .as_ref()
        .unwrap()
        .as_ref()
        .unwrap();
    assert_eq!(file.text, "actual file v2");
    assert_ne!(file.sha256, hash);
    std::fs::remove_file(app.session.workspace.join("archive.md")).unwrap();
    click(&mut app, &ctx, "刷新预览");
    wait_preview(&mut app, &ctx);
    assert!(matches!(
        app.workbench.preview.as_ref().unwrap().result,
        Some(Err(ArtifactError::Missing))
    ));
    click(&mut app, &ctx, "关闭预览");
    assert!(app.workbench.preview.is_none());
    assert_eq!(serde_json::to_value(&app.session).unwrap(), before);
}

#[test]
fn preview_worker_slot_cancels_and_discards_stale_completions() {
    for change in 0..6 {
        let f = Fixture::new();
        let ctx = egui::Context::default();
        let mut app = preview_app(&f, &ctx);
        let signal = CancellationToken::new();
        let (tx, rx) = mpsc::sync_channel(1);
        app.workbench.preview = Some(PreviewState {
            key: HistoryKey::new(&app.session),
            entry_id: "fixture".into(),
            result: None,
        });
        app.workbench.preview_worker = Some(PreviewWorker {
            signal: signal.clone(),
            handle: std::thread::spawn(move || {
                rx.recv().unwrap();
                Ok(ArtifactPreview {
                    text: "STALE".into(),
                    byte_len: 5,
                    sha256: "fixture".into(),
                    entry_id: "fixture".into(),
                    path: "archive.md".into(),
                })
            }),
        });
        match change {
            0 => app.session.workspace = f.0.join("different"),
            1 => {
                app.session.navigate(None).unwrap();
            }
            2 => app.workbench.page = WorkbenchPage::Tasks,
            3 => app.workbench.inspector_open = false,
            4 => {
                app.session
                    .append_message(Message::user("next revision"))
                    .unwrap();
            }
            _ => app.session.id = "another-session".into(),
        }
        app.workbench.poll_preview(&app.session, &ctx);
        assert!(signal.is_cancelled());
        assert!(app.workbench.preview.is_none());
        assert!(app.workbench.preview_worker.is_some());
        app.workbench
            .start_preview(&app.session, "another".into(), &ctx);
        assert!(
            app.workbench.preview.is_none(),
            "cancelled worker retains single slot"
        );
        tx.send(()).unwrap();
        wait_preview(&mut app, &ctx);
        assert!(
            app.workbench.preview.is_none(),
            "stale completion discarded"
        );
    }
}

#[test]
fn dropping_workbench_cancels_without_waiting_for_worker() {
    let signal = CancellationToken::new();
    let (tx, rx) = mpsc::sync_channel(1);
    let mut state = WorkbenchState::default();
    state.preview_worker = Some(PreviewWorker {
        signal: signal.clone(),
        handle: std::thread::spawn(move || {
            rx.recv().unwrap();
            Err(ArtifactError::Cancelled)
        }),
    });
    drop(state);
    assert!(signal.is_cancelled());
    tx.send(()).unwrap();
}

#[test]
fn preview_cancel_and_inspector_close_clicks_cancel_active_read() {
    for action in ["取消预览", "关闭"] {
        let f = Fixture::new();
        let ctx = egui::Context::default();
        let mut app = preview_app(&f, &ctx);
        app.workbench.refresh_history(&app.session);
        let snapshot = app.workbench.history.as_ref().unwrap().as_ref().unwrap();
        let entry_id = snapshot
            .artifact_detail(snapshot.projection.artifacts[0].result_message)
            .unwrap()
            .entry_id
            .clone();
        let signal = CancellationToken::new();
        let (tx, rx) = mpsc::sync_channel(1);
        app.workbench.preview = Some(PreviewState {
            key: HistoryKey::new(&app.session),
            entry_id,
            result: None,
        });
        app.workbench.preview_worker = Some(PreviewWorker {
            signal: signal.clone(),
            handle: std::thread::spawn(move || {
                rx.recv().unwrap();
                Err(ArtifactError::Cancelled)
            }),
        });
        click(&mut app, &ctx, action);
        assert!(signal.is_cancelled());
        assert!(app.workbench.preview.is_none());
        tx.send(()).unwrap();
        wait_preview(&mut app, &ctx);
    }
}

#[test]
fn panicked_preview_worker_releases_slot_with_sanitized_error() {
    let session = archived_session();
    let ctx = egui::Context::default();
    let mut state = WorkbenchState::default();
    state.inspector_open = true;
    state.preview = Some(PreviewState {
        key: HistoryKey::new(&session),
        entry_id: "fixture".into(),
        result: None,
    });
    state.preview_worker = Some(PreviewWorker {
        signal: CancellationToken::new(),
        handle: std::thread::spawn(|| panic!("fixture worker failure")),
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    while state.preview_worker.is_some() {
        state.poll_preview(&session, &ctx);
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(matches!(
        state.preview.as_ref().unwrap().result,
        Some(Err(ArtifactError::Io))
    ));
}

fn queue_app(f: &Fixture, ctx: &egui::Context) -> DesktopApp {
    let mut app = f.app(ctx);
    std::fs::create_dir_all(f.0.join("workspace")).unwrap();
    app.workspace = f.0.join("workspace").to_string_lossy().into_owned();
    app.session.workspace = f.0.join("workspace");
    app.queues.set_paused(true);
    app.workbench.queue_open = true;
    app
}
fn wait_queue(app: &mut DesktopApp, ctx: &egui::Context) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while app.busy() {
        app.poll(ctx);
        assert!(Instant::now() < deadline, "{}", app.status);
        std::thread::sleep(Duration::from_millis(2));
    }
}
#[test]
fn managed_queue_clicks_reorder_remove_pause_resume_and_persist_once() {
    let f = Fixture::new();
    let ctx = egui::Context::default();
    let mut app = queue_app(&f, &ctx);
    app.queues.steer(Message::user("first"));
    app.queues.steer(Message::user("second"));
    click(&mut app, &ctx, "下移");
    assert_eq!(app.queues.snapshot().items[0].message.text(), "second");
    click(&mut app, &ctx, "移除");
    assert_eq!(app.queues.len(), 1);
    assert_eq!(app.queues.snapshot().items[0].message.text(), "first");
    click(&mut app, &ctx, "恢复队列");
    app.poll(&ctx);
    wait_queue(&mut app, &ctx);
    let saved = app.store.list().unwrap().pop().unwrap();
    let users: Vec<_> = saved
        .context
        .messages
        .iter()
        .filter(|message| matches!(message, Message::User(_)))
        .collect();
    assert_eq!(users.len(), 1);
    assert_eq!(users[0].text(), "first");
    assert!(app.queues.is_empty());
    click(&mut app, &ctx, "暂停队列");
    assert!(app.queues.snapshot().paused);
}
#[test]
fn managed_queue_edit_preserves_raw_draft_images_and_rejects_stale_session_action() {
    let f = Fixture::new();
    let ctx = egui::Context::default();
    let mut app = queue_app(&f, &ctx);
    let image = Content::Image {
        data: "aW1hZ2U=".into(),
        mime_type: "image/png".into(),
    };
    let message = Message::User(agent_core::types::UserMessage {
        content: vec![Content::text("prepared"), image.clone()],
        timestamp: 4,
    });
    let id = app
        .queues
        .enqueue(message.clone(), agent_runtime::runtime::QueueLane::Steering);
    app.queued_drafts.push((id, message, "raw original".into()));
    app.input = "keep existing draft".into();
    app.pop_queue_for_edit(&app.session.id.clone(), id);
    assert_eq!(app.queues.len(), 1);
    assert_eq!(app.input, "keep existing draft");
    app.input.clear();
    app.pop_queue_for_edit("another-session", id);
    assert_eq!(app.queues.len(), 1);
    click(&mut app, &ctx, "移回编辑");
    assert_eq!(app.input, "raw original");
    assert_eq!(app.input_images, vec![image.clone()]);
    assert!(app.queues.is_empty());
    app.start(&ctx);
    wait_queue(&mut app, &ctx);
    let user = app
        .session
        .context
        .messages
        .iter()
        .find_map(|message| {
            if let Message::User(user) = message {
                Some(user)
            } else {
                None
            }
        })
        .unwrap();
    assert_eq!(user.content, vec![Content::text("raw original"), image]);
    assert!(app.input_images.is_empty());
}
#[test]
fn managed_queue_session_switch_preserves_paused_rows_and_rejects_other_queue_ids() {
    let f = Fixture::new();
    let ctx = egui::Context::default();
    let mut app = queue_app(&f, &ctx);
    let first = app.session.clone();
    let id = app.queues.enqueue(
        Message::user("first session pending"),
        agent_runtime::runtime::QueueLane::Steering,
    );
    let second = Session {
        workspace: app.session.workspace.clone(),
        ..Default::default()
    };
    app.start_extension_action(
        &ctx,
        ExtensionAction::Replace(Box::new(second), SessionStartReason::New),
    );
    wait_queue(&mut app, &ctx);
    assert!(app.queues.is_empty());
    app.pop_queue_for_edit(&first.id, id);
    assert!(app.input.is_empty());
    app.start_extension_action(
        &ctx,
        ExtensionAction::Replace(Box::new(first), SessionStartReason::Resume),
    );
    wait_queue(&mut app, &ctx);
    assert_eq!(app.queues.len(), 1);
    assert!(app.queues.snapshot().paused);
    assert_eq!(app.queues.snapshot().items[0].id, id);
    click(&mut app, &ctx, "立即发送");
    app.poll(&ctx);
    wait_queue(&mut app, &ctx);
    assert!(app.queues.snapshot().paused);
    assert_eq!(
        app.session
            .context
            .messages
            .iter()
            .filter(|message| matches!(message, Message::User(_)))
            .count(),
        1
    );
}
#[test]
fn managed_queue_enqueue_click_keeps_pending_after_worker_completion_and_clear_is_explicit() {
    let f = Fixture::new();
    let ctx = egui::Context::default();
    let mut app = queue_app(&f, &ctx);
    app.input = "queued through worker".into();
    click(&mut app, &ctx, "将输入加入队列");
    wait_queue(&mut app, &ctx);
    assert_eq!(app.queues.len(), 1);
    assert!(app.input.is_empty());
    app.input = "separate current turn".into();
    app.start(&ctx);
    wait_queue(&mut app, &ctx);
    assert_eq!(app.queues.len(), 1);
    assert_eq!(
        app.session
            .context
            .messages
            .iter()
            .filter(|message| matches!(message, Message::User(_)))
            .count(),
        1
    );
    click(&mut app, &ctx, "清空待发送");
    assert!(app.queues.is_empty());
    assert!(app.queued_drafts.is_empty());
}

struct QueueHook {
    seen: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    gate: std::sync::Arc<std::sync::atomic::AtomicBool>,
}
impl agent_runtime::extensions::Extension for QueueHook {
    fn id(&self) -> &str {
        "queue-hook"
    }
    fn input(
        &self,
        event: &agent_runtime::extensions::InputEvent,
        _: &agent_runtime::extensions::ExtensionContext,
        _: &CancellationToken,
    ) -> Result<agent_runtime::extensions::InputResult, String> {
        self.seen.lock().unwrap().push(event.text.clone());
        Ok(agent_runtime::extensions::InputResult::Transform {
            text: format!("prepared:{}", event.text),
            images: None,
        })
    }
    fn before_agent_start(
        &self,
        _: &agent_runtime::extensions::BeforeAgentStartEvent,
        _: &agent_runtime::extensions::ExtensionContext,
        signal: &CancellationToken,
    ) -> Result<agent_runtime::extensions::BeforeAgentStartResult, String> {
        while !self.gate.load(std::sync::atomic::Ordering::SeqCst) {
            if signal.is_cancelled() {
                return Err("cancelled fixture".into());
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        Ok(Default::default())
    }
}
#[test]
fn managed_queue_active_worker_steers_once_and_edited_input_reprocesses_hooks() {
    let f = Fixture::new();
    let ctx = egui::Context::default();
    let mut app = queue_app(&f, &ctx);
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let gate = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    app.extensions
        .register(std::sync::Arc::new(QueueHook {
            seen: seen.clone(),
            gate: gate.clone(),
        }))
        .unwrap();
    app.input = "primary".into();
    app.start(&ctx);
    app.input = "steering".into();
    app.start_steering_input(&ctx);
    let deadline = Instant::now() + Duration::from_secs(5);
    while app.queue_receiver.is_some() {
        app.poll_steering_input();
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(2));
    }
    assert_eq!(app.queues.len(), 1);
    click(&mut app, &ctx, "移回编辑");
    assert_eq!(app.input, "steering");
    app.input = "edited".into();
    app.start_steering_input(&ctx);
    while app.queue_receiver.is_some() {
        app.poll_steering_input();
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(2));
    }
    app.queues.set_paused(false);
    gate.store(true, std::sync::atomic::Ordering::SeqCst);
    wait_queue(&mut app, &ctx);
    let users: Vec<_> = app
        .session
        .context
        .messages
        .iter()
        .filter(|message| matches!(message, Message::User(_)))
        .map(Message::text)
        .collect();
    assert_eq!(users, ["prepared:primary", "prepared:edited"]);
    let calls = seen.lock().unwrap();
    assert_eq!(
        calls
            .iter()
            .filter(|text| text.as_str() == "primary")
            .count(),
        1
    );
    assert_eq!(
        calls
            .iter()
            .filter(|text| text.as_str() == "steering")
            .count(),
        1
    );
    assert_eq!(
        calls
            .iter()
            .filter(|text| text.as_str() == "edited")
            .count(),
        1
    );
    assert_eq!(calls.len(), 3);
    assert_eq!(
        app.store.list().unwrap()[0].context.messages,
        app.session.context.messages
    );
}
#[test]
fn managed_queue_duplicate_prepared_payloads_recover_the_selected_raw_input() {
    let f = Fixture::new();
    let ctx = egui::Context::default();
    let mut app = queue_app(&f, &ctx);
    let message = Message::user("same prepared payload");
    let first = app
        .queues
        .enqueue(message.clone(), agent_runtime::runtime::QueueLane::Steering);
    let second = app
        .queues
        .enqueue(message.clone(), agent_runtime::runtime::QueueLane::Steering);
    app.queued_drafts
        .push((first, message.clone(), "first raw".into()));
    app.queued_drafts
        .push((second, message, "second raw".into()));
    app.pop_queue_for_edit(&app.session.id.clone(), second);
    assert_eq!(app.input, "second raw");
    assert_eq!(app.queues.snapshot().items[0].id, first);
    assert_eq!(app.queued_drafts[0].2, "first raw");
}
#[test]
fn managed_queue_failed_idle_validation_restores_images_and_raw_draft_without_retry() {
    let f = Fixture::new();
    let ctx = egui::Context::default();
    let mut app = queue_app(&f, &ctx);
    let image = Content::Image {
        data: "aW1hZ2U=".into(),
        mime_type: "image/png".into(),
    };
    let message = Message::User(agent_core::types::UserMessage {
        content: vec![Content::text("prepared"), image.clone()],
        timestamp: 5,
    });
    let id = app
        .queues
        .enqueue(message.clone(), agent_runtime::runtime::QueueLane::Steering);
    app.queued_drafts.push((id, message, "raw pending".into()));
    app.settings.mode = ProviderMode::OpenAiCompatible;
    app.settings.model.clear();
    app.queues.set_paused(false);
    app.input = "new draft".into();
    app.poll(&ctx);
    wait_queue(&mut app, &ctx);
    assert_eq!(app.input, "raw pending\nnew draft");
    assert_eq!(app.input_images, [image]);
    assert!(app.queues.is_empty());
    assert!(app.session.context.messages.is_empty());
}

#[test]
fn managed_queue_deferred_input_waits_for_current_answer_and_send_now_uses_steering() {
    for immediate in [false, true] {
        let f = Fixture::new();
        let ctx = egui::Context::default();
        let mut app = queue_app(&f, &ctx);
        let gate = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        app.extensions
            .register(std::sync::Arc::new(QueueHook {
                seen: Default::default(),
                gate: gate.clone(),
            }))
            .unwrap();
        app.input = "primary".into();
        app.start(&ctx);
        app.input = "follow up".into();
        click(&mut app, &ctx, "将输入加入队列");
        let deadline = Instant::now() + Duration::from_secs(5);
        while app.queue_receiver.is_some() {
            app.poll_steering_input();
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(app.queues.snapshot().items[0].lane, QueueLane::FollowUp);
        if immediate {
            click(&mut app, &ctx, "立即发送");
        } else {
            app.queues.set_paused(false);
        }
        gate.store(true, std::sync::atomic::Ordering::SeqCst);
        wait_queue(&mut app, &ctx);
        let messages = &app.session.context.messages;
        let second_user = messages
            .iter()
            .position(|message| {
                matches!(message, Message::User(_)) && message.text() == "prepared:follow up"
            })
            .unwrap();
        assert_eq!(second_user, if immediate { 1 } else { 2 });
        assert_eq!(
            messages
                .iter()
                .filter(|message| matches!(message, Message::User(_)))
                .count(),
            2
        );
        assert!(app.queued_drafts.is_empty());
    }
}
#[test]
fn managed_queue_error_pauses_remaining_rows_instead_of_cascading_requests() {
    let f = Fixture::new();
    let ctx = egui::Context::default();
    let mut app = queue_app(&f, &ctx);
    app.settings.mode = ProviderMode::OpenAiCompatible;
    app.settings.model.clear();
    app.queues.enqueue(
        Message::user("first validation fails"),
        QueueLane::Immediate,
    );
    let second = app
        .queues
        .enqueue(Message::user("do not auto start"), QueueLane::Immediate);
    app.queues.set_paused(false);
    app.poll(&ctx);
    wait_queue(&mut app, &ctx);
    assert!(app.queues.snapshot().paused);
    assert_eq!(app.queues.snapshot().items[0].id, second);
    assert_eq!(app.input, "first validation fails");
    assert!(app.session.context.messages.is_empty());
}

#[test]
fn managed_queue_clear_click_keeps_raw_drafts_for_already_dispatched_items() {
    let f = Fixture::new();
    let ctx = egui::Context::default();
    let mut app = queue_app(&f, &ctx);
    let first_message = Message::user("in flight");
    let first = app
        .queues
        .enqueue(first_message.clone(), QueueLane::Immediate);
    app.queued_drafts
        .push((first, first_message, "raw in flight".into()));
    let reserved = app.queues.take_next().unwrap();
    let pending_message = Message::user("still pending");
    let pending = app
        .queues
        .enqueue(pending_message.clone(), QueueLane::FollowUp);
    app.queued_drafts
        .push((pending, pending_message, "raw pending".into()));
    click(&mut app, &ctx, "清空待发送");
    assert!(app.queues.is_empty());
    assert_eq!(app.queued_drafts.len(), 1);
    assert_eq!(app.queued_drafts[0].0, first);
    app.queues.restore(reserved).unwrap();
    app.pop_queue_for_edit(&app.session.id.clone(), first);
    assert_eq!(app.input, "raw in flight");
}

fn durable_app(f: &Fixture, ctx: &egui::Context) -> DesktopApp {
    let mut app = f.app(ctx);
    std::fs::create_dir_all(f.0.join("workspace")).unwrap();
    if app.sessions.is_empty() {
        app.session.workspace = std::fs::canonicalize(f.0.join("workspace")).unwrap();
        app.workspace = app.session.workspace.to_string_lossy().into_owned();
    }
    app.enable_durable_queues(ctx);
    wait_queue(&mut app, ctx);
    assert!(app.queue_available(), "{}", app.status);
    app.workbench.queue_open = true;
    app
}

#[test]
fn durable_queue_only_restart_recovers_paused_and_sends_exactly_once() {
    let f = Fixture::new();
    let ctx = egui::Context::default();
    let mut app = durable_app(&f, &ctx);
    app.queues.try_set_paused(true).unwrap();
    let id = app.session.id.clone();
    app.input = "queue only before first request".into();
    click(&mut app, &ctx, "将输入加入队列");
    wait_queue(&mut app, &ctx);
    assert_eq!(app.queues.len(), 1);
    assert!(app.session.context.messages.is_empty());
    drop(app);
    let mut recovered = durable_app(&f, &ctx);
    assert_eq!(recovered.session.id, id);
    assert_eq!(recovered.queues.len(), 1);
    assert!(recovered.queues.snapshot().recovery_required);
    for _ in 0..5 {
        recovered.poll(&ctx);
    }
    assert!(recovered.session.context.messages.is_empty());
    click(&mut recovered, &ctx, "恢复队列");
    wait_queue(&mut recovered, &ctx);
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
    assert!(recovered.queues.snapshot().uncertain.is_empty());
    drop(recovered);
    let again = durable_app(&f, &ctx);
    assert!(again.queues.is_empty());
    assert!(again.queues.snapshot().uncertain.is_empty());
    assert_eq!(
        again
            .session
            .context
            .messages
            .iter()
            .filter(|m| matches!(m, Message::User(_)))
            .count(),
        1
    );
}

#[test]
fn durable_recovered_edit_keeps_original_whitespace_and_images() {
    let f = Fixture::new();
    let ctx = egui::Context::default();
    let mut app = durable_app(&f, &ctx);
    app.queues.try_set_paused(true).unwrap();
    app.extensions
        .register(std::sync::Arc::new(QueueHook {
            seen: Default::default(),
            gate: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
        }))
        .unwrap();
    app.input = "  raw before hook  ".into();
    let image = Content::Image {
        data: "aW1hZ2U=".into(),
        mime_type: "image/png".into(),
    };
    app.input_images.push(image.clone());
    click(&mut app, &ctx, "将输入加入队列");
    wait_queue(&mut app, &ctx);
    assert_eq!(
        app.queues.snapshot().items[0].message.text(),
        "prepared:raw before hook"
    );
    drop(app);
    let mut recovered = durable_app(&f, &ctx);
    click(&mut recovered, &ctx, "移回编辑");
    wait_queue(&mut recovered, &ctx);
    assert_eq!(recovered.input, "  raw before hook  ");
    assert_eq!(recovered.input_images, [image]);
    assert!(recovered.queues.is_empty());
    assert!(recovered.session.context.messages.is_empty());
}

#[test]
fn durable_inflight_restart_requires_explicit_retry_not_general_resume() {
    let f = Fixture::new();
    let ctx = egui::Context::default();
    let app = durable_app(&f, &ctx);
    app.queues.bind_workspace(&app.session.workspace).unwrap();
    app.queues
        .try_enqueue(Message::user("interrupted"), QueueLane::Immediate, None)
        .unwrap();
    assert!(app.queues.try_take_next().unwrap().is_some());
    drop(app);
    let mut recovered = durable_app(&f, &ctx);
    assert_eq!(recovered.queues.snapshot().uncertain.len(), 1);
    click(&mut recovered, &ctx, "恢复队列");
    wait_queue(&mut recovered, &ctx);
    assert!(recovered.session.context.messages.is_empty());
    assert_eq!(recovered.queues.snapshot().uncertain.len(), 1);
    click(&mut recovered, &ctx, "确认重试此消息");
    wait_queue(&mut recovered, &ctx);
    if recovered.session.context.messages.is_empty() {
        click(&mut recovered, &ctx, "恢复队列");
        wait_queue(&mut recovered, &ctx);
    }
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
}

#[test]
fn durable_locked_store_never_falls_back_to_memory_and_can_retry_loading() {
    let f = Fixture::new();
    let ctx = egui::Context::default();
    let owner = durable_app(&f, &ctx);
    let mut blocked = f.app(&ctx);
    blocked.enable_durable_queues(&ctx);
    wait_queue(&mut blocked, &ctx);
    assert!(!blocked.queue_available());
    assert!(blocked.workbench.queue_load_error.is_some());
    blocked.input = "retain raw".into();
    blocked.start_queued_input(&ctx, QueueLane::FollowUp);
    assert_eq!(blocked.input, "retain raw");
    assert!(blocked.queue_receiver.is_none());
    assert!(blocked.queues.is_empty());
    drop(owner);
    click(&mut blocked, &ctx, "重试读取本地队列");
    blocked.poll(&ctx);
    wait_queue(&mut blocked, &ctx);
    assert!(blocked.queue_available(), "{}", blocked.status);
}

#[test]
fn durable_cancelled_reservation_ack_never_starts_a_request() {
    let f = Fixture::new();
    let ctx = egui::Context::default();
    let mut app = durable_app(&f, &ctx);
    app.queues.bind_workspace(&app.session.workspace).unwrap();
    app.queues
        .try_enqueue(
            Message::user("cancel before worker start"),
            QueueLane::Immediate,
            None,
        )
        .unwrap();
    app.dispatch_durable_queue(&ctx);
    let deadline = Instant::now() + Duration::from_secs(5);
    while !app
        .workbench
        .queue_io
        .as_ref()
        .unwrap()
        .handle
        .is_finished()
    {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(2));
    }
    app.cancel_queue_io();
    app.poll(&ctx);
    assert!(app.worker.is_none());
    assert!(app.session.context.messages.is_empty());
    assert_eq!(app.queues.snapshot().uncertain.len(), 1);
    assert!(app.queue_halted);
}

#[test]
fn durable_failed_idle_validation_retains_one_explicit_recovery_record() {
    let f = Fixture::new();
    let ctx = egui::Context::default();
    let mut app = durable_app(&f, &ctx);
    app.queues.bind_workspace(&app.session.workspace).unwrap();
    app.queues
        .try_enqueue(
            Message::user("invalid provider input"),
            QueueLane::Immediate,
            None,
        )
        .unwrap();
    app.settings.mode = ProviderMode::OpenAiCompatible;
    app.settings.model.clear();
    app.poll(&ctx);
    wait_queue(&mut app, &ctx);
    assert!(app.input.is_empty());
    assert!(app.status.contains("恢复记录"), "{}", app.status);
    assert_eq!(app.queues.snapshot().uncertain.len(), 1);
    assert!(app.session.context.messages.is_empty());
    click(&mut app, &ctx, "恢复队列");
    wait_queue(&mut app, &ctx);
    assert!(app.session.context.messages.is_empty());
    assert_eq!(app.queues.snapshot().uncertain.len(), 1);
}

#[test]
fn durable_workspace_change_blocks_replay_but_keeps_edit_path() {
    let f = Fixture::new();
    let ctx = egui::Context::default();
    let mut app = durable_app(&f, &ctx);
    app.queues.try_set_paused(true).unwrap();
    app.input = "workspace bound input".into();
    click(&mut app, &ctx, "将输入加入队列");
    wait_queue(&mut app, &ctx);
    drop(app);
    let mut recovered = durable_app(&f, &ctx);
    std::fs::create_dir_all(f.0.join("another")).unwrap();
    recovered.workspace = f.0.join("another").to_string_lossy().into_owned();
    click(&mut recovered, &ctx, "恢复队列");
    wait_queue(&mut recovered, &ctx);
    assert_eq!(recovered.queues.len(), 1);
    assert!(recovered.session.context.messages.is_empty());
    click(&mut recovered, &ctx, "移回编辑");
    wait_queue(&mut recovered, &ctx);
    assert_eq!(recovered.input, "workspace bound input");
    assert!(recovered.queues.is_empty());
}

#[test]
fn durable_enqueue_committed_before_cancel_does_not_duplicate_composer() {
    let f = Fixture::new();
    let ctx = egui::Context::default();
    let mut app = durable_app(&f, &ctx);
    app.queues.try_set_paused(true).unwrap();
    app.input = "commit before cancellation".into();
    app.start_queued_input(&ctx, QueueLane::FollowUp);
    let deadline = Instant::now() + Duration::from_secs(5);
    while !app.queue_worker.as_ref().unwrap().is_finished() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(2));
    }
    app.queue_signal.cancel();
    app.halt_queue();
    app.poll_steering_input();
    assert!(app.input.is_empty());
    assert_eq!(app.queues.len(), 1);
    assert!(app.session.context.messages.is_empty());
}
