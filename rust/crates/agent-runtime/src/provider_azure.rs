//! Azure Responses: SDK-observed /responses route, deployment in payload.model.
//! https://learn.microsoft.com/en-us/azure/foundry/openai/how-to/responses
use super::responses::{parse_responses_sse, responses_request_payload_for_protocol};
use agent_core::types::{AssistantMessage, CancellationToken, Context};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::BufRead;

#[derive(Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct AzureOptions {
    pub base_url: String,
    pub resource_name: String,
    pub deployment: String,
    pub deployment_map: BTreeMap<String, String>,
    pub api_version: String,
    #[serde(skip_serializing)]
    pub bearer_token: String,
}
impl Default for AzureOptions {
    fn default() -> Self {
        Self {
            base_url: String::new(),
            resource_name: String::new(),
            deployment: String::new(),
            deployment_map: BTreeMap::new(),
            api_version: "v1".into(),
            bearer_token: String::new(),
        }
    }
}
fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_default().trim().to_owned()
}
impl AzureOptions {
    /// Reads configured environment strings only; no network or credential file I/O.
    pub fn from_env() -> Self {
        let mut options = Self {
            base_url: env("AZURE_OPENAI_BASE_URL"),
            resource_name: env("AZURE_OPENAI_RESOURCE_NAME"),
            deployment: env("AZURE_OPENAI_DEPLOYMENT"),
            bearer_token: env("AZURE_OPENAI_AUTH_TOKEN"),
            ..Self::default()
        };
        if options.base_url.is_empty() {
            options.base_url = env("AZURE_OPENAI_ENDPOINT");
        }
        let version = env("AZURE_OPENAI_API_VERSION");
        if !version.is_empty() {
            options.api_version = version;
        }
        options.deployment_map = env("AZURE_OPENAI_DEPLOYMENT_NAME_MAP")
            .split(',')
            .filter_map(|entry| {
                let (model, deployment) = entry.split_once('=')?;
                let model = model.trim();
                let deployment = deployment.trim();
                (!model.is_empty() && !deployment.is_empty())
                    .then(|| (model.to_owned(), deployment.to_owned()))
            })
            .collect();
        options
    }
    pub fn effective_deployment<'a>(&'a self, model: &'a str) -> &'a str {
        if !self.deployment.trim().is_empty() {
            self.deployment.trim()
        } else {
            self.deployment_map.get(model).map_or(model, String::as_str)
        }
    }
    pub fn resolved_base_url(&self, configured: &str) -> Result<reqwest::Url, String> {
        let base = if !configured.trim().is_empty() {
            configured.trim().to_owned()
        } else if !self.base_url.is_empty() {
            self.base_url.clone()
        } else {
            if self.resource_name.is_empty()
                || !self
                    .resource_name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            {
                return Err("Azure endpoint or resource name is required".into());
            }
            format!("https://{}.openai.azure.com/openai/v1", self.resource_name)
        };
        let mut url = super::clean_url(&base)?;
        let path = url.path().trim_end_matches('/');
        let azure_host = url.host_str().is_some_and(|host| {
            host.ends_with(".openai.azure.com")
                || host.ends_with(".cognitiveservices.azure.com")
                || host.ends_with(".ai.azure.com")
        });
        if path.is_empty()
            || path == "/"
            || path == "/openai"
            || (azure_host && path == "/openai/v1/responses")
        {
            url.set_path("/openai/v1");
        }
        Ok(url)
    }
    pub fn validate(&self, configured: &str, model: &str, key: &str) -> Result<(), String> {
        self.resolved_base_url(configured)?;
        if self.api_version.is_empty()
            || !self
                .api_version
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.'))
        {
            return Err("Invalid Azure API version".into());
        }
        let deployment = self.effective_deployment(model);
        if deployment.is_empty()
            || !deployment
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        {
            return Err("Invalid Azure deployment name".into());
        }
        super::validate_header(&self.bearer_token)?;
        if key.trim().is_empty() && self.bearer_token.is_empty() {
            return Err("Azure API key or explicit access token is required".into());
        }
        if !key.trim().is_empty() && !self.bearer_token.is_empty() {
            return Err("Azure API key and access token are mutually exclusive".into());
        }
        Ok(())
    }
    pub fn request_url(&self, configured: &str) -> Result<reqwest::Url, String> {
        let mut url = self.resolved_base_url(configured)?;
        let path = url.path().trim_end_matches('/');
        if !path.ends_with("/responses") {
            url.set_path(&format!("{path}/responses"));
        }
        // Original AzureOpenAI SDK always emits this query, including api-version=v1.
        url.query_pairs_mut()
            .append_pair("api-version", &self.api_version);
        Ok(url)
    }
    pub fn route_identity(&self, url: &reqwest::Url, model: &str) -> String {
        let mut identity = url.clone();
        identity.set_fragment(Some(&format!(
            "deployment={}",
            self.effective_deployment(model)
        )));
        identity.into()
    }
}

pub fn azure_request_payload(
    context: &Context,
    model: &str,
    tools: &[Value],
    options: &AzureOptions,
) -> Value {
    let deployment = options.effective_deployment(model);
    let mut payload = responses_request_payload_for_protocol(
        context,
        deployment,
        tools,
        "azure-openai-responses",
    );
    payload["model"] = json!(deployment);
    payload
}
pub fn parse_azure_sse(
    reader: impl BufRead,
    signal: &CancellationToken,
    on_update: &mut dyn FnMut(AssistantMessage),
) -> Result<AssistantMessage, String> {
    let mut update = |mut message: AssistantMessage| {
        if let Some(metadata) = message.protocol_metadata.as_mut() {
            metadata["protocol"] = json!("azure-openai-responses");
        }
        on_update(message);
    };
    let mut message = parse_responses_sse(reader, signal, &mut update)?;
    if let Some(metadata) = message.protocol_metadata.as_mut() {
        metadata["protocol"] = json!("azure-openai-responses");
    }
    Ok(message)
}
