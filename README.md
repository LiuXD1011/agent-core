# Pi Core

Pi Core is an AI coding agent CLI, independently maintained as a secondary development of [Pi](https://github.com/earendil-works/pi) by earendil-works, based on the Pi v0.85.1 source code.

All credit for the original design and implementation goes to the [Pi authors](https://github.com/earendil-works/pi). Pi Core keeps the full capability set of the Pi agent harness and continues as an independent project with its own release identity:

* **CLI command**: `pi-core` (designed to coexist with an existing `pi` installation)
* **Main npm package**: `@liuxuedeng/pi-core`

## Packages

| Package | Description |
|---------|-------------|
| **[@liuxuedeng/pi-core-chord](packages/chord)** | Standalone application-composition runtime for services, replicated state, RPC, and plugins |
| **[@liuxuedeng/pi-core-telemetry](packages/telemetry)** | Vendor-neutral telemetry contracts, reference adapter, conformance tests, and typed schemas |
| **[@liuxuedeng/pi-core-ai](packages/ai)** | Unified multi-provider LLM API (OpenAI, Anthropic, Google, etc.) |
| **[@liuxuedeng/pi-core-agent](packages/agent)** | Agent runtime with tool calling and state management |
| **[@liuxuedeng/pi-core](packages/coding-agent)** | Interactive coding agent CLI |
| **[@liuxuedeng/pi-core-tui](packages/tui)** | Terminal UI library with differential rendering |

Workspace package names are being migrated to the `@liuxuedeng/pi-core-*` namespace.

## Development

```bash
npm install           # Install all dependencies
npm run build         # Build all packages
npm test              # Run tests (skips LLM-dependent tests without API keys)
npm run check         # Lint, format, and type check
./pi-test.sh          # Run pi-core from sources (can be run from any directory)
```

## Permissions & Containerization

Pi Core does not include a built-in permission system for restricting filesystem, process, network, or credential access. By default, it runs with the permissions of the user and process that launched it.

If you need stronger boundaries, containerize or sandbox the agent. See [packages/coding-agent/docs/containerization.md](packages/coding-agent/docs/containerization.md) for three patterns:

- **Gondolin extension**: keep the agent and provider auth on the host while routing built-in tools and `!` commands into a local Linux micro-VM.
- **Plain Docker**: run the whole agent process in a local container for simple isolation.
- **OpenShell**: run the whole agent process in a policy-controlled sandbox.

## Supply-chain hardening

npm dependency changes are treated as reviewed code changes (mechanisms inherited from upstream Pi):

- Direct external dependencies are pinned to exact versions. Internal workspace packages remain version-ranged.
- `.npmrc` sets `save-exact=true` and `min-release-age=2` to avoid same-day dependency releases during npm resolution.
- `package-lock.json` is the dependency ground truth. Pre-commit blocks accidental lockfile commits unless `PI_CORE_ALLOW_LOCKFILE_CHANGE=1` is set.
- `npm run check` verifies pinned direct deps, native TypeScript import compatibility, and the generated coding-agent shrinkwrap.
- The published CLI package includes `packages/coding-agent/npm-shrinkwrap.json`, generated from the root lockfile, to pin transitive deps for npm users.

## License

MIT. Based on Pi by earendil-works — see [LICENSE](LICENSE).
