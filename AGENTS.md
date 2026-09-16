# Development Rules

## Scope and communication

- Keep the Agent core simple. Make only requested changes; ask before removing intentional functionality or adding compatibility layers.
- Read affected files in full and inspect existing behavior before editing. Check installed dependency types rather than guessing APIs.
- Answer questions first. Keep explanations concise: problem, concrete example, solution. State agreement or disagreement when responding to feedback. No emojis or filler in project contributions.
- Preserve unrelated working-tree changes. Ask for confirmation when a request conflicts with these rules unless the user has explicitly authorized the override.

## Code

- Use strict, erasable TypeScript: no `enum`, parameter properties, namespaces, or other emit-only syntax. Avoid `any`.
- Use top-level imports only; no dynamic or inline type imports. Inline trivial helpers used once.
- Fix outdated dependency types by updating the dependency, not by removing working behavior.
- Keep shortcuts configurable through `DEFAULT_EDITOR_KEYBINDINGS` or `DEFAULT_APP_KEYBINDINGS`.
- Do not hand-edit `packages/ai/src/models.generated.ts`; change its generator and regenerate.

## Validation

- After code changes, run `npm run check`, show its full output, and fix all diagnostics. It does not run tests; documentation-only edits need link and diff checks instead.
- Run every test file you add or change. Use offline fixtures and the faux provider in `packages/agent-app/test/suite/harness.ts`; do not use real credentials or paid model calls in these tests.
- Run specific Vitest tests from their package directory:
  `node "$(git rev-parse --show-toplevel)/node_modules/vitest/dist/cli.js" --run test/specific.test.ts`
- For TUI tests, use `node --test test/specific.test.ts` from `packages/tui`.
- Use `./test.sh` for the full non-e2e suite. Do not run the full Vitest suite directly or invoke `npm test` / `npm run build` without a user request.
- Reference the issue number beside issue-regression tests. Write ad-hoc scripts to temporary files and remove them when done.

## Dependencies

- Pin direct external dependencies to exact versions. Review dependency and lockfile diffs; read release notes before updating `undici`.
- Install with `npm install --ignore-scripts` or `npm ci --ignore-scripts`. Run lifecycle scripts only when requested.
- After dependency metadata changes, refresh the root lockfile with `npm install --package-lock-only --ignore-scripts`.
- Regenerate coding-agent shrinkwrap with `node scripts/generate-coding-agent-shrinkwrap.mjs`; review and explicitly approve new lifecycle-script allowlist entries.
- Do not bypass the lockfile commit gate (`AGENT_CORE_ALLOW_LOCKFILE_CHANGE=1`) unless the user intends to commit those changes.

## Git and history

- Commit only when requested. Check `git status`, stage explicit paths, and include only your changes. Use informative `feat`, `fix`, or `docs` commit messages, optionally scoped to the affected package.
- Do not use `git add .`, `git add -A`, `git reset --hard`, `git checkout .`, `git clean -fd`, `git stash`, `git commit --no-verify`, or force push.
- Review PRs without switching the working tree unless requested. Resolve rebase conflicts only in your files; abort and ask if unrelated files conflict.
- Change only `Unreleased` changelog sections on `main` or PR branches. Read the section first; preserve released history and upstream attribution.
- Release, tag, and publish only when explicitly requested. Keep package versions synchronized and preserve license notices.

See [CONTRIBUTING.md](CONTRIBUTING.md) for setup and contribution guidance, and [SECURITY.md](SECURITY.md) for execution boundaries.
