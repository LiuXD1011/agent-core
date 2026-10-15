# Rust 完全替换对照

## 第八轮原生工作区

六个 Rust crate 共享原生执行路径；CLI、TUI、RPC 和 Windows egui 桌面不调用 TypeScript/Node/WebView。根 `Cargo.toml`、`Cargo.lock`、check/test 脚本与 CI 切换为 Rust；旧包、npm 元数据及依赖的原始字节转入本工作区 `.artifacts/rust-refactor-08` 的哈希快照与回滚包。构建入口替换不等于旧 SDK 的每个接口都已对等，下表及剩余项分别记录实际实现。

| 原模块/能力 | 原生实现 | 已验证内容 |
| --- | --- | --- |
| Agent Loop、队列、事件、工具调度 | `agent-core` + `runtime.rs` | 生命周期、并发、取消、失败消息、请求上限、准备请求 hook 的默认行为 |
| 上下文预算/摘要/重试 | `context.rs` + `automatic.rs` + `retry.rs` | 真实目录预算、观测计划 horizon、cache 债务、工具批次边界、摘要失败回滚、白名单溢出恢复及请求级重试 |
| 39 服务商/1408 模型目录 | `catalog.rs` + `models.snapshot.json` | 仓库快照元数据保真；目录标记与认证/协议可用性分开 |
| Chat Completions、Responses、Codex、Anthropic、Gemini | `provider*.rs` | 原生 HTTP/SSE、图像、思考、工具、usage、断流/取消、provider/route/protocol/model 身份门控 |
| Mistral、Pi Messages、Cloudflare、Copilot 路由 | `provider_mistral.rs` / `provider_pi_messages.rs` / `provider_routed_auth.rs` | 两个独立协议的真实 HTTP/SSE、工具 call-ID 配对、图像/思考/cache/usage；Cloudflare account/gateway 与 Copilot enterprise/proxy 专用路由及认证头；原 TypeScript adapter 的零外网请求 oracle |
| 图像生成与目录 | `images.rs` + `image-models.snapshot.json` | 52 模型/完整元数据，原生 OpenRouter 请求/内嵌图片/usage/cost、payload/response hooks、自定义注册、取消、显式重试；原 adapter 零外网 oracle，保留未知价格哨兵 |
| 思考等级 | `ThinkingLevel` + `thinking.rs` + provider thinking | off/minimal/low/medium/high/xhigh/max、目录能力映射与向上优先 clamp、实际协议 payload、token 预算、分支条目与提交后通知；CLI/RPC/TUI/桌面共用逻辑 |
| Azure Responses、Vertex Gemini | `provider_azure.rs` + `provider_vertex.rs` | Azure endpoint/resource/deployment/API version/key/Bearer；Vertex project/location/key/Bearer、authorized_user/service_account ADC、RS256 验签离线 fixture、刷新缓存/取消与 route 身份 |
| Bedrock ConverseStream | `provider_bedrock.rs` + `provider_aws.rs` + `provider_bedrock_stream.rs` | Bearer/SigV4、静态/env/shared profile、区域/ARN/自定义 endpoint、CRC 二进制流、文本/思考/图像/工具/cache/usage；独立 AWS SDK 签名与帧离线 fixture |
| AWS STS 临时认证 | `provider_aws_credentials.rs` | AssumeRole/source_profile、WebIdentity/profile/env、Query/XML、真实 SigV4、Expiration、5 分钟刷新、来源指纹、可取消 singleflight、角色图循环/深度检测；实际 SDK 与 loopback oracle |
| AWS SSO / credential_process | `provider_aws_sso.rs` + `provider_aws_process.rs` | legacy / sso-session 缓存、GetRoleCredentials、OIDC refresh/旋转、跨进程锁/原子提交、未知字段与 Windows DACL 保留、实际 used-token 指纹；原生 shell、Job/进程组、取消/期限/双流限额；两者可成为 source_profile |
| AWS Environment / ECS / EC2 认证 | `provider_aws_metadata.rs` | credential_source 与默认链的原生 HTTP 路径、ECS/EKS URI/token、IMDSv2 token→role→credentials、受配置约束的 v1 fallback、取消/期限/cache 身份与 STS source 接线 |
| Vertex 联合与模拟身份 ADC | `provider_vertex_federation.rs` | external_account 文件/URL OIDC/SAML→STS→可选 IAM、external_account_authorized_user、递归 impersonated_service_account/delegates/lifetime/expiry；不写 ADC 原件 |
| Rust 原生扩展 SDK 与执行宿主 | `agent-extensions` + `extensions.rs` + `extension_process.rs` | tools/commands/before-start/context/tool hooks/lifecycle；argv 子进程 JSONL、Windows Job、帧/超时/取消限额、命令原子持久化与分支状态恢复 |
| 原始输入扩展 hooks | SDK `InputEvent` / `InputResult` + 共享 input 准备管线 | 注册命令优先；continue/handled/transform 链、图像保留/替换/清空、模板前变换；CLI/RPC/TUI/桌面单次准备、繁忙队列/取消、handled 跳过模型和消息存储 |
| 模型选择与恢复通知 | `ModelSelectSource` + `LifecycleEvent::ModelSelect` + native `model-selection` state | identity-only 分支持久化；CLI/RPC/TUI/桌面 Set/Restore 去重、提交前取消/提交后通知取消、普通错误非 veto；桌面草稿/应用/保存失败回滚，RPC 非流式选型状态 |
| Codex OAuth/凭据 | `auth.rs` + `auth_storage.rs` | PKCE/state/localhost callback、账户提取、取消、刷新旋转、跨进程锁、Windows DPAPI、显式只读导入旧凭据 |
| 其他原有五个 OAuth | `auth_native.rs` | Anthropic/OpenRouter PKCE/manual、Copilot/Kimi/xAI device、各自 refresh/API Key 语义、Copilot model/policy 与 enterprise、provider-specific 加密存储；CLI 与桌面使用真实工作线程 |
| 扩展阻塞对话 | SDK `UiBroker` + `extension_process.rs` + frontend dialogs | select/confirm/input/editor request-response、callback 等待返回值而非事后 DTO；超时/取消/关闭/卸载 scope 撤销、原生进程协议和 RPC/TUI/桌面宿主 |
| 会话信息与前置 hooks | SDK session + `session_tree.rs` | session_info 全局最新值、空值清除及导出重导入；session_before_switch / session_before_fork veto、提交后通知与保存失败原件不变 |
| read / write / edit / ls / grep / find | `builtins.rs` + `search.rs` | 图片读取、偏移截断、CRLF/BOM、多编辑、模糊匹配、diff、忽略规则 |
| 命令与进程收尾 | `process.rs` | Windows Job Object；取消/超时/后台后代/管道收尾 |
| 旧 JSONL v1–v3 与原生追加式树 | `legacy_session.rs` + `session_tree.rs` | ID/parent/leaf 校验、导航、从用户消息分叉、克隆活跃路径、书签、未知字段、配对工具与分支状态持久化 |
| HTML/JSONL 导出 | `html_export.rs` + `session_tree.rs` | 自包含转义 HTML、图像/思考/工具差异、树与完整归档下载；分支 JSONL 可重新导入 |
| 技能和 prompt 模板 | `resources.rs` | 真 YAML、用户/项目/显式路径、忽略规则、重名诊断、参数展开 |
| 显式摘要与原文归档 | `compaction.rs` | 完整用户轮/工具配对、失败不变更、持续归档及文件记录 |
| CLI、SDK 和 RPC | `agent-cli` / `agent-runtime` | 原生库入口、认证、异步请求/队列、树/导出命令及明确的新快照协议 |
| Rust TUI | `agent-tui` + `terminal_client.rs` | grapheme 编辑、多行/粘贴、选择、撤销/重做、历史/kill-ring、可配置快捷键、主题/搜索/滚动、流式事件及取消保存 |
| Windows 桌面 | `agent-desktop` | 模型配置/目录、原生认证、会话/树/导出、摘要/自动化、流式消息、图片/思考/工具、停止/关闭收尾 |
| Windows 安装、更新、卸载与可选签名 | `install-windows.ps1` / `sign-windows.ps1` | 固定六文件 ZIP、SHA256/x64 PE/路径校验、升级替换托管文件、真实文件共享失败回滚、卸载保留改过的托管文件、无关数据保留、PS5.1 实际子进程；默认包未签名，不声称实际证书签名或断电自动恢复 |

