# Agent Core — Rust CLI / Windows Desktop

Rust 客户端直接执行原生 Agent Loop，不调用 Node.js、TypeScript、WebView 或旧 CLI。桌面端和命令行共享模型、工具、资源和会话运行时。根目录 Cargo 是唯一构建入口；旧源码与依赖转入可核验回滚证据，不再作为主项目入口。行为差异与剩余接口见 [迁移对照](MIGRATION.md)。

## 构建与运行

需要 Rust **1.90.0**、Windows x64 MSVC C++ Build Tools 和 Windows SDK。工具链、直接依赖和锁文件均固定版本。

```powershell
pwsh -File rust/build-windows.ps1 -Action Check -Offline
pwsh -File rust/build-windows.ps1 -Action Test -Offline
pwsh -File rust/build-windows.ps1 -Action Package -Offline
.\dist\agent-core-desktop-windows-x64\agent-core-desktop.exe
.\dist\agent-core-desktop-windows-x64\agent-core.exe --help
```

缓存未就绪时省略 `-Offline`。脚本优先使用项目本地 `.artifacts/vendor/rust-toolchain`，否则使用 PATH 中的 Cargo；只在当前进程配置 PATH，不安装工具。ZIP 包含桌面 EXE、CLI EXE、说明和许可证。静态 CRT 构建不另需 Visual C++ 运行库。

## Windows 桌面端

- 默认是明确标注的离线演示，不发模型请求，不伪造工具执行。
- 左栏配置工作区、协议、API Base URL、模型和密钥。模型目录可筛选选择，也可输入自定义服务的模型 ID；配置编辑只修改草稿，点击“应用模型配置”后才切换运行模型，撤销按钮恢复已应用配置。不同协议的草稿在本进程中分别保留。
- 已接入 OpenAI-compatible Chat Completions、OpenAI Responses、OpenAI Codex Responses、Anthropic Messages、Google Gemini、Azure Responses、Vertex Gemini、Bedrock ConverseStream、Mistral 和 Pi Messages 原生流式协议；图像、思考内容、function 工具调用与 usage 使用同一个 Rust 运行时。Cloudflare account/gateway 与 Copilot enterprise/proxy 配置使用各自实际请求路由和认证头。
- `read` / `ls` / `grep` / `find` 默认启用；勾选后才启用 `write` / `edit` / `run_command`。Windows 命令使用 PowerShell 和 Job Object 管理进程树。
- `Ctrl+Enter` 发送。快捷键在 `DEFAULT_APP_KEYBINDINGS` 中配置。运行中可插入指令，在工具批次后处理；停止和关窗会取消请求、收尾并保存会话。
- 输入准备失败或取消时恢复原始草稿；模型已经接受 User 后的请求失败则保存 User 与错误收尾，不再将它冒充未发送输入。
- 导入旧 `.jsonl` 会话时可指定分支 entry ID；空值选择最新叶子，原件不改。旧工作区路径会保留，跨系统时可在左栏修改后再运行。
- “压缩上下文”保留最近两个用户轮次，将旧轮次摘要并保存完整原文归档；摘要失败不改会话。此按钮需要配置真实模型服务。
- “自动上下文压缩”和“请求失败重试”默认关闭；目录模型带入真实窗口/成本，自定义型号需明确填写窗口。运行状态显示压缩/重试事件；手动与自动摘要账本都随所选分支保存。
- 登录支持 Codex、Anthropic、GitHub Copilot、OpenRouter、Kimi Coding 与 xAI。PKCE 浏览器回调或设备码使用独立工作线程；Anthropic/OpenRouter 可粘贴授权响应，取消/关闭会回收 callback 与等待。凭据默认位于 `%USERPROFILE%/.agent-core-rust/auth/credentials.dat`。

会话默认保存到 `%LOCALAPPDATA%/AgentCore/sessions`；桌面进程可用 `AGENT_CORE_SESSION_DIR` 指定隔离目录。原生会话采用追加式 parent/leaf 树，支持导航、从用户消息分叉、克隆当前分支及 HTML 导出。分支 JSONL 含可再次导入的 header 和完整路径；未知记录、旧分支和原文归档保留在原生 JSON 中。

