# Agent Core 文档

Agent Core 是简洁的终端编程 Agent，通过 TypeScript 扩展、技能、提示模板、主题和 Agent Core 包适配不同工作流。

## 快速开始

使用 npm 安装 Agent Core：

```bash
npm install -g --ignore-scripts @liuxuedeng/agent-core
```

`--ignore-scripts` 会在安装期间禁用依赖的生命周期脚本。常规 npm 安装不需要安装脚本。

卸载 Agent Core：

```bash
npm uninstall -g @liuxuedeng/agent-core
```

对于 pnpm、Yarn 或 Bun 安装，请使用对应的全局移除命令：`pnpm remove -g @liuxuedeng/agent-core`、`yarn global remove @liuxuedeng/agent-core` 或 `bun uninstall -g @liuxuedeng/agent-core`。

然后在项目目录中运行：

```bash
agent-core
```

订阅类服务商使用 `/login` 认证，或在启动 Agent Core 前设置 API 密钥（如 `ANTHROPIC_API_KEY`）。

## 从这里开始

- [使用 Agent Core](usage.md) - 安装、认证、交互模式、斜杠命令、会话、上下文文件与 CLI 参考。
- [模型与服务商](models.md) - 内置服务商、认证与自定义模型。
- [安全](security.md) - 项目信任、沙箱边界与漏洞报告。
- [容器化](containerization.md) - 使用 Gondolin 或 Docker 沙箱化 Agent Core。
- [设置](settings.md) - 全局与项目设置。
- [快捷键](keybindings.md) - 默认快捷键与自定义键位。
- [上下文压缩](compaction.md) - 上下文压缩与分支总结。

## 定制

- [扩展](extensions.md) - 提供工具、命令、事件和自定义 UI 的 TypeScript 模块。
- [技能](skills.md) - 可复用、按需调用的 Agent Skills。
- [提示模板](prompt-templates.md) - 从斜杠命令展开的可复用提示。
- [主题](themes.md) - 内置与自定义终端主题。
- [Agent Core 包](packages.md) - 打包并共享扩展、技能、提示与主题。
- [自定义服务商](custom-provider.md) - 实现自定义 API 与 OAuth 流程。

## 编程使用

- [SDK](sdk.md) - 在 Node.js 应用中嵌入 Agent Core。
- [RPC 模式](rpc.md) - 通过 stdin/stdout JSONL 集成；也涵盖 print JSON 事件流。
- [TUI 组件](tui.md) - 为扩展构建自定义终端 UI。

## 参考

- [环境变量](environment-variables.md) - Agent Core 进程配置，以及 bash 工具可用的会话元数据。
- [会话格式](session-format.md) - JSONL 会话文件格式、条目类型与 SessionManager API。

## 平台设置

- [Windows](terminal-setup.md#windows-shell-setup)
- [Android 上的 Termux](termux.md)
- [tmux](terminal-setup.md#tmux)
- [终端设置](terminal-setup.md) - 终端配置、Windows shell 设置、tmux 与 shell 别名。

## 开发

- [开发](development.md) - 本地搭建、项目结构与调试。
