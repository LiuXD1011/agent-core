//! Native JSONL RPC. Responses follow the existing id/command/success envelope;
//! stream updates are Rust message snapshots, not the legacy TypeScript delta schema.
use crate::client::{
    compact_with_accounting, load_codex_auth, load_native, prepare_request_input, provider_name,
    Client, ModelSelection,
};
use agent_core::types::{
    timestamp, AgentEvent, CancellationToken, Content, Message, ThinkingLevel, UserMessage,
};
use agent_runtime::catalog::models;
use agent_runtime::extensions::{
    execute_extension_command, extension_context, load_native_extensions, notify_model_select,
    notify_session_info_changed, prepare_session_change, record_model_selection,
    stored_model_selection, ForkPosition, InputSource, InputStreamingBehavior, LifecycleEvent,
    ModelIdentity, ModelSelectSource, SessionBeforeEvent, SessionShutdownReason,
    SessionStartReason, SessionSwitchReason, SharedRegistry, UiBroker, UiReply,
};
use agent_runtime::legacy_session::{load_legacy_session, LeafSelection};
use agent_runtime::provider::{ModelProvider, ProviderMode};
use agent_runtime::runtime::{configured_tools, run_prompt, QueueMode};
use agent_runtime::session::{Session, SessionStore};
use agent_runtime::thinking::{
    notify_thinking_level_select, prepare_thinking_selection, restore_thinking_settings,
    ThinkingSelection,
};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::io::{self, BufRead, Read, Write};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::Path;
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender, TryRecvError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

enum Input {
    Line(String),
    Invalid(String),
    End,
    Event(Box<AgentEvent>),
    Finished(Box<Session>, Result<Option<Value>, String>),
    Reloaded(Result<SharedRegistry, String>),
    Transitioned(
        Box<Session>,
        SessionShutdownReason,
        Result<Box<TransitionConfig>, String>,
    ),
    Prepared(
        Box<Value>,
        InputStreamingBehavior,
        bool,
        Result<Option<Message>, String>,
    ),
    ModelPrepared(
        Box<ModelSelection>,
        mpsc::SyncSender<Result<ModelNotification, String>>,
    ),
    ModelFinished(Result<Option<Value>, String>),
    ThinkingPrepared(
        Box<ThinkingSelection>,
        mpsc::SyncSender<Result<ModelNotification, String>>,
    ),
    BranchPrepared(Result<Box<BranchCandidate>, String>),
    CommandPrepared(Box<Session>, mpsc::SyncSender<Result<Session, String>>),
}

struct BranchCandidate {
    session: Session,
    selection: Option<ModelSelection>,
    draft: Option<String>,
    settings: agent_runtime::provider::ProviderSettings,
}
struct TransitionConfig {
    registry: SharedRegistry,
    selection: Option<ModelSelection>,
    settings: agent_runtime::provider::ProviderSettings,
}

struct ModelNotification {
    session: Session,
    registry: SharedRegistry,
    mode: agent_runtime::extensions::ExtensionMode,
    model: ModelIdentity,
    previous: Option<ModelIdentity>,
    thinking: Option<(ThinkingLevel, ThinkingLevel)>,
}

struct PendingInput {
    command: Value,
    message: Message,
    behavior: InputStreamingBehavior,
    launch: bool,
}
struct ActiveInput {
    signal: CancellationToken,
    thread: Option<JoinHandle<()>>,
}
impl Drop for ActiveInput {
    fn drop(&mut self) {
        self.signal.cancel();
    }
}

struct ActiveRun {
    signal: CancellationToken,
    thread: Option<JoinHandle<()>>,
    command: Option<Value>,
    committed_session: Session,
    compacting: bool,
    extension: bool,
    selecting_model: bool,
}
impl Drop for ActiveRun {
    fn drop(&mut self) {
        self.signal.cancel();
    }
}

fn write_record(output: &mut dyn Write, value: &Value) -> Result<(), String> {
    serde_json::to_writer(&mut *output, value).map_err(|e| e.to_string())?;
    output
        .write_all(b"\n")
        .and_then(|_| output.flush())
        .map_err(|e| e.to_string())
}

fn response(command: &Value, result: Result<Option<Value>, String>) -> Value {
    let mut value = json!({"type":"response","command":command["type"].as_str().unwrap_or("parse"),"success":result.is_ok()});
    if let Some(id) = command.get("id").filter(|id| id.is_string()) {
        value["id"] = id.clone();
    }
    match result {
        Ok(Some(data)) => value["data"] = data,
        Err(error) => value["error"] = json!(error),
        _ => {}
    }
    value
}

fn reject_startup_commands(
    queue: &mut VecDeque<String>,
    output: &mut dyn Write,
    error: &str,
) -> Result<(), String> {
    for line in queue.drain(..) {
        let command: Value = serde_json::from_str(&line).map_err(|error| error.to_string())?;
        write_record(output, &response(&command, Err(error.into())))?;
    }
    Ok(())
}

fn field<'a>(command: &'a Value, key: &str) -> Result<&'a str, String> {
    command
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{key} must be a string"))
}

fn optional_id<'a>(command: &'a Value, key: &str) -> Result<Option<&'a str>, String> {
    match command.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(id)) if !id.is_empty() => Ok(Some(id)),
        _ => Err(format!("{key} must be a non-empty string or null")),
    }
}

fn prompt(command: &Value) -> Result<Message, String> {
    let text = field(command, "message")?;
    let mut content = vec![Content::text(text)];
    if let Some(images) = command.get("images") {
        let images = images.as_array().ok_or("images must be an array")?;
        if images.len() > 16 {
            return Err("At most 16 images per prompt".into());
        }
        for image in images {
            let image: Content =
                serde_json::from_value(image.clone()).map_err(|e| e.to_string())?;
            if !matches!(image, Content::Image { .. }) {
                return Err("images accepts image content only".into());
            }
            content.push(image);
        }
    }
    if text.trim().is_empty() && content.len() == 1 {
        return Err("Prompt is empty".into());
    }
    Ok(Message::User(UserMessage {
        content,
        timestamp: timestamp(),
    }))
}

fn start(
    client: &mut Client,
    message: Message,
    sender: &SyncSender<Input>,
    prepared: bool,
) -> Result<ActiveRun, String> {
    let extension = if prepared {
        None
    } else {
        match &message {
            Message::User(user) => match user.content.as_slice() {
                [Content::Text { text }] => client
                    .extension_command(text)
                    .map(|(name, args)| (name.to_owned(), args.to_owned())),
                _ => None,
            },
            _ => None,
        }
    };
    let is_extension = extension.is_some();
    if !client.session.workspace.is_dir() {
        return Err("Workspace is not a directory".into());
    }
    let mut session = client.session.clone();
    let mut settings = client.settings.clone();
    let auth_dir = client.auth_dir.clone();
    let options = client.options.clone();
    let queues = client.queues.clone();
    let resources = client.resources.clone();
    let before = session.clone();
    let signal = CancellationToken::new();
    let cancellation = signal.clone();
    let sender = sender.clone();
    let worker = thread::Builder::new()
        .name("agent-rpc-run".into())
        .spawn(move || {
            let result = catch_unwind(AssertUnwindSafe(|| {
                if let Some((name, args)) = &extension {
                    let context = extension_context(
                        &session,
                        &options.extensions,
                        options.extension_mode,
                        options.extension_model.clone(),
                    )?;
                    let result = execute_extension_command(
                        &mut session,
                        &options.extensions,
                        name,
                        args,
                        &context,
                        &cancellation,
                    )?;
                    let (ack_sender, ack) = mpsc::sync_channel(1);
                    sender
                        .send(Input::CommandPrepared(
                            Box::new(session.clone()),
                            ack_sender,
                        ))
                        .map_err(|_| "Command host closed")?;
                    match ack.recv() {
                        Ok(Ok(committed)) => session = committed,
                        Ok(Err(error)) => {
                            session = before.clone();
                            return Err(error);
                        }
                        Err(_) => {
                            session = before.clone();
                            return Err("Command host closed".into());
                        }
                    }
                    if result.session_name.is_some() {
                        notify_session_info_changed(
                            &session,
                            &options.extensions,
                            options.extension_mode,
                            options.extension_model.clone(),
                            &cancellation,
                        )
                        .map_err(|error| {
                            format!("Session name committed; notification failed: {error}")
                        })?;
                    }
                    return serde_json::to_value(result)
                        .map(Some)
                        .map_err(|e| e.to_string());
                }
                let message = if prepared {
                    message
                } else {
                    let Some(message) = prepare_request_input(
                        &session,
                        &options,
                        &resources,
                        message,
                        InputSource::Rpc,
                        None,
                        &cancellation,
                    )?
                    else {
                        return Ok(Some(json!({"type":"input_handled"})));
                    };
                    message
                };
                if settings.mode == ProviderMode::OpenAiCodex && settings.options.oauth.is_none() {
                    load_codex_auth(&mut settings, &auth_dir, &cancellation)?;
                }
                settings.validate()?;
                options.automation.validate()?;
                configured_tools(&session.workspace, &options)?;
                run_prompt(
                    &mut session,
                    message,
                    &settings,
                    &options,
                    &queues,
                    &cancellation,
                    &mut |event| {
                        if sender.send(Input::Event(Box::new(event))).is_err() {
                            cancellation.cancel();
                        }
                    },
                )
                .map(|_| None)
            }))
            .unwrap_or_else(|_| Err("Agent worker panicked".into()));
            let _ = sender.send(Input::Finished(Box::new(session), result));
        })
        .map_err(|e| e.to_string())?;
    Ok(ActiveRun {
        signal,
        thread: Some(worker),
        command: None,
        committed_session: client.session.clone(),
        compacting: false,
        extension: is_extension,
        selecting_model: false,
    })
}

