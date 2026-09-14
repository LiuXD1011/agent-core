# Agent Core Documentation

Agent Core is a minimal terminal coding harness, independently maintained as a secondary development of [Pi](https://github.com/earendil-works/pi) (based on Pi v0.85.1). It is designed to stay small at the core while being extended through TypeScript extensions, skills, prompt templates, themes, and Agent Core packages.

## Quick start

Install Agent Core with npm:

```bash
npm install -g --ignore-scripts @liuxuedeng/agent-core
```

`--ignore-scripts` disables dependency lifecycle scripts during install. Agent Core does not require install scripts for normal npm installs.

To uninstall Agent Core:

```bash
npm uninstall -g @liuxuedeng/agent-core
```

For pnpm, Yarn, or Bun installs, use the matching global remove command: `pnpm remove -g @liuxuedeng/agent-core`, `yarn global remove @liuxuedeng/agent-core`, or `bun uninstall -g @liuxuedeng/agent-core`.

Then run it in a project directory:

```bash
agent-core
```

Authenticate with `/login` for subscription providers, or set an API key such as `ANTHROPIC_API_KEY` before starting Agent Core.

## Start here

- [Using Agent Core](usage.md) - install, authenticate, interactive mode, slash commands, sessions, context files, and CLI reference.
- [Models and Providers](models.md) - built-in providers, authentication, and custom models.
- [Security](security.md) - project trust, sandbox boundaries, and vulnerability reporting.
- [Containerization](containerization.md) - sandbox Agent Core with Gondolin or Docker.
- [Settings](settings.md) - global and project settings.
- [Keybindings](keybindings.md) - default shortcuts and custom keybindings.
- [Compaction](compaction.md) - context compaction and branch summarization.

## Customization

- [Extensions](extensions.md) - TypeScript modules for tools, commands, events, and custom UI.
- [Skills](skills.md) - Agent Skills for reusable on-demand capabilities.
- [Prompt templates](prompt-templates.md) - reusable prompts that expand from slash commands.
- [Themes](themes.md) - built-in and custom terminal themes.
- [Agent Core packages](packages.md) - bundle and share extensions, skills, prompts, and themes.
- [Custom providers](custom-provider.md) - implement custom APIs and OAuth flows.

## Programmatic usage

- [SDK](sdk.md) - embed Agent Core in Node.js applications.
- [RPC mode](rpc.md) - integrate over stdin/stdout JSONL; also covers the print JSON event stream.
- [TUI components](tui.md) - build custom terminal UI for extensions.

## Reference

- [Environment variables](environment-variables.md) - Agent Core process configuration and session metadata available to bash tools.
- [Session format](session-format.md) - JSONL session file format, entry types, and SessionManager API.

## Platform setup

- [Windows](terminal-setup.md#windows-shell-setup)
- [Termux on Android](termux.md)
- [tmux](terminal-setup.md#tmux)
- [Terminal setup](terminal-setup.md) - terminal configuration, Windows shell setup, tmux, and shell aliases.

## Development

- [Development](development.md) - local setup, project structure, and debugging.
