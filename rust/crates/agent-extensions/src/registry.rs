use crate::{
    custom_message_content, normalize_session_name, validate_input, validate_tool_schema,
    BeforeAgentStartEvent, BeforeAgentStartResult, CommandInfo, CommandResult, Extension,
    ExtensionCommand, ExtensionContext, ExtensionDiagnostic, InputEvent, InputResult,
    LifecycleEvent, SessionBeforeEvent, SessionBeforeResult, UiHandle, UiRequest,
};
use agent_core::tools::{
    AfterToolCallContext, AfterToolCallResult, AgentTool, BeforeToolCallContext,
    BeforeToolCallResult, ToolHooks,
};
use agent_core::types::{CancellationToken, Content, ExecutionMode, Message, ToolResult, Value};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex, RwLock,
};

const MAX_DIAGNOSTICS: usize = 1024;
const RESERVED_COMMANDS: &[&str] = &[
    "settings",
    "model",
    "tree",
    "thinking",
    "export",
    "import",
    "name",
    "session",
    "changelog",
    "hotkeys",
    "fork",
    "clone",
    "trust",
    "login",
    "logout",
    "new",
    "compact",
    "resume",
    "reload",
    "quit",
    "help",
    "exit",
    "sessions",
    "navigate",
    "export-html",
    "export-jsonl",
    "theme",
    "queued",
    "extensions",
];

struct ToolRegistration {
    tool: Arc<dyn AgentTool>,
    name: String,
    description: String,
    parameters: Value,
}

struct CommandRegistration {
    command: Arc<dyn ExtensionCommand>,
    name: String,
    description: String,
}

struct Registration {
    id: String,
    extension: Arc<dyn Extension>,
    active: Arc<AtomicBool>,
    ui_scopes: Mutex<UiScopes>,
    tools: Vec<Arc<ToolRegistration>>,
    commands: Vec<Arc<CommandRegistration>>,
}

struct UiEpoch {
    current: Arc<AtomicBool>,
    retiring: VecDeque<Arc<AtomicBool>>,
}
struct UiScopes {
    sessions: BTreeMap<Option<String>, UiEpoch>,
    binding: Option<String>,
}
impl UiScopes {
    fn initial(scope: Arc<AtomicBool>) -> Self {
        Self {
            sessions: BTreeMap::from([(
                None,
                UiEpoch {
                    current: scope,
                    retiring: VecDeque::new(),
                },
            )]),
            binding: None,
        }
    }
    fn scope(&mut self, key: Option<String>, active: bool) -> Arc<AtomicBool> {
        self.sessions
            .entry(key)
            .or_insert_with(|| UiEpoch {
                current: Arc::new(AtomicBool::new(active)),
                retiring: VecDeque::new(),
            })
            .current
            .clone()
    }
    fn renew(&mut self, key: Option<String>, active: bool, retire: bool) -> Arc<AtomicBool> {
        let scope = Arc::new(AtomicBool::new(active));
        if let Some(epoch) = self.sessions.get_mut(&key) {
            if retire && epoch.current.load(Ordering::Acquire) {
                epoch.retiring.push_back(epoch.current.clone());
            } else {
                epoch.current.store(false, Ordering::Release);
            }
            epoch.current = scope.clone();
        } else {
            self.sessions.insert(
                key.clone(),
                UiEpoch {
                    current: scope.clone(),
                    retiring: VecDeque::new(),
                },
            );
        }
        self.binding = key;
        scope
    }
    fn shutdown_scope(&mut self, key: Option<String>, active: bool) -> Arc<AtomicBool> {
        let scope = self.scope(key.clone(), active);
        let epoch = self.sessions.get_mut(&key).expect("created UI scope");
        epoch.retiring.pop_front().unwrap_or(scope)
    }
    fn revoke_all(&self) {
        for epoch in self.sessions.values() {
            epoch.current.store(false, Ordering::Release);
            for scope in &epoch.retiring {
                scope.store(false, Ordering::Release);
            }
        }
    }
}

struct Registry {
    registrations: Vec<Arc<Registration>>,
    reserved_commands: BTreeSet<String>,
    reserved_tools: BTreeSet<String>,
    diagnostics: VecDeque<ExtensionDiagnostic>,
    ui: UiHandle,
}

