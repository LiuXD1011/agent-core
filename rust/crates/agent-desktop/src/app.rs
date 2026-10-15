use crate::shell;
use crate::ui_dialog::DesktopDialogs;
use agent_core::agent_loop::{run_agent_loop, LoopConfig, NoopLoopHooks, Provider};
use agent_core::tools::NoopToolHooks;
use agent_core::types::{
    timestamp, AgentEvent, AssistantMessage, CancellationToken, Content, Context, Message,
    ThinkingLevel,
};
use agent_runtime::auth::{
    CodexOAuthClient, NativeOAuthClient, NativeOAuthConfig, OAuthCredential, CODEX_PROVIDER,
};
use agent_runtime::auth_storage::{default_auth_directory, AuthStorage};
use agent_runtime::automatic::{
    record_manual_checkpoint, AutomationHandle, ModelBudget, RuntimeEvent,
};
use agent_runtime::builtins::{create_tools, tool_definitions};
use agent_runtime::catalog::search;
use agent_runtime::compaction::compact_session;
use agent_runtime::credentials::resolve_api_key;
use agent_runtime::extensions::{
    execute_extension_command, extension_context, load_native_extensions, notify_model_select,
    notify_session_info_changed, prepare_input, prepare_input_with_behavior,
    prepare_session_change, record_model_selection, stored_model_selection, CommandResult,
    ExtensionMode, ForkPosition, InputSource, InputStreamingBehavior, LifecycleEvent,
    ModelIdentity, ModelSelectSource, NotificationLevel, SessionBeforeEvent, SessionShutdownReason,
    SessionStartReason, SessionSwitchReason, SharedRegistry, UiRequest,
};
use agent_runtime::instructions::load_context_files;
use agent_runtime::legacy_session::{load_legacy_session, LeafSelection};
use agent_runtime::provider::{
    AzureOptions, BedrockCacheRetention, BedrockOptions, ModelProvider, ProviderMode,
    ProviderOptions, ProviderSettings, VertexOptions,
};
use agent_runtime::resources::{
    format_skills_for_prompt, load_resources, prepare_prompt, ResourceLoad, ResourceOptions,
};
use agent_runtime::retry::{RetryOptions, RetryingProvider};
use agent_runtime::runtime::{run_prompt, MessageQueues, QueueItemId, QueueLane, RunOptions};
use agent_runtime::session::{Session, SessionStore};
use agent_runtime::thinking::{notify_thinking_level_select, restore_thinking_settings};
use base64::{engine::general_purpose::STANDARD, Engine};
use eframe::egui::{self, FontFamily, RichText};
use std::collections::BTreeMap;
use std::io::Cursor;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError};
use std::thread::JoinHandle;
use std::time::Duration;

#[path = "app_connections.rs"]
mod connections_ui;
use connections_ui::ConnectionsState;

#[path = "app_workbench.rs"]
mod workbench_ui;
use workbench_ui::{WorkbenchPage, WorkbenchState};

#[path = "app_task_management.rs"]
mod task_management_ui;
use task_management_ui::TaskManagement;

#[cfg(test)]
#[path = "app_auth_tests.rs"]
mod native_auth_tests;
#[cfg(test)]
#[path = "app_ui_tests.rs"]
mod native_ui_tests;

#[derive(Clone, Copy)]
pub struct AppKeybindings {
    pub send: egui::KeyboardShortcut,
}

pub const DEFAULT_APP_KEYBINDINGS: AppKeybindings = AppKeybindings {
    send: egui::KeyboardShortcut::new(egui::Modifiers::CTRL, egui::Key::Enter),
};

enum WorkerMessage {
    Event(AgentEvent),
    Finished(Box<Session>, Result<(), String>),
    InputHandled,
    InputFailed(String),
    ForkFinished(Box<Session>, Result<String, String>),
    SessionCommitted(Box<Session>, SyncSender<()>),
    ExtensionFinished(
        Box<Session>,
        SharedRegistry,
        Vec<PathBuf>,
        Result<Option<CommandResult>, String>,
    ),
}

struct QueueInputResult {
    session_id: String,
    lane: QueueLane,
    result: Result<Option<Message>, String>,
    persisted_id: Option<QueueItemId>,
    workspace: Option<PathBuf>,
}

#[derive(Clone)]
struct ModelConfiguration {
    settings: ProviderSettings,
    context_window: u64,
    max_output_tokens: u64,
}

enum ModelSelectionMessage {
    Prepared(Box<Session>, Box<ModelConfiguration>),
    Settled {
        saved: bool,
        result: Result<(), String>,
    },
}

enum ExtensionAction {
    Load(Vec<PathBuf>),
    Command(String, String),
    Replace(Box<Session>, SessionStartReason),
    Fork(String),
    Name(String),
    Quit,
}

#[derive(Clone, Copy)]
enum AuthAction {
    Login,
    Status,
    Logout,
}
enum AuthOutcome {
    Credential(Option<OAuthCredential>),
    ApiKey,
    LoggedOut,
}
enum AuthMessage {
    Authorization(String),
    DeviceCode(String),
    Finished(String, Result<AuthOutcome, String>),
}

pub struct DesktopApp {
    pub keybindings: AppKeybindings,
    session: Session,
    sessions: Vec<Session>,
    store: SessionStore,
    settings: ProviderSettings,
    settings_draft: Option<ModelConfiguration>,
    model_drafts: BTreeMap<String, ModelConfiguration>,
    workspace: String,
    session_name_draft: String,
    input: String,
    input_images: Vec<Content>,
    pending_images: Vec<Content>,
    queue_images: Vec<Content>,
    status: String,
    running: bool,
    compacting: bool,
    closing: bool,
    allow_changes: bool,
    allow_commands: bool,
    receiver: Option<Receiver<WorkerMessage>>,
    signal: CancellationToken,
    partial: Option<AssistantMessage>,
    tool_activity: Vec<String>,
    queues: MessageQueues,
    queue_halted: bool,
    import_path: String,
    import_leaf: String,
    model_filter: String,
    images: BTreeMap<String, Result<egui::TextureHandle, String>>,
    worker: Option<JoinHandle<()>>,
    auth_worker: Option<JoinHandle<()>>,
    auth_receiver: Option<Receiver<AuthMessage>>,
    auth_signal: CancellationToken,
    auth_path: PathBuf,
    auth_status: String,
    auth_provider: String,
    auth_manual: String,
    auth_manual_sender: Option<SyncSender<String>>,
    auth_device_code: Option<String>,
    login_url: Option<String>,
    auto_compaction: bool,
    retry_enabled: bool,
    context_window: u64,
    max_output_tokens: u64,
    reserve_tokens: u64,
    automation: AutomationHandle,
    runtime_activity: String,
    tree_target: String,
    extensions: SharedRegistry,
    extension_paths: String,
    loaded_extension_paths: Vec<PathBuf>,
    extension_operation: bool,
    extension_display: Vec<String>,
    extension_statuses: BTreeMap<String, String>,
    pending_draft: Option<String>,
    pending_queue_id: Option<QueueItemId>,
    queue_draft: Option<String>,
    queued_drafts: Vec<(QueueItemId, Message, String)>,
    queue_worker: Option<JoinHandle<()>>,
    queue_receiver: Option<Receiver<QueueInputResult>>,
    queue_signal: CancellationToken,
    model_worker: Option<JoinHandle<()>>,
    model_receiver: Option<Receiver<ModelSelectionMessage>>,
    model_commit: Option<SyncSender<bool>>,
    model_before: Option<(Session, ModelConfiguration)>,
    model_committed: bool,
    ui_dialogs: DesktopDialogs,
    settings_open: bool,
    connections: ConnectionsState,
    workbench: WorkbenchState,
    tasks: TaskManagement,
}

fn configure_ui(ctx: &egui::Context) {
    shell::configure(ctx);
    let mut fonts = egui::FontDefinitions::default();
    if let Some(directory) = std::env::var_os("WINDIR") {
        for name in ["msyh.ttc", "simhei.ttf"] {
            if let Ok(bytes) = std::fs::read(PathBuf::from(&directory).join("Fonts").join(name)) {
                fonts
                    .font_data
                    .insert("cjk".into(), egui::FontData::from_owned(bytes).into());
                fonts
                    .families
                    .entry(FontFamily::Proportional)
                    .or_default()
                    .insert(0, "cjk".into());
                fonts
                    .families
                    .entry(FontFamily::Monospace)
                    .or_default()
                    .push("cjk".into());
                break;
            }
        }
    }
    ctx.set_fonts(fonts);
}

