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
if ([string]::IsNullOrWhiteSpace($WixDirectory)) {
    $compiler = Get-Command candle.exe -ErrorAction SilentlyContinue
    if ($compiler) { $WixDirectory = Split-Path -Parent $compiler.Source }
    elseif ($env:WIX) { $WixDirectory = Join-Path $env:WIX 'bin' }
    else { throw 'WiX Toolset 3.14 is required to build the MSI. Set -WixDirectory to its bin directory.' }
}
$wixRoot = (Resolve-Path -LiteralPath $WixDirectory).Path
foreach ($tool in @('candle.exe', 'light.exe', 'WixUIExtension.dll', 'WixUtilExtension.dll')) {
    if (-not (Test-Path -LiteralPath (Join-Path $wixRoot $tool) -PathType Leaf)) {
        throw "Missing WiX build tool: $tool"
    }
}
$manifest = Get-Content -LiteralPath (Join-Path $sourceRoot 'Cargo.toml') -Raw
$versionMatch = [regex]::Match($manifest, '(?m)^version\s*=\s*"(\d+\.\d+\.\d+)"')
if (-not $versionMatch.Success) { throw 'Cannot read the package version.' }
$version = $versionMatch.Groups[1].Value
if (-not $SkipBuild) {
    Push-Location -LiteralPath $sourceRoot
    try {
        & cargo build --release --locked
        if ($LASTEXITCODE -ne 0) { throw 'Release build failed.' }
    } finally { Pop-Location }
}
$buildRoot = (Resolve-Path -LiteralPath $BuildDirectory).Path
foreach ($name in @('mwb.exe', 'mwb-input-service.exe')) {
    $bytes = [IO.File]::ReadAllBytes((Join-Path $buildRoot $name))
    $offset = [BitConverter]::ToInt32($bytes, 0x3c)
    if ([BitConverter]::ToUInt32($bytes, $offset) -ne 0x00004550 -or
        [BitConverter]::ToUInt16($bytes, $offset + 4) -ne 0x8664) {
        throw "The installer requires a Windows x64 executable: $name"
    }
    $nativeVersion = [Diagnostics.FileVersionInfo]::GetVersionInfo((Join-Path $buildRoot $name))
    if ($nativeVersion.ProductVersion -ne $version) {
        throw "Executable version mismatch for $name. Rebuild v$version before packaging."
    }
}
$null = New-Item -ItemType Directory -Path $OutputDirectory -Force
$outputRoot = (Resolve-Path -LiteralPath $OutputDirectory).Path
$objectDirectory = Join-Path $sourceRoot "target\installer\$version"
$null = New-Item -ItemType Directory -Path $objectDirectory -Force
$objectPath = Join-Path $objectDirectory 'mwb.wixobj'
$packagePath = Join-Path $outputRoot "mwb-v$version-windows-x64.msi"
& (Join-Path $wixRoot 'candle.exe') -nologo -wx -arch x64 "-dVersion=$version" `
    "-dBuildDirectory=$buildRoot" -out $objectPath (Join-Path $sourceRoot 'installer\mwb.wxs')
if ($LASTEXITCODE -ne 0) { throw 'Installer compilation failed.' }
& (Join-Path $wixRoot 'light.exe') -nologo -wx -cultures:zh-cn `
    -ext (Join-Path $wixRoot 'WixUIExtension.dll') -ext (Join-Path $wixRoot 'WixUtilExtension.dll') `
    -out $packagePath -pdbout (Join-Path $objectDirectory 'mwb.wixpdb') $objectPath
if ($LASTEXITCODE -ne 0) { throw 'Installer linking or MSI validation failed.' }
$digest = (Get-FileHash -LiteralPath $packagePath -Algorithm SHA256).Hash.ToLowerInvariant()
[IO.File]::WriteAllText("$packagePath.sha256", "$digest  $([IO.Path]::GetFileName($packagePath))`r`n",
    (New-Object Text.UTF8Encoding($false)))
Write-Output "Built and validated: $packagePath"