impl Default for Registry {
    fn default() -> Self {
        Self {
            registrations: Vec::new(),
            reserved_commands: RESERVED_COMMANDS
                .iter()
                .map(|name| (*name).into())
                .collect(),
            reserved_tools: BTreeSet::new(),
            diagnostics: VecDeque::new(),
            ui: UiHandle::default(),
        }
    }
}

/// Clones share registration and diagnostics. No registry lock is held during callbacks.
#[derive(Clone, Default)]
pub struct SharedRegistry(Arc<RwLock<Registry>>);

impl fmt::Debug for SharedRegistry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SharedRegistry")
            .field("extensions", &self.extension_ids())
            .finish_non_exhaustive()
    }
}

impl SharedRegistry {
    fn snapshot(&self) -> Vec<Arc<Registration>> {
        self.0
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .registrations
            .clone()
    }

    pub fn extension_ids(&self) -> Vec<String> {
        self.snapshot()
            .iter()
            .map(|entry| entry.id.clone())
            .collect()
    }

    pub fn ids(&self) -> Vec<String> {
        self.extension_ids()
    }
    pub fn ui(&self) -> UiHandle {
        self.0.read().unwrap_or_else(|e| e.into_inner()).ui.clone()
    }
    pub fn bind_ui(&self, ui: UiHandle) -> Result<(), String> {
        self.0.write().unwrap_or_else(|e| e.into_inner()).ui = ui.clone();
        let mut failure = None;
        for entry in self.snapshot() {
            if !entry.active.load(Ordering::Acquire) {
                continue;
            }
            let scope = {
                let mut scopes = entry.ui_scopes.lock().unwrap_or_else(|e| e.into_inner());
                let key = scopes.binding.clone();
                let current = scopes.scope(key.clone(), true);
                if current.load(Ordering::Acquire) {
                    current
                } else {
                    scopes.renew(key, true, false)
                }
            };
            if let Err(error) = guarded(|| entry.extension.bind_ui(ui.scoped(scope))) {
                self.diagnostic(&entry.id, "ui_bind", &error);
                failure.get_or_insert(error);
            }
        }
        failure.map_or(Ok(()), Err)
    }

    /// Validates a complete extension before atomically installing it. Duplicate tool
    /// names across extensions keep the first registration; duplicate commands get aliases.
    pub fn register(&self, extension: Arc<dyn Extension>) -> Result<(), String> {
        let active = Arc::new(AtomicBool::new(false));
        let ui_scope = Arc::new(AtomicBool::new(false));
        let (id, tools, commands) = guarded(|| {
            let id = extension.id().to_owned();
            validate_name(&id, "extension", false)?;
            let mut tool_names = BTreeSet::new();
            let tools = extension
                .tools()
                .into_iter()
                .map(|tool| {
                    let name = tool.name().to_owned();
                    validate_name(&name, "tool", false)?;
                    if !tool_names.insert(name.clone()) {
                        return Err(format!("Duplicate tool {name} within extension {id}"));
                    }
                    let parameters = tool.parameters();
                    validate_tool_schema(&parameters)
                        .map_err(|error| format!("Tool {name}: {error}"))?;
                    Ok(Arc::new(ToolRegistration {
                        description: tool.description().to_owned(),
                        tool,
                        name,
                        parameters,
                    }))
                })
                .collect::<Result<Vec<_>, String>>()?;
            let mut command_names = BTreeSet::new();
            let commands = extension
                .commands()
                .into_iter()
                .map(|command| {
                    let name = command.name().to_owned();
                    validate_name(&name, "command", true)?;
                    if !command_names.insert(name.clone()) {
                        return Err(format!("Duplicate command {name} within extension {id}"));
                    }
                    Ok(Arc::new(CommandRegistration {
                        description: command.description().to_owned(),
                        command,
                        name,
                    }))
                })
                .collect::<Result<Vec<_>, String>>()?;
            Ok((id, tools, commands))
        })?;
        {
            let inner = self.0.read().unwrap_or_else(|error| error.into_inner());
            if inner.registrations.iter().any(|entry| entry.id == id) {
                return Err(format!("Extension {id} is already registered"));
            }
            for tool in &tools {
                if inner.reserved_tools.contains(&tool.name) {
                    return Err(format!("Tool {} is reserved by the host", tool.name));
                }
            }
            for command in &commands {
                if inner.reserved_commands.contains(&command.name) {
                    return Err(format!("Command {} is reserved by the host", command.name));
                }
            }
        }
        guarded(|| extension.bind_ui(self.ui().scoped(ui_scope.clone())))?;
        let mut inner = self.0.write().unwrap_or_else(|error| error.into_inner());
        if inner.registrations.iter().any(|entry| entry.id == id) {
            return Err(format!("Extension {id} is already registered"));
        }
        for tool in &tools {
            if inner.reserved_tools.contains(&tool.name) {
                return Err(format!("Tool {} is reserved by the host", tool.name));
            }
        }
        for command in &commands {
            if inner.reserved_commands.contains(&command.name) {
                return Err(format!("Command {} is reserved by the host", command.name));
            }
        }
        for tool in &tools {
            if inner.registrations.iter().any(|entry| {
                entry
                    .tools
                    .iter()
                    .any(|existing| existing.name == tool.name)
            }) {
                push_diagnostic(
                    &mut inner,
                    ExtensionDiagnostic {
                        extension_id: id.clone(),
                        event: "register_tool".into(),
                        error: format!(
                            "Duplicate tool {}: first registration remains active",
                            tool.name
                        ),
                    },
                );
            }
        }
        inner.registrations.push(Arc::new(Registration {
            id,
            extension,
            active: active.clone(),
            ui_scopes: Mutex::new(UiScopes::initial(ui_scope.clone())),
            tools,
            commands,
        }));
        active.store(true, Ordering::Release);
        ui_scope.store(true, Ordering::Release);
        Ok(())
    }
    fn scoped_context(&self, entry: &Registration, context: &ExtensionContext) -> ExtensionContext {
        let scope = entry
            .ui_scopes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .scope(
                context.session_id.clone(),
                entry.active.load(Ordering::Acquire),
            );
        self.context_with_scope(context, scope)
    }
    fn context_with_scope(
        &self,
        context: &ExtensionContext,
        scope: Arc<AtomicBool>,
    ) -> ExtensionContext {
        let mut context = context.clone();
        context.ui = self.ui().scoped(scope);
        context.has_ui = context.has_ui && context.ui.is_enabled();
        context
    }

