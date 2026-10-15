[CmdletBinding()]
param(
    [Parameter(Mandatory)][ValidateSet('Install', 'Update', 'Uninstall')][string]$Action,
    [Parameter(Mandatory)][string]$InstallDirectory,
    [string]$Package,
    [string]$Sha256,
    [switch]$RequireSignature,
    [string]$SignerThumbprint
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
if ([Environment]::OSVersion.Platform -ne [PlatformID]::Win32NT) { throw 'This installer requires native Windows.' }
Add-Type -AssemblyName System.IO.Compression
$names = @('agent-core-desktop.exe', 'agent-core.exe', 'LICENSE', 'MIGRATION.md', 'NOTICE.md', 'README.md')
$manifestName = '.agent-core-install.json'
$lockName = '.agent-core-install.lock'
$target = [IO.Path]::GetFullPath($InstallDirectory)
if ([string]::IsNullOrWhiteSpace($InstallDirectory) -or $target.TrimEnd('\') -eq [IO.Path]::GetPathRoot($target).TrimEnd('\')) {
    throw 'Select an explicit application directory, not a drive root.'
}
if ($SignerThumbprint -and $SignerThumbprint -notmatch '^[A-Fa-f0-9]{40}$') { throw 'Signer thumbprint must contain 40 hexadecimal characters.' }
if ($RequireSignature -or $SignerThumbprint) {
    Import-Module -Name (Join-Path $PSHOME 'Modules/Microsoft.PowerShell.Security/Microsoft.PowerShell.Security.psd1')
}

function Assert-NoReparse([string]$Path) {
    $current = [IO.Path]::GetFullPath($Path)
    while ($current) {
        if (Test-Path -LiteralPath $current) {
            $item = Get-Item -LiteralPath $current -Force
            if ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) { throw 'Reparse-point install paths are rejected.' }
        }
        $parent = [IO.Path]::GetDirectoryName($current)
        if ($parent -eq $current) { break }
        $current = $parent
    }
}
function Managed-Path([string]$Name) {
    if ($Name -notin $names -and $Name -ne $manifestName -and $Name -ne $lockName) { throw 'Unknown managed installation path.' }
    $path = [IO.Path]::GetFullPath((Join-Path $target $Name))
    $prefix = $target.TrimEnd('\') + '\'
    if (-not $path.StartsWith($prefix, [StringComparison]::OrdinalIgnoreCase)) { throw 'Installation path escapes its target.' }
    Assert-NoReparse $path
    return $path
}
function File-Hash([string]$Path) {
    $stream = [IO.File]::OpenRead($Path)
    $algorithm = [Security.Cryptography.SHA256]::Create()
    try { return [BitConverter]::ToString($algorithm.ComputeHash($stream)).Replace('-', '').ToLowerInvariant() }
    finally { $algorithm.Dispose(); $stream.Dispose() }
}
function Read-Manifest {
    $path = Managed-Path $manifestName
    if (-not (Test-Path -LiteralPath $path)) { return $null }
    $item = Get-Item -LiteralPath $path
    if ($item.PSIsContainer -or $item.Length -gt 65536) { throw 'Invalid installation manifest.' }
    $value = [IO.File]::ReadAllText($path) | ConvertFrom-Json
    if ($value.schema -ne 'agent-core-install/1' -or $value.packageSha256 -notmatch '^[a-fA-F0-9]{64}$') { throw 'Invalid installation manifest.' }
    $seen = [Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
    foreach ($file in @($value.files)) {
        if ($file.path -notin $names -or $file.sha256 -notmatch '^[a-fA-F0-9]{64}$' -or -not $seen.Add($file.path)) {
            throw 'Invalid managed-file manifest entry.'
        }
    }
    if ($seen.Count -ne 6) { throw 'Installation manifest must describe the six release files.' }
    return $value
}
function Assert-PackageExe([string]$Path) {
    $stream = [IO.File]::OpenRead($Path)
    $reader = [IO.BinaryReader]::new($stream)
    try {
        if ($stream.Length -lt 128 -or $reader.ReadUInt16() -ne 0x5A4D) { throw 'Package executable is not a Windows PE image.' }
        $stream.Position = 60
        $offset = $reader.ReadUInt32()
        if ($offset -gt $stream.Length - 26) { throw 'Package executable PE header is invalid.' }
        $stream.Position = $offset
        if ($reader.ReadUInt32() -ne 0x4550 -or $reader.ReadUInt16() -ne 0x8664) { throw 'Package executable must target Windows AMD64.' }
        $stream.Position = $offset + 24
        if ($reader.ReadUInt16() -ne 0x20B) { throw 'Package executable must use PE32+.' }
    } finally { $reader.Dispose() }
    if ($RequireSignature -or $SignerThumbprint) {
        $signature = Get-AuthenticodeSignature -LiteralPath $Path
        if ($signature.Status -ne 'Valid' -or $null -eq $signature.SignerCertificate) { throw 'Package executable has no valid trusted Authenticode signature.' }
        if ($SignerThumbprint -and $signature.SignerCertificate.Thumbprint -ne $SignerThumbprint) { throw 'Package signer thumbprint differs from the requested certificate.' }
    }
}
function Remove-OwnedDirectory([string]$Path, [string]$Nonce) {
    if (-not $Path -or -not (Test-Path -LiteralPath $Path)) { return }
    Assert-NoReparse $Path
    $full = [IO.Path]::GetFullPath($Path)
    if (-not $full.StartsWith($target.TrimEnd('\') + '\.agent-core-', [StringComparison]::OrdinalIgnoreCase)) { throw 'Installer cleanup escaped its transaction target.' }
    $marker = Join-Path $full '.owned'
    if (-not (Test-Path -LiteralPath $marker) -or [IO.File]::ReadAllText($marker) -ne $Nonce) { throw 'Installer cleanup ownership marker differs.' }
    foreach ($item in Get-ChildItem -LiteralPath $full -Force -Recurse) {
        if ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) { throw 'Installer transaction contains an unexpected reparse point.' }
    }
    # Only this nonce-marked transaction directory is recursively removed.
    Remove-Item -LiteralPath $full -Recurse -Force
}

Assert-NoReparse $target
if (Test-Path -LiteralPath $target -PathType Leaf) { throw 'Installation target is a file.' }
$packageStream = $null
if ($Action -ne 'Uninstall') {
    if (-not $Package -or $Sha256 -notmatch '^[A-Fa-f0-9]{64}$') { throw 'Install/update requires a release ZIP and its expected SHA256.' }
    $packagePath = [IO.Path]::GetFullPath($Package)
    Assert-NoReparse $packagePath
    # Hold one non-writable/non-delete-shared handle from hash validation through
    # extraction, so replacing the archive after hashing cannot change its bytes.
    $packageStream = [IO.File]::Open($packagePath, [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::Read)
    try {
        $algorithm = [Security.Cryptography.SHA256]::Create()
        try { $hash = [BitConverter]::ToString($algorithm.ComputeHash($packageStream)).Replace('-', '').ToLowerInvariant() }
        finally { $algorithm.Dispose() }
        if ($hash -ne $Sha256.ToLowerInvariant()) { throw 'Release ZIP SHA256 differs from the expected hash.' }
        $packageStream.Position = 0
    } catch { $packageStream.Dispose(); throw }
} elseif ($Package -or $Sha256 -or $RequireSignature -or $SignerThumbprint) { throw 'Uninstall accepts only the explicit installation directory.' }

$createdTarget = -not (Test-Path -LiteralPath $target)
if ($Action -eq 'Uninstall' -and $createdTarget) { throw 'Installation directory does not exist.' }
$lockPath = Join-Path $target $lockName
$lock = $null
$stage = $null
$backup = $null
$nonce = [Guid]::NewGuid().ToString('N')
$changes = [Collections.Generic.List[object]]::new()
$completed = $false
$rollbackFailed = $false
$preserved = [Collections.Generic.List[string]]::new()
try {
    [IO.Directory]::CreateDirectory($target) | Out-Null
    Assert-NoReparse $target
    $lockPath = Managed-Path $lockName
    # Never overwrite an unrelated pre-existing lock filename. A live installer
    # retains this exclusive file until its transaction and rollback finish.
    $lock = [IO.File]::Open($lockPath, [IO.FileMode]::CreateNew, [IO.FileAccess]::ReadWrite, [IO.FileShare]::None)
    $old = Read-Manifest
    if (($Action -eq 'Update' -or $Action -eq 'Uninstall') -and $null -eq $old) { throw 'Update/uninstall requires a valid owned installation manifest.' }
    $oldNames = @()
    if ($null -ne $old) { $oldNames = @($old.files | ForEach-Object { $_.path }) }
    foreach ($name in $names) {
        $path = Managed-Path $name
        if (Test-Path -LiteralPath $path) {
            if ((Get-Item -LiteralPath $path).PSIsContainer) { throw 'A managed file path contains a directory.' }
            if ($name -notin $oldNames) { throw 'An unrelated file occupies a release filename; it will not be overwritten.' }
        }
    }
    $backup = Join-Path $target ".agent-core-backup-$nonce"
    [IO.Directory]::CreateDirectory($backup) | Out-Null
    [IO.File]::WriteAllText((Join-Path $backup '.owned'), $nonce)
    $new = $null
    if ($Action -ne 'Uninstall') {
        $stage = Join-Path $target ".agent-core-stage-$nonce"
        [IO.Directory]::CreateDirectory($stage) | Out-Null
        [IO.File]::WriteAllText((Join-Path $stage '.owned'), $nonce)
        $archive = [IO.Compression.ZipArchive]::new($packageStream, [IO.Compression.ZipArchiveMode]::Read, $true)
        try {
            $seen = [Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
            $total = 0L
            if ($archive.Entries.Count -ne 6) { throw 'Release ZIP must contain exactly six files.' }
            foreach ($entry in $archive.Entries) {
                $name = $entry.FullName
                if ($name -notin $names -or $name.IndexOfAny([char[]]@('/', '\', ':')) -ge 0 -or -not $seen.Add($name)) { throw 'ZIP path traversal, duplicate or unknown release entry.' }
                $kind = ($entry.ExternalAttributes -shr 16) -band 0xF000
                if ($kind -ne 0 -and $kind -ne 0x8000) { throw 'ZIP entry is not a regular file.' }
                if ($entry.ExternalAttributes -band 0x410) { throw 'ZIP reparse/directory entries are rejected.' }
                $total += $entry.Length
                if ($entry.Length -le 0 -or $entry.Length -gt 268435456 -or $total -gt 536870912) { throw 'Release ZIP entry exceeds the bounded package size.' }
            }
            foreach ($entry in $archive.Entries) {
                $output = [IO.File]::Open((Join-Path $stage $entry.FullName), [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::None)
                $reader = $null
                try {
                    $reader = $entry.Open()
                    $buffer = [byte[]]::new(65536)
                    $copied = 0L
                    while (($count = $reader.Read($buffer, 0, $buffer.Length)) -gt 0) {
                        $copied += $count
                        if ($copied -gt $entry.Length) { throw 'ZIP payload exceeds its declared bounded length.' }
                        $output.Write($buffer, 0, $count)
                    }
                    if ($copied -ne $entry.Length) { throw 'ZIP payload was truncated.' }
                } finally { if ($null -ne $reader) { $reader.Dispose() }; $output.Dispose() }
            }
        } finally { $archive.Dispose() }
        foreach ($name in $names[0..1]) { Assert-PackageExe (Join-Path $stage $name) }
        $encoding = [Text.UTF8Encoding]::new($false, $true)
        foreach ($name in $names[2..5]) { $null = $encoding.GetString([IO.File]::ReadAllBytes((Join-Path $stage $name))) }
        $readme = [IO.File]::ReadAllText((Join-Path $stage 'README.md'))
        if ($readme -notmatch '\]\(LICENSE\)' -or $readme -notmatch '\]\(NOTICE\.md\)') { throw 'Packaged README must link its local LICENSE and NOTICE.' }
        $files = @($names | ForEach-Object { [ordered]@{ path = $_; sha256 = File-Hash (Join-Path $stage $_) } })
        $new = [ordered]@{ schema = 'agent-core-install/1'; packageSha256 = $Sha256.ToLowerInvariant(); installedAt = [DateTime]::UtcNow.ToString('o'); files = $files }
        [IO.File]::WriteAllText((Join-Path $stage $manifestName), ($new | ConvertTo-Json -Depth 5), [Text.UTF8Encoding]::new($false))
    }
    foreach ($name in $names) {
        $path = Managed-Path $name
        if ($Action -eq 'Uninstall') {
            if (-not (Test-Path -LiteralPath $path)) { continue }
            $expected = @($old.files | Where-Object { $_.path -eq $name })[0].sha256
            if ((File-Hash $path) -ne $expected) { $preserved.Add($name); continue }
        }
        $change = [pscustomobject]@{ Name = $name; OldMoved = $false; NewMoved = $false }
        $changes.Add($change)
        if (Test-Path -LiteralPath $path) { [IO.File]::Move($path, (Join-Path $backup $name)); $change.OldMoved = $true }
        if ($Action -ne 'Uninstall') { [IO.File]::Move((Join-Path $stage $name), $path); $change.NewMoved = $true }
    }
    $manifestPath = Managed-Path $manifestName
    $change = [pscustomobject]@{ Name = $manifestName; OldMoved = $false; NewMoved = $false }
    $changes.Add($change)
    if (Test-Path -LiteralPath $manifestPath) { [IO.File]::Move($manifestPath, (Join-Path $backup $manifestName)); $change.OldMoved = $true }
    if ($Action -ne 'Uninstall') { [IO.File]::Move((Join-Path $stage $manifestName), $manifestPath); $change.NewMoved = $true }
    $completed = $true
    if ($Action -eq 'Uninstall') {
        Write-Output "UNINSTALLED: $target; modified managed files preserved: $($preserved -join ', ')"
    } else {
        Write-Output "$($Action.ToUpperInvariant()): $target; package SHA256 $($Sha256.ToLowerInvariant()); six files verified"
    }
} catch {
    $original = $_
    for ($index = $changes.Count - 1; $index -ge 0; $index--) {
        $change = $changes[$index]
        try {
            $path = Managed-Path $change.Name
            if ($change.NewMoved -and (Test-Path -LiteralPath $path)) { [IO.File]::Delete($path) }
            if ($change.OldMoved) { [IO.File]::Move((Join-Path $backup $change.Name), $path) }
        } catch { $rollbackFailed = $true }
    }
    if ($rollbackFailed) { throw "Installer failed; recoverable old files remain in $backup. Original error: $($original.Exception.Message)" }
    throw $original
} finally {
    if ($null -ne $packageStream) { $packageStream.Dispose() }
    $cleanupErrors = [Collections.Generic.List[string]]::new()
    try { Remove-OwnedDirectory $stage $nonce } catch { $cleanupErrors.Add($_.Exception.Message) }
    if (-not $rollbackFailed) {
        try { Remove-OwnedDirectory $backup $nonce } catch { $cleanupErrors.Add($_.Exception.Message) }
    }
    if ($null -ne $lock) {
        $lock.Dispose()
        try { Assert-NoReparse $lockPath; [IO.File]::Delete($lockPath) } catch { $cleanupErrors.Add($_.Exception.Message) }
    }
    if ((($createdTarget -and -not $completed) -or $Action -eq 'Uninstall') -and (Test-Path -LiteralPath $target -PathType Container)) {
        Assert-NoReparse $target
        if (@(Get-ChildItem -LiteralPath $target -Force).Count -eq 0) { [IO.Directory]::Delete($target) }
    }
    foreach ($message in $cleanupErrors) { Write-Warning "Installer transaction ended; retained cleanup artifact: $message" }
}
