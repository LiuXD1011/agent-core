# Contributing to Agent Core

Contributions to Agent Core are welcome. Keep the core simple and changes focused.
Source and copyright information is recorded in [NOTICE.md](NOTICE.md).

## Development setup

```bash
npm install --ignore-scripts  # Install dependencies without lifecycle scripts
npm run build                 # Build all packages
./test.sh                     # Run non-e2e tests (no API keys needed)
npm run check                 # Lint, format, and type check (also runs in pre-commit)
```

See [packages/agent-app/docs/development.md](packages/agent-app/docs/development.md) for the full development manual.

Set `AGENT_CORE_NO_LOCAL_LLM=1` to skip local-LLM integration tests on machines
with Ollama installed:

```bash
AGENT_CORE_NO_LOCAL_LLM=1 ./test.sh
```

## Pull requests

- Keep the core minimal. Prefer extensions for features that are not core.
- Every commit must pass the pre-commit gate: formatting, lint, type check,
  dependency pinning, shrinkwrap, and install-lock checks.
- Lockfile changes are blocked unless intentional; commit with
  `AGENT_CORE_ALLOW_LOCKFILE_CHANGE=1` when dependency changes are deliberate.
- Do not edit `CHANGELOG.md` files; entries are added by maintainers.

## Known test environment issues

The following upstream tests are environment-sensitive and may fail outside
native Linux:

- `test/clipboard-image.test.ts` — assumes a non-Wayland session; WSLg exposes
  Wayland, so the two assertions about `wl-paste` fail on WSL2.
- `test/auth-storage.test.ts` and `test/agent-session-concurrent.test.ts` —
  timing-sensitive; may time out under WSL2 load. They pass in isolation and on
  GitHub Actions runners.

## Issue reporting

Open issues on the GitHub issue tracker. Include:

- Agent Core version (`agent-core --version`) and how it was installed
- OS, Node.js version (`node --version`)
- Minimal reproduction steps and expected vs. actual behavior