    pub fn reserve_commands(&self, names: &[&str]) -> Result<(), String> {
        self.reserve(names, true)
    }

    pub fn reserve_tools(&self, names: &[&str]) -> Result<(), String> {
        self.reserve(names, false)
    }

    fn reserve(&self, names: &[&str], commands: bool) -> Result<(), String> {
        for name in names {
            validate_name(name, if commands { "command" } else { "tool" }, commands)?;
        }
        let mut inner = self.0.write().unwrap_or_else(|error| error.into_inner());
        for name in names {
            let occupied = inner.registrations.iter().any(|entry| {
                if commands {
                    entry.commands.iter().any(|item| item.name == *name)
                } else {
                    entry.tools.iter().any(|item| item.name == *name)
                }
            });
            if occupied {
                return Err(format!("Name {name} is already registered"));
            }
        }
        let target = if commands {
            &mut inner.reserved_commands
        } else {
            &mut inner.reserved_tools
        };
        target.extend(names.iter().map(|name| (*name).to_owned()));
        Ok(())
    }

    /// Invalidates captured tool wrappers before removing registration. The host is
    /// responsible for emitting SessionShutdown before removal when cleanup is needed.
    pub fn unregister(&self, id: &str) -> Result<bool, String> {
        let mut inner = self.0.write().unwrap_or_else(|error| error.into_inner());
        let Some(index) = inner.registrations.iter().position(|entry| entry.id == id) else {
            return Ok(false);
        };
        inner.registrations[index]
            .active
            .store(false, Ordering::Release);
        inner.registrations[index]
            .ui_scopes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .revoke_all();
        inner.registrations.remove(index);
        Ok(true)
    }

    pub fn tools(&self) -> Vec<Box<dyn AgentTool>> {
        let mut seen = BTreeSet::new();
        let mut tools: Vec<Box<dyn AgentTool>> = Vec::new();
        for entry in self.snapshot() {
            for tool in &entry.tools {
                if seen.insert(tool.name.clone()) {
                    tools.push(Box::new(RegisteredTool {
                        registration: tool.clone(),
                        active: entry.active.clone(),
                    }));
                }
            }
        }
        tools
    }

