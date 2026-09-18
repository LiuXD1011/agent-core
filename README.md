# Agent Core

Agent Core 是一个简洁、可扩展的终端编程 Agent，围绕 **Agent Loop、上下文管理和工具调用** 构建，面向日常开发与 Agent 学习。

## 主要功能

- **编程协作**：连接多家模型服务，读取、编辑文件并执行命令。
- **会话管理**：保存和恢复对话，导出 HTML / JSONL，便于调试与复盘。
- **扩展与集成**：支持技能、扩展、交互终端、单次执行、SDK 和 RPC。

## 快速开始

需要 Node.js **22.19.0 或更高版本**。

```bash
npm install -g --ignore-scripts @liuxuedeng/agent-core
agent-core
```

启动后输入 `/login` 配置支持订阅登录的服务商，也可以在启动前设置服务商 API 密钥。输入 `/` 查看命令，使用 `/export` 导出会话。详见[使用指南](packages/agent-app/docs/usage.md)和[模型配置](packages/agent-app/docs/models.md)。

升级：`npm install -g --ignore-scripts @liuxuedeng/agent-core@latest`。

## 从源码开发

```bash
git clone https://github.com/LiuXD1011/agent-core.git
cd agent-core
npm install --ignore-scripts
npm run build
./agent-core-test.sh
```

```bash
npm run check    # 格式、类型及项目检查
bash ./test.sh   # 全量非 e2e 测试，无需模型 API 密钥
```

构建使用仓库内的固定模型快照。开发约定见 [CONTRIBUTING.md](CONTRIBUTING.md)，模型更新与构建说明见[开发文档](packages/agent-app/docs/development.md)。

## 项目结构

| 目录 | 职责 |
|---|---|
| [packages/agent](packages/agent) | Agent Loop、请求上下文与工具执行 |
| [packages/ai](packages/ai) | 模型目录、认证与多服务商 API |
| [packages/agent-app](packages/agent-app) | CLI、会话管理、上下文处理、内置工具与扩展 |
| [packages/tui](packages/tui) | 终端输入、布局与渲染组件 |

学习建议：先理解“模型 → 工具 → 结果 → 下一轮”的循环，再阅读上下文处理和应用层。参见[架构与源码地图](docs/architecture/README.md)。

## 文档

- [使用指南](packages/agent-app/docs/usage.md) · [设置](packages/agent-app/docs/settings.md) · [快捷键](packages/agent-app/docs/keybindings.md)
- [扩展](packages/agent-app/docs/extensions.md) · [技能](packages/agent-app/docs/skills.md) · [SDK](packages/agent-app/docs/sdk.md) · [RPC](packages/agent-app/docs/rpc.md)
- [完整文档](packages/agent-app/docs/index.md) · [版本变更](packages/agent-app/CHANGELOG.md)

Agent 会以当前用户权限操作文件和执行命令；需要隔离时，请使用沙箱或容器。参见[安全说明](SECURITY.md)与[容器化指南](packages/agent-app/docs/containerization.md)。

## 致谢

- 感谢 [Pi](https://github.com/earendil-works/pi) 的作者与贡献者提供的基础代码。
- 感谢 [NVIDIA SoL-Pi](https://github.com/NVlabs/SoL-Pi) 提供的上下文管理与工具执行思路及代码。

来源与版权说明参见 [NOTICE.md](NOTICE.md)。

## 许可证

本项目采用 [MIT 许可证](LICENSE)。
