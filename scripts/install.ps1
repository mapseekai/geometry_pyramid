#Requires -Version 7.0
<#
Install a release bundle into one PostgreSQL installation. This copies files;
it never executes CREATE EXTENSION or modifies database data.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$PgConfig,
    [string]$DestDir = '',
    [switch]$DryRun
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
if (-not $IsWindows) { throw 'Use install.sh for Linux/macOS. This installer requires Windows and PowerShell 7+.' }
if (-not (Test-Path -LiteralPath $PgConfig -PathType Leaf)) { throw "pg_config.exe not found: $PgConfig" }

function Get-PgValue([string]$Option) {
    $value = & $PgConfig $Option
    if ($LASTEXITCODE -ne 0) { throw "pg_config failed: $Option" }
    return ($value -join "`n").Trim()
}

# Check every file without evaluating the manifest as code.
$checksums = Get-Content -LiteralPath (Join-Path $PSScriptRoot 'SHA256SUMS')
$seen = @{}
foreach ($line in $checksums) {
    if ($line -notmatch '^([0-9a-f]{64})  ([A-Za-z0-9_./+-]+)$') { throw 'Invalid checksum record.' }
    $digest, $relative = $Matches[1], $Matches[2]
    if ($relative.StartsWith('/') -or $relative.Contains('..') -or $seen.ContainsKey($relative)) { throw 'Invalid or duplicate checksum path.' }
    $seen[$relative] = $true
    $file = Join-Path $PSScriptRoot $relative
    if ((Get-Item -LiteralPath $file).Attributes -band [IO.FileAttributes]::ReparsePoint) { throw "Reparse point in bundle: $relative" }
    if ((Get-FileHash -Algorithm SHA256 -LiteralPath $file).Hash.ToLowerInvariant() -ne $digest) { throw "Checksum mismatch: $relative" }
}
$manifest = Get-Content -Raw -LiteralPath (Join-Path $PSScriptRoot 'manifest.json') | ConvertFrom-Json
if ($manifest.format -ne 1 -or $manifest.extension -ne 'geometry_pyramid' -or $manifest.target.system -ne 'windows') { throw 'Unrecognized Windows bundle.' }
if ($manifest.library -ne 'lib/geometry_pyramid.dll' -or $manifest.version -notmatch '^[A-Za-z0-9][A-Za-z0-9.+-]*$') { throw 'Invalid package metadata.' }
foreach ($required in @('manifest.json', 'install.ps1', 'lib/geometry_pyramid.dll', 'extension/geometry_pyramid.control', "extension/geometry_pyramid--$($manifest.version).sql")) {
    if (-not $seen.ContainsKey($required)) { throw "Artifact missing from SHA256SUMS: $required" }
}
$targetVersion = Get-PgValue '--version'
if ($targetVersion -notmatch '^PostgreSQL\s+(\d+)(?:\.|\s|$)' -or [int]$Matches[1] -ne $manifest.postgresql.major) { throw "Bundle requires PostgreSQL $($manifest.postgresql.major); target is $targetVersion." }

# Inspect the actual PostgreSQL executable, including under WOW/ARM emulation.
$postgres = Join-Path (Get-PgValue '--bindir') 'postgres.exe'
$bytes = [IO.File]::ReadAllBytes($postgres)
if ($bytes.Length -lt 64 -or $bytes[0] -ne 77 -or $bytes[1] -ne 90) { throw 'Invalid PostgreSQL PE executable.' }
$offset = [BitConverter]::ToInt32($bytes, 60)
if ($offset -lt 0 -or $offset + 24 -gt $bytes.Length -or [BitConverter]::ToUInt32($bytes, $offset) -ne 17744) { throw 'Invalid PostgreSQL PE header.' }
$machine = [BitConverter]::ToUInt16($bytes, $offset + 4)
$serverArchitecture = switch ($machine) { 0x8664 { 'x86_64' }; 0xAA64 { 'aarch64' }; default { throw 'Unsupported PostgreSQL architecture.' } }
if ($serverArchitecture -ne $manifest.target.architecture) { throw "PostgreSQL architecture $serverArchitecture does not match $($manifest.target.architecture)." }

function Destination([string]$Path) {
    if (-not [IO.Path]::IsPathFullyQualified($Path)) { throw "pg_config returned a nonabsolute path: $Path" }
    if ($DestDir) {
        if (-not [IO.Path]::IsPathFullyQualified($DestDir)) { throw '-DestDir must be an absolute path.' }
        return Join-Path $DestDir $Path.Substring([IO.Path]::GetPathRoot($Path).Length)
    }
    return $Path
}
$libDir = Destination (Get-PgValue '--pkglibdir')
$extensionDir = Join-Path (Destination (Get-PgValue '--sharedir')) 'extension'
$files = [Collections.Generic.List[object]]::new()
$files.Add(@{ Source = (Join-Path $PSScriptRoot 'lib/geometry_pyramid.dll'); Destination = (Join-Path $libDir 'geometry_pyramid.dll') })
foreach ($sql in (Get-ChildItem -LiteralPath (Join-Path $PSScriptRoot 'extension') -Filter 'geometry_pyramid--*.sql' | Sort-Object Name)) {
    if ($sql.Name -notmatch '^geometry_pyramid--[A-Za-z0-9.+-]+\.sql$' -or -not $seen.ContainsKey("extension/$($sql.Name)")) { throw "Unverified SQL artifact: $($sql.Name)" }
    $files.Add(@{ Source = $sql.FullName; Destination = (Join-Path $extensionDir $sql.Name) })
}
# Publish the control file last so its default version has all required files.
$files.Add(@{ Source = (Join-Path $PSScriptRoot 'extension/geometry_pyramid.control'); Destination = (Join-Path $extensionDir 'geometry_pyramid.control') })
foreach ($file in $files) { Write-Output "$([IO.Path]::GetFileName($file.Source)) -> $($file.Destination)" }
if ($DryRun) { return }

