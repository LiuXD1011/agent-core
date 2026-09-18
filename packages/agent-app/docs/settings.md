# 设置

Agent Core 使用 JSON 设置文件，项目设置覆盖全局设置。

| 位置 | 作用范围 |
|----------|-------|
| `~/.agent-core/agent/settings.json` | 全局（所有项目） |
| `.agent-core/settings.json` | 项目（当前目录） |

可以直接编辑，或使用 `/settings` 调整常用选项。要以交互方式保存启动模型默认值，使用 `/model` 并在目标模型上按 Ctrl+S。要保存启动思考级别，使用 `/thinking` 并按 Ctrl+S。

## 项目信任

交互模式启动时，如果项目文件夹包含项目级设置、资源或项目 `.agents/skills`，且 `~/.agent-core/agent/trust.json` 中没有该文件夹或其父文件夹的信任记录，Agent Core 会先询问是否信任该项目。信任项目后，agent-core 才能加载项目设置（`.agent-core/settings.json`）和 `.agent-core/` 下的项目资源、安装缺失的项目包，并执行项目扩展。

非交互模式（`-p`、`--mode json` 和 `--mode rpc`）不显示信任提示。在没有适用的已保存信任决定时，它们使用全局设置中的 `defaultProjectTrust`：`ask`（默认）和 `never` 会忽略这些项目资源，`always` 则信任它们。可传 `--approve`/`-a` 或 `--no-approve`/`-na` 为单次运行覆盖项目信任。

如果没有扩展或已保存的决定适用，`defaultProjectTrust` 决定回退行为。可在 `~/.agent-core/agent/settings.json` 中将其设为 `"ask"`、`"always"` 或 `"never"`，或通过 `/settings` 修改。

`agent-core config` 和包命令使用相同的项目信任流程，但 `agent-core update` 从不提示。传 `--approve` 为单条命令信任项目本地设置，或传 `--no-approve` 忽略它们。

在交互模式中使用 `/trust` 可保存项目信任决定供后续会话使用，包括对直接父文件夹的信任。它只写入 `~/.agent-core/agent/trust.json`；当前会话不会重新加载，需重启 agent-core 才能生效。

## 全部设置

### 模型与思考

| 设置项 | 类型 | 默认值 | 说明 |
|---------|------|---------|-------------|
| `defaultProvider` | string | - | 启动服务商（如 `"anthropic"`、`"openai"`；在 `/model` 中按 Ctrl+S 保存，或手动编辑） |
| `defaultModel` | string | - | 启动模型 ID（在 `/model` 中按 Ctrl+S 保存，或手动编辑） |
| `defaultThinkingLevel` | string | - | 启动思考级别（在 `/thinking` 中按 Ctrl+S 保存，或手动编辑）：`"off"`、`"minimal"`、`"low"`、`"medium"`、`"high"`、`"xhigh"`、`"max"` |
| `modelThinkingLevels` | object | - | 按模型设置的启动思考级别，键为 `"provider/modelId"`；可在 `/settings` → 每个模型的默认思考级别 中配置，或手动编辑 |
| `hideThinkingBlock` | boolean | `false` | 隐藏输出中的思考块 |
| `showCacheMissNotices` | boolean | `false` | 在对话记录中显示重要提示缓存未命中、压缩或分支摘要用量，以及服务商恢复诊断（如被丢弃的 Anthropic 思考块）的通知 |
| `thinkingBudgets` | object | - | 每个思考级别的自定义 Token 预算。Anthropic、Google 和 Bedrock 原生使用。OpenAI 兼容模型在设置了 `compat.thinkingTokenBudgetField`（或 `supportsThinkingTokenBudget`）时使用。 |

#### thinkingBudgets

```json
{
  "thinkingBudgets": {
    "minimal": 1024,
    "low": 4096,
    "medium": 10240,
    "high": 32768
  }
}
```

### UI 与显示