API 密钥不写入会话。可设置 `AGENT_CORE_API_KEY` 或对应服务商环境变量；显式 `--api-key` / 桌面手填优先，其次为所选服务商持久凭据，再次为环境密钥。存储认证失败不会降级为另一来源。Windows 凭据使用 DPAPI 加密，跨进程锁保护刷新与 refresh-token 旋转。OpenRouter 授权交换为长期 API Key，不虚构 refresh token。`AGENT_CORE_AUTH_DIR` 或 CLI `--auth-dir` 指定独立目录。

## Rust CLI

```powershell
$cli = '.\dist\agent-core-desktop-windows-x64\agent-core.exe'
& $cli -p '你好，Rust' --no-session
& $cli --list-models anthropic
& $cli --provider anthropic --model claude-sonnet-4-6 -p '阅读 README.md 并解释架构'
& $cli --model google/gemini-2.5-flash --tools read,grep,find,ls -p '查找 TODO'
& $cli --provider openai-compatible --base-url http://localhost:8000/v1 --model local-model -p '解释项目'
& $cli --continue --compact --provider anthropic --model claude-sonnet-4-6
& $cli --import-session .\old-session.jsonl --leaf branch-id --cwd C:\work\project -p '继续任务'
& $cli --mode rpc
& $cli --tui-mode fullscreen
& $cli --tui-smoke-test
& $cli auth login openai-codex
& $cli auth login anthropic
& $cli auth login github-copilot
& $cli auth login github-copilot --enterprise-domain company.ghe.com
& $cli auth login openrouter
& $cli auth login kimi-coding
& $cli auth login xai
& $cli auth status
& $cli --model openai-codex/gpt-5.4 --auto-compact --retry -p '继续任务'
& $cli --session SESSION_ID --export-html .\session.html
& $cli --extension .\extension.json -p '/hello Rust'
& $cli --provider azure --model configured-model --azure-resource RESOURCE --azure-deployment DEPLOYMENT -p '解释项目'
& $cli --provider vertex --model gemini-2.5-flash --vertex-project PROJECT --vertex-location us-central1 --vertex-credentials .\adc.json -p '解释项目'
& $cli --provider bedrock --model us.anthropic.claude-sonnet-4-20250514-v1:0 --aws-region us-east-1 --aws-profile PROFILE -p '解释项目'
```

无参数且 stdin 是终端时进入 Rust TUI；regular 使用内联 viewport，fullscreen 使用备用屏幕。`--line` 保留逐行接口。TUI 支持 Unicode grapheme 编辑、多行/粘贴、选择、撤销/重做、kill/yank、历史、主题、搜索、滚动、流式输出和工具显示。Enter 发送，Shift+Enter 换行，Esc 停止，Ctrl+R 选择会话，Ctrl+Q 退出；Ctrl+C 复制选区，或取消任务/清空草稿，空草稿时退出。`--keybindings PATH` 加载 `{"overrides":{"submit":["ctrl+s"]}}`，快捷键定义集中在 `DEFAULT_EDITOR_KEYBINDINGS` 和 `DEFAULT_APP_KEYBINDINGS`。

TUI 的 `/tree`、`/navigate ID`、`/fork USER_ID`、`/clone`、`/export-html PATH` 与 `/export-jsonl PATH` 操作同一原生会话；运行中发送普通文本进入 steering 队列，树变更需等任务结束。`--mode json` 输出原生事件，`@file` 附加文本或图像。`--session ID` 精确恢复，`--continue` 按工作区找最近会话，`--session-dir` 指定目录。非 TUI 模式第一次 Ctrl-C 取消任务并保存，空闲或再次 Ctrl-C 退出。