    fn resolved_commands(&self) -> Vec<(CommandInfo, Arc<Registration>, Arc<CommandRegistration>)> {
        let entries = self.snapshot();
        let mut counts = BTreeMap::new();
        for entry in &entries {
            for command in &entry.commands {
                *counts.entry(command.name.clone()).or_insert(0_usize) += 1;
            }
        }
        let mut seen = BTreeMap::new();
        let mut taken = BTreeSet::new();
        let mut result = Vec::new();
        for entry in entries {
            for command in &entry.commands {
                let occurrence = seen.entry(command.name.clone()).or_insert(0_usize);
                *occurrence += 1;
                let mut suffix = *occurrence;
                let mut invocation = if counts[&command.name] > 1 {
                    format!("{}:{suffix}", command.name)
                } else {
                    command.name.clone()
                };
                while taken.contains(&invocation) {
                    suffix += 1;
                    invocation = format!("{}:{suffix}", command.name);
                }
                taken.insert(invocation.clone());
                result.push((
                    CommandInfo {
                        extension_id: entry.id.clone(),
                        name: command.name.clone(),
                        invocation_name: invocation,
                        description: command.description.clone(),
                    },
                    entry.clone(),
                    command.clone(),
                ));
            }
        }
        result
    }

    pub fn commands(&self) -> Vec<CommandInfo> {
        self.resolved_commands()
            .into_iter()
            .map(|(info, _, _)| info)
            .collect()
    }

    pub fn resolve_command(&self, name: &str) -> Option<CommandInfo> {
        self.commands()
            .into_iter()
            .find(|info| info.invocation_name == name)
    }

    pub fn contains_command(&self, name: &str) -> bool {
        self.resolve_command(name).is_some()
    }

    pub fn dispatch_command(
        &self,
        name: &str,
        args: &str,
        context: &ExtensionContext,
        signal: &CancellationToken,
    ) -> Result<CommandResult, String> {
        check_cancelled(signal)?;
        let Some((_, entry, command)) = self
            .resolved_commands()
            .into_iter()
            .find(|(info, _, _)| info.invocation_name == name)
        else {
            return Err(format!("Extension command {name} not found"));
        };
        check_active(&entry.active)?;
        let context = self.scoped_context(&entry, context);
        let result = guarded(|| command.command.execute(args, &context, signal))
            .and_then(validate_command_result);
        match result {
            Ok(result) => {
                check_cancelled(signal)?;
                check_active(&entry.active)?;
                Ok(result)
            }
            Err(error) => {
                self.diagnostic(&entry.id, "command", &error);
                Err(format!("Extension {} command {name}: {error}", entry.id))
            }
        }
    }

    pub fn before_agent_start(
        &self,
        event: &BeforeAgentStartEvent,
        context: &ExtensionContext,
        signal: &CancellationToken,
    ) -> Result<BeforeAgentStartResult, String> {
        check_cancelled(signal)?;
        let mut current_event = event.clone();
        let mut current_context = context.clone();
        let mut combined = BeforeAgentStartResult::default();
        for entry in self.snapshot() {
            check_cancelled(signal)?;
            if !entry.active.load(Ordering::Acquire) {
                continue;
            }
            current_context
                .context
                .system_prompt
                .clone_from(&current_event.system_prompt);
            let scoped = self.scoped_context(&entry, &current_context);
            match guarded(|| {
                entry
                    .extension
                    .before_agent_start(&current_event, &scoped, signal)
            }) {
                Ok(result) if result.messages.iter().all(valid_custom_message) => {
                    combined.messages.extend(result.messages);
                    if let Some(prompt) = result.system_prompt {
                        current_event.system_prompt.clone_from(&prompt);
                        combined.system_prompt = Some(prompt);
                    }
                }
                Ok(_) => self.diagnostic(
                    &entry.id,
                    "before_agent_start",
                    "Only valid custom messages may be injected",
                ),
                Err(error) => self.diagnostic(&entry.id, "before_agent_start", &error),
            }
        }
        check_cancelled(signal)?;
        Ok(combined)
    }

