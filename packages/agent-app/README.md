# Agent Core

Agent Core is an AI coding agent CLI — a minimal terminal coding harness independently maintained as a secondary development of [Pi](https://github.com/earendil-works/pi), based on Pi v0.85.1. It ships under its own name, package, and configuration namespace, so it can coexist with an existing `pi` installation.

<p align="center">
  <a href="https://www.npmjs.com/package/@liuxuedeng/agent-core"><img alt="npm" src="https://img.shields.io/npm/v/@liuxuedeng/agent-core?style=flat-square" /></a>
</p>

通过 TypeScript [扩展](docs/extensions.md)、[技能](docs/skills.md)、[提示模板](docs/prompt-templates.md)和[主题](docs/themes.md)适配你的工作流，并以 [Agent Core 包](docs/packages.md)的形式打包与共享。它支持交互模式、单次执行（`-p`）、JSON 事件流、RPC 子进程，以及嵌入式 SDK。

## 快速开始

```bash
npm install -g --ignore-scripts @liuxuedeng/agent-core
```

`--ignore-scripts` 会在安装期间禁用依赖的生命周期脚本。常规 npm 安装不需要安装脚本。

使用 API 密钥认证，或使用你已有的订阅：

```bash
export ANTHROPIC_API_KEY=sk-ant-...
agent-core
```

```bash
agent-core
/login  # 然后选择服务商
```

Agent Core 提供文件与 shell 工具（`read`、`write`、`edit`、`bash`），以及计划跟踪和存档结果召回。显式的工具选择始终优先。可通过技能、提示模板、扩展或包添加能力。

## 文档

- [使用 Agent Core](docs/usage.md) — 安装、认证、交互模式、斜杠命令、会话、上下文文件与 CLI 参考。
- [模型与服务商](docs/models.md) — 内置服务商、订阅、API 密钥与自定义模型。
- [设置](docs/settings.md)、[安全](docs/security.md)、[容器化](docs/containerization.md)、[终端设置](docs/terminal-setup.md)（Windows、tmux、shell 别名）。
- [扩展](docs/extensions.md)、[技能](docs/skills.md)、[提示模板](docs/prompt-templates.md)、[主题](docs/themes.md)、[Agent Core 包](docs/packages.md)、[自定义服务商](docs/custom-provider.md)。
- [上下文与工具行为](docs/context.md)。
- [SDK](docs/sdk.md)、[RPC 模式](docs/rpc.md)、[TUI 组件](docs/tui.md)。
- [环境变量](docs/environment-variables.md)、[会话格式](docs/session-format.md)、[快捷键](docs/keybindings.md)、[上下文压缩](docs/compaction.md)。
- 完整文档索引：[docs/index.md](docs/index.md)。

平台说明：[Windows](docs/terminal-setup.md#windows-shell-setup) | [Termux（Android）](docs/termux.md) | [tmux](docs/terminal-setup.md#tmux)

## 设计原则

Agent Core 保持核心精简：不内置 MCP、子 Agent、权限弹窗、独立计划模式或后台 bash。这些工作流可通过扩展、技能或包实现。参见 [docs/usage.md](docs/usage.md#design-principles)。

## 开发

参见 [CONTRIBUTING.md](../../CONTRIBUTING.md) 与 [docs/development.md](docs/development.md)。

## 无界面 SDK

使用 `@liuxuedeng/agent-core/sdk` 创建会话而不加载终端组件，使用 `@liuxuedeng/agent-core/session/export` 进行离线导出。根入口也为扩展暴露 UI API。

## 许可证

MIT. Agent Core is an independent secondary development of [Pi](https://github.com/earendil-works/pi), based on Pi v0.85.1. All credit for the original design and implementation goes to the Pi authors; see [NOTICE.md](../../NOTICE.md).

## 另见

- [@liuxuedeng/agent-core-ai](https://www.npmjs.com/package/@liuxuedeng/agent-core-ai)：核心 LLM 工具库
- [@liuxuedeng/agent-core-agent](https://www.npmjs.com/package/@liuxuedeng/agent-core-agent)：Agent 框架
- [@liuxuedeng/agent-core-tui](https://www.npmjs.com/package/@liuxuedeng/agent-core-tui)：终端 UI 组件
