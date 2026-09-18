# Development

See [AGENTS.md](https://github.com/LiuXD1011/agent-core/blob/main/AGENTS.md) for additional guidelines.

## Setup

```bash
git clone https://github.com/LiuXD1011/agent-core
cd agent-core
npm install --ignore-scripts
npm run build
```

Run from source:

```bash
/path/to/agent-core/agent-core-test.sh
```

The script can be run from any directory. Agent Core keeps the caller's current working directory. Pass `--no-env` to unset API-key environment variables for the run.

## Forking / Rebranding

Agent Core itself is configured via `package.json`:

```json
{
  "agentCoreConfig": {
    "name": "agent-core",
    "configDir": ".agent-core"
  }
}
```

Change `name`, `configDir`, and `bin` field for your own fork. Affects CLI banner, config paths, and environment variable names. Project identity uses `agentCoreConfig`; forks must rename their former `piConfig` key. This package metadata is separate from the unchanged `pi` resource-manifest key used by extension packages.

## Branding exceptions and non-maintained integrations

Names that are deliberately **not** rebranded, because they are protocols, historical inputs, or third-party contracts:

- **Source and license history:** upstream Pi copyright, `LICENSE`, and upstream issue links stay as-is.
- **Persistent namespaces `pi.*`:** storage value namespaces and fork policies are interdependent with stored data; renaming would make old sessions unreadable.
- **Resource manifest keys:** The `pi` resource manifest (`pi.extensions`, `pi.skills`, `pi.prompts`, `pi.themes`) are the configuration format; the `pi-package` npm keyword remains the ecosystem discovery convention.
- **`pi-messages` API** in `packages/ai`: an independent adapter protocol, kept independent of product naming.
- **`pi-managed-install` marker and update aliases:** the install-layout recognition marker and the `update pi` alias are compatibility protocols; changing them requires a coordinated writer/reader/test change.
- **OAuth originator/referrer fields** (OpenAI Codex, xAI): literal values that providers may contract on; verify the official contract before changing.
- **Third-party names** (pi-doom, pi.dev gallery, vendor highlight libraries, math `pi`) keep their real names.

Integrations this project does **not** actively verify: Termux/Android (page kept as-is, unverified), third-party managed sandboxes (excluded from the containerization page), and upstream services (the npm gallery, install statistics, release feeds — all contacts disabled).

## Path Resolution

Three execution modes: npm install, standalone binary, tsx from source.

**Always use `src/config.ts`** for package assets:

```typescript
import { getPackageDir, getThemeDir } from "./config.ts";
```

Never use `__dirname` directly for package assets.

## Debug Command

`/debug` (hidden) writes to `~/.agent-core/agent/agent-core-debug.log`:
- Rendered TUI lines with ANSI codes
- Last messages sent to the LLM

## Testing

```bash
./test.sh                         # Run all non-e2e tests from the repo root (no API keys needed)
```

E2E tests activate only when provider endpoint/auth environment variables are present; `./test.sh` skips them. To run a specific test, invoke vitest from the package root:

```bash
cd packages/agent-app
node ../../node_modules/vitest/dist/cli.js --run test/specific.test.ts
```

Never run the full vitest suite directly; it includes e2e tests that activate when endpoint/auth env vars are present.

### Published package smoke test

After building, run `npm run check:package-install`. It packs the public packages and installs only `@liuxuedeng/agent-core` as a direct dependency in a temporary directory outside the repository. Local tarball overrides select declared transitive dependencies without installing development-only packages. The check verifies SDK imports and CLI startup without credentials or model requests.

`npm run check` also checks runtime dependency declarations and rejects excluded development sources pulled into a package's build through imports.

## Project Structure

```
packages/
  ai/             # LLM provider abstraction
  agent/          # Agent loop and tool execution
  tui/            # Terminal UI components
  agent-app/      # CLI, context management, sessions, and exports
```