    /// Ordered raw-input interception. Ordinary callback failures are diagnostic,
    /// preserve the preceding transformed input, and let the next handler run.
    pub fn emit_input(
        &self,
        event: &InputEvent,
        context: &ExtensionContext,
        signal: &CancellationToken,
    ) -> Result<InputResult, String> {
        check_cancelled(signal)?;
        validate_input(event)?;
        let mut current = event.clone();
        let mut images_replaced = false;
        for entry in self.snapshot() {
            check_cancelled(signal)?;
            if !entry.active.load(Ordering::Acquire) {
                continue;
            }
            match guarded(|| {
                entry
                    .extension
                    .input(&current, &self.scoped_context(&entry, context), signal)
            }) {
                Ok(InputResult::Continue) => {}
                Ok(InputResult::Handled) => {
                    check_cancelled(signal)?;
                    return Ok(InputResult::Handled);
                }
                Ok(InputResult::Transform { text, images }) => {
                    let replaces_images = images.is_some();
                    let mut candidate = current.clone();
                    candidate.text = text;
                    if let Some(images) = images {
                        candidate.images = images;
                    }
                    match validate_input(&candidate) {
                        Ok(()) => {
                            current = candidate;
                            images_replaced |= replaces_images;
                        }
                        Err(error) => self.diagnostic(&entry.id, "input", &error),
                    }
                }
                Err(error) => self.diagnostic(&entry.id, "input", &error),
            }
            check_cancelled(signal)?;
        }
        check_cancelled(signal)?;
        if current.text == event.text && !images_replaced {
            Ok(InputResult::Continue)
        } else {
            Ok(InputResult::Transform {
                text: current.text,
                images: Some(current.images),
            })
        }
    }

    pub fn transform_context(
        &self,
        messages: &[Message],
        context: &ExtensionContext,
        signal: &CancellationToken,
    ) -> Result<Vec<Message>, String> {
        check_cancelled(signal)?;
        let mut current = messages.to_vec();
        for entry in self.snapshot() {
            check_cancelled(signal)?;
            if !entry.active.load(Ordering::Acquire) {
                continue;
            }
            match guarded(|| {
                entry
                    .extension
                    .context(&current, &self.scoped_context(&entry, context), signal)
            }) {
                Ok(Some(messages)) => current = messages,
                Ok(None) => {}
                Err(error) => self.diagnostic(&entry.id, "context", &error),
            }
        }
        check_cancelled(signal)?;
        Ok(current)
    }

    pub fn emit_event(
        &self,
        event: &LifecycleEvent,
        context: &ExtensionContext,
        signal: &CancellationToken,
    ) -> Result<(), String> {
        self.emit_lifecycle(event, context, signal, false)
    }

    /// Ordered pre-commit veto. Ordinary errors are diagnostic; the first explicit
    /// cancellation wins, and token cancellation always prevents the host commit.
    pub fn emit_session_before(
        &self,
        event: &SessionBeforeEvent,
        context: &ExtensionContext,
        signal: &CancellationToken,
    ) -> Result<SessionBeforeResult, String> {
        check_cancelled(signal)?;
        for entry in self.snapshot() {
            check_cancelled(signal)?;
            if !entry.active.load(Ordering::Acquire) {
                continue;
            }
            match guarded(|| {
                entry.extension.before_session_change(
                    event,
                    &self.scoped_context(&entry, context),
                    signal,
                )
            }) {
                Ok(result) => {
                    check_cancelled(signal)?;
                    if result.cancel {
                        return Ok(result);
                    }
                }
                Err(error) => self.diagnostic(&entry.id, event.name(), &error),
            }
        }
        check_cancelled(signal)?;
        Ok(SessionBeforeResult::default())
    }

    /// Candidate initialization must acknowledge lifecycle delivery before replacing
    /// a healthy runtime. Ordinary observations keep the best-effort `emit_event` path.
    pub fn emit_event_checked(
        &self,
        event: &LifecycleEvent,
        context: &ExtensionContext,
        signal: &CancellationToken,
    ) -> Result<(), String> {
        self.emit_lifecycle(event, context, signal, true)
    }