impl DesktopApp {
    pub fn new(ctx: &egui::Context) -> Self {
        let store = SessionStore::new(
            std::env::var_os("AGENT_CORE_SESSION_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(SessionStore::default_directory),
        );
        let mut app = Self::with_store(ctx, store);
        app.enable_task_management(ctx);
        app.enable_durable_queues(ctx);
        app
    }
    fn with_store(ctx: &egui::Context, store: SessionStore) -> Self {
        configure_ui(ctx);
        let (sessions, status) = match store.load_with_diagnostics() {
            Ok(loaded) => {
                let status = if loaded.diagnostics.is_empty() {
                    "就绪 · Rust 原生运行时".into()
                } else {
                    format!("已跳过 {} 个损坏会话文件", loaded.diagnostics.len())
                };
                (loaded.sessions, status)
            }
            Err(error) => (vec![], format!("读取会话失败：{error}")),
        };
        let session = sessions.first().cloned().unwrap_or_default();
        let workspace = session.workspace.to_string_lossy().into_owned();
        let session_name_draft = session
            .get_session_name()
            .ok()
            .flatten()
            .unwrap_or_default();
        let auth_path = default_auth_directory().join("credentials.dat");
        let mut app = Self {
            keybindings: DEFAULT_APP_KEYBINDINGS,
            session,
            sessions,
            store,
            settings: ProviderSettings {
                options: ProviderOptions {
                    provider_id: "demo".into(),
                    auth_storage_path: Some(auth_path.clone()),
                    ..Default::default()
                },
                api_key: std::env::var("AGENT_CORE_API_KEY")
                    .or_else(|_| std::env::var("OPENAI_API_KEY"))
                    .unwrap_or_default(),
                ..Default::default()
            },
            settings_draft: None,
            model_drafts: BTreeMap::new(),
            workspace,
            session_name_draft,
            input: String::new(),
            input_images: Vec::new(),
            pending_images: Vec::new(),
            queue_images: Vec::new(),
            status,
            running: false,
            compacting: false,
            closing: false,
            allow_changes: false,
            allow_commands: false,
            receiver: None,
            signal: CancellationToken::new(),
            partial: None,
            tool_activity: vec![],
            queues: MessageQueues::default(),
            queue_halted: false,
            import_path: String::new(),
            import_leaf: String::new(),
            model_filter: String::new(),
            images: BTreeMap::new(),
            worker: None,
            auth_worker: None,
            auth_receiver: None,
            auth_signal: CancellationToken::new(),
            auth_path,
            auth_status: "登录状态尚未读取".into(),
            auth_provider: CODEX_PROVIDER.into(),
            auth_manual: String::new(),
            auth_manual_sender: None,
            auth_device_code: None,
            login_url: None,
            auto_compaction: false,
            retry_enabled: false,
            context_window: 0,
            max_output_tokens: 0,
            reserve_tokens: 16_384,
            automation: AutomationHandle::default(),
            runtime_activity: String::new(),
            tree_target: String::new(),
            extensions: SharedRegistry::default(),
            extension_paths: String::new(),
            loaded_extension_paths: Vec::new(),
            extension_operation: false,
            extension_display: Vec::new(),
            extension_statuses: BTreeMap::new(),
            pending_draft: None,
            pending_queue_id: None,
            queue_draft: None,
            queued_drafts: Vec::new(),
            queue_worker: None,
            queue_receiver: None,
            queue_signal: CancellationToken::new(),
            model_worker: None,
            model_receiver: None,
            model_commit: None,
            model_before: None,
            model_committed: false,
            ui_dialogs: DesktopDialogs::default(),
            settings_open: false,
            connections: ConnectionsState::default(),
            workbench: WorkbenchState::default(),
            tasks: TaskManagement::default(),
        };
        if let Err(error) = app.extensions.bind_ui(app.ui_dialogs.broker().handle()) {
            app.status = error;
        }
        app.start_model_restore(ctx);
        app.sync_queue_session();
        app
    }
    fn busy(&self) -> bool {
        self.queue_io_busy()
            || self.connections.busy()
            || self.running
            || self.ui_dialogs.has_pending()
            || self.auth_receiver.is_some()
            || self.queue_receiver.is_some()
            || self.model_receiver.is_some()
    }
    fn provider_id(&self) -> &str {
        if !self.settings.options.provider_id.is_empty() {
            return &self.settings.options.provider_id;
        }
        match self.settings.mode {
            ProviderMode::Demo => "demo",
            ProviderMode::OpenAiCompatible | ProviderMode::OpenAiResponses => "openai",
            ProviderMode::OpenAiCodex => CODEX_PROVIDER,
            ProviderMode::Anthropic => "anthropic",
            ProviderMode::Google => "google",
            ProviderMode::AzureOpenAiResponses => "azure-openai-responses",
            ProviderMode::GoogleVertex => "google-vertex",
            ProviderMode::AmazonBedrock => "amazon-bedrock",
            ProviderMode::Mistral => "mistral",
            ProviderMode::PiMessages => "pi-messages",
        }
    }
    #[cfg(test)]
    fn update_model_budget(&mut self) {
        if self.settings.mode == ProviderMode::AzureOpenAiResponses {
            self.context_window = 0;
            self.max_output_tokens = 0;
            return;
        }
        if let Some(model) = ModelBudget::from_catalog(self.provider_id(), &self.settings.model) {
            self.context_window = model.context_window;
            self.max_output_tokens = model.max_output_tokens;
        } else {
            self.context_window = 0;
            self.max_output_tokens = 0;
        }
    }
    fn model_identity(&self) -> Option<ModelIdentity> {
        (self.settings.mode != ProviderMode::Demo).then(|| ModelIdentity {
            provider: self.provider_id().into(),
            model: self.settings.model.clone(),
        })
    }
    fn model_budget(&self) -> Option<ModelBudget> {
        let catalog = (self.settings.mode != ProviderMode::AzureOpenAiResponses)
            .then(|| ModelBudget::from_catalog(self.provider_id(), &self.settings.model))
            .flatten();
        let mut budget = catalog.or_else(|| {
            (self.context_window > 0).then(|| ModelBudget {
                provider: self.provider_id().into(),
                id: self.settings.model.clone(),
                context_window: self.context_window,
                max_output_tokens: self.max_output_tokens,
                cost: serde_json::Value::Null,
            })
        });
        if self.settings.mode == ProviderMode::AmazonBedrock {
            if let (Some(budget), Some(limit)) =
                (&mut budget, self.settings.options.bedrock.max_tokens)
            {
                budget.max_output_tokens = u64::from(limit);
            }
        }
        budget
    }
    fn model_configuration(&self) -> ModelConfiguration {
        ModelConfiguration {
            settings: self.settings.clone(),
            context_window: self.context_window,
            max_output_tokens: self.max_output_tokens,
        }
    }
    fn commit_model_configuration(&mut self, configuration: ModelConfiguration) {
        self.settings = configuration.settings.clone();
        self.context_window = configuration.context_window;
        self.max_output_tokens = configuration.max_output_tokens;
        self.settings_draft = Some(configuration);
    }
    fn start_model_selection(
        &mut self,
        ctx: &egui::Context,
        mut configuration: ModelConfiguration,
        source: ModelSelectSource,
    ) {
        if self.busy() || self.closing {
            return;
        }
        let mut candidate = self.session.clone();
        let previous = settings_identity(&self.settings);
        let previous_thinking: ThinkingLevel =
            self.settings.options.thinking_level.unwrap_or_default();
        let registry = self.extensions.clone();
        let store = self.store.clone();
        let mut options = self.base_run_options();
        self.model_before = Some((self.session.clone(), self.model_configuration()));
        self.model_committed = false;
        self.signal = CancellationToken::new();
        let signal = self.signal.clone();
        let (sender, receiver) = mpsc::sync_channel(2);
        let (commit, committed) = mpsc::sync_channel(1);
        self.model_receiver = Some(receiver);
        self.model_commit = Some(commit);
        self.status = "正在验证模型配置…".into();
        let repaint = ctx.clone();
        self.model_worker = Some(std::thread::spawn(move || {
            let mut saved = false;
            let result = catch_unwind(AssertUnwindSafe(|| {
                if signal.is_cancelled() {
                    return Err("Model selection aborted before commit".into());
                }
                if configuration.settings.mode == ProviderMode::Demo {
                    configuration.settings.options.provider_id = "demo".into();
                }
                if configuration.settings.mode == ProviderMode::OpenAiCodex
                    && configuration.settings.options.oauth.is_none()
                {
                    if let Some(path) = &configuration.settings.options.auth_storage_path {
                        configuration.settings.options.oauth = AuthStorage::new(path)
                            .read(CODEX_PROVIDER, &signal)?
                            .map(|value| {
                                serde_json::from_value(value)
                                    .map_err(|_| "已存登录凭据格式错误".to_owned())
                            })
                            .transpose()?;
                    }
                }
                // Selection validates local configuration only. STS/ADC exchanges and
                // paid provider calls belong to the later cancellable request worker.
                if source == ModelSelectSource::Restore && configuration.context_window == 0 {
                    if let (Some(service), Some(path)) = (
                        agent_runtime::connections::Service::from_id(
                            configuration.settings.provider_id(),
                        ),
                        configuration.settings.options.auth_storage_path.as_ref(),
                    ) {
                        if let Ok(connection) = agent_runtime::connections::load_connection(
                            &AuthStorage::new(path),
                            service,
                            &signal,
                        ) {
                            (
                                configuration.context_window,
                                configuration.max_output_tokens,
                            ) = connection.budget(&configuration.settings.model);
                        }
                    }
                }
                if source == ModelSelectSource::Restore {
                    restore_thinking_settings(&candidate, &mut configuration.settings)?;
                }
                if let Some(level) = configuration.settings.options.thinking_level {
                    let level = configuration.settings.clamp_thinking_level(level);
                    configuration.settings.options.thinking_level = Some(level);
                    candidate.append_thinking_level(level)?;
                }
                configuration.settings.validate()?;
                options.automation.model = configuration_budget(&configuration);
                if configuration.settings.mode != ProviderMode::Demo {
                    options.automation.validate()?;
                }
                let model = settings_identity(&configuration.settings);
                if source == ModelSelectSource::Set {
                    record_model_selection(&mut candidate, &model)?;
                }
                if signal.is_cancelled() {
                    return Err("Model selection aborted before commit".into());
                }
                sender
                    .send(ModelSelectionMessage::Prepared(
                        Box::new(candidate.clone()),
                        Box::new(configuration),
                    ))
                    .map_err(|_| "Model selection window closed")?;
                repaint.request_repaint();
                loop {
                    match committed.recv_timeout(Duration::from_millis(10)) {
                        Ok(true) => break,
                        Ok(false) => return Err("Model selection aborted before commit".into()),
                        Err(mpsc::RecvTimeoutError::Disconnected) => {
                            return Err("Model selection window closed".into())
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) if signal.is_cancelled() => {
                            return Err("Model selection aborted before commit".into())
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                    }
                }
                store.save(&candidate)?;
                saved = true;
                notify_thinking_level_select(
                    &candidate,
                    &registry,
                    ExtensionMode::Desktop,
                    Some(model.clone()),
                    candidate.get_thinking_level()?.unwrap_or_default(),
                    previous_thinking,
                    &signal,
                )
                .map_err(|error| {
                    format!("Thinking selection committed; notification failed: {error}")
                })?;
                notify_model_select(
                    &candidate,
                    &registry,
                    ExtensionMode::Desktop,
                    model,
                    Some(previous),
                    source,
                    &signal,
                )
            }))
            .unwrap_or_else(|_| Err("Model configuration worker panicked".into()));
            let _ = sender.send(ModelSelectionMessage::Settled { saved, result });
            repaint.request_repaint();
        }));
    }
    fn start_model_restore(&mut self, ctx: &egui::Context) {
        let identity = match stored_model_selection(&self.session) {
            Ok(identity) => identity,
            Err(error) => {
                self.status = error;
                return;
            }
        };
        let configuration = identity.as_ref().map_or_else(
            || Ok(self.model_configuration()),
            |identity| configuration_for_identity(&self.model_configuration(), identity),
        );
        if identity.is_none()
            && self.session.get_thinking_level().ok().flatten().is_none()
            && self.settings.options.thinking_level.is_none()
        {
            return;
        }
        match configuration {
            Ok(configuration) => {
                self.start_model_selection(ctx, configuration, ModelSelectSource::Restore)
            }
            Err(error) => self.status = error,
        }
    }
    fn poll_model_selection(&mut self) {
        let mut messages = Vec::new();
        let mut disconnected = false;
        if let Some(receiver) = &self.model_receiver {
            loop {
                match receiver.try_recv() {
                    Ok(message) => messages.push(message),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                }
            }
        }
        for message in messages {
            match message {
                ModelSelectionMessage::Prepared(candidate, configuration) => {
                    if self.signal.is_cancelled() || self.closing || candidate.id != self.session.id
                    {
                        if let Some(commit) = self.model_commit.take() {
                            let _ = commit.send(false);
                        }
                    } else {
                        self.session = *candidate;
                        self.commit_model_configuration(*configuration);
                        self.model_committed = true;
                        self.status = "模型配置已应用，正在保存并通知扩展…".into();
                        if let Some(commit) = self.model_commit.take() {
                            let _ = commit.send(true);
                        }
                    }
                }
                ModelSelectionMessage::Settled { saved, result } => {
                    if self.model_committed && !saved {
                        if let Some((session, configuration)) = self.model_before.take() {
                            self.session = session;
                            self.settings = configuration.settings;
                            self.context_window = configuration.context_window;
                            self.max_output_tokens = configuration.max_output_tokens;
                        }
                        self.model_committed = false;
                    }
                    self.status = match result {
                        Ok(()) => "模型配置已应用并保存".into(),
                        Err(_) if self.signal.is_cancelled() && self.model_committed => {
                            "模型已选择并保存；扩展通知已取消".into()
                        }
                        Err(_) if self.signal.is_cancelled() => {
                            "模型选择已取消，原配置与草稿保持不变".into()
                        }
                        Err(error) if self.model_committed => {
                            format!("模型已选择并保存；扩展通知失败：{error}")
                        }
                        Err(error) => format!("模型配置未应用：{error}"),
                    };
                    self.model_receiver = None;
                    self.model_commit = None;
                    self.model_before = None;
                    self.model_committed = false;
                    if let Some(worker) = self.model_worker.take() {
                        let _ = worker.join();
                    }
                }
            }
        }
        if disconnected && self.model_receiver.is_some() {
            self.signal.cancel();
            self.model_receiver = None;
            self.model_commit = None;
            self.model_before = None;
            self.status = if self.model_committed {
                "模型配置已应用；通知工作线程意外结束".into()
            } else {
                "模型工作线程意外结束，原配置保持不变".into()
            };
            self.model_committed = false;
            if let Some(worker) = self.model_worker.take() {
                let _ = worker.join();
            }
        }
    }
    fn base_run_options(&self) -> RunOptions {
        let mut options = RunOptions {
            allow_changes: self.allow_changes,
            allow_commands: self.allow_commands,
            ..Default::default()
        };
        options.automation.enabled = self.auto_compaction;
        options.automation.retry.enabled = self.retry_enabled;
        options.automation.reserve_tokens = self.reserve_tokens;
        options.automation_state = self.automation.clone();
        options.automation.model = self.model_budget();
        options.extensions = self.extensions.clone();
        options.extension_mode = ExtensionMode::Desktop;
        options.extension_model = self.model_identity();
        options
    }
    fn run_options(&self) -> Result<RunOptions, String> {
        let options = self.base_run_options();
        options.automation.validate()?;
        Ok(options)
    }
    fn reset_view(&mut self) {
        self.partial = None;
        self.tool_activity.clear();
        self.sync_queue_session();
        self.images.clear();
        self.tree_target.clear();
        self.automation = AutomationHandle::default();
        self.runtime_activity.clear();
        self.extension_display.clear();
        self.extension_statuses.clear();
    }
    fn navigate_tree(&mut self, target: Option<&str>) -> Result<(), String> {
        if self.busy() {
            return Err("运行期间会话树保持不变".into());
        }
        let mut candidate = self.session.clone();
        candidate.navigate(target)?;
        self.store.save(&candidate)?;
        self.session = candidate;
        self.reset_view();
        self.sessions = self.store.list()?;
        Ok(())
    }
    #[cfg(test)]
    fn fork_tree(&mut self, target: &str) -> Result<(), String> {
        if self.busy() {
            return Err("运行期间会话树保持不变".into());
        }
        let mut candidate = self.session.clone();
        let text = candidate.fork(target)?;
        self.store.save(&candidate)?;
        self.session = candidate;
        self.reset_view();
        self.input = text;
        self.sessions = self.store.list()?;
        Ok(())
    }
    fn clone_tree(&mut self) -> Result<(), String> {
        if self.busy() {
            return Err("运行期间会话树保持不变".into());
        }
        let clone = self.session.clone_current_branch()?;
        self.store.save(&self.session)?;
        self.store.save(&clone)?;
        self.session = clone;
        self.reset_view();
        self.sessions = self.store.list()?;
        Ok(())
    }
    fn start_auth(&mut self, ctx: &egui::Context, action: AuthAction) {
        if self.busy() {
            return;
        }
        self.auth_signal = CancellationToken::new();
        let signal = self.auth_signal.clone();
        let storage = AuthStorage::new(&self.auth_path);
        let client = CodexOAuthClient::new(self.settings.options.codex_oauth.clone());
        let provider = self.auth_provider.clone();
        let native_config = self.settings.options.oauth_config.clone();
        let enterprise = self.settings.options.copilot.enterprise_domain.clone();
        let (manual_sender, manual_receiver) = mpsc::sync_channel(1);
        self.auth_manual_sender = Some(manual_sender);
        self.auth_manual.clear();
        self.auth_device_code = None;
        let (sender, receiver) = mpsc::channel();
        self.auth_receiver = Some(receiver);
        self.login_url = None;
        self.auth_status = "正在处理登录凭据…".into();
        let repaint = ctx.clone();
        self.auth_worker = Some(std::thread::spawn(move || {
            let result = catch_unwind(AssertUnwindSafe(|| match action {
                AuthAction::Login => {
                    let credential = if provider == CODEX_PROVIDER {
                        let login = client.begin_login()?;
                        sender
                            .send(AuthMessage::Authorization(login.authorization_url.clone()))
                            .map_err(|_| "登录窗口已关闭")?;
                        repaint.request_repaint();
                        login.finish(&signal)?
                    } else {
                        let mut config = native_config
                            .clone()
                            .unwrap_or(NativeOAuthConfig::for_provider(&provider)?);
                        if config.provider != provider {
                            return Err("认证服务商配置不匹配".into());
                        }
                        if provider == "github-copilot" && !enterprise.is_empty() {
                            config = config.with_enterprise_domain(&enterprise)?;
                        }
                        let native = NativeOAuthClient::new(config);
                        if matches!(provider.as_str(), "anthropic" | "openrouter") {
                            let login = native.begin_pkce()?;
                            sender
                                .send(AuthMessage::Authorization(login.authorization_url.clone()))
                                .map_err(|_| "登录窗口已关闭")?;
                            repaint.request_repaint();
                            login.finish_with_manual(Some(&manual_receiver), &signal)?
                        } else {
                            let login = native.begin_device(&signal)?;
                            sender
                                .send(AuthMessage::DeviceCode(login.user_code.clone()))
                                .map_err(|_| "登录窗口已关闭")?;
                            sender
                                .send(AuthMessage::Authorization(login.verification_uri.clone()))
                                .map_err(|_| "登录窗口已关闭")?;
                            repaint.request_repaint();
                            login.finish(&signal)?
                        }
                    };
                    storage.write(
                        &provider,
                        serde_json::to_value(&credential).map_err(|_| "凭据序列化失败")?,
                        &CancellationToken::new(),
                    )?;
                    Ok(AuthOutcome::Credential(Some(credential)))
                }
                AuthAction::Status => {
                    let value = storage.read(&provider, &signal)?;
                    if value
                        .as_ref()
                        .is_some_and(|value| value["type"] == "api_key")
                    {
                        return Ok(AuthOutcome::ApiKey);
                    }
                    let credential = value
                        .map(|value| {
                            serde_json::from_value::<OAuthCredential>(value)
                                .map_err(|_| "已存登录凭据格式错误".to_owned())
                        })
                        .transpose()?;
                    if let Some(credential) = &credential {
                        if provider == CODEX_PROVIDER {
                            credential.validate()?;
                        } else {
                            credential.validate_for_provider(&provider)?;
                        }
                    }
                    Ok(AuthOutcome::Credential(credential))
                }
                AuthAction::Logout => {
                    storage.delete(&provider, &signal)?;
                    Ok(AuthOutcome::LoggedOut)
                }
            }))
            .unwrap_or_else(|_| Err("登录工作线程意外结束".into()));
            let _ = sender.send(AuthMessage::Finished(provider, result));
            repaint.request_repaint();
        }));
    }
    fn poll_auth(&mut self, ctx: &egui::Context) {
        let mut messages = vec![];
        let mut disconnected = false;
        if let Some(receiver) = &self.auth_receiver {
            loop {
                match receiver.try_recv() {
                    Ok(message) => messages.push(message),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                }
            }
        }
        for message in messages {
            match message {
                AuthMessage::Authorization(url) => {
                    self.login_url = Some(url.clone());
                    self.auth_status = "请在浏览器完成登录；可随时取消".into();
                    if !self.closing {
                        ctx.open_url(egui::OpenUrl::new_tab(url));
                    }
                }
                AuthMessage::DeviceCode(code) => self.auth_device_code = Some(code),
                AuthMessage::Finished(provider, result) => {
                    self.connections.loaded = false;
                    self.auth_receiver = None;
                    self.login_url = None;
                    self.auth_manual_sender = None;
                    self.auth_manual.clear();
                    self.auth_device_code = None;
                    if let Some(worker) = self.auth_worker.take() {
                        let _ = worker.join();
                    }
                    match result {
                        Ok(AuthOutcome::Credential(credential)) => {
                            self.auth_status = match &credential {
                                Some(credential) if credential.is_expired() => {
                                    "登录已过期，发送时自动刷新".into()
                                }
                                Some(_) => format!("{provider} 已登录 · 凭据加密存储"),
                                None => format!("{provider} 未登录"),
                            };
                            if provider == CODEX_PROVIDER {
                                self.settings.options.oauth = credential;
                                if let Some(draft) = &mut self.settings_draft {
                                    draft.settings.options.oauth =
                                        self.settings.options.oauth.clone();
                                }
                            }
                        }
                        Ok(AuthOutcome::ApiKey) => {
                            self.auth_status = format!("{provider} 已保存 API Key · 凭据加密存储")
                        }
                        Ok(AuthOutcome::LoggedOut) => {
                            if provider == CODEX_PROVIDER {
                                self.settings.options.oauth = None;
                                if let Some(draft) = &mut self.settings_draft {
                                    draft.settings.options.oauth = None;
                                }
                            }
                            self.auth_status = format!("{provider} 已退出登录");
                        }
                        Err(_) if self.auth_signal.is_cancelled() => {
                            self.auth_status = "登录操作已取消".into()
                        }
                        Err(error) => self.auth_status = error,
                    }
                    self.settings.options.auth_storage_path = Some(self.auth_path.clone());
                    if let Some(draft) = &mut self.settings_draft {
                        draft.settings.options.auth_storage_path = Some(self.auth_path.clone());
                    }
                    if self.connections.open && !self.closing {
                        self.start_connection_job(ctx, connections_ui::ConnectionJob::Load);
                    }
                }
            }
        }
        if disconnected && self.auth_receiver.is_some() {
            self.auth_receiver = None;
            self.auth_signal.cancel();
            if let Some(worker) = self.auth_worker.take() {
                let _ = worker.join();
            }
            self.auth_status = "登录工作线程意外结束".into();
            self.login_url = None;
            self.auth_manual_sender = None;
            self.auth_device_code = None;
            self.auth_manual.clear();
        }
    }
    fn auth_ui(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.separator();
        ui.label("原生订阅登录");
        ui.label(RichText::new(&self.auth_status).size(12.0));
        ui.add_enabled_ui(!self.busy(), |ui| {
            egui::ComboBox::from_id_salt("auth-provider")
                .selected_text(&self.auth_provider)
                .show_ui(ui, |ui| {
                    for provider in [
                        CODEX_PROVIDER,
                        "anthropic",
                        "github-copilot",
                        "openrouter",
                        "kimi-coding",
                        "xai",
                    ] {
                        if ui
                            .selectable_value(&mut self.auth_provider, provider.into(), provider)
                            .changed()
                        {
                            self.auth_status = "登录服务商已更改，请读取状态或登录".into();
                        }
                    }
                });
            let mut path = self.auth_path.to_string_lossy().into_owned();
            ui.label("原生加密凭据文件");
            if ui
                .add(egui::TextEdit::singleline(&mut path).desired_width(230.0))
                .changed()
            {
                self.auth_path = PathBuf::from(path);
                self.settings.options.auth_storage_path = Some(self.auth_path.clone());
                self.settings.options.oauth = None;
                if let Some(draft) = &mut self.settings_draft {
                    draft.settings.options.auth_storage_path = Some(self.auth_path.clone());
                    draft.settings.options.oauth = None;
                }
                self.auth_status = "凭据路径已更改，请读取状态或登录".into();
            }
            ui.horizontal_wrapped(|ui| {
                if ui.button("登录").clicked() {
                    self.start_auth(ctx, AuthAction::Login);
                }
                if ui.button("状态").clicked() {
                    self.start_auth(ctx, AuthAction::Status);
                }
                if ui.button("退出登录").clicked() {
                    self.start_auth(ctx, AuthAction::Logout);
                }
            });
        });
        if self.auth_receiver.is_some() {
            ui.horizontal(|ui| {
                ui.spinner();
                if ui.button("取消登录操作").clicked() {
                    self.auth_signal.cancel();
                    self.auth_status = "正在取消登录操作…".into();
                }
            });
        }
        if let Some(url) = &self.login_url {
            ui.hyperlink_to("在浏览器继续登录", url);
            if let Some(code) = &self.auth_device_code {
                ui.label(format!("设备授权码：{code}"));
            }
            if matches!(self.auth_provider.as_str(), "anthropic" | "openrouter") {
                ui.label("浏览器未回调时，粘贴授权码或回调 URL");
                ui.add(
                    egui::TextEdit::singleline(&mut self.auth_manual)
                        .password(true)
                        .desired_width(230.0),
                );
                if ui.button("提交授权响应").clicked() && !self.auth_manual.trim().is_empty()
                {
                    if let Some(sender) = &self.auth_manual_sender {
                        if sender.try_send(self.auth_manual.clone()).is_ok() {
                            self.auth_manual.clear();
                        }
                    }
                }
            }
        }
    }
    fn tree_ui(&mut self, ui: &mut egui::Ui) {
        egui::CollapsingHeader::new("会话树 / 分支").show(ui, |ui| {
            ui.add_enabled_ui(!self.busy(), |ui| {
                match self.session.get_tree() {
                    Ok(roots) => {
                        let leaf = self.session.get_leaf_id().ok().flatten();
                        egui::ScrollArea::vertical()
                            .id_salt("session-tree")
                            .max_height(140.0)
                            .show(ui, |ui| {
                                let mut nodes: Vec<_> =
                                    roots.into_iter().rev().map(|node| (node, 0usize)).collect();
                                while let Some((node, depth)) = nodes.pop() {
                                    let entry = node.entry;
                                    nodes.extend(
                                        node.children
                                            .into_iter()
                                            .rev()
                                            .map(|child| (child, depth + 1)),
                                    );
                                    let preview = entry.field("message")["content"]
                                        .as_str()
                                        .or_else(|| entry.field("summary").as_str())
                                        .unwrap_or_default()
                                        .chars()
                                        .take(30)
                                        .collect::<String>();
                                    let preview = if preview.is_empty() {
                                        entry.field("message")["content"]
                                            .as_array()
                                            .map(|parts| {
                                                parts
                                                    .iter()
                                                    .filter_map(|part| part["text"].as_str())
                                                    .collect::<Vec<_>>()
                                                    .join(" ")
                                                    .chars()
                                                    .take(30)
                                                    .collect::<String>()
                                            })
                                            .unwrap_or_default()
                                    } else {
                                        preview
                                    };
                                    let role = entry.field("message")["role"]
                                        .as_str()
                                        .unwrap_or(&entry.entry_type);
                                    if ui
                                        .selectable_label(
                                            self.tree_target == entry.id,
                                            format!(
                                                "{}{} {} {}",
                                                "  ".repeat(depth.min(8)),
                                                if leaf.as_ref() == Some(&entry.id) {
                                                    "●"
                                                } else {
                                                    "·"
                                                },
                                                role,
                                                preview
                                            ),
                                        )
                                        .on_hover_text(format!(
                                            "{}\nparent={:?}",
                                            entry.id, entry.parent_id
                                        ))
                                        .clicked()
                                    {
                                        self.tree_target = entry.id;
                                    }
                                }
                            });
                    }
                    Err(error) => {
                        ui.label(error);
                    }
                }
                ui.add(
                    egui::TextEdit::singleline(&mut self.tree_target)
                        .hint_text("entry ID")
                        .desired_width(230.0),
                );
                ui.horizontal_wrapped(|ui| {
                    if ui.button("导航").clicked() {
                        let target = self.tree_target.clone();
                        self.status = match self.navigate_tree(Some(&target)) {
                            Ok(()) => {
                                self.start_model_restore(ui.ctx());
                                "已切换当前分支".into()
                            }
                            Err(error) => error,
                        };
                    }
                    if ui.button("编辑此用户轮次").clicked() {
                        let target = self.tree_target.clone();
                        self.start_extension_action(ui.ctx(), ExtensionAction::Fork(target));
                    }
                    if ui.button("根节点").clicked() {
                        self.status = match self.navigate_tree(None) {
                            Ok(()) => {
                                self.start_model_restore(ui.ctx());
                                "当前叶已重置，旧分支完整保留".into()
                            }
                            Err(error) => error,
                        };
                    }
                    if ui.button("克隆当前分支").clicked() {
                        self.status = match self.clone_tree() {
                            Ok(()) => {
                                self.start_model_restore(ui.ctx());
                                "当前分支已克隆到新会话".into()
                            }
                            Err(error) => error,
                        };
                    }
                });
            });
        });
        egui::CollapsingHeader::new("自动上下文 / 重试").show(ui, |ui| {
            ui.add_enabled_ui(!self.busy(), |ui| {
                ui.checkbox(&mut self.auto_compaction, "启用自动压缩（模型请求边界）");
                ui.checkbox(&mut self.retry_enabled, "启用有界模型重试");
                if self.auto_compaction {
                    if let Some(model) = (self.settings.mode != ProviderMode::AzureOpenAiResponses)
                        .then(|| {
                            ModelBudget::from_catalog(self.provider_id(), &self.settings.model)
                        })
                        .flatten()
                    {
                        ui.label(format!(
                            "目录上下文 {} · 最大输出 {}",
                            model.context_window,
                            self.model_budget()
                                .map_or(model.max_output_tokens, |budget| budget.max_output_tokens)
                        ));
                    } else {
                        ui.label("未知型号 / Azure 部署：显式填写真实窗口，价格保持未知");
                        ui.horizontal(|ui| {
                            ui.label("上下文");
                            ui.add(
                                egui::DragValue::new(&mut self.context_window).range(0..=u64::MAX),
                            );
                        });
                        ui.horizontal(|ui| {
                            ui.label("预算最大输出");
                            if let Some(limit) = (self.settings.mode == ProviderMode::AmazonBedrock)
                                .then_some(self.settings.options.bedrock.max_tokens)
                                .flatten()
                            {
                                ui.label(format!("{limit}（使用请求 max tokens）"));
                            } else {
                                ui.add(
                                    egui::DragValue::new(&mut self.max_output_tokens)
                                        .range(0..=u64::MAX),
                                );
                            }
                        });
                    }
                    ui.horizontal(|ui| {
                        ui.label("保留 tokens");
                        ui.add(egui::DragValue::new(&mut self.reserve_tokens).range(0..=u64::MAX));
                    });
                }
            });
            if !self.runtime_activity.is_empty() {
                ui.label(RichText::new(&self.runtime_activity).size(11.0));
            }
            let report = self.automation.snapshot();
            if report.state.request_count > 0 {
                ui.label(format!(
                    "模型请求 {} · 摘要 {} · 重试 {}",
                    report.state.request_count, report.compactions, report.retries
                ));
            }
        });
    }
    fn stop_workers(&mut self) -> bool {
        self.stop_task_management();
        self.cancel_queue_io();
        self.connections.stop();
        self.ui_dialogs.close();
        self.signal.cancel();
        self.queue_signal.cancel();
        self.auth_signal.cancel();
        if !self.workbench.durable {
            self.queues.clear();
        }
        let running =
            self.worker.is_some() || self.queue_worker.is_some() || self.model_worker.is_some();
        self.receiver = None;
        self.auth_receiver = None;
        self.queue_receiver = None;
        self.model_receiver = None;
        self.model_commit = None;
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        if let Some(worker) = self.auth_worker.take() {
            let _ = worker.join();
        }
        if let Some(worker) = self.queue_worker.take() {
            let _ = worker.join();
        }

        if let Some(worker) = self.model_worker.take() {
            let _ = worker.join();
        }
        self.login_url = None;
        self.running = false;
        self.compacting = false;
        self.extension_operation = false;
        running
    }
    fn shutdown_extensions(&mut self) {
        if self.extensions.ids().is_empty() {
            return;
        }
        let registry = self.extensions.clone();
        let session = self.session.clone();
        let store = self.store.clone();
        let model = self.model_identity();
        let worker = std::thread::spawn(move || {
            let session = store
                .list()
                .ok()
                .and_then(|sessions| {
                    sessions
                        .into_iter()
                        .find(|candidate| candidate.id == session.id)
                })
                .unwrap_or(session);
            let _ = shutdown_registry(&registry, &session, model, SessionShutdownReason::Quit);
            for id in registry.ids() {
                let _ = registry.unregister(&id);
            }
        });
        let _ = worker.join();
        self.extensions = SharedRegistry::default();
    }
    fn save(&mut self) {
        self.session.updated_at = timestamp();
        if let Err(error) = self.store.save(&self.session) {
            self.status = format!("保存会话失败：{error}");
            return;
        }
        match self.store.list() {
            Ok(sessions) => self.sessions = sessions,
            Err(error) => self.status = format!("读取会话失败：{error}"),
        }
    }
    fn start(&mut self, ctx: &egui::Context) {
        if self.busy()
            || self.closing
            || !self.task_execution_allowed()
            || !self.queue_available()
            || self.input.trim().is_empty()
        {
            return;
        }
        if let Some(command) = self.input.trim().strip_prefix('/') {
            let (name, args) = command
                .split_once(char::is_whitespace)
                .unwrap_or((command, ""));
            if self.extensions.contains_command(name) {
                let action =
                    ExtensionAction::Command(name.to_owned(), args.trim_start().to_owned());
                self.pending_draft = Some(self.input.clone());
                self.input.clear();
                self.start_extension_action(ctx, action);
                return;
            }
            if name == "name" {
                self.pending_draft = Some(self.input.clone());
                let action = ExtensionAction::Name(args.into());
                self.input.clear();
                self.start_extension_action(ctx, action);
                return;
            }
        }
        let prompt = self.composer_message();
        self.pending_images = std::mem::take(&mut self.input_images);
        self.pending_draft = Some(self.input.clone());
        self.input.clear();
        self.start_prompt_worker(ctx, prompt, false);
    }
    fn start_prompt_worker(
        &mut self,
        ctx: &egui::Context,
        prompt: Message,
        already_prepared: bool,
    ) {
        if !self.task_execution_allowed() {
            self.restore_pending_draft();
            return;
        }
        if self.workbench.durable && !self.queue_workspace_matches() {
            self.halt_queue();
        }
        let mut session = self.session.clone();
        let workspace = self.workspace.clone();
        let registry = self.extensions.clone();
        let model = self.model_identity();
        let options = self.run_options();
        let settings = self.settings.clone();
        let store = self.store.clone();
        let queues = self.queues.clone();
        let initial_queue_id = self.pending_queue_id;
        let durable = self.workbench.durable;
        self.signal = CancellationToken::new();
        let signal = self.signal.clone();
        let (sender, receiver) = mpsc::sync_channel(64);
        self.receiver = Some(receiver);
        self.running = true;
        self.compacting = false;
        self.extension_operation = false;
        self.tool_activity.clear();
        self.status = "正在处理…".into();
        let repaint = ctx.clone();
        self.worker = Some(std::thread::spawn(move || {
            let preparation = catch_unwind(AssertUnwindSafe(|| {
                session.workspace = std::fs::canonicalize(workspace.trim())
                    .map_err(|_| "工作目录不存在，请修改左侧工作区路径".to_owned())?;
                if !session.workspace.is_dir() {
                    return Err("工作区路径不是目录".into());
                }
                let prompt = if already_prepared {
                    Some(prompt)
                } else {
                    let context =
                        extension_context(&session, &registry, ExtensionMode::Desktop, model)?;
                    prepare_input(
                        &session,
                        &registry,
                        prompt,
                        InputSource::Interactive,
                        &context,
                        &signal,
                    )?
                };
                let Some(prompt) = prompt else {
                    return Ok(None);
                };
                if signal.is_cancelled() {
                    return Err("Input operation aborted".into());
                }
                let resources = load_resources(&ResourceOptions {
                    cwd: session.workspace.clone(),
                    ..Default::default()
                });
                let prompt = if already_prepared {
                    prompt
                } else {
                    expand_user_prompt(prompt, &resources)?
                };
                let mut options = options?;
                options.queued_prompt_id = initial_queue_id;
                if durable {
                    queues
                        .bind_workspace(&session.workspace)
                        .map_err(|e| e.to_string())?;
                }
                settings.validate()?;
                options.system_prompt_suffix = load_context_files(&session.workspace).text;
                options
                    .system_prompt_suffix
                    .push_str(&format_skills_for_prompt(&resources.skills));
                if signal.is_cancelled() {
                    return Err("Input operation aborted".into());
                }
                if session.context.messages.is_empty() && session.get_session_name()?.is_none() {
                    session.name = prompt.text().chars().take(24).collect();
                }
                Ok(Some((prompt, options)))
            }))
            .unwrap_or_else(|_| Err("Input preparation worker panicked".into()));
            let (prompt, options) = match preparation {
                Ok(Some(prepared)) => prepared,
                Ok(None) => {
                    let _ = sender.send(WorkerMessage::InputHandled);
                    repaint.request_repaint();
                    return;
                }
                Err(error) => {
                    let _ = sender.send(WorkerMessage::InputFailed(error));
                    repaint.request_repaint();
                    return;
                }
            };
            let mut emit = |event| {
                let _ = sender.send(WorkerMessage::Event(event));
                repaint.request_repaint();
            };
            let mut result = catch_unwind(AssertUnwindSafe(|| {
                run_prompt(
                    &mut session,
                    prompt,
                    &settings,
                    &options,
                    &queues,
                    &signal,
                    &mut emit,
                )
            }))
            .unwrap_or_else(|_| Err("Agent worker panicked".into()));
            if let Err(error) = store.save(&session) {
                result = Err(format!("保存会话失败：{error}"));
            } else if let Err(error) = queues.confirm_saved_session(&session) {
                result = Err(format!("会话已保存，队列确认未提交：{error}"));
            }
            let _ = sender.send(WorkerMessage::Finished(Box::new(session), result));
            repaint.request_repaint();
        }));
    }
    fn restore_draft(&mut self, draft: Option<String>) {
        if let Some(draft) = draft {
            if self.input.is_empty() {
                self.input = draft;
            } else if self.input != draft {
                self.input = format!("{draft}\n{}", self.input);
            }
        }
    }
    fn start_steering_input(&mut self, ctx: &egui::Context) {
        self.start_queued_input(ctx, QueueLane::Steering);
    }
    fn start_queued_input(&mut self, ctx: &egui::Context, lane: QueueLane) {
        if self.closing
            || !self.task_execution_allowed()
            || self.queue_io_busy()
            || !self.queue_available()
            || self.compacting
            || self.extension_operation
            || self.model_receiver.is_some()
            || self.auth_receiver.is_some()
            || self.connections.busy()
            || self.queue_receiver.is_some()
            || self.input.trim().is_empty()
        {
            return;
        }
        if let Some(command) = self.input.trim().strip_prefix('/') {
            let name = command
                .split_once(char::is_whitespace)
                .map_or(command, |(name, _)| name);
            if self.extensions.contains_command(name) {
                self.status = "扩展命令请在当前任务结束后发送，草稿已保留".into();
                return;
            }
        }
        let mut session = self.session.clone();
        let workspace = self.workspace.clone();
        let registry = self.extensions.clone();
        let model = self.model_identity();
        let prompt = self.composer_message();
        let mut original_prompt = prompt.clone();
        if let Message::User(user) = &mut original_prompt {
            user.content[0] = Content::text(self.input.clone());
        }
        let queues = self.queues.clone();
        let durable = self.workbench.durable;
        let idle = !self.running;
        let store = self.store.clone();
        if !self.running {
            self.signal = CancellationToken::new();
        }
        self.queue_images = std::mem::take(&mut self.input_images);
        self.queue_draft = Some(std::mem::take(&mut self.input));
        self.queue_signal = CancellationToken::new();
        let signal = self.queue_signal.clone();
        let (sender, receiver) = mpsc::sync_channel(1);
        self.queue_receiver = Some(receiver);
        self.status = "正在处理插入指令…".into();
        let repaint = ctx.clone();
        self.queue_worker = Some(std::thread::spawn(move || {
            let result = catch_unwind(AssertUnwindSafe(|| {
                session.workspace =
                    std::fs::canonicalize(workspace.trim()).map_err(|error| error.to_string())?;
                let context =
                    extension_context(&session, &registry, ExtensionMode::Desktop, model)?;
                let result = prepare_input_with_behavior(
                    &session,
                    &registry,
                    prompt,
                    InputSource::Interactive,
                    &context,
                    &signal,
                    Some(if lane == QueueLane::FollowUp {
                        InputStreamingBehavior::FollowUp
                    } else {
                        InputStreamingBehavior::Steer
                    }),
                )?;
                if signal.is_cancelled() {
                    return Err("Input operation aborted".into());
                }
                result
                    .map(|prompt| {
                        let resources = load_resources(&ResourceOptions {
                            cwd: session.workspace.clone(),
                            ..Default::default()
                        });
                        expand_user_prompt(prompt, &resources)
                    })
                    .transpose()
            }))
            .unwrap_or_else(|_| Err("Queued input worker panicked".into()));
            let mut persisted_id = None;
            let result = result.and_then(|prepared| {
                if durable {
                    if let Some(prompt) = &prepared {
                        queues
                            .bind_workspace(&session.workspace)
                            .map_err(|e| e.to_string())?;
                        if idle {
                            store.save(&session)?;
                        }
                        persisted_id = Some(
                            queues
                                .try_enqueue(prompt.clone(), lane, Some(original_prompt))
                                .map_err(|e| e.to_string())?,
                        );
                    }
                }
                Ok(prepared)
            });
            let _ = sender.send(QueueInputResult {
                session_id: session.id,
                lane,
                result,
                persisted_id,
                workspace: (durable && idle).then_some(session.workspace),
            });
            repaint.request_repaint();
        }));
    }
    fn poll_steering_input(&mut self) {
        let received = self
            .queue_receiver
            .as_ref()
            .map(|receiver| receiver.try_recv());
        let Some(received) = received else {
            return;
        };
        let result = match received {
            Ok(result) => result,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => QueueInputResult {
                session_id: self.session.id.clone(),
                lane: QueueLane::Steering,
                result: Err("Queued input worker disconnected".into()),
                persisted_id: None,
                workspace: None,
            },
        };
        self.queue_receiver = None;
        if let Some(worker) = self.queue_worker.take() {
            let _ = worker.join();
        }
        if result.session_id != self.session.id {
            // A completed durable write belongs to its original session. Never
            // inject another session's draft or workspace into the current one.
            self.queue_draft = None;
            self.queue_images.clear();
            self.status = "其他会话的排队操作已结束；当前会话保持不变".into();
            return;
        }
        if self.workbench.durable {
            if let Some(error) = self.queues.storage_error() {
                let draft = self.queue_draft.take();
                if result.persisted_id.is_none() {
                    self.input_images.append(&mut self.queue_images);
                    self.restore_draft(draft);
                } else {
                    self.queue_images.clear();
                }
                self.queue_store_failed(error.to_string());
                return;
            }
        }
        if result.persisted_id.is_none()
            && (self.queue_signal.is_cancelled() || self.signal.is_cancelled() || self.closing)
        {
            let draft = self.queue_draft.take();
            self.input_images.append(&mut self.queue_images);
            self.restore_draft(draft);
            self.status = "插入指令已取消，草稿已保留".into();
            return;
        }
        match result.result {
            Ok(Some(prompt)) => {
                if let Some(workspace) = result.workspace {
                    self.session.workspace = workspace;
                }
                self.queue_images.clear();
                let id = result
                    .persisted_id
                    .unwrap_or_else(|| self.queues.enqueue(prompt.clone(), result.lane));
                if let Some(draft) = self.queue_draft.take() {
                    if !self.workbench.durable {
                        self.queued_drafts.push((id, prompt, draft));
                    }
                }
                self.status = if result.lane == QueueLane::FollowUp {
                    "消息已排队，当前任务结束后处理"
                } else {
                    "指令已排队，将在当前工具批次后处理"
                }
                .into();
                if result.persisted_id.is_some()
                    && (self.queue_signal.is_cancelled()
                        || self.signal.is_cancelled()
                        || self.closing)
                {
                    self.halt_queue();
                    self.queue_halted = true;
                    self.status =
                        "消息已在取消前保存到队列；已停止自动派发，没有重复放回输入框。".into();
                }
            }
            Ok(None) => {
                self.queue_images.clear();
                self.queue_draft = None;
                self.status = "插入输入已由扩展处理，未添加模型消息".into();
            }
            Err(error) => {
                let draft = self.queue_draft.take();
                self.input_images.append(&mut self.queue_images);
                self.restore_draft(draft);
                if self.workbench.durable && self.queues.storage_error().is_some() {
                    self.queue_store_failed(error);
                } else {
                    self.status = error;
                }
            }
        }
    }
    fn compact(&mut self, ctx: &egui::Context) {
        if self.busy() {
            return;
        }
        if self.settings.mode == ProviderMode::Demo {
            self.status = "请配置模型后压缩上下文".into();
            return;
        }
        if let Err(error) = self.settings.validate() {
            self.status = error;
            return;
        }
        let mut session = self.session.clone();
        let store = self.store.clone();
        let handle = self.automation.clone();
        let model = self.model_budget();
        let retry = RetryOptions {
            enabled: self.retry_enabled,
            ..Default::default()
        };
        let mut provider = ModelProvider {
            settings: self.settings.clone(),
            tool_definitions: vec![],
        };
        self.signal = CancellationToken::new();
        let signal = self.signal.clone();
        let (sender, receiver) = mpsc::sync_channel(64);
        self.receiver = Some(receiver);
        self.running = true;
        self.compacting = true;
        self.extension_operation = false;
        self.status = "正在摘要旧轮次，原始消息将保留在会话归档…".into();
        let repaint = ctx.clone();
        self.worker = Some(std::thread::spawn(move || {
            let mut result = catch_unwind(AssertUnwindSafe(|| {
                compact_with_ledger(
                    &mut session,
                    &mut provider,
                    model.as_ref(),
                    &handle,
                    retry,
                    &signal,
                )
            }))
            .unwrap_or_else(|_| Err("Compaction worker panicked".into()));
            if let Err(error) = store.save(&session) {
                result = Err(format!("保存会话失败：{error}"));
            }
            let _ = sender.send(WorkerMessage::Finished(Box::new(session), result));
            repaint.request_repaint();
        }));
    }
    fn start_extension_action(&mut self, ctx: &egui::Context, action: ExtensionAction) {
        if self.busy() {
            return;
        }
        let before = self.session.clone();
        let is_fork = matches!(&action, ExtensionAction::Fork(_));
        let store = self.store.clone();
        let model = self.model_identity();
        let configured_workspace = self.workspace.clone();
        let configured_thinking = self.settings.options.thinking_level;
        let mut loaded_paths = self.loaded_extension_paths.clone();
        let mut registry = self.extensions.clone();
        self.signal = CancellationToken::new();
        let signal = self.signal.clone();
        let (sender, receiver) = mpsc::sync_channel(64);
        self.receiver = Some(receiver);
        self.running = true;
        self.extension_operation = true;
        self.compacting = false;
        self.status = "正在处理原生扩展…".into();
        let repaint = ctx.clone();
        self.worker = Some(std::thread::spawn(move || {
            let mut session = before.clone();
            let mut committed = false;
            let result = catch_unwind(AssertUnwindSafe(|| match action {
                ExtensionAction::Command(name, args) => {
                    let context = extension_context(
                        &session,
                        &registry,
                        ExtensionMode::Desktop,
                        model.clone(),
                    )?;
                    let result = execute_extension_command(
                        &mut session,
                        &registry,
                        &name,
                        &args,
                        &context,
                        &signal,
                    )?;
                    if let Err(error) = store.save(&session) {
                        session = before.clone();
                        return Err(format!("保存扩展状态失败：{error}"));
                    }
                    committed = true;
                    let mut result = result;
                    if result.session_name.is_some() {
                        let (acknowledge, ack) = mpsc::sync_channel(1);
                        sender
                            .send(WorkerMessage::SessionCommitted(
                                Box::new(session.clone()),
                                acknowledge,
                            ))
                            .map_err(|_| "Session name committed; window closed")?;
                        ack.recv()
                            .map_err(|_| "Session name committed; window closed")?;
                        if let Err(error) = notify_session_info_changed(
                            &session,
                            &registry,
                            ExtensionMode::Desktop,
                            model.clone(),
                            &signal,
                        ) {
                            result
                                .text
                                .push_str(&format!("\n会话名称已保存；扩展通知未完成：{error}"));
                        }
                    }
                    Ok(Some(result))
                }
                ExtensionAction::Name(name) => {
                    session.append_session_info(&name)?;
                    if signal.is_cancelled() {
                        return Err("Session name cancelled before commit".into());
                    }
                    store.save(&session)?;
                    committed = true;
                    let (acknowledge, ack) = mpsc::sync_channel(1);
                    sender
                        .send(WorkerMessage::SessionCommitted(
                            Box::new(session.clone()),
                            acknowledge,
                        ))
                        .map_err(|_| "Session name committed; window closed")?;
                    ack.recv()
                        .map_err(|_| "Session name committed; window closed")?;
                    let mut result = CommandResult {
                        text: session.get_session_name()?.map_or_else(
                            || "会话名称已清除".into(),
                            |name| format!("会话名称：{name}"),
                        ),
                        ..Default::default()
                    };
                    if let Err(error) = notify_session_info_changed(
                        &session,
                        &registry,
                        ExtensionMode::Desktop,
                        model.clone(),
                        &signal,
                    ) {
                        result
                            .text
                            .push_str(&format!("\n名称已保存；扩展通知未完成：{error}"));
                    }
                    Ok(Some(result))
                }
                ExtensionAction::Fork(entry_id) => {
                    if !prepare_session_change(
                        &session,
                        &registry,
                        ExtensionMode::Desktop,
                        model.clone(),
                        &SessionBeforeEvent::Fork {
                            entry_id: entry_id.clone(),
                            position: ForkPosition::Before,
                        },
                        &signal,
                    )? {
                        return Err("Session change cancelled by extension".into());
                    }
                    let text = session.fork(&entry_id)?;
                    if signal.is_cancelled() {
                        return Err("Session fork cancelled before commit".into());
                    }
                    store.save(&session)?;
                    committed = true;
                    Ok(Some(CommandResult {
                        text,
                        ..Default::default()
                    }))
                }
                ExtensionAction::Load(paths) => {
                    let workspace = std::fs::canonicalize(configured_workspace.trim())
                        .map_err(|error| format!("扩展工作目录：{error}"))?;
                    if !workspace.is_dir() {
                        return Err("扩展工作目录不是目录".into());
                    }
                    session.workspace = workspace;
                    let paths = paths
                        .into_iter()
                        .map(|path| {
                            if path.is_absolute() {
                                path
                            } else {
                                session.workspace.join(path)
                            }
                        })
                        .collect::<Vec<_>>();
                    let first = registry.ids().is_empty();
                    let context = extension_context(
                        &session,
                        &registry,
                        ExtensionMode::Desktop,
                        model.clone(),
                    )?;
                    let candidate = load_native_extensions(&paths, &context, &signal)?;
                    let context = extension_context(
                        &session,
                        &candidate,
                        ExtensionMode::Desktop,
                        model.clone(),
                    )?;
                    candidate.emit_event_checked(
                        &LifecycleEvent::SessionStart {
                            reason: if first {
                                SessionStartReason::Startup
                            } else {
                                SessionStartReason::Reload
                            },
                            previous_session_file: None,
                        },
                        &context,
                        &signal,
                    )?;
                    store.save(&session)?;
                    shutdown_registry(
                        &registry,
                        &before,
                        model.clone(),
                        SessionShutdownReason::Reload,
                    )?;
                    for id in registry.ids() {
                        registry.unregister(&id)?;
                    }
                    registry = candidate;
                    loaded_paths = paths;
                    Ok(None)
                }
                ExtensionAction::Replace(mut candidate, reason) => {
                    if signal.is_cancelled() {
                        return Err("Extension operation aborted".into());
                    }
                    let switch = match reason {
                        SessionStartReason::New => Some(SessionSwitchReason::New),
                        SessionStartReason::Resume => Some(SessionSwitchReason::Resume),
                        _ => None,
                    };
                    if let Some(reason) = switch {
                        if !prepare_session_change(
                            &session,
                            &registry,
                            ExtensionMode::Desktop,
                            model.clone(),
                            &SessionBeforeEvent::Switch {
                                reason,
                                target_session_file: if reason == SessionSwitchReason::Resume {
                                    Some(store.path(&candidate.id)?)
                                } else {
                                    None
                                },
                            },
                            &signal,
                        )? {
                            return Err("Session change cancelled by extension".into());
                        }
                    }
                    if reason == SessionStartReason::New {
                        if let Some(level) = configured_thinking {
                            candidate.append_thinking_level(level)?;
                        }
                    }
                    let shutdown_reason = match reason {
                        SessionStartReason::New => SessionShutdownReason::New,
                        SessionStartReason::Fork => SessionShutdownReason::Fork,
                        _ => SessionShutdownReason::Resume,
                    };
                    let replacement = if !loaded_paths.is_empty() {
                        let context = extension_context(
                            &candidate,
                            &registry,
                            ExtensionMode::Desktop,
                            model.clone(),
                        )?;
                        load_native_extensions(&loaded_paths, &context, &signal)?
                    } else {
                        registry.clone()
                    };
                    let context = extension_context(
                        &candidate,
                        &replacement,
                        ExtensionMode::Desktop,
                        model.clone(),
                    )?;
                    replacement.emit_event_checked(
                        &LifecycleEvent::SessionStart {
                            reason,
                            previous_session_file: None,
                        },
                        &context,
                        &signal,
                    )?;
                    if signal.is_cancelled() {
                        return Err("Session transition cancelled before commit".into());
                    }
                    store.save(&session)?;
                    store.save(&candidate)?;
                    let outgoing = session.clone();
                    let outgoing_registry = registry.clone();
                    registry = replacement;
                    session = *candidate;
                    committed = true;
                    shutdown_registry(
                        &outgoing_registry,
                        &outgoing,
                        model.clone(),
                        shutdown_reason,
                    )?;
                    if !loaded_paths.is_empty() {
                        for id in outgoing_registry.ids() {
                            outgoing_registry.unregister(&id)?;
                        }
                    }
                    Ok(None)
                }
                ExtensionAction::Quit => {
                    let saved = store.save(&session);
                    let cleanup = shutdown_registry(
                        &registry,
                        &session,
                        model.clone(),
                        SessionShutdownReason::Quit,
                    );
                    for id in registry.ids() {
                        registry.unregister(&id)?;
                    }
                    registry = SharedRegistry::default();
                    loaded_paths.clear();
                    saved?;
                    cleanup?;
                    Ok(None)
                }
            }))
            .unwrap_or_else(|_| Err("Extension worker panicked".into()));
            if result.is_err() && !committed {
                session = before;
            }
            if is_fork {
                let result =
                    result.map(|result| result.map(|result| result.text).unwrap_or_default());
                let _ = sender.send(WorkerMessage::ForkFinished(Box::new(session), result));
                repaint.request_repaint();
                return;
            }
            let _ = sender.send(WorkerMessage::ExtensionFinished(
                Box::new(session),
                registry,
                loaded_paths,
                result,
            ));
            repaint.request_repaint();
        }));
    }
    fn apply_command_display(&mut self, result: &CommandResult) {
        if !result.text.is_empty() {
            self.extension_display.push(result.text.clone());
        }
        for request in &result.ui {
            match request {
                UiRequest::Notification { level, text } => self.extension_display.push(format!(
                    "{} · {text}",
                    match level {
                        NotificationLevel::Info => "信息",
                        NotificationLevel::Warning => "提示",
                        NotificationLevel::Error => "错误",
                    }
                )),
                UiRequest::SetStatus {
                    key,
                    text: Some(text),
                } => {
                    self.extension_statuses.insert(key.clone(), text.clone());
                }
                UiRequest::SetStatus { key, text: None } => {
                    self.extension_statuses.remove(key);
                }
            }
        }
        if self.extension_display.len() > 128 {
            self.extension_display
                .drain(..self.extension_display.len() - 128);
        }
    }
    fn extensions_ui(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        egui::CollapsingHeader::new("原生 Rust 扩展").show(ui, |ui| {
            ui.add_enabled_ui(!self.busy(), |ui| {
                ui.label("显式 manifest 路径（每行一个）");
                ui.add(
                    egui::TextEdit::multiline(&mut self.extension_paths)
                        .desired_rows(2)
                        .desired_width(230.0),
                );
                if ui.button("加载 / 重载扩展").clicked() {
                    let paths = self
                        .extension_paths
                        .lines()
                        .map(str::trim)
                        .filter(|line| !line.is_empty())
                        .map(PathBuf::from)
                        .collect();
                    self.start_extension_action(ctx, ExtensionAction::Load(paths));
                }
                for id in self.extensions.ids() {
                    ui.label(format!("扩展：{id}"));
                }
                for command in self.extensions.commands() {
                    if ui
                        .button(format!("/{}", command.invocation_name))
                        .on_hover_text(command.description)
                        .clicked()
                    {
                        self.input = format!("/{} ", command.invocation_name);
                    }
                }
            });
            for (key, text) in &self.extension_statuses {
                ui.label(format!("{key} · {text}"));
            }
        });
    }
    fn switch_draft_mode(
        &mut self,
        configuration: &mut ModelConfiguration,
        previous: ModelConfiguration,
    ) {
        let next_mode = configuration.settings.mode.clone();
        self.model_drafts
            .insert(format!("{:?}", previous.settings.mode), previous.clone());
        *configuration = self
            .model_drafts
            .get(&format!("{next_mode:?}"))
            .cloned()
            .unwrap_or_else(|| {
                let mut next = previous.clone();
                next.settings.options.oauth = None;
                next.settings.options.oauth_config = None;
                next.settings.options.api_key_explicit = false;
                next.settings.mode = next_mode;
                next.settings.model.clear();
                next.settings.options.provider_id = canonical_provider(&next.settings.mode).into();
                if next.settings.base_url.trim().is_empty()
                    || next.settings.base_url == previous.settings.mode.default_base_url()
                {
                    next.settings.base_url = next.settings.mode.default_base_url().into();
                }
                next.settings.api_key = if next.settings.mode == ProviderMode::OpenAiCodex {
                    String::new()
                } else {
                    std::env::var("AGENT_CORE_API_KEY")
                        .or_else(|_| std::env::var(next.settings.mode.api_key_env()))
                        .unwrap_or_default()
                };
                fill_cloud_defaults(&mut next.settings);
                refresh_configuration_budget(&mut next);
                next
            });
    }
    fn model_configuration_ui(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let mut configuration = self
            .settings_draft
            .take()
            .unwrap_or_else(|| self.model_configuration());
        egui::CollapsingHeader::new("模型连接")
            .default_open(true)
            .show(ui, |ui| {
                let previous_configuration = configuration.clone();
                let previous_mode = configuration.settings.mode.clone();
                egui::ComboBox::from_id_salt("provider")
                    .selected_text(match configuration.settings.mode {
                        ProviderMode::Demo => "离线演示",
                        ProviderMode::OpenAiCompatible => "OpenAI-compatible",
                        ProviderMode::OpenAiResponses => "OpenAI Responses",
                        ProviderMode::OpenAiCodex => "OpenAI Codex · OAuth",
                        ProviderMode::Anthropic => "Anthropic",
                        ProviderMode::Google => "Google Gemini",
                        ProviderMode::AzureOpenAiResponses => "Azure OpenAI Responses",
                        ProviderMode::GoogleVertex => "Google Vertex AI",
                        ProviderMode::AmazonBedrock => "Amazon Bedrock",
                        ProviderMode::Mistral => "Mistral Conversations",
                        ProviderMode::PiMessages => "Pi Messages",
                    })
                    .show_ui(ui, |ui| {
                        ui.selectable_value(
                            &mut configuration.settings.mode,
                            ProviderMode::OpenAiCodex,
                            "OpenAI Codex · OAuth",
                        );
                        ui.selectable_value(
                            &mut configuration.settings.mode,
                            ProviderMode::Demo,
                            "离线演示",
                        );
                        ui.selectable_value(
                            &mut configuration.settings.mode,
                            ProviderMode::OpenAiCompatible,
                            "OpenAI-compatible",
                        );
                        ui.selectable_value(
                            &mut configuration.settings.mode,
                            ProviderMode::OpenAiResponses,
                            "OpenAI Responses",
                        );
                        ui.selectable_value(
                            &mut configuration.settings.mode,
                            ProviderMode::Anthropic,
                            "Anthropic",
                        );
                        ui.selectable_value(
                            &mut configuration.settings.mode,
                            ProviderMode::Google,
                            "Google Gemini",
                        );
                        ui.selectable_value(
                            &mut configuration.settings.mode,
                            ProviderMode::AzureOpenAiResponses,
                            "Azure OpenAI Responses",
                        );
                        ui.selectable_value(
                            &mut configuration.settings.mode,
                            ProviderMode::GoogleVertex,
                            "Google Vertex AI",
                        );
                        ui.selectable_value(
                            &mut configuration.settings.mode,
                            ProviderMode::AmazonBedrock,
                            "Amazon Bedrock",
                        );
                        ui.selectable_value(
                            &mut configuration.settings.mode,
                            ProviderMode::Mistral,
                            "Mistral Conversations",
                        );
                        ui.selectable_value(
                            &mut configuration.settings.mode,
                            ProviderMode::PiMessages,
                            "Pi Messages",
                        );
                    });
                if configuration.settings.mode != previous_mode {
                    self.switch_draft_mode(&mut configuration, previous_configuration);
                }
                if configuration.settings.mode != ProviderMode::Demo {
                    ui.label("API Base URL");
                    if ui
                        .add(
                            egui::TextEdit::singleline(&mut configuration.settings.base_url)
                                .desired_width(230.0),
                        )
                        .changed()
                        && configuration.settings.mode == ProviderMode::AmazonBedrock
                    {
                        configuration.settings.options.bedrock.endpoint_explicit = true;
                    }
                    ui.label("模型 ID");
                    if ui
                        .add(
                            egui::TextEdit::singleline(&mut configuration.settings.model)
                                .hint_text("服务商提供的模型 ID")
                                .desired_width(230.0),
                        )
                        .changed()
                    {
                        refresh_configuration_budget(&mut configuration);
                    }
                    ui.label("Provider ID（目录规范名称）");
                    if ui
                        .add(
                            egui::TextEdit::singleline(
                                &mut configuration.settings.options.provider_id,
                            )
                            .desired_width(230.0),
                        )
                        .changed()
                    {
                        refresh_configuration_budget(&mut configuration);
                    }
                    ui.add(
                        egui::TextEdit::singleline(&mut self.model_filter)
                            .hint_text("筛选离线模型目录")
                            .desired_width(230.0),
                    );
                    egui::ComboBox::from_id_salt("model-catalog")
                        .selected_text("从目录选择模型")
                        .show_ui(ui, |ui| {
                            let mode = configuration.settings.mode.clone();
                            for model in search(&self.model_filter)
                                .into_iter()
                                .filter(|model| model.mode().as_ref() == Some(&mode))
                                .take(80)
                            {
                                if ui
                                    .selectable_label(
                                        configuration.settings.model == model.id,
                                        if settings_identity(&self.settings).provider
                                            == model.provider
                                            && self.settings.model == model.id
                                        {
                                            format!("{}（已应用）", model.canonical_id())
                                        } else {
                                            model.canonical_id()
                                        },
                                    )
                                    .clicked()
                                {
                                    configuration.settings.model = model.id.clone();
                                    if !matches!(
                                        configuration.settings.mode,
                                        ProviderMode::AzureOpenAiResponses
                                            | ProviderMode::GoogleVertex
                                            | ProviderMode::AmazonBedrock
                                    ) {
                                        if configuration.settings.base_url.trim().is_empty()
                                            || configuration.settings.base_url
                                                == configuration.settings.mode.default_base_url()
                                        {
                                            configuration.settings.base_url =
                                                model.base_url.clone();
                                        }
                                        if configuration.settings.api_key.is_empty() {
                                            configuration.settings.api_key =
                                                resolve_api_key(&model.provider);
                                        }
                                    }
                                    configuration.settings.options.provider_id =
                                        model.provider.clone();
                                    if configuration.settings.options.auth_storage_path.is_none() {
                                        configuration.settings.options.auth_storage_path =
                                            Some(self.auth_path.clone());
                                    }
                                    refresh_configuration_budget(&mut configuration);
                                }
                            }
                        });
                    if configuration.settings.mode != ProviderMode::OpenAiCodex {
                        ui.label(
                            if configuration.settings.mode == ProviderMode::AmazonBedrock {
                                "Bearer token（仅当前进程，可留空使用 SigV4）"
                            } else {
                                "API Key（仅当前进程）"
                            },
                        );
                        if ui
                            .add(
                                egui::TextEdit::singleline(&mut configuration.settings.api_key)
                                    .password(true)
                                    .desired_width(230.0),
                            )
                            .changed()
                        {
                            configuration.settings.options.api_key_explicit =
                                !configuration.settings.api_key.is_empty();
                        }
                    }
                    Self::deployment_options_ui(ui, &mut configuration.settings);
                }
                egui::ComboBox::from_id_salt("thinking-level")
                    .selected_text(configuration.settings.options.thinking_level.map_or_else(
                        || "Thinking：未配置（服务默认）".into(),
                        |level| format!("Thinking：{level}"),
                    ))
                    .show_ui(ui, |ui| {
                        for level in configuration.settings.supported_thinking_levels() {
                            ui.selectable_value(
                                &mut configuration.settings.options.thinking_level,
                                Some(level),
                                level.as_str(),
                            );
                        }
                    });
                if configuration.settings.mode != ProviderMode::Demo {
                    egui::CollapsingHeader::new("显式 reasoning 能力（自定义型号）").show(
                        ui,
                        |ui| {
                            let mut enabled = configuration
                                .settings
                                .options
                                .thinking_capabilities
                                .is_some();
                            if ui
                                .checkbox(&mut enabled, "使用明确提供的型号能力")
                                .changed()
                            {
                                configuration.settings.options.thinking_capabilities = enabled
                                    .then(|| agent_runtime::provider::ThinkingCapabilities {
                                        reasoning: false,
                                        max_tokens: 0,
                                        context_window: 0,
                                        name: String::new(),
                                        thinking_level_map: BTreeMap::new(),
                                        compat: serde_json::Value::Null,
                                    });
                            }
                            if let Some(cap) =
                                &mut configuration.settings.options.thinking_capabilities
                            {
                                ui.checkbox(&mut cap.reasoning, "型号支持 reasoning");
                                ui.label("填写型号真实上限；0 不可应用");
                                ui.horizontal(|ui| {
                                    ui.label("输出上限");
                                    ui.add(
                                        egui::DragValue::new(&mut cap.max_tokens)
                                            .range(0..=u64::MAX),
                                    );
                                });
                                ui.horizontal(|ui| {
                                    ui.label("上下文上限");
                                    ui.add(
                                        egui::DragValue::new(&mut cap.context_window)
                                            .range(0..=u64::MAX),
                                    );
                                });
                                ui.add(
                                    egui::TextEdit::singleline(&mut cap.name)
                                        .hint_text("服务端型号名称"),
                                );
                                for level in ["xhigh", "max"] {
                                    let mut mapped = cap
                                        .thinking_level_map
                                        .get(level)
                                        .and_then(Option::as_ref)
                                        .cloned()
                                        .unwrap_or_default();
                                    ui.label(format!("{level} 服务端映射（空=不支持）"));
                                    if ui.add(egui::TextEdit::singleline(&mut mapped)).changed() {
                                        cap.thinking_level_map.insert(
                                            level.into(),
                                            (!mapped.is_empty()).then_some(mapped),
                                        );
                                    }
                                }
                            }
                        },
                    );
                }
                if self.auto_compaction
                    && configuration.settings.mode != ProviderMode::Demo
                    && (configuration.settings.mode == ProviderMode::AzureOpenAiResponses
                        || ModelBudget::from_catalog(
                            &settings_identity(&configuration.settings).provider,
                            &configuration.settings.model,
                        )
                        .is_none())
                {
                    ui.label("未知型号预算（不代表请求输出硬上限）");
                    ui.horizontal(|ui| {
                        ui.label("上下文");
                        ui.add(
                            egui::DragValue::new(&mut configuration.context_window)
                                .range(0..=u64::MAX),
                        );
                    });
                    ui.horizontal(|ui| {
                        ui.label("最大输出");
                        ui.add(
                            egui::DragValue::new(&mut configuration.max_output_tokens)
                                .range(0..=u64::MAX),
                        );
                    });
                }
                ui.horizontal_wrapped(|ui| {
                    if ui.button("应用模型配置").clicked() {
                        self.start_model_selection(
                            ctx,
                            configuration.clone(),
                            ModelSelectSource::Set,
                        );
                    }
                    if ui.button("撤回未应用编辑").clicked() {
                        configuration = self.model_configuration();
                    }
                });
                let active = settings_identity(&self.settings);
                ui.label(format!(
                    "已应用：{} / {} · thinking {}",
                    active.provider,
                    active.model,
                    self.settings
                        .options
                        .thinking_level
                        .map_or_else(|| "服务默认".into(), |level| level.to_string())
                ));
                ui.label("编辑不会立即切换；应用仅验证本地配置，不发模型或 STS 请求");
            });
        self.settings_draft = Some(configuration);
    }
    fn deployment_options_ui(ui: &mut egui::Ui, settings: &mut ProviderSettings) {
        match settings.mode {
            ProviderMode::AzureOpenAiResponses => {
                ui.label("Azure resource name（Base URL 空时使用）");
                ui.add(
                    egui::TextEdit::singleline(&mut settings.options.azure.resource_name)
                        .desired_width(230.0),
                );
                ui.label("部署名称（空时使用映射 / 模型 ID）");
                ui.add(
                    egui::TextEdit::singleline(&mut settings.options.azure.deployment)
                        .desired_width(230.0),
                );
                ui.label("API version");
                ui.add(
                    egui::TextEdit::singleline(&mut settings.options.azure.api_version)
                        .desired_width(230.0),
                );
                ui.label("Bearer token（与 API Key 二选一，仅当前进程）");
                ui.add(
                    egui::TextEdit::singleline(&mut settings.options.azure.bearer_token)
                        .password(true)
                        .desired_width(230.0),
                );
                if !settings.options.azure.deployment_map.is_empty() {
                    ui.label(format!(
                        "环境部署映射：{} 项",
                        settings.options.azure.deployment_map.len()
                    ));
                }
                ui.label("部署底层型号未验证：目录价格不用于自动预算");
            }
            ProviderMode::GoogleVertex => {
                ui.label("Project（ADC / Bearer 模式）");
                ui.add(
                    egui::TextEdit::singleline(&mut settings.options.vertex.project)
                        .desired_width(230.0),
                );
                ui.label("Location（例如 global / us-central1）");
                ui.add(
                    egui::TextEdit::singleline(&mut settings.options.vertex.location)
                        .desired_width(230.0),
                );
                ui.label("ADC JSON 路径（仅工作线程读取）");
                let mut path = settings
                    .options
                    .vertex
                    .adc_path
                    .as_ref()
                    .map(|path| path.to_string_lossy().into_owned())
                    .unwrap_or_default();
                if ui
                    .add(egui::TextEdit::singleline(&mut path).desired_width(230.0))
                    .changed()
                {
                    settings.options.vertex.adc_path =
                        (!path.trim().is_empty()).then(|| PathBuf::from(path.trim()));
                }
                ui.label("Bearer token（与 API Key 二选一，仅当前进程）");
                ui.add(
                    egui::TextEdit::singleline(&mut settings.options.vertex.access_token)
                        .password(true)
                        .desired_width(230.0),
                );
                ui.collapsing("ADC token endpoint", |ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut settings.options.vertex.token_url)
                            .desired_width(230.0),
                    );
                });
                ui.label("API Key 非空使用 Express；否则使用 Bearer / ADC");
            }
            ProviderMode::AmazonBedrock => {
                ui.label("AWS Region（模型 ARN 的 region 优先）");
                ui.add(
                    egui::TextEdit::singleline(&mut settings.options.bedrock.region)
                        .desired_width(230.0),
                );
                ui.label("AWS Profile（仅请求工作线程读取）");
                ui.add(
                    egui::TextEdit::singleline(&mut settings.options.bedrock.profile)
                        .desired_width(230.0),
                );
                for (label, value) in [
                    (
                        "Shared credentials file",
                        &mut settings.options.bedrock.credentials_file,
                    ),
                    ("AWS config file", &mut settings.options.bedrock.config_file),
                    (
                        "SSO cache directory（仅请求工作线程读取）",
                        &mut settings.options.bedrock.sso_cache_dir,
                    ),
                ] {
                    ui.label(label);
                    let mut path = value
                        .as_ref()
                        .map(|path| path.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    if ui
                        .add(egui::TextEdit::singleline(&mut path).desired_width(230.0))
                        .changed()
                    {
                        *value = (!path.trim().is_empty()).then(|| PathBuf::from(path.trim()));
                    }
                }
                ui.label("STS endpoint（空值使用区域默认，仅请求线程连接）");
                ui.add(
                    egui::TextEdit::singleline(&mut settings.options.bedrock.sts_endpoint)
                        .desired_width(230.0),
                );
                for (label, value) in [
                    (
                        "SSO endpoint（空值使用区域默认，仅请求线程连接）",
                        &mut settings.options.bedrock.sso_endpoint,
                    ),
                    (
                        "SSO OIDC endpoint（空值使用区域默认，仅请求线程连接）",
                        &mut settings.options.bedrock.sso_oidc_endpoint,
                    ),
                ] {
                    ui.label(label);
                    ui.add(egui::TextEdit::singleline(value).desired_width(230.0));
                }
                ui.collapsing("当前进程 AWS 凭据", |ui| {
                    for (label, value) in [
                        ("Access key ID", &mut settings.options.bedrock.access_key_id),
                        (
                            "Secret access key",
                            &mut settings.options.bedrock.secret_access_key,
                        ),
                        ("Session token", &mut settings.options.bedrock.session_token),
                        (
                            "Bearer token（优先于 SigV4）",
                            &mut settings.options.bedrock.bearer_token,
                        ),
                    ] {
                        ui.label(label);
                        ui.add(
                            egui::TextEdit::singleline(value)
                                .password(true)
                                .desired_width(230.0),
                        );
                    }
                });
                egui::ComboBox::from_id_salt("bedrock-cache-retention")
                    .selected_text(match settings.options.bedrock.cache_retention {
                        BedrockCacheRetention::None => "Cache: none",
                        BedrockCacheRetention::Short => "Cache: short",
                        BedrockCacheRetention::Long => "Cache: long",
                    })
                    .show_ui(ui, |ui| {
                        for value in [
                            BedrockCacheRetention::None,
                            BedrockCacheRetention::Short,
                            BedrockCacheRetention::Long,
                        ] {
                            ui.selectable_value(
                                &mut settings.options.bedrock.cache_retention,
                                value,
                                match value {
                                    BedrockCacheRetention::None => "none",
                                    BedrockCacheRetention::Short => "short",
                                    BedrockCacheRetention::Long => "long",
                                },
                            );
                        }
                    });
                let mut limit = settings.options.bedrock.max_tokens.is_some();
                if ui.checkbox(&mut limit, "显式 max tokens").changed() {
                    settings.options.bedrock.max_tokens = limit.then_some(1024);
                }
                if let Some(value) = &mut settings.options.bedrock.max_tokens {
                    ui.add(egui::DragValue::new(value).range(1..=u32::MAX));
                }
                let mut temperature = settings.options.bedrock.temperature.is_some();
                if ui.checkbox(&mut temperature, "显式 temperature").changed() {
                    settings.options.bedrock.temperature = temperature.then_some(0.0);
                }
                if let Some(value) = &mut settings.options.bedrock.temperature {
                    ui.add(egui::DragValue::new(value).range(0.0..=1.0));
                }
                ui.checkbox(
                    &mut settings.options.bedrock.endpoint_explicit,
                    "固定当前 endpoint（不按 region 重选）",
                );
                ui.label(
                    "支持静态 / shared profile / STS role / web identity / 原生 SSO / credential_process / Bearer；凭据不写入会话",
                );
            }
            ProviderMode::Mistral | ProviderMode::PiMessages => {
                let (temperature, max_tokens) = if settings.mode == ProviderMode::Mistral {
                    (
                        &mut settings.options.mistral.temperature,
                        &mut settings.options.mistral.max_tokens,
                    )
                } else {
                    (
                        &mut settings.options.pi_messages.temperature,
                        &mut settings.options.pi_messages.max_tokens,
                    )
                };
                let mut enabled = temperature.is_some();
                if ui.checkbox(&mut enabled, "显式 temperature").changed() {
                    *temperature = enabled.then_some(0.2);
                }
                if let Some(value) = temperature {
                    ui.add(egui::DragValue::new(value).range(0.0..=2.0));
                }
                let mut enabled = max_tokens.is_some();
                if ui.checkbox(&mut enabled, "显式 max tokens").changed() {
                    *max_tokens = enabled.then_some(4096);
                }
                if let Some(value) = max_tokens {
                    ui.add(egui::DragValue::new(value).range(1..=u64::MAX));
                }
                if settings.mode == ProviderMode::PiMessages {
                    ui.checkbox(
                        &mut settings.options.pi_messages.debug,
                        "Pi server debug 路由",
                    );
                    ui.label("Pi Messages 使用独立 /messages 协议；请填写服务器 Base URL");
                } else {
                    ui.label("Mistral 原生 thinking / tool ID / cache accounting");
                }
            }
            _ => {}
        }
        if matches!(
            settings.options.provider_id.as_str(),
            "cloudflare-workers-ai" | "cloudflare-ai-gateway"
        ) {
            ui.label("Cloudflare Account ID");
            ui.add(
                egui::TextEdit::singleline(&mut settings.options.cloudflare.account_id)
                    .desired_width(230.0),
            );
            if settings.options.provider_id == "cloudflare-ai-gateway" {
                ui.label("Cloudflare Gateway ID");
                ui.add(
                    egui::TextEdit::singleline(&mut settings.options.cloudflare.gateway_id)
                        .desired_width(230.0),
                );
            }
        }
        if settings.options.provider_id == "github-copilot" {
            ui.label("Copilot enterprise domain（可选）");
            ui.add(
                egui::TextEdit::singleline(&mut settings.options.copilot.enterprise_domain)
                    .desired_width(230.0),
            );
            ui.label("Copilot account / proxy endpoint（可选）");
            ui.add(
                egui::TextEdit::singleline(&mut settings.options.copilot.base_url)
                    .desired_width(230.0),
            );
        }
    }
    fn poll(&mut self, ctx: &egui::Context) {
        self.poll_task_management(ctx);
        self.sync_queue_session();
        self.poll_queue_io(ctx);
        self.poll_connections(ctx);
        self.poll_auth(ctx);
        self.poll_model_selection();
        self.poll_steering_input();
        for event in self.automation.drain_events() {
            self.runtime_activity = runtime_event_text(&event);
        }
        let mut messages = vec![];
        let mut disconnected = false;
        if let Some(receiver) = &self.receiver {
            for _ in 0..256 {
                match receiver.try_recv() {
                    Ok(message) => messages.push(message),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                }
            }
        }
        for message in messages {
            match message {
                WorkerMessage::Event(event) => match event {
                    AgentEvent::MessageUpdate {
                        message: Message::Assistant(message),
                    }
                    | AgentEvent::MessageStart {
                        message: Message::Assistant(message),
                    } => self.partial = Some(message),
                    AgentEvent::MessageEnd { message } => {
                        let user = matches!(&message, Message::User(_));
                        if matches!(&message, Message::Assistant(_)) {
                            self.partial = None;
                        }
                        if let Err(error) = self.session.append_message(message) {
                            self.status = error;
                            self.signal.cancel();
                        } else if user {
                            self.pending_draft = None;
                            self.pending_images.clear();
                        }
                    }
                    AgentEvent::ToolExecutionStart { tool_name, .. } => {
                        self.tool_activity.push(format!("执行 {tool_name}"))
                    }
                    AgentEvent::ToolExecutionEnd {
                        tool_name,
                        is_error,
                        ..
                    } => self.tool_activity.push(format!(
                        "{tool_name} · {}",
                        if is_error { "错误" } else { "完成" }
                    )),
                    _ => {}
                },
                WorkerMessage::Finished(session, result) => {
                    let durable_attempt =
                        self.workbench.durable && self.pending_queue_id.take().is_some();
                    self.session = *session;
                    if self
                        .session
                        .context
                        .messages
                        .iter()
                        .rev()
                        .find_map(|message| match message {
                            Message::Assistant(assistant) => {
                                Some(assistant.error_message.is_some())
                            }
                            _ => None,
                        })
                        .unwrap_or(false)
                    {
                        self.halt_queue();
                        self.queue_halted = true;
                    }
                    self.running = false;
                    self.partial = None;
                    if result.is_err() {
                        self.halt_queue();
                        self.queue_halted = true;
                        if durable_attempt {
                            self.pending_draft = None;
                            self.pending_images.clear();
                        } else {
                            self.restore_pending_draft();
                        }
                    } else {
                        self.pending_draft = None;
                    }
                    self.status = match result {
                        Ok(()) if self.signal.is_cancelled() => "已停止".into(),
                        Ok(()) if self.compacting => "上下文已压缩 · 原始消息已归档".into(),
                        Ok(()) => self
                            .session
                            .context
                            .messages
                            .iter()
                            .rev()
                            .find_map(|message| match message {
                                Message::Assistant(assistant) => Some(assistant),
                                _ => None,
                            })
                            .and_then(|assistant| assistant.error_message.clone())
                            .unwrap_or_else(|| "已完成 · 会话已保存".into()),
                        Err(_) if self.signal.is_cancelled() => "已停止".into(),
                        Err(error) => error,
                    };
                    self.compacting = false;
                    self.save();
                    self.receiver = None;
                    if let Some(worker) = self.worker.take() {
                        let _ = worker.join();
                    }
                    for event in self.automation.drain_events() {
                        self.runtime_activity = runtime_event_text(&event);
                    }
                }
                WorkerMessage::InputHandled => {
                    self.pending_images.clear();
                    self.running = false;
                    self.partial = None;
                    self.tool_activity.clear();
                    self.pending_draft = None;
                    self.receiver = None;
                    self.status = "输入已由扩展处理，未请求模型或保存消息".into();
                    if let Some(worker) = self.worker.take() {
                        let _ = worker.join();
                    }
                }
                WorkerMessage::InputFailed(error) => {
                    self.halt_queue();
                    self.queue_halted = true;
                    self.running = false;
                    self.partial = None;
                    self.receiver = None;
                    let retained = self.workbench.durable && self.pending_queue_id.take().is_some();
                    if retained {
                        self.pending_draft = None;
                        self.pending_images.clear();
                    } else {
                        self.restore_pending_draft();
                    }
                    self.status = if retained {
                        format!(
                            "输入未开始执行：{error}。消息保留在队列恢复记录中，请检查后明确重试。"
                        )
                    } else if self.signal.is_cancelled() {
                        "输入处理已取消，未接收输入已保留".into()
                    } else {
                        error
                    };
                    if let Some(worker) = self.worker.take() {
                        let _ = worker.join();
                    }
                }
                WorkerMessage::SessionCommitted(session, ack) => {
                    self.session = *session;
                    self.session_name_draft = self
                        .session
                        .get_session_name()
                        .ok()
                        .flatten()
                        .unwrap_or_default();
                    self.status = "会话名称已保存，正在通知扩展…".into();
                    let _ = ack.send(());
                }
                WorkerMessage::ForkFinished(session, result) => {
                    self.session = *session;
                    self.running = false;
                    self.extension_operation = false;
                    self.receiver = None;
                    if let Some(worker) = self.worker.take() {
                        let _ = worker.join();
                    }
                    self.status = match result {
                        Ok(text) => {
                            self.reset_view();
                            self.input = text;
                            self.start_model_restore(ctx);
                            "已定位到用户消息之前，编辑后发送创建分支".into()
                        }
                        Err(error) => error,
                    };
                    self.sessions = self.store.list().unwrap_or_else(|_| self.sessions.clone());
                }
                WorkerMessage::ExtensionFinished(session, registry, paths, result) => {
                    let restore_model = matches!(&result, Ok(None)) && !self.closing;
                    let changed_session = session.id != self.session.id;
                    self.session = *session;
                    self.session_name_draft = self
                        .session
                        .get_session_name()
                        .ok()
                        .flatten()
                        .unwrap_or_default();
                    self.workspace = self.session.workspace.to_string_lossy().into_owned();
                    self.extensions = registry;
                    self.loaded_extension_paths = paths;
                    self.running = false;
                    self.extension_operation = false;
                    self.receiver = None;
                    if changed_session {
                        self.reset_view();
                    }
                    self.status = match result {
                        Ok(Some(result)) => {
                            self.pending_draft = None;
                            self.apply_command_display(&result);
                            if self.signal.is_cancelled() {
                                "变更已保存；扩展通知已取消".into()
                            } else {
                                "扩展命令已完成 · 状态已保存".into()
                            }
                        }
                        Ok(None) => {
                            self.extension_statuses.clear();
                            format!("原生扩展已就绪 · {} 个扩展", self.extensions.ids().len())
                        }
                        Err(error) => {
                            self.restore_pending_draft();
                            if self.signal.is_cancelled() {
                                "扩展操作已取消".into()
                            } else {
                                error
                            }
                        }
                    };
                    if let Some(worker) = self.worker.take() {
                        let _ = worker.join();
                    }
                    if self.extensions.ids().is_empty() {
                        self.extension_statuses.clear();
                    }
                    self.sessions = self.store.list().unwrap_or_else(|_| self.sessions.clone());
                    if restore_model {
                        self.start_model_restore(ctx);
                    }
                }
            }
        }
        for diagnostic in self.extensions.drain_diagnostics() {
            self.extension_display.push(format!(
                "{} / {} · {}",
                diagnostic.extension_id, diagnostic.event, diagnostic.error
            ));
        }
        if self.extension_display.len() > 128 {
            self.extension_display
                .drain(..self.extension_display.len() - 128);
        }
        if disconnected && self.running {
            self.halt_queue();
            self.queue_halted = true;
            self.signal.cancel();
            self.running = false;
            self.compacting = false;
            self.extension_operation = false;
            self.partial = None;
            self.restore_pending_draft();
            self.receiver = None;
            self.status = "Agent 工作线程意外结束，已保存完成的消息".into();
            self.save();
            if let Some(worker) = self.worker.take() {
                let _ = worker.join();
            }
        }
        if self.workbench.durable && self.queue_available() {
            if let Some(error) = self.queues.storage_error() {
                self.queue_store_failed(error.to_string());
            }
        }
        if !self.workbench.durable {
            let admitted = self.queues.take_admitted_ids();
            self.queued_drafts
                .retain(|(id, _, _)| !admitted.contains(id));
        }
        if !self.busy()
            && !self.closing
            && self.task_execution_allowed()
            && !self.signal.is_cancelled()
            && !self.queue_halted
        {
            if self.workbench.durable {
                self.dispatch_durable_queue(ctx);
            } else if let Some(item) = self.queues.take_next() {
                let message = item.message.clone();
                let raw = self
                    .queued_drafts
                    .iter()
                    .position(|(id, _, _)| *id == item.id)
                    .map(|index| self.queued_drafts.remove(index));
                self.pending_draft = Some(
                    raw.as_ref()
                        .map_or_else(|| message.text(), |(_, _, raw)| raw.clone()),
                );
                if let Message::User(user) = &message {
                    self.pending_images = user
                        .content
                        .iter()
                        .filter(|part| matches!(part, Content::Image { .. }))
                        .cloned()
                        .collect();
                }
                self.start_prompt_worker(ctx, message.clone(), true);
                if !self.running {
                    if let Some(raw) = raw {
                        self.queued_drafts.push(raw);
                    }
                    let _ = self.queues.restore(item);
                    self.pending_draft = None;
                    self.pending_images.clear();
                }
            }
        }
        if !self.workbench.durable && !self.running && self.signal.is_cancelled() {
            let drafts = std::mem::take(&mut self.queued_drafts);
            let pending = self.queues.snapshot();
            for (id, message, draft) in drafts.into_iter().rev() {
                // Pending messages remain manageable after stopping the current run.
                // Only drafts explicitly removed from the queue need restoration.
                if pending.items.iter().any(|item| item.id == id) {
                    self.queued_drafts.insert(0, (id, message, draft));
                } else {
                    self.restore_draft(Some(draft));
                }
            }
        }
    }
    fn new_session(&mut self, ctx: &egui::Context) {
        self.workbench.page = WorkbenchPage::Conversation;
        let session = Session {
            workspace: PathBuf::from(&self.workspace),
            ..Default::default()
        };
        self.start_extension_action(
            ctx,
            ExtensionAction::Replace(Box::new(session), SessionStartReason::New),
        );
    }

