# 使用 Agent Core

本页是主要用户手册：涵盖安装、日常使用、会话与 CLI 参考。

## 安装与卸载

Agent Core 以 npm 包形式分发：

```bash
npm install -g --ignore-scripts @liuxuedeng/agent-core
```

`--ignore-scripts` 会在安装期间禁用依赖的生命周期脚本。正常的 npm 安装不需要 Agent Core 的安装脚本。

然后在你希望它工作的项目目录中启动：

```bash
cd /path/to/project
agent-core
```

### 卸载

使用安装 Agent Core 时所用的包管理器：

```bash
# npm install -g
npm uninstall -g @liuxuedeng/agent-core

# pnpm
pnpm remove -g @liuxuedeng/agent-core

# Yarn
yarn global remove @liuxuedeng/agent-core

# Bun
bun uninstall -g @liuxuedeng/agent-core
```

卸载 Agent Core 后，设置、凭据、会话和已安装的包仍保留在 `~/.agent-core/agent/` 中。

## 认证

Agent Core 可以通过 `/login` 使用订阅型服务商，或通过环境变量和认证文件使用 API 密钥型服务商。

- **订阅登录：** 启动 Agent Core 并运行 `/login`，然后选择服务商。内置的订阅登录包括 Claude Pro/Max、ChatGPT Plus/Pro（Codex）和 GitHub Copilot。
- **API 密钥：** 启动前设置 API 密钥，例如 `export ANTHROPIC_API_KEY=sk-ant-...`，然后运行 `agent-core`。也可以运行 `/login` 并选择 API 密钥型服务商，把密钥保存到 `~/.agent-core/agent/auth.json`。

所有支持的服务商、环境变量和云服务商配置见 [模型与服务商](models.md)。

## 交互模式

<p align="center"><img src="images/interactive-mode.png" alt="Interactive Mode" width="600"></p>

界面分为四个主要区域：

- **启动页眉** - 快捷键、已加载的上下文文件、提示模板、技能和扩展
- **消息** - 用户消息、助手回复、工具调用、工具结果、通知、错误和扩展 UI
- **编辑器** - 你的输入区域；边框颜色表示当前思考级别
- **页脚** - 工作目录、会话名称、Token/缓存用量、费用、上下文使用情况和当前模型。统计包含助手回复、工具上报的用量以及摘要生成。

编辑器可以被 `/settings` 等内置 UI 或自定义扩展 UI 临时替换。

### 编辑器功能

| 功能 | 操作 |
|---------|-----|
| 文件引用 | 输入 `@` 模糊搜索项目文件 |
| 路径补全 | 按 Tab 补全路径 |
| 多行输入 | Shift+Enter，Windows Terminal 中为 Ctrl+Enter |
| 图像 | 用 Ctrl+V 粘贴（Windows 上为 Alt+V），或拖入终端 |
| Shell 命令 | `!command` 执行并把输出发送给模型 |
| 隐藏 Shell 命令 | `!!command` 执行但不把输出发送给模型 |
| 外部编辑器 | Ctrl+G 依次打开 `externalEditor`、`$VISUAL`、`$EDITOR`、Windows 上的记事本或其他平台上的 `nano` |

所有快捷键与自定义方式见 [快捷键](keybindings.md)。

## 斜杠命令

在编辑器中输入 `/` 打开命令补全。扩展可以注册自定义命令，技能以 `/skill:name` 的形式提供，提示模板通过 `/templatename` 展开。

| 命令 | 说明 |
|---------|-------------|
| `/login`, `/logout` | 管理 OAuth 或 API 密钥凭据 |
| `/model` | 切换模型；在选择器中按 Ctrl+S 保存启动默认值 |
| `/thinking` | 切换思考级别；在选择器中按 Ctrl+S 保存启动默认值 |
| `/settings` | 主题、消息投递、传输方式及其他偏好设置 |
| `/resume` | 从历史会话中选择 |
| `/new` | 开始新会话 |
| `/name <name>` | 设置会话显示名称 |
| `/session` | 显示会话文件、ID、消息数、Token 和费用 |
| `/tree` | 跳转到会话中的任意位置并从那里继续 |
| `/trust` | 保存项目信任决定供后续会话使用 |
| `/fork` | 从之前的用户消息创建新会话 |
| `/clone` | 将当前活动分支复制为新会话 |
| `/compact [prompt]` | 手动压缩上下文，可附带自定义指令 |
| `/export [file]` | 将会话导出为 HTML 或 JSONL |
| `/import <file>` | 从 JSONL 文件导入并恢复会话 |
| `/reload` | 重新加载快捷键、扩展、技能、提示、主题和上下文文件 |
| `/hotkeys` | 显示所有键盘快捷键 |
| `/changelog` | 显示版本历史 |
| `/quit` | 退出 agent-core |

