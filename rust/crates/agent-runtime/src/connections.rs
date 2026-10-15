//! Desktop connection presets and bounded model discovery. No UI or automatic network I/O.
//! Endpoint/auth choices are code, not remotely supplied model metadata.
use crate::{
    auth_storage::AuthStorage,
    catalog,
    provider::{ProviderMode, ProviderOptions, ProviderSettings},
};
use agent_core::types::{timestamp, CancellationToken};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{fmt, path::Path, time::Duration};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Service {
    DeepSeek,
    Zhipu,
    ZhipuCoding,
    OpenAi,
    Codex,
}

impl Service {
    pub const ALL: [Self; 5] = [
        Self::DeepSeek,
        Self::Zhipu,
        Self::ZhipuCoding,
        Self::OpenAi,
        Self::Codex,
    ];
    pub fn id(self) -> &'static str {
        match self {
            Self::DeepSeek => "deepseek",
            Self::Zhipu => "zhipuai",
            Self::ZhipuCoding => "zai-coding-cn",
            Self::OpenAi => "openai",
            Self::Codex => "openai-codex",
        }
    }
    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|s| s.id() == id)
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::DeepSeek => "DeepSeek",
            Self::Zhipu => "智谱 · 标准 API",
            Self::ZhipuCoding => "智谱 · Coding Plan",
            Self::OpenAi => "OpenAI · API",
            Self::Codex => "OpenAI · ChatGPT / Codex 账号",
        }
    }
    pub fn browser_login(self) -> bool {
        self == Self::Codex
    }
    pub fn base_url(self) -> &'static str {
        match self {
            Self::DeepSeek => "https://api.deepseek.com",
            Self::Zhipu => "https://open.bigmodel.cn/api/paas/v4",
            Self::ZhipuCoding => "https://open.bigmodel.cn/api/coding/paas/v4",
            Self::OpenAi => "https://api.openai.com/v1",
            Self::Codex => "https://chatgpt.com/backend-api",
        }
    }
    pub fn mode(self) -> ProviderMode {
        match self {
            Self::OpenAi => ProviderMode::OpenAiResponses,
            Self::Codex => ProviderMode::OpenAiCodex,
            _ => ProviderMode::OpenAiCompatible,
        }
    }
    pub fn key_url(self) -> &'static str {
        match self {
            Self::DeepSeek => "https://platform.deepseek.com/api_keys",
            Self::Zhipu | Self::ZhipuCoding => "https://bigmodel.cn/usercenter/proj-mgmt/apikeys",
            Self::OpenAi => "https://platform.openai.com/api-keys",
            Self::Codex => "https://developers.openai.com/codex/auth",
        }
    }
    pub fn settings(self, model: &str, auth_path: &Path) -> ProviderSettings {
        ProviderSettings {
            mode: self.mode(),
            base_url: self.base_url().into(),
            model: model.trim().into(),
            api_key: String::new(),
            options: ProviderOptions {
                provider_id: self.id().into(),
                auth_storage_path: Some(auth_path.into()),
                ..Default::default()
            },
        }
    }
    fn preference_key(self) -> String {
        format!("desktop-connection/{}", self.id())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ModelSource {
    Bundled,
    ProviderApi,
    PublicCatalog,
}
impl ModelSource {
    pub fn label(self) -> &'static str {
        match self {
            Self::Bundled => "内置目录（非账号权限列表）",
            Self::ProviderApi => "厂商模型接口（不保证每个型号适合对话）",
            Self::PublicCatalog => "Models.dev 公开目录（非账号权限验证）",
        }
    }
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DiscoveredModel {
    pub id: String,
    pub context_window: Option<u64>,
    pub max_output: Option<u64>,
    /// Preserve upstream fields without turning them into endpoint/auth configuration.
    pub metadata: Value,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Connection {
    pub service: Service,
    pub selected_model: String,
    pub models: Vec<DiscoveredModel>,
    pub source: ModelSource,
    pub refreshed_at: Option<u64>,
}
impl Connection {
    pub fn bundled(service: Service) -> Self {
        Self {
            service,
            selected_model: String::new(),
            source: ModelSource::Bundled,
            refreshed_at: None,
            models: catalog::models()
                .iter()
                .filter(|m| m.provider == service.id())
                .map(|m| DiscoveredModel {
                    id: m.id.clone(),
                    context_window: Some(m.context_window),
                    max_output: Some(m.max_tokens),
                    metadata: m.metadata.clone(),
                })
                .collect(),
        }
    }
    pub fn budget(&self, model: &str) -> (u64, u64) {
        self.models
            .iter()
            .find(|m| m.id == model)
            .map_or((0, 0), |m| {
                (m.context_window.unwrap_or(0), m.max_output.unwrap_or(0))
            })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectionError {
    Cancelled,
    InvalidKey,
    MissingKey,
    Storage,
    InvalidModels,
    TooLarge,
    Network,
    Http(u16),
    AccountLogin,
}
impl fmt::Display for ConnectionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => f.write_str("操作已取消，未应用新配置"),
            Self::InvalidKey => f.write_str("API Key 为空或包含无效字符"),
            Self::MissingKey => f.write_str("请先保存此厂商的 API Key"),
            Self::Storage => f.write_str("读取或保存连接失败；请检查凭据目录"),
            Self::InvalidModels => f.write_str("模型目录格式错误，保留上次有效目录"),
            Self::TooLarge => f.write_str("模型目录超过大小限制"),
            Self::Network => f.write_str("模型刷新网络失败或超时，保留上次有效目录"),
            Self::Http(code) => write!(f, "模型刷新 HTTP {code}；请检查凭据、权限或稍后重试"),
            Self::AccountLogin => f.write_str("此连接使用浏览器账号登录，不接收 API Key"),
        }
    }
}
impl std::error::Error for ConnectionError {}
fn check(signal: &CancellationToken) -> Result<(), ConnectionError> {
    if signal.is_cancelled() {
        Err(ConnectionError::Cancelled)
    } else {
        Ok(())
    }
}
pub fn validate_key(key: &str) -> Result<(), ConnectionError> {
    if key.is_empty() || key.len() > 8192 || !key.bytes().all(|c| c.is_ascii_graphic()) {
        Err(ConnectionError::InvalidKey)
    } else {
        Ok(())
    }
}
pub fn save_key(
    storage: &AuthStorage,
    service: Service,
    key: &str,
    signal: &CancellationToken,
) -> Result<(), ConnectionError> {
    check(signal)?;
    if service.browser_login() {
        return Err(ConnectionError::AccountLogin);
    }
    validate_key(key)?;
    storage
        .write(
            service.id(),
            serde_json::json!({"type":"api_key", "key":key}),
            signal,
        )
        .map_err(|_| ConnectionError::Storage)
}
pub fn load_connection(
    storage: &AuthStorage,
    service: Service,
    signal: &CancellationToken,
) -> Result<Connection, ConnectionError> {
    check(signal)?;
    let value = storage
        .read(&service.preference_key(), signal)
        .map_err(|_| ConnectionError::Storage)?;
    let Some(value) = value else {
        return Ok(Connection::bundled(service));
    };
    if value["version"] != 1 {
        return Err(ConnectionError::InvalidModels);
    }
    let connection: Connection = serde_json::from_value(value["connection"].clone())
        .map_err(|_| ConnectionError::InvalidModels)?;
    validate_connection(&connection)?;
    if connection.service != service {
        return Err(ConnectionError::InvalidModels);
    }
    Ok(connection)
}
fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 512
        && !id.chars().any(char::is_whitespace)
        && !id.chars().any(char::is_control)
}
fn validate_connection(connection: &Connection) -> Result<(), ConnectionError> {
    if (!connection.selected_model.is_empty() && !valid_id(&connection.selected_model))
        || connection.models.len() > 2048
        || connection.models.iter().any(|m| !valid_id(&m.id))
    {
        return Err(ConnectionError::InvalidModels);
    }
    Ok(())
}
pub fn save_connection(
    storage: &AuthStorage,
    connection: &Connection,
    signal: &CancellationToken,
) -> Result<(), ConnectionError> {
    check(signal)?;
    validate_connection(connection)?;
    storage
        .write(
            &connection.service.preference_key(),
            serde_json::json!({"version":1,"connection":connection}),
            signal,
        )
        .map_err(|_| ConnectionError::Storage)
}
pub fn has_credential(
    storage: &AuthStorage,
    service: Service,
    signal: &CancellationToken,
) -> Result<bool, ConnectionError> {
    check(signal)?;
    let entries = storage.list(signal).map_err(|_| ConnectionError::Storage)?;
    Ok(entries.iter().any(|entry| {
        entry.provider_id == service.id()
            && entry.credential_type
                == if service.browser_login() {
                    "oauth"
                } else {
                    "api_key"
                }
    }))
}

