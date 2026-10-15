use crate::auth::{
    codex_account_id, CodexOAuthClient, CodexOAuthConfig, NativeOAuthClient, NativeOAuthConfig,
    OAuthCredential,
};
use crate::auth_storage::AuthStorage;
use agent_core::agent_loop::Provider;
use agent_core::types::{
    AssistantMessage, CancellationToken, Content, Context, Message, StopReason, ThinkingLevel,
    ToolCall, Usage,
};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, BufRead, BufReader, Cursor, Read};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::Duration;

#[path = "provider_anthropic.rs"]
mod anthropic;
#[path = "provider_aws.rs"]
mod aws;
#[path = "provider_azure.rs"]
mod azure;
#[path = "provider_bedrock.rs"]
mod bedrock;
#[path = "provider_bedrock_stream.rs"]
mod bedrock_stream;
#[path = "provider_google.rs"]
mod google;
#[path = "provider_mistral.rs"]
mod mistral;
#[path = "provider_pi_messages.rs"]
mod pi_messages;
#[path = "provider_responses.rs"]
mod responses;
#[path = "provider_routed_auth.rs"]
mod routed_auth;
#[path = "provider_sse.rs"]
mod sse;
#[path = "provider_thinking.rs"]
mod thinking;
#[path = "provider_vertex.rs"]
mod vertex;
pub use anthropic::{anthropic_request_payload, parse_anthropic_sse};
pub use aws::{aws_timestamp, sign_aws_v4, AwsCredentials, SigV4Parameters, SigV4Signature};
pub use azure::{azure_request_payload, parse_azure_sse, AzureOptions};
pub use bedrock::{
    bedrock_request_payload, parse_bedrock_stream, BedrockCacheRetention, BedrockOptions,
    ResolvedBedrock,
};
pub use bedrock_stream::{read_eventstream_frame, EventStreamFrame, EventStreamHeader};
pub use google::{google_request_payload, parse_google_sse};
pub use mistral::{mistral_request_payload, parse_mistral_sse, MistralOptions};
pub use pi_messages::{parse_pi_messages_sse, pi_messages_request_payload, PiMessagesOptions};
pub use responses::{parse_codex_sse, parse_responses_sse, responses_request_payload};
pub use routed_auth::{CloudflareOptions, CopilotOptions};
pub use thinking::{apply_thinking, ThinkingCapabilities};
pub use vertex::{
    parse_vertex_sse, service_account_assertion, vertex_request_payload, VertexOptions,
};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum ProviderMode {
    #[default]
    Demo,
    OpenAiCompatible,
    OpenAiResponses,
    OpenAiCodex,
    Anthropic,
    Google,
    AzureOpenAiResponses,
    GoogleVertex,
    AmazonBedrock,
    Mistral,
    PiMessages,
}

impl ProviderMode {
    pub fn default_base_url(&self) -> &'static str {
        match self {
            Self::Demo | Self::OpenAiCompatible | Self::OpenAiResponses => {
                "https://api.openai.com/v1"
            }
            Self::Anthropic => "https://api.anthropic.com",
            Self::Google => "https://generativelanguage.googleapis.com/v1beta",
            Self::OpenAiCodex => "https://chatgpt.com/backend-api",
            Self::AzureOpenAiResponses => "",
            Self::GoogleVertex => "https://aiplatform.googleapis.com",
            Self::AmazonBedrock => "https://bedrock-runtime.us-east-1.amazonaws.com",
            Self::Mistral => "https://api.mistral.ai",
            Self::PiMessages => "",
        }
    }

    pub fn api_key_env(&self) -> &'static str {
        match self {
            Self::Demo | Self::OpenAiCompatible | Self::OpenAiResponses => "OPENAI_API_KEY",
            Self::Anthropic => "ANTHROPIC_API_KEY",
            Self::Google => "GEMINI_API_KEY",
            Self::OpenAiCodex => "",
            Self::AzureOpenAiResponses => "AZURE_OPENAI_API_KEY",
            Self::GoogleVertex => "GOOGLE_CLOUD_API_KEY",
            Self::AmazonBedrock => "AWS_BEARER_TOKEN_BEDROCK",
            Self::Mistral => "MISTRAL_API_KEY",
            Self::PiMessages => "AGENT_CORE_PI_MESSAGES_API_KEY",
        }
    }
}

#[derive(Clone)]
pub struct ProviderOptions {
    pub provider_id: String,
    pub oauth: Option<OAuthCredential>,
    pub codex_oauth: CodexOAuthConfig,
    pub auth_storage_path: Option<PathBuf>,
    pub azure: AzureOptions,
    pub vertex: VertexOptions,
    pub bedrock: BedrockOptions,
    pub mistral: MistralOptions,
    pub pi_messages: PiMessagesOptions,
    pub cloudflare: CloudflareOptions,
    pub copilot: CopilotOptions,
    pub headers: BTreeMap<String, Option<String>>,
    pub thinking_level: Option<ThinkingLevel>,
    /// Explicit model metadata for a custom model absent from the offline catalog.
    pub thinking_capabilities: Option<ThinkingCapabilities>,
    pub thinking_budgets: BTreeMap<String, u64>,
    pub max_tokens: Option<u64>,
    pub oauth_config: Option<NativeOAuthConfig>,
    pub api_key_explicit: bool,
}
impl Default for ProviderOptions {
    fn default() -> Self {
        Self {
            provider_id: String::new(),
            oauth: None,
            codex_oauth: CodexOAuthConfig::default(),
            auth_storage_path: None,
            azure: AzureOptions::default(),
            vertex: VertexOptions::default(),
            bedrock: BedrockOptions::default(),
            mistral: MistralOptions::default(),
            pi_messages: PiMessagesOptions::default(),
            cloudflare: CloudflareOptions::from_env(),
            copilot: CopilotOptions::from_env(),
            headers: BTreeMap::new(),
            thinking_level: None,
            thinking_capabilities: None,
            thinking_budgets: BTreeMap::new(),
            max_tokens: None,
            oauth_config: None,
            api_key_explicit: false,
        }
    }
}

#[derive(Clone)]
pub struct ProviderSettings {
    pub mode: ProviderMode,
    pub base_url: String,
    pub model: String,
    pub api_key: String,
    pub options: ProviderOptions,
}

impl Default for ProviderSettings {
    fn default() -> Self {
        Self {
            mode: ProviderMode::Demo,
            base_url: "https://api.openai.com/v1".into(),
            model: "".into(),
            api_key: String::new(),
            options: ProviderOptions::default(),
        }
    }
}

