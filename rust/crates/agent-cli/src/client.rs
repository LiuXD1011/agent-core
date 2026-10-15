use crate::args::Args;
use crate::auth_commands::{auth_store_for, default_auth_directory};
use agent_core::agent_loop::Provider;
use agent_core::types::{
    timestamp, AgentEvent, CancellationToken, Content, Message, ThinkingLevel, UserMessage,
};
use agent_runtime::auth::CodexOAuthClient;
use agent_runtime::automatic::{record_manual_checkpoint, AutomationOptions, ModelBudget};
use agent_runtime::builtins::create_tools;
use agent_runtime::catalog::{find, find_canonical};
use agent_runtime::compaction::{compact_session, CompactionResult};
use agent_runtime::credentials::resolve_api_key;
use agent_runtime::extensions::{
    execute_extension_command, extension_context, load_native_extensions, notify_model_select,
    notify_session_info_changed, prepare_input_with_behavior, prepare_session_change,
    record_model_selection, stored_model_selection, CommandResult, ExtensionContext, ExtensionMode,
    ForkPosition, InputSource, InputStreamingBehavior, LifecycleEvent, ModelIdentity,
    ModelSelectSource, SessionBeforeEvent, SessionShutdownReason, SessionStartReason,
    SessionSwitchReason,
};
use agent_runtime::instructions::load_context_files;
use agent_runtime::legacy_session::{load_legacy_session, LeafSelection};
use agent_runtime::provider::ModelProvider;
use agent_runtime::provider::{
    AzureOptions, BedrockCacheRetention, BedrockOptions, ProviderMode, ProviderOptions,
    ProviderSettings, VertexOptions,
};
use agent_runtime::resources::{
    format_skills_for_prompt, load_resources, prepare_prompt, ResourceLoad, ResourceOptions,
};
use agent_runtime::retry::RetryingProvider;
use agent_runtime::runtime::{run_prompt, MessageQueues, RunOptions};
use agent_runtime::session::{Session, SessionStore};
use agent_runtime::thinking::{
    notify_thinking_level_select, prepare_thinking_selection, restore_thinking_settings,
};
use std::fs;
use std::path::{Path, PathBuf};

pub struct Client {
    pub session: Session,
    pub settings: ProviderSettings,
    pub provider_id: String,
    pub auth_dir: PathBuf,
    pub options: RunOptions,
    pub queues: MessageQueues,
    pub store: Option<SessionStore>,
    pub resources: ResourceLoad,
    pub extension_paths: Vec<PathBuf>,
    pub last_command: Option<CommandResult>,
    pub input_handled: bool,
    pub(crate) model_notification_pending: bool,
    pub(crate) session_start_pending: bool,
    thinking_notification: Option<(ThinkingLevel, ThinkingLevel)>,
    resource_options: ResourceOptions,
    load_context: bool,
    no_skills: bool,
    no_prompts: bool,
}

/// Credentials remain in process; only identity is committed to the session tree.
pub(crate) struct ModelSelection {
    pub settings: ProviderSettings,
    pub provider_id: String,
    pub budget: Option<ModelBudget>,
}
pub(crate) struct StartupNotifications {
    session: Session,
    registry: agent_runtime::extensions::SharedRegistry,
    mode: ExtensionMode,
    model: Option<ModelIdentity>,
    start: bool,
    restore: bool,
    thinking: Option<(ThinkingLevel, ThinkingLevel)>,
}
impl StartupNotifications {
    pub(crate) fn complete(self, signal: &CancellationToken) -> Result<(), String> {
        if self.start {
            let context =
                extension_context(&self.session, &self.registry, self.mode, self.model.clone())?;
            self.registry.emit_event_checked(
                &LifecycleEvent::SessionStart {
                    reason: SessionStartReason::Startup,
                    previous_session_file: None,
                },
                &context,
                signal,
            )?;
        }
        if self.restore {
            if let Some(model) = self.model.clone() {
                notify_model_select(
                    &self.session,
                    &self.registry,
                    self.mode,
                    model,
                    None,
                    ModelSelectSource::Restore,
                    signal,
                )?;
            }
        }
        if let Some((level, previous)) = self.thinking {
            notify_thinking_level_select(
                &self.session,
                &self.registry,
                self.mode,
                self.model,
                level,
                previous,
                signal,
            )?;
        }
        Ok(())
    }
}

pub(crate) struct ModelSelector {
    settings: ProviderSettings,
    provider_id: String,
    auth_dir: PathBuf,
    automation: AutomationOptions,
}

impl ModelSelector {
    pub(crate) fn prepare(
        &self,
        provider: &str,
        model: &str,
        signal: &CancellationToken,
    ) -> Result<ModelSelection, String> {
        if signal.is_cancelled() {
            return Err("Model selection aborted before commit".into());
        }
        let provider = canonical_provider(provider);
        let mut settings = settings_for(provider, model)?;
        if provider == self.provider_id {
            settings.base_url = self.settings.base_url.clone();
            settings.api_key = self.settings.api_key.clone();
            settings.options = self.settings.options.clone();
            settings.options.provider_id = provider.into();
        }
        settings.options.thinking_level = self
            .settings
            .options
            .thinking_level
            .map(|level| settings.clamp_thinking_level(level));
        load_codex_auth(&mut settings, &self.auth_dir, signal)?;
        if settings.mode != ProviderMode::Demo && settings.model.trim().is_empty() {
            return Err("modelId must not be empty".into());
        }
        settings.validate()?;
        let mut automation = self.automation.clone();
        if self.provider_id != provider || self.settings.model != settings.model {
            automation.model = (settings.mode != ProviderMode::AzureOpenAiResponses)
                .then(|| ModelBudget::from_catalog(provider, &settings.model))
                .flatten();
        }
        if settings.mode == ProviderMode::AmazonBedrock {
            if let (Some(model), Some(max)) =
                (&mut automation.model, settings.options.bedrock.max_tokens)
            {
                model.max_output_tokens = u64::from(max);
            }
        }
        automation.validate()?;
        if signal.is_cancelled() {
            return Err("Model selection aborted before commit".into());
        }
        Ok(ModelSelection {
            settings,
            provider_id: provider.into(),
            budget: automation.model,
        })
    }
}