fn start_input(
    client: &Client,
    pending: PendingInput,
    sender: &SyncSender<Input>,
) -> Result<ActiveInput, String> {
    let session = client.session.clone();
    let options = client.options.clone();
    let resources = client.resources.clone();
    let signal = CancellationToken::new();
    let cancellation = signal.clone();
    let sender = sender.clone();
    let worker = thread::Builder::new()
        .name("agent-rpc-input-hook".into())
        .spawn(move || {
            let result = catch_unwind(AssertUnwindSafe(|| {
                prepare_request_input(
                    &session,
                    &options,
                    &resources,
                    pending.message,
                    InputSource::Rpc,
                    Some(pending.behavior),
                    &cancellation,
                )
            }))
            .unwrap_or_else(|_| Err("Input worker panicked".into()));
            let _ = sender.send(Input::Prepared(
                Box::new(pending.command),
                pending.behavior,
                pending.launch,
                result,
            ));
        })
        .map_err(|e| e.to_string())?;
    Ok(ActiveInput {
        signal,
        thread: Some(worker),
    })
}

fn start_compaction(
    client: &mut Client,
    command: &Value,
    sender: &SyncSender<Input>,
) -> Result<ActiveRun, String> {
    if client.settings.mode == ProviderMode::Demo {
        return Err("Compaction requires a configured model provider".into());
    }
    let keep = command.get("keepRecentUserTurns").map_or(Ok(2), |value| {
        value
            .as_u64()
            .filter(|value| *value > 0 && *value <= 1024)
            .map(|value| value as usize)
            .ok_or("keepRecentUserTurns must be 1..1024")
    })?;
    let mut session = client.session.clone();
    let mut settings = client.settings.clone();
    let auth_dir = client.auth_dir.clone();
    let options = client.options.clone();
    let signal = CancellationToken::new();
    let cancellation = signal.clone();
    let sender = sender.clone();
    let worker = thread::Builder::new()
        .name("agent-rpc-compaction".into())
        .spawn(move || {
            let result = catch_unwind(AssertUnwindSafe(|| {
                if settings.mode == ProviderMode::OpenAiCodex && settings.options.oauth.is_none() {
                    load_codex_auth(&mut settings, &auth_dir, &cancellation)?;
                }
                settings.validate()?;
                options.automation.validate()?;
                let mut provider = ModelProvider {
                    settings,
                    tool_definitions: vec![],
                };
                compact_with_accounting(&mut session, &mut provider, &options, keep, &cancellation)
            }))
            .unwrap_or_else(|_| Err("Compaction worker panicked".into()))
            .and_then(|result| {
                serde_json::to_value(result)
                    .map(Some)
                    .map_err(|e| e.to_string())
            });
            let _ = sender.send(Input::Finished(Box::new(session), result));
        })
        .map_err(|e| e.to_string())?;
    Ok(ActiveRun {
        signal,
        thread: Some(worker),
        command: Some(command.clone()),
        committed_session: client.session.clone(),
        compacting: true,
        extension: false,
        selecting_model: false,
    })
}

fn start_reload(
    client: &Client,
    command: &Value,
    sender: &SyncSender<Input>,
) -> Result<ActiveRun, String> {
    let paths = client.extension_paths.clone();
    let context = client.extension_context()?;
    let session = client.session.clone();
    let mode = client.options.extension_mode;
    let model = client.options.extension_model.clone();
    let signal = CancellationToken::new();
    let cancellation = signal.clone();
    let sender = sender.clone();
    let worker = thread::Builder::new()
        .name("agent-rpc-extension-reload".into())
        .spawn(move || {
            let result = catch_unwind(AssertUnwindSafe(|| {
                let registry = load_native_extensions(&paths, &context, &cancellation)?;
                let fresh = extension_context(&session, &registry, mode, model)?;
                registry.emit_event_checked(
                    &LifecycleEvent::SessionStart {
                        reason: SessionStartReason::Reload,
                        previous_session_file: None,
                    },
                    &fresh,
                    &cancellation,
                )?;
                if cancellation.is_cancelled() {
                    return Err("Extension reload cancelled".into());
                }
                Ok(registry)
            }))
            .unwrap_or_else(|_| Err("Extension reload worker panicked".into()));
            let _ = sender.send(Input::Reloaded(result));
        })
        .map_err(|e| e.to_string())?;
    Ok(ActiveRun {
        signal,
        thread: Some(worker),
        command: Some(command.clone()),
        committed_session: client.session.clone(),
        compacting: false,
        extension: true,
        selecting_model: false,
    })
}

fn start_transition(
    client: &Client,
    mut session: Session,
    start: SessionStartReason,
    shutdown: SessionShutdownReason,
    command: &Value,
    sender: &SyncSender<Input>,
) -> Result<ActiveRun, String> {
    let paths = client.extension_paths.clone();
    let old = client.options.extensions.clone();
    let mode = client.options.extension_mode;
    let current = client.options.extension_model.clone();
    let selector = client.model_selector();
    let current_settings = client.settings.clone();
    let before = client.session.clone();
    let before_event = match start {
        SessionStartReason::New => Some(SessionBeforeEvent::Switch {
            reason: SessionSwitchReason::New,
            target_session_file: None,
        }),
        SessionStartReason::Resume => Some(SessionBeforeEvent::Switch {
            reason: SessionSwitchReason::Resume,
            target_session_file: if command["type"] == "switch_session" {
                Some(std::path::PathBuf::from(field(command, "sessionPath")?))
            } else {
                client
                    .store
                    .as_ref()
                    .map(|store| store.path(&session.id))
                    .transpose()?
            },
        }),
        _ => None,
    };
    let signal = CancellationToken::new();
    let cancellation = signal.clone();
    let sender = sender.clone();
    let worker = thread::Builder::new()
        .name("agent-rpc-session-transition".into())
        .spawn(move || {
            let result = catch_unwind(AssertUnwindSafe(|| {
                if let Some(event) = &before_event {
                    if !prepare_session_change(
                        &before,
                        &old,
                        mode,
                        current.clone(),
                        event,
                        &cancellation,
                    )? {
                        return Err("Session change cancelled by extension".into());
                    }
                }
                let mut selection = stored_model_selection(&session)?
                    .filter(|model| Some(model) != current.as_ref())
                    .map(|model| selector.prepare(&model.provider, &model.model, &cancellation))
                    .transpose()?;
                let mut settings = selection
                    .as_ref()
                    .map_or(current_settings, |selection| selection.settings.clone());
                restore_thinking_settings(&session, &mut settings)?;
                if let Some(level) = settings.options.thinking_level {
                    session.append_thinking_level(level)?;
                }
                if let Some(selection) = &mut selection {
                    selection.settings = settings.clone();
                }
                let model = selection
                    .as_ref()
                    .map(|selection| ModelIdentity {
                        provider: selection.provider_id.clone(),
                        model: selection.settings.model.clone(),
                    })
                    .or(current);
                let context = extension_context(&session, &old, mode, model.clone())?;
                let registry = if paths.is_empty() {
                    old
                } else {
                    load_native_extensions(&paths, &context, &cancellation)?
                };
                let fresh = extension_context(&session, &registry, mode, model)?;
                registry.emit_event_checked(
                    &LifecycleEvent::SessionStart {
                        reason: start,
                        previous_session_file: None,
                    },
                    &fresh,
                    &cancellation,
                )?;
                if cancellation.is_cancelled() {
                    return Err("Session transition cancelled".into());
                }
                Ok(Box::new(TransitionConfig {
                    registry,
                    selection,
                    settings,
                }))
            }))
            .unwrap_or_else(|_| Err("Session transition worker panicked".into()));
            let _ = sender.send(Input::Transitioned(Box::new(session), shutdown, result));
        })
        .map_err(|e| e.to_string())?;
    Ok(ActiveRun {
        signal,
        thread: Some(worker),
        command: Some(command.clone()),
        committed_session: client.session.clone(),
        compacting: false,
        extension: true,
        selecting_model: false,
    })
}