| 设置项 | 类型 | 默认值 | 说明 |
|---------|------|---------|-------------|
| `theme` | string | `"dark"` | 主题名称（`"dark"`、`"light"` 或自定义） |
| `externalEditor` | string | `$VISUAL`，其次 `$EDITOR`，Windows 上为记事本，其他平台为 `nano` | Ctrl+G 外部编辑器命令；优先于环境变量 |
| `quietStartup` | boolean | `false` | 安静启动：隐藏启动页眉 |
| `defaultProjectTrust` | string | `"ask"` | 回退的项目信任行为：`"ask"`、`"always"` 或 `"never"`。仅限全局设置 |
| `collapseChangelog` | boolean | `false` | 更新后显示折叠的变更记录 |
| `enableAnalytics` | boolean | `false` | 可选择加入的分析数据共享。目前仅在实验性首次设置（`AGENT_CORE_EXPERIMENTAL=1`）中询问 |
| `trackingId` | string | - | 分析跟踪标识，在开启 `enableAnalytics` 时生成 |
| `doubleEscapeAction` | string | `"tree"` | 双击 Esc 触发的动作：`"tree"`、`"fork"` 或 `"none"` |
| `treeFilterMode` | string | `"default"` | `/tree` 的默认筛选模式：`"default"`、`"no-tools"`、`"user-only"`、`"labeled-only"`、`"all"` |
| `editorPaddingX` | number | `0` | 输入编辑器的水平内边距（0-3） |
| `outputPad` | number | `1` | 用户消息、助手消息和思考内容的水平内边距（0 或 1） |
| `autocompleteMaxVisible` | number | `5` | 自动补全下拉列表中最多可见项数（3-20） |
| `showHardwareCursor` | boolean | `false` | TUI 为支持输入法定位终端光标时显示该光标 |
| `tuiMode` | string | `"regular"` | 交互式 TUI 模式：`"regular"` 或实验性的 `"fullscreen"`。在 `/settings` 中的更改立即生效；启动时 `--tui-mode` 覆盖此设置 |
| `fullscreenExitOutput` | string | `"transcript"` | 全屏退出输出：`"transcript"` 打印最终对话记录和恢复提示，`"resume-hint"` 恢复之前的屏幕并只打印恢复提示。普通 TUI 模式下无效 |
| `fullscreenScrollbar` | string | `"auto"` | 全屏对话记录滚动条：`"auto"` 在滚动或指针悬停在最右列轨道上时临时显示，`"always"` 保留该列并保持可见，`"hidden"` 隐藏。普通 TUI 模式下无效 |

VS Code 需要加 `--wait`，这样 agent-core 才会在编辑器退出后继续：

```json
{
  "externalEditor": "code --wait"
}
```

### 更新检查


更新检查默认关闭：Agent Core 启动时不发起版本请求。只有配置了 `AGENT_CORE_VERSION_CHECK_URL`（一个返回 `{ "packageName": "...", "version": "..." }` 的源）时才会检查；返回其他包的源会被拒绝。设置 `AGENT_CORE_SKIP_VERSION_CHECK=1` 可在单次运行中跳过已配置的检查。未配置源时，可用 `npm install -g @liuxuedeng/agent-core@latest` 手动升级。使用 `--offline` 或 `AGENT_CORE_OFFLINE=1` 可禁用这里描述的所有启动网络操作，包括包更新检查。

### 网络

| 设置项 | 类型 | 默认值 | 说明 |
|---------|------|---------|-------------|
| `httpProxy` | string | - | HTTP 代理 URL，会应用为 `HTTP_PROXY` 和 `HTTPS_PROXY`。仅限全局设置。 |

```json
{
  "httpProxy": "http://127.0.0.1:7890"
}
```

### 警告

| 设置项 | 类型 | 默认值 | 说明 |
|---------|------|---------|-------------|
| `warnings.anthropicExtraUsage` | boolean | `true` | 当 Anthropic 订阅认证可能使用付费额外用量时显示警告 |

```json
{
  "warnings": {
    "anthropicExtraUsage": false
  }
}
```

### 上下文压缩

| 设置项 | 类型 | 默认值 | 说明 |
|---------|------|---------|-------------|
| `compaction.enabled` | boolean | `true` | 启用自动压缩 |
| `compaction.reserveTokens` | number | `16384` | 为 LLM 响应保留的 Token 数 |
| `compaction.keepRecentTokens` | number | `20000` | 保留的近期 Token 数（不做摘要） |

```json
{
  "compaction": {
    "enabled": true,
    "reserveTokens": 16384,
    "keepRecentTokens": 20000
  }
}
```

### 分支摘要

| 设置项 | 类型 | 默认值 | 说明 |
|---------|------|---------|-------------|
| `branchSummary.reserveTokens` | number | `16384` | 选择分支历史时保留的 Token 数；输出上限为 4096 Token |
| `branchSummary.skipPrompt` | boolean | `false` | 在 `/tree` 导航时跳过“是否摘要分支？”提示（默认不生成摘要） |

### 重试

