//! Offline model catalog copied losslessly from the TypeScript migration oracle.
//! Protocol support is not a claim that credentials or every model feature work.

use crate::provider::ProviderMode;
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::OnceLock;

pub const SNAPSHOT_JSON: &str = include_str!("models.snapshot.json");

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelRecord {
    pub provider: String,
    pub id: String,
    pub name: String,
    pub api: String,
    pub base_url: String,
    pub reasoning: bool,
    pub input: Vec<String>,
    pub cost: Value,
    pub context_window: u64,
    pub max_tokens: u64,
    /// Exact complete model object, including compatibility, reasoning mappings,
    /// tiered costs, headers, and any future metadata fields.
    pub metadata: Value,
}

impl ModelRecord {
    pub fn canonical_id(&self) -> String {
        format!("{}/{}", self.provider, self.id)
    }

    pub fn is_supported(&self) -> bool {
        self.mode().is_some()
    }

    /// Native protocols with at least one migrated authentication route.
    /// This marker does not imply every ambient credential type is implemented.
    pub fn mode(&self) -> Option<ProviderMode> {
        if self.provider == "openai-codex" && self.api == "openai-codex-responses" {
            return Some(ProviderMode::OpenAiCodex);
        }
        if self.provider == "google-vertex" && self.api == "google-vertex" {
            return Some(ProviderMode::GoogleVertex);
        }
        if self.provider == "azure-openai-responses" && self.api == "azure-openai-responses" {
            return Some(ProviderMode::AzureOpenAiResponses);
        }
        if self.provider == "amazon-bedrock" && self.api == "bedrock-converse-stream" {
            return Some(ProviderMode::AmazonBedrock);
        }
        if self.provider == "cloudflare-workers-ai" && self.api != "openai-completions" {
            return None;
        }
        if matches!(
            self.provider.as_str(),
            "github-copilot" | "cloudflare-ai-gateway"
        ) && !matches!(
            self.api.as_str(),
            "openai-completions" | "openai-responses" | "anthropic-messages"
        ) {
            return None;
        }
        if matches!(
            self.provider.as_str(),
            "openai-codex" | "amazon-bedrock" | "google-vertex"
        ) {
            return None;
        }
        match self.api.as_str() {
            "openai-completions" => Some(ProviderMode::OpenAiCompatible),
            "openai-responses" => Some(ProviderMode::OpenAiResponses),
            "anthropic-messages" => Some(ProviderMode::Anthropic),
            "google-generative-ai" => Some(ProviderMode::Google),
            "mistral-conversations" => Some(ProviderMode::Mistral),
            "pi-messages" => Some(ProviderMode::PiMessages),
            _ => None,
        }
    }

    pub fn unsupported_reason(&self) -> Option<&'static str> {
        if self.is_supported() {
            return None;
        }
        Some(
            if matches!(
                self.provider.as_str(),
                "github-copilot"
                    | "openai-codex"
                    | "amazon-bedrock"
                    | "google-vertex"
                    | "cloudflare-workers-ai"
                    | "cloudflare-ai-gateway"
            ) {
                "Provider credential or deployment route has not been migrated"
            } else {
                "Provider protocol has not been migrated"
            },
        )
    }
}

struct Catalog {
    models: Vec<ModelRecord>,
    /// Provider and model IDs stay exact; model IDs containing '/' are not split.
    index: BTreeMap<String, BTreeMap<String, usize>>,
}

fn string_field(object: &Value, field: &str) -> Result<String, String> {
    object[field]
        .as_str()
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| format!("Model field {field} must be a non-empty string"))
}