    fn sidebar_ui(&mut self, ctx: &egui::Context) {
        let palette = shell::palette(ctx);
        egui::SidePanel::left("sidebar")
            .exact_width(shell::SIDEBAR_WIDTH)
            .resizable(false)
            .frame(egui::Frame::new().fill(palette.sidebar).inner_margin(14))
            .show(ctx, |ui| {
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    shell::mark(ui, 27.0);
                    ui.label(RichText::new("Agent Core").size(16.0).strong());
                });
                ui.add_space(22.0);
                if ui
                    .add_enabled(
                        !self.busy(),
                        egui::Button::new("＋   新会话")
                            .min_size(egui::vec2(ui.available_width(), 38.0))
                            .frame(false),
                    )
                    .on_hover_text("在当前工作区创建新会话")
                    .clicked()
                {
                    self.new_session(ctx);
                }
                ui.add_space(20.0);
                self.workbench_navigation(ui);
                ui.add_space(8.0);
                ui.label(RichText::new("最近会话").size(11.0).color(palette.muted));
                ui.add_space(4.0);
                egui::ScrollArea::vertical()
                    .id_salt("sessions")
                    .max_height((ui.available_height() - 228.0).max(40.0))
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        if self.sessions.is_empty() {
                            ui.add_space(8.0);
                            ui.label(
                                RichText::new(
                                    "还没有会话
开始一个任务，记录会保存在本地。",
                                )
                                .size(12.0)
                                .color(palette.muted),
                            );
                        }
                        let mut selected = None;
                        let mut visible: Vec<_> = self
                            .sessions
                            .iter()
                            .filter(|s| self.task_state(&s.id).is_some_and(|state| !state.archived))
                            .collect();
                        visible.sort_by_key(|s| self.task_sort_key(s));
                        for session in visible {
                            if ui
                                .add_enabled(
                                    !self.busy(),
                                    egui::Button::new(&session.name)
                                        .selected(session.id == self.session.id)
                                        .truncate()
                                        .min_size(egui::vec2(ui.available_width(), 34.0)),
                                )
                                .on_hover_text(&session.name)
                                .clicked()
                            {
                                selected = Some(session.clone());
                            }
                        }
                        if let Some(session) = selected {
                            self.workbench.page = WorkbenchPage::Conversation;
                            self.start_extension_action(
                                ctx,
                                ExtensionAction::Replace(
                                    Box::new(session),
                                    SessionStartReason::Resume,
                                ),
                            );
                        }
                    });
                ui.add_space((ui.available_height() - 196.0).max(0.0));
                ui.separator();
                ui.add_space(4.0);
                shell::theme_controls(ui);
                if ui
                    .add(
                        egui::Button::new("模型服务 · 添加 / 快速切换")
                            .frame(false)
                            .min_size(egui::vec2(ui.available_width(), 30.0)),
                    )
                    .clicked()
                {
                    self.open_connections(ctx);
                }
                let workspace = PathBuf::from(&self.workspace);
                let label = workspace
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("本地工作区");
                if ui
                    .add(
                        egui::Button::new(format!("工作区 · {label}"))
                            .frame(false)
                            .truncate()
                            .min_size(egui::vec2(ui.available_width(), 30.0)),
                    )
                    .on_hover_text(&self.workspace)
                    .clicked()
                {
                    self.settings_open = true;
                }
                if ui
                    .add(
                        egui::Button::new("设置与工具")
                            .frame(false)
                            .min_size(egui::vec2(ui.available_width(), 32.0)),
                    )
                    .on_hover_text("模型、权限、登录、会话树与原生扩展")
                    .clicked()
                {
                    self.settings_open = true;
                }
                ui.label(
                    RichText::new("本地会话  ·  Rust 原生桌面")
                        .size(10.0)
                        .color(palette.muted),
                );
            });
    }

    fn settings_ui(&mut self, ctx: &egui::Context) {
        let palette = shell::palette(ctx);
        if !self.settings_open {
            return;
        }
        let mut open = self.settings_open;
        egui::Window::new("设置与工具")
            .id(egui::Id::new("desktop-settings"))
            .open(&mut open)
            .default_width(480.0)
            .max_width(600.0)
            .max_height((ctx.screen_rect().height() - 96.0).max(240.0))
            .vscroll(true)
            .show(ctx, |ui| {
                shell::theme_controls(ui);
                ui.label(
                    RichText::new("配置只在点击应用后生效，打开此窗口不会发起模型请求。")
                        .size(12.0)
                        .color(palette.muted),
                );
                ui.add_space(8.0);
                egui::CollapsingHeader::new("会话名称").show(ui, |ui| {
                    ui.add_enabled_ui(!self.busy(), |ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut self.session_name_draft)
                                .hint_text("会话名称（空值清除）")
                                .desired_width(230.0),
                        );
                        ui.horizontal(|ui| {
                            if ui.button("应用名称").clicked() {
                                self.start_extension_action(
                                    ctx,
                                    ExtensionAction::Name(self.session_name_draft.clone()),
                                );
                            }
                            if ui.button("清除名称").clicked() {
                                self.start_extension_action(
                                    ctx,
                                    ExtensionAction::Name(String::new()),
                                );
                            }
                        });
                    });
                });
                ui.add_enabled_ui(!self.busy(), |ui| {
                    ui.label(RichText::new("工作区").size(12.0).color(palette.muted));
                    ui.add(egui::TextEdit::singleline(&mut self.workspace).desired_width(230.0));
                    ui.add_space(6.0);
                    ui.checkbox(&mut self.allow_changes, "允许写入 / 编辑文件");
                    ui.checkbox(&mut self.allow_commands, "允许运行系统命令");
                    ui.add_space(12.0);
                    self.model_configuration_ui(ui, ctx);
                    egui::CollapsingHeader::new("导入旧版会话树").show(ui, |ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut self.import_path)
                                .hint_text("旧版 .jsonl 路径")
                                .desired_width(230.0),
                        );
                        ui.add(
                            egui::TextEdit::singleline(&mut self.import_leaf)
                                .hint_text("分支 entry ID（空=最新）")
                                .desired_width(230.0),
                        );
                        if ui.button("导入分支（保留原件）").clicked() {
                            let result =
                                load_legacy_session(std::path::Path::new(self.import_path.trim()))
                                    .and_then(|legacy| {
                                        legacy.import(if self.import_leaf.trim().is_empty() {
                                            LeafSelection::Latest
                                        } else {
                                            LeafSelection::Entry(self.import_leaf.trim().into())
                                        })
                                    });
                            match result {
                                Ok(session) => self.start_extension_action(
                                    ctx,
                                    ExtensionAction::Replace(
                                        Box::new(session),
                                        SessionStartReason::Resume,
                                    ),
                                ),
                                Err(error) => self.status = error,
                            }
                        }
                    });
                });

                egui::CollapsingHeader::new("登录 / 凭据").show(ui, |ui| self.auth_ui(ui, ctx));
                self.tree_ui(ui);
                self.extensions_ui(ui, ctx);
            });
        self.settings_open = open;
    }

    fn header_ui(&mut self, ctx: &egui::Context) {
        let palette = shell::palette(ctx);
        egui::TopBottomPanel::top("header")
            .exact_height(56.0)
            .frame(
                egui::Frame::new()
                    .fill(palette.background)
                    .inner_margin(egui::Margin::symmetric(24, 12)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    let title_width = (ui.available_width() - 330.0).max(40.0);
                    ui.add_sized(
                        [title_width, 24.0],
                        egui::Label::new(RichText::new(&self.session.name).size(14.0).strong())
                            .truncate(),
                    )
                    .on_hover_text(&self.session.name);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        self.workbench_header(ui);
                        ui.menu_button("···", |ui| {
                            if ui
                                .add_enabled(
                                    !self.busy()
                                        && self
                                            .session
                                            .context
                                            .messages
                                            .iter()
                                            .filter(|message| matches!(message, Message::User(_)))
                                            .count()
                                            > 2,
                                    egui::Button::new("压缩上下文"),
                                )
                                .clicked()
                            {
                                self.compact(ctx);
                            }
                            if ui
                                .add_enabled(!self.busy(), egui::Button::new("导出 HTML"))
                                .clicked()
                            {
                                let path = PathBuf::from(&self.workspace)
                                    .join(format!("agent-core-session-{}.html", self.session.id));
                                self.status = match SessionStore::export_html(&self.session, &path)
                                {
                                    Ok(()) => format!("完整会话树已导出 {}", path.display()),
                                    Err(error) => format!("导出失败：{error}"),
                                };
                            }
                            if ui
                                .add_enabled(
                                    !self.busy() && !self.session.context.messages.is_empty(),
                                    egui::Button::new("导出 JSONL"),
                                )
                                .clicked()
                            {
                                let path = PathBuf::from(&self.workspace)
                                    .join(format!("agent-core-session-{}.jsonl", self.session.id));
                                self.status =
                                    match SessionStore::export_branch_jsonl(&self.session, &path) {
                                        Ok(()) => format!("已导出 {}", path.display()),
                                        Err(error) => format!("导出失败：{error}"),
                                    };
                            }
                        });
                        ui.label(
                            RichText::new(if self.settings.mode == ProviderMode::Demo {
                                "离线演示"
                            } else {
                                "模型已配置"
                            })
                            .size(11.0)
                            .color(palette.muted),
                        );
                    });
                });
            });
    }

    fn composer_ui(&mut self, ctx: &egui::Context) {
        let palette = shell::palette(ctx);
        egui::TopBottomPanel::bottom("composer")
            .min_height(184.0)
            .frame(
                egui::Frame::new()
                    .fill(palette.background)
                    .inner_margin(egui::Margin::symmetric(24, 12)),
            )
            .show(ctx, |ui| {
                shell::column(ui, |ui| {
                    self.task_execution_banner(ui);
                    egui::Frame::new()
                        .fill(palette.surface)
                        .corner_radius(20)
                        .stroke(egui::Stroke::new(1.0, palette.border))
                        .inner_margin(14)
                        .show(ui, |ui| {
                            if !self.input_images.is_empty() {
                                ui.horizontal(|ui| {
                                    ui.label(format!(
                                        "{} 个保留的图片附件",
                                        self.input_images.len()
                                    ));
                                    if ui.small_button("移除附件").clicked() {
                                        self.input_images.clear();
                                    }
                                });
                            }
                            let response = egui::ScrollArea::vertical()
                                .id_salt("composer-scroll")
                                .max_height(80.0)
                                .show(ui, |ui| {
                                    ui.add_enabled(
                                        !self.closing,
                                        egui::TextEdit::multiline(&mut self.input)
                                            .hint_text("描述任务，或提问项目中的代码…")
                                            .id_salt("message-input")
                                            .frame(false)
                                            .desired_rows(2)
                                            .desired_width(f32::INFINITY),
                                    )
                                })
                                .inner;
                            let shortcut = !self.ui_dialogs.has_pending()
                                && response.has_focus()
                                && ctx.input_mut(|input| {
                                    input.consume_shortcut(&self.keybindings.send)
                                });
                            ui.horizontal(|ui| {
                                let model_label = if self.settings.mode == ProviderMode::Demo {
                                    "离线演示".to_owned()
                                } else {
                                    self.settings.model.clone()
                                };
                                let model_width = (ui.available_width() * 0.5).min(220.0);
                                if ui
                                    .add_sized(
                                        [model_width, 34.0],
                                        egui::Button::new(format!("模型 · {model_label}"))
                                            .frame(false)
                                            .truncate(),
                                    )
                                    .on_hover_text(
                                        "添加模型服务 / 快速切换；高级设置内配置 Thinking",
                                    )
                                    .clicked()
                                {
                                    self.open_connections(ctx);
                                }
                                let permission = if self.allow_commands {
                                    "允许命令"
                                } else if self.allow_changes {
                                    "允许编辑"
                                } else {
                                    "只读工作区"
                                };
                                if ui
                                    .add(
                                        egui::Button::new(
                                            RichText::new(permission)
                                                .size(11.0)
                                                .color(palette.muted),
                                        )
                                        .frame(false),
                                    )
                                    .on_hover_text("查看工作区与执行权限")
                                    .clicked()
                                {
                                    self.settings_open = true;
                                }
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        if self.running
                                            || self.queue_receiver.is_some()
                                            || self.model_receiver.is_some()
                                        {
                                            if ui.button("停止").clicked() {
                                                self.signal.cancel();
                                                self.queue_signal.cancel();
                                                self.cancel_queue_io();
                                                self.halt_queue();
                                                self.status = "正在停止…".into();
                                            }
                                            ui.spinner();
                                            if !self.compacting
                                                && !self.extension_operation
                                                && self.model_receiver.is_none()
                                                && self.queue_receiver.is_none()
                                                && self.task_execution_allowed()
                                                && !self.input.trim().is_empty()
                                                && ui.button("插入指令").clicked()
                                            {
                                                self.start_steering_input(ctx);
                                            }
                                        } else if ui
                                            .add_enabled(
                                                !self.input.trim().is_empty()
                                                    && !self.busy()
                                                    && self.task_execution_allowed(),
                                                egui::Button::new(
                                                    RichText::new("↑")
                                                        .size(22.0)
                                                        .color(palette.sidebar),
                                                )
                                                .fill(palette.text)
                                                .corner_radius(16)
                                                .min_size(egui::vec2(34.0, 34.0)),
                                            )
                                            .on_hover_text("发送任务")
                                            .clicked()
                                            || shortcut
                                        {
                                            self.start(ctx);
                                        }
                                    },
                                );
                            });
                        });
                    ui.add_space(2.0);
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::Label::new(
                                RichText::new(&self.status).size(11.0).color(palette.muted),
                            )
                            .truncate(),
                        )
                        .on_hover_text(&self.status);
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.label(
                                RichText::new(format!(
                                    "{} 发送",
                                    ctx.format_shortcut(&self.keybindings.send)
                                ))
                                .size(10.0)
                                .color(palette.muted),
                            );
                        });
                    });
                });
            });
    }

    fn transcript_ui(&mut self, ctx: &egui::Context) {
        let palette = shell::palette(ctx);
        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(palette.background)
                    .inner_margin(egui::Margin::symmetric(24, 8)),
            )
            .show(ctx, |ui| {
                egui::ScrollArea::vertical()
                    .id_salt("transcript")
                    .stick_to_bottom(true)
                    .show(ui, |ui| {
                        shell::column(ui, |ui| {
                            for text in &self.extension_display {
                                egui::Frame::new()
                                    .fill(palette.surface)
                                    .corner_radius(8)
                                    .inner_margin(12)
                                    .show(ui, |ui| {
                                        ui.label(
                                            RichText::new("EXTENSION · 展示输出")
                                                .size(11.0)
                                                .color(palette.text),
                                        );
                                        ui.label(text);
                                    });
                            }
                            if self.session.context.messages.is_empty() {
                                ui.add_space(
                                    (ctx.screen_rect().height() * 0.16).clamp(32.0, 140.0),
                                );
                                if let Some(prompt) = shell::welcome(ui) {
                                    self.input = prompt.into();
                                }
                            }
                            for (index, message) in self.session.context.messages.iter().enumerate()
                            {
                                if let Message::ToolResult(result) = message {
                                    egui::CollapsingHeader::new(format!(
                                        "{} · {}",
                                        result.tool_name,
                                        if result.is_error { "错误" } else { "结果" }
                                    ))
                                    .id_salt(index)
                                    .show(ui, |ui| {
                                        ui.label(
                                            RichText::new(message.text()).monospace().size(13.0),
                                        );
                                        if let Some(diff) = result.details["diff"].as_str() {
                                            ui.collapsing("代码差异", |ui| {
                                                ui.label(
                                                    RichText::new(diff).monospace().size(13.0),
                                                );
                                            });
                                        }
                                        show_images(
                                            ui,
                                            &mut self.images,
                                            &format!(
                                                "{}-{index}-{}",
                                                self.session.id, result.timestamp
                                            ),
                                            &result.content,
                                        );
                                    });
                                    continue;
                                }
                                egui::Frame::new()
                                    .fill(if matches!(message, Message::User(_)) {
                                        palette.surface
                                    } else {
                                        palette.background
                                    })
                                    .corner_radius(16)
                                    .inner_margin(16)
                                    .show(ui, |ui| {
                                        ui.set_min_width((ui.available_width() - 8.0).max(0.0));
                                        ui.label(
                                            RichText::new(if matches!(message, Message::User(_)) {
                                                "你"
                                            } else {
                                                "Agent Core"
                                            })
                                            .size(11.0)
                                            .strong()
                                            .color(palette.text),
                                        );
                                        ui.label(message.text());
                                        let content = match message {
                                            Message::User(message) => &message.content,
                                            Message::Assistant(message) => &message.content,
                                            _ => return,
                                        };
                                        show_images(
                                            ui,
                                            &mut self.images,
                                            &format!("{}-{index}", self.session.id),
                                            content,
                                        );
                                        if let Message::Assistant(assistant) = message {
                                            for block in &assistant.content {
                                                if let Content::Thinking { thinking } = block {
                                                    ui.collapsing("思考", |ui| {
                                                        ui.label(thinking);
                                                    });
                                                }
                                            }
                                            if let Some(error) = &assistant.error_message {
                                                ui.colored_label(
                                                    ui.visuals().error_fg_color,
                                                    error,
                                                );
                                            }
                                            for call in assistant.tool_calls() {
                                                ui.label(
                                                    RichText::new(format!(
                                                        "调用 {}  {}",
                                                        call.name, call.arguments
                                                    ))
                                                    .monospace()
                                                    .size(12.0)
                                                    .color(palette.muted),
                                                );
                                            }
                                        }
                                    });
                                ui.add_space(12.0);
                            }
                            if let Some(partial) = &self.partial {
                                ui.label(
                                    RichText::new("AGENT CORE · 正在生成")
                                        .size(11.0)
                                        .color(palette.text),
                                );
                                ui.label(Message::Assistant(partial.clone()).text());
                            }
                            if self.running {
                                if let Some(activity) = self.tool_activity.last() {
                                    ui.label(
                                        RichText::new(activity).size(12.0).color(palette.text),
                                    );
                                }
                            }
                        });
                    });
            });
    }

    pub fn show(&mut self, ctx: &egui::Context) {
        self.poll(ctx);
        if ctx.input(|input| input.viewport().close_requested())
            && (self.busy() || self.task_management_busy() || !self.extensions.ids().is_empty())
        {
            self.closing = true;
            self.cancel_task_management();
            self.ui_dialogs.close();
            self.signal.cancel();
            self.queue_signal.cancel();
            self.auth_signal.cancel();
            self.connections.cancel();
            self.cancel_queue_io();
            self.status = "正在停止并保存会话…".into();
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
        }
        if self.closing && !self.busy() && !self.task_management_busy() {
            if self.extensions.ids().is_empty() {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            } else {
                self.start_extension_action(ctx, ExtensionAction::Quit);
            }
        }
        self.sidebar_ui(ctx);
        self.header_ui(ctx);
        if self.workbench.page == WorkbenchPage::Tasks {
            self.tasks_ui(ctx);
        } else {
            self.inspector_panel(ctx);
            self.composer_ui(ctx);
            self.transcript_ui(ctx);
            self.inspector_window(ctx);
        }
        self.settings_ui(ctx);
        self.queue_window(ctx);
        self.connections_ui(ctx);
        if let Err(error) = self.ui_dialogs.show(ctx, self.keybindings.send) {
            self.status = error;
        }
        if self.busy()
            || (cfg!(feature = "ui-screenshot")
                && std::env::var_os("EFRAME_SCREENSHOT_TO").is_some())
        {
            ctx.request_repaint_after(Duration::from_millis(50));
        }
    }
}