`/model PROVIDER MODEL_ID` 在原生 CLI/TUI 中切换模型（Demo 可省略 MODEL_ID）。模型验证与通知在可取消 worker 执行，不当成付费 prompt；模型/会话操作期间的普通输入返回 busy。仅 `{provider,model}` 写入当前分支的 `model-selection` 状态，Base URL、密钥、OAuth 和云凭据不写入该状态。恢复原生会话/分支时读取这一状态；显式 CLI provider/model 优先，旧导入记录不猜模型。跨进程自定义服务仍须重新配置 transport/endpoint/凭据。

`--auto-compact` 默认关闭；启用后从所选目录模型读取 context window 和真实成本元数据，在完整工具批次边界检查压缩。`update_plan` 记录已完成计划步骤，用实际请求跨度估算剩余 horizon；缺少观测时只使用窗口防溢出，不虚构固定未来轮数。cache 债务和观测状态随原生分支保存。`--retry` 仅重试一个 provider 请求，不重放工具；取消、中途输出、非白名单错误都不会重复请求。摘要失败保留原上下文；HTTP 400/413 本身不等同上下文溢出。

技能从用户 `.agent-core/agent/skills`、项目 `.agent-core/skills` 与 `--skill` 路径加载；模板从对应 `prompts` 目录及 `--prompt-template` 路径加载。支持 YAML frontmatter、忽略文件、重名诊断、`/skill:NAME` 和模板参数。项目 `AGENTS.md` / `CLAUDE.md` 按祖先到工作区的顺序加入请求，不重复写入会话。`--no-skills` / `--no-prompt-templates` / `--no-context-files` 可关闭。

## 原生 JSONL RPC v1

stdin/stdout 按 LF 分帧，响应采用 `id` / `command` / `success` envelope。先调用 `initialize` 获取命令清单。事件的 `message_update` 是完整 Rust 消息快照，**不是旧 TypeScript 的 delta schema**；接入方须按 `protocol: agent-core-rust` 区分。

```json
{"id":"init","type":"initialize"}
{"id":"run","type":"prompt","message":"阅读 README.md"}
{"id":"status","type":"get_state"}
{"id":"stop","type":"abort"}
```

支持异步 prompt、steer / follow-up、取消、队列模式、状态/消息/usage、模型目录、切换模型、会话新建/切换/克隆/重命名、资源命令、旧树导入、树/分支查询及导航/分叉、HTML/分支 JSONL 导出及手动摘要。运行时另一个 prompt 必须设置 `streamingBehavior`。`compact` 的可选 `keepRecentUserTurns` 指定保留轮数。`abort` 响应后等待 `agent_settled`；EOF、Ctrl-C、输出管道关闭也会先取消、收尾和保存。未知命令返回明确错误，不返回伪成功。`initialize` 中的 command 清单是准确入口；忙时树查询返回标注的 committed 快照，不把前端观察消息伪装成已保存节点。

`set_model` 采用异步准备→提交→通知：`get_state.isSelectingModel` 表示选型/通知工作，`isStreaming` 仅表示模型请求；提交时发 `model_selected`（`committed:true`），完成后返回相关 id 的响应及 `agent_settled`。fork 始终经过可取消的前置 hook worker；navigate 需要恢复模型或思考等级时异步执行，其余 navigate 保持即时响应。取消发生在提交前时旧模型/分支不变；提交后取消仅终止通知，不撤销已经保存的选择。调用方以响应/settled 和 committed 状态区分，不把通知失败等同模型未切换。

RPC 启动的首次 `model_select` Restore 也在可取消 worker 中执行，期间仍读取查询、abort、shutdown 和 EOF。`isStartingModelNotification` 与 `pendingStartupCommandCount` 显示这段状态；最多暂存 32 个其他命令，启动成功后按原顺序执行，不额外发送无关 command 响应或成功 settled。启动通知取消时暂存命令返回错误，不成为模型输入。

## 原生扩展与云模型配置

