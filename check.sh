#!/usr/bin/env bash
set -euo pipefail
root="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"
cd "$root"
network=()
for argument in "$@"; do
    case "$argument" in
        --offline) network=(--offline) ;;
        *) printf 'Usage: %s [--offline]\n' "$0" >&2; exit 2 ;;
    esac
done
local_toolchain="$root/.artifacts/vendor/rust-toolchain"
if [[ -x "$local_toolchain/cargo/bin/cargo" || -x "$local_toolchain/cargo/bin/cargo.exe" ]]; then
    export CARGO_HOME="$local_toolchain/cargo" RUSTUP_HOME="$local_toolchain/rustup"
    export PATH="$local_toolchain/cargo/bin:$PATH"
fi
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked "${network[@]}" -- -D warnings
