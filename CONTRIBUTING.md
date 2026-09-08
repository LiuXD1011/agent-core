# Contributing to Pi Core

Pi Core is an independent secondary-development project based on
[Pi](https://github.com/earendil-works/pi) v0.85.1 (see [NOTICE.md](NOTICE.md)).
Contributions are welcome.

## Development setup

```bash
npm install           # Install all dependencies
npm run build         # Build all packages
npm test              # Run tests
npm run check         # Lint, format, and type check (also runs in pre-commit)
```

Set `PI_CORE_NO_LOCAL_LLM=1` to skip local-LLM integration tests on machines
with Ollama installed:

```bash
PI_CORE_NO_LOCAL_LLM=1 npm test
```

## Pull requests

- Keep the core minimal. Prefer extensions for features that are not core.
- Every commit must pass the pre-commit gate: formatting, lint, type check,
  dependency pinning, shrinkwrap, and install-lock checks.
- Lockfile changes are blocked unless intentional; commit with
  `PI_CORE_ALLOW_LOCKFILE_CHANGE=1` when dependency changes are deliberate.
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

- Pi Core version (`pi-core --version`) and how it was installed
- OS, Node.js version (`node --version`)
- Minimal reproduction steps and expected vs. actual behavior