| 设置项 | 类型 | 默认值 | 说明 |
|---------|------|---------|-------------|
| `retry.enabled` | boolean | `true` | 对瞬时错误启用 Agent 级自动重试 |
| `retry.maxRetries` | number | `3` | Agent 级最大重试次数 |
| `retry.baseDelayMs` | number | `2000` | Agent 级指数退避的基础延迟（2s、4s、8s） |
| `retry.provider.timeoutMs` | number | SDK 默认值 | 服务商/SDK 请求超时（毫秒） |
| `retry.provider.maxRetries` | number | `0` | 服务商/SDK 重试次数 |
| `retry.provider.maxRetryDelayMs` | number | `60000` | 失败前允许的服务商要求延迟上限（60s） |

当服务商要求的重试延迟超过 `retry.provider.maxRetryDelayMs` 时，请求会立即失败并给出明确的错误，而不是静默等待。设为 `0` 可禁用该上限。

除非明确需要服务商级重试，否则保持 `retry.provider.maxRetries` 为 `0`。设为大于 0 后，SDK/服务商重试可能在 Agent Core 感知之前就处理超出用量限制的错误，某些情况下会让 Agent 一直阻塞到服务商配额重置。

```json
{
  "retry": {
    "enabled": true,
    "maxRetries": 3,
    "baseDelayMs": 2000,
    "provider": {
      "timeoutMs": 3600000,
      "maxRetries": 0,
      "maxRetryDelayMs": 60000
    }
  }
}
```

### 消息投递

| 设置项 | 类型 | 默认值 | 说明 |
|---------|------|---------|-------------|
| `steeringMode` | string | `"one-at-a-time"` | 插话消息的发送方式：`"all"` 或 `"one-at-a-time"` |
| `followUpMode` | string | `"one-at-a-time"` | 追加消息的发送方式：`"all"` 或 `"one-at-a-time"` |
| `transport` | string | `"auto"` | 支持多种传输方式的服务商的首选传输：`"sse"`、`"websocket"`、`"websocket-cached"` 或 `"auto"` |
| `httpIdleTimeoutMs` | number | `300000` | HTTP 头/主体的空闲超时（毫秒），有显式流空闲超时的服务商也使用它。设为 `0` 禁用。 |
| `websocketConnectTimeoutMs` | number | `15000` | 支持 WebSocket 传输的服务商的 WebSocket 连接/握手超时（毫秒）。设为 `0` 禁用。 |

### 终端与图像

| 设置项 | 类型 | 默认值 | 说明 |
|---------|------|---------|-------------|
| `terminal.showImages` | boolean | `true` | 在终端中显示图像（如果支持） |
| `terminal.imageWidthCells` | number | `60` | 内联图像的首选宽度（终端字符列数） |
| `terminal.clearOnShrink` | boolean | `false` | 内容变矮时清除空行（可能造成闪烁） |
| `terminal.hyperlinks` | boolean or `"auto"` | `"auto"` | 覆盖 OSC 8 超链接支持（高级，仅 JSON） |
| `terminal.images` | string or boolean | `"auto"` | 用 `"kitty"`、`"iterm2"`、`false` 或 `"auto"` 覆盖图像协议支持（高级，仅 JSON） |
| `terminal.trueColor` | boolean or `"auto"` | `"auto"` | 覆盖真彩色支持（高级，仅 JSON） |
| `images.autoResize` | boolean | `true` | 自动缩放图像至最大 2000x2000。适用于 `@file` 附件、`read` 以及工具返回的图像 |
| `images.blockImages` | boolean | `false` | 阻止所有图像发送给 LLM |

### Shell

| 设置项 | 类型 | 默认值 | 说明 |
|---------|------|---------|-------------|
| `shellPath` | string | - | 自定义 shell 路径（例如 Windows 上的 Cygwin）；支持开头的 `~` 表示主目录 |
| `shellCommandPrefix` | string | - | 每条 bash 命令的前缀（如 `"shopt -s expand_aliases"`） |
| `npmCommand` | string[] | - | 用于 npm 包查找/安装操作的命令 argv（如 `["mise", "exec", "node@20", "--", "npm"]`） |

JSON 中的 Windows 路径必须使用正斜杠或转义的反斜杠：

```json
{
  "shellPath": "C:/Program Files/Git/bin/bash.exe"
}
```

```json
{
  "shellPath": "C:\\Program Files\\Git\\bin\\bash.exe"
}
```

```json
{
  "npmCommand": ["mise", "exec", "node@20", "--", "npm"]
}
```

`npmCommand` 用于所有 npm 包管理器操作，包括安装、卸载以及 git 包内的依赖安装。用户级 npm 包安装在 `~/.agent-core/agent/npm/` 下；项目级 npm 包安装在 `.agent-core/npm/` 下。argv 形式的条目应与进程的实际启动方式完全一致。配置 `npmCommand` 后，git 包依赖安装会使用普通的 `install`，避免在包装器或其他包管理器中出现 npm 特有的标志。

