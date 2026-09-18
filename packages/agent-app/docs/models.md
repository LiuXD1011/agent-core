# 模型与服务商

本页介绍内置服务商与认证，以及通过 `~/.agent-core/agent/models.json` 添加自定义模型和服务商。

## 目录

- [内置服务商与认证](#内置服务商与认证)
  - [订阅](#订阅)
  - [API 密钥](#api-密钥)
  - [认证文件](#认证文件)
  - [凭据键解析](#凭据键解析)
  - [云服务商](#云服务商)
  - [解析顺序](#解析顺序)
- [自定义模型](#自定义模型)
  - [最小示例](#最小示例)
  - [完整示例](#完整示例)
  - [支持的 API](#支持的-api)
  - [服务商配置](#服务商配置)
  - [模型配置](#模型配置)
  - [覆盖内置服务商](#覆盖内置服务商)
  - [按模型覆盖](#按模型覆盖)
  - [Anthropic Messages 兼容性](#anthropic-messages-兼容性)
  - [OpenAI 兼容性](#openai-兼容性)

## 内置服务商与认证

Agent Core 通过 OAuth 支持订阅型服务商，通过环境变量或认证文件支持 API 密钥型服务商。内置目录随 agent-core 一起发布；扩展注册的动态服务商可以刷新自己的目录，并缓存在 `~/.agent-core/agent/models-store.json` 中供离线使用。

### 订阅

在交互模式中使用 `/login`，然后选择服务商：

- ChatGPT Plus/Pro (Codex)
- Claude Pro/Max
- GitHub Copilot
- xAI（Grok/X 订阅）
- OpenRouter（OAuth 签发的 API 密钥，从 OpenRouter 额度计费）

使用 `/logout` 清除凭据。Token 保存在 `~/.agent-core/agent/auth.json` 中，过期后自动刷新。OpenRouter 则签发一个由用户掌控的 API 密钥，不会自动过期。

#### OpenAI Codex

- 需要 ChatGPT Plus 或 Pro 订阅
- 获 OpenAI 官方认可：[Codex for OSS](https://developers.openai.com/community/codex-for-oss)

#### Claude Pro/Max

Claude Pro/Max 账户可使用 Anthropic 订阅认证。第三方客户端的用量从[额外用量](https://claude.ai/settings/usage)中扣除，按 Token 计费，不占用 Claude 套餐额度。

#### GitHub Copilot

- github.com 直接按 Enter，或输入你的 GitHub Enterprise Server 域名
- 如果看到 "model not supported"，在 VS Code 中启用它：Copilot Chat → 模型选择器 → 选择模型 → "Enable"

#### xAI（Grok/X 订阅）

- 运行 `/login xai`，然后选择 **使用订阅**
- `XAI_API_KEY` 仍可通过 **使用 API 密钥** 使用

#### OpenRouter

- 运行 `/login openrouter`，然后选择 **使用 OpenRouter 登录**，打开 OpenRouter PKCE 授权流程
- 授权会创建一个由用户掌控的 OpenRouter API 密钥，从你的 OpenRouter 额度计费
- 在远程/无界面机器上（例如通过 SSH），浏览器无法访问回环回调；改为把最终重定向 URL（或授权码）粘贴到登录提示中
- `OPENROUTER_API_KEY` 仍可通过 **使用 API 密钥** 使用

### API 密钥

在交互模式中使用 `/login` 并选择服务商，把 API 密钥保存到 `auth.json`，或通过环境变量设置凭据：

```bash
export ANTHROPIC_API_KEY=sk-ant-...
agent-core
```

| 服务商 | 环境变量 | `auth.json` 键 |
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
| Cloudflare AI Gateway | `CLOUDFLARE_API_KEY`（+ `CLOUDFLARE_ACCOUNT_ID`、`CLOUDFLARE_GATEWAY_ID`） | `cloudflare-ai-gateway` |
| Cloudflare Workers AI | `CLOUDFLARE_API_KEY`（+ `CLOUDFLARE_ACCOUNT_ID`） | `cloudflare-workers-ai` |
| xAI | `XAI_API_KEY` | `xai` |
| OpenRouter | `OPENROUTER_API_KEY` | `openrouter` |
| Vercel AI Gateway | `AI_GATEWAY_API_KEY` | `vercel-ai-gateway` |
| ZAI Coding Plan（国际） | `ZAI_API_KEY` | `zai` |
| ZAI Coding Plan（中国） | `ZAI_CODING_CN_API_KEY` | `zai-coding-cn` |
| OpenCode Zen | `OPENCODE_API_KEY` | `opencode` |
| OpenCode Go | `OPENCODE_API_KEY` | `opencode-go` |
| Hugging Face | `HF_TOKEN` | `huggingface` |
| Fireworks | `FIREWORKS_API_KEY` | `fireworks` |
| Together AI | `TOGETHER_API_KEY` | `together` |
| Baseten | `BASETEN_API_KEY` | `baseten` |
| Kimi For Coding | `KIMI_API_KEY` | `kimi-coding` |
| MiniMax | `MINIMAX_API_KEY` | `minimax` |
| MiniMax（中国） | `MINIMAX_CN_API_KEY` | `minimax-cn` |
| Qwen Token Plan（现有目录） | `QWEN_TOKEN_PLAN_API_KEY` | `qwen-token-plan` |
| Qwen Token Plan（个人版） | `QWEN_TOKEN_PLAN_API_KEY` | `qwen-token-plan-individual` |
| Qwen Token Plan（中国） | `QWEN_TOKEN_PLAN_CN_API_KEY` | `qwen-token-plan-cn` |
| Xiaomi MiMo | `XIAOMI_API_KEY` | `xiaomi` |
| Xiaomi MiMo Token Plan（中国） | `XIAOMI_TOKEN_PLAN_CN_API_KEY` | `xiaomi-token-plan-cn` |
| Xiaomi MiMo Token Plan（阿姆斯特丹） | `XIAOMI_TOKEN_PLAN_AMS_API_KEY` | `xiaomi-token-plan-ams` |
| Xiaomi MiMo Token Plan（新加坡） | `XIAOMI_TOKEN_PLAN_SGP_API_KEY` | `xiaomi-token-plan-sgp` |

环境变量与 `auth.json` 键的参考：[`packages/ai/src/env-api-keys.ts`](https://github.com/LiuXD1011/agent-core/blob/main/packages/ai/src/env-api-keys.ts) 中的 [`const envMap`](https://github.com/LiuXD1011/agent-core/blob/main/packages/ai/src/env-api-keys.ts)。

#### 认证文件

把凭据保存在 `~/.agent-core/agent/auth.json`：

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

`qwen-token-plan-individual` 使用与 `qwen-token-plan` 相同的国际端点和 `QWEN_TOKEN_PLAN_API_KEY`，但选择器中只列出个人订阅文档中的模型。现有服务商为向后兼容保留更全的目录。使用 `auth.json` 时，把凭据存在你所选的服务商条目下；环境变量由两个国际服务商共用。

文件以 `0600` 权限创建（仅用户可读写）。认证文件凭据优先于环境变量。

API 密钥凭据还可以包含服务商作用域的环境变量值。解析凭据键、服务商/模型请求头以及服务商配置（如 Cloudflare 账户 ID、Azure OpenAI 设置、Vertex project/location、Bedrock 设置、`AGENT_CORE_CACHE_RETENTION` 和 `HTTP_PROXY`/`HTTPS_PROXY`）时，这些值优先于进程环境变量。

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

当 agent-core 需要使用与项目 shell 环境不同的服务商设置时，可以使用这种方式。

### 凭据键解析

`key` 字段支持命令执行、环境变量插值和字面量：

- **Shell 命令：** 以 `"!command"` 开头时，整个值会作为命令执行并使用其 stdout（在进程生命周期内缓存）
  ```json
  { "type": "api_key", "key": "!security find-generic-password -ws 'anthropic'" }
  { "type": "api_key", "key": "!op read 'op://vault/item/credential'" }
  ```
- **环境变量插值：** `"$ENV_VAR"` 或 `"${ENV_VAR}"` 使用指定变量的值。插值也可以出现在更长的字面量中。
  ```json
  { "type": "api_key", "key": "$MY_ANTHROPIC_KEY" }
  { "type": "api_key", "key": "${KEY_PREFIX}_${KEY_SUFFIX}" }
  ```
  `$FOO_BAR` 指变量 `FOO_BAR`；当 `BAR` 是字面文本时用 `${FOO}_BAR`。环境变量缺失会使该值无法解析。
- **转义：** `"$$"` 输出字面 `"$"`；`"$!"` 输出字面 `"!"`，不会触发命令执行。
  ```json
  { "type": "api_key", "key": "$$literal-dollar-prefix" }
  { "type": "api_key", "key": "$!literal-bang-prefix" }
  ```
- **字面量：** 直接使用。`MY_API_KEY` 这类纯大写字符串是字面量；要引用环境变量请用 `$MY_API_KEY`。
  ```json
  { "type": "api_key", "key": "sk-ant-..." }
  { "type": "api_key", "key": "public" }
  ```

`/login` 之后 OAuth 凭据也保存在这里，并自动管理。

### 云服务商

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

使用 `/login amazon-bedrock` 保存 Bedrock API 密钥，或配置以下任一环境 AWS 凭据来源：

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

还支持 ECS 任务角色（`AWS_CONTAINER_CREDENTIALS_*`）和 IRSA（`AWS_WEB_IDENTITY_TOKEN_FILE`）。

```bash
agent-core --provider amazon-bedrock --model us.anthropic.claude-sonnet-4-20250514-v1:0
```

ID 中包含可识别模型名的 Claude 模型（基础模型和系统定义的推理配置档）会自动启用提示缓存。对于应用推理配置档（ARN 中不含模型名），设置 `AWS_BEDROCK_FORCE_CACHE=1` 以启用缓存点：

```bash
export AWS_BEDROCK_FORCE_CACHE=1
agent-core --provider amazon-bedrock --model arn:aws:bedrock:us-east-1:123456789012:application-inference-profile/abc123
```

如果连接的是 Bedrock API 代理，可以使用以下环境变量：

```bash
# Set the URL for the Bedrock proxy (standard AWS SDK env var)
export AWS_ENDPOINT_URL_BEDROCK_RUNTIME=https://my.corp.proxy/bedrock

# Set if your proxy does not require authentication
export AWS_BEDROCK_SKIP_AUTH=1

# Set if your proxy only supports HTTP/1.1
export AWS_BEDROCK_FORCE_HTTP1=1
```

#### Cloudflare AI Gateway

`CLOUDFLARE_API_KEY` 可通过 `/login` 设置。账户 ID 和网关 slug 可以设为环境变量，或放在 `auth.json` 中 API 密钥凭据的 `env` 对象里。

```bash
export CLOUDFLARE_API_KEY=...           # or use /login
export CLOUDFLARE_ACCOUNT_ID=...
export CLOUDFLARE_GATEWAY_ID=...        # create at dash.cloudflare.com → AI → AI Gateway
agent-core --provider cloudflare-ai-gateway --model "claude-sonnet-4-5"
```

通过 Cloudflare AI Gateway 路由到 OpenAI、Anthropic 和 Workers AI。Workers AI 使用统一 API（`/compat`）和带前缀的模型 ID（`workers-ai/@cf/...`）。OpenAI 使用 OpenAI 直通路由（`/openai`）和原生 OpenAI 模型 ID（如 `gpt-5.1`）。Anthropic 使用 Anthropic 直通路由（`/anthropic`）和原生 Anthropic 模型 ID（如 `claude-sonnet-4-5`）。

AI Gateway 认证使用 `CLOUDFLARE_API_KEY` 作为 `cf-aig-authorization`。上游认证可以是以下方式之一：

| 模式 | 请求认证 | 上游认证 |
|------|--------------|---------------|
| Workers AI | 仅 Cloudflare token | Cloudflare 原生 |
| 统一计费 | 仅 Cloudflare token | Cloudflare 处理上游认证并扣除额度 |
| 存储式 BYOK | 仅 Cloudflare token | Cloudflare 注入 AI Gateway 控制台中保存的服务商密钥 |
| 内联 BYOK | Cloudflare token 加上游 `Authorization` 头 | 请求自带上游服务商密钥 |

正常使用 agent-core 时，建议选择统一计费或存储式 BYOK。内联 BYOK 需要为 Cloudflare AI Gateway 服务商额外配置上游 `Authorization` 头，例如通过 `models.json` 的 provider/model 覆盖。

#### Cloudflare Workers AI

`CLOUDFLARE_API_KEY` 可通过 `/login` 设置。`CLOUDFLARE_ACCOUNT_ID` 可以设为环境变量，或放在 `auth.json` 中 API 密钥凭据的 `env` 对象里。

```bash
export CLOUDFLARE_API_KEY=...           # or use /login
export CLOUDFLARE_ACCOUNT_ID=...
agent-core --provider cloudflare-workers-ai --model "@cf/moonshotai/kimi-k2.6"
```

Agent Core 会自动设置 `x-session-affinity`，以享受[前缀缓存](https://developers.cloudflare.com/workers-ai/features/prompt-caching/)折扣。

#### Google Vertex AI

使用应用默认凭据（Application Default Credentials）：

```bash
gcloud auth application-default login
export GOOGLE_CLOUD_PROJECT=your-project
export GOOGLE_CLOUD_LOCATION=us-central1
```

或将 `GOOGLE_APPLICATION_CREDENTIALS` 指向服务账号密钥文件。

### 解析顺序

解析服务商凭据时的顺序：

1. CLI `--api-key` 参数
2. `auth.json` 条目（API 密钥或 OAuth token）
3. 环境变量
4. `models.json` 中的自定义服务商密钥

## 自定义模型

通过 `~/.agent-core/agent/models.json` 添加自定义服务商和模型（Ollama、vLLM、LM Studio、代理等）。需要自定义 API 实现或 OAuth 流程的服务商请改用扩展实现；见 [custom-provider.md](custom-provider.md)。

## 最小示例

对本地模型（Ollama、LM Studio、vLLM）而言，每个模型只需 `id`：

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

`apiKey` 值只是占位符，Ollama 会忽略它。但 agent-core 仍要求模型有认证才会出现在 `/model` 中，因此无密钥的本地服务应保留一个占位值、用 `/login` 为该服务商保存一个密钥，或在选择模型时传 `--api-key`。

一些 OpenAI 兼容服务器无法理解推理模型使用的 `developer` 角色。对这类服务商，把 `compat.supportsDeveloperRole` 设为 `false`，agent-core 会改用 `system` 消息发送系统提示。如果服务器也不支持 `reasoning_effort`，再把 `compat.supportsReasoningEffort` 设为 `false`。

`compat` 可以设在服务商级别（对所有模型生效），也可以设在模型级别（覆盖特定模型）。常见于 Ollama、vLLM、SGLang 等类似的 OpenAI 兼容服务器。

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

## 完整示例

需要特定值时覆盖默认配置：

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

每次打开 `/model` 都会重新加载该文件。可在会话中直接编辑，无需重启。

## Google AI Studio 示例

将 `google-generative-ai` 与 `baseUrl` 结合使用，可添加来自 Google AI Studio 的模型，包括自定义 Gemma 4 条目：

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

为 `google-generative-ai` API 类型添加自定义模型时必须提供 `baseUrl`。

## 支持的 API

| API | 说明 |
|-----|-------------|
| `openai-completions` | OpenAI Chat Completions（兼容性最好） |
| `openai-responses` | OpenAI Responses API |
| `anthropic-messages` | Anthropic Messages API |
| `google-generative-ai` | Google Generative AI |

`api` 可设在服务商级别（对所有模型生效）或模型级别（覆盖单个模型）。

## 服务商配置

| 字段 | 说明 |
|-------|-------------|
| `baseUrl` | API 端点 URL |
| `api` | API 类型（见上文） |
| `apiKey` | 可选的 API 密钥配置（取值解析见下文）。若认证由 `/login`/`auth.json` 或 CLI `--api-key` 提供，可省略。 |
| `headers` | 自定义请求头（取值解析见下文） |
| `authHeader` | 设为 `true` 自动添加 `Authorization: Bearer <apiKey>` |
| `models` | 模型配置数组 |
| `modelOverrides` | 对该服务商上内置或扩展注册模型的按模型覆盖 |

带 `models` 的非内置服务商配置需要在服务商或模型级别提供 `baseUrl` 和 `api` 值。加载文件不要求 `apiKey`：只要通过 `/login`/`auth.json`、CLI `--api-key` 或服务商 `apiKey` 配置了认证，模型即可用。未配置认证时模型仍会加载，但在 `/model` 和 `--list-models` 中不可用。

### 取值解析

`apiKey` 和 `headers` 字段支持命令执行、环境变量插值和字面量：

- **Shell 命令：** 以 `"!command"` 开头时，整个值会作为命令执行并使用其 stdout
  ```json
  "apiKey": "!security find-generic-password -ws 'anthropic'"
  "apiKey": "!op read 'op://vault/item/credential'"
  ```
- **环境变量插值：** `"$ENV_VAR"` 或 `"${ENV_VAR}"` 使用指定变量的值。插值也可以出现在更长的字面量中。
  ```json
  "apiKey": "$MY_API_KEY"
  "apiKey": "${KEY_PREFIX}_${KEY_SUFFIX}"
  ```
  `$FOO_BAR` 指变量 `FOO_BAR`；当 `BAR` 是字面文本时用 `${FOO}_BAR`。环境变量缺失会使该值无法解析。
- **转义：** `"$$"` 输出字面 `"$"`；`"$!"` 输出字面 `"!"`，不会触发命令执行。
  ```json
  "apiKey": "$$literal-dollar-prefix"
  "apiKey": "$!literal-bang-prefix"
  ```
- **字面量：** 直接使用。`MY_API_KEY` 这类纯大写字符串是字面量；要引用环境变量请用 `$MY_API_KEY`。
  ```json
  "apiKey": "sk-..."
  ```

对 `models.json`，shell 命令在请求时解析。agent-core 有意不为任意命令应用内置的 TTL、过期复用或恢复逻辑。不同命令需要不同的缓存与失败策略，pi 无法替你推断。

如果你的命令很慢、开销大、有速率限制，或应在瞬时失败时沿用上一次的值，请用自带缓存或 TTL 行为的脚本或命令包装它。

`/model` 的可用性检查只看已配置的认证是否存在，不会执行 shell 命令。

### 自定义请求头

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

## 模型配置

| 字段 | 必填 | 默认值 | 说明 |
|-------|----------|---------|-------------|
| `id` | 是 | — | 模型标识符（传给 API） |
| `name` | 否 | `id` | 人类可读的模型标签。用于匹配（`--model` 模式），并显示为次要模型详情文本。 |
| `api` | 否 | 服务商的 `api` | 为该模型覆盖服务商的 API |
| `reasoning` | 否 | `false` | 支持扩展思考 |
| `thinkingLevelMap` | 否 | 省略 | 将 agent-core 思考级别映射到服务商取值，并标记不支持的级别（见下文） |
| `input` | 否 | `["text"]` | 输入类型：`["text"]` 或 `["text", "image"]` |
| `contextWindow` | 否 | `128000` | 上下文窗口大小（Token） |
| `maxTokens` | 否 | `16384` | 最大输出 Token 数 |
| `samplingParams` | 否 | 省略 | 原样合并进每个请求体的采样参数（见下文） |
| `cost` | 否 | 全为零 | 每百万 Token 费率，可带作用于整个请求的输入价格分层 |
| `compat` | 否 | 服务商 `compat` | 服务商兼容性覆盖。两者都设置时与服务商级 `compat` 合并。 |

价格分层提供一套完整的替代费率，当总输入用量（`input + cacheRead + cacheWrite`）超过 `inputTokensAbove` 时作用于整个请求。多个分层都匹配时取阈值最高的那个。

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

当前行为：
- `/model`、`--list-models` 和交互式页脚按模型 `id` 显示条目。
- 配置的 `name` 用于模型匹配和次要模型详情文本，不会替换页脚/状态栏中的模型 id。

自定义模型条目是静态的：没有在线目录保持其更新。添加新模型意味着自行配置 `api`、`baseUrl`、API 密钥、模型 ID 以及所需的能力参数（`reasoning`、`input`、`compat`）。价格和上下文窗口必须手动更新，得到的用量/费用数字只是本地估算，不代表服务商账单。

### 采样参数

`samplingParams` 是自由格式的对象，会在 agent-core 自己设置的字段之后原样合并进该模型的每个请求体，因此其键值优先生效。用它发送 agent-core 未建模的采样参数——包括服务器特有的参数，如 llama.cpp 的 `min_p` 或 vLLM 的 `top_k`：

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

只有 OpenAI 兼容 API（`openai-completions`、`openai-responses`、`azure-openai-responses`）会应用它；其他 API 忽略。其键会覆盖 agent-core 的具名请求字段（例如这里的 `temperature` 键会压过请求级 temperature），因此建议把它当作单个模型采样参数的唯一来源。在 `modelOverrides` 中，`samplingParams` 按键与基础模型的值合并。

固定的思考 Token 上限也可以放在这里，但它不会跟随 `thinkingBudgets`，也不会为回答留出空间。要实现后者请使用 `compat.thinkingTokenBudgetField`（或别名 `supportsThinkingTokenBudget`）。

### 思考级别映射

在模型上使用 `thinkingLevelMap` 描述模型特有的思考控制。键为 agent-core 思考级别：`off`、`minimal`、`low`、`medium`、`high`、`xhigh`、`max`。映射允许有缺口，例如模型可以提供 `high` 和 `max` 而不提供 `xhigh`。

取值有三种状态：

| 取值 | 含义 |
|-------|---------|
| 省略 | 到 `high` 为止的标准级别使用服务商默认映射；扩展的 `xhigh` 和 `max` 级别不受支持 |
| 字符串 | 该级别受支持，此值会发送给服务商 |
| `null` | 该级别不受支持，会被隐藏/跳过/收敛 |

只支持 off、high 和 max 推理的模型示例：

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

思考无法关闭的模型示例：

```json
{
  "id": "always-thinking-model",
  "reasoning": true,
  "thinkingLevelMap": {
    "off": null
  }
}
```

迁移：使用 `compat.reasoningEffortMap` 的旧配置应把该映射移到模型级 `thinkingLevelMap`。不应出现在 UI 中的级别用 `null`。

## 覆盖内置服务商

无需重新定义模型即可让内置服务商走代理：

```json
{
  "providers": {
    "anthropic": {
      "baseUrl": "https://my-proxy.example.com/v1"
    }
  }
}
```

所有内置 Anthropic 模型仍然可用，已有的 OAuth 或 API 密钥认证继续生效。

要把自定义模型合并进内置服务商，加上 `models` 数组：

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

合并语义：
- 保留内置模型。
- 自定义模型按 `id` 在该服务商内 upsert。
- 自定义模型 `id` 与内置模型 `id` 相同时，自定义模型替换该内置模型。
- 自定义模型 `id` 是新的时，与内置模型并列添加。

## 按模型覆盖

使用 `modelOverrides` 可自定义内置模型和匹配的扩展注册模型，而无需替换服务商的完整模型列表。

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

`modelOverrides` 对每个模型支持这些字段：`name`、`reasoning`、`thinkingLevelMap`、`input`、`cost`（部分）、`contextWindow`、`maxTokens`、`samplingParams`（按键合并）、`headers`、`compat`。

OpenAI 直连的 GPT-5.6 Sol、Terra 和 Luna 默认 `272000` 上下文窗口，使请求保持在 OpenAI 短上下文价格档内。要改用 OpenAI 的 1.05M 上下文窗口，请为你使用的每个模型调大该值：

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

该覆盖保留内置定价元数据。总输入 Token 超过 272K 的请求对整个请求使用 GPT-5.6 的长上下文费率。需要时对 `gpt-5.6-terra` 或 `gpt-5.6-luna` 应用相同覆盖。

行为说明：
- `modelOverrides` 应用于内置服务商模型和匹配的扩展注册服务商模型。
- 未知的模型 ID 会被忽略。
- 服务商级 `baseUrl`/`headers` 可以与 `modelOverrides` 组合使用。
- 覆盖 `name` 只影响模型匹配和次要详情文本；页脚和主要模型列表仍显示模型 `id`。
- 如果服务商还定义了 `models`，自定义模型在内置覆盖之后合并。`id` 相同的自定义模型会替换被覆盖的内置模型条目。

## Anthropic Messages 兼容性

对使用 `api: "anthropic-messages"` 的服务商或代理，用 `compat` 控制 Anthropic 特有的请求兼容性。

默认情况下 agent-core 会发送每个工具的 `eager_input_streaming: true`。如果代理或 Anthropic 兼容后端拒绝该字段，把 `supportsEagerToolInputStreaming` 设为 `false`。Agent Core 会省略 `tools[].eager_input_streaming`，并在启用工具的请求上改用旧版 `fine-grained-tool-streaming-2025-05-14` beta 头。

一些 Anthropic 模型要求自适应思考（`thinking.type: "adaptive"` 加 `output_config.effort`），而不是旧版基于预算的思考负载。内置模型会自动设置。路由到这些模型的自定义服务商或别名请把 `forceAdaptiveThinking` 设为 `true`。

支持每回合 effort 的 Claude 模型使用 `supportsMidConvoEffort`。启用后 Agent Core 会持久化每次响应的服务商 effort、在后续请求中重建仅含 effort 的系统消息，并发送带 `prefix_mismatch_behavior: "drop_block"` 的思考绑定控制，避免过期的签名思考前缀造成持续的 400 响应。只在忠实实现 Anthropic Messages 传输且精确支持该特性的 Claude 模型上设置；对仅模仿 Messages 形状的 API 不要启用。

一些 Anthropic 兼容服务商会发出签名为空的思考块，并期望重放时保持原样。只为这类服务商把 `allowEmptySignature` 设为 `true`；真正的 Anthropic 会拒绝空的思考签名。

内置 Anthropic 模型在其模型元数据中启用了 `supportsStrictTools`。自定义 Anthropic 兼容模型如果端点接受严格 JSON-schema 工具定义，必须将其设为 `true`。

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

| 字段 | 说明 |
|-------|-------------|
| `supportsEagerToolInputStreaming` | 服务商是否接受每工具的 `eager_input_streaming`。默认 `true`。设为 `false` 可省略该字段，并在启用工具的请求上使用旧版细粒度工具流式 beta 头。 |
| `supportsLongCacheRetention` | 缓存保留期为 `long` 时，服务商是否接受 Anthropic 长缓存保留（`cache_control.ttl: "1h"`）。默认 `true`。 |
| `sendSessionAffinityHeaders` | 启用缓存时是否基于会话 id 发送 `x-session-affinity`。默认：对已知服务商自动检测。 |
| `supportsCacheControlOnTools` | 服务商是否接受工具定义上的 Anthropic 风格 `cache_control` 标记。默认 `true`。 |
| `forceAdaptiveThinking` | 是否为该模型发送自适应思考（`thinking.type: "adaptive"` 加 `output_config.effort`）。内置自适应模型自动设置。默认 `false`。 |
| `supportsMidConvoEffort` | 该 Claude 模型的传输是否支持每回合 effort 系统消息和思考绑定控制。启用后 Agent Core 持久化原生 effort 级别并始终发送 `drop_block`。默认 `false`。 |
| `allowEmptySignature` | 是否以 `signature: ""` 重放空思考签名，而不是把思考转换为文本。默认 `false`。 |
| `supportsStrictTools` | 服务商是否接受严格 JSON-schema 工具定义。默认 `false`；内置 Anthropic 模型在生成的元数据中启用了它。 |

## OpenAI 兼容性

对部分兼容 OpenAI 的服务商，使用 `compat` 字段。

- 服务商级 `compat` 为该服务商下所有模型提供默认值。
- 模型级 `compat` 为该模型覆盖服务商级取值。

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

| 字段 | 说明 |
|-------|-------------|
| `supportsStore` | 服务商是否支持 `store` 字段 |
| `supportsDeveloperRole` | 使用 `developer` 还是 `system` 角色 |
| `supportsReasoningEffort` | 是否支持 `reasoning_effort` 参数 |
| `supportsUsageInStreaming` | 是否支持 `stream_options: { include_usage: true }`（默认 `true`） |
| `supportsFinishReason` | 流式响应是否包含 `finish_reason`。为 `false` 时，agent-core 会在流结束时推断 `stop` 或 `toolUse`。默认 `true`。 |
| `maxTokensField` | 使用 `max_completion_tokens` 还是 `max_tokens` |
| `requiresToolResultName` | 工具结果消息需包含 `name` |
| `requiresAssistantAfterToolResult` | 工具结果之后、用户消息之前需插入一条助手消息 |
| `requiresThinkingAsText` | 将思考块转换为纯文本 |
| `requiresReasoningContentOnAssistantMessages` | 启用推理时，在所有重放的助手消息上包含空的 `reasoning_content` |
| `thinkingFormat` | 使用 `reasoning_effort`、`openrouter`、`deepseek`、`together`、`baseten`、`zai`、`qwen`、`chat-template` 或 `qwen-chat-template` 思考参数 |
| `chatTemplateKwargs` | `thinkingFormat: "chat-template"` 的 `chat_template_kwargs` 取值；用 `{ "$var": "thinking.enabled" }`、`{ "$var": "thinking.effort" }` 或 `{ "$var": "thinking.budget" }` 表示由 agent-core 控制的思考值 |
| `chatTemplateArgs` | `thinkingFormat: "baseten"` 的 `chat_template_args` 取值；用 `{ "$var": "thinking.enabled" }`、`{ "$var": "thinking.effort" }` 或 `{ "$var": "thinking.budget" }` 表示由 agent-core 控制的思考值 |
| `thinkingTokenBudgetField` | 用于限制来自 `thinkingBudgets` 的推理 Token 的顶层请求字段，会向下收敛以保证至少 1024 Token 留给回答。可选 `"thinking_token_budget"`（vLLM）、`"thinking_budget"`（Qwen/DashScope/SGLang）、`"thinking_budget_tokens"`（llama.cpp）。默认关闭；生成的目录中不设置。 |
| `supportsThinkingTokenBudget` | `thinkingTokenBudgetField: "thinking_token_budget"`（vLLM）的别名。建议使用 `thinkingTokenBudgetField`。默认 `false`。 |
| `cacheControlFormat` | 在系统提示、最后一个工具定义以及最后一条用户/助手/工具结果文本内容上使用 Anthropic 风格 `cache_control` 标记。目前仅支持 `anthropic`。 |
| `sendSessionAffinityHeaders` | 对 `openai-completions`，启用缓存时基于会话 id 发送会话亲和头。默认 `false`。 |
| `sessionAffinityFormat` | 对 `openai-completions` 和 `openai-responses`，会话亲和头的格式：`openai` 发送 `session_id`/`x-client-request-id`（completions 还发送 `x-session-affinity`），`openai-nosession` 省略含下划线的 `session_id` 头，`openrouter` 发送 `x-session-id`。不影响 `prompt_cache_key` 请求体参数。默认自动检测。 |
| `supportsStrictMode` | 服务商是否接受严格 JSON-schema 函数工具定义。默认值取决于 API；内置 OpenAI 模型带有显式能力元数据。 |
| `supportsOpenAIGrammarTools` | OpenAI 兼容 API 是否发出自定义 Lark/regex 语法工具。为 `false` 时，语法约束工具回退为普通函数工具。默认 `false`；内置模型目录为 OpenAI、OpenAI Codex、Azure OpenAI、GitHub Copilot、opencode 和 Cloudflare AI Gateway 上的 GPT-5+ 模型启用了它。 |
| `deferredToolsMode` | 使用服务商特定的延迟工具序列化。目前仅支持 `"kimi"`，用于 Kimi 的 OpenAI 兼容 Chat Completions 格式。 |
| `supportsLongCacheRetention` | 缓存保留期为 `long` 时服务商是否接受长缓存保留：GPT-5.6+ Responses 模型用 `prompt_cache_options.ttl: "30m"`，更早的 OpenAI 模型用 `prompt_cache_retention: "24h"`，`cacheControlFormat` 为 `anthropic` 时用 `cache_control.ttl: "1h"`。默认 `true`。 |
| `openRouterRouting` | OpenRouter 服务商路由偏好。该对象原样放入 [OpenRouter API 请求](https://openrouter.ai/docs/guides/routing/provider-selection)的 `provider` 字段发送。 |
| `vercelGatewayRouting` | Vercel AI Gateway 用于选择服务商的路由配置（`only`、`order`） |

`openrouter` 使用 `reasoning: { effort }`。`together` 使用 `reasoning: { enabled }`，并在启用 `supportsReasoningEffort` 时同时发送 `reasoning_effort`。`qwen` 使用顶层的 `enable_thinking`。本地 Qwen 兼容服务器如果要求 `chat_template_kwargs.enable_thinking` 和 `preserve_thinking`，请使用 `qwen-chat-template`。vLLM/Hugging Face 聊天模板如果需要可配置的 `chat_template_kwargs`，请使用 `chat-template`，例如 DeepSeek V3.x 模板用 `chatTemplateKwargs: { "thinking": { "$var": "thinking.enabled" } }`。服务商通过 `chat_template_args` 暴露开关并可选支持顶层 `reasoning_effort` 时，请使用 `thinkingFormat: "baseten"` 配合 `chatTemplateArgs`。

`thinkingTokenBudgetField` 与 `thinkingFormat` 相互独立。不要在生成的 Qwen 目录上启用它：这些模型已经发送 `reasoning_effort`，而 DashScope 会同时拒绝 `thinking_budget` 与 `reasoning_effort`。

`cacheControlFormat: "anthropic"` 适用于通过文本内容和工具定义上的 `cache_control` 标记提供 Anthropic 风格提示缓存的 OpenAI 兼容服务商。

示例：

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

Vercel AI Gateway 示例：

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