fn show_images(
    ui: &mut egui::Ui,
    cache: &mut BTreeMap<String, Result<egui::TextureHandle, String>>,
    key: &str,
    content: &[Content],
) {
    for (index, block) in content.iter().enumerate() {
        let Content::Image { data, .. } = block else {
            continue;
        };
        let key = format!("{key}-{index}-{}", data.len());
        let image = cache.entry(key.clone()).or_insert_with(|| {
            if data.len() > 8_388_608 {
                return Err("图片超过预览上限".into());
            }
            let bytes = STANDARD.decode(data).map_err(|e| e.to_string())?;
            let mut reader = image::ImageReader::new(Cursor::new(bytes))
                .with_guessed_format()
                .map_err(|e| e.to_string())?;
            let mut limits = image::Limits::default();
            limits.max_image_width = Some(4096);
            limits.max_image_height = Some(4096);
            limits.max_alloc = Some(67_108_864);
            reader.limits(limits);
            let pixels = reader.decode().map_err(|e| e.to_string())?.to_rgba8();
            let size = [pixels.width() as usize, pixels.height() as usize];
            Ok(ui.ctx().load_texture(
                key,
                egui::ColorImage::from_rgba_unmultiplied(size, pixels.as_raw()),
                egui::TextureOptions::LINEAR,
            ))
        });
        match image {
            Ok(texture) => {
                let size = texture.size_vec2();
                let scale = (ui.available_width().min(600.0) / size.x)
                    .min(400.0 / size.y)
                    .min(1.0);
                ui.image((texture.id(), size * scale));
            }
            Err(error) => {
                ui.label(format!("图片预览：{error}"));
            }
        }
    }
}

impl eframe::App for DesktopApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.show(ctx);
    }
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        if !self.stop_workers() {
            self.save();
        }
        self.shutdown_extensions();
    }
}
impl Drop for DesktopApp {
    fn drop(&mut self) {
        self.stop_workers();
        self.shutdown_extensions();
    }
}

fn shutdown_registry(
    registry: &SharedRegistry,
    session: &Session,
    model: Option<ModelIdentity>,
    reason: SessionShutdownReason,
) -> Result<(), String> {
    let context = extension_context(session, registry, ExtensionMode::Desktop, model)?;
    registry.emit_event(
        &LifecycleEvent::SessionShutdown {
            reason,
            target_session_file: None,
        },
        &context,
        &CancellationToken::new(),
    )
}

fn canonical_provider(mode: &ProviderMode) -> &'static str {
    match mode {
        ProviderMode::Demo => "demo",
        ProviderMode::OpenAiCompatible | ProviderMode::OpenAiResponses => "openai",
        ProviderMode::OpenAiCodex => CODEX_PROVIDER,
        ProviderMode::Anthropic => "anthropic",
        ProviderMode::Google => "google",
        ProviderMode::AzureOpenAiResponses => "azure-openai-responses",
        ProviderMode::GoogleVertex => "google-vertex",
        ProviderMode::AmazonBedrock => "amazon-bedrock",
        ProviderMode::Mistral => "mistral",
        ProviderMode::PiMessages => "pi-messages",
    }
}

fn settings_identity(settings: &ProviderSettings) -> ModelIdentity {
    ModelIdentity {
        provider: if settings.mode == ProviderMode::Demo {
            "demo".into()
        } else if settings.options.provider_id.is_empty() {
            canonical_provider(&settings.mode).into()
        } else {
            settings.options.provider_id.clone()
        },
        model: settings.model.clone(),
    }
}

fn configuration_budget(configuration: &ModelConfiguration) -> Option<ModelBudget> {
    let identity = settings_identity(&configuration.settings);
    let mut budget = (configuration.settings.mode != ProviderMode::AzureOpenAiResponses
        && configuration.settings.mode != ProviderMode::Demo)
        .then(|| ModelBudget::from_catalog(&identity.provider, &identity.model))
        .flatten()
        .or_else(|| {
            (configuration.context_window > 0 && configuration.settings.mode != ProviderMode::Demo)
                .then_some(ModelBudget {
                    provider: identity.provider,
                    id: identity.model,
                    context_window: configuration.context_window,
                    max_output_tokens: configuration.max_output_tokens,
                    cost: serde_json::Value::Null,
                })
        });
    if configuration.settings.mode == ProviderMode::AmazonBedrock {
        if let (Some(budget), Some(limit)) = (
            &mut budget,
            configuration.settings.options.bedrock.max_tokens,
        ) {
            budget.max_output_tokens = u64::from(limit);
        }
    }
    budget
}

