# Models and Providers

This page covers built-in providers and authentication, and custom models and providers via `~/.agent-core/agent/models.json`.

## Table of Contents

- [Built-in Providers and Auth](#built-in-providers-and-auth)
  - [Subscriptions](#subscriptions)
  - [API Keys](#api-keys)
  - [Auth File](#auth-file)
  - [Credential Key Resolution](#credential-key-resolution)
  - [Cloud Providers](#cloud-providers)
  - [Resolution Order](#resolution-order)
- [Custom Models](#custom-models)
  - [Minimal Example](#minimal-example)
  - [Full Example](#full-example)
  - [Supported APIs](#supported-apis)
  - [Provider Configuration](#provider-configuration)
  - [Model Configuration](#model-configuration)
  - [Overriding Built-in Providers](#overriding-built-in-providers)
  - [Per-model Overrides](#per-model-overrides)
  - [Anthropic Messages Compatibility](#anthropic-messages-compatibility)
  - [OpenAI Compatibility](#openai-compatibility)

## Built-in Providers and Auth

Agent Core supports subscription-based providers via OAuth and API key providers via environment variables or auth file. Built-in catalogs ship with agent-core; dynamic providers registered by extensions may refresh their catalogs and cache them in `~/.agent-core/agent/models-store.json` for offline use.

### Subscriptions

Use `/login` in interactive mode, then select a provider:

- ChatGPT Plus/Pro (Codex)
- Claude Pro/Max
- GitHub Copilot
- xAI (Grok/X subscription)
- OpenRouter (OAuth-minted API key billed from OpenRouter credits)

Use `/logout` to clear credentials. Tokens are stored in `~/.agent-core/agent/auth.json` and auto-refresh when expired. OpenRouter instead mints a user-controlled API key that does not expire automatically.

#### OpenAI Codex

- Requires ChatGPT Plus or Pro subscription
- Officially endorsed by OpenAI: [Codex for OSS](https://developers.openai.com/community/codex-for-oss)

#### Claude Pro/Max

Anthropic subscription auth is active for Claude Pro/Max accounts. Third-party harness usage draws from [extra usage](https://claude.ai/settings/usage) and is billed per token, not against Claude plan limits.

#### GitHub Copilot

- Press Enter for github.com, or enter your GitHub Enterprise Server domain
- If you get "model not supported", enable it in VS Code: Copilot Chat → model selector → select model → "Enable"

#### xAI (Grok/X subscription)

- Run `/login xai`, then select **Use a subscription**
- `XAI_API_KEY` remains available through **Use an API key**

#### OpenRouter

- Run `/login openrouter`, then select **Sign in with OpenRouter** to open the OpenRouter PKCE authorization flow
- The authorization creates a user-controlled OpenRouter API key billed from your OpenRouter credits
- On remote/headless machines (e.g. over SSH) the browser cannot reach the loopback callback; paste the final redirect URL (or the authorization code) into the login prompt instead
- `OPENROUTER_API_KEY` remains available through **Use an API key**

### API Keys

Use `/login` in interactive mode and select a provider to store an API key in `auth.json`, or set credentials via environment variable:

```bash
export ANTHROPIC_API_KEY=sk-ant-...
agent-core
```

| Provider | Environment Variable | `auth.json` key |
|----------|----------------------|------------------|
| Anthropic | `ANTHROPIC_API_KEY` | `anthropic` |
| Ant Ling | `ANT_LING_API_KEY` | `ant-ling` |
| Azure OpenAI Responses | `AZURE_OPENAI_API_KEY` | `azure-openai-responses` |
| OpenAI | `OPENAI_API_KEY` | `openai` |
| DeepSeek | `DEEPSEEK_API_KEY` | `deepseek` |
| NVIDIA NIM | `NVIDIA_API_KEY` | `nvidia` |
| Google Gemini | `GEMINI_API_KEY` | `google` |
| Amazon Bedrock | `AWS_BEARER_TOKEN_BEDROCK` | `amazon-bedrock` |
| Mistral | `MISTRAL_API_KEY` | `mistral` |
| Groq | `GROQ_API_KEY` | `groq` |
| Cerebras | `CEREBRAS_API_KEY` | `cerebras` |
| Cloudflare AI Gateway | `CLOUDFLARE_API_KEY` (+ `CLOUDFLARE_ACCOUNT_ID`, `CLOUDFLARE_GATEWAY_ID`) | `cloudflare-ai-gateway` |
| Cloudflare Workers AI | `CLOUDFLARE_API_KEY` (+ `CLOUDFLARE_ACCOUNT_ID`) | `cloudflare-workers-ai` |
| xAI | `XAI_API_KEY` | `xai` |
| OpenRouter | `OPENROUTER_API_KEY` | `openrouter` |
| Vercel AI Gateway | `AI_GATEWAY_API_KEY` | `vercel-ai-gateway` |
| ZAI Coding Plan (Global) | `ZAI_API_KEY` | `zai` |
| ZAI Coding Plan (China) | `ZAI_CODING_CN_API_KEY` | `zai-coding-cn` |
| OpenCode Zen | `OPENCODE_API_KEY` | `opencode` |
| OpenCode Go | `OPENCODE_API_KEY` | `opencode-go` |
| Hugging Face | `HF_TOKEN` | `huggingface` |
| Fireworks | `FIREWORKS_API_KEY` | `fireworks` |
| Together AI | `TOGETHER_API_KEY` | `together` |
| Baseten | `BASETEN_API_KEY` | `baseten` |
| Kimi For Coding | `KIMI_API_KEY` | `kimi-coding` |
| MiniMax | `MINIMAX_API_KEY` | `minimax` |
| MiniMax (China) | `MINIMAX_CN_API_KEY` | `minimax-cn` |
| Qwen Token Plan (existing catalog) | `QWEN_TOKEN_PLAN_API_KEY` | `qwen-token-plan` |
| Qwen Token Plan (Individual) | `QWEN_TOKEN_PLAN_API_KEY` | `qwen-token-plan-individual` |
| Qwen Token Plan (China) | `QWEN_TOKEN_PLAN_CN_API_KEY` | `qwen-token-plan-cn` |
| Xiaomi MiMo | `XIAOMI_API_KEY` | `xiaomi` |
| Xiaomi MiMo Token Plan (China) | `XIAOMI_TOKEN_PLAN_CN_API_KEY` | `xiaomi-token-plan-cn` |
| Xiaomi MiMo Token Plan (Amsterdam) | `XIAOMI_TOKEN_PLAN_AMS_API_KEY` | `xiaomi-token-plan-ams` |
| Xiaomi MiMo Token Plan (Singapore) | `XIAOMI_TOKEN_PLAN_SGP_API_KEY` | `xiaomi-token-plan-sgp` |

Reference for environment variables and `auth.json` keys: [`const envMap`](https://github.com/LiuXD1011/agent-core/blob/main/packages/ai/src/env-api-keys.ts) in [`packages/ai/src/env-api-keys.ts`](https://github.com/LiuXD1011/agent-core/blob/main/packages/ai/src/env-api-keys.ts).

#### Auth File

Store credentials in `~/.agent-core/agent/auth.json`:

```json
{
  "anthropic": { "type": "api_key", "key": "sk-ant-..." },
  "ant-ling": { "type": "api_key", "key": "..." },
  "openai": { "type": "api_key", "key": "sk-..." },
  "deepseek": { "type": "api_key", "key": "sk-..." },
  "nvidia": { "type": "api_key", "key": "nvapi-..." },
  "google": { "type": "api_key", "key": "..." },
  "opencode": { "type": "api_key", "key": "..." },
  "opencode-go": { "type": "api_key", "key": "..." },
  "together": { "type": "api_key", "key": "..." },
  "qwen-token-plan":  { "type": "api_key", "key": "sk-sp-..." },
  "qwen-token-plan-individual": { "type": "api_key", "key": "sk-sp-..." },
  "qwen-token-plan-cn": { "type": "api_key", "key": "sk-sp-..." },
  "xiaomi": { "type": "api_key", "key": "..." },
  "xiaomi-token-plan-cn":  { "type": "api_key", "key": "..." },
  "xiaomi-token-plan-ams": { "type": "api_key", "key": "..." },
  "xiaomi-token-plan-sgp": { "type": "api_key", "key": "..." }
}
```

`qwen-token-plan-individual` uses the same international endpoint and `QWEN_TOKEN_PLAN_API_KEY` as
`qwen-token-plan`, but limits the picker to the models documented for Individual subscriptions. The existing
provider keeps its broader catalog for backward compatibility. When using `auth.json`, store the
credential under the provider you select; an environment variable is shared by both international providers.

The file is created with `0600` permissions (user read/write only). Auth file credentials take priority over environment variables.

API key credentials can also include provider-scoped environment values. These values are used before process environment variables when resolving the credential key, provider/model headers, and provider configuration such as Cloudflare account IDs, Azure OpenAI settings, Vertex project/location, Bedrock settings, `AGENT_CORE_CACHE_RETENTION`, and `HTTP_PROXY`/`HTTPS_PROXY`.

```json
{
  "cloudflare-ai-gateway": {
    "type": "api_key",
    "key": "$CLOUDFLARE_API_KEY",
    "env": {
      "CLOUDFLARE_API_KEY": "...",
      "CLOUDFLARE_ACCOUNT_ID": "account-id",
      "CLOUDFLARE_GATEWAY_ID": "gateway-id"
    }
  }
}
```

Use this when agent-core should use different provider settings than the project shell environment.

### Credential Key Resolution

The `key` field supports command execution, environment interpolation, and literals:

- **Shell command:** `"!command"` at the start executes the whole value as a command and uses stdout (cached for process lifetime)
  ```json
  { "type": "api_key", "key": "!security find-generic-password -ws 'anthropic'" }
  { "type": "api_key", "key": "!op read 'op://vault/item/credential'" }
  ```
- **Environment interpolation:** `"$ENV_VAR"` or `"${ENV_VAR}"` uses the value of the named variable. Interpolation works inside larger literals.
  ```json
  { "type": "api_key", "key": "$MY_ANTHROPIC_KEY" }
  { "type": "api_key", "key": "${KEY_PREFIX}_${KEY_SUFFIX}" }
  ```
  `$FOO_BAR` is the variable `FOO_BAR`; use `${FOO}_BAR` when `BAR` is literal text. Missing environment variables make the value unresolved.
- **Escapes:** `"$$"` emits a literal `"$"`; `"$!"` emits a literal `"!"` without triggering command execution.
  ```json
  { "type": "api_key", "key": "$$literal-dollar-prefix" }
  { "type": "api_key", "key": "$!literal-bang-prefix" }
  ```
- **Literal value:** Used directly. Plain uppercase strings such as `MY_API_KEY` are literals; use `$MY_API_KEY` for environment variables.
  ```json
  { "type": "api_key", "key": "sk-ant-..." }
  { "type": "api_key", "key": "public" }
  ```

OAuth credentials are also stored here after `/login` and managed automatically.

### Cloud Providers

#### Azure OpenAI

```bash
export AZURE_OPENAI_API_KEY=...
export AZURE_OPENAI_BASE_URL=https://your-resource.ai.azure.com
# also supported: https://your-resource.cognitiveservices.azure.com
# also supported: https://your-resource.openai.azure.com
# root endpoints are auto-normalized to /openai/v1
# or use resource name instead of base URL
export AZURE_OPENAI_RESOURCE_NAME=your-resource

# Optional
export AZURE_OPENAI_API_VERSION=2024-02-01
export AZURE_OPENAI_DEPLOYMENT_NAME_MAP=gpt-4=my-gpt4,gpt-4o=my-gpt4o
```

#### Amazon Bedrock

Use `/login amazon-bedrock` to store a Bedrock API key, or configure one of the ambient AWS credential sources below:

```bash
# Option 1: AWS Profile
export AWS_PROFILE=your-profile

# Option 2: IAM Keys
export AWS_ACCESS_KEY_ID=AKIA...
export AWS_SECRET_ACCESS_KEY=...

# Option 3: Bearer Token
export AWS_BEARER_TOKEN_BEDROCK=...

# Optional region (defaults to us-east-1)
export AWS_REGION=us-west-2
```

Also supports ECS task roles (`AWS_CONTAINER_CREDENTIALS_*`) and IRSA (`AWS_WEB_IDENTITY_TOKEN_FILE`).

```bash
agent-core --provider amazon-bedrock --model us.anthropic.claude-sonnet-4-20250514-v1:0
```

Prompt caching is enabled automatically for Claude models whose ID contains a recognizable model name (base models and system-defined inference profiles). For application inference profiles (whose ARNs don't contain the model name), set `AWS_BEDROCK_FORCE_CACHE=1` to enable cache points:

```bash
export AWS_BEDROCK_FORCE_CACHE=1
agent-core --provider amazon-bedrock --model arn:aws:bedrock:us-east-1:123456789012:application-inference-profile/abc123
```

If you are connecting to a Bedrock API proxy, the following environment variables can be used:

```bash
# Set the URL for the Bedrock proxy (standard AWS SDK env var)
export AWS_ENDPOINT_URL_BEDROCK_RUNTIME=https://my.corp.proxy/bedrock

# Set if your proxy does not require authentication
export AWS_BEDROCK_SKIP_AUTH=1

# Set if your proxy only supports HTTP/1.1
export AWS_BEDROCK_FORCE_HTTP1=1
```

#### Cloudflare AI Gateway

`CLOUDFLARE_API_KEY` can be set via `/login`. The account ID and gateway slug can be set as environment variables or in the API key credential's `env` object in `auth.json`.

```bash
export CLOUDFLARE_API_KEY=...           # or use /login
export CLOUDFLARE_ACCOUNT_ID=...
export CLOUDFLARE_GATEWAY_ID=...        # create at dash.cloudflare.com → AI → AI Gateway
agent-core --provider cloudflare-ai-gateway --model "claude-sonnet-4-5"
```

Routes to OpenAI, Anthropic, and Workers AI through Cloudflare AI Gateway. Workers AI uses the Unified API (`/compat`) and prefixed model IDs (`workers-ai/@cf/...`). OpenAI uses the OpenAI passthrough route (`/openai`) with native OpenAI model IDs such as `gpt-5.1`. Anthropic uses the Anthropic passthrough route (`/anthropic`) with native Anthropic model IDs such as `claude-sonnet-4-5`.

AI Gateway authentication uses `CLOUDFLARE_API_KEY` as `cf-aig-authorization`. Upstream authentication can be one of:

| Mode | Request auth | Upstream auth |
|------|--------------|---------------|
| Workers AI | Cloudflare token only | Cloudflare-native |
| Unified billing | Cloudflare token only | Cloudflare handles upstream auth and deducts credits |
| Stored BYOK | Cloudflare token only | Cloudflare injects provider keys stored in the AI Gateway dashboard |
| Inline BYOK | Cloudflare token plus upstream `Authorization` header | The request supplies the upstream provider key |

For normal agent-core usage, prefer unified billing or stored BYOK. Inline BYOK requires configuring an additional upstream `Authorization` header for the Cloudflare AI Gateway provider, for example via a `models.json` provider/model override.

#### Cloudflare Workers AI

`CLOUDFLARE_API_KEY` can be set via `/login`. `CLOUDFLARE_ACCOUNT_ID` can be set as an environment variable or in the API key credential's `env` object in `auth.json`.

```bash
export CLOUDFLARE_API_KEY=...           # or use /login
export CLOUDFLARE_ACCOUNT_ID=...
agent-core --provider cloudflare-workers-ai --model "@cf/moonshotai/kimi-k2.6"
```

Agent Core automatically sets `x-session-affinity` for [prefix caching](https://developers.cloudflare.com/workers-ai/features/prompt-caching/) discounts.

#### Google Vertex AI

Uses Application Default Credentials:

```bash
gcloud auth application-default login
export GOOGLE_CLOUD_PROJECT=your-project
export GOOGLE_CLOUD_LOCATION=us-central1
```

Or set `GOOGLE_APPLICATION_CREDENTIALS` to a service account key file.

### Resolution Order

When resolving credentials for a provider:

1. CLI `--api-key` flag
2. `auth.json` entry (API key or OAuth token)
3. Environment variable
4. Custom provider keys from `models.json`

## Custom Models

Add custom providers and models (Ollama, vLLM, LM Studio, proxies) via `~/.agent-core/agent/models.json`. For providers that need custom API implementations or OAuth flows, create an extension instead; see [custom-provider.md](custom-provider.md).

## Minimal Example

For local models (Ollama, LM Studio, vLLM), only `id` is required per model:

```json
{
  "providers": {
    "ollama": {
      "baseUrl": "http://localhost:11434/v1",
      "api": "openai-completions",
      "apiKey": "ollama",
      "models": [
        { "id": "llama3.1:8b" },
        { "id": "qwen2.5-coder:7b" }
      ]
    }
  }
}
```

The `apiKey` value is a placeholder because Ollama ignores it. agent-core still treats models as requiring auth before they appear in `/model`, so keyless local servers should keep a dummy value, save a key for that provider with `/login`, or pass `--api-key` when selecting the model.

Some OpenAI-compatible servers do not understand the `developer` role used for reasoning-capable models. For those providers, set `compat.supportsDeveloperRole` to `false` so agent-core sends the system prompt as a `system` message instead. If the server also does not support `reasoning_effort`, set `compat.supportsReasoningEffort` to `false` too.

You can set `compat` at the provider level to apply to all models, or at the model level to override a specific model. This commonly applies to Ollama, vLLM, SGLang, and similar OpenAI-compatible servers.

```json
{
  "providers": {
    "ollama": {
      "baseUrl": "http://localhost:11434/v1",
      "api": "openai-completions",
      "apiKey": "ollama",
      "compat": {
        "supportsDeveloperRole": false,
        "supportsReasoningEffort": false
      },
      "models": [
        {
          "id": "gpt-oss:20b",
          "reasoning": true
        }
      ]
    }
  }
}
```

## Full Example

Override defaults when you need specific values:

```json
{
  "providers": {
    "ollama": {
      "baseUrl": "http://localhost:11434/v1",
      "api": "openai-completions",
      "apiKey": "ollama",
      "models": [
        {
          "id": "llama3.1:8b",
          "name": "Llama 3.1 8B (Local)",
          "reasoning": false,
          "input": ["text"],
          "contextWindow": 128000,
          "maxTokens": 32000,
          "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 }
        }
      ]
    }
  }
}
```

The file reloads each time you open `/model`. Edit during session; no restart needed.

## Google AI Studio Example

Use `google-generative-ai` with a `baseUrl` to add models from Google AI Studio, including custom Gemma 4 entries:

```json
{
  "providers": {
    "my-google": {
      "baseUrl": "https://generativelanguage.googleapis.com/v1beta",
      "api": "google-generative-ai",
      "apiKey": "$GEMINI_API_KEY",
      "models": [
        {
          "id": "gemma-4-31b-it",
          "name": "Gemma 4 31B",
          "input": ["text", "image"],
          "contextWindow": 262144,
          "reasoning": true
        }
      ]
    }
  }
}
```

The `baseUrl` is required when adding custom models to the `google-generative-ai` API type.

## Supported APIs

| API | Description |
|-----|-------------|
| `openai-completions` | OpenAI Chat Completions (most compatible) |
| `openai-responses` | OpenAI Responses API |
| `anthropic-messages` | Anthropic Messages API |
| `google-generative-ai` | Google Generative AI |

Set `api` at provider level (default for all models) or model level (override per model).

## Provider Configuration

| Field | Description |
|-------|-------------|
| `baseUrl` | API endpoint URL |
| `api` | API type (see above) |
| `apiKey` | Optional API key config (see value resolution below). Omit it when auth is provided by `/login`/`auth.json` or CLI `--api-key`. |
| `headers` | Custom headers (see value resolution below) |
| `authHeader` | Set `true` to add `Authorization: Bearer <apiKey>` automatically |
| `models` | Array of model configurations |
| `modelOverrides` | Per-model overrides for built-in or extension-registered models on this provider |

For providers with `models`, non-built-in provider configs need `baseUrl` and an `api` value at either provider or model level. `apiKey` is not required to load the file: models become available when auth is configured through `/login`/`auth.json`, CLI `--api-key`, or provider `apiKey`. If no auth is configured, the models load but stay unavailable in `/model` and `--list-models`.

### Value Resolution

The `apiKey` and `headers` fields support command execution, environment interpolation, and literals:

- **Shell command:** `"!command"` at the start executes the whole value as a command and uses stdout
  ```json
  "apiKey": "!security find-generic-password -ws 'anthropic'"
  "apiKey": "!op read 'op://vault/item/credential'"
  ```
- **Environment interpolation:** `"$ENV_VAR"` or `"${ENV_VAR}"` uses the value of the named variable. Interpolation works inside larger literals.
  ```json
  "apiKey": "$MY_API_KEY"
  "apiKey": "${KEY_PREFIX}_${KEY_SUFFIX}"
  ```
  `$FOO_BAR` is the variable `FOO_BAR`; use `${FOO}_BAR` when `BAR` is literal text. Missing environment variables make the value unresolved.
- **Escapes:** `"$$"` emits a literal `"$"`; `"$!"` emits a literal `"!"` without triggering command execution.
  ```json
  "apiKey": "$$literal-dollar-prefix"
  "apiKey": "$!literal-bang-prefix"
  ```
- **Literal value:** Used directly. Plain uppercase strings such as `MY_API_KEY` are literals; use `$MY_API_KEY` for environment variables.
  ```json
  "apiKey": "sk-..."
  ```

For `models.json`, shell commands are resolved at request time. agent-core intentionally does not apply built-in TTL, stale reuse, or recovery logic for arbitrary commands. Different commands need different caching and failure strategies, and pi cannot infer the right one.

If your command is slow, expensive, rate-limited, or should keep using a previous value on transient failures, wrap it in your own script or command that implements the caching or TTL behavior you want.

`/model` availability checks use configured auth presence and do not execute shell commands.

### Custom Headers

```json
{
  "providers": {
    "custom-proxy": {
      "baseUrl": "https://proxy.example.com/v1",
      "apiKey": "$MY_API_KEY",
      "api": "anthropic-messages",
      "headers": {
        "x-portkey-api-key": "$PORTKEY_API_KEY",
        "x-secret": "!op read 'op://vault/item/secret'"
      },
      "models": [...]
    }
  }
}
```

## Model Configuration

| Field | Required | Default | Description |
|-------|----------|---------|-------------|
| `id` | Yes | — | Model identifier (passed to the API) |
| `name` | No | `id` | Human-readable model label. Used for matching (`--model` patterns) and shown as secondary model detail text. |
| `api` | No | provider's `api` | Override provider's API for this model |
| `reasoning` | No | `false` | Supports extended thinking |
| `thinkingLevelMap` | No | omitted | Maps agent-core thinking levels to provider values and marks unsupported levels (see below) |
| `input` | No | `["text"]` | Input types: `["text"]` or `["text", "image"]` |
| `contextWindow` | No | `128000` | Context window size in tokens |
| `maxTokens` | No | `16384` | Maximum output tokens |
| `samplingParams` | No | omitted | Sampling parameters merged verbatim into every request body (see below) |
| `cost` | No | all zeros | Per-million-token rates with optional request-wide input pricing tiers |
| `compat` | No | provider `compat` | Provider compatibility overrides. Merged with provider-level `compat` when both are set. |

A cost tier supplies a complete alternate rate set and applies to the full request when total input usage (`input + cacheRead + cacheWrite`) exceeds `inputTokensAbove`. When multiple tiers match, the highest threshold wins.

```json
{
  "cost": {
    "input": 5,
    "output": 30,
    "cacheRead": 0.5,
    "cacheWrite": 6.25,
    "tiers": [
      {
        "inputTokensAbove": 272000,
        "input": 10,
        "output": 45,
        "cacheRead": 1,
        "cacheWrite": 12.5
      }
    ]
  }
}
```

Current behavior:
- `/model`, `--list-models`, and the interactive footer display entries by model `id`.
- The configured `name` is used for model matching and secondary model detail text. It does not replace the footer/status-bar model id.

Custom model entries are static: there is no online catalog that keeps them current. Adding a new model means configuring `api`, `baseUrl`, an API key, the model ID, and any needed capability parameters (`reasoning`, `input`, `compat`) yourself. Prices and context windows must be updated manually, and the resulting usage/cost numbers are local estimates, not your vendor's bill.

### Sampling Parameters

`samplingParams` is a free-form object merged verbatim into every request body for the model, after the fields agent-core sets itself, so its keys win. Use it to send sampling parameters agent-core does not model — including server-specific ones like llama.cpp's `min_p` or vLLM's `top_k`:

```json
{
  "id": "deepseek-flash",
  "samplingParams": {
    "temperature": 1.0,
    "top_p": 0.95,
    "top_k": 0,
    "min_p": 0.0
  }
}
```

Only OpenAI-compatible APIs apply it (`openai-completions`, `openai-responses`, `azure-openai-responses`); other APIs ignore it. Keys override agent-core's named request fields (for example a `temperature` key here beats the request-level temperature), so prefer it as the single source of sampling truth for a model. In `modelOverrides`, `samplingParams` merges per key with the base model's value.

A constant thinking-token cap can go here too, but it will not follow `thinkingBudgets` or leave room for the answer. Prefer `compat.thinkingTokenBudgetField` (or the `supportsThinkingTokenBudget` alias) for that.

### Thinking Level Map

Use `thinkingLevelMap` on a model to describe model-specific thinking controls. Keys are agent-core thinking levels: `off`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max`. Maps may contain holes; for example, a model can expose `high` and `max` without exposing `xhigh`.

Values are tristate:

| Value | Meaning |
|-------|---------|
| omitted | Standard levels through `high` use the provider's default mapping; extended `xhigh` and `max` levels are unsupported |
| string | Level is supported and this value is sent to the provider |
| `null` | Level is unsupported and hidden/skipped/clamped away |

Example for a model that only supports off, high, and max reasoning:

```json
{
  "id": "deepseek-v4-pro",
  "reasoning": true,
  "thinkingLevelMap": {
    "minimal": null,
    "low": null,
    "medium": null,
    "high": "high",
    "xhigh": null,
    "max": "max"
  }
}
```

Example for a model where thinking cannot be disabled:

```json
{
  "id": "always-thinking-model",
  "reasoning": true,
  "thinkingLevelMap": {
    "off": null
  }
}
```

Migration: older configs that used `compat.reasoningEffortMap` should move that mapping to model-level `thinkingLevelMap`. Use `null` for levels that should not appear in the UI.

## Overriding Built-in Providers

Route a built-in provider through a proxy without redefining models:

```json
{
  "providers": {
    "anthropic": {
      "baseUrl": "https://my-proxy.example.com/v1"
    }
  }
}
```

All built-in Anthropic models remain available. Existing OAuth or API key auth continues to work.

To merge custom models into a built-in provider, include the `models` array:

```json
{
  "providers": {
    "anthropic": {
      "baseUrl": "https://my-proxy.example.com/v1",
      "apiKey": "$ANTHROPIC_API_KEY",
      "api": "anthropic-messages",
      "models": [...]
    }
  }
}
```

Merge semantics:
- Built-in models are kept.
- Custom models are upserted by `id` within the provider.
- If a custom model `id` matches a built-in model `id`, the custom model replaces that built-in model.
- If a custom model `id` is new, it is added alongside built-in models.

## Per-model Overrides

Use `modelOverrides` to customize built-in models and matching extension-registered models without replacing the provider's full model list.

```json
{
  "providers": {
    "openrouter": {
      "modelOverrides": {
        "anthropic/claude-sonnet-4": {
          "name": "Claude Sonnet 4 (Bedrock Route)",
          "compat": {
            "openRouterRouting": {
              "only": ["amazon-bedrock"]
            }
          }
        }
      }
    }
  }
}
```

`modelOverrides` supports these fields per model: `name`, `reasoning`, `thinkingLevelMap`, `input`, `cost` (partial), `contextWindow`, `maxTokens`, `samplingParams` (merged per key), `headers`, `compat`.

Direct OpenAI GPT-5.6 Sol, Terra, and Luna default to a `272000` context window so requests remain within OpenAI's short-context pricing tier. To opt into OpenAI's 1.05M context window, increase it for each model you use:

```json
{
  "providers": {
    "openai": {
      "modelOverrides": {
        "gpt-5.6-sol": {
          "contextWindow": 1050000
        }
      }
    }
  }
}
```

The override preserves the built-in pricing metadata. Requests with more than 272K total input tokens use GPT-5.6's long-context rates for the entire request. Apply the same override to `gpt-5.6-terra` or `gpt-5.6-luna` when needed.

Behavior notes:
- `modelOverrides` are applied to built-in provider models and matching extension-registered provider models.
- Unknown model IDs are ignored.
- You can combine provider-level `baseUrl`/`headers` with `modelOverrides`.
- Overriding `name` changes model matching and secondary detail text only; the footer and primary model lists continue to show the model `id`.
- If `models` is also defined for a provider, custom models are merged after built-in overrides. A custom model with the same `id` replaces the overridden built-in model entry.

## Anthropic Messages Compatibility

For providers or proxies using `api: "anthropic-messages"`, use `compat` to control Anthropic-specific request compatibility.

By default agent-core sends per-tool `eager_input_streaming: true`. If a proxy or Anthropic-compatible backend rejects that field, set `supportsEagerToolInputStreaming` to `false`. Agent Core will omit `tools[].eager_input_streaming` and send the legacy `fine-grained-tool-streaming-2025-05-14` beta header for tool-enabled requests instead.

Some Anthropic models require adaptive thinking (`thinking.type: "adaptive"` plus `output_config.effort`) instead of the legacy budget-based thinking payload. Built-in models set this automatically. For custom providers or aliases that route to those models, set `forceAdaptiveThinking` to `true`.

Claude models with per-turn effort support use `supportsMidConvoEffort`. Agent Core then persists each response's provider effort, reconstructs effort-only system messages on later requests, and sends thinking binding controls with `prefix_mismatch_behavior: "drop_block"` to avoid stale signed-thinking prefixes causing persistent 400 responses. Set this only for the exact supported Claude model on a faithful Anthropic Messages transport; do not enable it for APIs that merely imitate the Messages shape.

Some Anthropic-compatible providers emit thinking blocks with empty signatures and still expect them on replay. Set `allowEmptySignature` to `true` only for those providers; real Anthropic rejects empty thinking signatures.

Built-in Anthropic models enable `supportsStrictTools` in their model metadata. Custom Anthropic-compatible models must set it to `true` when their endpoint accepts strict JSON-schema tool definitions.

```json
{
  "providers": {
    "anthropic-proxy": {
      "baseUrl": "https://proxy.example.com",
      "api": "anthropic-messages",
      "apiKey": "$ANTHROPIC_PROXY_KEY",
      "compat": {
        "supportsEagerToolInputStreaming": false,
        "supportsLongCacheRetention": true,
        "forceAdaptiveThinking": true,
        "allowEmptySignature": true
      },
      "models": [
        {
          "id": "claude-opus-4-7",
          "reasoning": true,
          "input": ["text", "image"]
        }
      ]
    }
  }
}
```

| Field | Description |
|-------|-------------|
| `supportsEagerToolInputStreaming` | Whether the provider accepts per-tool `eager_input_streaming`. Default: `true`. Set to `false` to omit that field and use the legacy fine-grained tool streaming beta header on tool-enabled requests. |
| `supportsLongCacheRetention` | Whether the provider accepts Anthropic long cache retention (`cache_control.ttl: "1h"`) when cache retention is `long`. Default: `true`. |
| `sendSessionAffinityHeaders` | Whether to send `x-session-affinity` from the session id when caching is enabled. Default: auto-detected for known providers. |
| `supportsCacheControlOnTools` | Whether the provider accepts Anthropic-style `cache_control` markers on tool definitions. Default: `true`. |
| `forceAdaptiveThinking` | Whether to send adaptive thinking (`thinking.type: "adaptive"` plus `output_config.effort`) for this model. Built-in adaptive models set this automatically. Default: `false`. |
| `supportsMidConvoEffort` | Whether the exact Claude model transport supports per-turn effort system messages and thinking binding controls. Agent Core persists native effort levels and always sends `drop_block` when enabled. Default: `false`. |
| `allowEmptySignature` | Whether to replay empty thinking signatures as `signature: ""` instead of converting thinking to text. Default: `false`. |
| `supportsStrictTools` | Whether the provider accepts strict JSON-schema tool definitions. Default: `false`; built-in Anthropic models enable it in generated metadata. |

## OpenAI Compatibility

For providers with partial OpenAI compatibility, use the `compat` field.

- Provider-level `compat` applies defaults to all models under that provider.
- Model-level `compat` overrides provider-level values for that model.

```json
{
  "providers": {
    "local-llm": {
      "baseUrl": "http://localhost:8080/v1",
      "api": "openai-completions",
      "compat": {
        "supportsUsageInStreaming": false,
        "maxTokensField": "max_tokens"
      },
      "models": [...]
    }
  }
}
```

| Field | Description |
|-------|-------------|
| `supportsStore` | Provider supports `store` field |
| `supportsDeveloperRole` | Use `developer` vs `system` role |
| `supportsReasoningEffort` | Support for `reasoning_effort` parameter |
| `supportsUsageInStreaming` | Supports `stream_options: { include_usage: true }` (default: `true`) |
| `supportsFinishReason` | Whether streamed responses include `finish_reason`. When `false`, agent-core infers `stop` or `toolUse` when the stream ends. Default: `true`. |
| `maxTokensField` | Use `max_completion_tokens` or `max_tokens` |
| `requiresToolResultName` | Include `name` on tool result messages |
| `requiresAssistantAfterToolResult` | Insert an assistant message before a user message after tool results |
| `requiresThinkingAsText` | Convert thinking blocks to plain text |
| `requiresReasoningContentOnAssistantMessages` | Include empty `reasoning_content` on all replayed assistant messages when reasoning is enabled |
| `thinkingFormat` | Use `reasoning_effort`, `openrouter`, `deepseek`, `together`, `baseten`, `zai`, `qwen`, `chat-template`, or `qwen-chat-template` thinking parameters |
| `chatTemplateKwargs` | `chat_template_kwargs` values for `thinkingFormat: "chat-template"`; use `{ "$var": "thinking.enabled" }`, `{ "$var": "thinking.effort" }`, or `{ "$var": "thinking.budget" }` for agent-core-controlled thinking values |
| `chatTemplateArgs` | `chat_template_args` values for `thinkingFormat: "baseten"`; use `{ "$var": "thinking.enabled" }`, `{ "$var": "thinking.effort" }`, or `{ "$var": "thinking.budget" }` for agent-core-controlled thinking values |
| `thinkingTokenBudgetField` | Top-level request field used to cap reasoning tokens from `thinkingBudgets`, clamped so at least 1024 tokens remain for the answer. `"thinking_token_budget"` (vLLM), `"thinking_budget"` (Qwen/DashScope/SGLang), `"thinking_budget_tokens"` (llama.cpp). Off by default; not set on the generated catalog. |
| `supportsThinkingTokenBudget` | Alias for `thinkingTokenBudgetField: "thinking_token_budget"` (vLLM). Prefer `thinkingTokenBudgetField`. Default: `false`. |
| `cacheControlFormat` | Use Anthropic-style `cache_control` markers on the system prompt, last tool definition, and last user, assistant, or tool-result text content. Currently only `anthropic` is supported. |
| `sendSessionAffinityHeaders` | For `openai-completions`, send session-affinity headers from the session id when caching is enabled. Default: `false`. |
| `sessionAffinityFormat` | For `openai-completions` and `openai-responses`, the session-affinity header format: `openai` sends `session_id`/`x-client-request-id` (completions also `x-session-affinity`), `openai-nosession` omits the underscore-containing `session_id` header, `openrouter` sends `x-session-id`. Does not affect the `prompt_cache_key` body param. Default: auto-detected. |
| `supportsStrictMode` | Whether the provider accepts strict JSON-schema function tool definitions. Defaults depend on the API; built-in OpenAI models carry explicit capability metadata. |
| `supportsOpenAIGrammarTools` | Whether OpenAI-compatible APIs emit custom Lark/regex grammar tools. When `false`, grammar-constrained tools fall back to normal function tools. Default: `false`; the built-in model catalog enables it for GPT-5+ models on OpenAI, OpenAI Codex, Azure OpenAI, GitHub Copilot, opencode, and Cloudflare AI Gateway. |
| `deferredToolsMode` | Use provider-specific deferred tool serialization. Currently only `"kimi"` is supported for Kimi's OpenAI-compatible Chat Completions format. |
| `supportsLongCacheRetention` | Whether the provider accepts long cache retention when cache retention is `long`: `prompt_cache_options.ttl: "30m"` for GPT-5.6+ Responses models, `prompt_cache_retention: "24h"` for earlier OpenAI models, or `cache_control.ttl: "1h"` when `cacheControlFormat` is `anthropic`. Default: `true`. |
| `openRouterRouting` | OpenRouter provider routing preferences. This object is sent as-is in the `provider` field of the [OpenRouter API request](https://openrouter.ai/docs/guides/routing/provider-selection). |
| `vercelGatewayRouting` | Vercel AI Gateway routing config for provider selection (`only`, `order`) |

`openrouter` uses `reasoning: { effort }`. `together` uses `reasoning: { enabled }` and also `reasoning_effort` when `supportsReasoningEffort` is enabled. `qwen` uses top-level `enable_thinking`. Use `qwen-chat-template` for local Qwen-compatible servers that require `chat_template_kwargs.enable_thinking` and `preserve_thinking`. Use `chat-template` for vLLM/Hugging Face chat templates that need configurable `chat_template_kwargs`, such as `chatTemplateKwargs: { "thinking": { "$var": "thinking.enabled" } }` for DeepSeek V3.x templates. Use `thinkingFormat: "baseten"` with `chatTemplateArgs` for providers that expose toggle controls through `chat_template_args` and optionally support top-level `reasoning_effort`.

`thinkingTokenBudgetField` is independent of `thinkingFormat`. Do not enable it on the generated Qwen catalog: those models already send `reasoning_effort`, and DashScope rejects `thinking_budget` together with `reasoning_effort`.

`cacheControlFormat: "anthropic"` is for OpenAI-compatible providers that expose Anthropic-style prompt caching through `cache_control` markers on text content and tool definitions.

Example:

```json
{
  "providers": {
    "openrouter": {
      "baseUrl": "https://openrouter.ai/api/v1",
      "apiKey": "$OPENROUTER_API_KEY",
      "api": "openai-completions",
      "models": [
        {
          "id": "openrouter/anthropic/claude-3.5-sonnet",
          "name": "OpenRouter Claude 3.5 Sonnet",
          "compat": {
            "openRouterRouting": {
              "allow_fallbacks": true,
              "require_parameters": false,
              "data_collection": "deny",
              "zdr": true,
              "enforce_distillable_text": false,
              "order": ["anthropic", "amazon-bedrock", "google-vertex"],
              "only": ["anthropic", "amazon-bedrock"],
              "ignore": ["gmicloud", "friendli"],
              "quantizations": ["fp16", "bf16"],
              "sort": {
                "by": "price",
                "partition": "model"
              },
              "max_price": {
                "prompt": 10,
                "completion": 20
              },
              "preferred_min_throughput": {
                "p50": 100,
                "p90": 50
              },
              "preferred_max_latency": {
                "p50": 1,
                "p90": 3,
                "p99": 5
              }
            }
          }
        }
      ]
    }
  }
}
```

Vercel AI Gateway example:

```json
{
  "providers": {
    "vercel-ai-gateway": {
      "baseUrl": "https://ai-gateway.vercel.sh/v1",
      "apiKey": "$AI_GATEWAY_API_KEY",
      "api": "openai-completions",
      "models": [
        {
          "id": "moonshotai/kimi-k2.5",
          "name": "Kimi K2.5 (Fireworks via Vercel)",
          "reasoning": true,
          "input": ["text", "image"],
          "cost": { "input": 0.6, "output": 3, "cacheRead": 0, "cacheWrite": 0 },
          "contextWindow": 262144,
          "maxTokens": 262144,
          "compat": {
            "vercelGatewayRouting": {
              "only": ["fireworks", "novita"],
              "order": ["fireworks", "novita"]
            }
          }
        }
      ]
    }
  }
}
```