`--extension PATH` 可重复，显式加载 manifest；`/extensions` 列出扩展和命令，`/reload` 在请求线程重新加载。TUI 自动补全原生命令；Windows 桌面提供独立加载/重载控件。命令返回的 text、notification/status 是 UI 输出，不写成模型回答；User/Custom 消息、会话名和 `extension:ID` 状态经校验后原子写入会话，状态随所选分支恢复。运行中的扩展命令可取消；关闭会回收原生子进程树。

```json
{"schema":"agent-core-native-extension/1","id":"hello","executable":"./hello.exe","args":[],"requestTimeoutMs":30000,"maxFrameBytes":1048576}
```

可执行文件按行读取 `{protocol:"agent-core-extension",version:1,id,method,params}`，按同一 id 回复 `type:"result",success:true,result`；工具流式更新使用 `type:"update"`。`initialize` 返回 `{id,tools,commands,hooks}`。方法包括 `command`、`execute`、`input`、`before_agent_start`、`context`、`before_tool_call`、`after_tool_call`、`session_before`、`lifecycle`。`session_before` 接收 `session_before_switch` 或 `session_before_fork` 事件，返回 `{"cancel":true}` 可否决尚未提交的操作。工具参数支持显式校验的 JSON Schema 子集；不支持的断言会在注册时报告，不假装已校验。源码中的 `agent-runtime` example `native_extension_fixture` 展示完整原生协议，`agent-extensions` example `native_greet` 展示可复用 trait SDK。旧 JS/TS 扩展、安装器及高级 UI API 仍见迁移清单。

`input` 在原始注册命令路由之后、技能/模板展开和模型预检之前运行。事件含 `text`、`images`、`source`（`interactive` / `rpc` / `extension`）和可选 `streamingBehavior`（`steer` / `followUp`）。返回 `{"action":"continue"}`、`{"action":"handled"}` 或 `{"action":"transform","text":"新文本","images":[]}`；transform 的 images 缺省/null 保留原图像，空数组清空。hook 按注册顺序执行，handled 结束处理，不调用模型或伪造会话消息；普通 hook 错误诊断后继续，取消则终止。变换出的斜杠文本不会重新执行扩展命令。

CLI、RPC、TUI 和桌面都使用同一输入准备管线。RPC/TUI/桌面的繁忙队列在独立可取消线程执行 hooks，排队消息只展开一次；模型先结束时也不重新执行 hooks。原始注册命令优先，不经过 input。RPC 的 handled 通知为 `{"type":"input_handled"}`；`agent_settled` 表示模型和输入准备线程均已收尾。RPC 的 steer/follow-up 在空闲或模型请求期间接收；会话切换、扩展命令/重载及手动摘要期间返回 busy，避免用即将替换的会话/注册表处理输入。

RPC 新增 `get_extensions`、`extension_command` 和 `reload_extensions`，`get_commands` 含扩展来源；命令/重载异步响应，`abort` 后仍须等待 `agent_settled`。

扩展可通过 `context.ui` 等待 select/confirm/input/editor 的真实返回值。RPC 发送 `extension_ui_request`，调用方用同一 id 发送 `extension_ui_response`，只带 `value`、`confirmed` 或 `cancelled:true` 之一；错误/过期/重复回复不完成对话。TUI 和桌面显示独立模态编辑器，不把对话答案伪造成会话消息。print/json 默认没有阻塞 UI。启动 SessionStart 在读取/UI loop 就绪后执行；取消、超时、EOF、关闭及旧 scope 退场会解除等待。

`--thinking LEVEL` / `/thinking LEVEL` 使用 off/minimal/low/medium/high/xhigh/max；`/thinking` 无参数查看当前值。RPC 用 `get_state.thinkingLevel`、`get_available_thinking_levels`、`set_thinking_level` 和 `cycle_thinking_level`。支持等级来自目录或 Rust 调用方显式能力；稀疏映射向上优先 clamp，不猜未知模型。选中等级是分支条目，保存后发提交通知；默认未选等级时保留原 provider 默认 payload。