fn start_model_selection(
    client: &Client,
    command: &Value,
    sender: &SyncSender<Input>,
) -> Result<ActiveRun, String> {
    let provider = field(command, "provider")?.to_owned();
    let model = field(command, "modelId")?.to_owned();
    let selector = client.model_selector();
    let signal = CancellationToken::new();
    let cancellation = signal.clone();
    let sender = sender.clone();
    let worker=thread::Builder::new().name("agent-rpc-model-selection".into()).spawn(move||{
        let result=catch_unwind(AssertUnwindSafe(||{
            let selection=selector.prepare(&provider,&model,&cancellation)?;
            let (ack_sender,ack)=mpsc::sync_channel(1);
            sender.send(Input::ModelPrepared(Box::new(selection),ack_sender)).map_err(|_|"Model selection host closed")?;
            let notification=loop{
                match ack.recv_timeout(Duration::from_millis(25)){
                    Ok(value)=>break value?,
                    Err(RecvTimeoutError::Disconnected)=>return Err("Model selection host closed".into()),
                    // The host owns the commit boundary and always acknowledges it,
                    // including cancellation; do not race a filesystem commit here.
                    Err(RecvTimeoutError::Timeout)=>{}
                }
            };
            if let Some((level,previous))=notification.thinking {notify_thinking_level_select(&notification.session,&notification.registry,notification.mode,Some(notification.model.clone()),level,previous,&cancellation).map_err(|error|format!("Selection committed; thinking notification failed: {error}"))?;}
            notify_model_select(&notification.session,&notification.registry,notification.mode,notification.model.clone(),notification.previous,ModelSelectSource::Set,&cancellation).map_err(|error|format!("Model selection committed; notification failed: {error}"))?;
            Ok(Some(json!({"provider":notification.model.provider,"id":notification.model.model,"committed":true})))
        })).unwrap_or_else(|_|Err("Model selection worker panicked".into()));
        let _=sender.send(Input::ModelFinished(result));
    }).map_err(|error|error.to_string())?;
    Ok(ActiveRun {
        signal,
        thread: Some(worker),
        command: Some(command.clone()),
        committed_session: client.session.clone(),
        compacting: false,
        extension: false,
        selecting_model: true,
    })
}

fn start_notification(
    client: &mut Client,
    command: &Value,
    data: Value,
    previous: Option<ModelIdentity>,
    source: ModelSelectSource,
    sender: &SyncSender<Input>,
) -> Result<ActiveRun, String> {
    let signal = CancellationToken::new();
    let cancellation = signal.clone();
    let session = client.session.clone();
    let registry = client.options.extensions.clone();
    let mode = client.options.extension_mode;
    let model = ModelIdentity {
        provider: client.provider_id.clone(),
        model: client.settings.model.clone(),
    };
    let startup =
        (command["type"] == "startup_model_restore").then(|| client.take_startup_notifications());
    let thinking = if startup.is_none() {
        client.take_thinking_notification()
    } else {
        None
    };
    let sender = sender.clone();
    let worker = thread::Builder::new()
        .name("agent-rpc-model-notification".into())
        .spawn(move || {
            let result = catch_unwind(AssertUnwindSafe(|| {
                if let Some(startup) = startup {
                    startup.complete(&cancellation)?;
                    return Ok(Some(data));
                }
                if let Some((level, previous)) = thinking {
                    notify_thinking_level_select(
                        &session,
                        &registry,
                        mode,
                        Some(model.clone()),
                        level,
                        previous,
                        &cancellation,
                    )
                    .map_err(|error| {
                        format!("Selection committed; thinking notification failed: {error}")
                    })?;
                }
                if !model.model.is_empty() || model.provider == "demo" {
                    notify_model_select(
                        &session,
                        &registry,
                        mode,
                        model,
                        previous,
                        source,
                        &cancellation,
                    )
                    .map_err(|error| {
                        format!("Selection committed; model notification failed: {error}")
                    })?;
                }
                Ok(Some(data))
            }))
            .unwrap_or_else(|_| Err("Model notification worker panicked".into()));
            let _ = sender.send(Input::ModelFinished(result));
        })
        .map_err(|error| error.to_string())?;
    Ok(ActiveRun {
        signal,
        thread: Some(worker),
        command: Some(command.clone()),
        committed_session: client.session.clone(),
        compacting: false,
        extension: false,
        selecting_model: true,
    })
}

fn start_thinking_selection(
    client: &Client,
    command: &Value,
    level: ThinkingLevel,
    sender: &SyncSender<Input>,
) -> Result<ActiveRun, String> {
    let session = client.session.clone();
    let settings = client.settings.clone();
    let signal = CancellationToken::new();
    let cancellation = signal.clone();
    let sender = sender.clone();
    let worker = thread::Builder::new()
        .name("agent-rpc-thinking-selection".into())
        .spawn(move || {
            let result = catch_unwind(AssertUnwindSafe(|| {
                let selection =
                    prepare_thinking_selection(&session, &settings, level, &cancellation)?;
                let (ack_sender, ack) = mpsc::sync_channel(1);
                sender
                    .send(Input::ThinkingPrepared(Box::new(selection), ack_sender))
                    .map_err(|_| "Thinking selection host closed")?;
                let notification = ack.recv().map_err(|_| "Thinking selection host closed")??;
                let selected = notification
                    .session
                    .get_thinking_level()?
                    .unwrap_or_default();
                if let Some((level, previous)) = notification.thinking {
                    notify_thinking_level_select(
                        &notification.session,
                        &notification.registry,
                        notification.mode,
                        Some(notification.model),
                        level,
                        previous,
                        &cancellation,
                    )
                    .map_err(|error| {
                        format!("Thinking selection committed; notification failed: {error}")
                    })?;
                }
                Ok(Some(json!({"level":selected,"committed":true})))
            }))
            .unwrap_or_else(|_| Err("Thinking selection worker panicked".into()));
            let _ = sender.send(Input::ModelFinished(result));
        })
        .map_err(|error| error.to_string())?;
    Ok(ActiveRun {
        signal,
        thread: Some(worker),
        command: Some(command.clone()),
        committed_session: client.session.clone(),
        compacting: false,
        extension: false,
        selecting_model: true,
    })
}

fn start_branch_selection(
    client: &Client,
    command: &Value,
    sender: &SyncSender<Input>,
) -> Result<ActiveRun, String> {
    let mut session = client.session.clone();
    let selector = client.model_selector();
    let current_settings = client.settings.clone();
    let current = ModelIdentity {
        provider: client.provider_id.clone(),
        model: client.settings.model.clone(),
    };
    let registry = client.options.extensions.clone();
    let mode = client.options.extension_mode;
    let fork = command["type"] == "fork";
    let target = if fork {
        Some(field(command, "entryId")?.to_owned())
    } else {
        optional_id(command, "entryId")?.map(str::to_owned)
    };
    let signal = CancellationToken::new();
    let cancellation = signal.clone();
    let sender = sender.clone();
    let worker = thread::Builder::new()
        .name("agent-rpc-branch-selection".into())
        .spawn(move || {
            let result = catch_unwind(AssertUnwindSafe(|| {
                let draft = if fork {
                    if !prepare_session_change(
                        &session,
                        &registry,
                        mode,
                        Some(current.clone()),
                        &SessionBeforeEvent::Fork {
                            entry_id: target.clone().ok_or("entryId is required")?,
                            position: ForkPosition::Before,
                        },
                        &cancellation,
                    )? {
                        return Err("Session change cancelled by extension".into());
                    }
                    Some(session.fork(target.as_deref().ok_or("entryId is required")?)?)
                } else {
                    session.navigate(target.as_deref())?;
                    None
                };
                let mut selection = stored_model_selection(&session)?
                    .filter(|model| model != &current)
                    .map(|model| selector.prepare(&model.provider, &model.model, &cancellation))
                    .transpose()?;
                let mut settings = selection
                    .as_ref()
                    .map_or(current_settings, |selection| selection.settings.clone());
                restore_thinking_settings(&session, &mut settings)?;
                if let Some(level) = settings.options.thinking_level {
                    session.append_thinking_level(level)?;
                }
                if let Some(selection) = &mut selection {
                    selection.settings = settings.clone();
                }
                if cancellation.is_cancelled() {
                    return Err("Branch selection aborted before commit".into());
                }
                Ok(Box::new(BranchCandidate {
                    session,
                    selection,
                    draft,
                    settings,
                }))
            }))
            .unwrap_or_else(|_| Err("Branch selection worker panicked".into()));
            let _ = sender.send(Input::BranchPrepared(result));
        })
        .map_err(|error| error.to_string())?;
    Ok(ActiveRun {
        signal,
        thread: Some(worker),
        command: Some(command.clone()),
        committed_session: client.session.clone(),
        compacting: false,
        extension: false,
        selecting_model: true,
    })
}