impl ProviderSettings {
    pub fn provider_id(&self) -> &str {
        if !self.options.provider_id.is_empty() {
            &self.options.provider_id
        } else {
            match self.mode {
                ProviderMode::Anthropic => "anthropic",
                ProviderMode::Google => "google",
                ProviderMode::OpenAiCodex => "openai-codex",
                ProviderMode::AzureOpenAiResponses => "azure-openai-responses",
                ProviderMode::GoogleVertex => "google-vertex",
                ProviderMode::AmazonBedrock => "amazon-bedrock",
                ProviderMode::Mistral => "mistral",
                ProviderMode::PiMessages => "pi-messages",
                _ => "openai",
            }
        }
    }
    pub fn supported_thinking_levels(&self) -> Vec<ThinkingLevel> {
        thinking::supported_levels(self)
    }
    pub fn validate_thinking_options(&self) -> Result<(), String> {
        thinking::validate(self)
    }
    pub fn clamp_thinking_level(&self, level: ThinkingLevel) -> ThinkingLevel {
        thinking::clamp_level(self, level)
    }
    /// Local configuration checks; validation never contacts a model endpoint.
    pub fn validate(&self) -> Result<(), String> {
        if self.mode == ProviderMode::Demo {
            return Ok(());
        }
        thinking::validate(self)?;
        if self.model.trim().is_empty() {
            return Err("请先填写模型 ID".into());
        }
        self.parsed_base_url()?;
        routed_auth::validate(
            &self.options.provider_id,
            if self.api_key.is_empty()
                && (self.options.auth_storage_path.is_some() || self.options.oauth.is_some())
            {
                "configured-credential-store"
            } else {
                &self.api_key
            },
            &self.mode,
        )?;
        if self.mode == ProviderMode::Mistral {
            self.options.mistral.validate()?;
        }
        if self.mode == ProviderMode::PiMessages {
            self.options.pi_messages.validate()?;
        }
        if matches!(self.mode, ProviderMode::Mistral | ProviderMode::PiMessages)
            && self.api_key.trim().is_empty()
            && self.options.auth_storage_path.is_none()
        {
            return Err("Provider API key is required".into());
        }
        for (name, value) in &self.options.headers {
            reqwest::header::HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| "Invalid provider header name")?;
            if let Some(value) = value {
                validate_header(value)?;
            }
        }
        if self.mode == ProviderMode::AzureOpenAiResponses {
            self.options
                .azure
                .validate(&self.base_url, &self.model, &self.api_key)?;
        }
        if self.mode == ProviderMode::GoogleVertex {
            self.options
                .vertex
                .validate(&self.base_url, &self.model, &self.api_key)?;
        }
        if self.mode == ProviderMode::AmazonBedrock {
            self.options.bedrock.validate(&self.base_url, &self.model)?;
        }
        if self.mode == ProviderMode::OpenAiCodex {
            self.options
                .oauth
                .as_ref()
                .ok_or("OpenAI Codex OAuth login is required")?
                .validate()?;
        }
        if self.mode == ProviderMode::Google {
            let model = self.model.trim();
            let model_id = model.strip_prefix("models/").unwrap_or(model);
            if model_id.is_empty()
                || !model_id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
            {
                return Err("Invalid Google model ID".into());
            }
        }
        if self
            .api_key
            .trim()
            .bytes()
            .any(|byte| byte.is_ascii_control())
        {
            return Err("Invalid API key header format".into());
        }
        Ok(())
    }

    fn parsed_base_url(&self) -> Result<reqwest::Url, String> {
        if matches!(
            self.options.provider_id.as_str(),
            "cloudflare-workers-ai" | "cloudflare-ai-gateway"
        ) {
            return self
                .options
                .cloudflare
                .resolve_base(&self.options.provider_id, &self.base_url);
        }
        if self.options.provider_id == "github-copilot" {
            return self
                .options
                .copilot
                .resolve_base(&self.base_url, &self.api_key);
        }
        if self.mode == ProviderMode::AmazonBedrock {
            return self
                .options
                .bedrock
                .preview_base_url(&self.base_url, &self.model);
        }
        if self.mode == ProviderMode::AzureOpenAiResponses {
            return self.options.azure.resolved_base_url(&self.base_url);
        }
        if self.mode == ProviderMode::GoogleVertex {
            return self.options.vertex.resolved_base_url(
                &self.base_url,
                self.options.vertex.uses_api_key(&self.api_key),
            );
        }
        clean_url(&self.base_url)
    }
}

pub(crate) fn validate_header(value: &str) -> Result<(), String> {
    if value
        .bytes()
        .any(|byte| byte.is_ascii_control() || !byte.is_ascii())
    {
        Err("Invalid credential header format".into())
    } else {
        Ok(())
    }
}
pub(crate) fn clean_url(source: &str) -> Result<reqwest::Url, String> {
    let url = reqwest::Url::parse(source.trim()).map_err(|_| "API 地址格式错误".to_owned())?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err("API 地址必须是 http(s) URL，且不含凭据、查询参数或片段".into());
    }
    Ok(url)
}

pub struct ModelProvider {
    pub settings: ProviderSettings,
    pub tool_definitions: Vec<Value>,
}
fn claude_tool_name(name: &str) -> &str {
    [
        "Read",
        "Write",
        "Edit",
        "Bash",
        "Grep",
        "Glob",
        "AskUserQuestion",
        "EnterPlanMode",
        "ExitPlanMode",
        "KillShell",
        "NotebookEdit",
        "Skill",
        "Task",
        "TaskOutput",
        "TodoWrite",
        "WebFetch",
        "WebSearch",
    ]
    .into_iter()
    .find(|canonical| canonical.eq_ignore_ascii_case(name))
    .unwrap_or(name)
}
fn restore_anthropic_tool_names(message: &mut AssistantMessage, tools: &[Value]) {
    for content in &mut message.content {
        if let Content::ToolCall(call) = content {
            if let Some(name) = tools
                .iter()
                .map(|tool| tool.get("function").unwrap_or(tool))
                .filter_map(|tool| tool["name"].as_str())
                .find(|name| name.eq_ignore_ascii_case(&call.name))
            {
                call.name = name.into();
            }
        }
    }
}
fn anthropic_oauth_payload(payload: &mut Value, context: &Context) {
    let mut system = vec![
        json!({"type":"text","text":"You are Claude Code, Anthropic's official CLI for Claude.","cache_control":{"type":"ephemeral"}}),
    ];
    if !context.system_prompt.is_empty() {
        system.push(json!({"type":"text","text":context.system_prompt,"cache_control":{"type":"ephemeral"}}));
    }
    payload["system"] = json!(system);
    if let Some(tools) = payload["tools"].as_array_mut() {
        for tool in tools {
            if let Some(name) = tool["name"].as_str() {
                tool["name"] = json!(claude_tool_name(name));
            }
        }
    }
    if let Some(messages) = payload["messages"].as_array_mut() {
        for message in messages {
            if let Some(content) = message["content"].as_array_mut() {
                for block in content {
                    if block["type"] == "tool_use" {
                        if let Some(name) = block["name"].as_str() {
                            block["name"] = json!(claude_tool_name(name));
                        }
                    }
                }
            }
        }
    }
}

