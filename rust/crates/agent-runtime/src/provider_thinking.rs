//! Simple thinking levels map to each provider's native wire contract.
//! Model capabilities are catalog metadata or an explicit custom-model override.
use super::{ProviderMode, ProviderSettings};
use agent_core::types::{Context, Message, ThinkingLevel};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThinkingCapabilities {
    pub reasoning: bool,
    pub max_tokens: u64,
    pub context_window: u64,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub thinking_level_map: BTreeMap<String, Option<String>>,
    #[serde(default)]
    pub compat: Value,
}
fn provider(settings: &ProviderSettings) -> &str {
    if !settings.options.provider_id.is_empty() {
        return &settings.options.provider_id;
    }
    match settings.mode {
        ProviderMode::Anthropic => "anthropic",
        ProviderMode::Google => "google",
        ProviderMode::GoogleVertex => "google-vertex",
        ProviderMode::AmazonBedrock => "amazon-bedrock",
        ProviderMode::AzureOpenAiResponses => "azure-openai-responses",
        ProviderMode::OpenAiCodex => "openai-codex",
        ProviderMode::Mistral => "mistral",
        ProviderMode::PiMessages => "pi-messages",
        _ => "openai",
    }
}
fn capabilities(settings: &ProviderSettings) -> Option<ThinkingCapabilities> {
    settings.options.thinking_capabilities.clone().or_else(|| {
        crate::catalog::find(provider(settings), &settings.model).map(|record| {
            ThinkingCapabilities {
                reasoning: record.reasoning,
                max_tokens: record.max_tokens,
                context_window: record.context_window,
                name: record.name.clone(),
                thinking_level_map: serde_json::from_value(
                    record.metadata["thinkingLevelMap"].clone(),
                )
                .unwrap_or_default(),
                compat: record.metadata["compat"].clone(),
            }
        })
    })
}
pub(super) fn supported_levels(settings: &ProviderSettings) -> Vec<ThinkingLevel> {
    if settings.mode == ProviderMode::Demo {
        return ThinkingLevel::ALL.to_vec();
    }
    let Some(cap) = capabilities(settings).filter(|cap| cap.reasoning) else {
        return vec![ThinkingLevel::Off];
    };
    ThinkingLevel::ALL
        .into_iter()
        .filter(|level| match cap.thinking_level_map.get(level.as_str()) {
            Some(None) => false,
            Some(Some(_)) => true,
            None => !matches!(level, ThinkingLevel::Xhigh | ThinkingLevel::Max),
        })
        .collect()
}
pub(super) fn clamp_level(settings: &ProviderSettings, level: ThinkingLevel) -> ThinkingLevel {
    let available = supported_levels(settings);
    ThinkingLevel::ALL
        .into_iter()
        .filter(|candidate| *candidate >= level)
        .find(|candidate| available.contains(candidate))
        .or_else(|| {
            ThinkingLevel::ALL
                .into_iter()
                .rev()
                .filter(|candidate| *candidate < level)
                .find(|candidate| available.contains(candidate))
        })
        .unwrap_or(ThinkingLevel::Off)
}
pub(super) fn validate(settings: &ProviderSettings) -> Result<(), String> {
    if settings.options.max_tokens == Some(0)
        || settings
            .options
            .thinking_budgets
            .iter()
            .any(|(level, budget)| {
                !matches!(level.as_str(), "minimal" | "low" | "medium" | "high") || *budget == 0
            })
    {
        return Err("Invalid thinking output cap or token budget".into());
    }
    if let Some(cap) = &settings.options.thinking_capabilities {
        if cap.max_tokens == 0
            || cap.context_window == 0
            || cap.thinking_level_map.iter().any(|(level, value)| {
                !ThinkingLevel::ALL
                    .iter()
                    .any(|known| known.as_str() == level)
                    || value.as_ref().is_some_and(|value| value.is_empty())
            })
        {
            return Err("Invalid explicit thinking capabilities".into());
        }
    }
    if settings.mode != ProviderMode::Demo
        && settings
            .options
            .thinking_level
            .is_some_and(|level| level != ThinkingLevel::Off)
        && capabilities(settings).is_none()
    {
        return Err("Thinking requires catalog or explicit custom-model capabilities".into());
    }
    Ok(())
}
pub(super) fn stamp_metadata(settings: &ProviderSettings, metadata: &mut Value) {
    if let Some(level) = settings.options.thinking_level {
        let level = clamp_level(settings, level);
        metadata["thinking_level"] = json!(level);
        if settings.mode == ProviderMode::Anthropic {
            if let Some(cap) =
                capabilities(settings).filter(|cap| cap.compat["supportsMidConvoEffort"] == true)
            {
                let fallback = match level {
                    ThinkingLevel::Minimal | ThinkingLevel::Low => "low",
                    ThinkingLevel::Medium => "medium",
                    _ => "high",
                };
                metadata["provider_thinking_level"] = json!(mapped(&cap, level, fallback));
            }
        }
    }
}
pub(super) fn anthropic_betas(settings: &ProviderSettings) -> Vec<&'static str> {
    let Some(cap) = capabilities(settings).filter(|_| settings.mode == ProviderMode::Anthropic)
    else {
        return vec![];
    };
    let mut betas = vec![];
    if cap.reasoning
        && settings
            .options
            .thinking_level
            .is_some_and(|level| clamp_level(settings, level) != ThinkingLevel::Off)
        && cap.compat["forceAdaptiveThinking"] != true
    {
        betas.push("interleaved-thinking-2025-05-14");
    }
    if cap.compat["supportsMidConvoEffort"] == true {
        betas.extend([
            "mid-conversation-output-config-2026-07-01",
            "thinking-binding-controls-2026-08-01",
        ]);
    }
    betas
}
fn mapped<'a>(cap: &'a ThinkingCapabilities, level: ThinkingLevel, fallback: &'a str) -> &'a str {
    cap.thinking_level_map
        .get(level.as_str())
        .and_then(Option::as_deref)
        .unwrap_or(fallback)
}
fn budget(settings: &ProviderSettings, level: ThinkingLevel) -> u64 {
    let level = if matches!(level, ThinkingLevel::Xhigh | ThinkingLevel::Max) {
        ThinkingLevel::High
    } else {
        level
    };
    settings
        .options
        .thinking_budgets
        .get(level.as_str())
        .copied()
        .unwrap_or(match level {
            ThinkingLevel::Off => 0,
            ThinkingLevel::Minimal => 1024,
            ThinkingLevel::Low => 2048,
            ThinkingLevel::Medium => 8192,
            _ => 16384,
        })
}
fn ceiling(
    settings: &ProviderSettings,
    cap: &ThinkingCapabilities,
    context: &Context,
    tools: &[Value],
) -> u64 {
    let explicit = settings.options.max_tokens.or_else(|| match settings.mode {
        ProviderMode::AmazonBedrock => settings.options.bedrock.max_tokens.map(u64::from),
        ProviderMode::Mistral => settings.options.mistral.max_tokens,
        ProviderMode::PiMessages => settings.options.pi_messages.max_tokens,
        _ => None,
    });
    let estimate = context.system_prompt.encode_utf16().count().div_ceil(4) as f64
        + context
            .messages
            .iter()
            .map(crate::automatic::estimate_message_tokens)
            .sum::<f64>()
        + if tools.is_empty() {
            0.0
        } else {
            tools
                .iter()
                .map(|tool| tool.to_string().encode_utf16().count())
                .sum::<usize>()
                .div_ceil(4) as f64
        };
    explicit.unwrap_or(cap.max_tokens).min(cap.max_tokens).min(
        (cap.context_window as f64 - estimate - 4096.0)
            .floor()
            .max(1.0) as u64,
    )
}
fn template(values: &Value, enabled: bool, effort: Option<&str>, budget: u64) -> Option<Value> {
    let object = values.as_object()?;
    let mut result = serde_json::Map::new();
    for (key, value) in object {
        if value.is_object() {
            if !enabled && value["omitWhenOff"] == true {
                continue;
            }
            let resolved = match value["$var"].as_str() {
                Some("thinking.enabled") => Some(json!(enabled)),
                Some("thinking.budget") => (enabled && budget > 0).then(|| json!(budget)),
                Some("thinking.effort") => effort.map(|effort| json!(effort)),
                _ => return None,
            };
            if let Some(resolved) = resolved {
                result.insert(key.clone(), resolved);
            }
        } else {
            result.insert(key.clone(), value.clone());
        }
    }
    (!result.is_empty()).then_some(Value::Object(result))
}
/// Applies only an explicitly selected level. None preserves pre-migration defaults.
pub fn apply_thinking(
    settings: &ProviderSettings,
    context: &Context,
    tools: &[Value],
    payload: &mut Value,
) -> Result<(), String> {
    validate(settings)?;
    if let Some(max_tokens) = settings.options.max_tokens {
        let output = capabilities(settings)
            .as_ref()
            .map_or(max_tokens, |cap| ceiling(settings, cap, context, tools));
        match settings.mode {
            ProviderMode::Anthropic | ProviderMode::Mistral => {
                payload["max_tokens"] = json!(output);
            }
            ProviderMode::OpenAiResponses | ProviderMode::AzureOpenAiResponses => {
                payload["max_output_tokens"] = json!(output);
            }
            ProviderMode::PiMessages => {
                payload["options"]["maxTokens"] = json!(output);
            }
            ProviderMode::AmazonBedrock => {
                if payload.get("inferenceConfig").is_none() {
                    payload["inferenceConfig"] = json!({});
                }
                payload["inferenceConfig"]["maxTokens"] = json!(output);
            }
            ProviderMode::Google | ProviderMode::GoogleVertex => {
                if payload.get("generationConfig").is_none() {
                    payload["generationConfig"] = json!({});
                }
                payload["generationConfig"]["maxOutputTokens"] = json!(output);
            }
            ProviderMode::OpenAiCompatible => {
                let cap = capabilities(settings);
                let field = cap
                    .as_ref()
                    .and_then(|cap| cap.compat["maxTokensField"].as_str())
                    .unwrap_or("max_completion_tokens");
                payload[field] = json!(output);
            }
            ProviderMode::OpenAiCodex | ProviderMode::Demo => {}
        }
    }
    let Some(requested) = settings.options.thinking_level else {
        return Ok(());
    };
    let Some(cap) = capabilities(settings) else {
        return Ok(());
    };
    if !cap.reasoning {
        return Ok(());
    }
    let level = clamp_level(settings, requested);
    let enabled = level != ThinkingLevel::Off;
    let id = settings.model.to_lowercase();
    let compat = &cap.compat;
    let output = ceiling(settings, &cap, context, tools);
    let tokens = budget(settings, level).min(output.saturating_sub(1024));
    match settings.mode {
        ProviderMode::Demo => {}
        ProviderMode::PiMessages => {
            if enabled {
                payload["options"]["reasoning"] = json!(level);
            } else {
                payload["options"]
                    .as_object_mut()
                    .expect("Owned options")
                    .remove("reasoning");
            }
            payload["options"]["maxTokens"] = json!(output);
        }
        ProviderMode::Mistral => {
            payload["max_tokens"] = json!(output);
            payload
                .as_object_mut()
                .expect("Owned payload")
                .remove("reasoning_effort");
            payload
                .as_object_mut()
                .expect("Owned payload")
                .remove("prompt_mode");
            if enabled {
                if matches!(
                    id.as_str(),
                    "mistral-small-2603" | "mistral-small-latest" | "mistral-medium-3.5"
                ) {
                    let effort = mapped(&cap, level, "high");
                    if !matches!(effort, "none" | "high") {
                        return Err("Invalid Mistral thinking mapping".into());
                    }
                    payload["reasoning_effort"] = json!(effort);
                } else {
                    payload["prompt_mode"] = json!("reasoning");
                }
            }
        }
        ProviderMode::OpenAiResponses
        | ProviderMode::OpenAiCodex
        | ProviderMode::AzureOpenAiResponses => {
            if settings.mode != ProviderMode::OpenAiCodex {
                payload["max_output_tokens"] = json!(output);
            }
            if enabled {
                payload["reasoning"] =
                    json!({"effort":mapped(&cap,level,level.as_str()),"summary":"auto"});
                payload["include"] = json!(["reasoning.encrypted_content"]);
            } else if settings.mode != ProviderMode::OpenAiCodex
                && provider(settings) != "github-copilot"
            {
                payload["reasoning"] = json!({"effort":mapped(&cap,level,"none")});
            } else {
                payload
                    .as_object_mut()
                    .expect("Owned payload")
                    .remove("reasoning");
            }
        }
        ProviderMode::Google | ProviderMode::GoogleVertex => {
            let pro = id.contains("gemini-3") && id.contains("-pro");
            let flash = id.contains("gemini-3") && id.contains("-flash")
                || matches!(
                    id.as_str(),
                    "gemini-flash-latest" | "gemini-flash-lite-latest"
                );
            let gemma = id.contains("gemma-4") || id.contains("gemma4");
            if payload.get("generationConfig").is_none() {
                payload["generationConfig"] = json!({});
            }
            payload["generationConfig"]["maxOutputTokens"] = json!(output);
            let config = if !enabled {
                if pro {
                    json!({"thinkingLevel":"LOW"})
                } else if flash || gemma {
                    json!({"thinkingLevel":"MINIMAL"})
                } else {
                    json!({"thinkingBudget":0})
                }
            } else {
                let effort = mapped(&cap, level, level.as_str()).to_lowercase();
                let index = match effort.as_str() {
                    "minimal" => 0,
                    "low" => 1,
                    "medium" => 2,
                    "high" => 3,
                    _ => return Err("Invalid Google thinking mapping".into()),
                };
                if pro || flash || gemma {
                    json!({"includeThoughts":true,"thinkingLevel":if pro{if index<2{"LOW"}else{"HIGH"}}else if gemma{if index<2{"MINIMAL"}else{"HIGH"}}else{["MINIMAL","LOW","MEDIUM","HIGH"][index]}})
                } else {
                    let defaults = if id.contains("2.5-pro") {
                        [128, 2048, 8192, 32768]
                    } else if id.contains("2.5-flash-lite") {
                        [512, 2048, 8192, 24576]
                    } else {
                        [128, 2048, 8192, 24576]
                    };
                    let custom = settings
                        .options
                        .thinking_budgets
                        .get(effort.as_str())
                        .copied();
                    let known = id.contains("2.5-pro") || id.contains("2.5-flash");
                    let tokens = custom.map(|tokens| tokens as i64).unwrap_or(if known {
                        defaults[index]
                    } else {
                        -1
                    });
                    json!({"includeThoughts":true,"thinkingBudget":tokens})
                }
            };
            payload["generationConfig"]["thinkingConfig"] = config;
        }
        ProviderMode::Anthropic | ProviderMode::AmazonBedrock => {
            let bedrock = settings.mode == ProviderMode::AmazonBedrock;
            let candidates =
                format!("{id} {}", cap.name.to_lowercase()).replace([' ', '_', '.', ':'], "-");
            let claude = !bedrock || candidates.contains("claude");
            if !claude {
                return Ok(());
            }
            let adaptive = if bedrock {
                [
                    "opus-4-6",
                    "opus-4-7",
                    "opus-4-8",
                    "opus-5",
                    "sonnet-4-6",
                    "sonnet-5",
                    "fable-5",
                ]
                .iter()
                .any(|part| candidates.contains(part))
            } else {
                compat["forceAdaptiveThinking"] == true
            };
            if bedrock {
                if payload.get("inferenceConfig").is_none() {
                    payload["inferenceConfig"] = json!({});
                }
                payload["inferenceConfig"]["maxTokens"] = json!(output);
            } else {
                payload["max_tokens"] = json!(output);
            }
            let managed = !bedrock && compat["supportsMidConvoEffort"] == true;
            let fallback = match level {
                ThinkingLevel::Minimal | ThinkingLevel::Low => "low",
                ThinkingLevel::Medium => "medium",
                _ => "high",
            };
            if managed {
                let mut history = context.messages.iter().filter_map(|message| {
                    if let Message::Assistant(assistant) = message {
                        (!assistant.content.is_empty()).then_some(assistant)
                    } else {
                        None
                    }
                });
                let mut rows = vec![];
                for row in payload["messages"].as_array().into_iter().flatten() {
                    if row["role"] == "assistant" {
                        if let Some(metadata) = history
                            .next()
                            .and_then(|assistant| assistant.protocol_metadata.as_ref())
                            .filter(|metadata| {
                                metadata["provider"] == provider(settings)
                                    && metadata["protocol"] == "anthropic"
                            })
                        {
                            if let Some(effort) = metadata["provider_thinking_level"]
                                .as_str()
                                .filter(|effort| {
                                    matches!(*effort, "low" | "medium" | "high" | "xhigh" | "max")
                                })
                            {
                                rows.push(json!({"role":"system","content":[],"output_config":{"effort":effort}}));
                            }
                        }
                    }
                    rows.push(row.clone());
                }
                rows.push(json!({"role":"system","content":[],"output_config":{"effort":if enabled{mapped(&cap,level,fallback)}else{"high"}}}));
                payload["messages"] = json!(rows);
            }
            let mut config = if managed {
                json!({"thinking":{"type":"adaptive","display":"summarized","block_binding":{"prefix_mismatch_behavior":"drop_block"}},"output_config":{"effort":"high"}})
            } else if enabled && adaptive {
                json!({"thinking":{"type":"adaptive","display":"summarized"},"output_config":{"effort":mapped(&cap,level,fallback)}})
            } else if enabled {
                if tokens < 1024 {
                    return Err("Thinking output cap leaves less than 1024 budget tokens".into());
                }
                json!({"thinking":{"type":"enabled","budget_tokens":tokens,"display":"summarized"}})
            } else if !bedrock {
                json!({"thinking":{"type":"disabled"}})
            } else {
                json!({})
            };
            if enabled {
                payload
                    .as_object_mut()
                    .expect("Owned payload")
                    .remove("temperature");
                if bedrock {
                    payload["inferenceConfig"]
                        .as_object_mut()
                        .expect("Owned inference")
                        .remove("temperature");
                    if !adaptive {
                        config["anthropic_beta"] = json!(["interleaved-thinking-2025-05-14"]);
                    }
                    if settings.options.bedrock.region.starts_with("us-gov-")
                        || id.starts_with("us-gov.")
                        || id.starts_with("arn:aws-us-gov:")
                    {
                        if let Some(thinking) = config["thinking"].as_object_mut() {
                            thinking.remove("display");
                        }
                    }
                }
            }
            if bedrock {
                if !config.as_object().expect("Owned config").is_empty() {
                    payload["additionalModelRequestFields"] = config;
                }
            } else {
                for (key, value) in config.as_object().expect("Owned config") {
                    payload[key] = value.clone();
                }
            }
        }
        ProviderMode::OpenAiCompatible => {
            let p = provider(settings);
            let base = &settings.base_url;
            let format = compat["thinkingFormat"].as_str().unwrap_or(
                if p == "deepseek" || base.contains("deepseek.com") {
                    "deepseek"
                } else if p.starts_with("zai") || base.contains("bigmodel.cn") {
                    "zai"
                } else if p == "together" || base.contains("together.ai") {
                    "together"
                } else if p == "ant-ling" {
                    "ant-ling"
                } else if p == "openrouter" || base.contains("openrouter.ai") {
                    "openrouter"
                } else {
                    "openai"
                },
            );
            let supports = compat["supportsReasoningEffort"]
                .as_bool()
                .unwrap_or(!matches!(
                    p,
                    "xai"
                        | "zai"
                        | "zai-coding-plan"
                        | "moonshotai"
                        | "moonshotai-cn"
                        | "together"
                        | "cloudflare-ai-gateway"
                        | "nvidia"
                        | "ant-ling"
                ));
            let effort = if enabled {
                Some(mapped(&cap, level, level.as_str()))
            } else {
                cap.thinking_level_map.get("off").and_then(Option::as_deref)
            };
            let maxfield = compat["maxTokensField"].as_str().unwrap_or(
                if matches!(format, "deepseek" | "zai" | "together" | "ant-ling")
                    || matches!(
                        p,
                        "moonshotai" | "moonshotai-cn" | "cloudflare-ai-gateway" | "nvidia"
                    )
                    || base.contains("chutes.ai")
                {
                    "max_tokens"
                } else {
                    "max_completion_tokens"
                },
            );
            payload[maxfield] = json!(output);
            match format {
                "zai" => {
                    payload["thinking"] = if enabled {
                        json!({"type":"enabled","clear_thinking":false})
                    } else {
                        json!({"type":"disabled"})
                    };
                }
                "qwen" => {
                    payload["enable_thinking"] = json!(enabled);
                }
                "qwen-chat-template" => {
                    payload["chat_template_kwargs"] =
                        json!({"enable_thinking":enabled,"preserve_thinking":true});
                }
                "chat-template" | "baseten" => {
                    let (key, source) = if format == "baseten" {
                        ("chat_template_args", "chatTemplateArgs")
                    } else {
                        ("chat_template_kwargs", "chatTemplateKwargs")
                    };
                    if let Some(values) = template(&compat[source], enabled, effort, tokens) {
                        payload[key] = values;
                    }
                }
                "deepseek" => {
                    payload["thinking"] = json!({"type":if enabled{"enabled"}else{"disabled"}});
                }
                "openrouter" => {
                    payload["reasoning"] = json!({"effort":effort.unwrap_or("none")});
                }
                "ant-ling" => {
                    if let Some(effort) = effort {
                        payload["reasoning"] = json!({"effort":effort});
                    }
                }
                "together" => {
                    payload["reasoning"] = json!({"enabled":enabled});
                }
                "string-thinking" => {
                    payload["thinking"] = json!(effort.unwrap_or("none"));
                }
                "openai" => {}
                _ => return Err("Unsupported thinking compatibility format".into()),
            }
            if supports
                && !matches!(
                    format,
                    "qwen-chat-template"
                        | "chat-template"
                        | "openrouter"
                        | "ant-ling"
                        | "string-thinking"
                )
            {
                if let Some(effort) = effort {
                    payload["reasoning_effort"] = json!(effort);
                }
            }
            let field = compat["thinkingTokenBudgetField"].as_str().or_else(|| {
                (compat["supportsThinkingTokenBudget"] == true).then_some("thinking_token_budget")
            });
            if enabled && tokens > 0 {
                if let Some(field) = field {
                    payload[field] = json!(tokens);
                }
            }
        }
    }
    Ok(())
}
