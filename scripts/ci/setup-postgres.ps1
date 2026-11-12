#Requires -Version 7.0
<#
Prepare a runner-owned EDB PostgreSQL installation and the matching PostGIS
bundle for a Windows release job. cargo-pgrx must already be installed.

Inputs: GP_PG_MAJOR (13..18), RUNNER_TEMP; GITHUB_ENV/GITHUB_PATH are optional
when running this script interactively. Upstream package inventory:
https://postgis.net/documentation/getting_started/install_windows/released_versions/
https://download.osgeo.org/postgis/windows/
#>
[CmdletBinding()]
param()

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $false

function Invoke-CheckedNative {
    param([string]$Command, [string[]]$Arguments)
    & $Command @Arguments
    if ($LASTEXITCODE -ne 0) {
        throw "$Command exited with code $LASTEXITCODE"
    }
}

function Export-CiEnvironment {
    param([string]$Name, [string]$Value)
    if ($Value.Contains("`r") -or $Value.Contains("`n")) {
        throw "Cannot export a multiline value for $Name"
    }
    [Environment]::SetEnvironmentVariable($Name, $Value, 'Process')
    if ($env:GITHUB_ENV) {
        [IO.File]::AppendAllText($env:GITHUB_ENV, "$Name=$Value`n", [Text.UTF8Encoding]::new($false))
    }
}

if (-not $IsWindows) {
    throw 'setup-postgres.ps1 requires a native Windows runner.'
}
if ([Runtime.InteropServices.RuntimeInformation]::ProcessArchitecture -ne 'X64') {
    throw 'The EDB and PostGIS bundles used by this script target Windows x64.'
}
if ($env:GP_PG_MAJOR -notmatch '^(13|14|15|16|17|18)$') {
    throw 'GP_PG_MAJOR must be a PostgreSQL major version from 13 through 18.'
}
if (-not $env:RUNNER_TEMP) {
    throw 'RUNNER_TEMP must point to a writable temporary directory.'
}

$pgMajor = $env:GP_PG_MAJOR
Export-CiEnvironment 'GP_PG_MAJOR' $pgMajor
Export-CiEnvironment 'PG_PG_MAJOR' $pgMajor
# Versions and vendor-published MD5 sums were checked on 2026-11-12. OSGeo
# publishes MD5 sidecars for these bundles; SHA256 is also recorded below.
$postgisBundles = @{
    '13' = @{ Version = '3.6.2'; Md5 = '7c3396e34b616f2db76f02595d8ec6ad' }
    '14' = @{ Version = '3.6.1'; Md5 = '2db3dd19c82c6174c7f485339e66684e' }
    '15' = @{ Version = '3.6.2'; Md5 = 'aa4810d34833d7831ec806dd70ba6c3d' }
    '16' = @{ Version = '3.6.2'; Md5 = '6730107131c29b7ff9a36702f736007b' }
    '17' = @{ Version = '3.6.2'; Md5 = 'faca768cc580c4ab2eef621be05b408e' }
    '18' = @{ Version = '3.7.0'; Md5 = '2e5fccfe6d4d20c799dc235251404cf6' }
}
$bundle = $postgisBundles[$pgMajor]
$workDir = Join-Path $env:RUNNER_TEMP "geometry-pyramid-pg$pgMajor-$([Guid]::NewGuid().ToString('N'))"
New-Item -ItemType Directory -Path $workDir | Out-Null
Export-CiEnvironment 'PGRX_HOME' (Join-Path $workDir 'pgrx')

# The GitHub Windows image includes standalone LLVM and MSVC. Keep a caller's
# explicit libclang selection, otherwise use the installed LLVM directory.
$clangBin = $env:LIBCLANG_PATH
if (-not $clangBin) {
    $clangBin = Split-Path -Parent (Get-Command clang.exe -ErrorAction Stop).Source
}
if (-not (Test-Path -LiteralPath (Join-Path $clangBin 'libclang.dll') -PathType Leaf)) {
    throw "libclang.dll is missing from $clangBin"
}
Export-CiEnvironment 'LIBCLANG_PATH' $clangBin

# On Windows pgrx downloads the matching EDB x64 binaries, including server
# headers and import libraries. It does not use the runner's preinstalled PG.
Invoke-CheckedNative 'cargo' @('pgrx', 'init', "--pg${pgMajor}=download")
$pgConfig = (Invoke-CheckedNative 'cargo' @('pgrx', 'info', 'pg-config', $pgMajor)).Trim()
if (-not (Test-Path -LiteralPath $pgConfig -PathType Leaf)) {
    throw "cargo pgrx did not return a usable pg_config.exe: $pgConfig"
}
$pgVersion = (Invoke-CheckedNative $pgConfig @('--version')).Trim()
if ($pgVersion -notmatch "^PostgreSQL ${pgMajor}(?:\.|$)") {
    throw "Expected PostgreSQL $pgMajor, got $pgVersion"
}
$pgBin = (Invoke-CheckedNative $pgConfig @('--bindir')).Trim()
$pgLib = (Invoke-CheckedNative $pgConfig @('--pkglibdir')).Trim()
$pgShare = (Invoke-CheckedNative $pgConfig @('--sharedir')).Trim()
$extensionDir = Join-Path $pgShare 'extension'
Export-CiEnvironment 'PG_CONFIG' $pgConfig
Export-CiEnvironment 'GP_POSTGIS_VERSION' $bundle.Version
$env:PATH = "$pgBin;$env:PATH"
if ($env:GITHUB_PATH) {
    [IO.File]::AppendAllText($env:GITHUB_PATH, "$pgBin`n", [Text.UTF8Encoding]::new($false))
} elseif ($env:GITHUB_ENV) {
    Export-CiEnvironment 'PATH' $env:PATH
}

