# Agent Core

Rust 原生编程 Agent，提供 Windows 桌面、CLI、TUI、JSONL RPC 和库入口。六个 crate 共用模型协议、工具和会话运行时；启动、构建与测试均使用 Cargo，不运行 Node.js、TypeScript 或 WebView。

## Windows 快速开始

需要 Rust **1.90.0**、Windows x64 MSVC C++ Build Tools 和 Windows SDK。

```powershell
pwsh -File check.ps1 -Offline
pwsh -File test.ps1 -Offline
pwsh -File rust/build-windows.ps1 -Action Package -Offline
.\dist\agent-core-desktop-windows-x64\agent-core-desktop.exe
.\dist\agent-core-desktop-windows-x64\agent-core.exe --help
```

首次获取固定依赖时省略 `-Offline`。本地工具链缓存存在时脚本优先使用它，否则使用 PATH 中的 Cargo。ZIP 含两个 EXE、使用说明、迁移对照与许可证；静态 CRT 构建无需另装 Visual C++ 运行库。

```powershell
# SHA256 为已下载包的实际哈希
pwsh -File rust/install-windows.ps1 -Action Install -Package C:\downloads\agent-core.zip -Sha256 SHA256 -InstallDirectory C:\apps\AgentCore
pwsh -File rust/install-windows.ps1 -Action Update -Package C:\downloads\agent-core.zip -Sha256 SHA256 -InstallDirectory C:\apps\AgentCore
pwsh -File rust/install-windows.ps1 -Action Uninstall -InstallDirectory C:\apps\AgentCore
```

安装器仅管理安装清单中的文件，更新替换托管文件，失败恢复原件；非托管文件与用户配置保留。卸载另外保留用户修改过的托管文件。可选签名校验与签名构建参数见 [Rust 使用说明](rust/README.md)。

## 使用

```powershell
$cli = '.\dist\agent-core-desktop-windows-x64\agent-core.exe'
& $cli -p '你好，Rust' --no-session
& $cli --list-models anthropic
& $cli --provider openai-compatible --base-url http://localhost:8000/v1 --model local-model -p '解释项目'
& $cli auth login openai-codex
& $cli auth status
& $cli --continue
& $cli --mode rpc
```

默认 Demo 为明确的离线演示。桌面端采用模型配置草稿与应用事务；认证、模型请求与扩展在工作线程执行。`Ctrl+Enter` 发送；停止或关闭窗口会取消并保存。CLI 无参数进入原生 TUI，`--line` 使用逐行界面。API 密钥不写入会话，Windows 持久凭据使用当前账户 DPAPI 加密。

完整命令、协议、云配置、资源、扩展与会话导出见 [Rust 使用说明](rust/README.md)。[迁移对照](rust/MIGRATION.md) 区分已实现功能和仍未对等的旧接口；原有 TypeScript 扩展不是 Rust 可执行扩展。旧源码与依赖的可核验回滚快照保存在本次工作区的 `.artifacts/rust-refactor-08`，不属于运行入口。

## 项目结构

| 路径 | 职责 |
| --- | --- |
| [agent-core](rust/crates/agent-core) | Agent Loop、工具调度、上下文预算、消息与事件 |
| [agent-runtime](rust/crates/agent-runtime) | 模型协议、认证、工具、会话树、资源、摘要与重试 |
| [agent-extensions](rust/crates/agent-extensions) | Rust trait SDK、注册表、hooks 与交互 broker |
| [agent-tui](rust/crates/agent-tui) | 原生编辑器、终端渲染、输入、剪贴板与恢复 |
| [agent-cli](rust/crates/agent-cli) | CLI/TUI host、JSONL RPC、认证与会话命令 |
| [agent-desktop](rust/crates/agent-desktop) | egui Windows 原生桌面端 |

根 `Cargo.toml` 与 `Cargo.lock` 是唯一工作区及依赖锁入口。开发约定见 [AGENTS.md](AGENTS.md) 和 [CONTRIBUTING.md](CONTRIBUTING.md)，执行边界见 [SECURITY.md](SECURITY.md)。

## 来源与许可证

- 感谢 [Pi](https://github.com/earendil-works/pi) 的作者与贡献者提供基础代码。
- 感谢 [NVIDIA SoL-Pi](https://github.com/NVlabs/SoL-Pi) 提供的上下文管理与工具执行代码。

保留原来源与版权声明，见 [NOTICE.md](NOTICE.md) 和 [MIT 许可证](LICENSE)。