pub fn settings_for(provider: &str, model: &str) -> Result<ProviderSettings, String> {
    let provider = canonical_provider(provider);
    let model = if let Some(record) = find_canonical(model) {
        if record.provider != provider {
            return Err("Model provider differs from --provider".into());
        }
        record.id.as_str()
    } else {
        model
    };
    if provider != "demo" && provider != "openai-compatible" {
        if let Some(record) = find(provider, model) {
            let mode = record.mode().ok_or_else(|| {
                format!(
                    "{}: {}",
                    record.canonical_id(),
                    record
                        .unsupported_reason()
                        .unwrap_or("Protocol not migrated")
                )
            })?;
            return Ok(ProviderSettings {
                mode,
                base_url: record.base_url.clone(),
                model: record.id.clone(),
                api_key: resolve_api_key(provider),
                options: ProviderOptions {
                    provider_id: provider.into(),
                    azure: AzureOptions::from_env(),
                    vertex: VertexOptions::from_env(),
                    bedrock: BedrockOptions::from_env(),
                    ..Default::default()
                },
            });
        }
    }
    let mode = match provider {
        "demo" => ProviderMode::Demo,
        "openai" | "openai-compatible" => ProviderMode::OpenAiCompatible,
        "openai-responses" => ProviderMode::OpenAiResponses,
        "openai-codex" => ProviderMode::OpenAiCodex,
        "azure-openai-responses" | "azure" => ProviderMode::AzureOpenAiResponses,
        "google-vertex" | "vertex" => ProviderMode::GoogleVertex,
        "amazon-bedrock" | "bedrock" => ProviderMode::AmazonBedrock,
        "anthropic" => ProviderMode::Anthropic,
        "google" | "gemini" => ProviderMode::Google,
        "mistral" | "mistral-conversations" => ProviderMode::Mistral,
        "pi" | "pi-messages" => ProviderMode::PiMessages,
        _ => return Err(format!("Unknown provider: {provider}")),
    };
    Ok(ProviderSettings {
        base_url: mode.default_base_url().into(),
        api_key: resolve_api_key(provider),
        mode,
        model: model.into(),
        options: ProviderOptions {
            provider_id: provider.into(),
            azure: AzureOptions::from_env(),
            vertex: VertexOptions::from_env(),
            bedrock: BedrockOptions::from_env(),
            ..Default::default()
        },
    })
}

fn canonical_provider(provider: &str) -> &str {
    match provider {
        "azure" => "azure-openai-responses",
        "vertex" => "google-vertex",
        "bedrock" => "amazon-bedrock",
        "mistral-conversations" => "mistral",
        "pi" => "pi-messages",
        _ => provider,
    }
}

pub fn provider_name(mode: &ProviderMode) -> &'static str {
    match mode {
        ProviderMode::Demo => "demo",
        ProviderMode::OpenAiCompatible => "openai",
        ProviderMode::OpenAiResponses => "openai-responses",
        ProviderMode::OpenAiCodex => "openai-codex",
        ProviderMode::Anthropic => "anthropic",
        ProviderMode::Google => "google",
        ProviderMode::AzureOpenAiResponses => "azure-openai-responses",
        ProviderMode::GoogleVertex => "google-vertex",
        ProviderMode::AmazonBedrock => "amazon-bedrock",
        ProviderMode::Mistral => "mistral",
        ProviderMode::PiMessages => "pi-messages",
    }
}

pub fn load_native(path: &Path) -> Result<Session, String> {
    if fs::metadata(path).map_err(|e| e.to_string())?.len() > 67_108_864 {
        return Err("Session exceeds 64MB".into());
    }
    serde_json::from_slice(&fs::read(path).map_err(|e| e.to_string())?)
        .map_err(|e| format!("Session decode: {e}"))
}

impl Client {
    pub fn from_args(args: &Args) -> Result<Self, String> {
        Self::from_args_with_signal(args, &CancellationToken::new())
    }

    pub fn from_args_with_signal(args: &Args, signal: &CancellationToken) -> Result<Self, String> {
        let mode = match args.mode {
            crate::args::OutputMode::Rpc => ExtensionMode::Rpc,
            crate::args::OutputMode::Json => ExtensionMode::Json,
            _ if !args.print
                && std::io::IsTerminal::is_terminal(&std::io::stdin())
                && !args.line =>
            {
                ExtensionMode::Tui
            }
            _ => ExtensionMode::Print,
        };
        Self::from_args_for_mode(args, mode, signal)
    }

