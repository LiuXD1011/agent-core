# Changelog

## [Unreleased]

### Breaking Changes

- Renamed the application identity manifest key from `piConfig` to `agentCoreConfig`. Forks with custom package metadata must update that key; user settings, extension resource manifests, and session formats are unchanged.

- Removed install telemetry settings, the AGENT_CORE_TELEMETRY switch, and automatic OpenRouter/Cloudflare attribution headers. Explicit headers, OpenCode session headers, session history, HTML/JSONL export, and token/cost statistics remain supported.
- Moved the source package to `packages/agent-app` while retaining its npm name and CLI. Context configuration is now `context.json`, with `mutationCommands`, `resultReferences`, `logReduction`, and `boundaryCompaction` settings; session artifacts use `context/`. The project configuration has been migrated; other legacy configurations require manual migration. Existing archive files remain untouched.
- Renamed the package to `@liuxuedeng/agent-core` and the CLI binary to `agent-core`; update install commands and any scripts or wrappers that invoke the old binary name.
- Renamed the config directory to `~/.agent-core/` and the environment variable prefix to `AGENT_CORE_*`; the previous config directory is migrated automatically on startup.
- Removed the Radius gateway integration: the built-in `radius` provider and login entry, the `oauth: "radius"` models.json provider option, the `RADIUS_API_KEY` mapping, and the Radius session-share upload; legacy models.json `oauth` entries degrade to plain custom providers without network access.
- Removed the experimental remote-session architecture and the `@liuxuedeng/agent-core-client`, `@liuxuedeng/agent-core-protocol`, and `@liuxuedeng/agent-core-server` packages, including the `./client` and `./experimental/plugin` subpath exports and the experimental CLI. The local SDK, stdio RPC, extensions, and tool subprocess behavior are unchanged.
- Removed the online model catalog overlay: built-in providers expose only their static local catalogs and no refresh path contacts the catalog service; stale overlay caches in `models-store.json` are ignored. Local catalogs, custom models, and dynamic extension provider refreshes are unchanged.
- No longer injects the NVIDIA NIM `X-BILLING-INVOKE-ORIGIN` attribution header automatically; user-configured headers still apply.
- Removed the scoped models feature (`/scoped-models`): the built-in command, the `--models` CLI flag, the `enabledModels` setting, Ctrl+P model cycling and its `app.model.cycleForward`/`cycleBackward` keybinding actions, the `cycle_model` RPC command, `AgentSession.cycleModel()`/`scopedModels`/`setScopedModels()` and the `ModelCycleResult` type, the `resolveModelScope*` functions and the `ScopedModel` type, the `ExtensionContext.scopedModels` property, and the all/scoped tabs in the `/model` selector. Manual `/model` selection, model refresh, default model, thinking levels, `--model`, and `--provider` are unchanged. Legacy `enabledModels` entries are dropped from settings.json on save.
- Removed the built-in llama.cpp extension and its `/llama` command, the llama.cpp login guidance in interactive mode, and the `builtInExtensions` mechanism in `main.ts`. llama.cpp remains usable through a generic OpenAI-compatible custom provider. The project-local `/ir` (import-repro) extension is removed as well.
- Removed the `/share` command: the gist upload implementation, the `share` built-in slash command, and the `AGENT_CORE_SHARE_VIEWER_URL` environment variable with `getShareViewerUrl()`. Session HTML export (`/export`) is unchanged. The project-local `/wr`, `/is`, `/pr`, `/sa`, `/cl`, and `/deslop` prompt templates are removed as well.
- Removed every clipboard copy feature: the `/copy` built-in command, the `Ctrl+X` (`app.message.copy`) keybinding action, copying the selected message in `/tree`, the `fullscreenCopyOnSelect` setting and its `/settings` entry, and the exported `copyToClipboard()` helper. Clipboard paste (text and images) is unchanged.

### Removed

- Removed the advanced extension/SDK examples, game demonstrations, and unused deprecation helper. Minimal learning examples, regression fixtures, evaluation tooling, and core agent APIs remain.
- Removed the interactive animation/announcement Easter eggs, their commands, model-selection triggers, and bundled image. The decorative `ArminComponent` export is no longer available.

### Added

- 精简终端显示：字符像素字标、可展开的资源摘要、可配置快捷键提示和两行紧凑底栏；无需图片或图形依赖。

- Localized the user-facing interface and getting-started docs to Simplified Chinese: CLI help and command messages, interactive TUI (settings/model/session/tree selectors, status and error messages, keybinding descriptions), the exported session HTML (page chrome, dynamic labels, and accessibility titles), plus root/package READMEs and the usage, settings, models, keybindings, and session-format docs. Command names, config keys, model/tool identifiers, prompts, JSONL/RPC data, and user/model content stay unchanged.
- Integrated context preparation, tool-result reduction, artifact recall, and mutation validation into the normal session lifecycle. Explicit tool policies and custom tools take precedence.
- Added UI-independent `/sdk` and `/session/export` package entry points.

- Initial Agent Core release.

### Fixed

- 补齐启动帮助、资源列表、用量统计、设置值与 HTML 会话树的中文显示，统一使用“推理强度”；移除凭据选择和登录取消对显示文案的逻辑依赖。

- Fixed compaction success detection, event snapshot isolation, rapid credential changes, explicit tool allowlists, and archive-preserving JSONL/HTML export and import.
- Corrected relocated extension-loading and binary HTML-template paths.
