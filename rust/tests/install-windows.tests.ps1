$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
Add-Type -AssemblyName System.IO.Compression
$root = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '../..'))
$installer = Join-Path $root 'rust/install-windows.ps1'
$fixtureParent = Join-Path $root '.artifacts/installer-tests'
$fixture = Join-Path $fixtureParent ([Guid]::NewGuid().ToString('N'))
[IO.Directory]::CreateDirectory($fixture) | Out-Null
$nonce = [Guid]::NewGuid().ToString('N')
[IO.File]::WriteAllText((Join-Path $fixture '.owned'), $nonce)
$names = @('agent-core-desktop.exe', 'agent-core.exe', 'LICENSE', 'MIGRATION.md', 'NOTICE.md', 'README.md')
$log = [Collections.Generic.List[string]]::new()
$passed = 0
function Assert([bool]$Value, [string]$Message) { if (-not $Value) { throw $Message } }
function Hash([string]$Path) { return (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash }
function Package-Bytes([string]$Name, [string]$Tag) {
    if ($Name -in $names[0..1]) {
        # These structurally PE-shaped bytes are never executed. File-operation
        # tests do not purport to validate the separate release application's UI.
        $data = [byte[]]::new(384)
        $data[0] = 0x4D; $data[1] = 0x5A; $data[60] = 128
        $data[128] = 0x50; $data[129] = 0x45
        $data[132] = 0x64; $data[133] = 0x86
        $data[152] = 0x0B; $data[153] = 0x02
        [Text.Encoding]::UTF8.GetBytes($Tag).CopyTo($data, 256)
        return ,$data
    }
    $text = if ($Name -eq 'README.md') { "Package $Tag [LICENSE](LICENSE) [NOTICE](NOTICE.md)" } else { "$Name $Tag" }
    return ,[Text.Encoding]::UTF8.GetBytes($text)
}
function New-Package([string]$Name, [string]$Tag, [string[]]$Entries = $names, [int]$Attributes = 0, [switch]$BadExe) {
    $path = Join-Path $fixture "$Name.zip"
    $file = [IO.File]::Open($path, [IO.FileMode]::CreateNew)
    $zip = [IO.Compression.ZipArchive]::new($file, [IO.Compression.ZipArchiveMode]::Create)
    try {
        foreach ($entryName in $Entries) {
            $entry = $zip.CreateEntry($entryName)
            if ($entryName -eq 'README.md') { $entry.ExternalAttributes = $Attributes }
            $data = Package-Bytes $entryName $Tag
            if ($BadExe -and $entryName -eq 'agent-core.exe') { $data = [Text.Encoding]::UTF8.GetBytes('Not a Windows executable') }
            $stream = $entry.Open()
            try { $stream.Write($data, 0, $data.Length) } finally { $stream.Dispose() }
        }
    } finally { $zip.Dispose(); $file.Dispose() }
    return $path
}
function Invoke-Installer([string]$Action, [string]$Target, [string]$Zip, [string]$Sha, [bool]$Success = $true, [string[]]$Extra = @()) {
    $parameters = @{ Action = $Action; InstallDirectory = $Target }
    if ($Zip) { $parameters.Package = $Zip; $parameters.Sha256 = $Sha }
    $label = "& $installer -Action $Action -InstallDirectory $Target"
    if ($Zip) { $label += " -Package $Zip -Sha256 $Sha" }
    $log.Add("COMMAND: $label $($Extra -join ' ')")
    $failure = $null
    try { $output = & $installer @parameters @Extra | Out-String } catch { $failure = $_ }
    if ($null -ne $failure) {
        if ($Success) { throw $failure }
        $log.Add("LITERAL RESULT: $($failure.Exception.Message)`nCALL STATUS: terminating error (expected; caught by the harness, not a separate process)")
    } else {
        $log.Add("LITERAL RESULT: $($output.Trim())`nCALL STATUS: returned normally")
        Assert $Success 'An invalid installer invocation unexpectedly succeeded.'
    }
}
function Snapshot([string]$Target) {
    $value = [ordered]@{}
    foreach ($item in Get-ChildItem -LiteralPath $Target -Recurse -File -Force | Sort-Object FullName) {
        $value[$item.FullName.Substring($Target.Length + 1)] = Hash $item.FullName
    }
    return ($value | ConvertTo-Json -Compress)
}
function Case([string]$Name, [scriptblock]$Run) {
    & $Run
    $script:passed++
    $log.Add("TEST: $Name PASS")
    Write-Output "PASS $Name"
}
try {
    $v1 = New-Package 'v1' 'version-one'
    $v2 = New-Package 'v2' 'version-two'
    $target = Join-Path $fixture 'application'
    Case 'first install writes exact managed files and hashes' {
        Invoke-Installer Install $target $v1 (Hash $v1)
        $manifest = [IO.File]::ReadAllText((Join-Path $target '.agent-core-install.json')) | ConvertFrom-Json
        Assert ($manifest.files.Count -eq 6) 'Managed manifest did not have six release files.'
        foreach ($entry in $manifest.files) { Assert ((Hash (Join-Path $target $entry.path)) -eq $entry.sha256) 'Managed file hash differed.' }
        Assert (@(Get-ChildItem -LiteralPath $target -Force).Count -eq 7) 'Installer left transaction files after first install.'
    }
    Case 'update preserves user configuration and unrelated files' {
        [IO.Directory]::CreateDirectory((Join-Path $target 'data')) | Out-Null
        [IO.File]::WriteAllText((Join-Path $target 'config.json'), '{"user":"keep"}')
        [IO.File]::WriteAllText((Join-Path $target 'data/history.json'), 'history untouched')
        $before = Hash (Join-Path $target 'config.json')
        $old = Hash (Join-Path $target 'agent-core.exe')
        Invoke-Installer Update $target $v2 (Hash $v2)
        Assert ((Hash (Join-Path $target 'agent-core.exe')) -ne $old) 'Update did not change executable fixture bytes.'
        Assert ((Hash (Join-Path $target 'config.json')) -eq $before) 'Update changed user configuration.'
        Assert ([IO.File]::ReadAllText((Join-Path $target 'data/history.json')) -eq 'history untouched') 'Update changed user history.'
    }
    Case 'uninstall preserves user data and modified managed files' {
        [IO.File]::WriteAllText((Join-Path $target 'README.md'), 'user edited readme')
        Invoke-Installer Uninstall $target '' ''
        Assert (-not (Test-Path (Join-Path $target 'agent-core.exe'))) 'Uninstall left an unchanged managed executable.'
        Assert (Test-Path (Join-Path $target 'config.json')) 'Uninstall deleted user configuration.'
        Assert (Test-Path (Join-Path $target 'data/history.json')) 'Uninstall deleted user history.'
        Assert ([IO.File]::ReadAllText((Join-Path $target 'README.md')) -eq 'user edited readme') 'Uninstall deleted a user-modified managed file.'
        Assert (-not (Test-Path (Join-Path $target '.agent-core-install.json'))) 'Uninstall left active management metadata.'
    }
    Case 'bad archive hash changes no installation files' {
        $target = Join-Path $fixture 'bad-hash'
        Invoke-Installer Install $target $v1 ('0' * 64) $false
        Assert (-not (Test-Path $target)) 'Bad hash created an installation.'
    }
    Case 'archive traversal and absolute entries never escape target' {
        foreach ($evil in @('../escaped.txt', '/absolute.txt', 'C:/escaped.txt', '..\escaped.txt', 'README.md:stream')) {
            $entries = $names.Clone(); $entries[5] = $evil
            $zip = New-Package ([Guid]::NewGuid().ToString('N')) 'bad-path' $entries
            $target = Join-Path $fixture ([Guid]::NewGuid().ToString('N'))
            Invoke-Installer Install $target $zip (Hash $zip) $false
            Assert (-not (Test-Path $target)) 'Unsafe archive left an installation.'
            Assert (-not (Test-Path (Join-Path $fixture 'escaped.txt'))) 'Archive traversal escaped its target.'
        }
    }
    Case 'case-folded duplicate ZIP names are rejected' {
        $entries = $names.Clone(); $entries[5] = 'AGENT-CORE.EXE'
        $zip = New-Package 'duplicate' 'bad' $entries
        $target = Join-Path $fixture 'duplicate-target'
        Invoke-Installer Install $target $zip (Hash $zip) $false
        Assert (-not (Test-Path $target)) 'Duplicate archive left an installation.'
    }
    Case 'Unix symlink and Windows reparse ZIP attributes are rejected' {
        foreach ($attributes in @(-1610612736, 1024)) {
            $zip = New-Package ([Guid]::NewGuid().ToString('N')) 'link' $names $attributes
            $target = Join-Path $fixture ([Guid]::NewGuid().ToString('N'))
            Invoke-Installer Install $target $zip (Hash $zip) $false
            Assert (-not (Test-Path $target)) 'Link archive left an installation.'
        }
    }
    Case 'first install validation failure rolls back staging and target' {
        $zip = New-Package 'bad-executable' 'invalid' $names 0 -BadExe
        $target = Join-Path $fixture 'bad-executable-target'
        Invoke-Installer Install $target $zip (Hash $zip) $false
        Assert (-not (Test-Path $target)) 'Failed first install retained staging files.'
    }
    Case 'existing unrelated package-name file is never overwritten' {
        $target = Join-Path $fixture 'unmanaged-collision'
        [IO.Directory]::CreateDirectory($target) | Out-Null
        [IO.File]::WriteAllText((Join-Path $target 'agent-core.exe'), 'unrelated user executable')
        $before = Snapshot $target
        Invoke-Installer Install $target $v1 (Hash $v1) $false
        Assert ((Snapshot $target) -eq $before) 'Unmanaged collision changed user files.'
    }
    Case 'update sharing violation restores exact old package and manifest' {
        $target = Join-Path $fixture 'update-rollback'
        Invoke-Installer Install $target $v1 (Hash $v1)
        $before = Snapshot $target
        $held = [IO.File]::Open((Join-Path $target 'agent-core.exe'), [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::Read)
        try { Invoke-Installer Update $target $v2 (Hash $v2) $false } finally { $held.Dispose() }
        Assert ((Snapshot $target) -eq $before) 'Partial update did not restore original bytes and manifest.'
    }
    Case 'uninstall sharing violation restores exact old package' {
        $target = Join-Path $fixture 'uninstall-rollback'
        Invoke-Installer Install $target $v1 (Hash $v1)
        $before = Snapshot $target
        $held = [IO.File]::Open((Join-Path $target 'agent-core.exe'), [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::Read)
        try { Invoke-Installer Uninstall $target '' '' $false } finally { $held.Dispose() }
        Assert ((Snapshot $target) -eq $before) 'Partial uninstall did not restore original bytes and manifest.'
    }
    Case 'tampered manifest cannot delete user data outside managed names' {
        $target = Join-Path $fixture 'tampered-manifest'
        Invoke-Installer Install $target $v1 (Hash $v1)
        [IO.File]::WriteAllText((Join-Path $target 'user-data.txt'), 'preserve')
        $manifestPath = Join-Path $target '.agent-core-install.json'
        $manifest = [IO.File]::ReadAllText($manifestPath) | ConvertFrom-Json
        $manifest.files[0].path = '../outside.txt'
        [IO.File]::WriteAllText($manifestPath, ($manifest | ConvertTo-Json -Depth 5))
        $before = Snapshot $target
        Invoke-Installer Uninstall $target '' '' $false
        Assert ((Snapshot $target) -eq $before) 'Tampered manifest changed user data.'
    }
    Case 'target junction and managed directory junction are rejected' {
        $outside = Join-Path $fixture 'outside'
        [IO.Directory]::CreateDirectory($outside) | Out-Null
        [IO.File]::WriteAllText((Join-Path $outside 'user.txt'), 'unchanged')
        $junction = Join-Path $fixture 'target-junction'
        New-Item -ItemType Junction -Path $junction -Value $outside | Out-Null
        try { Invoke-Installer Install $junction $v1 (Hash $v1) $false } finally { Remove-Item -LiteralPath $junction -Force }
        Assert (@(Get-ChildItem $outside).Count -eq 1) 'Junction target was modified.'
        $target = Join-Path $fixture 'managed-junction'
        Invoke-Installer Install $target $v1 (Hash $v1)
        $managed = Join-Path $target 'LICENSE'
        Remove-Item -LiteralPath $managed
        New-Item -ItemType Junction -Path $managed -Value $outside | Out-Null
        try { Invoke-Installer Update $target $v2 (Hash $v2) $false } finally { Remove-Item -LiteralPath $managed -Force }
        Assert ([IO.File]::ReadAllText((Join-Path $outside 'user.txt')) -eq 'unchanged') 'Managed junction changed external data.'
    }
    Case 'drive roots are rejected before file operations' {
        Invoke-Installer Install ([IO.Path]::GetPathRoot($fixture)) $v1 (Hash $v1) $false
    }
    Case 'unsigned package cannot satisfy required Authenticode' {
        $target = Join-Path $fixture 'unsigned'
        $failed = $false
        try { & $installer -Action Install -InstallDirectory $target -Package $v1 -Sha256 (Hash $v1) -RequireSignature }
        catch { $failed = $_.Exception.Message -match 'valid trusted Authenticode signature' }
        Assert $failed 'Unsigned fixture was reported as signed.'
        Assert (-not (Test-Path $target)) 'Signature failure left installation files.'
    }
    Case 'malformed signer thumbprint is rejected without certificate access' {
        $failed = $false
        try { & (Join-Path $root 'rust/sign-windows.ps1') -Directory $fixture -Thumbprint 'invalid' }
        catch { $failed = $true }
        Assert $failed 'Signer accepted an invalid thumbprint.'
    }
    Case 'native Windows PowerShell installer process succeeds with literal output and exit zero' {
        $target = Join-Path $fixture 'native application with spaces'
        $info = [Diagnostics.ProcessStartInfo]::new()
        $info.FileName = (Get-Command powershell.exe).Source
        $arguments = @('-NoProfile', '-NonInteractive', '-ExecutionPolicy', 'Bypass', '-File', $installer, '-Action', 'Install', '-InstallDirectory', $target, '-Package', $v1, '-Sha256', (Hash $v1))
        $info.Arguments = ($arguments | ForEach-Object { '"' + $_.Replace('"', '\"') + '"' }) -join ' '
        $info.UseShellExecute = $false; $info.CreateNoWindow = $true
        $info.RedirectStandardOutput = $true; $info.RedirectStandardError = $true
        $process = [Diagnostics.Process]::Start($info)
        $stdout = $process.StandardOutput.ReadToEndAsync()
        $stderr = $process.StandardError.ReadToEndAsync()
        try {
            Assert ($process.WaitForExit(30000)) 'Native installer child exceeded its deadline.'
            $out = $stdout.GetAwaiter().GetResult(); $err = $stderr.GetAwaiter().GetResult()
            $log.Add("COMMAND: $($info.FileName) $($info.Arguments)`nINPUT: empty stdin, hidden window`nLITERAL STDOUT: $out`nLITERAL STDERR: $err`nEXIT: $($process.ExitCode)")
            if ($process.ExitCode -ne 0) { Write-Output "NATIVE EXIT: $($process.ExitCode)`nSTDOUT: $out`nSTDERR: $err" }
            Assert ($process.ExitCode -eq 0 -and $out.StartsWith('INSTALL:') -and -not $err) 'Native installer process did not succeed.'
            Assert (Test-Path (Join-Path $target '.agent-core-install.json')) 'Native installer did not commit its manifest.'
        } finally { if (-not $process.HasExited) { $process.Kill(); $process.WaitForExit() }; $process.Dispose() }
    }
    Case 'native Windows PowerShell bad hash process fails with exit one and no target writes' {
        $target = Join-Path $fixture 'native hash failure'
        $info = [Diagnostics.ProcessStartInfo]::new()
        $info.FileName = (Get-Command powershell.exe).Source
        $arguments = @('-NoProfile', '-NonInteractive', '-ExecutionPolicy', 'Bypass', '-File', $installer, '-Action', 'Install', '-InstallDirectory', $target, '-Package', $v1, '-Sha256', ('0' * 64))
        $info.Arguments = ($arguments | ForEach-Object { '"' + $_.Replace('"', '\"') + '"' }) -join ' '
        $info.UseShellExecute = $false; $info.CreateNoWindow = $true
        $info.RedirectStandardOutput = $true; $info.RedirectStandardError = $true
        $process = [Diagnostics.Process]::Start($info)
        $stdout = $process.StandardOutput.ReadToEndAsync()
        $stderr = $process.StandardError.ReadToEndAsync()
        try {
            Assert ($process.WaitForExit(30000)) 'Native installer child exceeded its deadline.'
            $out = $stdout.GetAwaiter().GetResult(); $err = $stderr.GetAwaiter().GetResult()
            $log.Add("COMMAND: $($info.FileName) $($info.Arguments)`nINPUT: empty stdin, hidden window`nLITERAL STDOUT: $out`nLITERAL STDERR: $err`nEXIT: $($process.ExitCode)")
            Assert ($process.ExitCode -eq 1 -and $err.Contains('SHA256 differs') -and -not (Test-Path $target)) 'Native bad-hash failure did not preserve the target.'
        } finally { if (-not $process.HasExited) { $process.Kill(); $process.WaitForExit() }; $process.Dispose() }
    }
    $report = "COMMAND: pwsh -File rust/tests/install-windows.tests.ps1`nINPUT: workspace-only ZIP/file fixtures; PE-shaped executable bytes are never run; no real credentials/AppData/certificates`n" + ($log -join "`n") + "`nRESULT: $passed/$passed PASS`nEXIT: 0`n"
    [IO.File]::WriteAllText((Join-Path $root '.artifacts/rust-refactor-08/INSTALLER_TESTS.txt'), $report)
    Write-Output "PASS $passed offline installer file-operation tests"
} finally {
    $full = [IO.Path]::GetFullPath($fixture)
    $prefix = [IO.Path]::GetFullPath($fixtureParent).TrimEnd('\') + '\'
    if (-not $full.StartsWith($prefix, [StringComparison]::OrdinalIgnoreCase) -or
        [IO.File]::ReadAllText((Join-Path $full '.owned')) -ne $nonce -or
        (Get-Item -LiteralPath $full).Attributes -band [IO.FileAttributes]::ReparsePoint) { throw 'Fixture cleanup ownership failed.' }
    foreach ($item in Get-ChildItem -LiteralPath $full -Recurse -Force) {
        if ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) { throw 'Fixture cleanup encountered an unreleased junction.' }
    }
    Remove-Item -LiteralPath $full -Recurse -Force
}