## 消息队列

Agent 仍在工作时你也可以提交消息：

- **Enter** 入队一条插话消息，在当前助手回合执行完工具调用后投递。
- **Alt+Enter** 入队一条追加消息，在 Agent 完成全部工作后投递。
- **Escape** 中止并把队列中的消息恢复到编辑器。
- **Alt+Up** 把队列中的消息取回编辑器。

在 Windows Terminal 中，Alt+Enter 默认切换全屏。如果想让 agent-core 接收该快捷键，请按 [终端配置](terminal-setup.md) 中的说明重新映射。

可在 [设置](settings.md) 中通过 `steeringMode` 和 `followUpMode` 配置投递方式。

## 会话

Agent Core 把对话保存为会话，方便你继续工作、从较早的回合创建分支，或回访之前的路径。

### 会话存储

会话自动保存到 `~/.agent-core/agent/sessions/`，按工作目录组织。每个会话是一个树状结构的 JSONL 文件。

```bash
agent-core -c                  # Continue most recent session
agent-core -r                  # Browse and select from past sessions
agent-core --no-session        # Ephemeral mode; do not save
agent-core --name "my task"    # Set session display name at startup
agent-core --session <path|id> # Use a specific session file or partial session ID
agent-core --fork <path|id>    # Fork a session file or partial session ID into a new session
```

在交互模式中使用 `/session` 查看当前会话文件、会话 ID、消息数、Token 和费用。

JSONL 文件格式与 SessionManager API 见 [会话文件格式](session-format.md)。

### 会话命令

| 命令 | 说明 |
|---------|-------------|
| `/resume` | 浏览并选择历史会话 |
| `/new` | 开始新会话 |
| `/name <name>` | 设置当前会话显示名称 |
| `/session` | 显示会话信息 |
| `/tree` | 浏览当前会话树 |
| `/fork` | 从之前的用户消息创建新会话 |
| `/clone` | 将当前活动分支复制为新会话 |
| `/compact [prompt]` | 摘要较旧的上下文；见 [上下文压缩](compaction.md) |
| `/export [file]` | 将会话导出为 HTML |

### 恢复与删除会话

`/resume` 打开当前项目的交互式会话选择器。`agent-core -r` 在启动时打开同样的选择器。

在选择器中可以：

- 输入文字搜索
- 用 Ctrl+P 切换路径显示
- 用 Ctrl+S 切换排序方式
- 用 Ctrl+N 只筛选已命名的会话
- 用 Ctrl+R 重命名
- 用 Ctrl+D 删除，然后确认

如果可用，agent-core 会使用 `trash` 命令行工具删除，而不是永久移除文件。

### 会话命名

使用 `/name <name>` 设置易读的会话名称：

```text
/name Refactor auth module
```

启动时用 `--name` 或 `-n` 设置名称：

```bash
agent-core --name "Refactor auth module"
agent-core --name "CI audit" -p "Review this build failure"
```

命名的会话在 `/resume` 和 `agent-core -r` 中更容易找到。

### 使用 `/tree` 分支

会话以树的形式存储。每个条目都有 `id` 和 `parentId`，当前位置是活动叶子节点。`/tree` 可以跳转到之前任意位置并从那里继续，无需创建新文件。

<p align="center"><img src="images/tree-view.png" alt="Tree View" width="600"></p>

形状示例：

```text
├─ user: "Hello, can you help..."
│  └─ assistant: "Of course! I can..."
│     ├─ user: "Let's try approach A..."
│     │  └─ assistant: "For approach A..."
│     │     └─ user: "That worked..."  ← active
│     └─ user: "Actually, approach B..."
│        └─ assistant: "For approach B..."
```

#### 树视图控制按键