### 工具

| 设置项 | 类型 | 默认值 | 说明 |
|---------|------|---------|-------------|
| `defaultTools` | string[] | - | 初始启用的内置工具。省略时 Agent Core 使用标准默认值 |

`defaultTools` 选择启动时启用的内置工具。扩展和 SDK 自定义工具不受影响。可用的内置工具为 `read`、`bash`、`powershell`、`edit`、`write`、`grep`、`find` 和 `ls`：

```json
{
  "defaultTools": ["bash", "edit", "write"]
}
```

在 Windows 上选择 `powershell` 而不是 `bash`，或两者都包含：

```json
{
  "defaultTools": ["read", "powershell", "edit", "write"]
}
```

空数组表示启动时不启用任何内置工具，同时保留扩展和 SDK 自定义工具。`--tools` 会用对所有工具的严格允许列表取代这一行为，`--no-tools` 禁用所有工具，`--no-builtin-tools` 禁用内置默认值。`--exclude-tools` 对结果列表再过滤。项目级 `defaultTools` 数组会替换全局数组。

### 会话

| 设置项 | 类型 | 默认值 | 说明 |
|---------|------|---------|-------------|
| `sessionDir` | string | - | 会话文件存储目录。接受绝对或相对路径，以及 `~`。 |

```json
{ "sessionDir": ".agent-core/sessions" }
```

当多个来源指定会话目录时，优先级为 `--session-dir`、`AGENT_CORE_CODING_AGENT_SESSION_DIR`，然后是 settings.json 中的 `sessionDir`。

### Markdown

| 设置项 | 类型 | 默认值 | 说明 |
|---------|------|---------|-------------|
| `markdown.codeBlockIndent` | string | `"  "` | 代码块的缩进 |
| `markdown.mermaid` | string | `"streaming"` | Mermaid 图表渲染模式：`"off"`、`"final"` 或 `"streaming"` |

### 资源

这些设置定义从哪里加载扩展、技能、提示和主题。

`~/.agent-core/agent/settings.json` 中的路径相对于 `~/.agent-core/agent` 解析。`.agent-core/settings.json` 中的路径相对于 `.agent-core` 解析。支持绝对路径和 `~`。

| 设置项 | 类型 | 默认值 | 说明 |
|---------|------|---------|-------------|
| `packages` | array | `[]` | 从中加载资源的 npm/git 包 |
| `extensions` | string[] | `[]` | 本地扩展文件路径或目录 |
| `skills` | string[] | `[]` | 本地技能文件路径或目录 |
| `prompts` | string[] | `[]` | 本地提示模板路径或目录 |
| `themes` | string[] | `[]` | 本地主题文件路径或目录 |
| `enableSkillCommands` | boolean | `true` | 将技能注册为 `/skill:name` 技能命令 |

数组支持 glob 模式和排除项。用 `!pattern` 排除。用 `+path` 强制包含精确路径，用 `-path` 强制排除精确路径。

#### packages

字符串形式加载包中的所有资源：

```json
{
  "packages": ["agent-core-skills", "@org/my-extension"]
}
```

对象形式筛选要加载的资源：

```json
{
  "packages": [
    {
      "source": "agent-core-skills",
      "skills": ["brave-search", "transcribe"],
      "extensions": []
    }
  ]
}
```

包管理详情见 [packages.md](packages.md)。

## 示例

```json
{
  "defaultProvider": "anthropic",
  "defaultModel": "claude-sonnet-4-20250514",
  "defaultThinkingLevel": "medium",
  "modelThinkingLevels": {
    "anthropic/claude-sonnet-4-20250514": "high"
  },
  "theme": "dark",
  "compaction": {
    "enabled": true,
    "reserveTokens": 16384,
    "keepRecentTokens": 20000
  },
  "retry": {
    "enabled": true,
    "maxRetries": 3
  },
  "warnings": {
    "anthropicExtraUsage": true
  },
  "packages": ["agent-core-skills"]
}
```

## 项目级覆盖

项目设置（`.agent-core/settings.json`）覆盖全局设置。嵌套对象会合并：

```json
// ~/.agent-core/agent/settings.json (global)
{
  "theme": "dark",
  "compaction": { "enabled": true, "reserveTokens": 16384 }
}

// .agent-core/settings.json (project)
{
  "compaction": { "reserveTokens": 8192 }
}

// Result
{
  "theme": "dark",
  "compaction": { "enabled": true, "reserveTokens": 8192 }
}
```