$archiveName = "postgis-bundle-pg$pgMajor-$($bundle.Version)x64.zip"
$archivePath = Join-Path $workDir $archiveName
$downloaded = $false
foreach ($baseUrl in @('https://download-cache.osgeo.org/postgis/windows', 'https://download.osgeo.org/postgis/windows')) {
    $downloadUrl = "$baseUrl/pg$pgMajor/$archiveName"
    try {
        Invoke-WebRequest -Uri $downloadUrl -OutFile $archivePath -TimeoutSec 240 -MaximumRetryCount 2 -RetryIntervalSec 3
        $archiveMd5 = (Get-FileHash -LiteralPath $archivePath -Algorithm MD5).Hash.ToLowerInvariant()
        if ($archiveMd5 -ne $bundle.Md5) {
            throw "PostGIS checksum mismatch: expected $($bundle.Md5), received $archiveMd5"
        }
        $downloaded = $true
        break
    } catch {
        Write-Warning "Failed to obtain the pinned PostGIS bundle from ${downloadUrl}: $_"
    }
}
if (-not $downloaded) {
    throw "Could not download and verify $archiveName from OSGeo."
}
$archiveSha256 = (Get-FileHash -LiteralPath $archivePath -Algorithm SHA256).Hash.ToLowerInvariant()
Export-CiEnvironment 'GP_POSTGIS_ARCHIVE_SHA256' $archiveSha256
Write-Host "PostGIS archive SHA256: $archiveSha256"

$unpackDir = Join-Path $workDir 'postgis'
Expand-Archive -LiteralPath $archivePath -DestinationPath $unpackDir
# The official archives contain one named root, then bin/, lib/ and share/.
$bundleRoot = Join-Path $unpackDir ([IO.Path]::GetFileNameWithoutExtension($archiveName))
if (-not (Test-Path -LiteralPath (Join-Path $bundleRoot 'share/extension/postgis.control') -PathType Leaf)) {
    throw "The PostGIS archive has an unexpected layout: $bundleRoot"
}
$destinations = @{ bin = $pgBin; lib = $pgLib; share = $pgShare }
foreach ($subdir in @('bin', 'lib', 'share')) {
    $sourceDir = Join-Path $bundleRoot $subdir
    if (-not (Test-Path -LiteralPath $sourceDir -PathType Container)) {
        throw "Missing $subdir directory in the PostGIS bundle."
    }
    $destinationDir = $destinations[$subdir]
    New-Item -ItemType Directory -Path $destinationDir -Force | Out-Null
    Get-ChildItem -LiteralPath $sourceDir -Force | Copy-Item -Destination $destinationDir -Recurse -Force
}

if (-not (Test-Path -LiteralPath (Join-Path $pgLib 'postgis-3.dll') -PathType Leaf)) {
    throw 'The PostGIS server DLL was not installed in PostgreSQL pkglibdir.'
}
foreach ($directory in @($pgLib, $extensionDir)) {
    $writeProbe = Join-Path $directory ".geometry-pyramid-write-$([Guid]::NewGuid().ToString('N'))"
    [IO.File]::WriteAllText($writeProbe, '')
    Remove-Item -LiteralPath $writeProbe
}

# Exercise the actual DLL and MVT support before compiling the extension.
# A private temporary cluster avoids the runner's PostgreSQL service and is
# stopped before this script finishes.
$dataDir = Join-Path $workDir 'verify-data'
$serverLog = Join-Path $workDir 'verify-postgres.log'
$sqlPath = Join-Path $workDir 'verify-postgis.sql'
[IO.File]::WriteAllText($sqlPath, @'
CREATE EXTENSION postgis;
SELECT postgis_full_version();
DO $verify$
DECLARE
    tile_data bytea;
BEGIN
    SELECT ST_AsMVT(tile, 'setup_smoke', 4096, 'geom') INTO tile_data
    FROM (
        SELECT 1 AS id, ST_AsMVTGeom(
            ST_SetSRID(ST_MakePoint(0, 0), 3857),
            ST_TileEnvelope(0, 0, 0), 4096, 256, true
        ) AS geom
    ) AS tile;
    IF tile_data IS NULL OR octet_length(tile_data) = 0 THEN
        RAISE EXCEPTION 'PostGIS MVT verification produced an empty tile';
    END IF;
END;
$verify$;
'@, [Text.UTF8Encoding]::new($false))

$portProbe = [Net.Sockets.TcpListener]::new([Net.IPAddress]::Loopback, 0)
$portProbe.Start()
$validationPort = $portProbe.LocalEndpoint.Port
$portProbe.Stop()
$pgCtl = Join-Path $pgBin 'pg_ctl.exe'
Invoke-CheckedNative (Join-Path $pgBin 'initdb.exe') @(
    '-D', $dataDir, '--username=postgres', '--auth=trust', '--encoding=UTF8', '--locale=C'
)
$started = $false
try {
    Invoke-CheckedNative $pgCtl @('-D', $dataDir, '-l', $serverLog, '-o', "-h 127.0.0.1 -p $validationPort", '-w', 'start')
    $started = $true
    Invoke-CheckedNative (Join-Path $pgBin 'psql.exe') @(
        '-X', '-v', 'ON_ERROR_STOP=1', '-h', '127.0.0.1', '-p', "$validationPort",
        '-U', 'postgres', '-d', 'postgres', '-f', $sqlPath
    )
} catch {
    if (Test-Path -LiteralPath $serverLog) {
        Get-Content -LiteralPath $serverLog
    }
    throw
} finally {
    if ($started) {
        Invoke-CheckedNative $pgCtl @('-D', $dataDir, '-w', '-m', 'fast', 'stop')
    }
}
Write-Host "Ready: $pgVersion with PostGIS $($bundle.Version), $pgConfig"
