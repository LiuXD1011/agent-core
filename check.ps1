[CmdletBinding()]
param([switch]$Offline)
$ErrorActionPreference = 'Stop'
$root = $PSScriptRoot
$local = Join-Path $root '.artifacts/vendor/rust-toolchain'
$before = @{}
foreach ($name in @('CARGO_HOME', 'RUSTUP_HOME', 'PATH')) {
    $before[$name] = [Environment]::GetEnvironmentVariable($name, 'Process')
}
Push-Location $root
try {
    if (Test-Path -LiteralPath (Join-Path $local 'cargo/bin/cargo.exe')) {
        $env:CARGO_HOME = Join-Path $local 'cargo'
        $env:RUSTUP_HOME = Join-Path $local 'rustup'
        $env:PATH = (Join-Path $local 'cargo/bin') + [IO.Path]::PathSeparator + $env:PATH
    }
    if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) { throw 'Install the pinned Rust toolchain before running checks.' }
    & cargo fmt --all --check
    if ($LASTEXITCODE -ne 0) { throw "cargo fmt failed (exit $LASTEXITCODE)" }
    $network = @()
    if ($Offline) { $network = @('--offline') }
    & cargo clippy --workspace --all-targets --locked @network -- -D warnings
    if ($LASTEXITCODE -ne 0) { throw "cargo clippy failed (exit $LASTEXITCODE)" }
} finally {
    Pop-Location
    foreach ($name in $before.Keys) { [Environment]::SetEnvironmentVariable($name, $before[$name], 'Process') }
}