/// No credentials are ever sent to the public catalog; Codex keeps its vetted local catalog.
pub fn refresh(
    storage: &AuthStorage,
    previous: &Connection,
    signal: &CancellationToken,
) -> Result<Connection, ConnectionError> {
    check(signal)?;
    let service = previous.service;
    if service == Service::Codex {
        let mut next = Connection::bundled(service);
        next.selected_model.clone_from(&previous.selected_model);
        // This is a local reload, not a remote/account refresh.
        return Ok(next);
    }
    let public = matches!(service, Service::Zhipu | Service::ZhipuCoding);
    let key = if public {
        None
    } else {
        let entry = storage
            .resolve_provider(service.id(), None, signal)
            .map_err(|_| ConnectionError::Storage)?
            .ok_or(ConnectionError::MissingKey)?;
        validate_key(&entry.0)?;
        Some(entry.0)
    };
    let url = if public {
        "https://models.dev/api.json".into()
    } else {
        format!("{}/models", service.base_url())
    };
    let value = fetch_json(&url, key.as_deref(), signal)?;
    parse_models(previous, &value, public)
}

pub fn parse_models(
    previous: &Connection,
    value: &Value,
    public: bool,
) -> Result<Connection, ConnectionError> {
    let entries: Vec<(&str, &Value)> = if public {
        let id = match previous.service {
            Service::Zhipu => "zhipuai",
            Service::ZhipuCoding => "zhipuai-coding-plan",
            _ => return Err(ConnectionError::InvalidModels),
        };
        value[id]["models"]
            .as_object()
            .ok_or(ConnectionError::InvalidModels)?
            .iter()
            .map(|(id, entry)| (id.as_str(), entry))
            .collect()
    } else {
        value["data"]
            .as_array()
            .ok_or(ConnectionError::InvalidModels)?
            .iter()
            .map(|entry| {
                Ok((
                    entry["id"].as_str().ok_or(ConnectionError::InvalidModels)?,
                    entry,
                ))
            })
            .collect::<Result<_, _>>()?
    };
    if entries.len() > 2048 {
        return Err(ConnectionError::TooLarge);
    }
    let mut models = Vec::new();
    for (id, entry) in entries {
        if !valid_id(id) || !entry.is_object() {
            return Err(ConnectionError::InvalidModels);
        }
        if public {
            // Explicitly non-chat models stay out of the agent picker. Unknown capabilities stay unknown.
            if entry["modalities"]["output"]
                .as_array()
                .is_some_and(|items| !items.iter().any(|v| v == "text"))
            {
                continue;
            }
        }
        models.push(DiscoveredModel {
            id: id.into(),
            context_window: if public {
                entry["limit"]["context"].as_u64()
            } else {
                entry["context_window"].as_u64()
            }
            .filter(|n| *n > 0),
            max_output: if public {
                entry["limit"]["output"].as_u64()
            } else {
                entry["max_output_tokens"].as_u64()
            }
            .filter(|n| *n > 0),
            metadata: entry.clone(),
        });
    }
    models.sort_by(|a, b| a.id.cmp(&b.id));
    if models.windows(2).any(|pair| pair[0].id == pair[1].id) {
        return Err(ConnectionError::InvalidModels);
    }
    Ok(Connection {
        models,
        source: if public {
            ModelSource::PublicCatalog
        } else {
            ModelSource::ProviderApi
        },
        refreshed_at: Some(timestamp()),
        ..previous.clone()
    })
}