fn openai_content(content: &[Content], text: &str) -> Value {
    if !content
        .iter()
        .any(|block| matches!(block, Content::Image { .. }))
    {
        return json!(text);
    }
    json!(content.iter().filter_map(|block| match block {
        Content::Text { text } => Some(json!({"type":"text","text":text})),
        Content::Image { data, mime_type } => Some(json!({"type":"image_url","image_url":{"url":format!("data:{mime_type};base64,{data}")}})),
        _ => None,
    }).collect::<Vec<_>>())
}

pub fn request_payload(context: &Context, model: &str, tools: &[Value]) -> Value {
    let mut messages = vec![];
    if !context.system_prompt.is_empty() {
        messages.push(json!({"role":"system","content":context.system_prompt}));
    }
    for message in &context.messages {
        match message {
            Message::User(user) => messages.push(
                json!({"role":"user","content":openai_content(&user.content, &message.text())}),
            ),
            Message::Assistant(assistant) => {
                let calls: Vec<Value> = assistant.content.iter().filter_map(|part| match part {
                    Content::ToolCall(call) => Some(json!({"id":call.id,"type":"function","function":{"name":call.name,"arguments":call.arguments.to_string()}})),
                    _ => None,
                }).collect();
                let thinking = assistant
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        Content::Thinking { thinking } => Some(thinking.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                if message.text().is_empty() && calls.is_empty() && thinking.is_empty() {
                    continue;
                }
                let mut entry = json!({"role":"assistant","content":message.text()});
                if !calls.is_empty() {
                    entry["tool_calls"] = json!(calls);
                }
                if !thinking.is_empty() {
                    entry["reasoning_content"] = json!(thinking);
                }
                messages.push(entry);
            }
            Message::ToolResult(result) => {
                messages.push(json!({"role":"tool","tool_call_id":result.tool_call_id,"content":message.text()}));
                let images: Vec<_> = result
                    .content
                    .iter()
                    .filter(|block| matches!(block, Content::Image { .. }))
                    .cloned()
                    .collect();
                if !images.is_empty() {
                    messages.push(json!({"role":"user","content":openai_content(&images, "")}));
                }
            }
            Message::Custom { .. } => {}
        }
    }
    let mut payload = json!({"model":model,"messages":messages,"stream":true,"stream_options":{"include_usage":true}});
    if !tools.is_empty() {
        payload["tools"] = json!(tools);
    }
    payload
}

impl Provider for ModelProvider {
    fn complete(
        &mut self,
        context: &Context,
        signal: &CancellationToken,
        on_update: &mut dyn FnMut(AssistantMessage),
    ) -> Result<AssistantMessage, String> {
        if signal.is_cancelled() {
            return Err("Operation aborted".into());
        }
        self.settings.validate()?;
        if self.settings.mode == ProviderMode::Demo {
            let prompt = context
                .messages
                .iter()
                .rev()
                .find_map(|message| matches!(message, Message::User(_)).then(|| message.text()))
                .unwrap_or_default();
            let final_text = format!("Rust 核心已收到：{prompt}\n\n当前是离线演示，不会发送模型请求。选择左侧 OpenAI-compatible，填写 API 地址、模型和密钥后即可连接模型。桌面端的对话、会话存储与工具执行均由 Rust 实现。");
            let mut partial = String::new();
            for line in final_text.split_inclusive('\n') {
                if signal.is_cancelled() {
                    return Err("Operation aborted".into());
                }
                partial.push_str(line);
                on_update(AssistantMessage::text(&partial));
            }
            return Ok(AssistantMessage::text(final_text));
        }
        let mut account_id = None;
        let mut azure_bearer = false;
        let mut vertex_api_key = false;
        let mut quota_project = None;
        let mut bedrock_config = None;
        let mut request_key = self.settings.api_key.trim().to_owned();
        let mut native_oauth = false;
        if self.settings.mode != ProviderMode::OpenAiCodex
            && !self.settings.options.api_key_explicit
        {
            let provider = self.settings.provider_id().to_owned();
            let config = self
                .settings
                .options
                .oauth_config
                .clone()
                .map(Ok)
                .unwrap_or_else(|| NativeOAuthConfig::for_provider(&provider));
            let client = config.ok().map(NativeOAuthClient::new);
            let stored = if let Some(path) = &self.settings.options.auth_storage_path {
                AuthStorage::new(path).resolve_provider(&provider, client.as_ref(), signal)?
            } else {
                None
            };
            let (key, credential) = if let Some(stored) = stored {
                stored
            } else if let Some(mut credential) = self.settings.options.oauth.clone() {
                if client
                    .as_ref()
                    .is_some_and(|client| client.config.provider != provider)
                {
                    return Err("OAuth configuration provider identity mismatch".into());
                }
                credential.validate_for_provider(&provider)?;
                if credential.expires <= agent_core::types::timestamp().saturating_add(300000) {
                    credential = client
                        .as_ref()
                        .ok_or("Provider OAuth handler missing")?
                        .refresh(&credential, signal)?;
                }
                (credential.access.clone(), Some(credential))
            } else {
                (request_key.clone(), None)
            };
            request_key = key;
            if let Some(credential) = credential {
                if provider == "github-copilot" {
                    if let Some(domain) = credential
                        .extra
                        .get("enterpriseUrl")
                        .and_then(Value::as_str)
                    {
                        self.settings.options.copilot.enterprise_domain = domain.into();
                    }
                    if credential
                        .extra
                        .get("availableModelIds")
                        .and_then(Value::as_array)
                        .is_some_and(|models| !models.iter().any(|id| id == &self.settings.model))
                    {
                        return Err("Model is unavailable for this Copilot credential".into());
                    }
                }
                native_oauth = provider == "anthropic";
                self.settings.options.oauth = Some(credential);
            }
        }
        native_oauth |=
            self.settings.provider_id() == "anthropic" && request_key.contains("sk-ant-oat");
        routed_auth::validate(
            self.settings.provider_id(),
            &request_key,
            &self.settings.mode,
        )?;
        if matches!(
            self.settings.mode,
            ProviderMode::Mistral | ProviderMode::PiMessages
        ) && request_key.is_empty()
        {
            return Err("Provider API key is required".into());
        }
        if self.settings.mode == ProviderMode::AmazonBedrock {
            bedrock_config = Some(self.settings.options.bedrock.resolve(
                &self.settings.base_url,
                &self.settings.model,
                &self.settings.api_key,
                signal,
            )?);
            request_key.clear();
        }
        if self.settings.mode == ProviderMode::OpenAiCodex {
            let client = CodexOAuthClient::new(self.settings.options.codex_oauth.clone());
            let mut credential = if let Some(path) = &self.settings.options.auth_storage_path {
                AuthStorage::new(path).ensure_codex(&client, signal)?
            } else {
                self.settings
                    .options
                    .oauth
                    .clone()
                    .ok_or("OpenAI Codex OAuth login is required")?
            };
            if credential.is_expired() {
                credential = client.refresh(&credential, signal)?;
            }
            account_id = Some(codex_account_id(&credential.access)?);
            request_key = credential.access.clone();
            self.settings.options.oauth = Some(credential);
        }
        if self.settings.mode == ProviderMode::AzureOpenAiResponses
            && !self.settings.options.azure.bearer_token.is_empty()
        {
            request_key = self.settings.options.azure.bearer_token.clone();
            azure_bearer = true;
        }
        if self.settings.mode == ProviderMode::GoogleVertex {
            let auth = self
                .settings
                .options
                .vertex
                .authorize(&self.settings.api_key, signal)?;
            vertex_api_key = auth.api_key.is_some();
            request_key = auth
                .api_key
                .or(auth.access_token)
                .ok_or("Vertex credential token missing")?;
            quota_project = auth.quota_project;
        }
        let mut request_settings = self.settings.clone();
        request_settings.api_key = request_key.clone();
        let mut url = request_settings.parsed_base_url()?;
        let base_path = url.path().trim_end_matches('/');
        let configured_model = self.settings.model.trim();
        let model = if self.settings.mode == ProviderMode::AzureOpenAiResponses {
            self.settings
                .options
                .azure
                .effective_deployment(configured_model)
        } else {
            configured_model
        };
        match self.settings.mode {
            ProviderMode::Anthropic => {
                let path = if base_path.ends_with("/v1") {
                    format!("{base_path}/messages")
                } else {
                    format!("{base_path}/v1/messages")
                };
                url.set_path(&path);
            }
            ProviderMode::Google => {
                let model_id = model.strip_prefix("models/").unwrap_or(model);
                url.set_path(&format!(
                    "{base_path}/models/{model_id}:streamGenerateContent"
                ));
                url.set_query(Some("alt=sse"));
            }
            ProviderMode::OpenAiCompatible => {
                url.set_path(&format!("{base_path}/chat/completions"));
            }
            ProviderMode::Mistral => {
                url.set_path(&format!("{base_path}/v1/chat/completions"));
            }
            ProviderMode::PiMessages => {
                url.set_path(&format!("{base_path}/messages"));
                if self.settings.options.pi_messages.debug {
                    url.set_query(Some("debug=1"));
                }
            }
            ProviderMode::OpenAiResponses => {
                url.set_path(&format!("{base_path}/responses"));
            }
            ProviderMode::OpenAiCodex => {
                let path = if base_path.ends_with("/codex/responses") {
                    base_path.to_owned()
                } else if base_path.ends_with("/codex") {
                    format!("{base_path}/responses")
                } else {
                    format!("{base_path}/codex/responses")
                };
                url.set_path(&path);
            }
            ProviderMode::AzureOpenAiResponses => {
                url = self
                    .settings
                    .options
                    .azure
                    .request_url(&self.settings.base_url)?
            }
            ProviderMode::GoogleVertex => {
                url = self.settings.options.vertex.request_url(
                    &self.settings.base_url,
                    model,
                    &self.settings.api_key,
                )?
            }
            ProviderMode::AmazonBedrock => {
                url = self.settings.options.bedrock.request_url(
                    &self.settings.base_url,
                    model,
                    &bedrock_config.as_ref().expect("Bedrock config").region,
                )?;
            }
            ProviderMode::Demo => unreachable!("Demo returned before networking"),
        }
        let provider_id = if self.settings.options.provider_id.is_empty() {
            match self.settings.mode {
                ProviderMode::Anthropic => "anthropic",
                ProviderMode::Google => "google",
                ProviderMode::OpenAiCodex => "openai-codex",
                ProviderMode::AzureOpenAiResponses => "azure-openai-responses",
                ProviderMode::GoogleVertex => "google-vertex",
                ProviderMode::AmazonBedrock => "amazon-bedrock",
                ProviderMode::Mistral => "mistral",
                ProviderMode::PiMessages => "pi-messages",
                _ => "openai",
            }
        } else {
            &self.settings.options.provider_id
        };
        let route = if self.settings.mode == ProviderMode::AzureOpenAiResponses {
            self.settings
                .options
                .azure
                .route_identity(&url, configured_model)
        } else if self.settings.mode == ProviderMode::GoogleVertex {
            self.settings
                .options
                .vertex
                .route_identity(&url, &self.settings.api_key)
        } else if self.settings.mode == ProviderMode::AmazonBedrock {
            self.settings.options.bedrock.route_identity(
                &url,
                &bedrock_config.as_ref().expect("Bedrock config").region,
            )
        } else {
            url.to_string()
        };
        let protocol = match self.settings.mode {
            ProviderMode::OpenAiCompatible => "openai-completions",
            ProviderMode::OpenAiResponses => "openai-responses",
            ProviderMode::OpenAiCodex => "openai-codex-responses",
            ProviderMode::Anthropic => "anthropic",
            ProviderMode::Google => "google",
            ProviderMode::GoogleVertex => "google-vertex",
            ProviderMode::AzureOpenAiResponses => "azure-openai-responses",
            ProviderMode::AmazonBedrock => "bedrock-converse-stream",
            ProviderMode::Mistral => "mistral-conversations",
            ProviderMode::PiMessages => "pi-messages",
            ProviderMode::Demo => unreachable!("Demo returned before networking"),
        };
        let mut request_context = context.clone();
        for message in &mut request_context.messages {
            if let Message::Assistant(assistant) = message {
                if assistant
                    .protocol_metadata
                    .as_ref()
                    .is_some_and(|metadata| {
                        metadata["provider"] != provider_id || metadata["route"] != route
                    })
                {
                    assistant.protocol_metadata = None;
                }
            }
        }
        let mut payload = match self.settings.mode {
            ProviderMode::Anthropic => {
                anthropic_request_payload(&request_context, model, &self.tool_definitions)
            }
            ProviderMode::Google => {
                google_request_payload(&request_context, model, &self.tool_definitions)
            }
            ProviderMode::OpenAiCompatible => {
                request_payload(&request_context, model, &self.tool_definitions)
            }
            ProviderMode::Mistral => mistral_request_payload(
                &request_context,
                model,
                &self.tool_definitions,
                &self.settings.options.mistral,
            )?,
            ProviderMode::PiMessages => pi_messages_request_payload(
                context,
                model,
                &self.tool_definitions,
                &self.settings.options.pi_messages,
            )?,
            ProviderMode::OpenAiResponses => {
                responses_request_payload(&request_context, model, &self.tool_definitions)
            }
            ProviderMode::OpenAiCodex => {
                let mut payload = responses::responses_request_payload_for_protocol(
                    &request_context,
                    model,
                    &self.tool_definitions,
                    "openai-codex-responses",
                );
                payload["instructions"] = json!(if context.system_prompt.is_empty() {
                    "You are a helpful assistant."
                } else {
                    &context.system_prompt
                });
                payload["text"] = json!({"verbosity":"low"});
                payload["tool_choice"] = json!("auto");
                payload["parallel_tool_calls"] = json!(true);
                if let Some(tools) = payload.get_mut("tools").and_then(Value::as_array_mut) {
                    for tool in tools {
                        tool["strict"] = Value::Null;
                    }
                }
                payload
            }
            ProviderMode::AzureOpenAiResponses => azure_request_payload(
                &request_context,
                configured_model,
                &self.tool_definitions,
                &self.settings.options.azure,
            ),
            ProviderMode::GoogleVertex => {
                vertex_request_payload(&request_context, model, &self.tool_definitions)
            }
            ProviderMode::AmazonBedrock => bedrock_request_payload(
                &request_context,
                model,
                &self.tool_definitions,
                &self.settings.options.bedrock,
            )?,
            ProviderMode::Demo => unreachable!("Demo returned before networking"),
        };
        apply_thinking(
            &self.settings,
            &request_context,
            &self.tool_definitions,
            &mut payload,
        )?;
        if native_oauth {
            anthropic_oauth_payload(&mut payload, context);
        }
        let body =
            serde_json::to_vec(&payload).map_err(|_| "Model request JSON serialization failed")?;
        let routed_owned = matches!(
            provider_id,
            "github-copilot" | "cloudflare-workers-ai" | "cloudflare-ai-gateway"
        );
        let mut header_overrides: BTreeMap<String, Option<String>> =
            crate::catalog::find(provider_id, configured_model)
                .and_then(|record| record.metadata["headers"].as_object())
                .map(|headers| {
                    headers
                        .iter()
                        .map(|(name, value)| {
                            (name.to_lowercase(), value.as_str().map(str::to_owned))
                        })
                        .collect()
                })
                .unwrap_or_default();
        header_overrides.extend(
            self.settings
                .options
                .headers
                .iter()
                .map(|(name, value)| (name.to_lowercase(), value.clone())),
        );
        if self.settings.mode == ProviderMode::Mistral {
            if let Some(session) = self.settings.options.mistral.cache_key() {
                header_overrides
                    .entry("x-affinity".into())
                    .or_insert_with(|| Some(session.into()));
            }
        }
        if routed_owned {
            header_overrides.extend(
                routed_auth::headers(provider_id, &request_key, context)
                    .into_iter()
                    .map(|(name, value)| (name, Some(value))),
            );
            header_overrides.insert("x-api-key".into(), None);
            if provider_id == "cloudflare-ai-gateway" {
                header_overrides.insert("authorization".into(), None);
            }
        }
        if native_oauth {
            header_overrides.insert(
                "authorization".into(),
                Some(format!("Bearer {request_key}")),
            );
            header_overrides.insert("x-api-key".into(), None);
            header_overrides
                .entry("user-agent".into())
                .or_insert(Some("claude-cli/2.1.251".into()));
            header_overrides
                .entry("x-app".into())
                .or_insert(Some("cli".into()));
        }
        let mut betas = thinking::anthropic_betas(&self.settings);
        if native_oauth {
            betas.extend(["claude-code-20250219", "oauth-2025-04-20"]);
        }
        if !betas.is_empty() {
            header_overrides
                .entry("anthropic-beta".into())
                .or_insert_with(|| Some(betas.join(",")));
        }
        let aws_headers = if let Some(config) = bedrock_config {
            let headers = BTreeMap::from([
                ("content-type".into(), "application/json".into()),
                ("accept".into(), "application/vnd.amazon.eventstream".into()),
            ]);
            if let Some(token) = config.bearer_token {
                let mut headers = headers;
                headers.insert("authorization".into(), format!("Bearer {token}"));
                headers
            } else {
                let timestamp = aws_timestamp(agent_core::types::timestamp() / 1000)?;
                sign_aws_v4(SigV4Parameters {
                    method: "POST",
                    url: &url,
                    body: &body,
                    headers: &headers,
                    credentials: &config.credentials.ok_or("AWS credentials missing")?,
                    region: &config.region,
                    service: "bedrock",
                    timestamp: &timestamp,
                    checksum_header: true,
                })?
                .headers
            }
        } else {
            BTreeMap::new()
        };
        let reader = network_reader(
            url,
            body,
            self.settings.mode.clone(),
            RequestAuth {
                credential: request_key,
                account_id,
                azure_bearer,
                vertex_api_key,
                quota_project,
                aws_headers,
                header_overrides,
                routed_owned,
            },
            signal.clone(),
        );
        let mut update = |mut message: AssistantMessage| {
            if native_oauth {
                restore_anthropic_tool_names(&mut message, &self.tool_definitions);
            }
            {
                let metadata = message
                    .protocol_metadata
                    .get_or_insert_with(|| json!({"protocol":protocol}));
                metadata["model"] = json!(model);
                metadata["provider"] = json!(provider_id);
                metadata["route"] = json!(route);
                thinking::stamp_metadata(&self.settings, metadata);
                if self.settings.mode == ProviderMode::AzureOpenAiResponses {
                    metadata["configured_model"] = json!(configured_model);
                    metadata["model_identity_verified"] = json!(false);
                }
            }
            on_update(message);
        };
        let mut message = match self.settings.mode {
            ProviderMode::Anthropic => {
                parse_anthropic_sse(BufReader::new(reader), signal, &mut update)
            }
            ProviderMode::Google => parse_google_sse(BufReader::new(reader), signal, &mut update),
            ProviderMode::OpenAiCompatible => {
                parse_sse(BufReader::new(reader), signal, &mut update)
            }
            ProviderMode::Mistral => parse_mistral_sse(BufReader::new(reader), signal, &mut update),
            ProviderMode::PiMessages => {
                parse_pi_messages_sse(BufReader::new(reader), signal, &mut update)
            }
            ProviderMode::OpenAiResponses => {
                parse_responses_sse(BufReader::new(reader), signal, &mut update)
            }
            ProviderMode::OpenAiCodex => {
                parse_codex_sse(BufReader::new(reader), signal, &mut update)
            }
            ProviderMode::AzureOpenAiResponses => {
                parse_azure_sse(BufReader::new(reader), signal, &mut update)
            }
            ProviderMode::GoogleVertex => {
                parse_vertex_sse(BufReader::new(reader), signal, &mut update)
            }
            ProviderMode::AmazonBedrock => {
                parse_bedrock_stream(BufReader::new(reader), signal, &mut update)
            }
            ProviderMode::Demo => unreachable!("Demo returned before networking"),
        }?;
        if native_oauth {
            restore_anthropic_tool_names(&mut message, &self.tool_definitions);
        }
        {
            let metadata = message
                .protocol_metadata
                .get_or_insert_with(|| json!({"protocol":protocol}));
            metadata["model"] = json!(model);
            metadata["provider"] = json!(provider_id);
            metadata["route"] = json!(route);
            thinking::stamp_metadata(&self.settings, metadata);
            if self.settings.mode == ProviderMode::AzureOpenAiResponses {
                metadata["configured_model"] = json!(configured_model);
                metadata["model_identity_verified"] = json!(false);
            }
        }
        Ok(message)
    }

    fn complete_summary(
        &mut self,
        context: &Context,
        signal: &CancellationToken,
        on_update: &mut dyn FnMut(AssistantMessage),
    ) -> Result<AssistantMessage, String> {
        let mut provider = Self {
            settings: self.settings.clone(),
            tool_definitions: Vec::new(),
        };
        let result = provider.complete(context, signal, on_update);
        self.settings.options.oauth = provider.settings.options.oauth;
        result
    }
}

struct NetworkReader {
    receiver: Receiver<Result<Vec<u8>, String>>,
    buffer: Cursor<Vec<u8>>,
    signal: CancellationToken,
    transport: CancellationToken,
}

impl Read for NetworkReader {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if output.is_empty() {
            return Ok(0);
        }
        loop {
            if self.signal.is_cancelled() {
                return Err(io::Error::new(
                    io::ErrorKind::ConnectionAborted,
                    "Operation aborted",
                ));
            }
            let count = self.buffer.read(output)?;
            if count > 0 {
                return Ok(count);
            }
            match self.receiver.recv_timeout(Duration::from_millis(50)) {
                Ok(Ok(bytes)) if bytes.is_empty() => return Ok(0),
                Ok(Ok(bytes)) => self.buffer = Cursor::new(bytes),
                Ok(Err(error)) => return Err(io::Error::other(error)),
                Err(RecvTimeoutError::Disconnected) => return Ok(0),
                Err(RecvTimeoutError::Timeout) => {}
            }
        }
    }
}

