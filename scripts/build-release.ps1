#Requires -Version 5.1
[CmdletBinding()]
param(
    [string]$WixDirectory,
    [string]$BuildDirectory,
    [string]$OutputDirectory,
    [switch]$SkipBuild
)
$ErrorActionPreference = 'Stop'
$sourceRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
if ([string]::IsNullOrWhiteSpace($BuildDirectory)) {
    $BuildDirectory = Join-Path $sourceRoot 'target\release'
}
if ([string]::IsNullOrWhiteSpace($OutputDirectory)) {
    $OutputDirectory = Join-Path $sourceRoot 'artifacts'
}

# The installer script builds both binaries and validates their x64 format and version.
& (Join-Path $PSScriptRoot 'build-installer.ps1') -WixDirectory $WixDirectory `
    -BuildDirectory $BuildDirectory -OutputDirectory $OutputDirectory -SkipBuild:$SkipBuild

$manifest = Get-Content -LiteralPath (Join-Path $sourceRoot 'Cargo.toml') -Raw
$versionMatch = [regex]::Match($manifest, '(?m)^version\s*=\s*"(\d+\.\d+\.\d+)"')
if (-not $versionMatch.Success) { throw 'Cannot read the package version.' }
$version = $versionMatch.Groups[1].Value
$buildRoot = (Resolve-Path -LiteralPath $BuildDirectory).Path
$outputRoot = (Resolve-Path -LiteralPath $OutputDirectory).Path
$packagePath = Join-Path $outputRoot "mwb-v$version-windows-x64-portable.zip"

Add-Type -AssemblyName System.IO.Compression, System.IO.Compression.FileSystem
if (Test-Path -LiteralPath $packagePath) { Remove-Item -LiteralPath $packagePath -Force }
$archive = [IO.Compression.ZipFile]::Open($packagePath, [IO.Compression.ZipArchiveMode]::Create)
try {
    foreach ($name in @('mwb.exe', 'mwb-input-service.exe')) {
        $null = [IO.Compression.ZipFileExtensions]::CreateEntryFromFile(
            $archive, (Join-Path $buildRoot $name), $name, [IO.Compression.CompressionLevel]::Optimal)
    }
    $documents = @(Get-Item -LiteralPath (Join-Path $sourceRoot 'README.md'),(Join-Path $sourceRoot 'CHANGELOG.md'))
    $documents += @(Get-ChildItem -LiteralPath (Join-Path $sourceRoot 'docs') -Recurse -File)
    foreach ($document in ($documents | Sort-Object FullName)) {
        $entryName = $document.FullName.Substring($sourceRoot.Length + 1).Replace('\', '/')
        $null = [IO.Compression.ZipFileExtensions]::CreateEntryFromFile(
            $archive, $document.FullName, $entryName, [IO.Compression.CompressionLevel]::Optimal)
    }
} finally { $archive.Dispose() }

$digest = (Get-FileHash -LiteralPath $packagePath -Algorithm SHA256).Hash.ToLowerInvariant()
[IO.File]::WriteAllText("$packagePath.sha256", "$digest  $([IO.Path]::GetFileName($packagePath))`r`n",
    (New-Object Text.UTF8Encoding($false)))
Write-Output "Built: $packagePath"