fn configuration_for_identity(
    current: &ModelConfiguration,
    identity: &ModelIdentity,
) -> Result<ModelConfiguration, String> {
    let mut configuration = current.clone();
    let same_provider = settings_identity(&current.settings).provider == identity.provider;
    if !same_provider {
        configuration.settings.options.oauth = None;
        configuration.settings.options.oauth_config = None;
        configuration.settings.options.api_key_explicit = false;
        let mode = agent_runtime::catalog::find(&identity.provider, &identity.model)
            .and_then(|model| model.mode())
            .or_else(|| {
                agent_runtime::connections::Service::from_id(&identity.provider)
                    .map(|service| service.mode())
            })
            .or(match identity.provider.as_str() {
                "demo" => Some(ProviderMode::Demo),
                "openai-compatible" => Some(ProviderMode::OpenAiCompatible),
                "openai-responses" => Some(ProviderMode::OpenAiResponses),
                "openai-codex" => Some(ProviderMode::OpenAiCodex),
                "anthropic" => Some(ProviderMode::Anthropic),
                "google" => Some(ProviderMode::Google),
                "azure-openai-responses" => Some(ProviderMode::AzureOpenAiResponses),
                "google-vertex" => Some(ProviderMode::GoogleVertex),
                "amazon-bedrock" => Some(ProviderMode::AmazonBedrock),
                "mistral" => Some(ProviderMode::Mistral),
                "pi-messages" => Some(ProviderMode::PiMessages),
                _ => None,
            })
            .ok_or("此原生模型 identity 未包含可验证协议，请在模型连接中选择协议并应用配置")?;
        configuration.settings.mode = mode;
        configuration.settings.base_url =
            agent_runtime::catalog::find(&identity.provider, &identity.model)
                .filter(|_| {
                    !matches!(
                        configuration.settings.mode,
                        ProviderMode::AzureOpenAiResponses
                            | ProviderMode::GoogleVertex
                            | ProviderMode::AmazonBedrock
                    )
                })
                .map_or_else(
                    || {
                        agent_runtime::connections::Service::from_id(&identity.provider)
                            .map_or_else(
                                || configuration.settings.mode.default_base_url(),
                                |service| service.base_url(),
                            )
                            .into()
                    },
                    |record| record.base_url.clone(),
                );
        configuration.settings.api_key = if configuration.settings.mode == ProviderMode::OpenAiCodex
        {
            String::new()
        } else {
            resolve_api_key(&identity.provider)
        };
        fill_cloud_defaults(&mut configuration.settings);
    }
    configuration.settings.options.provider_id = identity.provider.clone();
    configuration.settings.model = identity.model.clone();
    if !same_provider || current.settings.model != identity.model {
        let budget = ModelBudget::from_catalog(&identity.provider, &identity.model)
            .filter(|_| configuration.settings.mode != ProviderMode::AzureOpenAiResponses);
        configuration.context_window = budget.as_ref().map_or(0, |budget| budget.context_window);
        configuration.max_output_tokens = budget.map_or(0, |budget| budget.max_output_tokens);
    }
    Ok(configuration)
}

fn fill_cloud_defaults(settings: &mut ProviderSettings) {
    match settings.mode {
        ProviderMode::AzureOpenAiResponses => {
            let defaults = AzureOptions::from_env();
            if settings.options.azure.resource_name.is_empty() {
                settings.options.azure.resource_name = defaults.resource_name;
            }
            if settings.options.azure.base_url.is_empty() {
                settings.options.azure.base_url = defaults.base_url;
            }
        }
        ProviderMode::GoogleVertex => {
            let defaults = VertexOptions::from_env();
            if settings.options.vertex.project.is_empty() {
                settings.options.vertex.project = defaults.project;
            }
            if settings.options.vertex.location.is_empty() {
                settings.options.vertex.location = defaults.location;
            }
            if settings.options.vertex.adc_path.is_none() {
                settings.options.vertex.adc_path = defaults.adc_path;
            }
        }
        ProviderMode::AmazonBedrock => {
            let defaults = BedrockOptions::from_env();
            if settings.options.bedrock.region.is_empty() {
                settings.options.bedrock.region = defaults.region;
            }
            if settings.options.bedrock.profile.is_empty() {
                settings.options.bedrock.profile = defaults.profile;
            }
            if settings.options.bedrock.credentials_file.is_none() {
                settings.options.bedrock.credentials_file = defaults.credentials_file;
            }
            if settings.options.bedrock.config_file.is_none() {
                settings.options.bedrock.config_file = defaults.config_file;
            }
            if settings.options.bedrock.sts_endpoint.is_empty() {
                settings.options.bedrock.sts_endpoint = defaults.sts_endpoint;
            }
            if settings.options.bedrock.sso_endpoint.is_empty() {
                settings.options.bedrock.sso_endpoint = defaults.sso_endpoint;
            }
            if settings.options.bedrock.sso_oidc_endpoint.is_empty() {
                settings.options.bedrock.sso_oidc_endpoint = defaults.sso_oidc_endpoint;
            }
        }
        _ => {}
    }
}

fn refresh_configuration_budget(configuration: &mut ModelConfiguration) {
    let identity = settings_identity(&configuration.settings);
    let budget = (configuration.settings.mode != ProviderMode::AzureOpenAiResponses)
        .then(|| ModelBudget::from_catalog(&identity.provider, &identity.model))
        .flatten();
    configuration.context_window = budget.as_ref().map_or(0, |budget| budget.context_window);
    configuration.max_output_tokens = budget.map_or(0, |budget| budget.max_output_tokens);
}

fn expand_user_prompt(prompt: Message, resources: &ResourceLoad) -> Result<Message, String> {
    let text = prepare_prompt(&prompt.text(), resources)?;
    let Message::User(mut user) = prompt else {
        return Err("Input preparation requires a user message".into());
    };
    let mut content = vec![Content::text(text)];
    content.extend(
        user.content
            .into_iter()
            .filter(|block| matches!(block, Content::Image { .. })),
    );
    user.content = content;
    Ok(Message::User(user))
}

fn runtime_event_text(event: &RuntimeEvent) -> String {
    match event {
        RuntimeEvent::BudgetDecision { decision } => format!(
            "预算 {} · 归档 {:.0} tokens · 预计剩余请求 {:?}",
            decision.reason, decision.archive_tokens, decision.expected_remaining_requests
        ),
        RuntimeEvent::CompactionStart { reason } => format!("自动摘要开始：{reason}"),
        RuntimeEvent::CompactionEnd {
            reason,
            error,
            aborted,
            ..
        } => format!(
            "自动摘要结束：{reason} · {}",
            if *aborted {
                "已取消"
            } else if error.is_some() {
                "失败，原上下文保留"
            } else {
                "已保存checkpoint"
            }
        ),
        RuntimeEvent::RetryScheduled {
            attempt,
            max_attempts,
            delay_ms,
            ..
        } => format!("模型重试 {attempt}/{max_attempts} · 等待 {delay_ms} ms"),
        RuntimeEvent::RetryAttemptStart { attempt, .. } => format!("正在执行模型重试 {attempt}"),
        RuntimeEvent::RetryFinished {
            success, attempt, ..
        } => format!(
            "模型重试 {attempt} · {}",
            if *success { "成功" } else { "已结束" }
        ),
    }
}

fn compact_with_ledger(
    session: &mut Session,
    provider: &mut dyn Provider,
    model: Option<&ModelBudget>,
    handle: &AutomationHandle,
    retry: RetryOptions,
    signal: &CancellationToken,
) -> Result<(), String> {
    let before = session.clone();
    let mut provider = RetryingProvider::new(provider, retry, handle.clone(), "compaction", None);
    let result = compact_session(session, &mut provider, 2, signal)?;
    if let Err(error) = record_manual_checkpoint(session, &before, &result, model, handle) {
        *session = before;
        return Err(error);
    }
    Ok(())
}