impl Drop for NetworkReader {
    fn drop(&mut self) {
        self.transport.cancel();
    }
}

async fn cancelled(signal: &CancellationToken, transport: &CancellationToken) {
    while !signal.is_cancelled() && !transport.is_cancelled() {
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

struct RequestAuth {
    credential: String,
    account_id: Option<String>,
    azure_bearer: bool,
    vertex_api_key: bool,
    quota_project: Option<String>,
    aws_headers: BTreeMap<String, String>,
    header_overrides: BTreeMap<String, Option<String>>,
    routed_owned: bool,
}
fn network_reader(
    url: reqwest::Url,
    body: Vec<u8>,
    mode: ProviderMode,
    auth: RequestAuth,
    signal: CancellationToken,
) -> NetworkReader {
    let (sender, receiver) = mpsc::sync_channel(8);
    let transport = CancellationToken::new();
    let worker_signal = signal.clone();
    let worker_transport = transport.clone();
    std::thread::spawn(move || {
        let RequestAuth {
            credential: api_key,
            account_id,
            azure_bearer,
            vertex_api_key,
            quota_project,
            aws_headers,
            header_overrides,
            routed_owned,
        } = auth;
        let result = (|| -> Result<(), String> {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| e.to_string())?;
            runtime.block_on(async {
                let builder = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).connect_timeout(Duration::from_secs(10)).timeout(Duration::from_secs(120));
                // Loopback fixtures never leave the local machine via a configured proxy.
                let client = if url.host_str().is_some_and(|host| matches!(host, "127.0.0.1" | "localhost" | "::1")) { builder.no_proxy() } else { builder }.build().map_err(|e| e.to_string())?;
                let mut request = client.post(url).header("content-type", "application/json").body(body);
                if matches!(mode,ProviderMode::Mistral|ProviderMode::PiMessages){request=request.header("accept","text/event-stream");}
                if mode==ProviderMode::Anthropic {request=request.header("anthropic-version","2023-06-01");}
                if !routed_owned {match mode {
                    ProviderMode::Anthropic => {
                        if !api_key.is_empty() { request = request.header("x-api-key", &api_key); }
                    }
                    ProviderMode::Google => if !api_key.is_empty() { request = request.header("x-goog-api-key", &api_key); },
                    ProviderMode::AzureOpenAiResponses=> {request=if azure_bearer {request.bearer_auth(&api_key)} else {request.header("api-key",&api_key)};},
                    ProviderMode::GoogleVertex=> {request=if vertex_api_key {request.header("x-goog-api-key",&api_key)}else {request.bearer_auth(&api_key)};if let Some(project)=quota_project {request=request.header("x-goog-user-project",project);}},
                    ProviderMode::AmazonBedrock => {for (name,value) in aws_headers {request=request.header(name,value);}},
                    ProviderMode::OpenAiCodex => {
                        request=request.bearer_auth(&api_key).header("chatgpt-account-id",account_id.unwrap_or_default()).header("originator","pi").header("OpenAI-Beta","responses=experimental").header("accept","text/event-stream").header("User-Agent",concat!("agent-core-rust/",env!("CARGO_PKG_VERSION")));
                    }
                    _ => if !api_key.is_empty() { request = request.bearer_auth(&api_key); },
                }}
                let mut request=request.build().map_err(|_|"Invalid model request configuration")?;
                for(name,value)in header_overrides {
                    let name=reqwest::header::HeaderName::from_bytes(name.as_bytes()).map_err(|_|"Invalid provider header name")?;
                    if let Some(value)=value {request.headers_mut().insert(name,reqwest::header::HeaderValue::from_str(&value).map_err(|_|"Invalid provider header value")?);}else{request.headers_mut().remove(&name);}
                }
                let mut response = tokio::select! {
                    response = client.execute(request) => response.map_err(|e| format!("模型请求失败：{e}"))?,
                    _ = cancelled(&worker_signal, &worker_transport) => return Err("Operation aborted".into()),
                };
                if !response.status().is_success() { return Err(http_error(&mut response,&worker_signal,&worker_transport).await); }
                loop {
                    let bytes = tokio::select! {
                        bytes = response.chunk() => bytes.map_err(|e| e.to_string())?,
                        _ = cancelled(&worker_signal, &worker_transport) => return Err("Operation aborted".into()),
                    };
                    match bytes {
                        Some(bytes) => if sender.send(Ok(bytes.to_vec())).is_err() { break; },
                        None => { let _ = sender.send(Ok(vec![])); break; }
                    }
                }
                Ok(())
            })
        })();
        if let Err(error) = result {
            let _ = sender.send(Err(error));
        }
    });
    NetworkReader {
        receiver,
        buffer: Cursor::new(vec![]),
        signal,
        transport,
    }
}