## 有意保留的迁移差异

- 旧 v1 JSONL ID 使用稳定 `legacy-N`，旧 flat Rust 会话使用稳定 `native-SESSION-INDEX`。重复 ID、断 parent、cycle、未知 leaf 返回错误，不静默选择另一分支；完整未知记录和旧兄弟分支保留。
- 导航不允许孤立 tool-call/result；分叉仅从用户节点取回草稿，并将活跃 leaf 指向其 parent。克隆使用新会话 ID，只包含当前路径及其相关归档/元数据，不改变原树。
- 手动摘要按完整用户轮次；自动压缩在请求/完整批次边界使用真实模型预算。未观测的未来轮数是 `None`，不是固定常量；计划、请求跨度和 cache 债务是树上的 runtime-state 元数据，不混入模型消息。
- 重试只针对一个 provider 请求。已有正文/思考/工具输出、取消和非瞬时错误不重试；HTTP 400/413 仅有白名单结构 code 时才视作上下文溢出。重试/摘要取消后保留原始会话与配对收尾。
- grep 使用 Rust regex，find 使用 globset，不跟随 directory junction/symlink。regex 不支持回溯和环视；搜索有取消及匹配/输出/遍历限额。
- 编辑优先精确匹配，再规范化匹配；多个 edit 在同一原文上校验，冲突/多匹配不写入。Windows 命令使用 PowerShell，不将其冒充 bash。
- 新 RPC 的 `message_update` 是完整 Rust 消息快照，协议为 `agent-core-rust`，不是旧 TS delta schema。忙时树查询是标注的 committed 快照，树变更等待 worker 收尾；以 `initialize` 清单为准。
- JSONL 消息导出仍可使用旧命令；分支导出另带 session header 和路径；原树、未知记录和原文归档以完整原生 JSON/HTML 保存。Windows 工作区由用户明确指定或调整。
- 签名重放同时检查 provider、实际完整请求 route、protocol、model。旧签名原始数据继续保留；缺少可验证 route/provider 的导入消息不会向未知端点重放签名，不猜测旧连接身份。
- OAuth 自动化测试使用 localhost 身份服务与临时凭据，没有真实账户授权或付费请求。原仓库六个 OAuth handler 分别实现，不能把它们的离线协议测试当成当前服务端授权验收；本树没有 Gemini/Antigravity OAuth handler，不将其虚列为旧功能缺口。
- Azure 的部署名不代表经过验证的底层目录模型：usage 标记 `model_identity_verified=false`，自动预算不借用部署同名的目录窗口或价格；用户可显式指定窗口，成本保持未知。
- Vertex ADC 支持 authorized_user、RSA PKCS8 service_account、external_account 文件/URL OIDC/SAML、external_account_authorized_user 与递归 impersonated_service_account，实际请求线程读取、交换并缓存令牌。AWS1/executable/certificate subject source 与完整 GoogleAuth metadata 发现仍未对等。自定义 base URL 保持原 COLLECTION 路径语义。
- Bedrock 已迁移 ConverseStream、Bearer/SigV4、STS、SSO 缓存、credential_process、credential_source Environment/ECS/EC2 与默认 metadata 链。有界 source_profile 图先校验，临时 AWS 凭据只驻留进程；SSO 已登录缓存沿原路径原子刷新。login_session、IMDS static-stability 行为及 Prompt Management ARN 等仍需对照；本树没有自动 MFA callback/SAML 公共选项，不将 SDK 的额外可能能力虚列成原功能。未知模型/ARN 不借用同名目录预算或价格。
- credential_process 遵循已安装 SDK 的 shell 命令语义（Windows ComSpec/cmd，Unix sh），不是 PowerShell 工具命令；只有凭据请求线程执行它。进程期限 30 秒，stdout/stderr 各 1MiB，非零状态或坏 JSON 不返回秘密。带 Expiration 时提前 5 分钟刷新；无到期时间时每次请求重新获取。完整静态 leaf 优先于 process/SSO，未迁移的认证字段仍不降级。
- SSO 支持已登录的 legacy URL 缓存与命名 sso-session 缓存；SSO region 与 Bedrock region 独立。现代注册仍有效时可刷新 OIDC，保留未知缓存字段及未返回的新 refreshToken；刷新提交前的失败/取消不写，提交后角色失败/取消保留已旋转缓存。实际发送的 token snapshot 绑定角色及后续 STS 缓存，不把 HTTP 期间外部新登录的 token 误标成旧凭据身份。首次交互登录仍是剩余项。
- model_select 是提交后通知，不是 veto。native model-selection 只存 provider/model，按当前分支恢复，不存 endpoint/秘密，也不猜旧导入模型元数据。CLI 显式选型覆盖恢复值；桌面未知自定义 provider 缺 transport 时要求重新应用配置。桌面选型只验证本地配置，CLI 的 Codex auth 检查在 worker，认证路径差异不伪装成全部旧应用命令已经对等。
- input hooks 是原始命令之后、资源展开之前的独立步骤；handled 不调用 provider，也不写假 Assistant。准备中的队列与模型 worker 都收尾后才发 RPC settled；已准备队列不再执行 hooks。该事件覆盖不代表旧 SDK 的全部会话/模型/UI 事件已经对等。
- 扩展使用显式 executable manifest 和版本化 JSONL 协议，不自动执行旧 JS/TS。Rust trait SDK 可直接注册；text/UI 不伪造 Assistant。四种阻塞对话已有原生 request-response；自定义 component/footer/header/editor/autocomplete/widget/theme、渲染器、shortcut/flag 注册、完整命令控制/provider hooks 与包管理等接口仍未全部对等。
- HTML 导出是无 JavaScript 的静态全树与原文折叠视图，含归档下载，不等同旧版 JS 搜索/Markdown 高亮/快捷键交互。
- 原生 TUI 使用 ratatui/crossterm；文本/思考/工具可显示，图片显示 MIME 占位。Kitty 图像/键盘协议、完整 Markdown 排版、全部旧 editor autocomplete/可访问性仍需对等实现。离线帧测试不等同每种实体终端的人工验收。