    /// Constructs a native host with an explicit frontend mode, independent of stdin.
    /// Interactive hosts bind their UI broker before completing startup notifications.
    pub fn from_args_for_mode(
        args: &Args,
        mode: ExtensionMode,
        signal: &CancellationToken,
    ) -> Result<Self, String> {
        let store = SessionStore::new(
            args.session_dir
                .clone()
                .unwrap_or_else(SessionStore::default_directory),
        );
        let cwd = fs::canonicalize(
            args.cwd
                .clone()
                .unwrap_or(std::env::current_dir().map_err(|e| e.to_string())?),
        )
        .map_err(|e| format!("Workspace: {e}"))?;
        if !cwd.is_dir() {
            return Err("Workspace is not a directory".into());
        }
        let mut session = if let Some(path) = &args.import_session {
            load_legacy_session(path)?.import_with_workspace(
                args.leaf
                    .clone()
                    .map_or(LeafSelection::Latest, LeafSelection::Entry),
                args.cwd.as_ref().map(|_| cwd.as_path()),
            )?
        } else if let Some(id) = &args.session {
            store
                .list()?
                .into_iter()
                .find(|session| &session.id == id)
                .ok_or_else(|| format!("Session not found: {id}"))?
        } else if args.continue_session {
            store
                .list()?
                .into_iter()
                .find(|session| fs::canonicalize(&session.workspace).ok().as_ref() == Some(&cwd))
                .ok_or_else(|| "No session in this workspace".to_string())?
        } else {
            Session {
                workspace: cwd.clone(),
                ..Default::default()
            }
        };
        if args.cwd.is_some() {
            session.workspace = cwd;
        }
        if let Some(name) = &args.name {
            session.append_session_info(name)?;
        }
        if let Some(prompt) = &args.system_prompt {
            session.context.system_prompt.clone_from(prompt);
        }
        for suffix in &args.append_system_prompt {
            session.context.system_prompt.push_str("\n\n");
            session.context.system_prompt.push_str(suffix);
        }
        let restored = if args.provider.is_none() && args.model.is_none() {
            stored_model_selection(&session)?
        } else {
            None
        };
        let mut provider_id = args
            .provider
            .clone()
            .or_else(|| {
                args.model
                    .as_ref()
                    .and_then(|model| find_canonical(model))
                    .map(|model| model.provider.clone())
            })
            .or_else(|| restored.as_ref().map(|model| model.provider.clone()))
            .unwrap_or_else(|| "demo".into());
        provider_id = canonical_provider(&provider_id).into();
        let mut settings = settings_for(
            &provider_id,
            args.model
                .as_deref()
                .or_else(|| restored.as_ref().map(|model| model.model.as_str()))
                .unwrap_or(""),
        )?;
        if let Some(url) = &args.base_url {
            settings.base_url.clone_from(url);
            settings.options.bedrock.endpoint_explicit = true;
        }
        if let Some(key) = &args.api_key {
            settings.api_key.clone_from(key);
            settings.options.api_key_explicit = true;
        }
        if let Some(resource) = &args.azure_resource {
            settings.options.azure.resource_name.clone_from(resource);
        }
        if args.azure_resource.is_some() && args.base_url.is_none() {
            settings.base_url.clear();
            settings.options.azure.base_url.clear();
        }
        if let Some(deployment) = &args.azure_deployment {
            settings.options.azure.deployment.clone_from(deployment);
        }
        if let Some(version) = &args.azure_api_version {
            settings.options.azure.api_version.clone_from(version);
        }
        if let Some(token) = &args.azure_token {
            settings.options.azure.bearer_token.clone_from(token);
        }
        if let Some(project) = &args.vertex_project {
            settings.options.vertex.project.clone_from(project);
        }
        if let Some(location) = &args.vertex_location {
            settings.options.vertex.location.clone_from(location);
        }
        if let Some(path) = &args.vertex_credentials {
            settings.options.vertex.adc_path = Some(path.clone());
        }
        if let Some(token) = &args.vertex_token {
            settings.options.vertex.access_token.clone_from(token);
        }
        if let Some(region) = &args.aws_region {
            settings.options.bedrock.region.clone_from(region);
        }
        if let Some(profile) = &args.aws_profile {
            settings.options.bedrock.profile.clone_from(profile);
        }
        if let Some(path) = &args.aws_credentials_file {
            settings.options.bedrock.credentials_file = Some(path.clone());
        }
        if let Some(path) = &args.aws_config_file {
            settings.options.bedrock.config_file = Some(path.clone());
        }
        if let Some(endpoint) = &args.aws_sts_endpoint {
            settings.options.bedrock.sts_endpoint.clone_from(endpoint);
        }
        if let Some(path) = &args.aws_sso_cache_dir {
            settings.options.bedrock.sso_cache_dir = Some(path.clone());
        }
        if let Some(endpoint) = &args.aws_sso_endpoint {
            settings.options.bedrock.sso_endpoint.clone_from(endpoint);
        }
        if let Some(endpoint) = &args.aws_sso_oidc_endpoint {
            settings
                .options
                .bedrock
                .sso_oidc_endpoint
                .clone_from(endpoint);
        }
        if let Some(key) = &args.aws_access_key_id {
            settings.options.bedrock.access_key_id.clone_from(key);
        }
        if let Some(key) = &args.aws_secret_access_key {
            settings.options.bedrock.secret_access_key.clone_from(key);
        }
        if let Some(token) = &args.aws_session_token {
            settings.options.bedrock.session_token.clone_from(token);
        }
        if let Some(token) = &args.bedrock_token {
            settings.options.bedrock.bearer_token.clone_from(token);
        }
        settings.options.bedrock.max_tokens = args.bedrock_max_tokens;
        settings.options.bedrock.temperature = args.bedrock_temperature;
        if let Some(cache) = &args.bedrock_cache {
            settings.options.bedrock.cache_retention = match cache.as_str() {
                "none" => BedrockCacheRetention::None,
                "long" => BedrockCacheRetention::Long,
                _ => BedrockCacheRetention::Short,
            };
        }
        if settings.mode == ProviderMode::AmazonBedrock
            && args.api_key.is_none()
            && (args.aws_access_key_id.is_some()
                || args.aws_secret_access_key.is_some()
                || args.bedrock_token.is_some())
        {
            settings.api_key.clear();
        }
        if args.api_key.is_some() && args.azure_token.is_none() {
            settings.options.azure.bearer_token.clear();
        }
        if args.api_key.is_none()
            && ((settings.mode == ProviderMode::AzureOpenAiResponses
                && !settings.options.azure.bearer_token.is_empty())
                || (settings.mode == ProviderMode::GoogleVertex
                    && (args.vertex_token.is_some() || args.vertex_credentials.is_some())))
        {
            settings.api_key.clear();
        }
        if let Some(level) = args.thinking {
            let level = settings.clamp_thinking_level(level);
            settings.options.thinking_level = Some(level);
            session.append_thinking_level(level)?;
        } else {
            restore_thinking_settings(&session, &mut settings)?;
        }
        let auth_dir = args.auth_dir.clone().unwrap_or_else(default_auth_directory);
        let resource_options = ResourceOptions {
            cwd: session.workspace.clone(),
            agent_dir: args
                .agent_dir
                .clone()
                .unwrap_or_else(|| ResourceOptions::default().agent_dir),
            skill_paths: args.skills.clone(),
            prompt_paths: args.prompts.clone(),
            include_defaults: true,
        };
        let mut resources = if args.no_skills && args.no_prompts {
            ResourceLoad::default()
        } else {
            load_resources(&resource_options)
        };
        if args.no_skills {
            resources.skills.clear();
        }
        if args.no_prompts {
            resources.prompts.clear();
        }
        for diagnostic in &resources.diagnostics {
            eprintln!(
                "Resource: {}: {}",
                diagnostic.path.display(),
                diagnostic.message
            );
        }
        let mut options = args.run.clone();
        options.automation.model = if settings.mode == ProviderMode::AzureOpenAiResponses {
            None
        } else {
            ModelBudget::from_catalog(&provider_id, &settings.model)
                .or_else(|| options.automation.model.clone())
        };
        if let Some(context_window) = args.context_window {
            options.automation.model = Some(ModelBudget {
                provider: provider_id.clone(),
                id: settings.model.clone(),
                context_window,
                max_output_tokens: args.max_output_tokens.unwrap_or(0),
                cost: serde_json::Value::Null,
            });
        } else if let Some(max_output_tokens) = args.max_output_tokens {
            options
                .automation
                .model
                .as_mut()
                .ok_or("--max-output-tokens requires --context-window or a catalog model")?
                .max_output_tokens = max_output_tokens;
        }
        if settings.mode == ProviderMode::AmazonBedrock && args.max_output_tokens.is_none() {
            if let (Some(model), Some(max)) =
                (&mut options.automation.model, args.bedrock_max_tokens)
            {
                model.max_output_tokens = u64::from(max);
            }
        }
        options.extension_mode = mode;
        options.extension_model = Some(ModelIdentity {
            provider: provider_id.clone(),
            model: settings.model.clone(),
        });
        if !args.no_context_files {
            let context = load_context_files(&session.workspace);
            for diagnostic in context.diagnostics {
                eprintln!("{diagnostic}");
            }
            options.system_prompt_suffix = context.text;
        }
        options
            .system_prompt_suffix
            .push_str(&format_skills_for_prompt(&resources.skills));
        let mut client = Self {
            session,
            settings,
            provider_id,
            auth_dir,
            options,
            queues: MessageQueues::default(),
            store: (!args.no_session).then_some(store),
            resources,
            extension_paths: args.extensions.clone(),
            last_command: None,
            input_handled: false,
            model_notification_pending: false,
            session_start_pending: false,
            thinking_notification: None,
            resource_options,
            load_context: !args.no_context_files,
            no_skills: args.no_skills,
            no_prompts: args.no_prompts,
        };
        let context = client.extension_context()?;
        client.options.extensions =
            load_native_extensions(&client.extension_paths, &context, signal)?;
        if matches!(
            client.options.extension_mode,
            ExtensionMode::Tui | ExtensionMode::Rpc
        ) && !client.options.extensions.ids().is_empty()
        {
            client.session_start_pending = true;
            client.model_notification_pending =
                !client.settings.model.is_empty() || client.provider_id == "demo";
            return Ok(client);
        }
        client.emit_lifecycle(
            LifecycleEvent::SessionStart {
                reason: SessionStartReason::Startup,
                previous_session_file: None,
            },
            signal,
        )?;
        if client.options.extension_mode == ExtensionMode::Rpc
            && !client.options.extensions.ids().is_empty()
            && (!client.settings.model.is_empty() || client.provider_id == "demo")
        {
            client.model_notification_pending = true;
        } else {
            client.notify_model_selection(None, ModelSelectSource::Restore, signal)?;
        }
        Ok(client)
    }
    pub(crate) fn take_startup_notifications(&mut self) -> StartupNotifications {
        let notifications = StartupNotifications {
            session: self.session.clone(),
            registry: self.options.extensions.clone(),
            mode: self.options.extension_mode,
            model: self.options.extension_model.clone(),
            start: self.session_start_pending,
            restore: self.model_notification_pending,
            thinking: self.take_thinking_notification(),
        };
        self.session_start_pending = false;
        self.model_notification_pending = false;
        notifications
    }
    pub(crate) fn complete_startup_notifications(
        &mut self,
        signal: &CancellationToken,
    ) -> Result<(), String> {
        self.take_startup_notifications().complete(signal)
    }