async fn http_error(
    response: &mut reqwest::Response,
    signal: &CancellationToken,
    transport: &CancellationToken,
) -> String {
    let status = response.status().as_u16();
    let read = async {
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.ok()? {
            if bytes.len().saturating_add(chunk.len()) > 16_384 {
                return None;
            }
            bytes.extend_from_slice(&chunk);
        }
        serde_json::from_slice::<Value>(&bytes)
            .ok()
            .and_then(|value| {
                [
                    value["error"]["code"].as_str(),
                    value["error"]["type"].as_str(),
                    value["code"].as_str(),
                    value["type"].as_str(),
                ]
                .into_iter()
                .flatten()
                .find(|code| sse::preserved_error_code(code))
                .map(str::to_owned)
            })
    };
    let code = tokio::select! {
        code=tokio::time::timeout(Duration::from_millis(250),read)=>code.ok().flatten(),
        _=cancelled(signal,transport)=>return "Operation aborted".into(),
    };
    match code {
        Some(code) => format!("模型服务返回 HTTP {status} ({code})"),
        None => format!("模型服务返回 HTTP {status}"),
    }
}

#[derive(Default)]
struct PartialCall {
    id: String,
    name: String,
    arguments: String,
}

pub fn parse_sse(
    mut reader: impl BufRead,
    signal: &CancellationToken,
    on_update: &mut dyn FnMut(AssistantMessage),
) -> Result<AssistantMessage, String> {
    let mut text = String::new();
    let mut thinking = String::new();
    let mut calls: BTreeMap<u64, PartialCall> = BTreeMap::new();
    let mut message = AssistantMessage::text("");
    let mut finish_reason = None;
    let mut frame = String::new();
    loop {
        if signal.is_cancelled() {
            return Err("Operation aborted".into());
        }
        let mut line = String::new();
        let size = reader
            .by_ref()
            .take(1_048_576)
            .read_line(&mut line)
            .map_err(|e| format!("流读取失败：{e}"))?;
        if size >= 1_048_576 {
            return Err("SSE line exceeds limit".into());
        }
        if size == 0 || line.trim_end_matches(['\r', '\n']).is_empty() {
            if frame.trim() == "[DONE]" {
                break;
            }
            if !frame.is_empty() {
                let value: Value =
                    serde_json::from_str(&frame).map_err(|e| format!("无效模型流：{e}"))?;
                frame.clear();
                if value.get("error").is_some() {
                    return Err("模型服务在响应流中报告错误".into());
                }
                if let Some(usage) = value.get("usage").filter(|u| u.is_object()) {
                    let cache = usage["prompt_tokens_details"]["cached_tokens"]
                        .as_u64()
                        .unwrap_or(0);
                    message.usage = Some(Usage {
                        input: usage["prompt_tokens"]
                            .as_u64()
                            .unwrap_or(0)
                            .saturating_sub(cache),
                        output: usage["completion_tokens"].as_u64().unwrap_or(0),
                        cache_read: cache,
                        total_tokens: usage["total_tokens"].as_u64().unwrap_or(0),
                        ..Usage::default()
                    });
                }
                if let Some(choice) = value["choices"].as_array().and_then(|items| items.first()) {
                    if let Some(reason) = choice["finish_reason"].as_str() {
                        finish_reason = Some(reason.to_owned());
                    }
                    let delta = &choice["delta"];
                    if let Some(content) = delta["content"].as_str() {
                        text.push_str(content);
                    }
                    if let Some(content) = delta["reasoning_content"]
                        .as_str()
                        .or_else(|| delta["reasoning"].as_str())
                    {
                        thinking.push_str(content);
                    }
                    if let Some(fragments) = delta["tool_calls"].as_array() {
                        for fragment in fragments {
                            let index = fragment["index"]
                                .as_u64()
                                .ok_or("Tool call index missing")?;
                            let call = calls.entry(index).or_default();
                            if let Some(id) = fragment["id"].as_str() {
                                if call.id.is_empty() {
                                    call.id = id.to_owned();
                                } else if !id.is_empty() && call.id != id {
                                    return Err("Conflicting tool call identity".into());
                                }
                            }
                            if let Some(name) = fragment["function"]["name"].as_str() {
                                if call.name.is_empty() {
                                    call.name = name.to_owned();
                                } else if !name.is_empty() && call.name != name {
                                    return Err("Conflicting tool call name".into());
                                }
                            }
                            if let Some(arguments) = fragment["function"]["arguments"].as_str() {
                                call.arguments.push_str(arguments);
                            }
                        }
                    }
                    message.content = Vec::new();
                    if !thinking.is_empty() {
                        message.content.push(Content::Thinking {
                            thinking: thinking.clone(),
                        });
                    }
                    if !text.is_empty() {
                        message.content.push(Content::text(&text));
                    }
                    on_update(message.clone());
                }
            }
            if size == 0 {
                break;
            }
        } else if let Some(data) = line.strip_prefix("data:") {
            if !frame.is_empty() {
                frame.push('\n');
            }
            frame.push_str(data.trim_start().trim_end_matches(['\r', '\n']));
            if frame.len() > 1_048_576 {
                return Err("SSE event exceeds limit".into());
            }
        }
        if text.len()
            + thinking.len()
            + calls
                .values()
                .map(|call| call.arguments.len())
                .sum::<usize>()
            > 8_388_608
        {
            return Err("Model response exceeds limit".into());
        }
    }
    if finish_reason.is_none() {
        return Err("模型响应流提前结束".into());
    }
    message.stop_reason = match finish_reason.as_deref() {
        Some("length") => StopReason::Length,
        Some("tool_calls") | Some("function_call") => StopReason::ToolUse,
        Some("content_filter") => return Err("模型服务未返回可用响应".into()),
        Some("stop") => {
            if calls.is_empty() {
                StopReason::Stop
            } else {
                StopReason::ToolUse
            }
        }
        _ => return Err("Unsupported OpenAI finish reason".into()),
    };
    message.content = vec![];
    if !thinking.is_empty() {
        message.content.push(Content::Thinking { thinking });
    }
    if !text.is_empty() {
        message.content.push(Content::text(text));
    }
    let mut ids = BTreeSet::new();
    for call in calls.into_values() {
        if call.id.is_empty() || call.name.is_empty() {
            return Err("Incomplete tool call identity".into());
        }
        if !ids.insert(call.id.clone()) {
            return Err("Duplicate tool call ID".into());
        }
        let arguments = if message.stop_reason == StopReason::Length {
            Value::Null
        } else {
            serde_json::from_str(&call.arguments)
                .map_err(|e| format!("Invalid tool arguments: {e}"))?
        };
        if message.stop_reason != StopReason::Length && !arguments.is_object() {
            return Err("Tool arguments must be an object".into());
        }
        message.content.push(Content::ToolCall(ToolCall {
            id: call.id,
            name: call.name,
            arguments,
        }));
    }
    if message.stop_reason == StopReason::ToolUse && ids.is_empty() {
        return Err("Tool-use stop has no tool calls".into());
    }
    Ok(message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    #[test]
    fn sse_utf8_content_usage_and_tool_fragments() {
        let frames = [
            json!({"choices":[{"delta":{"content":"你好","tool_calls":[{"index":0,"id":"call_1","function":{"name":"read","arguments":"{\"pa"}}]}}]}),
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"th\":\"README.md\"}"}}]},"finish_reason":"tool_calls"}]}),
            json!({"choices":[],"usage":{"prompt_tokens":5,"completion_tokens":8,"total_tokens":13}}),
        ];
        let mut stream = frames
            .into_iter()
            .map(|frame| format!("data: {frame}\r\n\r\n"))
            .collect::<String>();
        stream.push_str("data: [DONE]\n\n");
        let mut updates = 0;
        let result = parse_sse(Cursor::new(stream), &CancellationToken::new(), &mut |_| {
            updates += 1
        })
        .unwrap();
        assert_eq!(updates, 2);
        assert_eq!(result.stop_reason, StopReason::ToolUse);
        assert_eq!(result.usage.as_ref().unwrap().total_tokens, 13);
        assert_eq!(result.tool_calls()[0].arguments["path"], "README.md");
    }
    #[test]
    fn truncated_arguments_are_never_salvaged_for_execution() {
        let stream = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"id\",\"function\":{\"name\":\"write\",\"arguments\":\"{\"}}]},\"finish_reason\":\"length\"}]}\n\ndata: [DONE]\n\n";
        let result =
            parse_sse(Cursor::new(stream), &CancellationToken::new(), &mut |_| {}).unwrap();
        assert_eq!(result.stop_reason, StopReason::Length);
        assert_eq!(result.tool_calls()[0].arguments, Value::Null);
    }
    #[test]
    fn aborted_and_incomplete_streams_return_errors() {
        assert!(parse_sse(Cursor::new(""), &CancellationToken::new(), &mut |_| {}).is_err());
        let signal = CancellationToken::new();
        signal.cancel();
        assert!(parse_sse(Cursor::new("data: [DONE]\n\n"), &signal, &mut |_| {}).is_err());
    }
    #[test]
    fn payload_pairs_tool_results_and_filters_custom_messages() {
        let context = Context {
            system_prompt: "system".into(),
            messages: vec![
                Message::user("中文"),
                Message::Custom {
                    kind: "notice".into(),
                    data: Value::Null,
                    timestamp: 0,
                },
            ],
        };
        let payload = request_payload(&context, "offline-model", &[]);
        assert_eq!(payload["messages"].as_array().unwrap().len(), 2);
        assert_eq!(payload["messages"][1]["content"], "中文");
        assert!(payload.get("tools").is_none());
    }

    #[test]
    fn failed_empty_assistants_are_not_replayed_to_the_model() {
        let context = Context {
            system_prompt: String::new(),
            messages: vec![
                Message::user("first"),
                Message::Assistant(AssistantMessage::error("offline failure", false)),
                Message::user("retry"),
            ],
        };
        let payload = request_payload(&context, "offline", &[]);
        assert_eq!(payload["messages"].as_array().unwrap().len(), 2);
        assert_eq!(payload["messages"][1]["content"], "retry");
    }

    #[test]
    fn repeated_tool_identity_is_not_concatenated_and_conflicts_fail() {
        for second_id in ["call-1", "conflict"] {
            let frames = [
                json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call-1","function":{"name":"read","arguments":"{"}}]}}]}),
                json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":second_id,"function":{"name":"read","arguments":"}"}}]},"finish_reason":"tool_calls"}]}),
            ];
            let mut stream = frames
                .into_iter()
                .map(|frame| format!("data: {frame}\n\n"))
                .collect::<String>();
            stream.push_str("data: [DONE]\n\n");
            let result = parse_sse(Cursor::new(stream), &CancellationToken::new(), &mut |_| {});
            if second_id == "conflict" {
                assert!(result.unwrap_err().contains("Conflicting"));
            } else {
                let calls = result.unwrap().tool_calls();
                assert_eq!(calls[0].id, "call-1");
                assert_eq!(calls[0].name, "read");
                assert_eq!(calls[0].arguments, json!({}));
            }
        }
    }
}
