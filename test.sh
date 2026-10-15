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
else
    export CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}" RUSTUP_HOME="${RUSTUP_HOME:-$HOME/.rustup}"
fi
test_parent="$root/.artifacts/native-tests"
mkdir -p "$test_parent"
test_root="$(mktemp -d "$test_parent/run.XXXXXXXX")"
readonly test_parent test_root
touch "$test_root/.native-test-owned"
cleanup() {
    status=$?
    trap - EXIT
    case "$test_root" in
        "$test_parent"/run.*)
            if [[ -d "$test_root" && ! -L "$test_root" && -f "$test_root/.native-test-owned" ]]; then
                rm -rf -- "$test_root"
            else
                printf 'Native test cleanup target failed workspace ownership verification.\n' >&2
                [[ $status -ne 0 ]] || status=1
            fi ;;
        *) printf 'Unexpected native test cleanup target.\n' >&2; [[ $status -ne 0 ]] || status=1 ;;
    esac
    exit "$status"
}
trap cleanup EXIT
mkdir -p "$test_root/home/config" "$test_root/home/cache" "$test_root/tmp" "$test_root/auth" "$test_root/sessions"
test_env=(
    "PATH=$PATH" "PWD=$root" "CARGO_HOME=$CARGO_HOME" "RUSTUP_HOME=$RUSTUP_HOME"
    "HOME=$test_root/home" "USERPROFILE=$test_root/home"
    "XDG_CONFIG_HOME=$test_root/home/config" "XDG_CACHE_HOME=$test_root/home/cache"
    "TMPDIR=$test_root/tmp" "TMP=$test_root/tmp" "TEMP=$test_root/tmp"
    "AGENT_CORE_AUTH_DIR=$test_root/auth" "AGENT_CORE_SESSION_DIR=$test_root/sessions"
    "LANG=C" "LC_ALL=C" "TZ=UTC" "GIT_CONFIG_NOSYSTEM=1"
    "GIT_CONFIG_GLOBAL=/dev/null" "GIT_TERMINAL_PROMPT=0" "AWS_EC2_METADATA_DISABLED=true"
)
for name in SystemRoot SYSTEMROOT WINDIR COMSPEC PATHEXT PROGRAMFILES CI GITHUB_ACTIONS RUSTC RUSTDOC RUSTFLAGS CARGO_TARGET_DIR INCLUDE LIB LIBPATH VCINSTALLDIR VCToolsInstallDir WindowsSdkDir; do
    value="${!name-}"
    [[ -z "$value" ]] || test_env+=("$name=$value")
done
program_files_x86="$(printenv 'ProgramFiles(x86)' || printenv 'PROGRAMFILES(X86)' || true)"
[[ -z "$program_files_x86" ]] || test_env+=("ProgramFiles(x86)=$program_files_x86")
printf 'Running native offline-fixture tests in isolated home: %s\n' "$test_root/home"
env -i "${test_env[@]}" cargo test --workspace --locked "${network[@]}"