$temporaries = [Collections.Generic.List[string]]::new()
$backups = [Collections.Generic.List[string]]::new()
$commitStarted = $false
$recoveryFailed = $false
try {
    # Fail before changing any files if the existing DLL is mapped by a server.
    $dll = $files[0].Destination
    if (Test-Path -LiteralPath $dll) {
        try {
            $handle = [IO.File]::Open($dll, [IO.FileMode]::Open, [IO.FileAccess]::ReadWrite, [IO.FileShare]::None)
            $handle.Dispose()
        } catch {
            throw 'The extension DLL is in use or not writable. Stop PostgreSQL services using this installation, then run this installer as an administrator.'
        }
    }
    foreach ($file in $files) {
        $directory = [IO.Path]::GetDirectoryName($file.Destination)
        [IO.Directory]::CreateDirectory($directory) | Out-Null
        $temporary = Join-Path $directory ('.geometry_pyramid.' + [Guid]::NewGuid().ToString('N'))
        $temporaries.Add($temporary)
        [IO.File]::Copy($file.Source, $temporary, $false)
        if (Test-Path -LiteralPath $file.Destination) {
            $existing = Get-Item -LiteralPath $file.Destination
            if ($existing.PSIsContainer -or ($existing.Attributes -band [IO.FileAttributes]::ReparsePoint)) { throw "Destination must be a regular file: $($file.Destination)" }
            $backup = Join-Path $directory ('.geometry_pyramid.backup.' + [Guid]::NewGuid().ToString('N'))
            $backups.Add($backup)
            [IO.File]::Copy($file.Destination, $backup, $false)
            # Preserve the original permissions if an installation needs rollback.
            Set-Acl -LiteralPath $backup -AclObject (Get-Acl -LiteralPath $file.Destination)
        } else {
            $backups.Add('')
        }
    }
    $commitStarted = $true
    for ($index = 0; $index -lt $files.Count; $index++) {
        try {
            [IO.File]::Move($temporaries[$index], $files[$index].Destination, $true)
        } catch {
            throw "Could not replace $($files[$index].Destination). Stop PostgreSQL services using this installation and check administrator permissions. $($_.Exception.Message)"
        }
    }
} catch {
    $installationError = $_
    if ($commitStarted) {
        Write-Warning 'Installation interrupted; restoring the previous extension files.'
        for ($index = $files.Count - 1; $index -ge 0; $index--) {
            # Move consumes its temporary source. Untouched destinations need
            # no restore, including a DLL that failed its final sharing check.
            if (-not [IO.File]::Exists($temporaries[$index])) {
                try {
                    if ($backups[$index]) {
                        [IO.File]::Move($backups[$index], $files[$index].Destination, $true)
                    } else {
                        [IO.File]::Delete($files[$index].Destination)
                    }
                } catch {
                    $recoveryFailed = $true
                    Write-Warning "Could not restore $($files[$index].Destination): $($_.Exception.Message)"
                }
            }
        }
    }
    if ($recoveryFailed) {
        Write-Warning 'Automatic rollback could not finish. Stop PostgreSQL services and restore the following files; recovery materials have been kept.'
        for ($index = 0; $index -lt $backups.Count; $index++) {
            if ($backups[$index] -and [IO.File]::Exists($backups[$index])) {
                Write-Warning "Backup $($backups[$index]) -> $($files[$index].Destination)"
            } elseif (-not $backups[$index] -and -not [IO.File]::Exists($temporaries[$index])) {
                Write-Warning "Remove newly installed file $($files[$index].Destination)"
            }
        }
    }
    throw $installationError
} finally {
    if (-not $recoveryFailed) {
        foreach ($temporary in $temporaries) {
            if (Test-Path -LiteralPath $temporary) { Remove-Item -LiteralPath $temporary -Force }
        }
        foreach ($backup in $backups) {
            if ($backup -and (Test-Path -LiteralPath $backup)) { Remove-Item -LiteralPath $backup -Force }
        }
    }
}
Write-Output "Installed geometry_pyramid $($manifest.version) for $targetVersion."
Write-Output 'Start PostgreSQL and enable or upgrade the extension in each intended database using an administrator.'