| 按键 | 动作 |
|-----|--------|
| ↑/↓ | 在可见条目间移动 |
| ←/→ | 上/下翻页 |
| Ctrl+←/Ctrl+→ 或 Alt+←/Alt+→ | 折叠/展开，或在分支段之间跳转 |
| Shift+L | 为选中条目设置或清除标签 |
| Shift+T | 切换标签时间戳显示 |
| Enter | 选中条目 |
| Escape/Ctrl+C | 取消 |
| Ctrl+O | 循环切换筛选模式 |

筛选模式有：default、no-tools、user-only、labeled-only 和 all。可在 [设置](settings.md) 中用 `treeFilterMode` 配置默认值。

#### 选中行为

选中用户或自定义消息时：

1. 把叶子节点移到所选消息的父节点。
2. 把所选消息文本放入编辑器。
3. 你可以编辑并重新提交，从而创建新分支。

选中助手、工具、压缩或其他非用户条目时：

1. 把叶子节点移到该条目。
2. 编辑器保持为空。
3. 你可以从该位置继续。

选中根用户消息会把叶子节点重置为空对话，并把最初的提示放入编辑器。

### `/tree`、`/fork` 与 `/clone`

| 功能 | `/tree` | `/fork` | `/clone` |
|---------|---------|---------|----------|
| 输出 | 同一会话文件 | 新会话文件 | 新会话文件 |
| 视图 | 完整树 | 用户消息选择器 | 当前活动分支 |
| 典型用途 | 原地探索不同方案 | 从较早的提示开始新会话 | 继续之前先复制当前工作 |
| 摘要 | 可选的分支摘要 | 无 | 无 |

想把不同方案放在同一会话中就用 `/tree`；想要独立的会话文件就用 `/fork` 或 `/clone`。

### 分支摘要

当 `/tree` 从一个分支切换到另一个分支时，agent-core 可以摘要被离开的分支，并把摘要附加到新位置。这样无需重放整个分支，就能保留原路径的重要上下文。

出现提示时，可选择：

1. 不生成摘要
2. 用默认提示生成摘要
3. 用自定义关注点指令生成摘要

分支摘要的内部机制和扩展钩子见 [上下文压缩](compaction.md)。

## 上下文文件

Agent Core 启动时从以下位置加载 `AGENTS.md` 或 `CLAUDE.md`：

- `~/.agent-core/agent/AGENTS.md`，全局指令
- 父目录，从当前工作目录向上逐级查找
- 当前目录

如果某目录包含 `AGENTS.override.md`，Agent Core 会加载它，而不是该目录的 `AGENTS.md` 或 `CLAUDE.md`。其他目录的上下文文件仍正常叠加。

上下文文件用于记录项目约定、命令、安全规则和偏好。用 `--no-context-files` 或 `-nc` 禁用加载。

### 系统提示文件

用以下文件替换默认系统提示：

- `.agent-core/SYSTEM.md`（项目级）
- `~/.agent-core/agent/SYSTEM.md`（全局）

在这两个位置放置 `APPEND_SYSTEM.md` 可在不替换默认提示的情况下追加内容。

### 项目信任

交互模式启动时，如果项目文件夹包含项目级设置、资源或项目 `.agents/skills`，且 `~/.agent-core/agent/trust.json` 中没有该文件夹或其父文件夹的信任记录，Agent Core 会先询问是否信任该项目。信任项目后，agent-core 才能加载项目设置（`.agent-core/settings.json`）和 `.agent-core/` 下的项目资源、安装缺失的项目包，并执行项目扩展。

在做出信任决定之前，agent-core 只加载上下文文件、用户/全局扩展和 CLI `-e` 扩展，让它们能处理 `project_trust` 事件。项目本地扩展、项目包管理的扩展和项目设置只在项目被信任后才加载。切换到来自不同 cwd 且信任状态在当前进程中尚未确定的会话时，同样适用这一划分。

非交互模式（`-p`、`--mode json` 和 `--mode rpc`）不显示信任提示。在没有适用的已保存信任决定时，它们使用全局设置中的 `defaultProjectTrust`：`ask`（默认）和 `never` 会忽略这些项目资源，`always` 则信任它们。可传 `--approve`/`-a` 或 `--no-approve`/`-na` 为单次运行覆盖项目信任。