    fn emit_lifecycle(
        &self,
        event: &LifecycleEvent,
        context: &ExtensionContext,
        signal: &CancellationToken,
        checked: bool,
    ) -> Result<(), String> {
        struct ShutdownGuard(Vec<Arc<AtomicBool>>);
        impl Drop for ShutdownGuard {
            fn drop(&mut self) {
                for scope in &self.0 {
                    scope.store(false, Ordering::Release);
                }
            }
        }
        struct StartEpoch {
            entry: Arc<Registration>,
            key: Option<String>,
            scope: Arc<AtomicBool>,
            previous: Option<Arc<AtomicBool>>,
            binding: Option<String>,
        }
        struct StartGuard {
            registry: SharedRegistry,
            epochs: Vec<StartEpoch>,
            completed: bool,
        }
        impl Drop for StartGuard {
            fn drop(&mut self) {
                if self.completed {
                    return;
                }
                for epoch in self.epochs.iter().rev() {
                    epoch.scope.store(false, Ordering::Release);
                    let rebound = {
                        let mut scopes = epoch
                            .entry
                            .ui_scopes
                            .lock()
                            .unwrap_or_else(|e| e.into_inner());
                        if scopes
                            .sessions
                            .get(&epoch.key)
                            .is_some_and(|current| Arc::ptr_eq(&current.current, &epoch.scope))
                        {
                            if let Some(previous) = &epoch.previous {
                                let current = scopes
                                    .sessions
                                    .get_mut(&epoch.key)
                                    .expect("observed start scope");
                                current.current = previous.clone();
                                if let Some(index) = current
                                    .retiring
                                    .iter()
                                    .rposition(|scope| Arc::ptr_eq(scope, previous))
                                {
                                    current.retiring.remove(index);
                                }
                            } else {
                                scopes.sessions.remove(&epoch.key);
                            }
                            if scopes.binding == epoch.key {
                                scopes.binding = epoch.binding.clone();
                            }
                        }
                        let key = scopes.binding.clone();
                        scopes.scope(key, epoch.entry.active.load(Ordering::Acquire))
                    };
                    if epoch.entry.active.load(Ordering::Acquire) {
                        if let Err(error) = guarded(|| {
                            epoch
                                .entry
                                .extension
                                .bind_ui(self.registry.ui().scoped(rebound))
                        }) {
                            self.registry.diagnostic(&epoch.entry.id, "ui_bind", &error);
                        }
                    }
                }
            }
        }
        let entries = self.snapshot();
        let mut starts = StartGuard {
            registry: self.clone(),
            epochs: Vec::new(),
            completed: false,
        };
        // Registration lifetime is independent of a session's UI lifetime. Native
        // Rust hosts may reuse this registry for a candidate session before the
        // previous session's shutdown; only that previous session's scopes retire.
        let shutting_down = matches!(event, LifecycleEvent::SessionShutdown { .. });
        let mut shutdown_scopes = Vec::new();
        let mut generic_scopes = Vec::new();
        if shutting_down {
            for entry in &entries {
                let mut scopes = entry.ui_scopes.lock().unwrap_or_else(|e| e.into_inner());
                shutdown_scopes.push(scopes.shutdown_scope(
                    context.session_id.clone(),
                    entry.active.load(Ordering::Acquire),
                ));
                if context.session_id.is_some() {
                    generic_scopes.push(scopes.shutdown_scope(None, false));
                }
            }
        }
        let _shutdown = ShutdownGuard(
            shutdown_scopes
                .iter()
                .cloned()
                .chain(generic_scopes)
                .collect(),
        );
        for (index, entry) in entries.into_iter().enumerate() {
            if !event.is_terminal() {
                check_cancelled(signal)?;
            }
            if !entry.active.load(Ordering::Acquire) {
                continue;
            }
            let scoped = if shutting_down {
                self.context_with_scope(context, shutdown_scopes[index].clone())
            } else if let LifecycleEvent::SessionStart { reason, .. } = event {
                let mut scopes = entry.ui_scopes.lock().unwrap_or_else(|e| e.into_inner());
                let previous = scopes
                    .sessions
                    .get(&context.session_id)
                    .map(|epoch| epoch.current.clone());
                let binding = scopes.binding.clone();
                let scope = scopes.renew(
                    context.session_id.clone(),
                    true,
                    *reason != crate::SessionStartReason::Startup,
                );
                drop(scopes);
                starts.epochs.push(StartEpoch {
                    entry: entry.clone(),
                    key: context.session_id.clone(),
                    scope: scope.clone(),
                    previous,
                    binding,
                });
                if let Err(error) =
                    guarded(|| entry.extension.bind_ui(self.ui().scoped(scope.clone())))
                {
                    self.diagnostic(&entry.id, "ui_bind", &error);
                    if checked {
                        return Err(format!("Extension {} UI binding: {error}", entry.id));
                    }
                }
                self.context_with_scope(context, scope)
            } else {
                self.scoped_context(&entry, context)
            };
            if let Err(error) = guarded(|| entry.extension.on_event(event, &scoped, signal)) {
                let name = match event {
                    LifecycleEvent::ModelSelect { .. } => "model_select",
                    LifecycleEvent::ThinkingLevelSelect { .. } => "thinking_level_select",
                    LifecycleEvent::SessionInfoChanged { .. } => "session_info_changed",
                    _ => "lifecycle",
                };
                self.diagnostic(&entry.id, name, &error);
                if checked {
                    return Err(format!("Extension {} lifecycle: {error}", entry.id));
                }
            }
        }
        if !event.is_terminal() {
            check_cancelled(signal)?;
        }
        starts.completed = true;
        Ok(())
    }

