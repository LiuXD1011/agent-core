# Agent Core

Agent Core 是一个简洁、可扩展的终端编程 Agent 项目，由 LiuXD1011 独立维护。围绕 Agent Loop、上下文管理和工具调用组织核心能力，同时保留会话管理、导出与调试功能，便于使用、学习和二次开发。

* **CLI 命令**：`agent-core`
* **主 npm 包**：`@liuxuedeng/agent-core`

## 安装

```bash
npm install -g @liuxuedeng/agent-core
agent-core
```

需要 Node.js 22+。在交互会话中使用 `/login` 完成认证，或在启动前设置服务商 API 密钥环境变量（如 `ANTHROPIC_API_KEY`）。

也可以从源码运行：

```bash
git clone https://github.com/LiuXD1011/agent-core.git
cd agent-core
npm install --ignore-scripts
npm run build
./agent-core-test.sh
```

## 升级

```bash
npm install -g @liuxuedeng/agent-core@latest
```

版本变更参见 [CHANGELOG](packages/agent-app/CHANGELOG.md)。

## 包结构

| 包 | 说明 |
|---------|-------------|
| **[@liuxuedeng/agent-core-ai](packages/ai)** | 统一的多服务商 LLM API（OpenAI、Anthropic、Google 等） |
| **[@liuxuedeng/agent-core-agent](packages/agent)** | Agent 运行时：工具调用与状态管理 |
| **[@liuxuedeng/agent-core](packages/agent-app)** | 交互式编程 Agent CLI |
| **[@liuxuedeng/agent-core-tui](packages/tui)** | 差分渲染的终端 UI 库 |

原 Chord/Harness/SQLite 平台源码已归档到主仓库之外。

参见[架构与源码地图](docs/architecture/README.md)。核心教学路径是 Agent Loop（智能体循环）→ 模型上下文 → 工具执行；会话持久化与导出位于应用层。

## 项目特点

- **独立配置**：CLI 命令为 `agent-core`，配置位于 `~/.agent-core`，项目目录为 `.agent-core`，环境变量前缀为 `AGENT_CORE_*`。
- **独立发布**：所有包共享同一版本号（`0.1.0-alpha.1`）；通过变更日志记录功能与行为调整。
- **显式更新**：不上报安装/更新统计，除非配置了 `AGENT_CORE_VERSION_CHECK_URL`，否则不发起版本检查请求。自更新通过 npm 进行（`npm install -g @liuxuedeng/agent-core@latest`）。托管安装要求显式配置 Agent Core 发行源（`AGENT_CORE_INSTALLER_API_BASE`）。
- **请求归属**：CLI 与 RPC 入口设置 `AI_AGENT=agent-core`，HTTP User-Agent 为 `agent-core/<version>`，OpenCode 会话协议头标识会话与客户端。不自动注入 OpenRouter、Cloudflare、NVIDIA NIM 归属头；显式自定义头会被保留。

## 开发

```bash
npm install --ignore-scripts  # 安装依赖，不执行生命周期脚本
npm run build                 # 构建全部包
./test.sh                     # 运行非 e2e 测试（e2e 测试需配置端点/认证环境变量）
npm run check                 # Lint、格式化与类型检查
./agent-core-test.sh          # 从源码运行 agent-core（可在任意目录执行）
```

普通构建使用仓库中固定的模型目录，不联网刷新模型数据。需要更新模型时，执行 `npm run generate:models`，检查生成差异并通过构建、检查和测试后，再将模型数据与相关源码一起提交。详情参见[模型目录维护](packages/agent-app/docs/development.md#model-catalog-snapshot)。

## 权限与容器化

Agent Core 不内置限制文件系统、进程、网络或凭据访问的权限系统。默认情况下，它以启动它的用户和进程的权限运行。

如需更强的边界，请将 Agent 容器化或沙箱化。模式参见 [packages/agent-app/docs/containerization.md](packages/agent-app/docs/containerization.md)：

- **Gondolin 扩展**：Agent 与服务商认证留在宿主机，把内置工具和 `!` 命令路由进本地 Linux 微虚拟机。
- **纯 Docker**：将整个 Agent 进程跑在本地容器中，实现简单隔离。

## 供应链加固

npm 依赖变更按经过评审的代码变更处理：

- 直接外部依赖锁定到精确版本。内部 workspace 包保持版本区间。
- `.npmrc` 设置 `save-exact=true` 与 `min-release-age=2`，避免 npm 解析当天发布的依赖。
- `package-lock.json` 是依赖的唯一事实来源。除非设置 `AGENT_CORE_ALLOW_LOCKFILE_CHANGE=1`，pre-commit 会阻止意外的锁文件提交。
- `npm run check` 校验直接依赖锁定、原生 TypeScript 导入兼容性与生成的 coding-agent shrinkwrap。
- 发布的 CLI 包包含由根锁文件生成的 `packages/agent-app/npm-shrinkwrap.json`，为 npm 用户固定传递依赖。

## 致谢

- 感谢 [Pi](https://github.com/earendil-works/pi) 的作者与贡献者提供的基础代码。
- 感谢 [NVIDIA SoL-Pi](https://github.com/NVlabs/SoL-Pi) 提供的上下文管理与工具执行思路及代码。

来源与版权说明参见 [NOTICE.md](NOTICE.md)。

## 许可证

本项目采用 [MIT 许可证](LICENSE)。
