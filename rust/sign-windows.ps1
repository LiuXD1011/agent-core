[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$Directory,
    [Parameter(Mandatory)][ValidatePattern('^[a-fA-F0-9]{40}$')][string]$Thumbprint,
    [ValidateSet('CurrentUser', 'LocalMachine')][string]$CertificateStore = 'CurrentUser',
    [string]$TimestampServer
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
if ([Environment]::OSVersion.Platform -ne [PlatformID]::Win32NT) { throw 'Authenticode signing requires native Windows.' }
Import-Module -Name (Join-Path $PSHOME 'Modules/Microsoft.PowerShell.Security/Microsoft.PowerShell.Security.psd1')
$directoryPath = [IO.Path]::GetFullPath($Directory)
if (-not (Test-Path -LiteralPath $directoryPath -PathType Container)) { throw 'Signing directory does not exist.' }
if ($TimestampServer) {
    $url = [Uri]$TimestampServer
    # Set-AuthenticodeSignature's underlying timestamp API requires HTTP even
    # on PowerShell versions that parse HTTPS. Never silently claim timestamping.
    if (-not $url.IsAbsoluteUri -or $url.Scheme -ne 'http' -or $url.UserInfo -or $url.Fragment) { throw 'Authenticode timestamp server must be an explicit HTTP URL without credentials or fragments.' }
}
$certificate = Get-Item -LiteralPath "Cert:\$CertificateStore\My\$Thumbprint"
$codeSigning = @($certificate.Extensions | Where-Object {
    $_ -is [Security.Cryptography.X509Certificates.X509EnhancedKeyUsageExtension]
} | ForEach-Object { $_.EnhancedKeyUsages } | Where-Object { $_.Value -eq '1.3.6.1.5.5.7.3.3' }).Count -gt 0
if (-not $certificate.HasPrivateKey -or $certificate.NotAfter -le [DateTime]::Now -or
    $certificate.NotBefore -gt [DateTime]::Now -or
    -not $codeSigning) {
    throw 'Selected certificate must contain a current code-signing private key.'
}
$files = @('agent-core-desktop.exe', 'agent-core.exe')
foreach ($path in @($directoryPath) + @($files | ForEach-Object { Join-Path $directoryPath $_ })) {
    $current = $path
    while ($current) {
        $item = Get-Item -LiteralPath $current -Force
        if ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) { throw 'Signing paths may not traverse a reparse point.' }
        $current = [IO.Path]::GetDirectoryName($current)
    }
}
$nonce = [Guid]::NewGuid().ToString('N')
$backup = Join-Path $directoryPath ".agent-core-sign-$nonce"
[IO.Directory]::CreateDirectory($backup) | Out-Null
[IO.File]::WriteAllText((Join-Path $backup '.owned'), $nonce)
$copied = [Collections.Generic.List[string]]::new()
$rollbackFailed = $false
try {
    foreach ($name in $files) {
        [IO.File]::Copy((Join-Path $directoryPath $name), (Join-Path $backup $name))
        $copied.Add($name)
    }
    foreach ($name in $files) {
        $path = Join-Path $directoryPath $name
        $parameters = @{ LiteralPath = $path; Certificate = $certificate; HashAlgorithm = 'SHA256'; IncludeChain = 'All' }
        if ($TimestampServer) { $parameters.TimestampServer = $TimestampServer }
        $result = Set-AuthenticodeSignature @parameters
        if ($result.Status -ne 'Valid') { throw 'Authenticode signing failed trusted signature verification.' }
        $verified = Get-AuthenticodeSignature -LiteralPath $path
        if ($verified.Status -ne 'Valid' -or $verified.SignerCertificate.Thumbprint -ne $Thumbprint -or
            ($TimestampServer -and $null -eq $verified.TimeStamperCertificate)) { throw 'Signed output verification failed.' }
    }
    Write-Output 'SIGNED: both Windows executables verified against the selected code-signing certificate.'
} catch {
    $original = $_
    foreach ($name in $copied) {
        try { [IO.File]::Copy((Join-Path $backup $name), (Join-Path $directoryPath $name), $true) }
        catch { $rollbackFailed = $true }
    }
    if ($rollbackFailed) { throw "Signing failed; original binaries remain recoverable in $backup." }
    throw $original
} finally {
    if (-not $rollbackFailed) {
        $full = [IO.Path]::GetFullPath($backup)
        if (-not $full.StartsWith($directoryPath.TrimEnd('\') + '\.agent-core-sign-', [StringComparison]::OrdinalIgnoreCase) -or
            (Get-Item -LiteralPath $full).Attributes -band [IO.FileAttributes]::ReparsePoint -or
            [IO.File]::ReadAllText((Join-Path $full '.owned')) -ne $nonce) { throw 'Signing cleanup ownership verification failed.' }
        foreach ($item in Get-ChildItem -LiteralPath $full -Force) {
            if ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) { throw 'Signing backup contains a reparse point.' }
        }
        Remove-Item -LiteralPath $full -Recurse -Force
    }
}