`model_select` 复用原生 `lifecycle` 方法，事件为 `{type:"model_select",model:{provider,model},previousModel,source:"set"|"restore"}`；这里的 model 是无凭据的原生 identity DTO，不是旧 JS 的完整目录对象。同 provider/model 的 Set/Restore 去重，首次绑定可用 previousModel=null 发 Restore。事件在已提交配置之后通知，普通 hook 错误只产生诊断、不否决选择；挂起通知可取消并收尾原生子进程。桌面先验证草稿再提交与保存，保存失败恢复原配置/会话并保留待应用草稿；选择本身不调用模型、STS 或 ADC 交换。CLI 的 Codex 认证检查/刷新同样在可取消选型 worker 中执行。

```json
{"id":"extensions","type":"get_extensions"}
{"id":"hello","type":"extension_command","name":"hello","args":"Rust"}
{"id":"reload","type":"reload_extensions"}
```

Azure 使用 `AZURE_OPENAI_API_KEY` 或 `--azure-token`，二者互斥；配置 resource、endpoint、deployment 和 API version 后使用 Responses `/responses` 路由，deployment 在 payload 的 model 字段。部署底层模型未经验证，自动压缩需显式 `--context-window N`，可加 `--max-output-tokens N`；不从同名目录猜测窗口或价格。

Vertex 支持 API-key express、project/location + Bearer，以及 `--vertex-credentials PATH` / `GOOGLE_APPLICATION_CREDENTIALS` 指定的 authorized_user 或 RSA PKCS8 service_account ADC 文件；令牌交换只发生在可取消的请求线程。`GOOGLE_CLOUD_PROJECT` / `GCLOUD_PROJECT` 和 `GOOGLE_CLOUD_LOCATION` 提供默认配置。自定义 `--base-url` 保持旧 SDK COLLECTION 语义，若需要项目路径应在 URL 中完整填写。云凭据不写入会话。

