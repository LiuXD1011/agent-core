//! Scheduled automations page: definitions, run records and the message
//! center. The scheduler thread only classifies due work; actual runs are
//! spawned on the UI thread so they always use the current model settings,
//! and every command enters the transactional automation store.
use super::*;
use agent_runtime::automations::{
    parse_rrule, tick_scheduler, AutomationCatalog, AutomationDefinition, AutomationDelivery,
    AutomationDeliveryStatus, AutomationNotification, AutomationRunKind, AutomationRunRecord,
    AutomationRunStatus, AutomationSchedule, AutomationStatus, AutomationStore, DueDecision,
    Frequency, SchedulerOutcome,
};
use chrono::Timelike;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

const SCHEDULER_TICK_MS: u64 = 30_000;
const SCHEDULER_ACTIVE_TICK_MS: u64 = 5_000;
const RUN_CONCURRENCY: usize = 3;

pub(super) enum AutomationEvent {
    Catalog(Result<AutomationCatalog, String>),
    Scheduled(Vec<SchedulerOutcome>),
    RunFinished(AutomationRunRecord),
    StoreError(String),
}

pub(super) struct AutomationIo {
    pub signal: CancellationToken,
    pub handle: JoinHandle<()>,
}

struct AutomationRunWorker {
    signal: CancellationToken,
    handle: JoinHandle<()>,
}

impl AutomationsState {
    pub(super) fn active_run_count(&self) -> usize {
        self.active_runs.load(Ordering::SeqCst)
    }

