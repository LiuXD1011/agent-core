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
  "piConfig": {
    "name": "agent-core",
    "configDir": ".agent-core"
  }
}
```

Change `name`, `configDir`, and `bin` field for your own fork. Affects CLI banner, config paths, and environment variable names. The `piConfig` manifest key (and the `pi` resource-manifest key in extension packages) is kept as the upstream configuration format; product branding and the configuration format are separate concerns.

## Branding exceptions and non-maintained integrations

Names that are deliberately **not** rebranded, because they are protocols, historical inputs, or third-party contracts:

- **Source and license history:** upstream Pi copyright, `LICENSE`, historical changelog entries, and upstream issue links stay as-is.
- **Persistent namespaces `pi.*`:** storage value namespaces, fork policies, and telemetry schema names are interdependent with stored data; renaming would make old sessions unreadable.
- **Resource manifest keys:** `piConfig` in `package.json` and the `pi` resource manifest (`pi.extensions`, `pi.skills`, `pi.prompts`, `pi.themes`) are the configuration format; the `pi-package` npm keyword remains the ecosystem discovery convention.
- **`pi-messages` API** in `packages/ai`: an independent adapter protocol, kept independent of product naming.
- **`pi-managed-install` marker and update aliases:** the install-layout recognition marker and the `update pi` alias are compatibility protocols; changing them requires a coordinated writer/reader/test change.
- **OAuth originator/referrer fields** (OpenAI Codex, xAI): literal values that providers may contract on; verify the official contract before changing.
- **Share viewer:** `/share` uploads to an external viewer (`DEFAULT_SHARE_VIEWER_URL`); it is an external service, not self-hosted.
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
cd packages/coding-agent
node ../../node_modules/vitest/dist/cli.js --run test/specific.test.ts
```

Never run the full vitest suite directly; it includes e2e tests that activate when endpoint/auth env vars are present.

### Published package smoke test

After building, run `npm run check:package-install`. It packs the public packages and installs only coding-agent as a direct dependency in a temporary directory outside the repository. Local tarball overrides select declared transitive dependencies without installing development-only packages. The check verifies SDK imports and CLI startup without credentials or model requests.

`npm run check` also checks runtime dependency declarations and rejects excluded development sources pulled into a package's build through imports.

## Project Structure

```
packages/
  ai/             # LLM provider abstraction
  agent/          # Agent loop, harness, session persistence
  tui/            # Terminal UI components
  coding-agent/   # CLI and interactive mode
  chord/          # Application-composition runtime (services, replicated state, RPC, plugins)
  telemetry/      # Vendor-neutral telemetry contracts and typed schemas
  evals/          # Private model-backed behavioral evals
  session-backends/
    sqlite-node/  # Optional SQLite session backend
```