如果没有扩展或已保存的决定适用，`defaultProjectTrust` 决定回退行为。可在 `~/.agent-core/agent/settings.json` 中将其设为 `"ask"`、`"always"` 或 `"never"`，或通过 `/settings` 修改。

`agent-core config` 和包命令使用相同的项目信任流程，但 `agent-core update` 从不提示。传 `--approve` 为单条命令信任项目本地设置，或传 `--no-approve` 忽略它们。

在交互模式中使用 `/trust` 可保存项目信任决定供后续会话使用，包括对直接父文件夹的信任。它只写入 `~/.agent-core/agent/trust.json`；当前会话不会重新加载，需重启 agent-core 才能生效。


## 导出会话

使用 `/export [file]` 将会话写入 HTML。

If you use Agent Core for open source work and want to publish sessions for model, prompt, tool, and evaluation research, see [`badlogic/pi-share-hf`](https://github.com/badlogic/pi-share-hf). It publishes sessions to Hugging Face datasets.

## CLI 参考

```bash
agent-core [options] [--] [@files...] [messages...]
```

### 包命令

```bash
agent-core install <source> [-l]     # Install package, -l for project-local
agent-core remove <source> [-l]      # Remove package
agent-core uninstall <source> [-l]   # Alias for remove
agent-core update [source|self]      # Update agent-core only, or one package source
agent-core update --all              # Update agent-core and packages; reconcile pinned git refs
agent-core update --extensions       # Update packages only; reconcile pinned git refs
agent-core update --models           # Refresh model catalogs only
agent-core update --self             # Update agent-core only
agent-core update --extension <src>  # Update one package
agent-core list                      # List installed packages
agent-core config                    # Enable/disable package resources
```

这些命令管理 Agent Core 包，`agent-core update` 还可以更新 agent-core CLI 安装。要卸载 agent-core 本身，见 [卸载](#卸载)。`agent-core config` 和项目包命令接受 `--approve`/`--no-approve`，为单条命令信任或忽略项目本地设置。`agent-core update` 从不提示项目信任。

包来源与安全说明见 [Agent Core 包](packages.md)。

### 模式

| 标志 | 说明 |
|------|-------------|
| default | 交互模式 |
| `-p`, `--print` | 打印响应并退出 |
| `--mode json` | 以 JSON 行输出所有事件；见 [打印 JSON 事件流](rpc.md#print-json-event-stream) |
| `--mode rpc` | 基于 stdin/stdout 的 RPC 模式；见 [RPC 模式](rpc.md) |
| `--export <in> [out]` | 将会话导出为 HTML |

打印模式下，agent-core 还会读取管道输入的 stdin，并将其合并到初始提示中：

```bash
cat README.md | agent-core -p "Summarize this text"
```

### 模型选项

| 选项 | 说明 |
|--------|-------------|
| `--provider <name>` | 服务商，如 `anthropic`、`openai` 或 `google` |
| `--model <pattern>` | 模型匹配模式或 ID；支持 `provider/id` 和可选的 `:<thinking>` |
| `--api-key <key>` | API 密钥，覆盖环境变量 |
| `--thinking <level>` | `off`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max` |
| `--list-models [search]` | 列出可用模型 |

### 会话选项

| 选项 | 说明 |
|--------|-------------|
| `-c`, `--continue` | 继续最近的会话 |
| `-r`, `--resume` | 浏览并选择会话 |
| `--session <path\|id>` | 使用指定的会话文件或部分 UUID |
| `--fork <path\|id>` | 将会话文件或部分 UUID 分叉为新会话 |
| `--session-dir <dir>` | 自定义会话存储目录 |
| `--no-session` | 临时模式；不保存 |
| `--name <name>`, `-n <name>` | 启动时设置会话显示名称 |

### 工具选项

| 选项 | 说明 |
|--------|-------------|
| `--tools <list>`, `-t <list>` | 将指定的内置、扩展和自定义工具加入允许列表 |
| `--exclude-tools <list>`, `-xt <list>` | 禁用指定的内置、扩展和自定义工具 |
| `--no-builtin-tools`, `-nbt` | 禁用内置工具，但保留扩展/自定义工具 |
| `--no-tools`, `-nt` | 禁用所有工具 |

内置工具：`read`、`bash`、`powershell`（Windows）、`edit`、`write`、`grep`、`find`、`ls`。

### 资源选项

| 选项 | 说明 |
|--------|-------------|
| `-e`, `--extension <source>` | 从路径、npm 或 git 加载扩展；可重复 |
| `--no-extensions` | 禁用扩展发现 |
| `--skill <path>` | 加载技能；可重复 |
| `--no-skills` | 禁用技能发现 |
| `--prompt-template <path>` | 加载提示模板；可重复 |
| `--no-prompt-templates` | 禁用提示模板发现 |
| `--theme <path>` | 加载主题；可重复 |
| `--no-themes` | 禁用主题发现 |
| `--no-context-files`, `-nc` | 禁用 `AGENTS.md` 和 `CLAUDE.md` 发现 |

将 `--no-*` 与显式加载项组合，可忽略设置、只加载所需内容。例如：

```bash
agent-core --no-extensions -e ./my-extension.ts
```

### 其他选项

| 选项 | 说明 |
|--------|-------------|
| `--system-prompt <text>` | 替换默认提示；上下文文件和技能仍会追加 |
| `--append-system-prompt <text>` | 追加到系统提示 |
| `--tui-mode <mode>` | TUI 模式：`regular`（默认）或实验性的 `fullscreen` |
| `--use-theme <name[/name]>` | 为本次运行设置初始交互主题，不修改设置 |
| `--verbose` | 强制显示详细启动信息 |
| `-a`, `--approve` | 本次运行信任项目本地文件 |
| `-na`, `--no-approve` | 本次运行忽略项目本地文件 |
| `--` | 停止解析选项；其余参数视为提示或 `@file` 输入 |
| `-h`, `--help` | 显示帮助 |
| `-v`, `--version` | 显示版本 |

在 `fullscreen` 模式下，对话记录在终端视口内滚动，而队列消息、工作状态、扩展小部件、编辑器和页脚固定在底部。鼠标/触控板输入滚动指针所在区域；键盘视口操作始终可用。内联图像在支持 Kitty 图形协议的终端（如 Kitty 和 Ghostty）中可用。在 iTerm2 中它们渲染为文本占位符，因为其内联图像协议无法在应用接管滚动时删除或裁剪已放置的图像。`regular` 模式下，agent-core 使用主屏幕和终端自带的回滚缓冲，iTerm2 内联图像可正常渲染。终端相关的设置与变通方法见 [终端配置](terminal-setup.md)。

在 `/settings` 中设置 **TUI 模式**，可立即在 `regular` 和 `fullscreen` 之间切换并选择后续会话的默认值。**全屏退出输出** 控制退出全屏时是打印最终对话记录，还是恢复之前的屏幕并只打印会话恢复提示。

### 文件参数

在文件前加 `@` 前缀即可将其包含在消息中：

```bash
agent-core @prompt.md "Answer this"
agent-core -p @screenshot.png "What's in this image?"
agent-core @code.ts @test.ts "Review these files"
```

### 示例

```bash
# Interactive with initial prompt
agent-core "List all .ts files in src/"

# Non-interactive
agent-core -p "Summarize this codebase"

# Prompt beginning with a dash
agent-core -p -- "- Summarize these points"

# Non-interactive with piped stdin
cat README.md | agent-core -p "Summarize this text"

# Named one-shot session
agent-core --name "release audit" -p "Audit this repository"

# Different model
agent-core --provider openai --model gpt-4o "Help me refactor"

# Model with provider prefix
agent-core --model openai/gpt-4o "Help me refactor"

# Model with thinking level shorthand
agent-core --model sonnet:high "Solve this complex problem"

# Read-only mode
agent-core --tools read,grep,find,ls -p "Review the code"

# Disable one extension or built-in tool while keeping the rest available
agent-core --exclude-tools ask_question
```

## 设计原则

Agent Core 保持核心精简，把特定工作流的行为交给扩展、技能、提示模板和包。

它有意不内置 MCP、子 Agent、权限弹窗、计划模式、待办事项或后台 bash。这些工作流可以作为扩展或包自行构建或安装，也可以使用容器、tmux 等外部工具。

完整理由见这篇[博客文章](https://mariozechner.at/posts/2025-11-30-pi-coding-agent/)。