fn start_name_notification(
    client: &Client,
    command: &Value,
    sender: &SyncSender<Input>,
) -> Result<ActiveRun, String> {
    let session = client.session.clone();
    let registry = client.options.extensions.clone();
    let mode = client.options.extension_mode;
    let model = client.options.extension_model.clone();
    let signal = CancellationToken::new();
    let cancellation = signal.clone();
    let sender = sender.clone();
    let worker = thread::Builder::new()
        .name("agent-rpc-session-name-notification".into())
        .spawn(move || {
            let result = catch_unwind(AssertUnwindSafe(|| {
                notify_session_info_changed(&session, &registry, mode, model, &cancellation)
                    .map_err(|error| {
                        format!("Session name committed; notification failed: {error}")
                    })?;
                Ok(Some(
                    json!({"sessionName":session.get_session_name()?,"committed":true}),
                ))
            }))
            .unwrap_or_else(|_| Err("Session name notification worker panicked".into()));
            let _ = sender.send(Input::ModelFinished(result));
        })
        .map_err(|error| error.to_string())?;
    Ok(ActiveRun {
        signal,
        thread: Some(worker),
        command: Some(command.clone()),
        committed_session: client.session.clone(),
        compacting: false,
        extension: true,
        selecting_model: false,
    })
}

fn reader(mut input: impl BufRead, sender: SyncSender<Input>) {
    loop {
        let mut line = Vec::new();
        match input.by_ref().take(1_048_577).read_until(b'\n', &mut line) {
            Ok(0) => {
                let _ = sender.send(Input::End);
                return;
            }
            Ok(_) if line.len() > 1_048_576 => {
                // Discard the rest of this single LF-framed record, without allocating it.
                if !line.ends_with(b"\n") {
                    while let Ok(buffer) = input.fill_buf() {
                        if buffer.is_empty() {
                            break;
                        }
                        let newline = buffer.iter().position(|byte| *byte == b'\n');
                        let consumed = newline.map_or(buffer.len(), |index| index + 1);
                        input.consume(consumed);
                        if newline.is_some() {
                            break;
                        }
                    }
                }
                if sender
                    .send(Input::Invalid("RPC record exceeds 1MB".into()))
                    .is_err()
                {
                    return;
                }
            }
            Ok(_) => match String::from_utf8(line) {
                Ok(line) => {
                    if !line.trim().is_empty() && sender.send(Input::Line(line)).is_err() {
                        return;
                    }
                }
                Err(error) => {
                    if sender
                        .send(Input::Invalid(format!("Invalid UTF-8: {error}")))
                        .is_err()
                    {
                        return;
                    }
                }
            },
            Err(error) => {
                let _ = sender.send(Input::Invalid(error.to_string()));
                let _ = sender.send(Input::End);
                return;
            }
        }
    }
}

struct GracefulOutput<'a> {
    output: &'a mut dyn Write,
    signal: CancellationToken,
    error: Option<String>,
}

struct RpcUi(UiBroker);
impl Drop for RpcUi {
    fn drop(&mut self) {
        self.0.close();
    }
}

fn cleanup_signal() -> CancellationToken {
    let signal = CancellationToken::new();
    signal.cancel();
    signal
}

impl Write for GracefulOutput<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.error.is_some() {
            return Ok(bytes.len());
        }
        match self.output.write(bytes) {
            Ok(size) if size > 0 || bytes.is_empty() => Ok(size),
            result => {
                self.error = Some(match result {
                    Err(error) => error.to_string(),
                    _ => "Output stream closed".into(),
                });
                self.signal.cancel();
                Ok(bytes.len())
            }
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        if self.error.is_none() {
            if let Err(error) = self.output.flush() {
                self.error = Some(error.to_string());
                self.signal.cancel();
            }
        }
        Ok(())
    }
}

pub fn run_rpc(
    input: impl BufRead + Send + 'static,
    output: &mut dyn Write,
    mut client: Client,
    interrupt: &CancellationToken,
) -> Result<(), String> {
    client.options.extension_mode = agent_runtime::extensions::ExtensionMode::Rpc;
    client.session.ensure_tree()?;
    let ui = RpcUi(UiBroker::new());
    client.options.extensions.bind_ui(ui.0.handle())?;
    let mut output = GracefulOutput {
        output,
        signal: interrupt.clone(),
        error: None,
    };
    let result = run_rpc_inner(input, &mut output, client, interrupt, &ui.0);
    if let Some(error) = output.error {
        Err(format!("RPC output failed: {error}"))
    } else {
        result
    }
}