    pub fn prepare_text(&self, text: &str) -> Result<String, String> {
        if self.extension_command(text).is_some()
            || matches!(text.trim(), "/extensions" | "/reload")
        {
            return Ok(text.into());
        }
        prepare_prompt(text, &self.resources)
    }

    pub fn extension_context(&self) -> Result<ExtensionContext, String> {
        extension_context(
            &self.session,
            &self.options.extensions,
            self.options.extension_mode,
            self.options.extension_model.clone(),
        )
    }
    pub fn extension_command<'a>(&self, text: &'a str) -> Option<(&'a str, &'a str)> {
        let text = text.trim_start().strip_prefix('/')?;
        let (name, args) = text.split_once(char::is_whitespace).unwrap_or((text, ""));
        self.options
            .extensions
            .contains_command(name)
            .then_some((name, args.trim_start()))
    }
    pub fn emit_lifecycle(
        &self,
        event: LifecycleEvent,
        signal: &CancellationToken,
    ) -> Result<(), String> {
        if matches!(event, LifecycleEvent::SessionStart { .. }) {
            self.options
                .extensions
                .emit_event_checked(&event, &self.extension_context()?, signal)
        } else {
            self.options
                .extensions
                .emit_event(&event, &self.extension_context()?, signal)
        }
    }
    pub fn reload_extensions(&mut self, signal: &CancellationToken) -> Result<(), String> {
        let registry =
            load_native_extensions(&self.extension_paths, &self.extension_context()?, signal)?;
        let context = extension_context(
            &self.session,
            &registry,
            self.options.extension_mode,
            self.options.extension_model.clone(),
        )?;
        registry.emit_event_checked(
            &LifecycleEvent::SessionStart {
                reason: SessionStartReason::Reload,
                previous_session_file: None,
            },
            &context,
            signal,
        )?;
        if signal.is_cancelled() {
            return Err("Extension reload cancelled".into());
        }
        self.emit_lifecycle(
            LifecycleEvent::SessionShutdown {
                reason: SessionShutdownReason::Reload,
                target_session_file: None,
            },
            &CancellationToken::new(),
        )?;
        self.options.extensions = registry;
        self.notify_model_selection(None, ModelSelectSource::Restore, signal)
    }
    pub fn execute_command(
        &mut self,
        name: &str,
        args: &str,
        signal: &CancellationToken,
    ) -> Result<CommandResult, String> {
        let mut candidate = self.session.clone();
        let result = execute_extension_command(
            &mut candidate,
            &self.options.extensions,
            name,
            args,
            &self.extension_context()?,
            signal,
        )?;
        if let Some(store) = &self.store {
            store.save(&candidate)?;
        }
        self.session = candidate;
        self.last_command = Some(result.clone());
        if result.session_name.is_some() {
            self.notify_session_name(signal)?;
        }
        Ok(result)
    }

    pub fn set_session_name(
        &mut self,
        name: &str,
        signal: &CancellationToken,
    ) -> Result<(), String> {
        let mut candidate = self.session.clone();
        candidate.append_session_info(name)?;
        if signal.is_cancelled() {
            return Err("Session name cancelled before commit".into());
        }
        if let Some(store) = &self.store {
            store.save(&candidate)?;
        }
        self.session = candidate;
        self.notify_session_name(signal)
    }

    pub(crate) fn notify_session_name(&self, signal: &CancellationToken) -> Result<(), String> {
        notify_session_info_changed(
            &self.session,
            &self.options.extensions,
            self.options.extension_mode,
            self.options.extension_model.clone(),
            signal,
        )
        .map_err(|error| format!("Session name committed; notification failed: {error}"))
    }

    pub fn before_session_change(
        &self,
        event: &SessionBeforeEvent,
        signal: &CancellationToken,
    ) -> Result<(), String> {
        if !prepare_session_change(
            &self.session,
            &self.options.extensions,
            self.options.extension_mode,
            self.options.extension_model.clone(),
            event,
            signal,
        )? {
            return Err("Session change cancelled by extension".into());
        }
        Ok(())
    }

    pub fn message(&self, text: &str, files: &[PathBuf]) -> Result<Message, String> {
        let mut content = vec![];
        let tools = create_tools(&self.session.workspace, false, false);
        let read = tools
            .iter()
            .find(|tool| tool.name() == "read")
            .ok_or("Read tool missing")?;
        for path in files {
            let result = read.execute(
                "attachment",
                &serde_json::json!({"path":path}),
                &CancellationToken::new(),
                &mut |_| {},
            )?;
            content.push(Content::text(format!("Attached file: {}", path.display())));
            content.extend(result.content);
        }
        content.push(Content::text(text));
        Ok(Message::User(UserMessage {
            content,
            timestamp: timestamp(),
        }))
    }

    pub fn run(
        &mut self,
        prompt: Message,
        signal: &CancellationToken,
        emit: &mut dyn FnMut(AgentEvent),
    ) -> Result<(), String> {
        self.last_command = None;
        self.input_handled = false;
        if let Message::User(user) = &prompt {
            if let [Content::Text { text }] = user.content.as_slice() {
                if let Some((name, args)) = self.extension_command(text) {
                    return self.execute_command(name, args, signal).map(|_| ());
                }
                if text.trim() == "/extensions" {
                    self.last_command=Some(CommandResult{text:serde_json::json!({"extensions":self.options.extensions.ids(),"commands":self.options.extensions.commands()}).to_string(),..Default::default()});
                    return Ok(());
                }
                let (builtin, value) = text
                    .trim()
                    .split_once(char::is_whitespace)
                    .unwrap_or((text.trim(), ""));
                if builtin == "/name" {
                    self.set_session_name(value, signal)?;
                    self.last_command = Some(CommandResult {
                        text: self.session.get_session_name()?.map_or_else(
                            || "Session name cleared".into(),
                            |name| format!("Session name: {name}"),
                        ),
                        ..Default::default()
                    });
                    return Ok(());
                }
                if builtin == "/thinking" {
                    let level = match value.trim() {
                        "" => None,
                        "cycle" => self.next_thinking_level(),
                        value => Some(value.parse()?),
                    };
                    if let Some(level) = level {
                        self.set_thinking_level(level, signal)?;
                    }
                    self.last_command = Some(CommandResult {
                        text: format!(
                            "Thinking: {} (available: {})",
                            self.thinking_level(),
                            self.available_thinking_levels()
                                .iter()
                                .map(ToString::to_string)
                                .collect::<Vec<_>>()
                                .join(", ")
                        ),
                        ..Default::default()
                    });
                    return Ok(());
                }
                if builtin == "/model" {
                    if value.trim().is_empty() {
                        return Err(
                            "Usage: /model PROVIDER MODEL_ID (MODEL_ID optional for demo)".into(),
                        );
                    }
                    let (provider, model) = value
                        .trim()
                        .split_once(char::is_whitespace)
                        .unwrap_or((value.trim(), ""));
                    self.choose_model(provider, model.trim(), signal)?;
                    self.last_command = Some(CommandResult {
                        text: format!("Model: {}/{}", self.provider_id, self.settings.model),
                        ..Default::default()
                    });
                    return Ok(());
                }
                if builtin == "/navigate" {
                    let target = value.trim();
                    let mut session = self.session.clone();
                    session.navigate((!target.is_empty() && target != "root").then_some(target))?;
                    self.replace_selected_branch(session, signal)?;
                    self.last_command = Some(CommandResult {
                        text: "Branch selected".into(),
                        ..Default::default()
                    });
                    return Ok(());
                }
                if builtin == "/fork" {
                    let target = value.trim();
                    if target.is_empty() {
                        return Err("Usage: /fork USER_ENTRY_ID".into());
                    }
                    self.before_session_change(
                        &SessionBeforeEvent::Fork {
                            entry_id: target.into(),
                            position: ForkPosition::Before,
                        },
                        signal,
                    )?;
                    let mut session = self.session.clone();
                    let draft = session.fork(target.trim())?;
                    self.replace_selected_branch(session, signal)?;
                    self.last_command = Some(CommandResult {
                        text: draft,
                        ..Default::default()
                    });
                    return Ok(());
                }
                if text.trim() == "/reload" {
                    self.reload_extensions(signal)?;
                    self.last_command = Some(CommandResult {
                        text: "Extensions reloaded".into(),
                        ..Default::default()
                    });
                    return Ok(());
                }
                if text.trim() == "/new" {
                    self.new_session_with_signal(signal)?;
                    self.last_command = Some(CommandResult {
                        text: "New session".into(),
                        ..Default::default()
                    });
                    return Ok(());
                }
                if builtin == "/resume" {
                    let id = value.trim();
                    if id.is_empty() {
                        return Err("Usage: /resume SESSION_ID".into());
                    }
                    self.resume_with_signal(id.trim(), signal)?;
                    self.last_command = Some(CommandResult {
                        text: "Session resumed".into(),
                        ..Default::default()
                    });
                    return Ok(());
                }
                if text.trim() == "/clone" {
                    let session = self.session.clone_current_branch()?;
                    self.replace_session_with_signal(
                        session,
                        SessionStartReason::Fork,
                        SessionShutdownReason::Fork,
                        signal,
                    )?;
                    self.save()?;
                    self.last_command = Some(CommandResult {
                        text: "Session cloned".into(),
                        ..Default::default()
                    });
                    return Ok(());
                }
            }
        }
        let source = if self.options.extension_mode == ExtensionMode::Rpc {
            InputSource::Rpc
        } else {
            InputSource::Interactive
        };
        let Some(prompt) = prepare_request_input(
            &self.session,
            &self.options,
            &self.resources,
            prompt,
            source,
            None,
            signal,
        )?
        else {
            self.input_handled = true;
            return Ok(());
        };
        self.run_prepared(prompt, signal, emit)
    }
    pub fn run_prepared(
        &mut self,
        prompt: Message,
        signal: &CancellationToken,
        emit: &mut dyn FnMut(AgentEvent),
    ) -> Result<(), String> {
        self.last_command = None;
        self.input_handled = false;
        self.preflight(signal)?;
        let result = run_prompt(
            &mut self.session,
            prompt,
            &self.settings,
            &self.options,
            &self.queues,
            signal,
            emit,
        );
        self.save()?;
        result
    }
    pub fn save(&self) -> Result<(), String> {
        if let Some(store) = &self.store {
            store.save(&self.session)?;
        }
        Ok(())
    }
    pub fn compact(
        &mut self,
        keep_recent: usize,
        signal: &CancellationToken,
    ) -> Result<CompactionResult, String> {
        if self.settings.mode == ProviderMode::Demo {
            return Err("Compaction requires a configured model provider".into());
        }
        self.preflight(signal)?;
        let mut provider = ModelProvider {
            settings: self.settings.clone(),
            tool_definitions: vec![],
        };
        let result = compact_with_accounting(
            &mut self.session,
            &mut provider,
            &self.options,
            keep_recent,
            signal,
        );
        self.settings.options.oauth = provider.settings.options.oauth;
        let result = result?;
        self.save()?;
        Ok(result)
    }

    pub fn preflight(&mut self, signal: &CancellationToken) -> Result<(), String> {
        load_codex_auth(&mut self.settings, &self.auth_dir, signal)?;
        self.settings.validate()?;
        self.options.automation.validate()
    }

    pub fn choose_model(
        &mut self,
        provider: &str,
        model: &str,
        signal: &CancellationToken,
    ) -> Result<(), String> {
        let selection = self.prepare_model_selection(provider, model, signal)?;
        let previous = self.commit_model_selection(selection, signal)?;
        self.notify_model_selection(previous, ModelSelectSource::Set, signal)
    }
    pub fn thinking_level(&self) -> ThinkingLevel {
        self.settings.options.thinking_level.unwrap_or_default()
    }
    pub fn available_thinking_levels(&self) -> Vec<ThinkingLevel> {
        self.settings.supported_thinking_levels()
    }
    pub fn next_thinking_level(&self) -> Option<ThinkingLevel> {
        let levels = self.available_thinking_levels();
        if levels.iter().all(|level| *level == ThinkingLevel::Off) {
            return None;
        }
        let index = levels
            .iter()
            .position(|level| *level == self.thinking_level())
            .map_or(0, |index| (index + 1) % levels.len());
        levels.get(index).copied()
    }
    pub fn set_thinking_level(
        &mut self,
        level: ThinkingLevel,
        signal: &CancellationToken,
    ) -> Result<(), String> {
        let selection = prepare_thinking_selection(&self.session, &self.settings, level, signal)?;
        self.commit_thinking_selection(selection, signal)?;
        self.notify_thinking_selection(signal)
    }
    pub(crate) fn commit_thinking_selection(
        &mut self,
        selection: agent_runtime::thinking::ThinkingSelection,
        signal: &CancellationToken,
    ) -> Result<(), String> {
        if signal.is_cancelled() {
            return Err("Thinking selection aborted before commit".into());
        }
        if let Some(store) = &self.store {
            store.save(&selection.session)?;
        }
        self.session = selection.session;
        self.settings = selection.settings;
        self.thinking_notification = (selection.level != selection.previous_level)
            .then_some((selection.level, selection.previous_level));
        Ok(())
    }
    pub(crate) fn take_thinking_notification(&mut self) -> Option<(ThinkingLevel, ThinkingLevel)> {
        self.thinking_notification.take()
    }
    pub(crate) fn apply_thinking_settings(&mut self, settings: ProviderSettings) {
        let previous = self.thinking_level();
        self.settings = settings;
        let level = self.thinking_level();
        self.thinking_notification = (level != previous).then_some((level, previous));
    }
    fn notify_thinking_selection(&mut self, signal: &CancellationToken) -> Result<(), String> {
        if let Some((level, previous)) = self.take_thinking_notification() {
            notify_thinking_level_select(
                &self.session,
                &self.options.extensions,
                self.options.extension_mode,
                self.options.extension_model.clone(),
                level,
                previous,
                signal,
            )
            .map_err(|error| {
                format!("Thinking selection committed; notification failed: {error}")
            })?;
        }
        Ok(())
    }

    pub(crate) fn prepare_model_selection(
        &self,
        provider: &str,
        model: &str,
        signal: &CancellationToken,
    ) -> Result<ModelSelection, String> {
        self.model_selector().prepare(provider, model, signal)
    }

    pub(crate) fn model_selector(&self) -> ModelSelector {
        ModelSelector {
            settings: self.settings.clone(),
            provider_id: self.provider_id.clone(),
            auth_dir: self.auth_dir.clone(),
            automation: self.options.automation.clone(),
        }
    }

    pub(crate) fn commit_model_selection(
        &mut self,
        selection: ModelSelection,
        signal: &CancellationToken,
    ) -> Result<Option<ModelIdentity>, String> {
        let identity = ModelIdentity {
            provider: selection.provider_id.clone(),
            model: selection.settings.model.clone(),
        };
        let previous = (!self.settings.model.is_empty() || self.provider_id == "demo").then(|| {
            ModelIdentity {
                provider: self.provider_id.clone(),
                model: self.settings.model.clone(),
            }
        });
        let mut session = self.session.clone();
        record_model_selection(&mut session, &identity)?;
        if let Some(level) = selection.settings.options.thinking_level {
            session.append_thinking_level(level)?;
        }
        if signal.is_cancelled() {
            return Err("Model selection aborted before commit".into());
        }
        if let Some(store) = &self.store {
            store.save(&session)?;
        }
        // Saving and swapping are the commit: later notification cancellation is not a veto.
        self.session = session;
        let previous_level = self.thinking_level();
        self.settings = selection.settings;
        let level = self.thinking_level();
        self.thinking_notification = (level != previous_level).then_some((level, previous_level));
        self.provider_id = selection.provider_id;
        self.options.automation.model = selection.budget;
        self.options.extension_model = Some(identity);
        Ok(previous)
    }

    pub(crate) fn notify_model_selection(
        &mut self,
        previous: Option<ModelIdentity>,
        source: ModelSelectSource,
        signal: &CancellationToken,
    ) -> Result<(), String> {
        self.notify_thinking_selection(signal)?;
        if self.settings.model.is_empty() && self.provider_id != "demo" {
            return Ok(());
        }
        notify_model_select(
            &self.session,
            &self.options.extensions,
            self.options.extension_mode,
            ModelIdentity {
                provider: self.provider_id.clone(),
                model: self.settings.model.clone(),
            },
            previous,
            source,
            signal,
        )
    }

    pub(crate) fn replace_selected_branch(
        &mut self,
        mut session: Session,
        signal: &CancellationToken,
    ) -> Result<(), String> {
        let mut selection = stored_model_selection(&session)?
            .filter(|model| {
                model.provider != self.provider_id || model.model != self.settings.model
            })
            .map(|model| self.prepare_model_selection(&model.provider, &model.model, signal))
            .transpose()?;
        let mut target_settings = selection.as_ref().map_or_else(
            || self.settings.clone(),
            |selection| selection.settings.clone(),
        );
        restore_thinking_settings(&session, &mut target_settings)?;
        if let Some(level) = target_settings.options.thinking_level {
            session.append_thinking_level(level)?;
        }
        if let Some(selection) = &mut selection {
            selection.settings = target_settings.clone();
        }
        if signal.is_cancelled() {
            return Err("Branch selection aborted before commit".into());
        }
        if let Some(store) = &self.store {
            store.save(&session)?;
        }
        let previous = (!self.settings.model.is_empty() || self.provider_id == "demo").then(|| {
            ModelIdentity {
                provider: self.provider_id.clone(),
                model: self.settings.model.clone(),
            }
        });
        self.session = session;
        self.queues.clear();
        self.refresh_resources();
        if let Some(selection) = selection {
            self.apply_model_selection(selection);
            return self.notify_model_selection(previous, ModelSelectSource::Restore, signal);
        }
        let previous_level = self.thinking_level();
        self.settings = target_settings;
        let level = self.thinking_level();
        self.thinking_notification = (level != previous_level).then_some((level, previous_level));
        self.notify_thinking_selection(signal)
    }

    pub(crate) fn apply_model_selection(&mut self, selection: ModelSelection) {
        let previous_level = self.thinking_level();
        self.options.extension_model = Some(ModelIdentity {
            provider: selection.provider_id.clone(),
            model: selection.settings.model.clone(),
        });
        self.settings = selection.settings;
        let level = self.thinking_level();
        self.thinking_notification = (level != previous_level).then_some((level, previous_level));
        self.provider_id = selection.provider_id;
        self.options.automation.model = selection.budget;
    }
    pub fn refresh_resources(&mut self) {
        self.resource_options.cwd = self.session.workspace.clone();
        self.resources = if self.no_skills && self.no_prompts {
            ResourceLoad::default()
        } else {
            load_resources(&self.resource_options)
        };
        if self.no_skills {
            self.resources.skills.clear();
        }
        if self.no_prompts {
            self.resources.prompts.clear();
        }
        self.options.system_prompt_suffix = if self.load_context {
            load_context_files(&self.session.workspace).text
        } else {
            String::new()
        };
        self.options
            .system_prompt_suffix
            .push_str(&format_skills_for_prompt(&self.resources.skills));
    }
    pub fn new_session(&mut self) -> Result<(), String> {
        self.new_session_with_signal(&CancellationToken::new())
    }
    pub fn new_session_with_signal(&mut self, signal: &CancellationToken) -> Result<(), String> {
        let mut session = Session {
            workspace: self.session.workspace.clone(),
            context: agent_core::types::Context {
                system_prompt: self.session.context.system_prompt.clone(),
                messages: vec![],
            },
            ..Default::default()
        };
        if !self.settings.model.is_empty() || self.provider_id == "demo" {
            record_model_selection(
                &mut session,
                &ModelIdentity {
                    provider: self.provider_id.clone(),
                    model: self.settings.model.clone(),
                },
            )?;
        }
        if let Some(level) = self.settings.options.thinking_level {
            session.append_thinking_level(level)?;
        }
        self.replace_session_with_signal(
            session,
            SessionStartReason::New,
            SessionShutdownReason::New,
            signal,
        )?;
        self.save()
    }
    pub fn resume(&mut self, id: &str) -> Result<(), String> {
        self.resume_with_signal(id, &CancellationToken::new())
    }
    pub fn resume_with_signal(
        &mut self,
        id: &str,
        signal: &CancellationToken,
    ) -> Result<(), String> {
        let session = self
            .store
            .as_ref()
            .ok_or("Session storage disabled")?
            .list()?
            .into_iter()
            .find(|session| session.id == id)
            .ok_or_else(|| format!("Session not found: {id}"))?;
        self.replace_session_with_signal(
            session,
            SessionStartReason::Resume,
            SessionShutdownReason::Resume,
            signal,
        )
    }
    pub fn replace_session(
        &mut self,
        session: Session,
        start: SessionStartReason,
        shutdown: SessionShutdownReason,
    ) -> Result<(), String> {
        self.replace_session_with_signal(session, start, shutdown, &CancellationToken::new())
    }
    pub fn replace_session_with_signal(
        &mut self,
        mut session: Session,
        start: SessionStartReason,
        shutdown: SessionShutdownReason,
        signal: &CancellationToken,
    ) -> Result<(), String> {
        let reason = match start {
            SessionStartReason::New => Some(SessionSwitchReason::New),
            SessionStartReason::Resume => Some(SessionSwitchReason::Resume),
            _ => None,
        };
        if let Some(reason) = reason {
            self.before_session_change(
                &SessionBeforeEvent::Switch {
                    reason,
                    target_session_file: if reason == SessionSwitchReason::Resume {
                        self.store
                            .as_ref()
                            .map(|store| store.path(&session.id))
                            .transpose()?
                    } else {
                        None
                    },
                },
                signal,
            )?;
        }
        let mut selection = stored_model_selection(&session)?
            .filter(|model| {
                model.provider != self.provider_id || model.model != self.settings.model
            })
            .map(|model| self.prepare_model_selection(&model.provider, &model.model, signal))
            .transpose()?;
        let mut target_settings = selection.as_ref().map_or_else(
            || self.settings.clone(),
            |selection| selection.settings.clone(),
        );
        restore_thinking_settings(&session, &mut target_settings)?;
        if let Some(level) = target_settings.options.thinking_level {
            session.append_thinking_level(level)?;
        }
        if let Some(selection) = &mut selection {
            selection.settings = target_settings.clone();
        }
        let model = selection
            .as_ref()
            .map(|selection| ModelIdentity {
                provider: selection.provider_id.clone(),
                model: selection.settings.model.clone(),
            })
            .or_else(|| self.options.extension_model.clone());
        let context = extension_context(
            &session,
            &self.options.extensions,
            self.options.extension_mode,
            model.clone(),
        )?;
        let registry = if self.extension_paths.is_empty() {
            self.options.extensions.clone()
        } else {
            load_native_extensions(&self.extension_paths, &context, signal)?
        };
        let fresh = extension_context(&session, &registry, self.options.extension_mode, model)?;
        registry.emit_event_checked(
            &LifecycleEvent::SessionStart {
                reason: start,
                previous_session_file: None,
            },
            &fresh,
            signal,
        )?;
        if signal.is_cancelled() {
            return Err("Session transition cancelled".into());
        }
        self.save()?;
        if let Some(store) = &self.store {
            store.save(&session)?;
        }
        self.emit_lifecycle(
            LifecycleEvent::SessionShutdown {
                reason: shutdown,
                target_session_file: None,
            },
            &CancellationToken::new(),
        )?;
        let previous = (!self.settings.model.is_empty() || self.provider_id == "demo").then(|| {
            ModelIdentity {
                provider: self.provider_id.clone(),
                model: self.settings.model.clone(),
            }
        });
        self.session = session;
        self.queues.clear();
        self.refresh_resources();
        self.options.extensions = registry;
        if let Some(selection) = selection {
            self.apply_model_selection(selection);
        } else {
            let previous_level = self.thinking_level();
            self.settings = target_settings;
            let level = self.thinking_level();
            self.thinking_notification =
                (level != previous_level).then_some((level, previous_level));
        }
        self.notify_model_selection(previous, ModelSelectSource::Restore, signal)
    }
}