## 仍需对等的原有接口

1. AWS login_session/IMDS static-stability、Vertex AWS1/executable/certificate/metadata 发现、更多服务商缓存/采样/server/deferred/custom grammar 工具；Codex websocket/auto/cache/zstd 等传输路径。六个原有 OAuth 不代表这些其他认证/传输能力已经实现。
2. 原 package manager 的 git/local/npm 安装、更新、资源 bundle/scope/filter、网络目录更新与应用自更新；Windows 显式 ZIP 安装器不是该包管理器的等价实现。扩展高级组件、渲染器、shortcut/flag、provider 注册与全量宿主/会话/compaction/tree/project-trust 事件仍需迁移。旧 TS 扩展不会直接运行。
3. split-turn 压缩、全部模型 token/cost 差异、所有树/label/extension 事件与导出展示行为；真实终端及真实授权的单独验收。
4. 原 TUI 全部组件 API、Markdown 排版、Kitty 图像/键盘、autocomplete 与可访问性；真实终端、服务端授权与真实证书签名的独立验收。
5. 项目本地 `.agent-core` 资源与忽略的 `docs` 保留用户原件；其旧扩展示例不是生产 Rust 构建输入，不把删除它们当作实现其行为。

**根入口和运行栈已经切换为 Rust；上述未对等接口仍然是实际剩余工作。** 结果以代码、离线回归和 Windows 产物为准，不以删除文件冒充行为完成；保留原许可证及来源声明。