fn run_rpc_inner(
    input: impl BufRead + Send + 'static,
    output: &mut dyn Write,
    mut client: Client,
    interrupt: &CancellationToken,
    ui: &UiBroker,
) -> Result<(), String> {
    let (sender, receiver) = mpsc::sync_channel(128);
    let reader_sender = sender.clone();
    thread::Builder::new()
        .name("agent-rpc-input".into())
        .spawn(move || reader(input, reader_sender))
        .map_err(|e| e.to_string())?;
    let mut startup = client.model_notification_pending || client.session_start_pending;
    let mut startup_lines: VecDeque<String> = VecDeque::new();
    let mut active: Option<ActiveRun> = if startup {
        Some(start_notification(
            &mut client,
            &json!({"type":"startup_model_restore"}),
            Value::Null,
            None,
            ModelSelectSource::Restore,
            &sender,
        )?)
    } else {
        None
    };
    let mut preparing: Option<ActiveInput> = None;
    let mut pending_inputs: VecDeque<PendingInput> = VecDeque::new();
    let mut ended = false;
    let mut stopping = false;
    let mut settled: Option<Result<(), String>> = None;
    while !ended || active.is_some() || preparing.is_some() {
        for request in ui.drain_requests() {
            if ui.is_pending(&request.id) {
                let mut request =
                    serde_json::to_value(request).map_err(|error| error.to_string())?;
                request["type"] = json!("extension_ui_request");
                write_record(output, &request)?;
            }
        }
        for diagnostic in client.options.extensions.drain_diagnostics() {
            write_record(
                output,
                &json!({"type":"extension_error","diagnostic":diagnostic}),
            )?;
        }
        for event in client.options.automation_state.drain_events() {
            write_record(
                output,
                &serde_json::to_value(event).map_err(|error| error.to_string())?,
            )?;
        }
        if interrupt.is_cancelled() {
            ended = true;
            ui.close();
            client.queues.clear();
            if let Some(input) = &preparing {
                input.signal.cancel();
            }
            for pending in pending_inputs.drain(..) {
                write_record(
                    output,
                    &response(&pending.command, Err("Input preparation aborted".into())),
                )?;
            }
            reject_startup_commands(&mut startup_lines, output, "Startup commands aborted")?;
            if let Some(active) = &active {
                active.signal.cancel();
            } else if preparing.is_none() {
                break;
            }
        }
        let replay_ready = !startup
            && !ended
            && active.is_none()
            && preparing.is_none()
            && pending_inputs.is_empty()
            && !startup_lines.is_empty();
        let (input, replayed) = if replay_ready {
            // Live abort/EOF/UI/query records must remain readable while accepted
            // startup commands replay one at a time across asynchronous callbacks.
            match receiver.try_recv() {
                Ok(input) => (input, false),
                Err(TryRecvError::Empty) => (
                    Input::Line(startup_lines.pop_front().expect("nonempty startup queue")),
                    true,
                ),
                Err(error) => return Err(error.to_string()),
            }
        } else {
            match receiver.recv_timeout(Duration::from_millis(50)) {
                Ok(input) => (input, false),
                Err(RecvTimeoutError::Timeout) => continue,
                Err(error) => return Err(error.to_string()),
            }
        };
        match input {
            Input::BranchPrepared(result) => {
                let mut worker = active
                    .take()
                    .ok_or("Unexpected branch preparation completion")?;
                if let Some(thread) = worker.thread.take() {
                    thread
                        .join()
                        .map_err(|_| "Branch selection worker panicked")?;
                }
                let command = worker.command.as_ref().ok_or("Missing branch command")?;
                let result=result.and_then(|candidate|{
                    if ended||interrupt.is_cancelled()||worker.signal.is_cancelled(){return Err("Branch selection aborted before commit".into());}
                    if let Some(store)=&client.store{store.save(&candidate.session)?;}
                    let previous=Some(ModelIdentity{provider:client.provider_id.clone(),model:client.settings.model.clone()});
                    client.session=candidate.session;
                    if let Some(selection)=candidate.selection{client.apply_model_selection(selection);}else{client.apply_thinking_settings(candidate.settings);}
                    client.queues.clear();client.refresh_resources();
                    let mut data=json!({"leafId":client.session.get_leaf_id()?,"cancelled":false,"committed":true});
                    if let Some(draft)=candidate.draft{data["text"]=json!(draft);}
                    Ok((data,previous))
                });
                match result {
                    Ok((data, previous)) => {
                        active = Some(start_notification(
                            &mut client,
                            command,
                            data,
                            previous,
                            ModelSelectSource::Restore,
                            &sender,
                        )?)
                    }
                    Err(error) => {
                        if error == "Session change cancelled by extension" {
                            write_record(
                                output,
                                &response(command, Ok(Some(json!({"cancelled":true})))),
                            )?;
                            settled = Some(Ok(()));
                        } else {
                            write_record(output, &response(command, Err(error.clone())))?;
                            settled = Some(Err(error));
                        }
                    }
                }
            }
            Input::ModelPrepared(selection, ack) => {
                let worker = active
                    .as_ref()
                    .ok_or("Unexpected model preparation completion")?;
                let result = if ended || interrupt.is_cancelled() || worker.signal.is_cancelled() {
                    Err("Model selection aborted before commit".into())
                } else {
                    client
                        .commit_model_selection(*selection, &worker.signal)
                        .map(|previous| ModelNotification {
                            session: client.session.clone(),
                            registry: client.options.extensions.clone(),
                            mode: client.options.extension_mode,
                            model: ModelIdentity {
                                provider: client.provider_id.clone(),
                                model: client.settings.model.clone(),
                            },
                            previous,
                            thinking: client.take_thinking_notification(),
                        })
                };
                if result.is_ok() {
                    if let Some(worker) = &mut active {
                        worker.committed_session = client.session.clone();
                    }
                    write_record(
                        output,
                        &json!({"type":"model_selected","model":{"provider":client.provider_id,"id":client.settings.model},"source":"set","committed":true}),
                    )?;
                }
                let _ = ack.send(result);
            }
            Input::ThinkingPrepared(selection, ack) => {
                let worker = active.as_ref().ok_or("Unexpected thinking preparation")?;
                let result = if ended || interrupt.is_cancelled() || worker.signal.is_cancelled() {
                    Err("Thinking selection aborted before commit".into())
                } else {
                    client
                        .commit_thinking_selection(*selection, &worker.signal)
                        .map(|()| ModelNotification {
                            session: client.session.clone(),
                            registry: client.options.extensions.clone(),
                            mode: client.options.extension_mode,
                            model: ModelIdentity {
                                provider: client.provider_id.clone(),
                                model: client.settings.model.clone(),
                            },
                            previous: None,
                            thinking: client.take_thinking_notification(),
                        })
                };
                if result.is_ok() {
                    if let Some(worker) = &mut active {
                        worker.committed_session = client.session.clone();
                    }
                    write_record(
                        output,
                        &json!({"type":"thinking_level_changed","level":client.thinking_level(),"committed":true}),
                    )?;
                }
                let _ = ack.send(result);
            }
            Input::ModelFinished(result) => {
                let mut worker = active
                    .take()
                    .ok_or("Unexpected model selection completion")?;
                if let Some(thread) = worker.thread.take() {
                    thread
                        .join()
                        .map_err(|_| "Model selection worker panicked")?;
                }
                let result = result.map(|data| {
                    data.map(|mut data| {
                        if data.get("provider").is_some() {
                            data["api"] = json!(provider_name(&client.settings.mode));
                        }
                        data
                    })
                });
                if worker
                    .command
                    .as_ref()
                    .is_some_and(|command| command["type"] == "startup_model_restore")
                {
                    startup = false;
                    if result.is_err()
                        || ended
                        || worker.signal.is_cancelled()
                        || interrupt.is_cancelled()
                    {
                        reject_startup_commands(
                            &mut startup_lines,
                            output,
                            "Startup model notification aborted",
                        )?;
                        settled = Some(match result {
                            Err(error) => Err(error),
                            Ok(_) => Err("Startup model notification aborted".into()),
                        });
                    }
                } else {
                    write_record(
                        output,
                        &response(
                            worker.command.as_ref().ok_or("Missing model command")?,
                            result.clone(),
                        ),
                    )?;
                    if result.is_err()
                        || !worker
                            .command
                            .as_ref()
                            .is_some_and(|command| command["type"] == "set_session_name")
                    {
                        settled = Some(result.map(|_| ()));
                    }
                }
            }
            Input::Prepared(command, behavior, launch, result) => {
                let mut worker = preparing.take().ok_or("Unexpected input completion")?;
                if let Some(handle) = worker.thread.take() {
                    handle.join().map_err(|_| "Input worker panicked")?;
                }
                let result = if worker.signal.is_cancelled() || ended || interrupt.is_cancelled() {
                    Err("Input preparation aborted".into())
                } else {
                    result
                };
                let result = result.map(|message| {
                    if let Some(message) = message {
                        match behavior {
                            InputStreamingBehavior::Steer => client.queues.steer(message),
                            InputStreamingBehavior::FollowUp => client.queues.follow_up(message),
                        }
                        None
                    } else {
                        Some(json!({"handled":true}))
                    }
                });
                let success = result.is_ok();
                if let Err(error) = &result {
                    if !matches!(settled, Some(Err(_))) {
                        settled = Some(Err(error.clone()));
                    }
                } else {
                    settled.get_or_insert(Ok(()));
                }
                if matches!(&result,Ok(Some(data))if data["handled"]==true) {
                    write_record(output, &json!({"type":"input_handled"}))?;
                }
                write_record(output, &response(&command, result))?;
                if !ended {
                    if success && !stopping && launch && active.is_none() {
                        if let Some(message) = client.queues.pop_next() {
                            active = Some(start(&mut client, message, &sender, true)?);
                        }
                    }
                    if let Some(pending) = pending_inputs.pop_front() {
                        preparing = Some(start_input(&client, pending, &sender)?);
                    }
                }
            }
            Input::Transitioned(session, shutdown, result) => {
                let mut worker = active
                    .take()
                    .ok_or("Unexpected session transition completion")?;
                if let Some(handle) = worker.thread.take() {
                    handle
                        .join()
                        .map_err(|_| "Session transition worker panicked")?;
                }
                let result = result.and_then(|config| {
                    if ended||interrupt.is_cancelled()||worker.signal.is_cancelled(){return Err("Session transition cancelled before commit".into());}
                    if let Some(store)=&client.store{store.save(&client.session)?;store.save(&session)?;}
                    let previous=Some(ModelIdentity{provider:client.provider_id.clone(),model:client.settings.model.clone()});
                    client.emit_lifecycle(
                        LifecycleEvent::SessionShutdown { reason: shutdown, target_session_file: None },
                        &cleanup_signal(),
                    )?;
                    client.session = *session;
                    client.options.extensions = config.registry;
                    if let Some(selection)=config.selection{client.apply_model_selection(selection);}else{client.apply_thinking_settings(config.settings);}
                    client.queues.clear();
                    client.refresh_resources();
                    Ok((json!({"sessionId": client.session.id, "leafId": client.session.get_leaf_id()?, "cancelled": false,"committed":true}),previous))
                });
                let command = worker
                    .command
                    .as_ref()
                    .ok_or("Missing transition command")?;
                match result {
                    Ok((data, previous)) => {
                        active = Some(start_notification(
                            &mut client,
                            command,
                            data,
                            previous,
                            ModelSelectSource::Restore,
                            &sender,
                        )?)
                    }
                    Err(error) => {
                        if error == "Session change cancelled by extension" {
                            write_record(
                                output,
                                &response(command, Ok(Some(json!({"cancelled":true})))),
                            )?;
                            settled = Some(Ok(()));
                        } else {
                            write_record(output, &response(command, Err(error.clone())))?;
                            settled = Some(Err(error));
                        }
                    }
                }
            }
            Input::Reloaded(result) => {
                let mut worker = active
                    .take()
                    .ok_or("Unexpected extension reload completion")?;
                if let Some(handle) = worker.thread.take() {
                    handle
                        .join()
                        .map_err(|_| "Extension reload worker panicked")?;
                }
                let result=result.and_then(|registry|{
                    if ended||interrupt.is_cancelled()||worker.signal.is_cancelled(){return Err("Extension reload cancelled".into());}
                    client.emit_lifecycle(LifecycleEvent::SessionShutdown{reason:SessionShutdownReason::Reload,target_session_file:None},&cleanup_signal())?;
                    client.options.extensions=registry;
                    Ok(json!({"extensions":client.options.extensions.ids(),"commands":client.options.extensions.commands(),"committed":true}))
                });
                let command = worker.command.as_ref().ok_or("Missing reload command")?;
                match result {
                    Ok(data) => {
                        active = Some(start_notification(
                            &mut client,
                            command,
                            data,
                            None,
                            ModelSelectSource::Restore,
                            &sender,
                        )?)
                    }
                    Err(error) => {
                        write_record(output, &response(command, Err(error.clone())))?;
                        settled = Some(Err(error));
                    }
                }
            }
            Input::End => {
                ended = true;
                ui.close();
                client.queues.clear();
                if let Some(input) = &preparing {
                    input.signal.cancel();
                }
                for pending in pending_inputs.drain(..) {
                    write_record(
                        output,
                        &response(&pending.command, Err("Input preparation aborted".into())),
                    )?;
                }
                reject_startup_commands(&mut startup_lines, output, "Startup commands aborted")?;
                if let Some(active) = &active {
                    active.signal.cancel();
                }
            }
            Input::Invalid(error) => {
                write_record(output, &response(&json!({"type":"parse"}), Err(error)))?
            }
            Input::Event(event) => {
                if let AgentEvent::MessageEnd { message } = &*event {
                    client.session.context.messages.push(message.clone());
                }
                write_record(
                    output,
                    &serde_json::to_value(event).map_err(|e| e.to_string())?,
                )?;
            }
            Input::CommandPrepared(session, ack) => {
                let worker = active.as_mut().ok_or("Unexpected command preparation")?;
                let committed = (|| {
                    if ended || interrupt.is_cancelled() || worker.signal.is_cancelled() {
                        return Err("Extension command cancelled before commit".into());
                    }
                    if let Some(store) = &client.store {
                        store.save(&session)?;
                    }
                    client.session = *session;
                    worker.committed_session = client.session.clone();
                    Ok(client.session.clone())
                })();
                let _ = ack.send(committed);
            }
            Input::Finished(session, result) => {
                client.session = *session;
                let mut deferred = None;
                if let Some(mut worker) = active.take() {
                    deferred = worker.command.take();
                    if let Some(handle) = worker.thread.take() {
                        handle.join().map_err(|_| "Agent worker panicked")?;
                    }
                }
                let result = client.save().and(result);
                for event in client.options.automation_state.drain_events() {
                    write_record(
                        output,
                        &serde_json::to_value(event).map_err(|error| error.to_string())?,
                    )?;
                }
                if let Some(command) = deferred {
                    write_record(output, &response(&command, result.clone()))?;
                } else if let Ok(Some(data)) = &result {
                    if data["type"] == "input_handled" {
                        write_record(output, data)?;
                    } else {
                        write_record(
                            output,
                            &json!({"type":"extension_command_result","result":data}),
                        )?;
                    }
                }
                if let Err(error) = &result {
                    settled = Some(Err(error.clone()));
                    stopping = true;
                    client.queues.clear();
                    if let Some(input) = &preparing {
                        input.signal.cancel();
                    }
                    for pending in pending_inputs.drain(..) {
                        write_record(
                            output,
                            &response(
                                &pending.command,
                                Err("Input preparation aborted after request failure".into()),
                            ),
                        )?;
                    }
                } else {
                    settled.get_or_insert(Ok(()));
                }
                if !ended && result.is_ok() {
                    if let Some(message) = client.queues.pop_next() {
                        active = Some(start(&mut client, message, &sender, true)?);
                    }
                }
            }
            Input::Line(line) => {
                if ended {
                    continue;
                }
                let command: Value = match serde_json::from_str(&line) {
                    Ok(value) => value,
                    Err(error) => {
                        write_record(
                            output,
                            &response(&json!({"type":"parse"}), Err(error.to_string())),
                        )?;
                        continue;
                    }
                };
                if !command.is_object()
                    || !command["type"].is_string()
                    || command.get("id").is_some_and(|id| !id.is_string())
                {
                    write_record(
                        output,
                        &response(
                            &json!({"type":"parse"}),
                            Err("Command requires type:string and optional id:string".into()),
                        ),
                    )?;
                    continue;
                }
                let kind = command["type"].as_str().unwrap();
                if startup
                    && !matches!(
                        kind,
                        "abort"
                            | "extension_ui_response"
                            | "shutdown"
                            | "initialize"
                            | "get_state"
                            | "get_tree"
                            | "get_entries"
                            | "get_branch"
                            | "get_extensions"
                            | "get_commands"
                            | "get_available_models"
                            | "get_automation_state"
                            | "get_session_stats"
                            | "get_messages"
                            | "get_last_assistant_text"
                    )
                {
                    if startup_lines.len() >= 32 {
                        write_record(
                            output,
                            &response(
                                &command,
                                Err("Startup pending queue contains 32 commands".into()),
                            ),
                        )?;
                    } else {
                        startup_lines.push_back(line);
                    }
                    continue;
                }
                let mut deferred = false;
                let replay_busy = !startup && !replayed && !startup_lines.is_empty();
                let idle_required = matches!(
                    kind,
                    "new_session"
                        | "switch_session"
                        | "clone"
                        | "set_model"
                        | "set_session_name"
                        | "set_thinking_level"
                        | "cycle_thinking_level"
                        | "import_session"
                        | "compact"
                        | "navigate"
                        | "fork"
                        | "clone_session"
                        | "export_html"
                        | "export_branch_jsonl"
                        | "extension_command"
                        | "reload_extensions"
                );
                let result = if stopping && matches!(kind, "prompt" | "steer" | "follow_up") {
                    Err("Agent is stopping; wait for agent_settled".into())
                } else if matches!(kind, "prompt" | "steer" | "follow_up")
                    && (replay_busy
                        || active.as_ref().is_some_and(|run| {
                            run.extension || run.compacting || run.selecting_model
                        }))
                {
                    Err(
                        "RPC operation is running; wait for agent_settled before sending input"
                            .into(),
                    )
                } else if idle_required
                    && (replay_busy
                        || active.is_some()
                        || preparing.is_some()
                        || !pending_inputs.is_empty())
                {
                    Err("Agent is running; abort and wait for agent_settled".into())
                } else {
                    (|| -> Result<Option<Value>, String> {
                        match kind {
                            "extension_ui_response" => {
                                let id = field(&command, "id")?;
                                let mut fields =
                                    command.as_object().expect("validated RPC object").clone();
                                fields.remove("type");
                                fields.remove("id");
                                let reply: UiReply = serde_json::from_value(Value::Object(fields))
                                    .map_err(|error| format!("Invalid UI response: {error}"))?;
                                ui.respond(id, reply)?;
                                Ok(None)
                            }
                            "initialize" => Ok(Some(
                                json!({"protocol":"agent-core-rust","version":1,"messageUpdates":"snapshots","providers":["demo","openai","openai-responses","openai-codex","anthropic","google","azure-openai-responses","google-vertex","amazon-bedrock","mistral","pi-messages"],"commands":["prompt","steer","follow_up","abort","clear_queue","get_state","get_messages","get_session_stats","set_model","new_session","switch_session","clone","clone_session","navigate","fork","get_entries","get_tree","get_branch","get_automation_state","set_session_name","get_last_assistant_text","set_steering_mode","set_follow_up_mode","get_commands","get_extensions","extension_command","extension_ui_response","reload_extensions","get_available_thinking_levels","set_thinking_level","cycle_thinking_level","export_jsonl","export_html","export_branch_jsonl","import_session","compact","shutdown"]}),
                            )),
                            "get_extensions" => Ok(Some(
                                json!({"extensions":client.options.extensions.ids(),"commands":client.options.extensions.commands()}),
                            )),
                            "reload_extensions" => {
                                active = Some(start_reload(&client, &command, &sender)?);
                                deferred = true;
                                Ok(None)
                            }
                            "extension_command" => {
                                let name = field(&command, "name")?;
                                if !client.options.extensions.contains_command(name) {
                                    return Err(format!("Unknown extension command: {name}"));
                                }
                                let args = command.get("args").map_or(Ok(""), |v| {
                                    v.as_str().ok_or("args must be a string")
                                })?;
                                let message = Message::user(format!("/{name} {args}"));
                                let mut run = start(&mut client, message, &sender, false)?;
                                run.command = Some(command.clone());
                                active = Some(run);
                                deferred = true;
                                Ok(None)
                            }
                            "compact" => {
                                active = Some(start_compaction(&mut client, &command, &sender)?);
                                deferred = true;
                                Ok(None)
                            }
                            "prompt" => {
                                let message = prompt(&command)?;
                                if active.is_some() || preparing.is_some() {
                                    if client
                                        .extension_command(field(&command, "message")?)
                                        .is_some()
                                    {
                                        return Err(
                                            "Extension commands require an idle agent".into()
                                        );
                                    }
                                    let behavior=match command["streamingBehavior"].as_str(){Some("steer")=>InputStreamingBehavior::Steer,Some("followUp")=>InputStreamingBehavior::FollowUp,_=>return Err("Running prompt requires streamingBehavior: steer or followUp".into())};
                                    if pending_inputs.len()
                                        + usize::from(preparing.is_some())
                                        + client.queues.len()
                                        >= 32
                                    {
                                        return Err("RPC pending queue contains 32 prompts".into());
                                    }
                                    let pending = PendingInput {
                                        command: command.clone(),
                                        message,
                                        behavior,
                                        launch: true,
                                    };
                                    if preparing.is_none() {
                                        preparing = Some(start_input(&client, pending, &sender)?);
                                    } else {
                                        pending_inputs.push_back(pending);
                                    }
                                    deferred = true;
                                } else {
                                    active = Some(start(&mut client, message, &sender, false)?);
                                }
                                Ok(None)
                            }
                            "steer" => {
                                if client
                                    .extension_command(field(&command, "message")?)
                                    .is_some()
                                {
                                    return Err(
                                        "Extension commands use extension_command, not steer"
                                            .into(),
                                    );
                                }
                                if pending_inputs.len()
                                    + usize::from(preparing.is_some())
                                    + client.queues.len()
                                    >= 32
                                {
                                    return Err("RPC pending queue contains 32 prompts".into());
                                }
                                let pending = PendingInput {
                                    command: command.clone(),
                                    message: prompt(&command)?,
                                    behavior: InputStreamingBehavior::Steer,
                                    launch: active.is_some(),
                                };
                                if preparing.is_none() {
                                    preparing = Some(start_input(&client, pending, &sender)?);
                                } else {
                                    pending_inputs.push_back(pending);
                                }
                                deferred = true;
                                Ok(None)
                            }
                            "follow_up" => {
                                if client
                                    .extension_command(field(&command, "message")?)
                                    .is_some()
                                {
                                    return Err(
                                        "Extension commands use extension_command, not follow_up"
                                            .into(),
                                    );
                                }
                                if pending_inputs.len()
                                    + usize::from(preparing.is_some())
                                    + client.queues.len()
                                    >= 32
                                {
                                    return Err("RPC pending queue contains 32 prompts".into());
                                }
                                let pending = PendingInput {
                                    command: command.clone(),
                                    message: prompt(&command)?,
                                    behavior: InputStreamingBehavior::FollowUp,
                                    launch: active.is_some(),
                                };
                                if preparing.is_none() {
                                    preparing = Some(start_input(&client, pending, &sender)?);
                                } else {
                                    pending_inputs.push_back(pending);
                                }
                                deferred = true;
                                Ok(None)
                            }
                            "abort" => {
                                stopping = active.is_some()
                                    || preparing.is_some()
                                    || !startup_lines.is_empty();
                                client.queues.clear();
                                if let Some(input) = &preparing {
                                    input.signal.cancel();
                                }
                                for pending in pending_inputs.drain(..) {
                                    write_record(
                                        output,
                                        &response(
                                            &pending.command,
                                            Err("Input preparation aborted".into()),
                                        ),
                                    )?;
                                }
                                if let Some(active) = &active {
                                    active.signal.cancel();
                                }
                                Ok(None)
                            }
                            "shutdown" => {
                                ended = true;
                                ui.close();
                                client.queues.clear();
                                if let Some(input) = &preparing {
                                    input.signal.cancel();
                                }
                                for pending in pending_inputs.drain(..) {
                                    write_record(
                                        output,
                                        &response(
                                            &pending.command,
                                            Err("Input preparation aborted".into()),
                                        ),
                                    )?;
                                }
                                if let Some(active) = &active {
                                    active.signal.cancel();
                                }
                                Ok(None)
                            }
                            "clear_queue" => {
                                if let Some(input) = &preparing {
                                    input.signal.cancel();
                                }
                                for pending in pending_inputs.drain(..) {
                                    write_record(
                                        output,
                                        &response(
                                            &pending.command,
                                            Err("Input preparation cleared".into()),
                                        ),
                                    )?;
                                }
                                let (steering, follow_up) = client.queues.clear();
                                Ok(Some(
                                    json!({"steering":steering.iter().map(Message::text).collect::<Vec<_>>(),"followUp":follow_up.iter().map(Message::text).collect::<Vec<_>>()}),
                                ))
                            }
                            "set_steering_mode" => {
                                client
                                    .queues
                                    .set_steering_mode(QueueMode::parse(field(&command, "mode")?)?);
                                Ok(None)
                            }
                            "set_follow_up_mode" => {
                                client.queues.set_follow_up_mode(QueueMode::parse(field(
                                    &command, "mode",
                                )?)?);
                                Ok(None)
                            }
                            "get_state" => {
                                let (steering, follow_up) = client.queues.modes();
                                let mut state = json!({"model":{"provider":client.provider_id,"api":provider_name(&client.settings.mode),"id":client.settings.model},"isStreaming":active.as_ref().is_some_and(|run|!run.compacting&&!run.extension&&!run.selecting_model),"isSelectingModel":active.as_ref().is_some_and(|run|run.selecting_model),"isStartingModelNotification":startup,"pendingStartupCommandCount":startup_lines.len(),"isCompacting":active.as_ref().is_some_and(|run|run.compacting),"isExtensionRunning":active.as_ref().is_some_and(|run|run.extension),"sessionId":client.session.id,"sessionName":client.session.name,"workspace":client.session.workspace,"messageCount":client.session.context.messages.len(),"pendingMessageCount":client.queues.len()+pending_inputs.len()+usize::from(preparing.is_some()),"isPreparingInput":preparing.is_some(),"steeringMode":steering.name(),"followUpMode":follow_up.name()});
                                state["thinkingLevel"] = json!(client.thinking_level());
                                Ok(Some(state))
                            }
                            "get_messages" => {
                                Ok(Some(json!({"messages":client.session.context.messages})))
                            }
                            "get_entries" | "get_tree" | "get_branch" => {
                                let snapshot = active
                                    .as_ref()
                                    .map_or(&client.session, |run| &run.committed_session);
                                let data = match kind {
                                    "get_entries" => json!({"entries":snapshot.get_entries()?}),
                                    "get_tree" => json!({"tree":snapshot.get_tree()?}),
                                    _ => {
                                        json!({"branch":snapshot.get_branch(optional_id(&command,"entryId")?)?})
                                    }
                                };
                                let mut data = data;
                                data["leafId"] = json!(snapshot.get_leaf_id()?);
                                data["committed"] = json!(true);
                                data["isRunning"] = json!(active.is_some());
                                Ok(Some(data))
                            }
                            "get_automation_state" => Ok(Some(
                                serde_json::to_value(client.options.automation_state.snapshot())
                                    .map_err(|error| error.to_string())?,
                            )),
                            "navigate" => {
                                let mut session = client.session.clone();
                                session.navigate(optional_id(&command, "entryId")?)?;
                                let restore =
                                    stored_model_selection(&session)?.is_some_and(|model| {
                                        model.provider != client.provider_id
                                            || model.model != client.settings.model
                                    });
                                if restore
                                    || session.get_thinking_level()?
                                        != client.settings.options.thinking_level
                                {
                                    active =
                                        Some(start_branch_selection(&client, &command, &sender)?);
                                    deferred = true;
                                    Ok(None)
                                } else {
                                    if let Some(store) = &client.store {
                                        store.save(&session)?;
                                    }
                                    client.session = session;
                                    client.queues.clear();
                                    client.refresh_resources();
                                    Ok(Some(
                                        json!({"leafId":client.session.get_leaf_id()?,"cancelled":false}),
                                    ))
                                }
                            }
                            "fork" => {
                                active = Some(start_branch_selection(&client, &command, &sender)?);
                                deferred = true;
                                Ok(None)
                            }
                            "clone_session" => {
                                let cloned = client.session.clone_current_branch()?;
                                active = Some(start_transition(
                                    &client,
                                    cloned,
                                    SessionStartReason::Fork,
                                    SessionShutdownReason::Fork,
                                    &command,
                                    &sender,
                                )?);
                                deferred = true;
                                Ok(None)
                            }
                            "get_available_models" => Ok(Some(
                                json!({"models":models().iter().map(|model|json!({"model":model,"nativeProtocol":model.is_supported()})).collect::<Vec<_>>()}),
                            )),
                            "get_last_assistant_text" => Ok(Some(
                                json!({"text":client.session.context.messages.iter().rev().find(|message| matches!(message,Message::Assistant(_))).map(Message::text)}),
                            )),
                            "get_session_stats" => {
                                let mut input: u64 = 0;
                                let mut output_tokens: u64 = 0;
                                let mut cache_read: u64 = 0;
                                let mut cache_write: u64 = 0;
                                for message in &client.session.context.messages {
                                    if let Message::Assistant(message) = message {
                                        if let Some(usage) = &message.usage {
                                            input = input.saturating_add(usage.input);
                                            output_tokens =
                                                output_tokens.saturating_add(usage.output);
                                            cache_read =
                                                cache_read.saturating_add(usage.cache_read);
                                            cache_write =
                                                cache_write.saturating_add(usage.cache_write);
                                        }
                                    }
                                }
                                Ok(Some(
                                    json!({"sessionId":client.session.id,"messageCount":client.session.context.messages.len(),"tokens":{"input":input,"output":output_tokens,"cacheRead":cache_read,"cacheWrite":cache_write}}),
                                ))
                            }
                            "new_session" => {
                                let mut session = Session {
                                    workspace: client.session.workspace.clone(),
                                    context: agent_core::types::Context {
                                        system_prompt: client.session.context.system_prompt.clone(),
                                        messages: vec![],
                                    },
                                    ..Default::default()
                                };
                                if !client.settings.model.is_empty() || client.provider_id == "demo"
                                {
                                    record_model_selection(
                                        &mut session,
                                        &ModelIdentity {
                                            provider: client.provider_id.clone(),
                                            model: client.settings.model.clone(),
                                        },
                                    )?;
                                }
                                if let Some(level) = client.settings.options.thinking_level {
                                    session.append_thinking_level(level)?;
                                }
                                active = Some(start_transition(
                                    &client,
                                    session,
                                    SessionStartReason::New,
                                    SessionShutdownReason::New,
                                    &command,
                                    &sender,
                                )?);
                                deferred = true;
                                Ok(None)
                            }
                            "switch_session" => {
                                let session =
                                    load_native(Path::new(field(&command, "sessionPath")?))?;
                                active = Some(start_transition(
                                    &client,
                                    session,
                                    SessionStartReason::Resume,
                                    SessionShutdownReason::Resume,
                                    &command,
                                    &sender,
                                )?);
                                deferred = true;
                                Ok(None)
                            }
                            "clone" => {
                                let identity = Session::default();
                                let mut cloned = client.session.clone();
                                cloned.id = identity.id;
                                cloned.updated_at = identity.updated_at;
                                active = Some(start_transition(
                                    &client,
                                    cloned,
                                    SessionStartReason::Fork,
                                    SessionShutdownReason::Fork,
                                    &command,
                                    &sender,
                                )?);
                                deferred = true;
                                Ok(None)
                            }
                            "set_session_name" => {
                                let mut candidate = client.session.clone();
                                candidate.append_session_info(field(&command, "name")?)?;
                                if interrupt.is_cancelled() {
                                    return Err("Session name cancelled before commit".into());
                                }
                                if let Some(store) = &client.store {
                                    store.save(&candidate)?;
                                }
                                client.session = candidate;
                                if client.options.extensions.ids().is_empty() {
                                    // A pure metadata command has no asynchronous callbacks.
                                    // Preserve JSONL command ordering and expose the committed
                                    // name to immediately following state/tree queries.
                                    Ok(Some(
                                        json!({"sessionName":client.session.get_session_name()?,"committed":true}),
                                    ))
                                } else {
                                    active =
                                        Some(start_name_notification(&client, &command, &sender)?);
                                    deferred = true;
                                    Ok(None)
                                }
                            }
                            "set_model" => {
                                let provider = field(&command, "provider")?;
                                let model = field(&command, "modelId")?;
                                if model.trim().is_empty() && provider != "demo" {
                                    return Err("modelId must not be empty".into());
                                }
                                active = Some(start_model_selection(&client, &command, &sender)?);
                                deferred = true;
                                Ok(None)
                            }
                            "get_available_thinking_levels" => {
                                Ok(Some(json!({"levels":client.available_thinking_levels()})))
                            }
                            "set_thinking_level" => {
                                let level = field(&command, "level")?.parse()?;
                                active = Some(start_thinking_selection(
                                    &client, &command, level, &sender,
                                )?);
                                deferred = true;
                                Ok(None)
                            }
                            "cycle_thinking_level" => {
                                if let Some(level) = client.next_thinking_level() {
                                    active = Some(start_thinking_selection(
                                        &client, &command, level, &sender,
                                    )?);
                                    deferred = true;
                                    Ok(None)
                                } else {
                                    Ok(Some(Value::Null))
                                }
                            }
                            "get_commands" => {
                                let mut commands:Vec<_> = client.resources.prompts.iter().map(|prompt| json!({"name":prompt.name,"description":prompt.description,"source":"prompt","sourceInfo":{"path":prompt.file_path}})).collect();
                                commands.extend(client.resources.skills.iter().map(|skill| json!({"name":format!("skill:{}",skill.name),"description":skill.description,"source":"skill","sourceInfo":{"path":skill.file_path}})));
                                commands.extend(client.options.extensions.commands().iter().map(|command|json!({"name":command.invocation_name,"description":command.description,"source":"extension","sourceInfo":{"id":command.extension_id}})));
                                Ok(Some(json!({"commands":commands})))
                            }
                            "export_jsonl" => {
                                let path = Path::new(field(&command, "outputPath")?);
                                SessionStore::export_jsonl(&client.session, path)?;
                                Ok(Some(json!({"path":path})))
                            }
                            "export_html" | "export_branch_jsonl" => {
                                let path = Path::new(field(&command, "outputPath")?);
                                if kind == "export_html" {
                                    SessionStore::export_html(&client.session, path)?;
                                } else {
                                    SessionStore::export_branch_jsonl(&client.session, path)?;
                                }
                                Ok(Some(json!({"path":path})))
                            }
                            "import_session" => {
                                let legacy = load_legacy_session(Path::new(field(
                                    &command,
                                    "sessionPath",
                                )?))?;
                                let imported = legacy.import_with_workspace(
                                    command
                                        .get("entryId")
                                        .and_then(Value::as_str)
                                        .map_or(LeafSelection::Latest, |id| {
                                            LeafSelection::Entry(id.into())
                                        }),
                                    command
                                        .get("workspacePath")
                                        .and_then(Value::as_str)
                                        .map(Path::new),
                                )?;
                                active = Some(start_transition(
                                    &client,
                                    imported,
                                    SessionStartReason::Resume,
                                    SessionShutdownReason::Resume,
                                    &command,
                                    &sender,
                                )?);
                                deferred = true;
                                Ok(None)
                            }
                            _ => Err(format!("Unknown native RPC command: {kind}")),
                        }
                    })()
                };
                if !deferred {
                    write_record(output, &response(&command, result))?;
                }
                if matches!(kind, "abort" | "shutdown") {
                    // Acknowledge the interrupt before resolving the accepted
                    // commands it cancels, matching the existing RPC envelope order.
                    reject_startup_commands(
                        &mut startup_lines,
                        output,
                        "Startup commands aborted",
                    )?;
                }
            }
        }
        if active.is_none() && preparing.is_none() && pending_inputs.is_empty() {
            if let Some(result) = settled.take() {
                write_record(
                    output,
                    &json!({"type":"agent_settled","success":result.is_ok(),"error":result.err()}),
                )?;
            }
            stopping = false;
        }
    }
    client.save()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::args::parse;
    use std::io::Cursor;
    #[test]
    fn broken_output_cancels_and_settles_before_saving_the_transcript() {
        struct Broken;
        impl Write for Broken {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "offline broken pipe",
                ))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let dir = std::env::temp_dir().join(format!("agent-rpc-output-{}", Session::default().id));
        std::fs::create_dir(&dir).unwrap();
        let args = parse(vec![
            "--cwd".into(),
            dir.to_string_lossy().into_owned(),
            "--session-dir".into(),
            dir.join("sessions").to_string_lossy().into_owned(),
            "--no-context-files".into(),
            "--no-skills".into(),
            "--no-prompt-templates".into(),
        ])
        .unwrap();
        let client = Client::from_args(&args).unwrap();
        let error = run_rpc(
            Cursor::new(b"{\"type\":\"prompt\",\"message\":\"test\"}\n".to_vec()),
            &mut Broken,
            client,
            &CancellationToken::new(),
        )
        .unwrap_err();
        assert!(error.contains("RPC output failed"));
        let sessions = SessionStore::new(dir.join("sessions")).list().unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].context.messages.len(), 2);
        std::fs::remove_file(
            dir.join("sessions")
                .join(format!("{}.json", sessions[0].id)),
        )
        .unwrap();
        std::fs::remove_dir(dir.join("sessions")).unwrap();
        std::fs::remove_dir(dir).unwrap();
    }
}