pub fn smoke_test() -> Result<String, String> {
    let context = Context::default();
    let tools = create_tools(
        &std::env::current_dir().map_err(|e| e.to_string())?,
        false,
        false,
    );
    let mut provider = ModelProvider {
        settings: ProviderSettings::default(),
        tool_definitions: tool_definitions(&tools),
    };
    let mut events = vec![];
    let messages = run_agent_loop(
        vec![Message::user("Windows desktop smoke")],
        &context,
        LoopConfig::default(),
        &mut provider,
        &tools,
        &mut NoopLoopHooks,
        &NoopToolHooks,
        &CancellationToken::new(),
        &mut |event| events.push(event),
    )
    .map_err(|e| e.to_string())?;
    if messages.len() != 2
        || !messages[1].text().contains("Windows desktop smoke")
        || !matches!(events.last(), Some(AgentEvent::AgentEnd { .. }))
    {
        return Err("Offline workflow failed".into());
    }
    let ctx = egui::Context::default();
    let mut app = DesktopApp::with_store(
        &ctx,
        SessionStore::new(
            std::env::temp_dir().join(format!("agent-core-layout-{}", Session::default().id)),
        ),
    );
    app.session = Session::default();
    app.sessions.clear();
    let output = ctx.run(
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1280.0, 840.0),
            )),
            ..Default::default()
        },
        |ctx| app.show(ctx),
    );
    if output.shapes.is_empty() {
        return Err("Desktop UI produced no shapes".into());
    }
    Ok(format!(
        "PASS: offline Rust loop, {} lifecycle events, native desktop layout",
        events.len()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::time::Instant;
    #[derive(Default)]
    struct ThinkingObserver {
        entered: std::sync::atomic::AtomicBool,
        trace: std::sync::Mutex<Vec<(ThinkingLevel, ThinkingLevel)>>,
        ui_thread: Option<std::thread::ThreadId>,
        hang: bool,
    }
    impl agent_runtime::extensions::Extension for ThinkingObserver {
        fn id(&self) -> &str {
            "thinking-observer"
        }
        fn on_event(
            &self,
            event: &LifecycleEvent,
            context: &agent_runtime::extensions::ExtensionContext,
            signal: &CancellationToken,
        ) -> Result<(), String> {
            if let LifecycleEvent::ThinkingLevelSelect {
                level,
                previous_level,
            } = event
            {
                if let Some(ui) = self.ui_thread {
                    assert_ne!(std::thread::current().id(), ui);
                }
                assert_eq!(*level, context.thinking_level);
                self.trace.lock().unwrap().push((*level, *previous_level));
                self.entered
                    .store(true, std::sync::atomic::Ordering::SeqCst);
                if self.hang {
                    while !signal.is_cancelled() {
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    return Err("Thinking notification aborted".into());
                }
            }
            Ok(())
        }
    }
    #[test]
    fn refreshed_shell_keeps_long_labels_and_composer_inside_small_windows() {
        let fixture = Fixture::new();
        for size in [egui::vec2(900.0, 640.0), egui::vec2(1280.0, 840.0)] {
            let ctx = egui::Context::default();
            let mut app = fixture_app(&ctx, &fixture);
            app.session.name = "a very long session name ".repeat(20);
            app.settings.model = "a-long-model-name-".repeat(30);
            app.settings.mode = ProviderMode::OpenAiCompatible;
            app.input = "long draft\n".repeat(100);
            for _ in 0..2 {
                let _ = ctx.run(
                    egui::RawInput {
                        screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
                        ..Default::default()
                    },
                    |ctx| app.show(ctx),
                );
            }
            let composer =
                egui::containers::panel::PanelState::load(&ctx, egui::Id::new("composer")).unwrap();
            assert!(composer.rect.height() < 240.0, "{:?}", composer.rect);
            assert!(composer.rect.max.y <= size.y);
            assert!(composer.rect.min.x >= shell::SIDEBAR_WIDTH);
            assert!(!app.settings_open);
            assert!(!app.allow_changes && !app.allow_commands);
            assert!(app.auth_worker.is_none());
        }
    }

    #[test]
    fn switching_theme_repaints_all_panels_without_changing_the_session_or_draft() {
        let fixture = Fixture::new();
        let ctx = egui::Context::default();
        let mut app = fixture_app(&ctx, &fixture);
        app.input = "keep this unsent draft".into();
        app.settings_open = true;
        let session = serde_json::to_value(&app.session).unwrap();
        for theme in [egui::Theme::Light, egui::Theme::Dark] {
            ctx.set_theme(theme);
            let palette = shell::Palette::for_theme(theme);
            for _ in 0..2 {
                let output = ctx.run(
                    egui::RawInput {
                        screen_rect: Some(egui::Rect::from_min_size(
                            egui::Pos2::ZERO,
                            egui::vec2(900.0, 640.0),
                        )),
                        ..Default::default()
                    },
                    |ctx| app.show(ctx),
                );
                let fills: Vec<_> = output
                    .shapes
                    .iter()
                    .filter_map(|shape| {
                        if let egui::Shape::Rect(rect) = &shape.shape {
                            Some(rect.fill)
                        } else {
                            None
                        }
                    })
                    .collect();
                for color in [palette.background, palette.sidebar, palette.surface] {
                    assert!(
                        fills.contains(&color),
                        "panel did not use {theme:?} palette: {color:?}"
                    );
                }
            }
            assert_eq!(ctx.theme(), theme);
            assert_eq!(app.input, "keep this unsent draft");
            assert_eq!(serde_json::to_value(&app.session).unwrap(), session);
            assert!(!app.allow_changes && !app.allow_commands);
            assert!(app.worker.is_none() && app.auth_worker.is_none());
        }
    }

    #[test]
    fn opening_settings_preserves_session_permissions_and_model_without_requests() {
        let fixture = Fixture::new();
        let ctx = egui::Context::default();
        let mut app = fixture_app(&ctx, &fixture);
        let session = serde_json::to_value(&app.session).unwrap();
        let mode = app.settings.mode.clone();
        let model = app.settings.model.clone();
        app.settings_open = true;
        for _ in 0..2 {
            let _ = ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(900.0, 640.0),
                    )),
                    ..Default::default()
                },
                |ctx| app.show(ctx),
            );
        }
        assert_eq!(serde_json::to_value(&app.session).unwrap(), session);
        assert_eq!(app.settings.mode, mode);
        assert_eq!(app.settings.model, model);
        assert!(!app.allow_changes && !app.allow_commands);
        assert!(!app.running);
        assert!(app.auth_worker.is_none());
        assert!(app.worker.is_none());
    }

    #[test]
    fn desktop_thinking_draft_only_commits_on_worker_apply_and_deduplicates() {
        let fixture = Fixture::new();
        let ctx = egui::Context::default();
        let mut app = fixture_app(&ctx, &fixture);
        let observer = std::sync::Arc::new(ThinkingObserver {
            ui_thread: Some(std::thread::current().id()),
            ..Default::default()
        });
        app.extensions.register(observer.clone()).unwrap();
        let original = serde_json::to_value(&app.session).unwrap();
        let mut draft = app.model_configuration();
        draft.settings.options.thinking_level = Some(ThinkingLevel::High);
        app.settings_draft = Some(draft.clone());
        assert_eq!(app.settings.options.thinking_level, None);
        assert_eq!(serde_json::to_value(&app.session).unwrap(), original);
        app.start_model_selection(&ctx, draft.clone(), ModelSelectSource::Set);
        poll_until(&mut app, &ctx, |app| !app.busy());
        assert_eq!(
            app.settings.options.thinking_level,
            Some(ThinkingLevel::High)
        );
        assert_eq!(
            app.session.get_thinking_level().unwrap(),
            Some(ThinkingLevel::High)
        );
        assert!(app.session.context.messages.is_empty());
        let count = app.session.get_entries().unwrap().len();
        app.start_model_selection(&ctx, draft, ModelSelectSource::Set);
        poll_until(&mut app, &ctx, |app| !app.busy());
        assert_eq!(app.session.get_entries().unwrap().len(), count);
        assert_eq!(observer.trace.lock().unwrap().len(), 1);
        assert_eq!(
            app.store.list().unwrap()[0].get_thinking_level().unwrap(),
            Some(ThinkingLevel::High)
        );
    }
    #[test]
    fn desktop_thinking_cancel_before_commit_preserves_session_and_draft() {
        let fixture = Fixture::new();
        let ctx = egui::Context::default();
        let mut app = fixture_app(&ctx, &fixture);
        let original = serde_json::to_value(&app.session).unwrap();
        let mut draft = app.model_configuration();
        draft.settings.options.thinking_level = Some(ThinkingLevel::High);
        app.settings_draft = Some(draft.clone());
        app.start_model_selection(&ctx, draft, ModelSelectSource::Set);
        app.signal.cancel();
        poll_until(&mut app, &ctx, |app| !app.busy());
        assert_eq!(serde_json::to_value(&app.session).unwrap(), original);
        assert_eq!(app.settings.options.thinking_level, None);
        assert_eq!(
            app.settings_draft
                .as_ref()
                .unwrap()
                .settings
                .options
                .thinking_level,
            Some(ThinkingLevel::High)
        );
        assert!(app.store.list().unwrap().is_empty());
    }
    #[test]
    fn desktop_thinking_cancel_after_commit_keeps_exact_saved_metadata() {
        let fixture = Fixture::new();
        let ctx = egui::Context::default();
        let mut app = fixture_app(&ctx, &fixture);
        let observer = std::sync::Arc::new(ThinkingObserver {
            ui_thread: Some(std::thread::current().id()),
            hang: true,
            ..Default::default()
        });
        app.extensions.register(observer.clone()).unwrap();
        let mut draft = app.model_configuration();
        draft.settings.options.thinking_level = Some(ThinkingLevel::High);
        app.start_model_selection(&ctx, draft, ModelSelectSource::Set);
        poll_until(&mut app, &ctx, |_| {
            observer.entered.load(std::sync::atomic::Ordering::SeqCst)
        });
        let start = Instant::now();
        app.signal.cancel();
        poll_until(&mut app, &ctx, |app| !app.busy());
        assert!(start.elapsed() < Duration::from_secs(2));
        assert_eq!(
            app.settings.options.thinking_level,
            Some(ThinkingLevel::High)
        );
        assert_eq!(
            app.store.list().unwrap()[0].get_thinking_level().unwrap(),
            Some(ThinkingLevel::High)
        );
        assert!(app.session.context.messages.is_empty());
    }
    #[test]
    fn desktop_branch_restore_uses_branch_level_even_without_a_model_identity_entry() {
        let fixture = Fixture::new();
        let ctx = egui::Context::default();
        let mut app = fixture_app(&ctx, &fixture);
        app.settings.options.thinking_level = Some(ThinkingLevel::High);
        app.session
            .append_thinking_level(ThinkingLevel::High)
            .unwrap();
        let branch = app.session.append_message(Message::user("A")).unwrap();
        app.session
            .append_thinking_level(ThinkingLevel::Low)
            .unwrap();
        app.session.append_message(Message::user("B")).unwrap();
        app.settings.options.thinking_level = Some(ThinkingLevel::Low);
        app.navigate_tree(Some(&branch)).unwrap();
        app.start_model_restore(&ctx);
        poll_until(&mut app, &ctx, |app| !app.busy());
        assert_eq!(
            app.settings.options.thinking_level,
            Some(ThinkingLevel::High)
        );
        assert_eq!(
            app.session.get_thinking_level().unwrap(),
            Some(ThinkingLevel::High)
        );
        assert_eq!(app.session.context.messages.len(), 1);
        assert_eq!(app.session.context.messages[0].text(), "A");
    }
    #[derive(Default)]
    struct SessionObserver {
        trace: std::sync::Mutex<Vec<(String, agent_runtime::extensions::ExtensionContext)>>,
        entered: std::sync::atomic::AtomicBool,
        ui_thread: Option<std::thread::ThreadId>,
        veto: bool,
        error: bool,
        hang_info: bool,
    }
    impl agent_runtime::extensions::Extension for SessionObserver {
        fn id(&self) -> &str {
            "session-observer"
        }
        fn before_session_change(
            &self,
            event: &SessionBeforeEvent,
            context: &agent_runtime::extensions::ExtensionContext,
            _: &CancellationToken,
        ) -> Result<agent_runtime::extensions::SessionBeforeResult, String> {
            if let Some(ui) = self.ui_thread {
                assert_ne!(std::thread::current().id(), ui);
            }
            self.trace
                .lock()
                .unwrap()
                .push((event.name().into(), context.clone()));
            if self.error {
                return Err("Ordinary session callback error".into());
            }
            Ok(agent_runtime::extensions::SessionBeforeResult { cancel: self.veto })
        }
        fn on_event(
            &self,
            event: &LifecycleEvent,
            context: &agent_runtime::extensions::ExtensionContext,
            signal: &CancellationToken,
        ) -> Result<(), String> {
            let name = match event {
                LifecycleEvent::SessionInfoChanged { name } => {
                    assert_eq!(&context.session_name, name);
                    "session_info_changed"
                }
                LifecycleEvent::SessionStart { .. } => "session_start",
                LifecycleEvent::SessionShutdown { .. } => "session_shutdown",
                _ => return Ok(()),
            };
            if let Some(ui) = self.ui_thread {
                assert_ne!(std::thread::current().id(), ui);
            }
            self.trace
                .lock()
                .unwrap()
                .push((name.into(), context.clone()));
            if name == "session_info_changed" {
                self.entered
                    .store(true, std::sync::atomic::Ordering::SeqCst);
                if self.hang_info {
                    while !signal.is_cancelled() {
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    return Err("Name callback aborted".into());
                }
                if self.error {
                    return Err("Ordinary name callback error".into());
                }
            }
            Ok(())
        }
    }
    #[test]
    fn desktop_session_info_normalizes_clear_persists_and_notifies_off_ui() {
        let fixture = Fixture::new();
        let ctx = egui::Context::default();
        let mut app = fixture_app(&ctx, &fixture);
        let observer = std::sync::Arc::new(SessionObserver {
            ui_thread: Some(std::thread::current().id()),
            ..Default::default()
        });
        app.extensions.register(observer.clone()).unwrap();
        for name in ["  one\r\n\ntwo  ", " \r\n "] {
            app.start_extension_action(&ctx, ExtensionAction::Name(name.into()));
            poll_until(&mut app, &ctx, |app| !app.running);
        }
        let trace = observer.trace.lock().unwrap();
        assert_eq!(trace.len(), 2);
        assert_eq!(trace[0].1.session_name.as_deref(), Some("one two"));
        assert_eq!(trace[1].1.session_name, None);
        assert!(app.session.context.messages.is_empty());
        assert_eq!(app.session.get_session_name().unwrap(), None);
        assert_eq!(
            app.store.list().unwrap()[0].get_session_name().unwrap(),
            None
        );
        assert_eq!(
            app.session
                .get_entries()
                .unwrap()
                .iter()
                .filter(|e| e.entry_type == "session_info")
                .count(),
            2
        );
    }
    #[test]
    fn desktop_new_resume_and_fork_veto_preserve_old_session_disk_and_extensions() {
        let fixture = Fixture::new();
        let ctx = egui::Context::default();
        let mut app = fixture_app(&ctx, &fixture);
        let target = app
            .session
            .append_message(Message::user("edit me"))
            .unwrap();
        app.store.save(&app.session).unwrap();
        let path = app.store.path(&app.session.id).unwrap();
        let disk = std::fs::read(&path).unwrap();
        let original = serde_json::to_value(&app.session).unwrap();
        let observer = std::sync::Arc::new(SessionObserver {
            veto: true,
            ui_thread: Some(std::thread::current().id()),
            ..Default::default()
        });
        app.extensions.register(observer.clone()).unwrap();
        let resumed = Session {
            workspace: fixture.0.clone(),
            ..Default::default()
        };
        app.store.save(&resumed).unwrap();
        for action in [
            ExtensionAction::Replace(
                Box::new(Session {
                    workspace: fixture.0.clone(),
                    ..Default::default()
                }),
                SessionStartReason::New,
            ),
            ExtensionAction::Replace(Box::new(resumed), SessionStartReason::Resume),
            ExtensionAction::Fork(target),
        ] {
            app.start_extension_action(&ctx, action);
            poll_until(&mut app, &ctx, |app| !app.running);
            assert_eq!(serde_json::to_value(&app.session).unwrap(), original);
            assert_eq!(std::fs::read(&path).unwrap(), disk);
            assert_eq!(app.extensions.ids(), vec!["session-observer"]);
        }
        let trace = observer.trace.lock().unwrap();
        assert_eq!(trace.len(), 3);
        assert!(trace
            .iter()
            .all(|(kind, _)| kind.starts_with("session_before")));
        assert_eq!(app.store.list().unwrap().len(), 2);
    }
    #[test]
    fn desktop_name_cancel_after_commit_preserves_current_ui_and_disk() {
        let fixture = Fixture::new();
        let ctx = egui::Context::default();
        let mut app = fixture_app(&ctx, &fixture);
        let observer = std::sync::Arc::new(SessionObserver {
            hang_info: true,
            ui_thread: Some(std::thread::current().id()),
            ..Default::default()
        });
        app.extensions.register(observer.clone()).unwrap();
        app.start_extension_action(&ctx, ExtensionAction::Name("committed".into()));
        poll_until(&mut app, &ctx, |_| {
            observer.entered.load(std::sync::atomic::Ordering::SeqCst)
        });
        assert!(app.running);
        assert_eq!(
            app.session.get_session_name().unwrap().as_deref(),
            Some("committed")
        );
        let start = Instant::now();
        app.signal.cancel();
        poll_until(&mut app, &ctx, |app| !app.running);
        assert!(start.elapsed() < Duration::from_secs(2));
        assert_eq!(
            app.store.list().unwrap()[0]
                .get_session_name()
                .unwrap()
                .as_deref(),
            Some("committed")
        );
        assert!(app.session.context.messages.is_empty());
        assert!(app.status.contains("已保存"));
    }
    #[test]
    fn desktop_name_save_failure_does_not_commit_or_publish_info() {
        let fixture = Fixture::new();
        let ctx = egui::Context::default();
        let mut app = fixture_app(&ctx, &fixture);
        let observer = std::sync::Arc::new(SessionObserver::default());
        app.extensions.register(observer.clone()).unwrap();
        let original = serde_json::to_value(&app.session).unwrap();
        let bad = fixture.0.join("not-directory");
        std::fs::write(&bad, b"fixture").unwrap();
        app.store = SessionStore::new(bad);
        app.start_extension_action(&ctx, ExtensionAction::Name("not committed".into()));
        poll_until(&mut app, &ctx, |app| !app.running);
        assert_eq!(serde_json::to_value(&app.session).unwrap(), original);
        assert!(observer.trace.lock().unwrap().is_empty());
    }
    #[test]
    fn desktop_real_native_before_switch_hang_is_cancellable_without_shutdown_or_commit() {
        let fixture = Fixture::new();
        let ctx = egui::Context::default();
        let mut app = fixture_app(&ctx, &fixture);
        let record = fixture.0.join("requests.jsonl");
        let manifest = native_process_manifest(
            &fixture,
            vec![
                "--hang-before-switch".into(),
                format!("--record={}", record.display()),
            ],
        );
        app.start_extension_action(&ctx, ExtensionAction::Load(vec![manifest]));
        poll_until(&mut app, &ctx, |app| !app.busy());
        let before = serde_json::to_value(&app.session).unwrap();
        let path = app.store.path(&app.session.id).unwrap();
        let disk = std::fs::read(&path).unwrap();
        app.start_extension_action(
            &ctx,
            ExtensionAction::Replace(
                Box::new(Session {
                    workspace: fixture.0.clone(),
                    ..Default::default()
                }),
                SessionStartReason::New,
            ),
        );
        poll_until(&mut app, &ctx, |_| {
            std::fs::read_to_string(&record)
                .unwrap_or_default()
                .lines()
                .any(|line| {
                    serde_json::from_str::<serde_json::Value>(line)
                        .is_ok_and(|v| v["method"] == "session_before")
                })
        });
        let start = Instant::now();
        app.signal.cancel();
        poll_until(&mut app, &ctx, |app| !app.running);
        assert!(start.elapsed() < Duration::from_secs(2));
        assert_eq!(serde_json::to_value(&app.session).unwrap(), before);
        assert_eq!(std::fs::read(&path).unwrap(), disk);
        let requests = std::fs::read_to_string(record).unwrap();
        assert!(!requests
            .lines()
            .any(|line| serde_json::from_str::<serde_json::Value>(line)
                .is_ok_and(|v| v["params"]["event"]["type"] == "session_shutdown")));
    }
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("desktop-fixture-{}", Session::default().id));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).unwrap();
        }
    }
    fn fixture_app(ctx: &egui::Context, fixture: &Fixture) -> DesktopApp {
        let mut app = DesktopApp::with_store(ctx, SessionStore::new(fixture.0.join("sessions")));
        app.auth_path = fixture.0.join("auth").join("credentials.dat");
        app.settings.options.auth_storage_path = Some(app.auth_path.clone());
        app.workspace = fixture.0.to_string_lossy().into_owned();
        app.session.workspace = fixture.0.clone();
        app
    }
    fn poll_until(app: &mut DesktopApp, ctx: &egui::Context, ready: impl Fn(&DesktopApp) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while !ready(app) {
            app.poll(ctx);
            assert!(
                Instant::now() < deadline,
                "desktop worker did not settle: {} / {}",
                app.status,
                app.auth_status
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    fn mock_credential() -> OAuthCredential {
        let claims = URL_SAFE_NO_PAD
            .encode(br#"{"https://api.openai.com/auth":{"chatgpt_account_id":"fixture-account"}}"#);
        OAuthCredential {
            credential_type: "oauth".into(),
            access: format!("header.{claims}.fixture-secret"),
            refresh: "fixture-refresh-secret".into(),
            expires: timestamp() + 60_000,
            account_id: "fixture-account".into(),
            extra: BTreeMap::new(),
        }
    }
    #[test]
    fn offline_desktop_workflow_and_layout() {
        assert!(smoke_test().unwrap().starts_with("PASS:"));
    }

    #[test]
    fn long_input_keeps_the_composer_bounded_at_minimum_window_size() {
        let ctx = egui::Context::default();
        let mut app = DesktopApp::with_store(
            &ctx,
            SessionStore::new(
                std::env::temp_dir().join(format!("agent-core-layout-{}", Session::default().id)),
            ),
        );
        app.settings.mode = ProviderMode::OpenAiCompatible;
        app.input = "a long line of offline fixture text\n".repeat(50);
        for _ in 0..2 {
            let _ = ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(900.0, 640.0),
                    )),
                    ..Default::default()
                },
                |ctx| app.show(ctx),
            );
        }
        let panel =
            egui::containers::panel::PanelState::load(&ctx, egui::Id::new("composer")).unwrap();
        assert!(
            panel.rect.height() < 240.0,
            "composer height: {}",
            panel.rect.height()
        );
        assert!(panel.rect.max.y <= 640.0, "composer rect: {:?}", panel.rect);
        let sidebar =
            egui::containers::panel::PanelState::load(&ctx, egui::Id::new("sidebar")).unwrap();
        assert!(
            sidebar.rect.max.y <= 640.0,
            "sidebar rect: {:?}",
            sidebar.rect
        );
    }

    #[test]
    fn keybindings_can_override_the_default_send_shortcut() {
        let ctx = egui::Context::default();
        let mut app = DesktopApp::with_store(
            &ctx,
            SessionStore::new(
                std::env::temp_dir().join(format!("agent-core-keys-{}", Session::default().id)),
            ),
        );
        assert_eq!(app.keybindings.send, DEFAULT_APP_KEYBINDINGS.send);
        app.keybindings.send =
            egui::KeyboardShortcut::new(egui::Modifiers::SHIFT, egui::Key::Enter);
        assert_ne!(app.keybindings.send, DEFAULT_APP_KEYBINDINGS.send);
    }

    #[test]
    fn disconnected_worker_does_not_leave_the_window_permanently_running() {
        let ctx = egui::Context::default();
        let dir = std::env::temp_dir().join(format!(
            "agent-desktop-disconnect-{}",
            Session::default().id
        ));
        let mut app = DesktopApp::with_store(&ctx, SessionStore::new(&dir));
        let (sender, receiver) = mpsc::sync_channel(64);
        drop(sender);
        app.receiver = Some(receiver);
        app.running = true;
        app.closing = true;
        app.poll(&ctx);
        assert!(!app.running);
        assert!(app.receiver.is_none());
        assert!(app.signal.is_cancelled());
        assert!(app.status.contains("意外结束"));
        std::fs::remove_file(dir.join(format!("{}.json", app.session.id))).unwrap();
        std::fs::remove_dir(dir).unwrap();
    }

    #[test]
    fn image_preview_decodes_once_and_reports_invalid_data_without_panicking() {
        let mut encoded = Cursor::new(vec![]);
        image::RgbaImage::from_pixel(2, 1, image::Rgba([100, 120, 140, 255]))
            .write_to(&mut encoded, image::ImageFormat::Png)
            .unwrap();
        let content = vec![
            Content::Image {
                data: STANDARD.encode(encoded.into_inner()),
                mime_type: "image/png".into(),
            },
            Content::Image {
                data: "invalid base64!!!".into(),
                mime_type: "image/png".into(),
            },
        ];
        let ctx = egui::Context::default();
        let mut cache = BTreeMap::new();
        for _ in 0..2 {
            let _ = ctx.run(egui::RawInput::default(), |ctx| {
                egui::CentralPanel::default()
                    .show(ctx, |ui| show_images(ui, &mut cache, "fixture", &content));
            });
        }
        assert_eq!(cache.len(), 2);
        assert!(cache
            .values()
            .any(|result| result.as_ref().is_ok_and(|image| image.size() == [2, 1])));
        assert!(cache.values().any(Result::is_err));
    }

    #[test]
    fn windows_light_theme_does_not_override_the_dark_application_style() {
        let ctx = egui::Context::default();
        let mut app = DesktopApp::with_store(
            &ctx,
            SessionStore::new(
                std::env::temp_dir().join(format!("agent-core-theme-{}", Session::default().id)),
            ),
        );
        let _ = ctx.run(
            egui::RawInput {
                system_theme: Some(egui::Theme::Light),
                ..Default::default()
            },
            |ctx| app.show(ctx),
        );
        assert!(ctx.style().visuals.dark_mode);
    }

    #[test]
    fn desktop_tree_navigation_fork_clone_export_preserve_branches_and_block_busy_changes() {
        let fixture = Fixture::new();
        let ctx = egui::Context::default();
        let mut app = fixture_app(&ctx, &fixture);
        let root = app.session.append_message(Message::user("root")).unwrap();
        let answer = app
            .session
            .append_message(Message::Assistant(AssistantMessage::text("answer")))
            .unwrap();
        let old = app
            .session
            .append_message(Message::user("old question"))
            .unwrap();
        app.fork_tree(&old).unwrap();
        assert_eq!(app.input, "old question");
        assert_eq!(app.session.leaf_id, Some(answer));
        let edited = app.session.append_message(Message::user("edited")).unwrap();
        let original = serde_json::to_value(&app.session).unwrap();
        app.running = true;
        assert!(app.navigate_tree(Some(&root)).is_err());
        assert!(app.fork_tree(&edited).is_err());
        assert!(app.clone_tree().is_err());
        assert_eq!(serde_json::to_value(&app.session).unwrap(), original);
        app.running = false;
        app.clone_tree().unwrap();
        assert_eq!(
            app.session.context.messages.last().unwrap().text(),
            "edited"
        );
        assert_eq!(app.session.entries.len(), 3);
        let path = fixture.0.join("full.html");
        SessionStore::export_html(&app.session, &path).unwrap();
        assert!(std::fs::read_to_string(path).unwrap().contains("edited"));
        assert_eq!(app.store.list().unwrap().len(), 2);
    }

    #[test]
    fn oauth_status_and_logout_are_async_and_never_expose_tokens_in_session_or_ui_status() {
        let fixture = Fixture::new();
        let ctx = egui::Context::default();
        let mut app = fixture_app(&ctx, &fixture);
        let credential = mock_credential();
        let storage = AuthStorage::new(&app.auth_path);
        storage
            .write(
                CODEX_PROVIDER,
                serde_json::to_value(&credential).unwrap(),
                &CancellationToken::new(),
            )
            .unwrap();
        app.start_auth(&ctx, AuthAction::Status);
        assert!(app.busy());
        poll_until(&mut app, &ctx, |app| app.auth_receiver.is_none());
        assert!(app.settings.options.oauth.is_some());
        assert!(app.auth_status.contains("已登录"));
        assert!(!app.auth_status.contains("fixture-secret"));
        assert!(!serde_json::to_string(&app.session)
            .unwrap()
            .contains("fixture-refresh-secret"));
        app.start_auth(&ctx, AuthAction::Logout);
        poll_until(&mut app, &ctx, |app| app.auth_receiver.is_none());
        assert!(app.settings.options.oauth.is_none());
        assert!(storage
            .read(CODEX_PROVIDER, &CancellationToken::new())
            .unwrap()
            .is_none());
        assert!(app.auth_worker.is_none());
    }

    #[test]
    fn cancelling_oauth_releases_native_callback_port_and_joins_worker() {
        let fixture = Fixture::new();
        let ctx = egui::Context::default();
        let mut app = fixture_app(&ctx, &fixture);
        let reserved = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = reserved.local_addr().unwrap();
        drop(reserved);
        app.settings.options.codex_oauth = agent_runtime::auth::CodexOAuthConfig {
            authorize_url: "http://127.0.0.1:1/authorize".into(),
            token_url: "http://127.0.0.1:1/token".into(),
            callback_bind: address,
            redirect_uri: format!("http://127.0.0.1:{}/auth/callback", address.port()),
            ..Default::default()
        };
        app.start_auth(&ctx, AuthAction::Login);
        poll_until(&mut app, &ctx, |app| app.login_url.is_some());
        assert!(app.busy());
        let start = Instant::now();
        app.auth_signal.cancel();
        poll_until(&mut app, &ctx, |app| app.auth_receiver.is_none());
        assert!(start.elapsed() < Duration::from_secs(2));
        assert!(app.auth_worker.is_none());
        assert!(app.login_url.is_none());
        let rebound = TcpListener::bind(address).unwrap();
        drop(rebound);
        assert!(app.auth_status.contains("取消"));
    }

    #[test]
    fn window_exit_cancels_oauth_before_closing_and_does_not_leave_a_callback_or_worker() {
        let fixture = Fixture::new();
        let ctx = egui::Context::default();
        let mut app = fixture_app(&ctx, &fixture);
        let reserved = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = reserved.local_addr().unwrap();
        drop(reserved);
        app.settings.options.codex_oauth = agent_runtime::auth::CodexOAuthConfig {
            authorize_url: "http://127.0.0.1:1/authorize".into(),
            token_url: "http://127.0.0.1:1/token".into(),
            callback_bind: address,
            redirect_uri: format!("http://127.0.0.1:{}/auth/callback", address.port()),
            ..Default::default()
        };
        app.start_auth(&ctx, AuthAction::Login);
        poll_until(&mut app, &ctx, |app| app.login_url.is_some());
        let start = Instant::now();
        app.stop_workers();
        assert!(start.elapsed() < Duration::from_secs(2));
        assert!(app.auth_worker.is_none());
        assert!(app.auth_receiver.is_none());
        assert!(TcpListener::bind(address).is_ok());
    }

    #[test]
    fn detached_ui_receiver_still_commits_completed_worker_session() {
        let fixture = Fixture::new();
        let ctx = egui::Context::default();
        let mut app = fixture_app(&ctx, &fixture);
        app.input = "offline worker persistence".into();
        app.start(&ctx);
        assert!(app.running);
        app.receiver = None;
        let worker = app.worker.take().unwrap();
        worker.join().unwrap();
        app.running = false;
        let persisted = app.store.list().unwrap().pop().unwrap();
        assert_eq!(persisted.context.messages.len(), 2);
        assert!(persisted.context.messages[1]
            .text()
            .contains("offline worker persistence"));
        assert_eq!(persisted.entries.len(), 2);
    }

    #[test]
    fn automation_uses_catalog_costs_and_unknown_model_requires_explicit_window() {
        let fixture = Fixture::new();
        let ctx = egui::Context::default();
        let mut app = fixture_app(&ctx, &fixture);
        let model = search("")
            .into_iter()
            .find(|model| model.mode().is_some() && model.context_window > 20_000)
            .unwrap();
        app.settings.options.provider_id = model.provider.clone();
        app.settings.model = model.id.clone();
        app.auto_compaction = true;
        app.retry_enabled = true;
        let options = app.run_options().unwrap();
        let budget = options.automation.model.unwrap();
        assert_eq!(budget.cost, model.cost);
        assert_eq!(budget.context_window, model.context_window);
        assert_eq!(budget.max_output_tokens, model.max_tokens);
        assert!(options.automation.retry.enabled);
        app.settings.model = "unknown-fixture-model".into();
        app.update_model_budget();
        assert!(app.run_options().is_err());
        app.context_window = 100_000;
        app.max_output_tokens = 4_000;
        assert_eq!(
            app.run_options().unwrap().automation.model.unwrap().cost,
            serde_json::Value::Null
        );
    }

    #[test]
    fn headless_codex_automation_and_tree_panels_keep_original_composer_layout() {
        let fixture = Fixture::new();
        let ctx = egui::Context::default();
        let mut app = fixture_app(&ctx, &fixture);
        app.settings.mode = ProviderMode::OpenAiCodex;
        app.settings.options.provider_id = CODEX_PROVIDER.into();
        app.auto_compaction = true;
        app.retry_enabled = true;
        for _ in 0..2 {
            let output = ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(900.0, 640.0),
                    )),
                    ..Default::default()
                },
                |ctx| app.show(ctx),
            );
            assert!(!output.shapes.is_empty());
        }
        let panel =
            egui::containers::panel::PanelState::load(&ctx, egui::Id::new("composer")).unwrap();
        assert!(panel.rect.height() < 240.0);
        assert!(app.auth_worker.is_none());
    }

    #[test]
    fn oauth_login_exchanges_fixture_callback_and_encrypts_credentials_before_finishing() {
        let fixture = Fixture::new();
        let ctx = egui::Context::default();
        let mut app = fixture_app(&ctx, &fixture);
        let token_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let token_addr = token_listener.local_addr().unwrap();
        token_listener.set_nonblocking(true).unwrap();
        let credential = mock_credential();
        let access = credential.access.clone();
        let token_worker = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(3);
            let mut stream = loop {
                match token_listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline);
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("token accept: {error}"),
                }
            };
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_millis(100)))
                .unwrap();
            let mut bytes = Vec::new();
            let mut buffer = [0u8; 4096];
            loop {
                assert!(Instant::now() < deadline);
                match stream.read(&mut buffer) {
                    Ok(0) => panic!("token request EOF"),
                    Ok(length) => bytes.extend_from_slice(&buffer[..length]),
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::WouldBlock
                                | std::io::ErrorKind::TimedOut
                                | std::io::ErrorKind::Interrupted
                        ) =>
                    {
                        continue
                    }
                    Err(error) => panic!("token read: {error}"),
                }
                if let Some(header_end) = bytes.windows(4).position(|window| window == b"\r\n\r\n")
                {
                    let header = String::from_utf8_lossy(&bytes[..header_end]);
                    let length = header
                        .lines()
                        .find_map(|line| {
                            line.split_once(':')
                                .filter(|(key, _)| key.eq_ignore_ascii_case("content-length"))
                                .map(|(_, value)| value.trim().parse::<usize>().unwrap())
                        })
                        .unwrap();
                    if bytes.len() >= header_end + 4 + length {
                        break;
                    }
                }
            }
            let request = String::from_utf8(bytes).unwrap();
            assert!(request.contains("grant_type=authorization_code"));
            assert!(request.contains("code_verifier="));
            assert!(request.contains("code=fixture-code"));
            let body=serde_json::json!({"access_token":access,"refresh_token":"fixture-refresh-secret","expires_in":3600}).to_string();
            write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();
        });
        let reserved = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = reserved.local_addr().unwrap();
        drop(reserved);
        app.settings.options.codex_oauth = agent_runtime::auth::CodexOAuthConfig {
            authorize_url: format!("http://{token_addr}/authorize"),
            token_url: format!("http://{token_addr}/token"),
            callback_bind: address,
            redirect_uri: format!("http://{address}/auth/callback"),
            ..Default::default()
        };
        app.start_auth(&ctx, AuthAction::Login);
        poll_until(&mut app, &ctx, |app| app.login_url.is_some());
        let state = app
            .login_url
            .as_ref()
            .unwrap()
            .split('&')
            .find_map(|part| part.strip_prefix("state="))
            .unwrap();
        let mut callback = TcpStream::connect(address).unwrap();
        callback
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        write!(callback,"GET /auth/callback?state={state}&code=fixture-code HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n").unwrap();
        let mut response = String::new();
        callback.read_to_string(&mut response).unwrap();
        assert!(response.starts_with("HTTP/1.1 200"));
        drop(callback);
        poll_until(&mut app, &ctx, |app| app.auth_receiver.is_none());
        token_worker.join().unwrap();
        assert!(app.auth_status.contains("已登录"));
        assert!(app.settings.options.oauth.is_some());
        let encrypted = std::fs::read(&app.auth_path).unwrap();
        assert!(!String::from_utf8_lossy(&encrypted).contains("fixture-refresh-secret"));
        assert!(AuthStorage::new(&app.auth_path)
            .read(CODEX_PROVIDER, &CancellationToken::new())
            .unwrap()
            .is_some());
        assert!(TcpListener::bind(address).is_ok());
    }

    #[test]
    fn automatic_runtime_events_are_polled_and_saved_in_the_selected_branch() {
        let fixture = Fixture::new();
        let ctx = egui::Context::default();
        let mut app = fixture_app(&ctx, &fixture);
        app.auto_compaction = true;
        app.settings.model = "offline-fixture".into();
        app.context_window = 100_000;
        app.max_output_tokens = 4_000;
        app.input = "offline budget fixture".into();
        app.start(&ctx);
        assert!(app.running, "{}", app.status);
        poll_until(&mut app, &ctx, |app| !app.running);
        app.poll(&ctx);
        assert!(!app.runtime_activity.is_empty());
        assert_eq!(app.automation.state().request_count, 1);
        assert!(app.session.runtime_state("automation").unwrap().is_some());
        let stored = app.store.list().unwrap().pop().unwrap();
        assert!(stored.runtime_state("automation").unwrap().is_some());
    }

    struct SummaryFixture {
        calls: usize,
    }
    impl Provider for SummaryFixture {
        fn complete(
            &mut self,
            _: &Context,
            _: &CancellationToken,
            _: &mut dyn FnMut(AssistantMessage),
        ) -> Result<AssistantMessage, String> {
            self.calls += 1;
            Ok(AssistantMessage {
                usage: Some(agent_core::types::Usage {
                    input: 100,
                    output: 8,
                    total_tokens: 108,
                    ..Default::default()
                }),
                ..AssistantMessage::text("offline summary")
            })
        }
    }
    fn manual_fixture() -> Session {
        let mut session = Session::default();
        for index in 0..4 {
            session
                .append_message(Message::user(format!(
                    "old user {index} {}",
                    "fixture ".repeat(30)
                )))
                .unwrap();
            session
                .append_message(Message::Assistant(AssistantMessage::text(format!(
                    "old assistant {index}"
                ))))
                .unwrap();
        }
        session
    }
    #[test]
    fn manual_desktop_compaction_records_usage_debt_and_branch_state_without_an_extra_request() {
        let mut session = manual_fixture();
        let handle = AutomationHandle::default();
        let mut provider = SummaryFixture { calls: 0 };
        compact_with_ledger(
            &mut session,
            &mut provider,
            None,
            &handle,
            RetryOptions::default(),
            &CancellationToken::new(),
        )
        .unwrap();
        assert_eq!(provider.calls, 1);
        assert_eq!(session.compactions.len(), 1);
        assert_eq!(
            session.compactions[0].usage.as_ref().unwrap().total_tokens,
            108
        );
        let state = session.runtime_state("automation").unwrap().unwrap();
        assert_eq!(state["nativeCompactionCount"], 1);
        assert_eq!(state["cacheDebtUnpriced"], true);
        assert_eq!(handle.state().request_count, 0);
    }
    #[test]
    fn failed_manual_desktop_ledger_rolls_back_session_after_only_one_summary_request() {
        let mut session = manual_fixture();
        let original = serde_json::to_value(&session).unwrap();
        let handle = AutomationHandle::default();
        let mut provider = SummaryFixture { calls: 0 };
        let invalid = ModelBudget {
            provider: "".into(),
            id: "fixture".into(),
            context_window: 100_000,
            max_output_tokens: 4_000,
            cost: serde_json::Value::Null,
        };
        assert!(compact_with_ledger(
            &mut session,
            &mut provider,
            Some(&invalid),
            &handle,
            RetryOptions::default(),
            &CancellationToken::new()
        )
        .is_err());
        assert_eq!(provider.calls, 1);
        assert_eq!(serde_json::to_value(session).unwrap(), original);
        assert_eq!(handle.snapshot().compactions, 0);
    }

    struct DesktopExtensionFixture {
        ui_thread: std::thread::ThreadId,
        trace: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        slow: bool,
        invalid: bool,
    }
    impl agent_runtime::extensions::ExtensionCommand for DesktopExtensionFixture {
        fn name(&self) -> &str {
            "hello"
        }
        fn execute(
            &self,
            args: &str,
            context: &agent_runtime::extensions::ExtensionContext,
            signal: &CancellationToken,
        ) -> Result<CommandResult, String> {
            assert_ne!(
                std::thread::current().id(),
                self.ui_thread,
                "command ran on UI thread"
            );
            if self.slow {
                for _ in 0..100 {
                    if signal.is_cancelled() {
                        return Err("cancelled fixture".into());
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
            let count = context
                .states
                .get("desktop-ext")
                .and_then(|state| state["count"].as_u64())
                .unwrap_or(0);
            Ok(CommandResult {
                text: format!("native hello {args} #{}", count + 1),
                session_name: Some("  extension session  ".into()),
                state: serde_json::json!({"count":count+1}),
                messages: if self.invalid {
                    vec![Message::Assistant(AssistantMessage::text("forged"))]
                } else {
                    vec![]
                },
                ui: vec![
                    UiRequest::Notification {
                        level: NotificationLevel::Info,
                        text: "native notification".into(),
                    },
                    UiRequest::SetStatus {
                        key: "extension-count".into(),
                        text: Some((count + 1).to_string()),
                    },
                ],
            })
        }
    }
    impl agent_runtime::extensions::Extension for DesktopExtensionFixture {
        fn id(&self) -> &str {
            "desktop-ext"
        }
        fn commands(&self) -> Vec<std::sync::Arc<dyn agent_runtime::extensions::ExtensionCommand>> {
            vec![std::sync::Arc::new(Self {
                ui_thread: self.ui_thread,
                trace: self.trace.clone(),
                slow: self.slow,
                invalid: self.invalid,
            })]
        }
        fn before_agent_start(
            &self,
            _event: &agent_runtime::extensions::BeforeAgentStartEvent,
            _context: &agent_runtime::extensions::ExtensionContext,
            _signal: &CancellationToken,
        ) -> Result<agent_runtime::extensions::BeforeAgentStartResult, String> {
            Ok(agent_runtime::extensions::BeforeAgentStartResult {
                messages: vec![agent_runtime::extensions::custom_message(
                    "native-note",
                    agent_runtime::extensions::CustomMessageContent::Text("fixture context".into()),
                    true,
                    serde_json::Value::Null,
                )?],
                system_prompt: Some("temporary native system prompt".into()),
            })
        }
        fn on_event(
            &self,
            event: &LifecycleEvent,
            context: &agent_runtime::extensions::ExtensionContext,
            _signal: &CancellationToken,
        ) -> Result<(), String> {
            assert_ne!(
                std::thread::current().id(),
                self.ui_thread,
                "lifecycle ran on UI thread"
            );
            let phase = match event {
                LifecycleEvent::SessionStart { reason, .. } => format!("start:{reason:?}"),
                LifecycleEvent::SessionShutdown { reason, .. } => format!("shutdown:{reason:?}"),
                LifecycleEvent::AgentSettled => "settled".into(),
                LifecycleEvent::Agent {
                    event: AgentEvent::AgentStart,
                } => "agent_start".into(),
                LifecycleEvent::Agent {
                    event: AgentEvent::AgentEnd { .. },
                } => "agent_end".into(),
                _ => "event".into(),
            };
            self.trace.lock().unwrap().push(format!(
                "{phase}:{}",
                context.session_id.as_deref().unwrap_or_default()
            ));
            Ok(())
        }
    }
    fn desktop_extension(
        slow: bool,
        invalid: bool,
    ) -> (
        SharedRegistry,
        std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    ) {
        let trace = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let registry = SharedRegistry::default();
        registry
            .register(std::sync::Arc::new(DesktopExtensionFixture {
                ui_thread: std::thread::current().id(),
                trace: trace.clone(),
                slow,
                invalid,
            }))
            .unwrap();
        (registry, trace)
    }

    #[test]
    fn native_extension_command_is_async_view_only_and_state_survives_reopen() {
        let fixture = Fixture::new();
        let ctx = egui::Context::default();
        let mut app = fixture_app(&ctx, &fixture);
        app.extensions = desktop_extension(false, false).0;
        app.input = "/hello Ada".into();
        app.start(&ctx);
        assert!(app.running && app.extension_operation);
        poll_until(&mut app, &ctx, |app| !app.running);
        assert!(app.worker.is_none());
        assert!(app.session.context.messages.is_empty());
        assert_eq!(app.session.name, "extension session");
        assert_eq!(
            app.session
                .runtime_state("extension:desktop-ext")
                .unwrap()
                .unwrap()["count"],
            1
        );
        assert!(app
            .extension_display
            .iter()
            .any(|text| text.contains("native hello Ada #1")));
        assert!(app
            .extension_display
            .iter()
            .any(|text| text.contains("native notification")));
        assert_eq!(app.extension_statuses["extension-count"], "1");
        let mut reopened = DesktopApp::with_store(&ctx, app.store.clone());
        reopened.extensions = desktop_extension(false, false).0;
        reopened.input = "/hello again".into();
        reopened.start(&ctx);
        poll_until(&mut reopened, &ctx, |app| !app.running);
        assert_eq!(
            reopened
                .session
                .runtime_state("extension:desktop-ext")
                .unwrap()
                .unwrap()["count"],
            2
        );
        assert!(reopened.session.context.messages.is_empty());
    }

    #[test]
    fn invalid_native_command_does_not_commit_session_messages_or_state() {
        let fixture = Fixture::new();
        let ctx = egui::Context::default();
        let mut app = fixture_app(&ctx, &fixture);
        app.extensions = desktop_extension(false, true).0;
        let before = serde_json::to_value(&app.session).unwrap();
        app.input = "/hello bad".into();
        app.start(&ctx);
        poll_until(&mut app, &ctx, |app| !app.running);
        assert_eq!(serde_json::to_value(&app.session).unwrap(), before);
        assert!(app.status.contains("User"));
        assert!(app
            .session
            .runtime_state("extension:desktop-ext")
            .unwrap()
            .is_none());
    }

    #[test]
    fn cancelled_native_command_joins_without_committing_unfinished_state() {
        let fixture = Fixture::new();
        let ctx = egui::Context::default();
        let mut app = fixture_app(&ctx, &fixture);
        app.extensions = desktop_extension(true, false).0;
        let before = serde_json::to_value(&app.session).unwrap();
        app.input = "/hello cancellation".into();
        app.start(&ctx);
        assert!(app.busy());
        app.signal.cancel();
        poll_until(&mut app, &ctx, |app| !app.running);
        assert!(app.worker.is_none());
        assert!(app.status.contains("取消"));
        assert_eq!(serde_json::to_value(&app.session).unwrap(), before);
    }

    #[test]
    fn native_session_lifecycle_runs_on_worker_and_branch_state_is_restored() {
        let fixture = Fixture::new();
        let ctx = egui::Context::default();
        let mut app = fixture_app(&ctx, &fixture);
        let (registry, trace) = desktop_extension(false, false);
        app.extensions = registry;
        app.input = "/hello first".into();
        app.start(&ctx);
        poll_until(&mut app, &ctx, |app| !app.running);
        let previous = app.session.clone();
        let next = Session {
            workspace: fixture.0.clone(),
            ..Default::default()
        };
        let next_id = next.id.clone();
        app.start_extension_action(
            &ctx,
            ExtensionAction::Replace(Box::new(next), SessionStartReason::New),
        );
        poll_until(&mut app, &ctx, |app| !app.running);
        assert_eq!(app.session.id, next_id);
        assert!(app
            .session
            .runtime_state("extension:desktop-ext")
            .unwrap()
            .is_none());
        app.start_extension_action(
            &ctx,
            ExtensionAction::Replace(Box::new(previous.clone()), SessionStartReason::Resume),
        );
        poll_until(&mut app, &ctx, |app| !app.running);
        assert_eq!(
            app.session
                .runtime_state("extension:desktop-ext")
                .unwrap()
                .unwrap()["count"],
            1
        );
        app.start_extension_action(&ctx, ExtensionAction::Quit);
        poll_until(&mut app, &ctx, |app| !app.running);
        assert!(app.extensions.ids().is_empty());
        let events = trace.lock().unwrap();
        assert!(events
            .iter()
            .any(|event| event == &format!("shutdown:New:{}", previous.id)));
        assert!(events
            .iter()
            .any(|event| event == &format!("start:New:{next_id}")));
        assert!(events
            .iter()
            .any(|event| event == &format!("start:Resume:{}", previous.id)));
        assert!(events
            .iter()
            .any(|event| event == &format!("shutdown:Quit:{}", previous.id)));
    }

    #[test]
    fn native_prompt_hooks_use_runtime_and_persist_one_user_and_custom_message() {
        let fixture = Fixture::new();
        let ctx = egui::Context::default();
        let mut app = fixture_app(&ctx, &fixture);
        let (registry, trace) = desktop_extension(false, false);
        app.extensions = registry;
        let base = app.session.context.system_prompt.clone();
        app.input = "real input".into();
        app.start(&ctx);
        poll_until(&mut app, &ctx, |app| !app.running);
        assert_eq!(
            app.session
                .context
                .messages
                .iter()
                .filter(|message| matches!(message, Message::User(_)))
                .count(),
            1
        );
        assert_eq!(
            app.session
                .context
                .messages
                .iter()
                .filter(|message| matches!(message, Message::Custom { .. }))
                .count(),
            1
        );
        assert_eq!(app.session.context.messages.len(), 3);
        assert_eq!(app.session.context.system_prompt, base);
        let events = trace.lock().unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|event| event.starts_with("agent_start:"))
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| event.starts_with("agent_end:"))
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| event.starts_with("settled:"))
                .count(),
            1
        );
    }

    #[test]
    fn manifest_load_failure_is_worker_based_and_does_not_mutate_the_session() {
        let fixture = Fixture::new();
        let ctx = egui::Context::default();
        let mut app = fixture_app(&ctx, &fixture);
        let before = serde_json::to_value(&app.session).unwrap();
        app.start_extension_action(
            &ctx,
            ExtensionAction::Load(vec![PathBuf::from("missing native manifest.json")]),
        );
        assert!(app.running);
        poll_until(&mut app, &ctx, |app| !app.running);
        assert!(app.worker.is_none());
        assert!(app.status.contains("manifest"));
        assert_eq!(serde_json::to_value(&app.session).unwrap(), before);
        assert!(app.loaded_extension_paths.is_empty());
    }

    #[test]
    fn azure_budget_requires_explicit_window_and_never_borrows_catalog_pricing() {
        let fixture = Fixture::new();
        let ctx = egui::Context::default();
        let mut app = fixture_app(&ctx, &fixture);
        let catalog = search("")
            .into_iter()
            .find(|model| model.provider == "azure-openai-responses")
            .unwrap();
        app.settings.mode = ProviderMode::AzureOpenAiResponses;
        app.settings.options.provider_id = catalog.provider.clone();
        app.settings.model = catalog.id.clone();
        app.auto_compaction = true;
        app.update_model_budget();
        assert_eq!(app.context_window, 0);
        assert!(app.run_options().is_err());
        app.context_window = 100_000;
        app.max_output_tokens = 4_000;
        assert_eq!(
            app.run_options().unwrap().automation.model.unwrap().cost,
            serde_json::Value::Null
        );
    }

    #[test]
    fn azure_vertex_extension_panels_render_without_credentials_or_network_io() {
        let fixture = Fixture::new();
        let ctx = egui::Context::default();
        let mut app = fixture_app(&ctx, &fixture);
        for mode in [
            ProviderMode::AzureOpenAiResponses,
            ProviderMode::GoogleVertex,
        ] {
            app.settings.mode = mode.clone();
            app.settings.options.provider_id.clear();
            app.settings.model = "fixture-model".into();
            app.settings.base_url = mode.default_base_url().into();
            app.settings.api_key.clear();
            app.settings.options.vertex.adc_path = Some(fixture.0.join("not-read-on-ui.json"));
            let output = ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(900.0, 640.0),
                    )),
                    ..Default::default()
                },
                |ctx| app.show(ctx),
            );
            assert!(!output.shapes.is_empty());
            assert!(app.worker.is_none());
            assert!(app.auth_worker.is_none());
            assert!(!app.busy());
            let composer =
                egui::containers::panel::PanelState::load(&ctx, egui::Id::new("composer")).unwrap();
            assert!(composer.rect.height() < 240.0);
            assert_eq!(
                app.provider_id(),
                if mode == ProviderMode::AzureOpenAiResponses {
                    "azure-openai-responses"
                } else {
                    "google-vertex"
                }
            );
        }
    }

    fn native_process_manifest(fixture: &Fixture, args: Vec<String>) -> PathBuf {
        let executable = fixture.0.join(if cfg!(windows) {
            "desktop native fixture.exe"
        } else {
            "desktop-native-fixture"
        });
        let compiler = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
        let compiled = std::process::Command::new(compiler)
            .args(["--edition=2021", "-O"])
            .arg(
                PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join("../agent-runtime/examples/native_extension_fixture.rs"),
            )
            .arg("-o")
            .arg(&executable)
            .output()
            .unwrap();
        assert!(
            compiled.status.success(),
            "{}",
            String::from_utf8_lossy(&compiled.stderr)
        );
        let manifest = fixture.0.join("native extension.json");
        std::fs::write(&manifest,serde_json::to_vec(&serde_json::json!({"schema":"agent-core-native-extension/1","id":"fixture","executable":executable,"args":args,"requestTimeoutMs":2000,"maxFrameBytes":1_048_576})).unwrap()).unwrap();
        manifest
    }

    #[test]
    fn real_rust_manifest_load_reload_new_resume_quit_and_command_state_are_connected() {
        let fixture = Fixture::new();
        let ctx = egui::Context::default();
        let mut app = fixture_app(&ctx, &fixture);
        let record = fixture.0.join("native requests.jsonl");
        let manifest =
            native_process_manifest(&fixture, vec![format!("--record={}", record.display())]);
        app.start_extension_action(&ctx, ExtensionAction::Load(vec![manifest.clone()]));
        poll_until(&mut app, &ctx, |app| !app.running);
        assert_eq!(app.extensions.ids(), ["fixture"]);
        assert_eq!(
            app.loaded_extension_paths.as_slice(),
            std::slice::from_ref(&manifest)
        );
        app.input = "/fixture_echo".into();
        app.start(&ctx);
        poll_until(&mut app, &ctx, |app| !app.running);
        assert_eq!(
            app.session
                .runtime_state("extension:fixture")
                .unwrap()
                .unwrap()["count"],
            1
        );
        assert_eq!(app.session.context.messages.len(), 1);
        assert!(matches!(app.session.context.messages[0], Message::User(_)));
        assert!(app
            .extension_display
            .iter()
            .any(|text| text == "native command"));
        app.start_extension_action(&ctx, ExtensionAction::Load(vec![manifest]));
        poll_until(&mut app, &ctx, |app| !app.running);
        app.input = "/fixture_echo".into();
        app.start(&ctx);
        poll_until(&mut app, &ctx, |app| !app.running);
        assert_eq!(
            app.session
                .runtime_state("extension:fixture")
                .unwrap()
                .unwrap()["count"],
            2
        );
        let previous = app.session.clone();
        let next = Session {
            workspace: fixture.0.clone(),
            ..Default::default()
        };
        app.start_extension_action(
            &ctx,
            ExtensionAction::Replace(Box::new(next), SessionStartReason::New),
        );
        poll_until(&mut app, &ctx, |app| !app.running);
        assert!(app
            .session
            .runtime_state("extension:fixture")
            .unwrap()
            .is_none());
        app.start_extension_action(
            &ctx,
            ExtensionAction::Replace(Box::new(previous), SessionStartReason::Resume),
        );
        poll_until(&mut app, &ctx, |app| !app.running);
        app.input = "/fixture_echo".into();
        app.start(&ctx);
        poll_until(&mut app, &ctx, |app| !app.running);
        assert_eq!(
            app.session
                .runtime_state("extension:fixture")
                .unwrap()
                .unwrap()["count"],
            3
        );
        app.start_extension_action(&ctx, ExtensionAction::Quit);
        poll_until(&mut app, &ctx, |app| !app.running);
        assert!(app.extensions.ids().is_empty());
        assert!(app.worker.is_none());
        let records = std::fs::read_to_string(record)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .collect::<Vec<_>>();
        for reason in ["startup", "reload", "new", "resume"] {
            assert!(
                records.iter().any(|record| record["method"] == "lifecycle"
                    && record["params"]["event"]["type"] == "session_start"
                    && record["params"]["event"]["reason"] == reason),
                "missing session_start {reason}"
            );
        }
        assert!(records
            .iter()
            .any(
                |record| record["params"]["event"]["type"] == "session_shutdown"
                    && record["params"]["event"]["reason"] == "quit"
            ));
        assert_eq!(
            records
                .iter()
                .filter(|record| record["method"] == "initialize")
                .count(),
            4
        );
    }

    #[test]
    fn cancelling_real_manifest_initialization_keeps_session_and_joins_transport() {
        let fixture = Fixture::new();
        let ctx = egui::Context::default();
        let mut app = fixture_app(&ctx, &fixture);
        let pid = fixture.0.join("extension.pid");
        let manifest = native_process_manifest(
            &fixture,
            vec!["--hang".into(), format!("--pid={}", pid.display())],
        );
        let before = serde_json::to_value(&app.session).unwrap();
        app.start_extension_action(&ctx, ExtensionAction::Load(vec![manifest]));
        let deadline = Instant::now() + Duration::from_secs(2);
        while !pid.exists() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        let start = Instant::now();
        app.signal.cancel();
        poll_until(&mut app, &ctx, |app| !app.running);
        assert!(start.elapsed() < Duration::from_secs(2));
        assert!(app.worker.is_none());
        assert!(app.extensions.ids().is_empty());
        assert!(app.status.contains("取消"));
        assert_eq!(serde_json::to_value(&app.session).unwrap(), before);
    }

    #[test]
    fn dead_after_initialize_reload_candidate_does_not_replace_healthy_native_extension() {
        let fixture = Fixture::new();
        let ctx = egui::Context::default();
        let mut app = fixture_app(&ctx, &fixture);
        let healthy = native_process_manifest(&fixture, vec![]);
        app.start_extension_action(&ctx, ExtensionAction::Load(vec![healthy.clone()]));
        poll_until(&mut app, &ctx, |app| !app.running);
        app.input = "/fixture_echo".into();
        app.start(&ctx);
        poll_until(&mut app, &ctx, |app| !app.running);
        let before = serde_json::to_value(&app.session).unwrap();
        let bad_fixture = Fixture::new();
        let dead = native_process_manifest(&bad_fixture, vec!["--exit-after-init".into()]);
        app.start_extension_action(&ctx, ExtensionAction::Load(vec![dead]));
        poll_until(&mut app, &ctx, |app| !app.running);
        assert_eq!(serde_json::to_value(&app.session).unwrap(), before);
        assert_eq!(
            app.loaded_extension_paths.as_slice(),
            std::slice::from_ref(&healthy)
        );
        assert_eq!(app.extensions.ids(), ["fixture"]);
        assert!(app.status.contains("lifecycle"));
        app.input = "/fixture_echo".into();
        app.start(&ctx);
        poll_until(&mut app, &ctx, |app| !app.running);
        assert_eq!(
            app.session
                .runtime_state("extension:fixture")
                .unwrap()
                .unwrap()["count"],
            2
        );
        app.start_extension_action(&ctx, ExtensionAction::Quit);
        poll_until(&mut app, &ctx, |app| !app.running);
    }

    #[derive(Clone)]
    enum InputFixtureAction {
        Handled,
        Transform(String, Option<Vec<Content>>),
    }
    struct InputFixture {
        id: String,
        ui_thread: std::thread::ThreadId,
        action: InputFixtureAction,
        seen: std::sync::Arc<std::sync::Mutex<Vec<agent_runtime::extensions::InputEvent>>>,
    }
    impl agent_runtime::extensions::Extension for InputFixture {
        fn id(&self) -> &str {
            &self.id
        }
        fn input(
            &self,
            event: &agent_runtime::extensions::InputEvent,
            context: &agent_runtime::extensions::ExtensionContext,
            _signal: &CancellationToken,
        ) -> Result<agent_runtime::extensions::InputResult, String> {
            assert_ne!(
                std::thread::current().id(),
                self.ui_thread,
                "input hook ran on UI thread"
            );
            self.seen.lock().unwrap().push(event.clone());
            let result = match &self.action {
                InputFixtureAction::Handled => agent_runtime::extensions::InputResult::Handled,
                InputFixtureAction::Transform(text, images) => {
                    agent_runtime::extensions::InputResult::Transform {
                        text: text.replace(
                            "$STATE",
                            context
                                .states
                                .get(&self.id)
                                .and_then(|state| state["prefix"].as_str())
                                .unwrap_or_default(),
                        ),
                        images: images.clone(),
                    }
                }
            };
            Ok(result)
        }
    }
    fn input_extension(
        action: InputFixtureAction,
    ) -> (
        SharedRegistry,
        std::sync::Arc<std::sync::Mutex<Vec<agent_runtime::extensions::InputEvent>>>,
    ) {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let registry = SharedRegistry::default();
        registry
            .register(std::sync::Arc::new(InputFixture {
                id: "input-fixture".into(),
                ui_thread: std::thread::current().id(),
                action,
                seen: seen.clone(),
            }))
            .unwrap();
        (registry, seen)
    }

    #[test]
    fn handled_input_is_worker_only_without_credentials_provider_validation_or_persistence() {
        let fixture = Fixture::new();
        let ctx = egui::Context::default();
        let mut app = fixture_app(&ctx, &fixture);
        let (registry, seen) = input_extension(InputFixtureAction::Handled);
        app.extensions = registry;
        app.settings.mode = ProviderMode::OpenAiCodex;
        app.settings.model.clear();
        app.settings.options.oauth = None;
        app.auto_compaction = true; // An unknown paid model must not block handled input.
        let before = serde_json::to_value(&app.session).unwrap();
        app.input = "handled unconfigured input".into();
        app.start(&ctx);
        assert!(app.running);
        poll_until(&mut app, &ctx, |app| !app.running);
        assert!(app.status.contains("未请求模型"));
        assert!(app.input.is_empty());
        assert!(app.worker.is_none());
        assert_eq!(serde_json::to_value(&app.session).unwrap(), before);
        assert!(app.store.list().unwrap().is_empty());
        let events = seen.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].source, InputSource::Interactive);
        assert_eq!(events[0].text, "handled unconfigured input");
    }

    #[test]
    fn input_transforms_before_prompt_templates_and_keeps_branch_state_and_images_once() {
        let fixture = Fixture::new();
        let ctx = egui::Context::default();
        let mut app = fixture_app(&ctx, &fixture);
        let prompts = fixture.0.join(".agent-core/prompts");
        std::fs::create_dir_all(&prompts).unwrap();
        std::fs::write(prompts.join("fixture.md"), "expanded $@ from template").unwrap();
        let mut image = Cursor::new(Vec::new());
        image::RgbaImage::from_pixel(1, 1, image::Rgba([1, 2, 3, 255]))
            .write_to(&mut image, image::ImageFormat::Png)
            .unwrap();
        let image = Content::Image {
            data: STANDARD.encode(image.into_inner()),
            mime_type: "image/png".into(),
        };
        let (registry, seen) = input_extension(InputFixtureAction::Transform(
            "/fixture $STATE".into(),
            Some(vec![image.clone()]),
        ));
        app.extensions = registry;
        app.session
            .record_runtime_state(
                "extension:input-fixture",
                serde_json::json!({"prefix":"restored"}),
            )
            .unwrap();
        app.input = "original raw input".into();
        app.start(&ctx);
        poll_until(&mut app, &ctx, |app| !app.running);
        assert_eq!(seen.lock().unwrap()[0].text, "original raw input");
        assert_eq!(seen.lock().unwrap().len(), 1);
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
        assert_eq!(
            user.content,
            vec![Content::text("expanded restored from template"), image]
        );
        assert_eq!(
            app.session
                .context
                .messages
                .iter()
                .filter(|message| matches!(message, Message::User(_)))
                .count(),
            1
        );
        assert_eq!(
            app.session
                .runtime_state("extension:input-fixture")
                .unwrap()
                .unwrap()["prefix"],
            "restored"
        );
    }

    #[test]
    fn raw_registered_command_skips_input_and_transform_does_not_reroute_commands() {
        let fixture = Fixture::new();
        let ctx = egui::Context::default();
        let mut app = fixture_app(&ctx, &fixture);
        let (input, seen) = input_extension(InputFixtureAction::Handled);
        let (command, _) = desktop_extension(false, false);
        // Preserve one shared registration chain without exposing registry internals.
        input
            .register(std::sync::Arc::new(DesktopExtensionFixture {
                ui_thread: std::thread::current().id(),
                trace: std::sync::Arc::new(std::sync::Mutex::new(vec![])),
                slow: false,
                invalid: false,
            }))
            .unwrap();
        app.extensions = input;
        app.input = "/hello raw command".into();
        app.start(&ctx);
        poll_until(&mut app, &ctx, |app| !app.running);
        assert!(seen.lock().unwrap().is_empty());
        assert!(app.session.context.messages.is_empty());
        app.extensions = command;
        let (transformed, seen) = input_extension(InputFixtureAction::Transform(
            "/hello from input".into(),
            None,
        ));
        transformed
            .register(std::sync::Arc::new(DesktopExtensionFixture {
                ui_thread: std::thread::current().id(),
                trace: std::sync::Arc::new(std::sync::Mutex::new(vec![])),
                slow: false,
                invalid: false,
            }))
            .unwrap();
        app.extensions = transformed;
        let count = app
            .session
            .runtime_state("extension:desktop-ext")
            .unwrap()
            .unwrap()["count"]
            .clone();
        app.input = "not a command".into();
        app.start(&ctx);
        poll_until(&mut app, &ctx, |app| !app.running);
        assert_eq!(seen.lock().unwrap().len(), 1);
        assert_eq!(
            app.session
                .context
                .messages
                .iter()
                .find(|message| matches!(message, Message::User(_)))
                .unwrap()
                .text(),
            "/hello from input"
        );
        assert_eq!(
            app.session
                .runtime_state("extension:desktop-ext")
                .unwrap()
                .unwrap()["count"],
            count
        );
    }

    #[test]
    fn preparation_failure_preserves_original_and_new_drafts_without_saving() {
        let fixture = Fixture::new();
        let ctx = egui::Context::default();
        let mut app = fixture_app(&ctx, &fixture);
        app.settings.mode = ProviderMode::OpenAiCompatible;
        app.settings.model.clear();
        let before = serde_json::to_value(&app.session).unwrap();
        app.input = "original draft".into();
        app.start(&ctx);
        app.input = "newly typed draft".into();
        poll_until(&mut app, &ctx, |app| !app.running);
        assert!(app.input.contains("original draft"));
        assert!(app.input.contains("newly typed draft"));
        assert_eq!(serde_json::to_value(&app.session).unwrap(), before);
        assert!(app.store.list().unwrap().is_empty());
    }

    #[test]
    fn steering_prepares_once_off_ui_and_finished_agent_continues_without_replaying_input() {
        let fixture = Fixture::new();
        let ctx = egui::Context::default();
        let mut app = fixture_app(&ctx, &fixture);
        let (registry, seen) = input_extension(InputFixtureAction::Transform(
            "prepared steering".into(),
            None,
        ));
        app.extensions = registry;
        app.running = true;
        app.input = "raw steering".into();
        app.start_steering_input(&ctx);
        assert!(app.queue_receiver.is_some());
        // The current agent finishes before this preparation result reaches the UI.
        app.running = false;
        poll_until(&mut app, &ctx, |app| {
            app.queue_receiver.is_none() && !app.running
        });
        let events = seen.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0].streaming_behavior,
            Some(InputStreamingBehavior::Steer)
        );
        assert_eq!(events[0].text, "raw steering");
        assert_eq!(
            app.session
                .context
                .messages
                .iter()
                .find(|message| matches!(message, Message::User(_)))
                .unwrap()
                .text(),
            "prepared steering"
        );
        assert!(app.queue_worker.is_none());
    }

    #[test]
    fn native_hanging_input_cancellation_is_bounded_and_restores_draft_without_messages() {
        let fixture = Fixture::new();
        let ctx = egui::Context::default();
        let mut app = fixture_app(&ctx, &fixture);
        let record = fixture.0.join("input requests.jsonl");
        let manifest = native_process_manifest(
            &fixture,
            vec![
                "--hang-input".into(),
                format!("--record={}", record.display()),
            ],
        );
        app.start_extension_action(&ctx, ExtensionAction::Load(vec![manifest]));
        poll_until(&mut app, &ctx, |app| !app.running);
        let before = serde_json::to_value(&app.session).unwrap();
        app.input = "pending native input".into();
        app.start(&ctx);
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if std::fs::read_to_string(&record).is_ok_and(|text| {
                text.lines().any(|line| {
                    serde_json::from_str::<serde_json::Value>(line)
                        .is_ok_and(|value| value["method"] == "input")
                })
            }) {
                break;
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        let start = Instant::now();
        app.signal.cancel();
        poll_until(&mut app, &ctx, |app| !app.running);
        assert!(start.elapsed() < Duration::from_secs(2));
        assert!(app.worker.is_none());
        assert_eq!(app.input, "pending native input");
        assert_eq!(serde_json::to_value(&app.session).unwrap(), before);
        assert!(app.status.contains("取消"));
    }

    #[test]
    fn expand_user_prompt_preserves_timestamp_and_does_not_duplicate_images() {
        let image = Content::Image {
            data: "fixture".into(),
            mime_type: "image/png".into(),
        };
        let prompt = Message::User(agent_core::types::UserMessage {
            content: vec![Content::text("plain"), image.clone()],
            timestamp: 42,
        });
        let expanded = expand_user_prompt(prompt, &ResourceLoad::default()).unwrap();
        let Message::User(user) = expanded else {
            panic!("user")
        };
        assert_eq!(user.timestamp, 42);
        assert_eq!(user.content, vec![Content::text("plain"), image]);
    }

    #[test]
    fn bedrock_configuration_renders_without_reading_aws_profiles_and_uses_real_catalog_budget() {
        let fixture = Fixture::new();
        let ctx = egui::Context::default();
        let mut app = fixture_app(&ctx, &fixture);
        app.settings.mode = ProviderMode::AmazonBedrock;
        app.settings.options.provider_id.clear();
        app.settings.base_url = ProviderMode::AmazonBedrock.default_base_url().into();
        app.settings.api_key.clear();
        app.settings.options.bedrock.credentials_file =
            Some(fixture.0.join("not read on UI.credentials"));
        app.settings.options.bedrock.config_file = Some(fixture.0.join("not read on UI.config"));
        app.settings.options.bedrock.sso_cache_dir =
            Some(fixture.0.join("not read on UI.SSO cache"));
        app.settings.options.bedrock.sso_endpoint = "http://127.0.0.1:1/sso".into();
        app.settings.options.bedrock.sso_oidc_endpoint = "http://127.0.0.1:1/oidc".into();
        app.settings.options.bedrock.profile = "fixture-profile".into();
        app.settings.options.bedrock.region = "us-west-2".into();
        app.settings.options.bedrock.max_tokens = Some(512);
        app.settings.options.bedrock.temperature = Some(0.1);
        app.settings.options.bedrock.cache_retention = BedrockCacheRetention::Long;
        app.settings_open = true;
        for _ in 0..2 {
            let output = ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(900.0, 640.0),
                    )),
                    ..Default::default()
                },
                |ctx| app.show(ctx),
            );
            assert!(!output.shapes.is_empty());
        }
        assert!(!app.busy());
        assert!(app.worker.is_none());
        assert!(app.auth_worker.is_none());
        assert_eq!(app.provider_id(), "amazon-bedrock");
        let draft = &app
            .settings_draft
            .as_ref()
            .unwrap()
            .settings
            .options
            .bedrock;
        assert_eq!(
            draft.sso_cache_dir,
            Some(fixture.0.join("not read on UI.SSO cache"))
        );
        assert_eq!(draft.sso_endpoint, "http://127.0.0.1:1/sso");
        assert_eq!(draft.sso_oidc_endpoint, "http://127.0.0.1:1/oidc");
        let composer =
            egui::containers::panel::PanelState::load(&ctx, egui::Id::new("composer")).unwrap();
        assert!(composer.rect.height() < 240.0);
        let catalog = search("")
            .into_iter()
            .find(|model| model.provider == "amazon-bedrock")
            .unwrap();
        app.settings.model = catalog.id.clone();
        app.auto_compaction = true;
        let options = app.run_options().unwrap();
        let budget = options.automation.model.unwrap();
        assert_eq!(budget.cost, catalog.cost);
        assert_eq!(budget.max_output_tokens, 512);
        app.settings.model =
            "arn:aws:bedrock:us-west-2:123456789012:inference-profile/custom-profile".into();
        app.update_model_budget();
        assert!(app.run_options().is_err());
        app.context_window = 100_000;
        let budget = app.run_options().unwrap().automation.model.unwrap();
        assert_eq!(budget.cost, serde_json::Value::Null);
        assert_eq!(budget.max_output_tokens, 512);
    }

    #[test]
    fn bedrock_input_handled_needs_no_model_or_aws_profile_and_saves_nothing() {
        let fixture = Fixture::new();
        let ctx = egui::Context::default();
        let mut app = fixture_app(&ctx, &fixture);
        let (registry, seen) = input_extension(InputFixtureAction::Handled);
        app.extensions = registry;
        app.settings.mode = ProviderMode::AmazonBedrock;
        app.settings.model.clear();
        app.settings.api_key.clear();
        app.settings.options.bedrock.credentials_file =
            Some(fixture.0.join("must-not-open.credentials"));
        app.settings.options.bedrock.config_file = Some(fixture.0.join("must-not-open.config"));
        app.settings.options.bedrock.sso_cache_dir =
            Some(fixture.0.join("must-not-open.SSO cache"));
        app.settings.options.bedrock.sso_endpoint = "not-a-url".into();
        app.settings.options.bedrock.sso_oidc_endpoint = "not-an-oidc-url".into();
        let before = serde_json::to_value(&app.session).unwrap();
        app.input = "handled before AWS resolution".into();
        app.start(&ctx);
        poll_until(&mut app, &ctx, |app| !app.running);
        assert_eq!(seen.lock().unwrap().len(), 1);
        assert_eq!(serde_json::to_value(&app.session).unwrap(), before);
        assert!(app.store.list().unwrap().is_empty());
        assert!(app.status.contains("未请求模型"));
    }

    #[test]
    fn adc_file_error_occurs_on_provider_worker_and_preserves_the_accepted_user() {
        let fixture = Fixture::new();
        let ctx = egui::Context::default();
        let mut app = fixture_app(&ctx, &fixture);
        let (registry, seen) =
            input_extension(InputFixtureAction::Transform("transformed".into(), None));
        app.extensions = registry;
        app.settings.mode = ProviderMode::GoogleVertex;
        app.settings.model = "gemini-fixture".into();
        app.settings.options.vertex.adc_path = Some(fixture.0.join("missing ADC.json"));
        app.settings.base_url = "http://127.0.0.1:1".into();
        app.settings.api_key.clear();
        app.settings.options.vertex.project = "fixture-project".into();
        app.settings.options.vertex.location = "global".into();
        app.input = "keep ADC draft".into();
        app.start(&ctx);
        assert!(app.running);
        poll_until(&mut app, &ctx, |app| !app.running);
        assert_eq!(seen.lock().unwrap().len(), 1);
        assert!(app.input.is_empty());
        assert_eq!(
            app.session
                .context
                .messages
                .iter()
                .find(|message| matches!(message, Message::User(_)))
                .unwrap()
                .text(),
            "transformed"
        );
        assert!(app.status.contains("ADC file"));
        assert_eq!(
            app.store
                .list()
                .unwrap()
                .pop()
                .unwrap()
                .context
                .messages
                .len(),
            2
        );
    }

    #[test]
    fn real_native_input_transform_reaches_demo_once_and_keeps_shared_branch_state() {
        let fixture = Fixture::new();
        let ctx = egui::Context::default();
        let mut app = fixture_app(&ctx, &fixture);
        let record = fixture.0.join("native raw input.jsonl");
        let manifest = native_process_manifest(
            &fixture,
            vec![
                "--input-prefix=from native: ".into(),
                format!("--record={}", record.display()),
            ],
        );
        app.start_extension_action(&ctx, ExtensionAction::Load(vec![manifest]));
        poll_until(&mut app, &ctx, |app| !app.running);
        app.input = "/fixture_echo".into();
        app.start(&ctx);
        poll_until(&mut app, &ctx, |app| !app.running);
        app.input = "original prompt".into();
        app.start(&ctx);
        poll_until(&mut app, &ctx, |app| !app.running);
        assert_eq!(
            app.session
                .context
                .messages
                .iter()
                .filter(|message| matches!(message, Message::User(_)))
                .count(),
            2
        );
        assert_eq!(
            app.session
                .context
                .messages
                .iter()
                .rev()
                .find(|message| matches!(message, Message::User(_)))
                .unwrap()
                .text(),
            "from native: original prompt"
        );
        assert_eq!(
            app.session
                .runtime_state("extension:fixture")
                .unwrap()
                .unwrap()["count"],
            1
        );
        let requests = std::fs::read_to_string(record)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .collect::<Vec<_>>();
        let inputs = requests
            .iter()
            .filter(|request| request["method"] == "input")
            .collect::<Vec<_>>();
        assert_eq!(inputs.len(), 1);
        assert_eq!(inputs[0]["params"]["event"]["text"], "original prompt");
        assert_eq!(inputs[0]["params"]["event"]["source"], "interactive");
        let persisted = app.store.list().unwrap().pop().unwrap();
        assert_eq!(
            persisted
                .runtime_state("extension:fixture")
                .unwrap()
                .unwrap()["count"],
            1
        );
    }

    #[test]
    fn steering_registered_command_keeps_draft_and_never_calls_input_hook() {
        let fixture = Fixture::new();
        let ctx = egui::Context::default();
        let mut app = fixture_app(&ctx, &fixture);
        let (registry, seen) = input_extension(InputFixtureAction::Handled);
        registry
            .register(std::sync::Arc::new(DesktopExtensionFixture {
                ui_thread: std::thread::current().id(),
                trace: std::sync::Arc::new(std::sync::Mutex::new(vec![])),
                slow: false,
                invalid: false,
            }))
            .unwrap();
        app.extensions = registry;
        app.running = true;
        app.input = "/hello wait".into();
        app.start_steering_input(&ctx);
        assert!(app.queue_worker.is_none());
        assert!(seen.lock().unwrap().is_empty());
        assert_eq!(app.input, "/hello wait");
        app.running = false;
    }

    #[test]
    fn queued_prompt_validation_failure_restores_raw_draft_without_replaying_input() {
        let fixture = Fixture::new();
        let ctx = egui::Context::default();
        let mut app = fixture_app(&ctx, &fixture);
        let (registry, seen) = input_extension(InputFixtureAction::Transform(
            "prepared queued input".into(),
            None,
        ));
        app.extensions = registry;
        app.settings.mode = ProviderMode::OpenAiCompatible;
        app.settings.model.clear();
        let before = serde_json::to_value(&app.session).unwrap();
        app.running = true;
        app.input = "raw queued draft".into();
        app.start_steering_input(&ctx);
        let deadline = Instant::now() + Duration::from_secs(2);
        while app.queue_receiver.is_some() {
            app.poll_steering_input();
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(app.queues.len(), 1);
        assert_eq!(app.queued_drafts.len(), 1);
        app.running = false;
        app.input = "new draft".into();
        app.poll(&ctx);
        poll_until(&mut app, &ctx, |app| !app.running);
        assert_eq!(seen.lock().unwrap().len(), 1);
        assert_eq!(app.input, "raw queued draft\nnew draft");
        assert_eq!(serde_json::to_value(&app.session).unwrap(), before);
        assert!(app.store.list().unwrap().is_empty());
    }

    #[test]
    fn queued_prompt_cancel_restores_unaccepted_raw_draft() {
        let fixture = Fixture::new();
        let ctx = egui::Context::default();
        let mut app = fixture_app(&ctx, &fixture);
        let (registry, seen) = input_extension(InputFixtureAction::Transform(
            "prepared queued input".into(),
            None,
        ));
        app.extensions = registry;
        app.running = true;
        app.input = "raw queued draft".into();
        app.start_steering_input(&ctx);
        let deadline = Instant::now() + Duration::from_secs(2);
        while app.queue_receiver.is_some() {
            app.poll_steering_input();
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        app.signal.cancel();
        app.queues.clear();
        app.running = false;
        app.poll(&ctx);
        assert_eq!(seen.lock().unwrap().len(), 1);
        assert_eq!(app.input, "raw queued draft");
        assert!(app.queued_drafts.is_empty());
        assert!(app.session.context.messages.is_empty());
    }

    struct ModelObserver {
        id: String,
        ui_thread: std::thread::ThreadId,
        fail: bool,
        events: std::sync::Arc<std::sync::Mutex<Vec<LifecycleEvent>>>,
    }
    impl agent_runtime::extensions::Extension for ModelObserver {
        fn id(&self) -> &str {
            &self.id
        }
        fn on_event(
            &self,
            event: &LifecycleEvent,
            context: &agent_runtime::extensions::ExtensionContext,
            _signal: &CancellationToken,
        ) -> Result<(), String> {
            if let LifecycleEvent::ModelSelect { model, .. } = event {
                assert_ne!(std::thread::current().id(), self.ui_thread);
                assert_eq!(context.model.as_ref(), Some(model));
                self.events.lock().unwrap().push(event.clone());
                if self.fail {
                    return Err("model_select fixture error".into());
                }
            }
            Ok(())
        }
    }
    fn observe_models(
        fail: bool,
    ) -> (
        SharedRegistry,
        std::sync::Arc<std::sync::Mutex<Vec<LifecycleEvent>>>,
    ) {
        let registry = SharedRegistry::default();
        let events = std::sync::Arc::new(std::sync::Mutex::new(vec![]));
        registry
            .register(std::sync::Arc::new(ModelObserver {
                id: "model-observer".into(),
                ui_thread: std::thread::current().id(),
                fail,
                events: events.clone(),
            }))
            .unwrap();
        (registry, events)
    }
    fn fixture_model_configuration(app: &DesktopApp, model: &str) -> ModelConfiguration {
        let mut configuration = app.model_configuration();
        configuration.settings.mode = ProviderMode::OpenAiCompatible;
        configuration.settings.model = model.into();
        configuration.settings.options.provider_id = "custom-fixture".into();
        configuration.settings.base_url = "http://127.0.0.1:1/fixture".into();
        configuration.settings.api_key = "model-secret-fixture".into();
        configuration
    }

    #[test]
    fn model_configuration_draft_renders_without_changing_applied_settings_or_messages() {
        let fixture = Fixture::new();
        let ctx = egui::Context::default();
        let mut app = fixture_app(&ctx, &fixture);
        let before = serde_json::to_value(&app.session).unwrap();
        app.input = "untouched draft".into();
        let mut configuration = fixture_model_configuration(&app, "draft-model");
        configuration.settings.options.bedrock.sts_endpoint = "http://127.0.0.1:1/sts".into();
        configuration.settings.options.bedrock.profile = "preserved-profile".into();
        configuration.settings.options.vertex.project = "preserved-project".into();
        app.settings_draft = Some(configuration);
        for _ in 0..2 {
            let _ = ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(900.0, 640.0),
                    )),
                    ..Default::default()
                },
                |ctx| app.show(ctx),
            );
        }
        assert_eq!(app.settings.mode, ProviderMode::Demo);
        assert!(app.settings.model.is_empty());
        assert_eq!(app.input, "untouched draft");
        assert_eq!(serde_json::to_value(&app.session).unwrap(), before);
        assert!(!app.busy());
        assert_eq!(
            app.settings_draft
                .as_ref()
                .unwrap()
                .settings
                .options
                .vertex
                .project,
            "preserved-project"
        );
        assert!(app.store.list().unwrap().is_empty());
    }

    #[test]
    fn model_selection_commits_identity_only_then_notifies_on_worker_and_skips_same_set() {
        let fixture = Fixture::new();
        let ctx = egui::Context::default();
        let mut app = fixture_app(&ctx, &fixture);
        let (registry, events) = observe_models(false);
        app.extensions = registry;
        app.input = "original prompt draft".into();
        let mut configuration = fixture_model_configuration(&app, "selected-model");
        configuration.settings.options.azure.deployment = "keep-deployment".into();
        configuration.settings.options.vertex.project = "keep-project".into();
        configuration.settings.options.bedrock.profile = "keep-profile".into();
        configuration.settings.options.bedrock.sts_endpoint = "http://127.0.0.1:1/sts".into();
        app.start_model_selection(&ctx, configuration.clone(), ModelSelectSource::Set);
        assert!(app.settings.model.is_empty());
        poll_until(&mut app, &ctx, |app| !app.busy());
        assert_eq!(app.settings.model, "selected-model");
        assert_eq!(app.settings.base_url, configuration.settings.base_url);
        assert_eq!(app.settings.api_key, configuration.settings.api_key);
        assert_eq!(app.settings.options.azure.deployment, "keep-deployment");
        assert_eq!(app.settings.options.vertex.project, "keep-project");
        assert_eq!(app.settings.options.bedrock.profile, "keep-profile");
        assert_eq!(app.input, "original prompt draft");
        assert!(app.session.context.messages.is_empty());
        let persisted = app.store.list().unwrap().pop().unwrap();
        assert_eq!(
            stored_model_selection(&persisted).unwrap().unwrap().model,
            "selected-model"
        );
        assert!(!serde_json::to_string(&persisted)
            .unwrap()
            .contains("model-secret-fixture"));
        assert!(
            matches!(&events.lock().unwrap()[0], LifecycleEvent::ModelSelect { model, previous_model: Some(previous), source: ModelSelectSource::Set } if model.model == "selected-model" && previous.provider == "demo" && previous.model.is_empty())
        );
        app.start_model_selection(&ctx, configuration, ModelSelectSource::Set);
        poll_until(&mut app, &ctx, |app| !app.busy());
        assert_eq!(events.lock().unwrap().len(), 1);
        assert_eq!(app.session.entries.len(), persisted.entries.len());
    }

    #[test]
    fn model_selection_cancelled_after_validation_before_commit_keeps_configuration_and_draft() {
        let fixture = Fixture::new();
        let ctx = egui::Context::default();
        let mut app = fixture_app(&ctx, &fixture);
        let (registry, events) = observe_models(false);
        app.extensions = registry;
        let before = serde_json::to_value(&app.session).unwrap();
        app.input = "raw draft remains".into();
        let configuration = fixture_model_configuration(&app, "not-committed");
        app.settings_draft = Some(configuration.clone());
        app.start_model_selection(&ctx, configuration, ModelSelectSource::Set);
        // Validation produced a candidate, but this UI has not accepted the commit.
        let prepared = app
            .model_receiver
            .as_ref()
            .unwrap()
            .recv_timeout(Duration::from_secs(2))
            .unwrap();
        assert!(matches!(prepared, ModelSelectionMessage::Prepared(_, _)));
        app.signal.cancel();
        poll_until(&mut app, &ctx, |app| !app.busy());
        assert_eq!(app.settings.mode, ProviderMode::Demo);
        assert_eq!(app.input, "raw draft remains");
        assert_eq!(
            app.settings_draft.as_ref().unwrap().settings.model,
            "not-committed"
        );
        assert_eq!(serde_json::to_value(&app.session).unwrap(), before);
        assert!(app.store.list().unwrap().is_empty());
        assert!(events.lock().unwrap().is_empty());
        assert!(app.status.contains("选择已取消"));
    }

    #[test]
    fn invalid_model_configuration_keeps_draft_and_does_not_emit_or_persist_errors() {
        let fixture = Fixture::new();
        let ctx = egui::Context::default();
        let mut app = fixture_app(&ctx, &fixture);
        let (registry, events) = observe_models(false);
        app.extensions = registry;
        let before = serde_json::to_value(&app.session).unwrap();
        app.input = "raw draft remains".into();
        let mut configuration = fixture_model_configuration(&app, "invalid");
        configuration.settings.base_url = "http://127.0.0.1:1/?secret=model-secret-fixture".into();
        app.settings_draft = Some(configuration.clone());
        app.start_model_selection(&ctx, configuration, ModelSelectSource::Set);
        poll_until(&mut app, &ctx, |app| !app.busy());
        assert_eq!(app.settings.mode, ProviderMode::Demo);
        assert_eq!(app.input, "raw draft remains");
        assert_eq!(serde_json::to_value(&app.session).unwrap(), before);
        assert!(app.store.list().unwrap().is_empty());
        assert!(events.lock().unwrap().is_empty());
        assert!(!app.status.contains("model-secret-fixture"));
        assert_eq!(
            app.settings_draft.as_ref().unwrap().settings.model,
            "invalid"
        );
    }

    #[test]
    fn model_hook_failure_is_diagnostic_not_a_veto_and_later_observers_run() {
        let fixture = Fixture::new();
        let ctx = egui::Context::default();
        let mut app = fixture_app(&ctx, &fixture);
        let (registry, events) = observe_models(true);
        registry
            .register(std::sync::Arc::new(ModelObserver {
                id: "second-model-observer".into(),
                ui_thread: std::thread::current().id(),
                fail: false,
                events: events.clone(),
            }))
            .unwrap();
        app.extensions = registry;
        app.start_model_selection(
            &ctx,
            fixture_model_configuration(&app, "selected"),
            ModelSelectSource::Set,
        );
        poll_until(&mut app, &ctx, |app| !app.busy());
        assert_eq!(app.settings.model, "selected");
        assert_eq!(events.lock().unwrap().len(), 2);
        assert!(app
            .extension_display
            .iter()
            .any(|text| text.contains("model_select fixture error")));
        assert!(app.session.context.messages.is_empty());
    }

    #[test]
    fn model_storage_failure_rolls_back_applied_configuration_and_keeps_requested_edit() {
        let fixture = Fixture::new();
        let ctx = egui::Context::default();
        let mut app = fixture_app(&ctx, &fixture);
        let (registry, events) = observe_models(false);
        app.extensions = registry;
        let blocked = fixture.0.join("store-is-file");
        std::fs::write(&blocked, b"unchanged").unwrap();
        app.store = SessionStore::new(&blocked);
        let before = serde_json::to_value(&app.session).unwrap();
        let old_bedrock = serde_json::to_value(&app.settings.options.bedrock).unwrap();
        let mut configuration = fixture_model_configuration(&app, "unsaved");
        configuration.settings.options.bedrock.sso_cache_dir =
            Some(fixture.0.join("unsaved SSO cache"));
        configuration.settings.options.bedrock.sso_endpoint = "http://127.0.0.1:1/sso".into();
        configuration.settings.options.bedrock.sso_oidc_endpoint = "http://127.0.0.1:1/oidc".into();
        let requested_bedrock =
            serde_json::to_value(&configuration.settings.options.bedrock).unwrap();
        app.start_model_selection(&ctx, configuration, ModelSelectSource::Set);
        poll_until(&mut app, &ctx, |app| !app.busy());
        assert_eq!(app.settings.mode, ProviderMode::Demo);
        assert_eq!(
            app.settings_draft.as_ref().unwrap().settings.model,
            "unsaved"
        );
        assert_eq!(serde_json::to_value(&app.session).unwrap(), before);
        assert_eq!(
            serde_json::to_value(&app.settings.options.bedrock).unwrap(),
            old_bedrock
        );
        assert_eq!(
            serde_json::to_value(
                &app.settings_draft
                    .as_ref()
                    .unwrap()
                    .settings
                    .options
                    .bedrock
            )
            .unwrap(),
            requested_bedrock
        );
        assert!(events.lock().unwrap().is_empty());
        assert_eq!(std::fs::read(blocked).unwrap(), b"unchanged");
    }

    #[test]
    fn model_restore_reads_current_branch_and_demo_empty_identity_is_real() {
        let fixture = Fixture::new();
        let ctx = egui::Context::default();
        let mut app = fixture_app(&ctx, &fixture);
        let (registry, events) = observe_models(false);
        app.extensions = registry;
        let first = fixture_model_configuration(&app, "first");
        app.start_model_selection(&ctx, first, ModelSelectSource::Set);
        poll_until(&mut app, &ctx, |app| !app.busy());
        let first_leaf = app.session.get_leaf_id().unwrap().unwrap();
        app.start_model_selection(
            &ctx,
            fixture_model_configuration(&app, "second"),
            ModelSelectSource::Set,
        );
        poll_until(&mut app, &ctx, |app| !app.busy());
        app.navigate_tree(Some(&first_leaf)).unwrap();
        app.start_model_restore(&ctx);
        poll_until(&mut app, &ctx, |app| !app.busy());
        assert_eq!(app.settings.model, "first");
        assert_eq!(
            app.session.get_leaf_id().unwrap().as_ref(),
            Some(&first_leaf)
        );
        assert!(
            matches!(&events.lock().unwrap()[2], LifecycleEvent::ModelSelect { model, previous_model: Some(previous), source: ModelSelectSource::Restore } if model.model == "first" && previous.model == "second")
        );
        let mut demo = app.model_configuration();
        demo.settings.mode = ProviderMode::Demo;
        demo.settings.model.clear();
        app.start_model_selection(&ctx, demo, ModelSelectSource::Set);
        poll_until(&mut app, &ctx, |app| !app.busy());
        assert_eq!(
            stored_model_selection(&app.session).unwrap().unwrap(),
            ModelIdentity {
                provider: "demo".into(),
                model: String::new()
            }
        );
        assert_eq!(app.settings.mode, ProviderMode::Demo);
        assert!(app.session.context.messages.is_empty());
    }

    #[test]
    fn real_hanging_model_notification_cancels_promptly_and_keeps_committed_identity() {
        let fixture = Fixture::new();
        let ctx = egui::Context::default();
        let mut app = fixture_app(&ctx, &fixture);
        let record = fixture.0.join("model notifications.jsonl");
        let manifest = native_process_manifest(
            &fixture,
            vec![
                "--hang-model-select".into(),
                format!("--record={}", record.display()),
            ],
        );
        app.start_extension_action(&ctx, ExtensionAction::Load(vec![manifest]));
        poll_until(&mut app, &ctx, |app| !app.busy());
        app.input = "unsent raw draft".into();
        app.start_model_selection(
            &ctx,
            fixture_model_configuration(&app, "committed"),
            ModelSelectSource::Set,
        );
        poll_until(&mut app, &ctx, |app| app.model_committed);
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if std::fs::read_to_string(&record).is_ok_and(|text| {
                text.lines().any(|line| {
                    serde_json::from_str::<serde_json::Value>(line)
                        .is_ok_and(|value| value["params"]["event"]["type"] == "model_select")
                })
            }) {
                break;
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        let start = Instant::now();
        app.signal.cancel();
        poll_until(&mut app, &ctx, |app| !app.busy());
        assert!(start.elapsed() < Duration::from_secs(2));
        assert!(app.model_worker.is_none());
        assert_eq!(app.settings.model, "committed");
        assert_eq!(
            stored_model_selection(&app.store.list().unwrap().pop().unwrap())
                .unwrap()
                .unwrap()
                .model,
            "committed"
        );
        assert_eq!(app.input, "unsent raw draft");
        assert!(app.session.context.messages.is_empty());
        assert!(app.status.contains("通知已取消"));
        let requests = std::fs::read_to_string(record).unwrap();
        assert!(!requests.contains("model-secret-fixture"));
    }

    #[test]
    fn switching_protocol_drafts_preserves_options_custom_endpoint_and_typed_credentials() {
        let fixture = Fixture::new();
        let ctx = egui::Context::default();
        let mut app = fixture_app(&ctx, &fixture);
        let first = fixture_model_configuration(&app, "first");
        let mut next = first.clone();
        next.settings.mode = ProviderMode::AmazonBedrock;
        app.switch_draft_mode(&mut next, first.clone());
        next.settings.options.bedrock.profile = "role-profile".into();
        next.settings.options.bedrock.sts_endpoint = "http://127.0.0.1:1/sts".into();
        next.settings.options.bedrock.sso_cache_dir = Some(fixture.0.join("unread SSO cache"));
        next.settings.options.bedrock.sso_endpoint = "http://127.0.0.1:1/sso".into();
        next.settings.options.bedrock.sso_oidc_endpoint = "http://127.0.0.1:1/oidc".into();
        next.settings.options.vertex.project = "unrelated-project".into();
        let bedrock = next.clone();
        next.settings.mode = ProviderMode::OpenAiCompatible;
        app.switch_draft_mode(&mut next, bedrock.clone());
        assert_eq!(next.settings.model, first.settings.model);
        assert_eq!(next.settings.api_key, first.settings.api_key);
        assert_eq!(next.settings.base_url, first.settings.base_url);
        let previous = next.clone();
        next.settings.mode = ProviderMode::AmazonBedrock;
        app.switch_draft_mode(&mut next, previous);
        assert_eq!(next.settings.options.bedrock.profile, "role-profile");
        assert_eq!(
            next.settings.options.bedrock.sts_endpoint,
            "http://127.0.0.1:1/sts"
        );
        assert_eq!(next.settings.options.vertex.project, "unrelated-project");
        assert_eq!(
            next.settings.options.bedrock.sso_cache_dir,
            Some(fixture.0.join("unread SSO cache"))
        );
        assert_eq!(
            next.settings.options.bedrock.sso_endpoint,
            "http://127.0.0.1:1/sso"
        );
        assert_eq!(
            next.settings.options.bedrock.sso_oidc_endpoint,
            "http://127.0.0.1:1/oidc"
        );
        assert_eq!(app.settings.mode, ProviderMode::Demo);
        assert!(app.store.list().unwrap().is_empty());
    }

    #[test]
    fn new_protocol_draft_drops_foreign_oauth_but_cached_drafts_keep_their_own_auth() {
        let fixture = Fixture::new();
        let ctx = egui::Context::default();
        let mut app = fixture_app(&ctx, &fixture);
        let mut codex = app.model_configuration();
        codex.settings.mode = ProviderMode::OpenAiCodex;
        codex.settings.options.provider_id = "openai-codex".into();
        codex.settings.model = "codex-fixture".into();
        codex.settings.base_url = "http://127.0.0.1:1/preserved-custom".into();
        codex.settings.api_key = "old-codex-key-do-not-transfer".into();
        codex.settings.options.api_key_explicit = true;
        codex.settings.options.oauth = Some(mock_credential());
        codex.settings.options.oauth_config =
            Some(NativeOAuthConfig::for_provider("anthropic").unwrap());
        codex.settings.options.bedrock.profile = "preserved-cloud-profile".into();
        let mut next = codex.clone();
        next.settings.mode = ProviderMode::Anthropic;
        app.switch_draft_mode(&mut next, codex.clone());
        assert!(next.settings.options.oauth.is_none());
        assert!(next.settings.options.oauth_config.is_none());
        assert!(!next.settings.options.api_key_explicit);
        assert_ne!(next.settings.api_key, codex.settings.api_key);
        assert_eq!(next.settings.base_url, codex.settings.base_url);
        assert_eq!(
            next.settings.options.bedrock.profile,
            "preserved-cloud-profile"
        );
        next.settings.api_key = "typed-anthropic-key".into();
        next.settings.options.api_key_explicit = true;
        let anthropic = next.clone();
        next.settings.mode = ProviderMode::OpenAiCodex;
        app.switch_draft_mode(&mut next, anthropic.clone());
        assert_eq!(next.settings.api_key, codex.settings.api_key);
        assert!(next.settings.options.api_key_explicit);
        assert_eq!(
            next.settings.options.oauth.as_ref().unwrap().access,
            codex.settings.options.oauth.as_ref().unwrap().access
        );
        assert!(next.settings.options.oauth_config.is_some());
        let previous = next.clone();
        next.settings.mode = ProviderMode::Anthropic;
        app.switch_draft_mode(&mut next, previous);
        assert_eq!(next.settings.api_key, "typed-anthropic-key");
        assert!(next.settings.options.api_key_explicit);
        assert!(next.settings.options.oauth.is_none());
        assert!(next.settings.options.oauth_config.is_none());
        assert_eq!(app.settings.mode, ProviderMode::Demo);
        assert!(app.store.list().unwrap().is_empty());
    }

    #[test]
    fn cross_provider_native_identity_drops_old_auth_while_same_provider_preserves_it() {
        let fixture = Fixture::new();
        let ctx = egui::Context::default();
        let app = fixture_app(&ctx, &fixture);
        let mut current = app.model_configuration();
        current.settings.mode = ProviderMode::OpenAiCodex;
        current.settings.model = "original".into();
        current.settings.options.provider_id = "openai-codex".into();
        current.settings.options.oauth = Some(mock_credential());
        current.settings.options.oauth_config =
            Some(NativeOAuthConfig::for_provider("anthropic").unwrap());
        current.settings.options.api_key_explicit = true;
        current.settings.api_key = "foreign-explicit-key".into();
        current.settings.base_url = "http://127.0.0.1:1/custom".into();
        current.settings.options.auth_storage_path = Some(fixture.0.join("unread-auth.dat"));
        let next = configuration_for_identity(
            &current,
            &ModelIdentity {
                provider: "anthropic".into(),
                model: "native-fixture".into(),
            },
        )
        .unwrap();
        assert_eq!(next.settings.mode, ProviderMode::Anthropic);
        assert_eq!(next.settings.options.provider_id, "anthropic");
        assert!(next.settings.options.oauth.is_none());
        assert!(next.settings.options.oauth_config.is_none());
        assert!(!next.settings.options.api_key_explicit);
        assert_ne!(next.settings.api_key, current.settings.api_key);
        assert_eq!(
            next.settings.base_url,
            ProviderMode::Anthropic.default_base_url()
        );
        assert_eq!(
            next.settings.options.auth_storage_path,
            current.settings.options.auth_storage_path
        );
        let same = configuration_for_identity(
            &current,
            &ModelIdentity {
                provider: "openai-codex".into(),
                model: "another-codex".into(),
            },
        )
        .unwrap();
        assert_eq!(same.settings.api_key, current.settings.api_key);
        assert_eq!(same.settings.base_url, current.settings.base_url);
        assert!(same.settings.options.api_key_explicit);
        assert!(same.settings.options.oauth_config.is_some());
        assert_eq!(
            same.settings.options.oauth.as_ref().unwrap().access,
            current.settings.options.oauth.as_ref().unwrap().access
        );
        assert!(current.settings.options.oauth.is_some());
        assert!(app.store.list().unwrap().is_empty());
    }

    #[test]
    fn restoring_anthropic_from_codex_commits_identity_without_foreign_token_or_auth_io() {
        let fixture = Fixture::new();
        let ctx = egui::Context::default();
        let mut app = fixture_app(&ctx, &fixture);
        app.settings.mode = ProviderMode::OpenAiCodex;
        app.settings.model = "previous-codex".into();
        app.settings.options.provider_id = "openai-codex".into();
        app.settings.api_key = "foreign-explicit-key".into();
        app.settings.options.api_key_explicit = true;
        app.settings.options.oauth = Some(mock_credential());
        app.settings.options.oauth_config =
            Some(NativeOAuthConfig::for_provider("anthropic").unwrap());
        let auth = fixture.0.join("must-not-read-auth.dat");
        std::fs::write(&auth, b"not a valid encrypted credential store").unwrap();
        app.settings.options.auth_storage_path = Some(auth.clone());
        let identity = ModelIdentity {
            provider: "anthropic".into(),
            model: "native-fixture".into(),
        };
        record_model_selection(&mut app.session, &identity).unwrap();
        let (registry, events) = observe_models(false);
        app.extensions = registry;
        app.start_model_restore(&ctx);
        poll_until(&mut app, &ctx, |app| !app.busy());
        assert_eq!(settings_identity(&app.settings), identity);
        assert!(app.settings.options.oauth.is_none());
        assert!(app.settings.options.oauth_config.is_none());
        assert!(!app.settings.options.api_key_explicit);
        assert_eq!(
            std::fs::read(auth).unwrap(),
            b"not a valid encrypted credential store"
        );
        assert!(
            matches!(&events.lock().unwrap()[0], LifecycleEvent::ModelSelect { model, previous_model: Some(previous), source: ModelSelectSource::Restore } if model.provider == "anthropic" && previous.provider == "openai-codex")
        );
        let saved = app.store.list().unwrap().pop().unwrap();
        assert_eq!(stored_model_selection(&saved).unwrap(), Some(identity));
        assert!(saved.context.messages.is_empty());
        let stored = serde_json::to_string(&saved).unwrap();
        for secret in [
            "foreign-explicit-key",
            "fixture-secret",
            "fixture-refresh-secret",
        ] {
            assert!(!stored.contains(secret));
            assert!(!app.status.contains(secret));
        }
    }

    #[test]
    fn startup_restores_known_native_identity_without_credential_or_sts_requests() {
        let fixture = Fixture::new();
        let ctx = egui::Context::default();
        let store = SessionStore::new(fixture.0.join("sessions"));
        let mut session = Session {
            workspace: fixture.0.clone(),
            ..Default::default()
        };
        let record = search("")
            .into_iter()
            .find(|record| record.provider == "amazon-bedrock")
            .unwrap();
        let identity = ModelIdentity {
            provider: record.provider.clone(),
            model: record.id.clone(),
        };
        record_model_selection(&mut session, &identity).unwrap();
        let leaf = session.get_leaf_id().unwrap();
        store.save(&session).unwrap();
        let mut app = DesktopApp::with_store(&ctx, store);
        assert!(app.model_receiver.is_some());
        poll_until(&mut app, &ctx, |app| !app.busy());
        assert_eq!(app.settings.mode, ProviderMode::AmazonBedrock);
        assert_eq!(settings_identity(&app.settings), identity);
        assert_eq!(app.session.get_leaf_id().unwrap(), leaf);
        assert!(app.session.context.messages.is_empty());
        let (registry, events) = observe_models(false);
        app.extensions = registry;
        // A resumed branch can carry a different model than the current process.
        app.settings.model = "previous-model".into();
        app.settings.options.bedrock.profile = "worker-only-role".into();
        app.settings.options.bedrock.config_file =
            Some(fixture.0.join("not read during selection.config"));
        app.settings.options.bedrock.sts_endpoint = "http://127.0.0.1:1/sts".into();
        app.start_model_restore(&ctx);
        poll_until(&mut app, &ctx, |app| !app.busy());
        assert_eq!(events.lock().unwrap().len(), 1);
        assert!(matches!(
            &events.lock().unwrap()[0],
            LifecycleEvent::ModelSelect {
                source: ModelSelectSource::Restore,
                ..
            }
        ));
        assert_eq!(app.settings.options.bedrock.profile, "worker-only-role");
        assert_eq!(
            app.settings.options.bedrock.sts_endpoint,
            "http://127.0.0.1:1/sts"
        );
        assert!(app.session.context.messages.is_empty());
    }

    #[test]
    fn cli_shaped_native_named_demo_identity_restores_without_clearing_or_rewriting_it() {
        let fixture = Fixture::new();
        let ctx = egui::Context::default();
        let store = SessionStore::new(fixture.0.join("sessions"));
        let mut native = Session {
            workspace: fixture.0.clone(),
            ..Default::default()
        };
        // CLI /model demo selected records this same identity-only native state.
        let identity = ModelIdentity {
            provider: "demo".into(),
            model: "selected".into(),
        };
        record_model_selection(&mut native, &identity).unwrap();
        store.save(&native).unwrap();
        let path = fixture
            .0
            .join("sessions")
            .join(format!("{}.json", native.id));
        let before = std::fs::read(&path).unwrap();
        let mut app = DesktopApp::with_store(&ctx, store);
        poll_until(&mut app, &ctx, |app| !app.busy());
        assert_eq!(app.settings.mode, ProviderMode::Demo);
        assert_eq!(app.settings.model, "selected");
        assert_eq!(settings_identity(&app.settings), identity);
        assert_eq!(
            stored_model_selection(&app.session).unwrap().as_ref(),
            Some(&identity)
        );
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert!(app.session.context.messages.is_empty());
        let (registry, events) = observe_models(false);
        app.extensions = registry;
        app.settings.model.clear(); // A different currently applied model before resume.
        app.start_model_restore(&ctx);
        poll_until(&mut app, &ctx, |app| !app.busy());
        assert_eq!(settings_identity(&app.settings), identity);
        assert!(
            matches!(&events.lock().unwrap()[0], LifecycleEvent::ModelSelect {
            model, previous_model: Some(previous), source: ModelSelectSource::Restore,
        } if model == &identity && previous.provider == "demo" && previous.model.is_empty())
        );
        assert_eq!(std::fs::read(path).unwrap(), before);
        assert!(app.session.context.messages.is_empty());
    }

    #[test]
    fn fresh_desktop_restores_native_explicit_openai_protocol_aliases_without_guessing_metadata() {
        for (provider, mode) in [
            ("openai-compatible", ProviderMode::OpenAiCompatible),
            ("openai-responses", ProviderMode::OpenAiResponses),
        ] {
            let fixture = Fixture::new();
            let ctx = egui::Context::default();
            let store = SessionStore::new(fixture.0.join("sessions"));
            let mut native = Session {
                workspace: fixture.0.clone(),
                ..Default::default()
            };
            let identity = ModelIdentity {
                provider: provider.into(),
                model: "explicit-native-model".into(),
            };
            native.metadata.insert(
                "unrelatedEndpoint".into(),
                serde_json::json!("http://127.0.0.1:1/not-inferred"),
            );
            record_model_selection(&mut native, &identity).unwrap();
            store.save(&native).unwrap();
            let path = fixture
                .0
                .join("sessions")
                .join(format!("{}.json", native.id));
            let before = std::fs::read(&path).unwrap();
            let mut app = DesktopApp::with_store(&ctx, store);
            poll_until(&mut app, &ctx, |app| !app.busy());
            assert_eq!(app.settings.mode, mode);
            assert_eq!(settings_identity(&app.settings), identity);
            assert_eq!(app.settings.base_url, mode.default_base_url());
            assert_eq!(
                stored_model_selection(&app.session).unwrap().as_ref(),
                Some(&identity)
            );
            assert_eq!(std::fs::read(path).unwrap(), before);
            assert!(app.session.context.messages.is_empty());
            let unknown = ModelIdentity {
                provider: "unknown-custom-protocol".into(),
                model: "explicit-native-model".into(),
            };
            assert!(configuration_for_identity(&app.model_configuration(), &unknown).is_err());
        }
    }

    #[test]
    fn bedrock_model_draft_selection_never_reads_cache_executes_process_or_connects_auth() {
        let fixture = Fixture::new();
        let ctx = egui::Context::default();
        let mut app = fixture_app(&ctx, &fixture);
        let (registry, events) = observe_models(false);
        app.extensions = registry;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let cache = fixture.0.join("unread SSO cache");
        std::fs::create_dir(&cache).unwrap();
        let cached = cache.join("fixture-session.json");
        std::fs::write(&cached, b"unread malformed cache fixture").unwrap();
        let marker = fixture.0.join("credential-process-must-not-start.pid");
        let manifest = native_process_manifest(&fixture, vec![]);
        let descriptor: serde_json::Value =
            serde_json::from_slice(&std::fs::read(manifest).unwrap()).unwrap();
        let config = fixture.0.join("unread AWS config");
        std::fs::write(&config, format!(
            "[profile fixture-process]\nregion=us-east-1\ncredential_process=\"{}\" \"--pid={}\"\n\
             [profile fixture-sso]\nsso_session=fixture-session\nsso_account_id=123456789012\nsso_role_name=fixture-role\nregion=us-east-1\n\
             [sso-session fixture-session]\nsso_start_url=https://fixture.awsapps.com/start\nsso_region=us-east-1\n",
            descriptor["executable"].as_str().unwrap(), marker.display())).unwrap();
        let config_before = std::fs::read(&config).unwrap();
        let mut configuration = app.model_configuration();
        configuration.settings.mode = ProviderMode::AmazonBedrock;
        configuration.settings.base_url = ProviderMode::AmazonBedrock.default_base_url().into();
        configuration.settings.api_key.clear();
        configuration.settings.options.provider_id = "amazon-bedrock".into();
        configuration.settings.options.bedrock = BedrockOptions {
            region: "us-east-1".into(),
            config_file: Some(config.clone()),
            credentials_file: Some(fixture.0.join("missing credentials")),
            sso_cache_dir: Some(cache.clone()),
            sso_endpoint: endpoint.clone(),
            sso_oidc_endpoint: endpoint.clone(),
            ..Default::default()
        };
        for profile in ["fixture-process", "fixture-sso"] {
            configuration.settings.options.bedrock.profile = profile.into();
            configuration.settings.model = format!("selected-{profile}");
            app.settings_draft = Some(configuration.clone());
            app.start_model_selection(&ctx, configuration.clone(), ModelSelectSource::Set);
            poll_until(&mut app, &ctx, |app| !app.busy());
            assert_eq!(
                app.settings.mode,
                ProviderMode::AmazonBedrock,
                "{}",
                app.status
            );
            assert_eq!(app.settings.options.bedrock.profile, profile);
            assert_eq!(
                app.settings.options.bedrock.sso_cache_dir.as_ref(),
                Some(&cache)
            );
            assert_eq!(app.settings.options.bedrock.sso_endpoint, endpoint);
            assert_eq!(app.settings.options.bedrock.sso_oidc_endpoint, endpoint);
            assert!(
                app.model_worker.is_none() && app.auth_worker.is_none() && app.worker.is_none()
            );
            assert!(!marker.exists());
            assert_eq!(
                std::fs::read(&cached).unwrap(),
                b"unread malformed cache fixture"
            );
            assert_eq!(std::fs::read(&config).unwrap(), config_before);
            assert!(
                matches!(listener.accept(), Err(error) if error.kind()==std::io::ErrorKind::WouldBlock)
            );
            assert!(app.session.context.messages.is_empty());
        }
        assert_eq!(events.lock().unwrap().len(), 2);
        let stored = app.store.list().unwrap().pop().unwrap();
        assert_eq!(
            stored.runtime_state("model-selection").unwrap().unwrap(),
            serde_json::json!({"provider":"amazon-bedrock","model":"selected-fixture-sso"})
        );
        assert_eq!(stored.entries.len(), 2);
        assert!(stored
            .entries
            .iter()
            .all(|entry| entry.entry_type == "runtime_state"));
        let serialized = serde_json::to_string(&stored).unwrap();
        for value in [
            "unread SSO cache",
            "unread AWS config",
            "credential_process",
            endpoint.as_str(),
        ] {
            assert!(!serialized.contains(value));
        }
    }

    #[test]
    fn invalid_sso_endpoint_keeps_bedrock_draft_and_redacts_endpoint_secret() {
        for oidc in [false, true] {
            let fixture = Fixture::new();
            let ctx = egui::Context::default();
            let mut app = fixture_app(&ctx, &fixture);
            let (registry, events) = observe_models(false);
            app.extensions = registry;
            let before = serde_json::to_value(&app.session).unwrap();
            let mut configuration = app.model_configuration();
            configuration.settings.mode = ProviderMode::AmazonBedrock;
            configuration.settings.model = "fixture-model".into();
            configuration.settings.options.provider_id = "amazon-bedrock".into();
            configuration.settings.base_url = ProviderMode::AmazonBedrock.default_base_url().into();
            configuration.settings.options.bedrock.sso_cache_dir =
                Some(fixture.0.join("unread cache"));
            configuration.settings.options.bedrock.sso_endpoint = "http://127.0.0.1:1/sso".into();
            configuration.settings.options.bedrock.sso_oidc_endpoint =
                "http://127.0.0.1:1/oidc".into();
            let invalid = "https://fixture.invalid/?token=do-not-print-endpoint-secret";
            if oidc {
                configuration.settings.options.bedrock.sso_oidc_endpoint = invalid.into();
            } else {
                configuration.settings.options.bedrock.sso_endpoint = invalid.into();
            }
            let options = serde_json::to_value(&configuration.settings.options.bedrock).unwrap();
            app.settings_draft = Some(configuration.clone());
            app.start_model_selection(&ctx, configuration, ModelSelectSource::Set);
            poll_until(&mut app, &ctx, |app| !app.busy());
            assert_eq!(app.settings.mode, ProviderMode::Demo);
            assert_eq!(serde_json::to_value(&app.session).unwrap(), before);
            assert_eq!(
                serde_json::to_value(
                    &app.settings_draft
                        .as_ref()
                        .unwrap()
                        .settings
                        .options
                        .bedrock
                )
                .unwrap(),
                options
            );
            assert!(!app.status.contains("do-not-print-endpoint-secret"));
            assert!(app.store.list().unwrap().is_empty());
            assert!(events.lock().unwrap().is_empty());
        }
    }
}
