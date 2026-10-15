[CmdletBinding()]
param()
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
$base = [IO.Path]::GetFullPath((Join-Path $root '.artifacts/package-tests'))
$fixture = Join-Path $base ([Guid]::NewGuid().ToString('N'))
if (-not $fixture.StartsWith($base + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase)) { throw 'Fixture containment failed.' }
New-Item -ItemType Directory -Path $fixture -Force | Out-Null
try {
    # Execute the actual two packaging expressions from the build source. This
    # tests real ZIP I/O, not a mocked Cargo invocation or a claimed native build.
    $tokens = $null; $errors = $null
    $ast = [Management.Automation.Language.Parser]::ParseFile((Join-Path $root 'rust/build-windows.ps1'), [ref]$tokens, [ref]$errors)
    if ($errors.Count) { throw 'Packaging source syntax failed.' }
    $assignment = @($ast.FindAll({ param($node) $node -is [Management.Automation.Language.AssignmentStatementAst] -and $node.Left -is [Management.Automation.Language.VariableExpressionAst] -and $node.Left.VariablePath.UserPath -eq 'packageFiles' }, $true))
    $compression = @($ast.FindAll({ param($node) $node -is [Management.Automation.Language.CommandAst] -and $node.GetCommandName() -eq 'Compress-Archive' }, $true))
    if ($assignment.Count -ne 1 -or $compression.Count -ne 1) { throw 'Expected the single actual package file selection/compression expressions.' }
    $output = Join-Path $fixture 'release'; $archive = Join-Path $fixture 'package.zip'
    New-Item -ItemType Directory -Path $output | Out-Null
    $names = @('agent-core-desktop.exe', 'agent-core.exe', 'README.md', 'MIGRATION.md', 'LICENSE', 'NOTICE.md')
    foreach ($name in $names) { [IO.File]::WriteAllBytes((Join-Path $output $name), [Text.Encoding]::UTF8.GetBytes("offline packaging fixture $name")) }
    $extra = Join-Path $output 'unrelated-user-notes.txt'
    [IO.File]::WriteAllText($extra, 'preserve this unrelated file exactly', [Text.UTF8Encoding]::new($false))
    $extraHash = (Get-FileHash -LiteralPath $extra -Algorithm SHA256).Hash
    Invoke-Expression $assignment[0].Extent.Text
    Invoke-Expression $compression[0].Extent.Text
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $zip = [IO.Compression.ZipFile]::OpenRead($archive)
    try {
        if ($zip.Entries.Count -ne 6) { throw 'ZIP must contain exactly six selected files.' }
        foreach ($entry in $zip.Entries) {
            if ($entry.FullName -notin $names) { throw "Unrelated ZIP entry: $($entry.FullName)" }
            $reader = $entry.Open(); $expected = [IO.File]::ReadAllBytes((Join-Path $output $entry.FullName)); $actual = [IO.MemoryStream]::new()
            try {
                $reader.CopyTo($actual); $bytes = $actual.ToArray()
                if ($bytes.Length -ne $expected.Length) { throw 'ZIP file length differs.' }
                for ($index = 0; $index -lt $bytes.Length; $index++) { if ($bytes[$index] -ne $expected[$index]) { throw 'ZIP file bytes differ.' } }
            }
            finally { $reader.Dispose(); $actual.Dispose() }
        }
    } finally { $zip.Dispose() }
    if ((Get-FileHash -LiteralPath $extra -Algorithm SHA256).Hash -ne $extraHash) { throw 'Packaging changed the unrelated original.' }
    Write-Output 'PASS real ZIP compression: six byte-identical members; unrelated source file excluded and unchanged; Cargo/native build not executed'
    $environmentBefore = [Environment]::GetEnvironmentVariable('CARGO_BUILD_TARGET', 'Process')
    foreach ($case in @(
        @{ action = 'Build'; target = 'x86_64-pc-windows-msvc' },
        @{ action = 'Package'; target = 'x86_64-pc-windows-msvc' },
        @{ action = 'Build'; target = 'aarch64-pc-windows-msvc' },
        @{ action = 'Package'; target = 'aarch64-pc-windows-msvc' },
        @{ action = 'Package'; target = ' ' }
    )) {
        $caseRoot = Join-Path $fixture ([Guid]::NewGuid().ToString('N'))
        $caseRust = Join-Path $caseRoot 'rust'
        $caseDist = Join-Path $caseRoot 'dist/agent-core-desktop-windows-x64'
        New-Item -ItemType Directory -Path $caseRust, $caseDist -Force | Out-Null
        $actualEntry = Join-Path $caseRust 'build-windows.ps1'
        # The actual entry bytes are executed in an independent child, with
        # its normal PSScriptRoot-derived output confined to this fixture copy.
        Copy-Item -LiteralPath (Join-Path $root 'rust/build-windows.ps1') -Destination $actualEntry
        if ((Get-FileHash -LiteralPath $actualEntry).Hash -ne (Get-FileHash -LiteralPath (Join-Path $root 'rust/build-windows.ps1')).Hash) { throw 'Build entry copy differs.' }
        foreach ($name in $names) { [IO.File]::WriteAllText((Join-Path $caseDist $name), "old package sentinel $name") }
        $caseZip = Join-Path $caseRoot 'dist/agent-core-desktop-windows-x64.zip'
        [IO.File]::WriteAllText($caseZip, 'old ZIP sentinel')
        $sentinelHashes = @{}
        foreach ($file in Get-ChildItem -LiteralPath (Join-Path $caseRoot 'dist') -File -Recurse) { $sentinelHashes[$file.FullName] = (Get-FileHash -LiteralPath $file.FullName).Hash }
        $probe = Join-Path $caseRoot 'cargo-invocations.txt'
        $launcher = Join-Path $caseRoot 'run-entry.ps1'
        [IO.File]::WriteAllText($launcher, @'
param([string]$Entry,[string]$Action,[string]$Probe)
$ErrorActionPreference='Stop'
function global:cargo {
    [IO.File]::AppendAllText($Probe, "cargo invoked`n")
    throw 'Unexpected Cargo invocation in early-guard regression'
}
& $Entry -Action $Action -Offline
'@, [Text.UTF8Encoding]::new($false))
        $start = [Diagnostics.ProcessStartInfo]::new()
        $start.FileName = Join-Path $PSHOME 'pwsh.exe'
        $start.UseShellExecute = $false; $start.CreateNoWindow = $true
        $start.RedirectStandardOutput = $true; $start.RedirectStandardError = $true
        foreach ($argument in @('-NoProfile', '-NonInteractive', '-File', $launcher, '-Entry', $actualEntry, '-Action', $case.action, '-Probe', $probe)) { $start.ArgumentList.Add($argument) }
        $start.Environment['CARGO_BUILD_TARGET'] = $case.target
        $child = [Diagnostics.Process]::new(); $child.StartInfo = $start
        try {
            if (-not $child.Start()) { throw 'Early-guard child did not start.' }
            $stdoutTask = $child.StandardOutput.ReadToEndAsync(); $stderrTask = $child.StandardError.ReadToEndAsync()
            if (-not $child.WaitForExit(10000)) { $child.Kill($true); $child.WaitForExit(); throw 'Early-guard child timed out.' }
            $stdout = $stdoutTask.GetAwaiter().GetResult(); $stderr = $stderrTask.GetAwaiter().GetResult()
            if ($child.ExitCode -ne 1 -or ($stdout + $stderr) -notmatch 'CARGO_BUILD_TARGET must be unset') { throw "Early guard did not reject actual entry: exit=$($child.ExitCode); $stdout $stderr" }
            if (Test-Path -LiteralPath $probe) { throw 'A Cargo invocation occurred before rejection.' }
            $afterFiles = @(Get-ChildItem -LiteralPath (Join-Path $caseRoot 'dist') -File -Recurse)
            if ($afterFiles.Count -ne $sentinelHashes.Count) { throw 'Early guard changed output inventory.' }
            foreach ($file in $afterFiles) { if ((Get-FileHash -LiteralPath $file.FullName).Hash -ne $sentinelHashes[$file.FullName]) { throw 'Early guard changed an existing output file.' } }
            Write-Output "PASS actual child $($case.action) target='$($case.target)': exit=1; Cargo invocations=0; six EXE/docs sentinels and ZIP unchanged"
        } finally { $child.Dispose() }
    }
    if ([Environment]::GetEnvironmentVariable('CARGO_BUILD_TARGET', 'Process') -cne $environmentBefore) { throw 'Child target override leaked into the parent environment.' }
    Write-Output 'PASS parent CARGO_BUILD_TARGET environment unchanged after all actual child guards'
} finally {
    $resolved = [IO.Path]::GetFullPath($fixture)
    if (-not $resolved.StartsWith($base + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase)) { throw 'Cleanup containment failed.' }
    $item = Get-Item -LiteralPath $resolved -Force
    if ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) { throw 'Cleanup rejects reparse fixture.' }
    Remove-Item -LiteralPath $resolved -Recurse -Force
}
