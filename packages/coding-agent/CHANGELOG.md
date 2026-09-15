# Changelog

## [Unreleased]

### Breaking Changes

- Renamed execution-efficiency configuration to efficiency.json, archive storage to efficiency/, and persisted record identifiers to the efficiency namespace. Previous names are no longer read; existing archive files remain untouched. Start fresh sessions after updating.
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

### Added

- Added opt-in built-in execution-efficiency mechanisms: fused mutation/validation, archived observations with exact recall, verified diagnostic-log reduction, and plan-boundary native compaction with continuation. Existing tool policies and custom tools take precedence.

- Initial Agent Core release.
