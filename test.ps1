[CmdletBinding()]
param([switch]$Offline)
$ErrorActionPreference = 'Stop'
$root = $PSScriptRoot
$local = Join-Path $root '.artifacts/vendor/rust-toolchain'
$before = @{}
foreach ($entry in [Environment]::GetEnvironmentVariables('Process').GetEnumerator()) { $before[$entry.Key] = $entry.Value }
$testParent = [IO.Path]::GetFullPath((Join-Path $root '.artifacts/native-tests'))
$testRoot = [IO.Path]::GetFullPath((Join-Path $testParent ([Guid]::NewGuid().ToString('N'))))
$marker = Join-Path $testRoot '.native-test-owned'
Push-Location $root
try {
    New-Item -ItemType Directory -Path $testRoot -Force | Out-Null
    New-Item -ItemType File -Path $marker | Out-Null
    if (Test-Path -LiteralPath (Join-Path $local 'cargo/bin/cargo.exe')) {
        $env:CARGO_HOME = Join-Path $local 'cargo'
        $env:RUSTUP_HOME = Join-Path $local 'rustup'
        $env:PATH = (Join-Path $local 'cargo/bin') + [IO.Path]::PathSeparator + $env:PATH
    } else {
        if (-not $env:CARGO_HOME) { $env:CARGO_HOME = Join-Path $HOME '.cargo' }
        if (-not $env:RUSTUP_HOME) { $env:RUSTUP_HOME = Join-Path $HOME '.rustup' }
    }
    if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) { throw 'Install the pinned Rust toolchain before running tests.' }
    foreach ($name in @([Environment]::GetEnvironmentVariables('Process').Keys)) {
        if ($name -match '^(AWS_|AGENT_CORE_|OPENAI_|ANTHROPIC_|AZURE_|GOOGLE_|GEMINI_)|(_API_KEY|_TOKEN|_SECRET)$') {
            [Environment]::SetEnvironmentVariable($name, $null, 'Process')
        }
    }
    $paths = @{
        HOME = 'home'; USERPROFILE = 'home'; LOCALAPPDATA = 'home/local'; APPDATA = 'home/roaming'
        XDG_CONFIG_HOME = 'home/config'; XDG_CACHE_HOME = 'home/cache'
        TMP = 'tmp'; TEMP = 'tmp'; TMPDIR = 'tmp'
        AGENT_CORE_AUTH_DIR = 'auth'; AGENT_CORE_SESSION_DIR = 'sessions'
    }
    foreach ($name in $paths.Keys) {
        $path = Join-Path $testRoot $paths[$name]
        New-Item -ItemType Directory -Path $path -Force | Out-Null
        [Environment]::SetEnvironmentVariable($name, $path, 'Process')
    }
    $env:AWS_EC2_METADATA_DISABLED = 'true'
    $env:GIT_TERMINAL_PROMPT = '0'
    $env:TZ = 'UTC'
    $network = @()
    if ($Offline) { $network = @('--offline') }
    Write-Output "Running native offline-fixture tests in isolated home: $(Join-Path $testRoot 'home')"
    & cargo test --workspace --locked @network
    if ($LASTEXITCODE -ne 0) { throw "cargo test failed (exit $LASTEXITCODE)" }
} finally {
    Pop-Location
    foreach ($name in @([Environment]::GetEnvironmentVariables('Process').Keys)) {
        if (-not $before.ContainsKey($name)) { [Environment]::SetEnvironmentVariable($name, $null, 'Process') }
    }
    foreach ($name in $before.Keys) { [Environment]::SetEnvironmentVariable($name, $before[$name], 'Process') }
    if (Test-Path -LiteralPath $testRoot) {
        $resolved = (Resolve-Path -LiteralPath $testRoot).Path
        $parent = $testParent.TrimEnd([IO.Path]::DirectorySeparatorChar) + [IO.Path]::DirectorySeparatorChar
        if (-not $resolved.StartsWith($parent, [StringComparison]::OrdinalIgnoreCase) -or
            -not (Test-Path -LiteralPath $marker) -or
            ((Get-Item -LiteralPath $testRoot).Attributes -band [IO.FileAttributes]::ReparsePoint)) {
            throw 'Native test cleanup target failed workspace ownership verification.'
        }
        Remove-Item -LiteralPath $resolved -Recurse -Force
    }
}