    pub(super) fn cancel_all_runs(&mut self) {
        for worker in &self.runs {
            worker.signal.cancel();
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub(super) enum EditorFrequency {
    Once,
    Minutely,
    Hourly,
    #[default]
    Daily,
    Weekly,
    Monthly,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum EditorAccess {
    ReadOnly,
    WriteFiles,
    Full,
}

pub(super) struct AutomationEditor {
    pub id: Option<String>,
    pub name: String,
    pub prompt: String,
    pub(super) frequency: EditorFrequency,
    pub interval: u32,
    pub hour: u32,
    pub minute: u32,
    pub weekday: usize,
    pub monthday: u32,
    pub once_at: String,
    pub valid_until: String,
    pub(super) access: EditorAccess,
}

impl AutomationEditor {
    fn for_new() -> Self {
        Self {
            id: None,
            name: String::new(),
            prompt: String::new(),
            frequency: EditorFrequency::Daily,
            interval: 1,
            hour: 9,
            minute: 0,
            weekday: 0,
            monthday: 1,
            once_at: String::new(),
            valid_until: String::new(),
            access: EditorAccess::ReadOnly,
        }
    }

    fn build_schedule(&self) -> Result<AutomationSchedule, String> {
        Ok(match self.frequency {
            EditorFrequency::Once => AutomationSchedule::Once {
                scheduled_at: parse_once_input(&self.once_at)?,
            },
            EditorFrequency::Minutely => AutomationSchedule::Recurring {
                rrule: format!("FREQ=MINUTELY;INTERVAL={}", self.interval.max(1)),
            },
            EditorFrequency::Hourly => AutomationSchedule::Recurring {
                rrule: format!("FREQ=HOURLY;INTERVAL={}", self.interval.max(1)),
            },
            EditorFrequency::Daily => AutomationSchedule::Recurring {
                rrule: format!(
                    "FREQ=DAILY;INTERVAL={};BYHOUR={};BYMINUTE={}",
                    self.interval.max(1),
                    self.hour.min(23),
                    self.minute.min(59)
                ),
            },
            EditorFrequency::Weekly => {
                const DAYS: [&str; 7] = ["MO", "TU", "WE", "TH", "FR", "SA", "SU"];
                AutomationSchedule::Recurring {
                    rrule: format!(
                        "FREQ=WEEKLY;INTERVAL={};BYDAY={};BYHOUR={};BYMINUTE={}",
                        self.interval.max(1),
                        DAYS[self.weekday.min(6)],
                        self.hour.min(23),
                        self.minute.min(59)
                    ),
                }
            }
            EditorFrequency::Monthly => AutomationSchedule::Recurring {
                rrule: format!(
                    "FREQ=MONTHLY;INTERVAL=1;BYMONTHDAY={};BYHOUR={};BYMINUTE={}",
                    self.monthday.clamp(1, 31),
                    self.hour.min(23),
                    self.minute.min(59)
                ),
            },
        })
    }

    fn build_definition(&self, workspace: &str) -> Result<AutomationDefinition, String> {
        let now = timestamp();
        let workspace = std::fs::canonicalize(workspace.trim())
            .map_err(|_| format!("工作目录不存在：{}", workspace.trim()))?;
        let mut definition = AutomationDefinition {
            id: self
                .id
                .clone()
                .unwrap_or_else(|| format!("auto-{}-{}", timestamp(), std::process::id())),
            name: self.name.trim().to_owned(),
            prompt: self.prompt.trim().to_owned(),
            status: AutomationStatus::Active,
            schedule: self.build_schedule()?,
            workspaces: vec![workspace],
            model: None,
            allow_changes: self.access != EditorAccess::ReadOnly,
            allow_commands: self.access == EditorAccess::Full,
            valid_from: None,
            valid_until: parse_date_input(&self.valid_until, true)?,
            next_run_at: None,
            last_run_at: None,
            created_at: now,
            updated_at: now,
            deleted_at: None,
        };
        definition.next_run_at =
            agent_runtime::automations::compute_next_run_at(&definition, now, now, None);
        Ok(definition)
    }
}

fn schedule_description(schedule: &AutomationSchedule) -> String {
    let weekday_name = |day: chrono::Weekday| match day {
        chrono::Weekday::Mon => "周一",
        chrono::Weekday::Tue => "周二",
        chrono::Weekday::Wed => "周三",
        chrono::Weekday::Thu => "周四",
        chrono::Weekday::Fri => "周五",
        chrono::Weekday::Sat => "周六",
        chrono::Weekday::Sun => "周日",
    };
    match schedule {
        AutomationSchedule::Once { scheduled_at } => {
            format!("单次 · {}", format_time(*scheduled_at))
        }
        AutomationSchedule::Recurring { rrule } => match parse_rrule(rrule) {
            Ok(rule) => {
                let time = format!("{:02}:{:02}", rule.byhour[0], rule.byminute[0]);
                match rule.freq {
                    Frequency::Minutely => format!("每 {} 分钟", rule.interval),
                    Frequency::Hourly => format!("每 {} 小时", rule.interval),
                    Frequency::Daily if rule.interval == 1 => format!("每天 {time}"),
                    Frequency::Daily => format!("每 {} 天 {time}", rule.interval),
                    Frequency::Weekly => {
                        let days = rule
                            .byday
                            .iter()
                            .map(|day| weekday_name(*day))
                            .collect::<Vec<_>>()
                            .join("、");
                        if rule.interval == 1 {
                            format!("每周{days} {time}")
                        } else {
                            format!("每 {} 周 · {days} {time}", rule.interval)
                        }
                    }
                    Frequency::Monthly => {
                        let day = rule.bymonthday[0];
                        if rule.interval == 1 {
                            format!("每月 {day} 日 {time}")
                        } else {
                            format!("每 {} 月 · {day} 日 {time}", rule.interval)
                        }
                    }
                    Frequency::Yearly => format!(
                        "每年 {} 月 {} 日 {time}",
                        rule.bymonth[0], rule.bymonthday[0]
                    ),
                }
            }
            Err(_) => "重复规则无效".into(),
        },
    }
}

fn format_time(ms: u64) -> String {
    chrono::DateTime::from_timestamp_millis(ms as i64)
        .map(|moment| {
            moment
                .with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_else(|| "-".into())
}

fn status_label(
    definition: &AutomationDefinition,
    runtime_last_error: Option<&String>,
) -> &'static str {
    if definition.status == AutomationStatus::Paused {
        return "已暂停";
    }
    match runtime_last_error.map(String::as_str) {
        Some("valid_until_passed") | Some("once_window_expired") => "已过期",
        Some("missed_window_expired") => "已跳过错过的批次",
        Some("recovery_circuit_broken") => "已暂停恢复",
        _ => match definition.next_run_at {
            Some(_) => "已启用",
            None => "运行中",
        },
    }
}

fn parse_date_input(raw: &str, end_of_day: bool) -> Result<Option<u64>, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let date = chrono::NaiveDate::parse_from_str(trimmed, "%Y-%m-%d")
        .map_err(|_| format!("日期格式应为 YYYY-MM-DD：{trimmed}"))?;
    use chrono::TimeZone;
    let time = if end_of_day {
        date.and_hms_opt(23, 59, 59).unwrap_or_default()
    } else {
        date.and_hms_opt(9, 0, 0).unwrap_or_default()
    };
    let moment = chrono::Local
        .from_local_datetime(&time)
        .single()
        .ok_or("日期无法表示为本地时间")?;
    Ok(Some(moment.timestamp_millis().max(0) as u64))
}

fn parse_once_input(raw: &str) -> Result<u64, String> {
    let trimmed = raw.trim();
    for format in ["%Y-%m-%d %H:%M", "%Y-%m-%dT%H:%M"] {
        if let Ok(moment) = chrono::NaiveDateTime::parse_from_str(trimmed, format) {
            use chrono::TimeZone;
            let local = chrono::Local
                .from_local_datetime(&moment)
                .single()
                .ok_or("时间无法表示为本地时间")?;
            return Ok(local.timestamp_millis().max(0) as u64);
        }
    }
    Err("时间格式应为 YYYY-MM-DD HH:MM".into())
}

fn catalog_revision(catalog: &Option<Result<AutomationCatalog, String>>) -> u64 {
    catalog
        .as_ref()
        .and_then(|result| result.as_ref().ok())
        .map(|catalog| catalog.revision)
        .unwrap_or(0)
}

fn editor_for(definition: &AutomationDefinition) -> AutomationEditor {
    let mut editor = AutomationEditor {
        id: Some(definition.id.clone()),
        name: definition.name.clone(),
        prompt: definition.prompt.clone(),
        frequency: EditorFrequency::Daily,
        interval: 1,
        hour: 9,
        minute: 0,
        weekday: 0,
        monthday: 1,
        once_at: String::new(),
        valid_until: definition
            .valid_until
            .map(|until| {
                chrono::DateTime::from_timestamp_millis(until as i64)
                    .map(|moment| {
                        moment
                            .with_timezone(&chrono::Local)
                            .format("%Y-%m-%d")
                            .to_string()
                    })
                    .unwrap_or_default()
            })
            .unwrap_or_default(),
        access: if definition.allow_commands {
            EditorAccess::Full
        } else if definition.allow_changes {
            EditorAccess::WriteFiles
        } else {
            EditorAccess::ReadOnly
        },
    };
    match &definition.schedule {
        AutomationSchedule::Once { scheduled_at } => {
            editor.frequency = EditorFrequency::Once;
            editor.once_at = format_time(*scheduled_at);
        }
        AutomationSchedule::Recurring { rrule } => {
            if let Ok(rule) = parse_rrule(rrule) {
                editor.interval = rule.interval;
                editor.hour = rule.byhour[0];
                editor.minute = rule.byminute[0];
                editor.frequency = match rule.freq {
                    Frequency::Minutely => EditorFrequency::Minutely,
                    Frequency::Hourly => EditorFrequency::Hourly,
                    Frequency::Daily => EditorFrequency::Daily,
                    Frequency::Weekly => {
                        editor.weekday = rule
                            .byday
                            .first()
                            .map(|day| day.num_days_from_monday() as usize)
                            .unwrap_or(0);
                        EditorFrequency::Weekly
                    }
                    Frequency::Monthly => {
                        editor.monthday = rule.bymonthday[0];
                        EditorFrequency::Monthly
                    }
                    Frequency::Yearly => EditorFrequency::Monthly,
                };
            }
        }
    }
    editor
}

#[derive(Default)]
pub(super) struct AutomationsState {
    pub(super) catalog: Option<Result<AutomationCatalog, String>>,
    io: Option<AutomationIo>,
    runs: Vec<AutomationRunWorker>,
    active_runs: Arc<AtomicUsize>,
    pub(super) editor: Option<AutomationEditor>,
    pub(super) records_open: bool,
    pub(super) notifications_open: bool,
    pub(super) unread_notifications: usize,
}

impl Drop for AutomationsState {
    fn drop(&mut self) {
        for worker in &self.runs {
            worker.signal.cancel();
        }
        if let Some(io) = &self.io {
            io.signal.cancel();
        }
    }
}

impl DesktopApp {
    pub(super) fn enable_automations(&mut self, ctx: &egui::Context) {
        if self.automation_scheduler.is_some() {
            return;
        }
        let directory = self.store.directory().join("automations");
        match AutomationStore::open(directory) {
            Ok(store) => {
                let (sender, receiver) = mpsc::channel();
                self.automation_store = Some(Arc::new(Mutex::new(store)));
                self.automation_event_sender = Some(sender);
                self.automation_events = Some(receiver);
                self.request_automation_catalog(ctx);
                self.start_automation_scheduler(ctx);
            }
            Err(error) => {
                self.status = format!("定时任务存储不可用：{error}");
            }
        }
    }

    fn spawn_automation_io(
        &mut self,
        ctx: &egui::Context,
        work: impl FnOnce(&AutomationStore) + Send + 'static,
    ) {
        // One in-flight store worker keeps repeated clicks from stacking
        // unbounded detached writes against the same file lock.
        if self
            .schedules
            .io
            .as_ref()
            .is_some_and(|io| !io.handle.is_finished())
        {
            return;
        }
        let Some(store) = self.automation_store.clone() else {
            return;
        };
        let Some(sender) = self.automation_event_sender.clone() else {
            return;
        };
        let repaint = ctx.clone();
        let signal = CancellationToken::new();
        let worker_signal = signal.clone();
        let handle = std::thread::Builder::new()
            .name("automation-store".into())
            .spawn(move || {
                if worker_signal.is_cancelled() {
                    return;
                }
                let Ok(store) = store.lock() else {
                    return;
                };
                work(&store);
                let _ = sender.send(AutomationEvent::Catalog(
                    store.catalog().map_err(|error| error.to_string()),
                ));
                repaint.request_repaint();
            })
            .ok();
        if let Some(handle) = handle {
            self.schedules.io = Some(AutomationIo { signal, handle });
        }
    }

    pub(super) fn request_automation_catalog(&mut self, ctx: &egui::Context) {
        self.spawn_automation_io(ctx, |_| {});
    }

    fn start_automation_scheduler(&mut self, ctx: &egui::Context) {
        let Some(store) = self.automation_store.clone() else {
            return;
        };
        let Some(sender) = self.automation_event_sender.clone() else {
            return;
        };
        let active_runs = self.schedules.active_runs.clone();
        let repaint = ctx.clone();
        let signal = CancellationToken::new();
        let worker_signal = signal.clone();
        let handle = std::thread::Builder::new()
            .name("automation-scheduler".into())
            .spawn(move || loop {
                let sleep_ms = if active_runs.load(Ordering::SeqCst) > 0 {
                    SCHEDULER_ACTIVE_TICK_MS
                } else {
                    SCHEDULER_TICK_MS
                };
                for _ in 0..sleep_ms / 100 {
                    if worker_signal.is_cancelled() {
                        return;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
                if worker_signal.is_cancelled() {
                    return;
                }
                let result = store.lock().ok().map(|store| {
                    tick_scheduler(
                        &store,
                        timestamp(),
                        chrono::Local::now().hour(),
                        active_runs.load(Ordering::SeqCst),
                        RUN_CONCURRENCY,
                    )
                    .map_err(|error| error.to_string())
                });
                match result {
                    Some(Ok(outcomes)) if !outcomes.is_empty() => {
                        let _ = sender.send(AutomationEvent::Scheduled(outcomes));
                        repaint.request_repaint();
                    }
                    Some(Err(error)) => {
                        let _ = sender.send(AutomationEvent::StoreError(error));
                        repaint.request_repaint();
                    }
                    _ => {}
                }
            })
            .ok();
        if let Some(handle) = handle {
            self.automation_scheduler = Some(AutomationIo { signal, handle });
        }
    }

    fn save_automation(&mut self, ctx: &egui::Context) {
        let Some(editor) = self.schedules.editor.take() else {
            return;
        };
        let definition = match editor.build_definition(&self.workspace) {
            Ok(definition) => definition,
            Err(error) => {
                self.status = error;
                self.schedules.editor = Some(editor);
                return;
            }
        };
        let revision = catalog_revision(&self.schedules.catalog);
        let id = editor.id.clone();
        self.spawn_automation_io(ctx, move |store| {
            let result = match &id {
                Some(id) => store
                    .update_definition(id, revision, |existing| {
                        let created_at = existing.created_at;
                        let last_run_at = existing.last_run_at;
                        *existing = definition.clone();
                        existing.created_at = created_at;
                        existing.last_run_at = last_run_at;
                        Ok(())
                    })
                    .map(|_| ()),
                None => store.create(definition).map(|_| ()),
            };
            if let Err(error) = result {
                eprintln!("automation save failed: {error}");
            }
        });
        self.status = "定时任务已保存".into();
    }

    pub(super) fn run_automation_now(&mut self, automation_id: String, ctx: &egui::Context) {
        let definition = self
            .schedules
            .catalog
            .as_ref()
            .and_then(|result| result.as_ref().ok())
            .and_then(|catalog| {
                catalog
                    .definitions
                    .iter()
                    .find(|other| other.id == automation_id && other.deleted_at.is_none())
                    .cloned()
            });
        let Some(definition) = definition else {
            return;
        };
        if self.schedules.active_runs.load(Ordering::SeqCst) >= RUN_CONCURRENCY {
            self.status = "定时任务并发已达上限（3），请等待运行结束".into();
            return;
        }
        self.spawn_automation_run(definition, AutomationRunKind::Manual, None, ctx);
    }

    fn spawn_automation_run(
        &mut self,
        definition: AutomationDefinition,
        kind: AutomationRunKind,
        missed_scheduled_at: Option<u64>,
        ctx: &egui::Context,
    ) {
        let Some(store) = self.automation_store.clone() else {
            return;
        };
        let Some(sender) = self.automation_event_sender.clone() else {
            return;
        };
        let workspace = definition
            .workspaces
            .first()
            .cloned()
            .unwrap_or_else(|| self.session.workspace.clone());
        let session = Session {
            workspace: workspace.clone(),
            name: format!("定时任务 · {}", definition.name),
            ..Default::default()
        };
        let prompt = Message::user(definition.prompt.clone());
        let mut options = self.base_run_options();
        options.allow_changes = definition.allow_changes;
        options.allow_commands = definition.allow_commands;
        options.max_turns = 64;
        let settings = self.settings.clone();
        let signal = CancellationToken::new();
        let worker_signal = signal.clone();
        let repaint = ctx.clone();
        let active = self.schedules.active_runs.clone();
        active.fetch_add(1, Ordering::SeqCst);
        let automation_name = definition.name.clone();
        let handle = std::thread::Builder::new()
            .name("automation-run".into())
            .spawn(move || {
                let started_at = timestamp();
                let queues = MessageQueues::default();
                let mut session = session;
                let mut output = String::new();
                let result = run_prompt(
                    &mut session,
                    prompt,
                    &settings,
                    &options,
                    &queues,
                    &worker_signal,
                    &mut |event| {
                        if let AgentEvent::MessageEnd {
                            message: Message::Assistant(message),
                        } = event
                        {
                            output.push_str(&Message::Assistant(message).text());
                        }
                    },
                );
                let success = result.is_ok();
                let cancelled = worker_signal.is_cancelled();
                let record = AutomationRunRecord {
                    id: format!("run-{}-{}", definition.id, started_at),
                    automation_id: definition.id.clone(),
                    automation_name: definition.name.clone(),
                    session_id: Some(session.id.clone()),
                    status: match &result {
                        Ok(()) => AutomationRunStatus::Success,
                        Err(_) if cancelled => AutomationRunStatus::Skipped,
                        Err(_) => AutomationRunStatus::Failed,
                    },
                    kind,
                    missed_scheduled_at,
                    workspace: workspace.clone(),
                    output: if output.trim().is_empty() {
                        match &result {
                            Ok(()) => "（模型没有返回文本）".into(),
                            Err(error) => error.clone(),
                        }
                    } else {
                        output
                    },
                    artifacts: vec![],
                    success: Some(success),
                    failure_code: (!success).then(|| "run_failed".to_owned()),
                    reason_code: if cancelled {
                        Some("cancelled".to_owned())
                    } else {
                        (!success).then(|| "run_failed".to_owned())
                    },
                    started_at,
                    finished_at: Some(timestamp()),
                    read: false,
                };
                // The transcript persists in the shared session store so the
                // run stays inspectable from the sidebar like any other task.
                let store_directory = std::env::var_os("AGENT_CORE_SESSION_DIR")
                    .map(PathBuf::from)
                    .unwrap_or_else(SessionStore::default_directory);
                let _ = SessionStore::new(store_directory).save(&session);
                let delivery = AutomationDelivery {
                    id: format!("delivery-{}-{started_at}", definition.id),
                    dedupe_key: format!("automation:{}:{started_at}", definition.id),
                    automation_id: definition.id.clone(),
                    automation_name: definition.name.clone(),
                    run_id: record.id.clone(),
                    success,
                    summary: record.output.chars().take(4000).collect(),
                    status: AutomationDeliveryStatus::Pending,
                    attempt_count: 0,
                    next_attempt_at: 0,
                    lease_expire_at: None,
                    last_error: None,
                    created_at: timestamp(),
                    finished_at: None,
                };
                if let Ok(store) = store.lock() {
                    let _ = store.record_run(record.clone(), Some(delivery));
                    let title = match (record.kind, success) {
                        (AutomationRunKind::Missed, true) => {
                            format!("定时任务【{}】补跑成功", definition.name)
                        }
                        (AutomationRunKind::Missed, false) => {
                            format!("定时任务【{}】补跑失败", definition.name)
                        }
                        (_, true) => format!("定时任务【{}】运行成功", definition.name),
                        (_, false) => format!("定时任务【{}】运行失败", definition.name),
                    };
                    // Deliveries first, notifications after: this pass's save
                    // would overwrite any rows written inside the sender.
                    if let Ok(delivered) =
                        store.process_deliveries_with(timestamp(), 0.5, |_| Ok(()))
                    {
                        for delivery in delivered {
                            let _ = store.push_notification(AutomationNotification {
                                id: format!("notify-{}", delivery.id),
                                title: title.clone(),
                                body: delivery.summary.chars().take(500).collect(),
                                created_at: timestamp(),
                                read: false,
                            });
                        }
                    }
                }
                let _ = sender.send(AutomationEvent::RunFinished(record));
                active.fetch_sub(1, Ordering::SeqCst);
                repaint.request_repaint();
            })
            .ok();
        if let Some(handle) = handle {
            self.schedules
                .runs
                .push(AutomationRunWorker { signal, handle });
        }
        self.status = format!("定时任务【{}】已开始运行", automation_name);
        ctx.request_repaint();
    }

    pub(super) fn poll_automations(&mut self, ctx: &egui::Context) {
        if self
            .schedules
            .io
            .as_ref()
            .is_some_and(|io| io.handle.is_finished())
        {
            if let Some(io) = self.schedules.io.take() {
                let _ = io.handle.join();
            }
        }
        self.schedules
            .runs
            .retain(|worker| !worker.handle.is_finished());
        let Some(receiver) = self.automation_events.take() else {
            return;
        };
        let mut run_finished = false;
        for _ in 0..64 {
            match receiver.try_recv() {
                Ok(event) => match event {
                    AutomationEvent::Catalog(catalog) => {
                        if let Ok(catalog) = &catalog {
                            self.schedules.unread_notifications = catalog
                                .notifications
                                .iter()
                                .filter(|notification| !notification.read)
                                .count();
                        }
                        self.schedules.catalog = Some(catalog);
                    }
                    AutomationEvent::Scheduled(outcomes) => {
                        let due: Vec<_> = outcomes
                            .iter()
                            .filter_map(|outcome| match &outcome.decision {
                                DueDecision::Run {
                                    kind,
                                    missed_scheduled_at,
                                } => {
                                    Some((outcome.definition.clone(), *kind, *missed_scheduled_at))
                                }
                                _ => None,
                            })
                            .collect();
                        for (definition, kind, missed) in due {
                            self.spawn_automation_run(definition, kind, missed, ctx);
                        }
                        let notes: Vec<String> = outcomes
                            .iter()
                            .filter_map(|outcome| match &outcome.decision {
                                DueDecision::SkipMissed { reason_code } => Some(format!(
                                    "定时任务【{}】错过的批次已跳过（{reason_code}）",
                                    outcome.definition.name
                                )),
                                DueDecision::Expire { reason_code } => Some(format!(
                                    "定时任务【{}】已过期（{reason_code}）",
                                    outcome.definition.name
                                )),
                                _ => None,
                            })
                            .collect();
                        if !notes.is_empty() {
                            self.status = notes.join("；");
                        }
                    }
                    AutomationEvent::RunFinished(record) => {
                        run_finished = true;
                        self.status = format!(
                            "定时任务【{}】{}",
                            record.automation_name,
                            if record.success.unwrap_or(false) {
                                "运行完成"
                            } else {
                                "运行失败"
                            }
                        );
                        self.request_automation_catalog(ctx);
                    }
                    AutomationEvent::StoreError(error) => {
                        self.status = format!("定时任务调度失败：{error}");
                    }
                },
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    return;
                }
            }
        }
        self.automation_events = Some(receiver);
        if run_finished {
            self.sessions = self.store.list().unwrap_or_else(|_| self.sessions.clone());
        }
    }

    pub(super) fn automations_ui(&mut self, ctx: &egui::Context) {
        let palette = shell::palette(ctx);
        let mut run_now = None;
        let mut toggle = None;
        let mut edit = None;
        let mut delete = None;
        let mut create = false;
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(palette.background).inner_margin(24))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.heading("定时任务");
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("刷新").clicked() {
                            self.request_automation_catalog(ctx);
                        }
                        if ui.button("运行记录").clicked() {
                            self.schedules.records_open = !self.schedules.records_open;
                        }
                        if ui
                            .button(format!("消息 ({})", self.schedules.unread_notifications))
                            .on_hover_text("定时任务运行通知")
                            .clicked()
                        {
                            self.schedules.notifications_open = !self.schedules.notifications_open;
                        }
                        if ui
                            .add_enabled(
                                self.schedules.editor.is_none(),
                                egui::Button::new("新建定时任务"),
                            )
                            .clicked()
                        {
                            create = true;
                        }
                    });
                });
                ui.add_space(8.0);
                match &self.schedules.catalog {
                    None => {
                        ui.label("正在读取定时任务…");
                    }
                    Some(Err(error)) => {
                        ui.label(format!("定时任务存储读取失败：{error}"));
                    }
                    Some(Ok(_)) => {
                        let definitions: Vec<AutomationDefinition> = self
                            .schedules
                            .catalog
                            .as_ref()
                            .and_then(|result| result.as_ref().ok())
                            .map(|catalog| {
                                catalog
                                    .definitions
                                    .iter()
                                    .filter(|definition| definition.deleted_at.is_none())
                                    .cloned()
                                    .collect()
                            })
                            .unwrap_or_default();
                        if definitions.is_empty() {
                            ui.label(
                                "还没有定时任务。点击右上角“新建定时任务”，让任务按计划自动运行。",
                            );
                        }
                        egui::ScrollArea::vertical().show(ui, |ui| {
                            for definition in &definitions {
                                let last_error = self
                                    .schedules
                                    .catalog
                                    .as_ref()
                                    .and_then(|result| result.as_ref().ok())
                                    .and_then(|catalog| {
                                        catalog.runtime.get(&definition.id).cloned()
                                    })
                                    .and_then(|runtime| runtime.last_error);
                                ui.group(|ui| {
                                    ui.horizontal(|ui| {
                                        ui.vertical(|ui| {
                                            ui.strong(&definition.name);
                                            ui.label(schedule_description(&definition.schedule));
                                            ui.label(format!(
                                                "工作区 · {}",
                                                definition
                                                    .workspaces
                                                    .first()
                                                    .map(|path| path.display().to_string())
                                                    .unwrap_or_default()
                                            ));
                                            ui.label(match definition.next_run_at {
                                                Some(next) => {
                                                    format!("下次运行 {}", format_time(next))
                                                }
                                                None => "暂无计划批次".to_owned(),
                                            });
                                            ui.label(format!(
                                                "权限 · {}",
                                                if definition.allow_commands {
                                                    "完全访问"
                                                } else if definition.allow_changes {
                                                    "可写文件"
                                                } else {
                                                    "只读"
                                                }
                                            ));
                                            ui.label(format!(
                                                "状态 · {}",
                                                status_label(definition, last_error.as_ref())
                                            ));
                                            if let Some(error) = last_error {
                                                ui.label(error);
                                            }
                                        });
                                        ui.with_layout(
                                            egui::Layout::right_to_left(egui::Align::TOP),
                                            |ui| {
                                                if ui.button("删除").clicked() {
                                                    delete = Some(definition.id.clone());
                                                }
                                                if ui.button("编辑").clicked() {
                                                    edit = Some(definition.id.clone());
                                                }
                                                if ui
                                                    .button(
                                                        if definition.status
                                                            == AutomationStatus::Active
                                                        {
                                                            "暂停"
                                                        } else {
                                                            "恢复"
                                                        },
                                                    )
                                                    .clicked()
                                                {
                                                    toggle = Some(definition.id.clone());
                                                }
                                                if ui.button("立即运行").clicked() {
                                                    run_now = Some(definition.id.clone());
                                                }
                                            },
                                        );
                                    });
                                });
                                ui.add_space(4.0);
                            }
                        });
                    }
                }
            });
        if create {
            self.schedules.editor = Some(AutomationEditor::for_new());
        }
        if let Some(id) = run_now {
            self.run_automation_now(id, ctx);
        }
        if let Some(id) = toggle {
            let revision = catalog_revision(&self.schedules.catalog);
            self.spawn_automation_io(ctx, move |store| {
                let _ = store.update_definition(&id, revision, |definition| {
                    definition.status = if definition.status == AutomationStatus::Active {
                        AutomationStatus::Paused
                    } else {
                        AutomationStatus::Active
                    };
                    Ok(())
                });
            });
        }
        if let Some(id) = edit {
            if let Some(definition) = self
                .schedules
                .catalog
                .as_ref()
                .and_then(|result| result.as_ref().ok())
                .and_then(|catalog| {
                    catalog
                        .definitions
                        .iter()
                        .find(|other| other.id == id)
                        .cloned()
                })
            {
                self.schedules.editor = Some(editor_for(&definition));
            }
        }
        if let Some(id) = delete {
            let revision = catalog_revision(&self.schedules.catalog);
            self.spawn_automation_io(ctx, move |store| {
                let _ = store.delete(&id, revision);
            });
        }
        self.automation_editor_ui(ctx);
        self.automation_records_ui(ctx);
        self.automation_notifications_ui(ctx);
    }

    fn automation_editor_ui(&mut self, ctx: &egui::Context) {
        if self.schedules.editor.is_none() {
            return;
        }
        let mut window_open = true;
        let mut save = false;
        let mut cancel = false;
        let palette = shell::palette(ctx);
        egui::Window::new("定时任务设置")
            .id(egui::Id::new("automation-editor"))
            .open(&mut window_open)
            .default_width(520.0)
            .show(ctx, |ui| {
                let Some(editor) = &mut self.schedules.editor else {
                    return;
                };
                egui::Grid::new("automation-editor-grid")
                    .num_columns(2)
                    .spacing([12.0, 8.0])
                    .show(ui, |ui| {
                        ui.label("名称");
                        ui.text_edit_singleline(&mut editor.name);
                        ui.end_row();
                        ui.label("提示词");
                        ui.add(
                            egui::TextEdit::multiline(&mut editor.prompt)
                                .desired_rows(3)
                                .desired_width(320.0),
                        );
                        ui.end_row();
                        ui.label("执行频率");
                        ui.horizontal(|ui| {
                            egui::ComboBox::from_id_salt("automation-frequency")
                                .selected_text(match editor.frequency {
                                    EditorFrequency::Once => "单次",
                                    EditorFrequency::Minutely => "按分钟间隔",
                                    EditorFrequency::Hourly => "按小时间隔",
                                    EditorFrequency::Daily => "每天",
                                    EditorFrequency::Weekly => "每周",
                                    EditorFrequency::Monthly => "每月",
                                })
                                .show_ui(ui, |ui| {
                                    const MODES: [(EditorFrequency, &str); 6] = [
                                        (EditorFrequency::Once, "单次"),
                                        (EditorFrequency::Minutely, "按分钟间隔"),
                                        (EditorFrequency::Hourly, "按小时间隔"),
                                        (EditorFrequency::Daily, "每天"),
                                        (EditorFrequency::Weekly, "每周"),
                                        (EditorFrequency::Monthly, "每月"),
                                    ];
                                    for (mode, label) in MODES {
                                        ui.selectable_value(&mut editor.frequency, mode, label);
                                    }
                                });
                        });
                        ui.end_row();
                        match editor.frequency {
                            EditorFrequency::Once => {
                                ui.label("运行时间");
                                ui.text_edit_singleline(&mut editor.once_at)
                                    .on_hover_text("格式：YYYY-MM-DD HH:MM");
                                ui.end_row();
                            }
                            EditorFrequency::Minutely | EditorFrequency::Hourly => {
                                ui.label("间隔");
                                ui.add(egui::DragValue::new(&mut editor.interval).range(1..=1000));
                                ui.end_row();
                            }
                            EditorFrequency::Daily => {
                                ui.label("时间");
                                ui.horizontal(|ui| {
                                    ui.add(egui::DragValue::new(&mut editor.hour).range(0..=23));
                                    ui.label(":");
                                    ui.add(egui::DragValue::new(&mut editor.minute).range(0..=59));
                                    ui.label("每 N 天");
                                    ui.add(
                                        egui::DragValue::new(&mut editor.interval).range(1..=365),
                                    );
                                });
                                ui.end_row();
                            }
                            EditorFrequency::Weekly => {
                                ui.label("星期");
                                ui.horizontal(|ui| {
                                    const WEEKDAYS: [&str; 7] =
                                        ["周一", "周二", "周三", "周四", "周五", "周六", "周日"];
                                    for (index, label) in WEEKDAYS.iter().enumerate() {
                                        ui.selectable_value(&mut editor.weekday, index, *label);
                                    }
                                });
                                ui.end_row();
                                ui.label("时间");
                                ui.horizontal(|ui| {
                                    ui.add(egui::DragValue::new(&mut editor.hour).range(0..=23));
                                    ui.label(":");
                                    ui.add(egui::DragValue::new(&mut editor.minute).range(0..=59));
                                });
                                ui.end_row();
                            }
                            EditorFrequency::Monthly => {
                                ui.label("几号");
                                ui.add(egui::DragValue::new(&mut editor.monthday).range(1..=31));
                                ui.end_row();
                                ui.label("时间");
                                ui.horizontal(|ui| {
                                    ui.add(egui::DragValue::new(&mut editor.hour).range(0..=23));
                                    ui.label(":");
                                    ui.add(egui::DragValue::new(&mut editor.minute).range(0..=59));
                                });
                                ui.end_row();
                            }
                        }
                        ui.label("有效期至");
                        ui.text_edit_singleline(&mut editor.valid_until)
                            .on_hover_text("格式：YYYY-MM-DD（空=长期有效）");
                        ui.end_row();
                        ui.label("工作区");
                        ui.label(self.workspace.trim().to_owned()).on_hover_text(
                            "定时任务在当前工作区运行；如需更换请先在左侧设置中修改工作区",
                        );
                        ui.end_row();
                        ui.label("执行权限");
                        ui.vertical(|ui| {
                            ui.radio_value(&mut editor.access, EditorAccess::ReadOnly, "只读");
                            ui.radio_value(
                                &mut editor.access,
                                EditorAccess::WriteFiles,
                                "可写文件",
                            );
                            ui.radio_value(&mut editor.access, EditorAccess::Full, "完全访问");
                        });
                        ui.end_row();
                    });
                ui.add_space(8.0);
                ui.colored_label(palette.muted, "运行使用当前配置的模型服务。");
                ui.horizontal(|ui| {
                    if ui.button("保存").clicked() {
                        save = true;
                    }
                    if ui.button("取消").clicked() {
                        cancel = true;
                    }
                });
            });
        if save {
            self.save_automation(ctx);
        } else if cancel || !window_open {
            self.schedules.editor = None;
        }
    }

    fn automation_records_ui(&mut self, ctx: &egui::Context) {
        if !self.schedules.records_open {
            return;
        }
        let mut open = self.schedules.records_open;
        egui::Window::new("运行记录")
            .id(egui::Id::new("automation-records"))
            .open(&mut open)
            .default_width(680.0)
            .default_height(420.0)
            .show(ctx, |ui| {
                let records: Vec<AutomationRunRecord> = self
                    .schedules
                    .catalog
                    .as_ref()
                    .and_then(|result| result.as_ref().ok())
                    .map(|catalog| catalog.runs.clone())
                    .unwrap_or_default();
                if records.is_empty() {
                    ui.label("还没有运行记录。");
                    return;
                }
                egui::ScrollArea::vertical().show(ui, |ui| {
                    for record in &records {
                        ui.group(|ui| {
                            ui.horizontal(|ui| {
                                ui.strong(&record.automation_name);
                                ui.label(match record.status {
                                    AutomationRunStatus::Running => "运行中",
                                    AutomationRunStatus::Success => "成功",
                                    AutomationRunStatus::Failed => "失败",
                                    AutomationRunStatus::Skipped => "已跳过",
                                });
                                ui.label(match record.kind {
                                    AutomationRunKind::Scheduled => "计划",
                                    AutomationRunKind::Missed => "补跑",
                                    AutomationRunKind::Manual => "手动",
                                });
                                ui.label(format_time(record.started_at));
                                ui.label(record.workspace.display().to_string());
                            });
                            if let Some(missed) = record.missed_scheduled_at {
                                ui.label(format!("原定时间 {}", format_time(missed)));
                            }
                            egui::CollapsingHeader::new("输出").show(ui, |ui| {
                                ui.monospace(record.output.chars().take(4000).collect::<String>());
                            });
                        });
                    }
                });
            });
        self.schedules.records_open = open;
    }

    fn automation_notifications_ui(&mut self, ctx: &egui::Context) {
        if !self.schedules.notifications_open {
            return;
        }
        let mut open = self.schedules.notifications_open;
        let mut mark_read = false;
        egui::Window::new("消息中心")
            .id(egui::Id::new("automation-notifications"))
            .open(&mut open)
            .default_width(500.0)
            .default_height(380.0)
            .show(ctx, |ui| {
                let notifications: Vec<AutomationNotification> = self
                    .schedules
                    .catalog
                    .as_ref()
                    .and_then(|result| result.as_ref().ok())
                    .map(|catalog| catalog.notifications.clone())
                    .unwrap_or_default();
                if notifications.is_empty() {
                    ui.label("暂无消息。定时任务运行结束后会在这里通知。");
                    return;
                }
                if ui.button("全部标记已读").clicked() {
                    mark_read = true;
                }
                egui::ScrollArea::vertical().show(ui, |ui| {
                    for notification in &notifications {
                        ui.group(|ui| {
                            ui.horizontal(|ui| {
                                if !notification.read {
                                    ui.colored_label(egui::Color32::LIGHT_BLUE, "●");
                                }
                                ui.strong(&notification.title);
                            });
                            ui.label(notification.body.chars().take(300).collect::<String>());
                            ui.label(format_time(notification.created_at));
                        });
                    }
                });
            });
        self.schedules.notifications_open = open;
        if mark_read {
            self.spawn_automation_io(ctx, |store| {
                let _ = store.mark_notifications_read();
            });
        }
    }
}