pub(crate) fn prepare_request_input(
    session: &Session,
    options: &RunOptions,
    resources: &ResourceLoad,
    prompt: Message,
    source: InputSource,
    behavior: Option<InputStreamingBehavior>,
    signal: &CancellationToken,
) -> Result<Option<Message>, String> {
    let has_extensions = !options.extensions.ids().is_empty();
    let mut prompt = if has_extensions {
        let context = extension_context(
            session,
            &options.extensions,
            options.extension_mode,
            options.extension_model.clone(),
        )?;
        let Some(prompt) = prepare_input_with_behavior(
            session,
            &options.extensions,
            prompt,
            source,
            &context,
            signal,
            behavior,
        )?
        else {
            return Ok(None);
        };
        prompt
    } else {
        prompt
    };
    if let Message::User(user) = &mut prompt {
        if let Some(Content::Text { text }) = user
            .content
            .iter_mut()
            .rfind(|block| matches!(block, Content::Text { .. }))
        {
            *text = prepare_prompt(text, resources)?;
            if text.len() > 1_048_576 {
                return Err("Expanded prompt exceeds 1MB".into());
            }
        }
    }
    if has_extensions && signal.is_cancelled() {
        return Err("Input preparation aborted".into());
    }
    Ok(Some(prompt))
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.emit_lifecycle(
            LifecycleEvent::SessionShutdown {
                reason: SessionShutdownReason::Quit,
                target_session_file: None,
            },
            &CancellationToken::new(),
        );
    }
}