fn parse_snapshot(source: &str) -> Result<Catalog, String> {
    let snapshot: Value =
        serde_json::from_str(source).map_err(|error| format!("Invalid model snapshot: {error}"))?;
    if snapshot["schemaVersion"] != 1 {
        return Err("Unsupported model snapshot schema version".into());
    }
    let providers = snapshot["providers"]
        .as_object()
        .ok_or("Snapshot providers must be an object")?;
    if providers.is_empty() {
        return Err("Snapshot providers must not be empty".into());
    }
    let mut models = Vec::new();
    let mut index = BTreeMap::new();
    for (provider, entries) in providers {
        if provider.is_empty() {
            return Err("Provider ID must not be empty".into());
        }
        let entries = entries
            .as_object()
            .ok_or("Provider model catalog must be an object")?;
        let mut provider_index = BTreeMap::new();
        for (id, metadata) in entries {
            if string_field(metadata, "provider")? != *provider
                || string_field(metadata, "id")? != *id
            {
                return Err(format!(
                    "Catalog identity differs from canonical key {provider}/{id}"
                ));
            }
            let input = metadata["input"]
                .as_array()
                .ok_or("Model input must be an array")?
                .iter()
                .map(|input| {
                    input
                        .as_str()
                        .map(str::to_owned)
                        .ok_or_else(|| "Input modalities must be strings".to_owned())
                })
                .collect::<Result<Vec<_>, _>>()?;
            if !metadata["cost"].is_object() {
                return Err("Model cost must be an object".into());
            }
            let context_window = metadata["contextWindow"]
                .as_u64()
                .filter(|value| *value > 0)
                .ok_or("Model contextWindow must be positive")?;
            let max_tokens = metadata["maxTokens"]
                .as_u64()
                .filter(|value| *value > 0)
                .ok_or("Model maxTokens must be positive")?;
            let record = ModelRecord {
                provider: provider.clone(),
                id: id.clone(),
                name: string_field(metadata, "name")?,
                api: string_field(metadata, "api")?,
                // Azure catalog entries intentionally defer the deployment URL.
                base_url: metadata["baseUrl"]
                    .as_str()
                    .ok_or("Model baseUrl must be a string")?
                    .to_owned(),
                reasoning: metadata["reasoning"]
                    .as_bool()
                    .ok_or("Model reasoning must be a boolean")?,
                input,
                cost: metadata["cost"].clone(),
                context_window,
                max_tokens,
                metadata: metadata.clone(),
            };
            provider_index.insert(id.clone(), models.len());
            models.push(record);
        }
        index.insert(provider.clone(), provider_index);
    }
    if models.is_empty() {
        return Err("Snapshot must contain models".into());
    }
    Ok(Catalog { models, index })
}

fn embedded() -> &'static Catalog {
    static CATALOG: OnceLock<Catalog> = OnceLock::new();
    CATALOG.get_or_init(|| {
        parse_snapshot(SNAPSHOT_JSON).expect("Bundled model snapshot is validated by catalog tests")
    })
}

pub fn models() -> &'static [ModelRecord] {
    &embedded().models
}

pub fn find(provider: &str, id: &str) -> Option<&'static ModelRecord> {
    let index = *embedded().index.get(provider)?.get(id)?;
    embedded().models.get(index)
}

/// Canonical identifiers split only at the provider separator, never inside a
/// model ID such as 'organization/model'. No aliases or case folding are guessed.
pub fn find_canonical(canonical: &str) -> Option<&'static ModelRecord> {
    let (provider, id) = canonical.split_once('/')?;
    find(provider, id)
}

pub fn search(query: &str) -> Vec<&'static ModelRecord> {
    let query = query.trim().to_lowercase();
    models()
        .iter()
        .filter(|model| {
            query.is_empty()
                || model.canonical_id().to_lowercase().contains(&query)
                || model.name.to_lowercase().contains(&query)
                || model.api.to_lowercase().contains(&query)
        })
        .collect()
}

pub fn providers() -> Vec<&'static str> {
    embedded().index.keys().map(String::as_str).collect()
}

/// Used by the native offline update helper before replacing any snapshot file.
pub fn validate_snapshot(source: &str) -> Result<usize, String> {
    parse_snapshot(source).map(|catalog| catalog.models.len())
}
