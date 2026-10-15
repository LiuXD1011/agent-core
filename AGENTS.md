# Development Rules

## Scope and communication

- Keep the Agent core simple. Make only requested changes; ask before removing intentional functionality or adding compatibility layers.
- Read affected files in full and inspect existing behavior before editing. Check installed dependency types rather than guessing APIs.
- Answer questions first. Keep explanations concise: problem, concrete example, solution. State agreement or disagreement when responding to feedback. No emojis or filler in project contributions.
- Preserve unrelated working-tree changes. Ask for confirmation when a request conflicts with these rules unless explicitly authorized.

## Rust code

- The root Cargo workspace is the only application build entry. Production code and tests must not require Node.js or legacy TypeScript packages.
- Keep crates focused and shared behavior in agent-core/agent-runtime. Use typed structures and explicit errors; avoid panics on external input.
- Keep shortcuts configurable through DEFAULT_EDITOR_KEYBINDINGS or DEFAULT_APP_KEYBINDINGS.
- Keep secrets out of sessions, Debug output, status and error strings. Run authentication and process work on cancellable workers, not the UI thread.
- Model snapshots preserve metadata and provenance. Change the Rust snapshot updater and validated input instead of inventing model capabilities or prices.
- Native extensions use explicit executable manifests and the versioned Rust SDK/protocol. Do not claim automatic execution of old TypeScript extensions.

## Validation

- After code changes, run ./check.ps1 -Offline or ./check.sh --offline and show complete output. They run rustfmt verification and strict Clippy, not tests.
- Run every added or changed test file with cargo test -p PACKAGE --test TEST --locked --offline. Use offline fixtures and local faux providers, never real credentials or paid models.
- Use ./test.ps1 -Offline or ./test.sh --offline for the full isolated workspace suite.
- Documentation-only edits need link and diff checks. Reference issue numbers beside issue-regression tests.
- Write ad-hoc scripts to temporary files and remove them when done. Keep evidence and verified rollback snapshots separate from the application.
- The final pre-cutover npm run check output is retained in .artifacts/rust-refactor-08/NPM_CHECK.txt; npm tooling is not the Rust validation entry.

## Dependencies

- Pin direct external Cargo dependencies to exact versions. Review manifest and Cargo.lock diffs and installed dependency APIs.
- Dependency metadata changes require an offline lock refresh when possible, followed by --locked checks/tests. Inspect new build scripts and features.
- Do not bypass the Cargo.lock commit gate (AGENT_CORE_ALLOW_LOCKFILE_CHANGE=1) unless the user intends to commit those changes.

## Git and history

- Commit only when requested. Check git status, stage explicit paths, and include only your changes. Use informative feat, fix, or docs commit messages.
- Do not use git add ., git add -A, git reset --hard, git checkout ., git clean -fd, git stash, git commit --no-verify, or force push.
- Review PRs without switching the working tree unless requested. Resolve rebase conflicts only in your files; abort and ask if unrelated files conflict.
- Change only Unreleased changelog sections on main or PR branches. Preserve released history and upstream attribution.
- Release, tag, and publish only when explicitly requested. Keep package versions synchronized and preserve license notices.

See [CONTRIBUTING.md](CONTRIBUTING.md) for setup and [SECURITY.md](SECURITY.md) for execution boundaries.