    pub fn tool_hooks(&self, context: ExtensionContext) -> RegistryToolHooks {
        RegistryToolHooks {
            registry: self.clone(),
            context,
        }
    }

    pub fn drain_diagnostics(&self) -> Vec<ExtensionDiagnostic> {
        self.0
            .write()
            .unwrap_or_else(|error| error.into_inner())
            .diagnostics
            .drain(..)
            .collect()
    }

    fn diagnostic(&self, id: &str, event: &str, error: &str) {
        let mut inner = self.0.write().unwrap_or_else(|error| error.into_inner());
        push_diagnostic(
            &mut inner,
            ExtensionDiagnostic {
                extension_id: id.into(),
                event: event.into(),
                error: error.into(),
            },
        );
    }
}

#[derive(Clone, Debug)]
pub struct RegistryToolHooks {
    registry: SharedRegistry,
    context: ExtensionContext,
}

impl ToolHooks for RegistryToolHooks {
    fn before_tool_call(
        &self,
        call: BeforeToolCallContext<'_>,
        signal: &CancellationToken,
    ) -> Result<BeforeToolCallResult, String> {
        check_cancelled(signal)?;
        let mut context = self.context.clone();
        context.context = call.context.clone();
        let mut current = BeforeToolCallResult::default();
        for entry in self.registry.snapshot() {
            check_cancelled(signal)?;
            if !entry.active.load(Ordering::Acquire) {
                continue;
            }
            match guarded(|| {
                entry.extension.before_tool_call(
                    BeforeToolCallContext {
                        assistant_message: call.assistant_message,
                        tool_call: call.tool_call,
                        args: &mut *call.args,
                        context: call.context,
                    },
                    &self.registry.scoped_context(&entry, &context),
                    signal,
                )
            }) {
                Ok(result) => {
                    current = result;
                    check_cancelled(signal)?;
                    if current.block {
                        return Ok(current);
                    }
                }
                Err(error) => {
                    self.registry.diagnostic(&entry.id, "tool_call", &error);
                    return Err(format!("Extension {} tool_call: {error}", entry.id));
                }
            }
        }
        check_cancelled(signal)?;
        Ok(current)
    }

    fn after_tool_call(
        &self,
        call: AfterToolCallContext<'_>,
        signal: &CancellationToken,
    ) -> Result<Option<AfterToolCallResult>, String> {
        let mut context = self.context.clone();
        context.context = call.context.clone();
        let mut result = call.result.clone();
        let mut is_error = call.is_error;
        let mut combined = AfterToolCallResult::default();
        let mut modified = false;
        for entry in self.registry.snapshot() {
            // Cancellation cannot erase a tool result that already completed.
            if signal.is_cancelled() {
                break;
            }
            if !entry.active.load(Ordering::Acquire) {
                continue;
            }
            match guarded(|| {
                entry.extension.after_tool_call(
                    AfterToolCallContext {
                        assistant_message: call.assistant_message,
                        tool_call: call.tool_call,
                        args: call.args,
                        result: &result,
                        is_error,
                        context: call.context,
                    },
                    &self.registry.scoped_context(&entry, &context),
                    signal,
                )
            }) {
                Ok(Some(patch)) => {
                    if let Some(content) = patch.content {
                        result.content = content.clone();
                        combined.content = Some(content);
                        modified = true;
                    }
                    if let Some(details) = patch.details {
                        result.details = details.clone();
                        combined.details = Some(details);
                        modified = true;
                    }
                    if let Some(usage) = patch.usage {
                        result.usage = Some(usage.clone());
                        combined.usage = Some(usage);
                        modified = true;
                    }
                    if let Some(terminate) = patch.terminate {
                        result.terminate = terminate;
                        combined.terminate = Some(terminate);
                        modified = true;
                    }
                    if let Some(error) = patch.is_error {
                        is_error = error;
                        combined.is_error = Some(error);
                        modified = true;
                    }
                }
                Ok(None) => {}
                Err(error) => self.registry.diagnostic(&entry.id, "tool_result", &error),
            }
        }
        Ok(modified.then_some(combined))
    }
}

