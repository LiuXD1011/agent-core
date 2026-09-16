# Agent Core

Agent Core is an AI coding agent CLI, independently maintained as a secondary development of [Pi](https://github.com/earendil-works/pi) by earendil-works, based on the Pi v0.85.1 source code.

All credit for the original design and implementation goes to the [Pi authors](https://github.com/earendil-works/pi). Agent Core continues as an independent project with its own release identity:

* **CLI command**: `agent-core` (designed to coexist with an existing `pi` installation)
* **Main npm package**: `@liuxuedeng/agent-core`

## Install

```bash
npm install -g @liuxuedeng/agent-core
agent-core
```

Requires Node.js 22+. Authenticate with `/login` in the interactive session, or set a provider API key environment variable such as `ANTHROPIC_API_KEY` before starting.

Or run from source:

```bash
git clone https://github.com/LiuXD1011/agent-core.git
cd agent-core
npm install --ignore-scripts
npm run build
./agent-core-test.sh
```

## Upgrade

```bash
npm install -g @liuxuedeng/agent-core@latest
```

Each Agent Core release records the upstream Pi version it is based on: Agent Core 0.1.x is based on Pi v0.85.1. See [NOTICE.md](NOTICE.md) for the project relationship.

## Packages

| Package | Description |
|---------|-------------|
| **[@liuxuedeng/agent-core-ai](packages/ai)** | Unified multi-provider LLM API (OpenAI, Anthropic, Google, etc.) |
| **[@liuxuedeng/agent-core-agent](packages/agent)** | Agent runtime with tool calling and state management |
| **[@liuxuedeng/agent-core](packages/agent-app)** | Interactive coding agent CLI |
| **[@liuxuedeng/agent-core-tui](packages/tui)** | Terminal UI library with differential rendering |

The former Chord/Harness/SQLite platform sources have been archived outside the main repository.

See [architecture and source map](docs/architecture/README.md). The core teaching path is Agent Loop → model context → tool execution; session persistence and export live in the application.

## Differences from baseline Pi

Agent Core v0.1.x is based on Pi v0.85.1. Verified differences:

- **Coexistence:** the CLI is `agent-core` (the `pi` command of an upstream installation is untouched), configuration lives in `~/.agent-core` with the `.agent-core` project directory and `AGENT_CORE_*` environment variables, so an upstream `pi` installation can coexist on the same machine.
- **Independent releases:** all packages share one version (`0.1.0-alpha.1`); release history starts from the Agent Core changelog boundary, and inherited upstream changelog history is kept for provenance only.
- **No upstream contacts by default:** no install/update statistics are reported, and no version check request is made unless `AGENT_CORE_VERSION_CHECK_URL` is configured. Self-update goes through npm (`npm install -g @liuxuedeng/agent-core@latest`). Managed installs require an explicitly configured Agent Core release feed (`AGENT_CORE_INSTALLER_API_BASE`).
- **Request attribution:** the CLI and RPC entry points set `AI_AGENT=agent-core`, the HTTP User-Agent is `agent-core/<version>`, and OpenCode session protocol headers identify the session and client. Optional OpenRouter, Cloudflare, and NVIDIA NIM attribution headers are not injected automatically; explicit custom headers are preserved.

## Development

```bash
npm install --ignore-scripts  # Install dependencies without lifecycle scripts
npm run build                 # Build all packages
./test.sh                     # Run non-e2e tests (e2e tests activate with endpoint/auth env vars)
npm run check                 # Lint, format, and type check
./agent-core-test.sh          # Run agent-core from sources (can be run from any directory)
```

## Permissions & Containerization

Agent Core does not include a built-in permission system for restricting filesystem, process, network, or credential access. By default, it runs with the permissions of the user and process that launched it.

If you need stronger boundaries, containerize or sandbox the agent. See [packages/agent-app/docs/containerization.md](packages/agent-app/docs/containerization.md) for these patterns:

- **Gondolin extension**: keep the agent and provider auth on the host while routing built-in tools and `!` commands into a local Linux micro-VM.
- **Plain Docker**: run the whole agent process in a local container for simple isolation.

## Supply-chain hardening

npm dependency changes are treated as reviewed code changes (mechanisms inherited from upstream Pi):

- Direct external dependencies are pinned to exact versions. Internal workspace packages remain version-ranged.
- `.npmrc` sets `save-exact=true` and `min-release-age=2` to avoid same-day dependency releases during npm resolution.
- `package-lock.json` is the dependency ground truth. Pre-commit blocks accidental lockfile commits unless `AGENT_CORE_ALLOW_LOCKFILE_CHANGE=1` is set.
- `npm run check` verifies pinned direct deps, native TypeScript import compatibility, and the generated coding-agent shrinkwrap.
- The published CLI package includes `packages/agent-app/npm-shrinkwrap.json`, generated from the root lockfile, to pin transitive deps for npm users.

## Acknowledgments

Context management and tool-execution work incorporates ideas and MIT-licensed code from [NVIDIA SoL-Pi](https://github.com/NVlabs/SoL-Pi). Original copyright and license notices are preserved.

## License

MIT. Based on Pi by earendil-works — see [LICENSE](LICENSE).