pub(crate) fn compact_with_accounting(
    session: &mut Session,
    provider: &mut dyn Provider,
    options: &RunOptions,
    keep_recent: usize,
    signal: &CancellationToken,
) -> Result<CompactionResult, String> {
    let before = session.clone();
    let mut retry = RetryingProvider::new(
        provider,
        options.automation.retry.clone(),
        options.automation_state.clone(),
        "compaction",
        None,
    );
    let result = compact_session(session, &mut retry, keep_recent, signal)?;
    if let Err(error) = record_manual_checkpoint(
        session,
        &before,
        &result,
        options.automation.model.as_ref(),
        &options.automation_state,
    ) {
        *session = before;
        return Err(error);
    }
    Ok(result)
}

pub(crate) fn load_codex_auth(
    settings: &mut ProviderSettings,
    directory: &Path,
    signal: &CancellationToken,
) -> Result<(), String> {
    settings.options.auth_storage_path = Some(auth_store_for(directory).path().to_owned());
    if settings.mode == ProviderMode::OpenAiCodex {
        let store = auth_store_for(directory);
        settings.options.oauth = Some(store.ensure_codex(
            &CodexOAuthClient::new(settings.options.codex_oauth.clone()),
            signal,
        )?);
        settings.options.auth_storage_path = Some(store.path().to_owned());
    }
    Ok(())
}

pub fn nonempty_name(name: &str) -> Result<&str, String> {
    let name = name.trim();
    if name.is_empty() {
        Err("Session name must not be empty".into())
    } else {
        Ok(name)
    }
}
