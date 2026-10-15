[CmdletBinding()]
param(
    [ValidateSet('Check', 'Test', 'Build', 'Package')]
    [string]$Action = 'Package',
    [switch]$Offline,
    [string]$SignerThumbprint,
    [string]$TimestampServer
)

$ErrorActionPreference = 'Stop'
if ($Action -in @('Build', 'Package') -and -not [string]::IsNullOrEmpty($env:CARGO_BUILD_TARGET)) {
    throw 'CARGO_BUILD_TARGET must be unset for this host-only Windows build/package entry; no Cargo command or output mutation was performed.'
}
if ($TimestampServer -and -not $SignerThumbprint) { throw 'A timestamp server requires an explicit signer thumbprint.' }
if (($SignerThumbprint -or $TimestampServer) -and $Action -in @('Check', 'Test')) { throw 'Signing options apply only to Build or Package.' }
$root = Split-Path -Parent $PSScriptRoot
$workspace = $root
$localToolchain = Join-Path $root '.artifacts/vendor/rust-toolchain'
if (Test-Path -LiteralPath (Join-Path $localToolchain 'cargo/bin/cargo.exe')) {
    $env:CARGO_HOME = Join-Path $localToolchain 'cargo'
    $env:RUSTUP_HOME = Join-Path $localToolchain 'rustup'
    $env:Path = (Join-Path $localToolchain 'cargo/bin') + ';' + $env:Path
}
if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    throw 'Install Rust 1.90.0 and the Visual Studio C++ Build Tools, then run this script again.'
}
if ([Environment]::OSVersion.Platform -ne [PlatformID]::Win32NT) {
    throw 'This packaging script targets native Windows. Use cargo directly on other platforms.'
}

Push-Location $workspace
try {
    $networkArgs = @()
    if ($Offline) { $networkArgs = @('--offline') }
    function Invoke-Cargo {
        param([string[]]$Arguments)
        & cargo @Arguments
        if ($LASTEXITCODE -ne 0) { throw "cargo failed (exit $LASTEXITCODE): $($Arguments -join ' ')" }
    }
    if ($Action -eq 'Check') {
        & (Join-Path $root 'check.ps1') -Offline:$Offline
    } elseif ($Action -eq 'Test') {
        & (Join-Path $root 'test.ps1') -Offline:$Offline
    } else {
        $hostInfo = (& rustc -vV) -join "`n"
        if ($LASTEXITCODE -ne 0 -or $hostInfo -notmatch 'host: x86_64-pc-windows-msvc') {
            throw 'This x64 package requires the x86_64-pc-windows-msvc Rust host toolchain.'
        }
        Invoke-Cargo -Arguments (@('build', '--release', '-p', 'agent-desktop', '-p', 'agent-cli', '--locked') + $networkArgs)
        if ($Action -eq 'Package' -or $SignerThumbprint) {
            $metadataText = & cargo metadata --no-deps --format-version 1 --locked @networkArgs
            if ($LASTEXITCODE -ne 0) { throw "cargo metadata failed (exit $LASTEXITCODE)" }
            $target = ($metadataText | ConvertFrom-Json).target_directory
            if ($SignerThumbprint) {
                & (Join-Path $PSScriptRoot 'sign-windows.ps1') -Directory (Join-Path $target 'release') -Thumbprint $SignerThumbprint -TimestampServer $TimestampServer
            }
        }
        if ($Action -eq 'Package') {
            $output = Join-Path $root 'dist/agent-core-desktop-windows-x64'
            New-Item -ItemType Directory -Path $output -Force | Out-Null
            Copy-Item -LiteralPath (Join-Path $target 'release/agent-core-desktop.exe') -Destination $output
            Copy-Item -LiteralPath (Join-Path $target 'release/agent-core.exe') -Destination $output
            Copy-Item -LiteralPath (Join-Path $root 'rust/README.md') -Destination $output
            Copy-Item -LiteralPath (Join-Path $root 'rust/MIGRATION.md') -Destination $output
            Copy-Item -LiteralPath (Join-Path $root 'rust/LICENSE') -Destination $output
            Copy-Item -LiteralPath (Join-Path $root 'NOTICE.md') -Destination $output
            $packagedReadme = Join-Path $output 'README.md'
            $text = [System.IO.File]::ReadAllText($packagedReadme).Replace('[NOTICE.md](../NOTICE.md)', '[NOTICE.md](NOTICE.md)')
            [System.IO.File]::WriteAllText($packagedReadme, $text)
            $archive = $output + '.zip'
            $packageFiles = @('agent-core-desktop.exe', 'agent-core.exe', 'README.md', 'MIGRATION.md', 'LICENSE', 'NOTICE.md') | ForEach-Object { Join-Path $output $_ }
            Compress-Archive -LiteralPath $packageFiles -DestinationPath $archive -Force
            Write-Output "EXE: $(Join-Path $output 'agent-core-desktop.exe')"
            Write-Output "CLI: $(Join-Path $output 'agent-core.exe')"
            Write-Output "ZIP: $archive"
        }
    }
} finally {
    Pop-Location
}
