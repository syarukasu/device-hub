[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [string]$Executable
)

$ErrorActionPreference = 'Stop'
$source = (Resolve-Path -LiteralPath $Executable).Path
if ([IO.Path]::GetFileName($source) -ine 'device-hub.exe') {
    throw 'Specify a built device-hub.exe.'
}

$installDirectory = Join-Path $env:LOCALAPPDATA 'Programs\DeviceHub'
$installedExe = Join-Path $installDirectory 'device-hub.exe'
$account = [Security.Principal.WindowsIdentity]::GetCurrent().Name
$accountName = ($account -split '\\')[-1]
$taskName = "Device HUB ($accountName)"

if ($source -ne $installedExe -and (Get-Process -Name 'device-hub' -ErrorAction SilentlyContinue |
        Where-Object { $_.Path -eq $installedExe })) {
    throw 'Close Device HUB from its notification-area menu before updating it.'
}

New-Item -ItemType Directory -Path $installDirectory -Force | Out-Null
if ($source -ne $installedExe) {
    Copy-Item -LiteralPath $source -Destination $installedExe
}

$action = New-ScheduledTaskAction -Execute $installedExe -Argument '--tray' -WorkingDirectory $installDirectory
$trigger = New-ScheduledTaskTrigger -AtLogOn -User $account
$principal = New-ScheduledTaskPrincipal -UserId $account -LogonType Interactive -RunLevel Limited
$residentSettings = @{
    ExecutionTimeLimit = [TimeSpan]::Zero
    MultipleInstances = 'IgnoreNew'
    AllowStartIfOnBatteries = $true
    DontStopIfGoingOnBatteries = $true
    RestartCount = 3
    RestartInterval = New-TimeSpan -Minutes 1
}
$settings = New-ScheduledTaskSettingsSet @residentSettings
Register-ScheduledTask -TaskName $taskName -Action $action -Trigger $trigger -Principal $principal -Settings $settings -Force | Out-Null

$startMenu = [Environment]::GetFolderPath('Programs')
$shortcutPath = Join-Path $startMenu 'Device HUB.lnk'
$shell = New-Object -ComObject WScript.Shell
$shortcut = $shell.CreateShortcut($shortcutPath)
$shortcut.TargetPath = $installedExe
$shortcut.WorkingDirectory = $installDirectory
$shortcut.Save()

Start-ScheduledTask -TaskName $taskName
Write-Output "Installed: $installedExe"
Write-Output "Logon task: $taskName"