struct RegisteredTool {
    registration: Arc<ToolRegistration>,
    active: Arc<AtomicBool>,
}

impl AgentTool for RegisteredTool {
    fn name(&self) -> &str {
        &self.registration.name
    }
    fn description(&self) -> &str {
        &self.registration.description
    }
    fn parameters(&self) -> Value {
        self.registration.parameters.clone()
    }
    fn execution_mode(&self) -> Option<ExecutionMode> {
        self.registration.tool.execution_mode()
    }
    fn prepare_arguments(&self, args: &Value) -> Result<Value, String> {
        check_active(&self.active)?;
        guarded(|| self.registration.tool.prepare_arguments(args))
    }
    fn validate_arguments(&self, args: &Value) -> Result<(), String> {
        check_active(&self.active)?;
        guarded(|| self.registration.tool.validate_arguments(args))
    }
    fn execute(
        &self,
        call_id: &str,
        args: &Value,
        signal: &CancellationToken,
        on_update: &mut dyn FnMut(ToolResult),
    ) -> Result<ToolResult, String> {
        check_active(&self.active)?;
        check_cancelled(signal)?;
        guarded(|| {
            self.registration
                .tool
                .execute(call_id, args, signal, on_update)
        })
    }
}

fn validate_name(name: &str, kind: &str, colon: bool) -> Result<(), String> {
    if name.is_empty()
        || name.len() > 128
        || !name.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(byte, b'_' | b'-' | b'.')
                || (colon && byte == b':')
        })
    {
        return Err(format!(
            "Invalid {kind} name {name:?}: use 1-128 ASCII letters, digits, _, -, .{}",
            if colon { ", :" } else { "" }
        ));
    }
    Ok(())
}

fn valid_custom_message(message: &Message) -> bool {
    matches!(message, Message::Custom { kind, .. } if !kind.trim().is_empty())
        && custom_message_content(message).is_ok()
}

fn validate_command_result(mut result: CommandResult) -> Result<CommandResult, String> {
    for message in &result.messages {
        let valid = match message {
            Message::User(user) => user
                .content
                .iter()
                .all(|content| matches!(content, Content::Text { .. } | Content::Image { .. })),
            _ => valid_custom_message(message),
        };
        if !valid {
            return Err(
                "Command messages must be User(text/image) or Custom, not assistant/tool results"
                    .into(),
            );
        }
    }
    if let Some(name) = result.session_name.as_mut() {
        *name = normalize_session_name(name);
    }
    for request in &result.ui {
        if let UiRequest::SetStatus { key, .. } = request {
            if key.trim().is_empty() || key.len() > 128 {
                return Err("UI status key must be 1-128 nonempty bytes".into());
            }
        }
    }
    Ok(result)
}

fn push_diagnostic(inner: &mut Registry, diagnostic: ExtensionDiagnostic) {
    if inner.diagnostics.len() == MAX_DIAGNOSTICS {
        inner.diagnostics.pop_front();
    }
    inner.diagnostics.push_back(diagnostic);
}

fn check_cancelled(signal: &CancellationToken) -> Result<(), String> {
    if signal.is_cancelled() {
        Err("Operation aborted".into())
    } else {
        Ok(())
    }
}

fn check_active(active: &AtomicBool) -> Result<(), String> {
    if active.load(Ordering::Acquire) {
        Ok(())
    } else {
        Err("Extension registration is stale".into())
    }
}

fn guarded<T>(action: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    match catch_unwind(AssertUnwindSafe(action)) {
        Ok(result) => result,
        Err(_) => Err("Extension callback panicked".into()),
    }
}
