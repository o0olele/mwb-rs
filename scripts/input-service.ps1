#Requires -Version 5.1
[CmdletBinding(SupportsShouldProcess = $true)]
param(
    [ValidateSet('Install', 'Uninstall')]
    [string]$Action = 'Install',
    [string]$BuildDirectory
)

$ErrorActionPreference = 'Stop'
$serviceName = 'MwbRustInput'
$programFilesPath = [IO.Path]::GetFullPath([Environment]::GetFolderPath('ProgramFiles'))
$installDirectory = [IO.Path]::GetFullPath((Join-Path $programFilesPath 'MWB-Rust'))
if ([IO.Path]::GetDirectoryName($installDirectory) -ne $programFilesPath) {
    throw 'The installation directory must be directly beneath Program Files.'
}
$appPath = Join-Path $installDirectory 'mwb.exe'
$brokerPath = Join-Path $installDirectory 'mwb-input-service.exe'
$expectedCommand = '"' + $brokerPath + '" --service'

function Assert-OrdinaryPath([string]$Path) {
    if ((Test-Path -LiteralPath $Path) -and
        ((Get-Item -LiteralPath $Path -Force).Attributes -band [IO.FileAttributes]::ReparsePoint)) {
        throw "Refusing a reparse point: $Path"
    }
}

Assert-OrdinaryPath $programFilesPath
Assert-OrdinaryPath $installDirectory
$allowedNames = @('mwb.exe', 'mwb-input-service.exe', 'input-service.log')
if (Test-Path -LiteralPath $installDirectory) {
    foreach ($entry in (Get-ChildItem -LiteralPath $installDirectory -Force)) {
        Assert-OrdinaryPath $entry.FullName
        if ($entry.PSIsContainer -or $entry.Name -notin $allowedNames) {
            throw "Unexpected item in the service directory; refusing to modify it: $($entry.FullName)"
        }
    }
}

if ($Action -eq 'Install') {
    # Windows PowerShell 5.1 does not populate PSScriptRoot while evaluating
    # advanced-script parameter defaults. Resolve it only after binding.
    if ([string]::IsNullOrWhiteSpace($BuildDirectory)) {
        if ([string]::IsNullOrWhiteSpace($PSScriptRoot)) {
            throw 'Cannot locate the script directory. Run with -File or specify -BuildDirectory.'
        }
        $BuildDirectory = Join-Path -Path $PSScriptRoot -ChildPath '..\target\release'
    }
    $resolvedBuild = (Resolve-Path -LiteralPath $BuildDirectory).Path
    foreach ($name in @('mwb.exe', 'mwb-input-service.exe')) {
        $source = Join-Path $resolvedBuild $name
        if (-not (Test-Path -LiteralPath $source -PathType Leaf)) {
            throw "Build both executables first with cargo build --release --locked. Missing: $source"
        }
        if ([IO.Path]::GetFullPath($source) -eq (Join-Path $installDirectory $name)) {
            throw 'BuildDirectory must differ from the installation directory.'
        }
    }
}

# A single gate also covers native sc.exe and all file/service mutations.
# -WhatIf validates the package and its paths without changing the machine.
$operation = if ($Action -eq 'Install') {
    "Install the MWB input service from '$resolvedBuild'"
} else {
    'Uninstall the MWB input service'
}
if (-not $PSCmdlet.ShouldProcess($installDirectory, $operation)) { return }

$installerIdentity = [Security.Principal.WindowsIdentity]::GetCurrent()
try {
    $installerPrincipal = [Security.Principal.WindowsPrincipal]::new($installerIdentity)
    if (-not $installerPrincipal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
        throw 'Install and Uninstall require an administrator PowerShell window. Use -WhatIf to preview without elevation.'
    }
} finally { $installerIdentity.Dispose() }

$existing = Get-CimInstance Win32_Service -Filter "Name='$serviceName'"
if ($existing -and $existing.PathName -ne $expectedCommand) {
    throw 'A different service already uses this name. Its configuration was not changed.'
}

$runningApp = Get-CimInstance Win32_Process -Filter "Name='mwb.exe'" |
    Where-Object { $_.ExecutablePath -eq $appPath }
if ($runningApp) {
    throw "Close the installed MWB window before continuing: $appPath"
}
if ($existing) {
    Stop-Service -Name $serviceName -ErrorAction Stop
    (Get-Service -Name $serviceName).WaitForStatus('Stopped', [TimeSpan]::FromSeconds(15))
}

if ($Action -eq 'Uninstall') {
    if ($existing) {
        & sc.exe delete $serviceName
        if ($LASTEXITCODE -ne 0) { throw 'The service could not be deleted.' }
    }
    # Only the verified, exact application paths are removed. User settings and
    # received files live elsewhere and are never touched by this script.
    foreach ($name in $allowedNames) {
        $targetFile = [IO.Path]::GetFullPath((Join-Path $installDirectory $name))
        if ([IO.Path]::GetDirectoryName($targetFile) -ne $installDirectory) {
            throw 'Unexpected uninstall path.'
        }
        if (Test-Path -LiteralPath $targetFile) { Remove-Item -LiteralPath $targetFile -Force }
    }
    if ((Test-Path -LiteralPath $installDirectory) -and
        @(Get-ChildItem -LiteralPath $installDirectory -Force).Count -eq 0) {
        Remove-Item -LiteralPath $installDirectory
    }
    Write-Host 'MWB input service removed. User settings were preserved.'
    return
}

New-Item -ItemType Directory -Path $installDirectory -Force | Out-Null
$directoryAcl = [Security.AccessControl.DirectorySecurity]::new()
$directoryAcl.SetSecurityDescriptorSddlForm('O:BAG:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;0x1200a9;;;BU)')
Set-Acl -LiteralPath $installDirectory -AclObject $directoryAcl
$fileAcl = [Security.AccessControl.FileSecurity]::new()
$fileAcl.SetSecurityDescriptorSddlForm('O:BAG:BAD:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;0x1200a9;;;BU)')
foreach ($name in @('mwb.exe', 'mwb-input-service.exe')) {
    $destination = Join-Path $installDirectory $name
    # Remove the old directory entry first, so an old hard link cannot cause a
    # privileged overwrite of a file outside the installation directory.
    if (Test-Path -LiteralPath $destination) { Remove-Item -LiteralPath $destination -Force }
    Copy-Item -LiteralPath (Join-Path $resolvedBuild $name) -Destination $destination
    Set-Acl -LiteralPath $destination -AclObject $fileAcl
}
$logPath = Join-Path $installDirectory 'input-service.log'
if (Test-Path -LiteralPath $logPath) { Remove-Item -LiteralPath $logPath -Force }
if (-not $existing) {
    New-Service -Name $serviceName -BinaryPathName $expectedCommand -StartupType Automatic `
        -DisplayName 'MWB Rust Lock Screen Input' `
        -Description 'Receives authenticated MWB keyboard and mouse input in the active console session.' | Out-Null
}
Start-Service -Name $serviceName
(Get-Service -Name $serviceName).WaitForStatus('Running', [TimeSpan]::FromSeconds(10))
Write-Host 'Installed. On the receiving computer, launch this copy and join the workgroup before locking:'
Write-Host $appPath
Write-Host 'The input service does not save pairing keys or start the MWB window automatically.'
