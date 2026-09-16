# Agent Core

Agent Core is an AI coding agent CLI — a minimal terminal coding harness independently maintained as a secondary development of [Pi](https://github.com/earendil-works/pi), based on Pi v0.85.1. It ships under its own name, package, and configuration namespace, so it can coexist with an existing `pi` installation.

<p align="center">
  <a href="https://www.npmjs.com/package/@liuxuedeng/agent-core"><img alt="npm" src="https://img.shields.io/npm/v/@liuxuedeng/agent-core?style=flat-square" /></a>
</p>

Adapt Agent Core to your workflows with TypeScript [extensions](docs/extensions.md), [skills](docs/skills.md), [prompt templates](docs/prompt-templates.md), and [themes](docs/themes.md) — bundled and shared as [Agent Core packages](docs/packages.md). It runs interactively, one-shot (`-p`), as a JSON event stream or RPC subprocess, and as an embedded SDK.

## Quick start

```bash
npm install -g --ignore-scripts @liuxuedeng/agent-core
```

`--ignore-scripts` disables dependency lifecycle scripts during install. Agent Core does not require install scripts for normal npm installs.

Authenticate with an API key, or use your existing subscription:

```bash
export ANTHROPIC_API_KEY=sk-ant-...
agent-core
```

```bash
agent-core
/login  # Then select provider
```

Agent Core provides file and shell tools (`read`, `write`, `edit`, `bash`) together with plan tracking and stored-result recall. Explicit tool selections remain authoritative. Add capabilities via skills, prompt templates, extensions, or packages.

## Documentation

- [Using Agent Core](docs/usage.md) — install, authenticate, interactive mode, slash commands, sessions, context files, and CLI reference.
- [Models and Providers](docs/models.md) — built-in providers, subscriptions, API keys, and custom models.
- [Settings](docs/settings.md), [Security](docs/security.md), [Containerization](docs/containerization.md), [Terminal setup](docs/terminal-setup.md) (Windows, tmux, shell aliases).
- [Extensions](docs/extensions.md), [Skills](docs/skills.md), [Prompt templates](docs/prompt-templates.md), [Themes](docs/themes.md), [Agent Core packages](docs/packages.md), [Custom providers](docs/custom-provider.md).
- [Context and tool behavior](docs/context.md).
- [SDK](docs/sdk.md), [RPC mode](docs/rpc.md), [TUI components](docs/tui.md).
- [Environment variables](docs/environment-variables.md), [Session format](docs/session-format.md), [Keybindings](docs/keybindings.md), [Compaction](docs/compaction.md).
- Full documentation index: [docs/index.md](docs/index.md).

Platform notes: [Windows](docs/terminal-setup.md#windows-shell-setup) | [Termux (Android)](docs/termux.md) | [tmux](docs/terminal-setup.md#tmux)

## Design principles

Agent Core keeps the core small: no built-in MCP, sub-agents, permission popups, separate plan mode, or background bash. Build these workflows as extensions, skills, or packages. See [docs/usage.md](docs/usage.md#design-principles).

## Development

See [CONTRIBUTING.md](../../CONTRIBUTING.md) and [docs/development.md](docs/development.md).

## Headless SDK

Use `@liuxuedeng/agent-core/sdk` for session creation without loading terminal components, and `@liuxuedeng/agent-core/session/export` for offline exports. The root entry also exposes UI APIs for extensions.

## License

MIT. Agent Core is an independent secondary development of [Pi](https://github.com/earendil-works/pi), based on Pi v0.85.1. All credit for the original design and implementation goes to the Pi authors; see [NOTICE.md](../../NOTICE.md).

## See Also

- [@liuxuedeng/agent-core-ai](https://www.npmjs.com/package/@liuxuedeng/agent-core-ai): Core LLM toolkit
- [@liuxuedeng/agent-core-agent](https://www.npmjs.com/package/@liuxuedeng/agent-core-agent): Agent framework
- [@liuxuedeng/agent-core-tui](https://www.npmjs.com/package/@liuxuedeng/agent-core-tui): Terminal UI components
