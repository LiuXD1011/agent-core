# 桌面模型服务

点击左下角 **模型服务 · 添加 / 快速切换**，或输入框下方的模型按钮。
选择服务，保存 API Key（Windows 使用现有 DPAPI 加密存储），再选择模型并点击
**保存并切换到此模型**。已保存连接可以一键切换，运行中的任务期间禁用修改。
此版本每个服务入口保存一份凭据和一个最近选择的模型；不是同厂商多账号管理器。

| 服务 | 认证 | 模型列表来源 |
| --- | --- | --- |
| DeepSeek | API Key | 官方 `/models`，需先保存密钥 |
| 智谱标准 API | API Key | Models.dev `zhipuai` 公开元数据 |
| 智谱 Coding Plan | 独立 API Key | Models.dev `zhipuai-coding-plan` 公开元数据 |
| OpenAI API | API Key | 官方 `/v1/models`，需先保存密钥 |
| ChatGPT / Codex | 复用已有浏览器 OAuth | 重新载入内置 Codex 目录，不是在线账号权限查询 |

智谱标准 API 与 Coding Plan 的端点及凭据记录分别保存；套餐可用于哪些工具由厂商决定。
DeepSeek 与智谱入口不收集账号密码。Codex 账号凭据不作为通用 OpenAI API Key 使用。
保留原有高级模型、Thinking、云凭据和其他厂商登录界面。

## 刷新与状态

- 打开面板只在后台读取本地连接；点击刷新才请求网络。所有操作均可取消。
- 保存密钥只验证格式并加密落盘；不自动发送付费测试对话。
- 厂商模型列表返回成功不代表所有条目都适合 Agent 对话或当前账号有调用额度。
- 智谱公开目录刷新不发送 API Key，也不验证账号权限；只保留明确声明的能力。
- 新模型可手动输入 ID；未知上下文/输出限制保持未知。启用自动压缩时按高级设置填写真实预算。
- 刷新保存来源、时间戳及原始模型元数据，保留当前选择；失败继续使用旧缓存。
- 远程元数据不得修改请求地址、登录地址或认证策略。重定向不自动跟随。
- 快速切换复用现有模型选择事务和扩展通知，不发送模型请求、不丢失输入草稿。
- 连接偏好和缓存保存在现有凭据存储的 `desktop-connection/<provider>` 记录中。
  API Key 继续使用原有 provider 记录；会话只记录模型 identity，不记录密钥。
- 操作取消时可能已完成一次原子保存，但不会继续切换会话；重新读取可确认保存状态。

## 维护

`agent-runtime/src/connections.rs` 管理固定服务预设、目录解析和可取消 HTTP；
`agent-desktop/src/app_connections.rs` 管理统一界面及后台任务。
新增同协议厂商时先扩展服务预设，再添加离线解析/认证隔离测试。
认证协议、工具调用及会话执行仍使用现有实现。

官方参考：
- [DeepSeek 模型接口](https://api-docs.deepseek.com/api/list-models/)
- [OpenAI 模型接口](https://developers.openai.com/api/reference/resources/models/methods/list)
- [OpenAI Codex 认证](https://learn.chatgpt.com/docs/auth)
- [智谱 API 文档索引](https://docs.bigmodel.cn/llms.txt)
- [智谱 Coding Plan](https://docs.bigmodel.cn/cn/coding-plan/overview)
- [Models.dev](https://models.dev/)

可复用开发指令：

```text
为统一模型服务添加一个厂商预设，复用现有调用协议及加密存储。
明确认证方式、固定端点和模型目录来源；保留未知能力，不推断账号权限。
为密钥隔离、刷新失败、取消和切换保留草稿添加离线测试，不使用真实凭据。
```
