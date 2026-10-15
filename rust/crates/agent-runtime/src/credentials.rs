//! API-key discovery only. OAuth/ambient credentials are not treated as API keys.
pub fn api_key_env(provider: &str) -> Option<&'static str> {
    Some(match provider {
        "openai" | "openai-compatible" => "OPENAI_API_KEY",
        "anthropic" => "ANTHROPIC_API_KEY",
        "google" | "gemini" => "GEMINI_API_KEY",
        "google-vertex" => "GOOGLE_CLOUD_API_KEY",
        "azure-openai-responses" => "AZURE_OPENAI_API_KEY",
        "amazon-bedrock" => "AWS_BEARER_TOKEN_BEDROCK",
        "ant-ling" => "ANT_LING_API_KEY",
        "nvidia" => "NVIDIA_API_KEY",
        "deepseek" => "DEEPSEEK_API_KEY",
        "groq" => "GROQ_API_KEY",
        "cerebras" => "CEREBRAS_API_KEY",
        "xai" => "XAI_API_KEY",
        "openrouter" => "OPENROUTER_API_KEY",
        "vercel-ai-gateway" => "AI_GATEWAY_API_KEY",
        "zai" => "ZAI_API_KEY",
        "zai-coding-cn" => "ZAI_CODING_CN_API_KEY",
        "mistral" => "MISTRAL_API_KEY",
        "minimax" => "MINIMAX_API_KEY",
        "minimax-cn" => "MINIMAX_CN_API_KEY",
        "moonshotai" | "moonshotai-cn" => "MOONSHOT_API_KEY",
        "huggingface" => "HF_TOKEN",
        "fireworks" => "FIREWORKS_API_KEY",
        "together" => "TOGETHER_API_KEY",
        "baseten" => "BASETEN_API_KEY",
        "opencode" | "opencode-go" => "OPENCODE_API_KEY",
        "kimi-coding" => "KIMI_API_KEY",
        "github-copilot" => "GITHUB_COPILOT_TOKEN",
        "cloudflare-workers-ai" | "cloudflare-ai-gateway" => "CLOUDFLARE_API_TOKEN",
        "pi-messages" => "AGENT_CORE_PI_MESSAGES_API_KEY",
        "qwen-token-plan" | "qwen-token-plan-individual" => "QWEN_TOKEN_PLAN_API_KEY",
        "qwen-token-plan-cn" => "QWEN_TOKEN_PLAN_CN_API_KEY",
        "xiaomi" => "XIAOMI_API_KEY",
        "xiaomi-token-plan-cn" => "XIAOMI_TOKEN_PLAN_CN_API_KEY",
        "xiaomi-token-plan-ams" => "XIAOMI_TOKEN_PLAN_AMS_API_KEY",
        "xiaomi-token-plan-sgp" => "XIAOMI_TOKEN_PLAN_SGP_API_KEY",
        _ => return None,
    })
}

pub fn resolve_api_key(provider: &str) -> String {
    std::env::var("AGENT_CORE_API_KEY")
        .ok()
        .filter(|key| !key.trim().is_empty())
        .or_else(|| api_key_env(provider).and_then(|name| std::env::var(name).ok()))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn keeps_provider_specific_keys_and_never_guesses_oauth_as_api_keys() {
        assert_eq!(api_key_env("deepseek"), Some("DEEPSEEK_API_KEY"));
        assert_eq!(api_key_env("moonshotai-cn"), Some("MOONSHOT_API_KEY"));
        assert_eq!(api_key_env("anthropic"), Some("ANTHROPIC_API_KEY"));
        for provider in ["openai-codex", "unknown"] {
            assert_eq!(api_key_env(provider), None);
        }
        assert_eq!(api_key_env("github-copilot"), Some("GITHUB_COPILOT_TOKEN"));
        assert_eq!(api_key_env("google-vertex"), Some("GOOGLE_CLOUD_API_KEY"));
        assert_eq!(
            api_key_env("amazon-bedrock"),
            Some("AWS_BEARER_TOKEN_BEDROCK")
        );
        assert_eq!(
            api_key_env("azure-openai-responses"),
            Some("AZURE_OPENAI_API_KEY")
        );
    }
}