fn fetch_json(
    url: &str,
    key: Option<&str>,
    signal: &CancellationToken,
) -> Result<Value, ConnectionError> {
    check(signal)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| ConnectionError::Network)?;
    runtime.block_on(async {
        let request = async {
            let url = reqwest::Url::parse(url).map_err(|_| ConnectionError::Network)?;
            let mut builder = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).connect_timeout(Duration::from_secs(8)).timeout(Duration::from_secs(30));
            if url.host_str().is_some_and(|host| matches!(host, "127.0.0.1" | "localhost" | "::1")) { builder = builder.no_proxy(); }
            let client = builder.build().map_err(|_| ConnectionError::Network)?;
            let mut request = client.get(url);
            if let Some(key) = key { request = request.bearer_auth(key); }
            let mut response = request.send().await.map_err(|_| ConnectionError::Network)?;
            if !response.status().is_success() { return Err(ConnectionError::Http(response.status().as_u16())); }
            const LIMIT: usize = 16 * 1024 * 1024;
            if response.content_length().is_some_and(|size| size > LIMIT as u64) { return Err(ConnectionError::TooLarge); }
            let mut body = Vec::new();
            while let Some(chunk) = response.chunk().await.map_err(|_| ConnectionError::Network)? {
                if body.len() + chunk.len() > LIMIT { return Err(ConnectionError::TooLarge); }
                body.extend_from_slice(&chunk);
            }
            serde_json::from_slice(&body).map_err(|_| ConnectionError::InvalidModels)
        };
        tokio::select! { result = request => result, _ = async { while !signal.is_cancelled() { tokio::time::sleep(Duration::from_millis(20)).await; } } => Err(ConnectionError::Cancelled) }
    })
}

#[cfg(test)]
#[path = "connections_tests.rs"]
mod tests;
