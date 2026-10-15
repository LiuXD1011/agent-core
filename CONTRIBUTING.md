# Contributing to Agent Core

Keep the core simple and changes focused. Preserve the notices in [NOTICE.md](NOTICE.md).

## Development

Use Rust **1.90.0** from the pinned toolchain. Windows builds require x64 MSVC C++ Build Tools and the Windows SDK. Run commands at the repository root:

```powershell
pwsh -File check.ps1 -Offline
pwsh -File test.ps1 -Offline
cargo test -p agent-runtime --test images --locked --offline
```

Unix equivalents are `./check.sh --offline` and `./test.sh --offline`. Checks run formatting verification and strict Clippy, not tests. Test scripts isolate home, credentials and session directories and clear model keys. Fixtures must remain offline, without real credentials or paid model calls. See [Rust usage](rust/README.md) and [migration differences](rust/MIGRATION.md).

## Changes and validation

- Read affected files in full and inspect installed APIs before editing.
- Run every added or changed test file and the root check. Review complete diagnostics.
- Pin direct external Cargo dependencies to exact versions, review `Cargo.lock`, and keep model snapshots offline and provenance-preserving.
- Install the native hook explicitly with `git config core.hooksPath .githooks`; validation never rewrites or stages files.
- Intentional staged `Cargo.lock` changes require `AGENT_CORE_ALLOW_LOCKFILE_CHANGE=1` at commit time.
- Preserve unrelated changes, released changelog history and license notices. Commit, tag and publish only when explicitly requested.

## Issue reports

Include Agent Core version, installation method, OS/architecture, Rust toolchain for source builds, minimal reproduction and expected versus actual behavior. Remove credentials from logs.
