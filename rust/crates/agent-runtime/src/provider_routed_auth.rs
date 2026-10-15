//! Provider-owned routes and headers, separate from the reused wire protocol.
use super::{clean_url, validate_header, ProviderMode};
use agent_core::types::{Content, Context, Message};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct CloudflareOptions {
    pub account_id: String,
    pub gateway_id: String,
}
impl CloudflareOptions {
    pub fn from_env() -> Self {
        Self {
            account_id: std::env::var("CLOUDFLARE_ACCOUNT_ID").unwrap_or_default(),
            gateway_id: std::env::var("CLOUDFLARE_GATEWAY_ID").unwrap_or_default(),
        }
    }
    pub fn resolve_base(&self, provider: &str, configured: &str) -> Result<reqwest::Url, String> {
        segment(&self.account_id)?;
        if provider == "cloudflare-ai-gateway" {
            segment(&self.gateway_id)?;
        }
        let source = configured
            .replace("{CLOUDFLARE_ACCOUNT_ID}", &self.account_id)
            .replace("{CLOUDFLARE_GATEWAY_ID}", &self.gateway_id);
        let url = clean_url(&source)?;
        if url.host_str() == Some("workers-binding.ai") {
            return Err("Cloudflare Workers binding requires a binding transport".into());
        }
        Ok(url)
    }
}
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct CopilotOptions {
    pub enterprise_domain: String,
    pub base_url: String,
}
impl CopilotOptions {
    pub fn from_env() -> Self {
        Self {
            enterprise_domain: std::env::var("GITHUB_COPILOT_ENTERPRISE_DOMAIN")
                .unwrap_or_default(),
            base_url: std::env::var("AGENT_CORE_COPILOT_BASE_URL").unwrap_or_default(),
        }
    }
    pub fn resolve_base(&self, configured: &str, token: &str) -> Result<reqwest::Url, String> {
        let configured = if self.base_url.is_empty() {
            configured
        } else {
            &self.base_url
        };
        let mut url = clean_url(configured)?;
        // Explicit proxy/loopback bases remain explicit. Catalog defaults may be
        // replaced by the account endpoint embedded in a Copilot access token.
        if self.base_url.is_empty()
            && url
                .host_str()
                .is_some_and(|host| host.ends_with(".githubcopilot.com"))
        {
            if let Some(host) = token
                .split(';')
                .find_map(|part| part.strip_prefix("proxy-ep="))
            {
                if !host
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-'))
                    || !host.ends_with(".githubcopilot.com")
                {
                    return Err("Invalid Copilot account endpoint".into());
                }
                url = clean_url(&format!("https://{}", host.replacen("proxy.", "api.", 1)))?;
            } else if !self.enterprise_domain.is_empty() {
                let domain = &self.enterprise_domain;
                if !domain
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-'))
                {
                    return Err("Invalid Copilot enterprise domain".into());
                }
                url = clean_url(&format!("https://copilot-api.{domain}"))?;
            }
        }
        Ok(url)
    }
}
fn segment(value: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(
            "Cloudflare account/gateway configuration is required and must be a URL segment".into(),
        );
    }
    Ok(())
}
pub(super) fn validate(provider: &str, key: &str, mode: &ProviderMode) -> Result<(), String> {
    if matches!(
        provider,
        "github-copilot" | "cloudflare-workers-ai" | "cloudflare-ai-gateway"
    ) {
        if key.trim().is_empty() {
            return Err("Provider inference token is required".into());
        }
        validate_header(key)?;
        if !matches!(
            mode,
            ProviderMode::OpenAiCompatible
                | ProviderMode::OpenAiResponses
                | ProviderMode::Anthropic
        ) || provider == "cloudflare-workers-ai" && *mode != ProviderMode::OpenAiCompatible
        {
            return Err("Provider does not support the configured wire protocol".into());
        }
    }
    Ok(())
}
pub(super) fn headers(provider: &str, key: &str, context: &Context) -> BTreeMap<String, String> {
    if provider == "cloudflare-ai-gateway" {
        return BTreeMap::from([("cf-aig-authorization".into(), format!("Bearer {key}"))]);
    }
    let mut headers = BTreeMap::from([("authorization".into(), format!("Bearer {key}"))]);
    if provider == "github-copilot" {
        headers.extend([
            ("user-agent".into(), "GitHubCopilotChat/0.35.0".into()),
            ("editor-version".into(), "vscode/1.107.0".into()),
            ("editor-plugin-version".into(), "copilot-chat/0.35.0".into()),
            ("copilot-integration-id".into(), "vscode-chat".into()),
            ("openai-intent".into(), "conversation-edits".into()),
            (
                "x-initiator".into(),
                if context
                    .messages
                    .last()
                    .is_none_or(|message| matches!(message, Message::User(_)))
                {
                    "user"
                } else {
                    "agent"
                }
                .into(),
            ),
        ]);
        if context.messages.iter().any(|message| match message {
            Message::User(user) => user
                .content
                .iter()
                .any(|part| matches!(part, Content::Image { .. })),
            Message::ToolResult(tool) => tool
                .content
                .iter()
                .any(|part| matches!(part, Content::Image { .. })),
            _ => false,
        }) {
            headers.insert("copilot-vision-request".into(), "true".into());
        }
    }
    headers
}