Bedrock 使用原生 [ConverseStream](https://docs.aws.amazon.com/bedrock/latest/APIReference/API_runtime_ConverseStream.html) 二进制 AWS EventStream，校验 prelude/message CRC 和帧边界；支持文本、图像、thinking/redacted、工具调用/结果、cache 与 usage。模型 ID/inference-profile ARN 经路径编码后进入 `/model/ID/converse-stream`；实际 region、route、protocol、model 参与签名重放身份校验。

认证支持 `--bedrock-token` / `AWS_BEARER_TOKEN_BEDROCK`，或原生 [AWS SigV4](https://docs.aws.amazon.com/IAM/latest/UserGuide/reference_sigv-create-signed-request.html)。静态凭据由 `--aws-access-key-id` / `--aws-secret-access-key` / `--aws-session-token`、AWS 环境变量或只读 shared credentials/config profile 提供；路径可用 `--aws-credentials-file` / `--aws-config-file` 指定。`--aws-region` / `AWS_REGION` / `AWS_DEFAULT_REGION` 和 `--aws-profile` / `AWS_PROFILE` 配置区域与 profile；ARN 区域优先，自定义 `--base-url` 保持明确的代理端点。文件读取和认证只发生在可取消的请求线程，input handled 不触碰凭据。

Bedrock 还支持原生 STS [AssumeRole](https://docs.aws.amazon.com/STS/latest/APIReference/API_AssumeRole.html) 和 [AssumeRoleWithWebIdentity](https://docs.aws.amazon.com/STS/latest/APIReference/API_AssumeRoleWithWebIdentity.html)。profile 可用 `role_arn` + `source_profile`（静态凭据或有界角色链），或 `role_arn` + `web_identity_token_file`；可选 `role_session_name`、`external_id`、`duration_seconds`。也支持 `AWS_ROLE_ARN` / `AWS_WEB_IDENTITY_TOKEN_FILE` / `AWS_ROLE_SESSION_NAME`。实际 AWS Query/XML、STS SigV4、到期解析均为 Rust；近过期 5 分钟刷新，相同请求合并，等待者独立取消，来源文件/令牌/region/endpoint 变化使旧缓存失效。认证读取和交换仍只发生在模型请求线程，配置验证不执行它们。

`--aws-sts-endpoint URL` / `AWS_ENDPOINT_URL_STS` 可覆盖默认 region STS endpoint；Windows 草稿中有同样字段。自定义端点接受 HTTPS 或 loopback HTTP，不跟随重定向。角色图在请求前检测循环/深度，显式 unsupported 路径不退回无关静态凭据。

AWS [credential_process](https://docs.aws.amazon.com/sdkref/latest/guide/feature-process-credentials.html) 由选中 profile 的命令提供 Version 1 JSON。Windows 遵循已安装 SDK 的 ComSpec/cmd `/d /s /c` 命令串语义，Unix 使用 sh；不调用 Node.js，也不将它误当成 PowerShell 工具命令。执行目录为客户端进程的当前目录，建议配置绝对可执行文件路径。只有模型请求线程执行凭据命令，选型、草稿应用、input handled 都不执行；Windows Job / Unix process group 负责取消及后代收尾。期限为 30 秒，stdout/stderr 分别限 1MiB；错误不回显命令、stderr 或凭据。可选 Expiration 使用真实日期并提前 5 分钟刷新；无 Expiration 时每次请求重新获取。

AWS [IAM Identity Center](https://docs.aws.amazon.com/sdkref/latest/guide/feature-sso-credentials.html) 支持已登录的 legacy profile 与 `sso-session` 缓存。默认读取 `~/.aws/sso/cache`；legacy 文件名取 start URL 的 SHA1，命名会话取 session name 的 SHA1。`--aws-sso-cache-dir PATH` 指定缓存目录，`--aws-sso-endpoint URL` / `AWS_ENDPOINT_URL_SSO` 与 `--aws-sso-oidc-endpoint URL` / `AWS_ENDPOINT_URL_SSO_OIDC` 分别覆盖 GetRoleCredentials / OIDC endpoint，仍只接受 HTTPS 或 loopback HTTP、不重定向。Windows 草稿提供同样字段；这些配置不写入会话。

```ini
[profile native-sso]
region = us-west-2
sso_session = native
sso_account_id = 111122223333
sso_role_name = Developer

[sso-session native]
sso_region = us-east-1
sso_start_url = https://example.awsapps.com/start

[profile native-process]
region = us-west-2
credential_process = "C:\tools\credentials.exe" --profile native

[profile assumed]
region = us-west-2
role_arn = arn:aws:iam::111122223333:role/Developer
source_profile = native-sso
```

GetRoleCredentials 用缓存 bearer token 获取短期 AWS 凭据，再由真实 SigV4 签模型请求；SSO region 不冒充 Bedrock region。现代会话近到期且客户端注册/refreshToken 有效时调用 OIDC；每 token 文件锁合并刷新，CAS 防覆盖外部登录，未知字段保留，Windows 私密临时文件与替换保留原 DACL。OIDC 验证后原子写入是提交点：提交前取消/失败保持原缓存；提交后角色请求失败或取消不回滚已旋转令牌。Windows 罕见替换异常保留可核验的原件备份，不覆盖外部新文件。

角色缓存指纹绑定实际发送的 token 字节及 endpoint/region/account/role，不读取请求结束后的任意磁盘 token 为旧凭据重新命名。process/SSO 可以作为 `source_profile` 的叶子，完整静态凭据叶子保持 SDK 的优先级。legacy 缓存过期及现代注册不可刷新时返回登录需求；SSO 首次浏览器/device 登录仍见剩余迁移项。

`--bedrock-max-tokens N`、`--bedrock-temperature 0..1`、`--bedrock-cache none|short|long` 控制请求；已知模型使用目录预算，实际请求输出上限另受目录上限与剩余上下文约束。未知模型启用自动压缩需显式窗口；`--max-output-tokens` 是预算字段，不替代请求的 `--bedrock-max-tokens`。Windows 桌面提供同样的字段，并区分预算最大输出与显式 max tokens。AWS Environment/ECS/EC2 的 credential_source 和默认 metadata 链已接入；login_session、IMDS static-stability 与 Prompt Management ARN 等仍见迁移清单，不把当前路径当成完整 SDK 的全部行为。

## 工作区分层

TUI 停止时会将尚未消费的排队文本还原为草稿，不自动发起新请求；超过单份 1MB 草稿容量的余项可用 `/queued` 取回。提交预检失败时保留输入。

| Crate | 职责 |
| --- | --- |
| `agent-core` | Agent Loop、工具执行器、请求与上下文预算计算、共享消息类型 |
| `agent-extensions` | 原生 Rust trait SDK、扩展注册表、命令/工具冲突、schema 校验与 hook 链 |
| `agent-runtime` | 原生模型协议与 OAuth、固定目录、内置工具、进程树、会话树/导出、资源、自动摘要/重试、共享执行与队列 |
| `agent-tui` | Unicode 编辑器、可配置快捷键、终端渲染/事件状态、原生剪贴板与终端恢复 |
| `agent-cli` | CLI 参数、原生 TUI/逐行 host、认证命令、JSONL RPC、Ctrl-C 与持久化收尾 |
| `agent-desktop` | egui Windows UI，直接调用共享运行时 |

模型快照保留原目录的 **39 个服务商、1408 个模型**及完整元数据；目录条目不等于对应协议/认证都已可用。`nativeProtocol` 标记只表示本轮原生协议路由。离线快照更新工具：

```powershell
cargo run -p agent-runtime --example update_catalog_snapshot -- exported.json rust/crates/agent-runtime/src/models.snapshot.json
cargo run -p agent-runtime --example update_catalog_snapshot -- --images exported-images.json rust/crates/agent-runtime/src/image-models.snapshot.json
```

工具校验输入结构并保存 `.bak`，不下载模型目录或依赖 TypeScript。另有 **52 个 OpenRouter 图像模型**快照；`images::ImagesRegistry` 的原生 `generate_images` 发送真实多模态 HTTP 请求并解析内嵌图片、正文与 usage/cost。远程图像 URL 不会被暗中下载；自定义 provider/hooks 有独立注册入口。未知价格哨兵保留原值，不编造正价格。

## 验证

```powershell
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked --offline -- -D warnings
pwsh -File test.ps1 -Offline
```

测试使用固定 provider / SSE、loopback HTTP/OAuth、临时 DPAPI 凭据、临时文件和本地命令，没有真实凭据或付费调用。桌面 EXE 的 `--smoke-test` 验证离线 Loop 和 egui 布局；`AGENT_CORE_SMOKE_REPORT` 指定文本结果路径。CLI `--tui-smoke-test` 输出离线 100×28 原生终端帧。

原生循环取消或错误时为未执行工具补齐结果，保持调用配对；默认 schema 校验覆盖结构子集，其余断言需工具自定义校验。grep 使用 Rust regex，不支持回溯/环视；find 使用 globset。详细对等差异见 [迁移对照](MIGRATION.md)。

## Windows 安装、更新与签名

`rust/install-windows.ps1` 需要明确 `-Action Install|Update|Uninstall` 与 `-InstallDirectory`；Install/Update 另需 ZIP 的 `-Package` 和 `-Sha256`。包只允许固定六个成员，校验 x64 PE、路径、大小、重复项和文档链接；更新替换托管文件，失败恢复已备份原件与清单。非托管文件及用户数据始终保留，卸载另外保留修改过的托管文件。

可选 `-RequireSignature` / `-SignerThumbprint` 强制核验签名；`rust/build-windows.ps1 -Action Package -SignerThumbprint CERT_SHA1 -TimestampServer URL` 使用本机证书私钥调用原生签名脚本。默认包没有签名；本轮离线测试验证签名校验失败的回滚，而不声称执行过真实证书签名。进程被强制终止时遗留的 nonce lock/backup 需要按实际目录人工恢复，不宣称自动断电恢复。

## 许可

保留项目及迁移来源的 MIT 版权声明，见 [LICENSE](LICENSE) 和 [NOTICE.md](../NOTICE.md)。
