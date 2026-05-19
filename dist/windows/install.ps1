#Requires -Version 5.1
<#
.SYNOPSIS
  Install Synbad as a per-user Windows Scheduled Task that autostarts at login.

.DESCRIPTION
  Builds release binaries with cargo, copies them under %LOCALAPPDATA%\Synbad,
  and registers a Task Scheduler entry that launches synbadd at user logon.
  Re-running the script overwrites the existing task in place.

  We use Task Scheduler (not Services) because synbadd talks to the user's
  desktop session — a Service running as SYSTEM has no access to the
  per-user graphical context the Core needs.

.NOTES
  Run from PowerShell — does not require admin rights. The task is per-user.
#>

$ErrorActionPreference = 'Stop'

$RepoRoot = Resolve-Path (Join-Path $PSScriptRoot '..\..')
$InstallDir = Join-Path $env:LOCALAPPDATA 'Synbad'
$DaemonExe = Join-Path $InstallDir 'synbadd.exe'
$GuiExe = Join-Path $InstallDir 'synbad-gui.exe'
$TaskName = 'SynbadDaemon'

Write-Host "[synbad] building release binaries"
Push-Location $RepoRoot
try {
  & cargo build --release -p synbadd -p synbad-gui
  if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }
}
finally {
  Pop-Location
}

Write-Host "[synbad] installing binaries to $InstallDir"
New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
Copy-Item -Force (Join-Path $RepoRoot 'target\release\synbadd.exe') $DaemonExe
Copy-Item -Force (Join-Path $RepoRoot 'target\release\synbad-gui.exe') $GuiExe

Write-Host "[synbad] registering scheduled task '$TaskName' for $env:USERNAME"

# Remove any prior registration so re-running the installer is idempotent.
$existing = Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
if ($null -ne $existing) {
  Unregister-ScheduledTask -TaskName $TaskName -Confirm:$false
}

$action = New-ScheduledTaskAction -Execute $DaemonExe
$trigger = New-ScheduledTaskTrigger -AtLogOn -User $env:USERNAME
$settings = New-ScheduledTaskSettingsSet `
  -AllowStartIfOnBatteries `
  -DontStopIfGoingOnBatteries `
  -StartWhenAvailable `
  -RestartCount 3 `
  -RestartInterval (New-TimeSpan -Minutes 1) `
  -ExecutionTimeLimit (New-TimeSpan -Seconds 0)  # 0 = no limit
$principal = New-ScheduledTaskPrincipal -UserId $env:USERNAME -LogonType Interactive -RunLevel Limited

Register-ScheduledTask `
  -TaskName $TaskName `
  -Action $action `
  -Trigger $trigger `
  -Settings $settings `
  -Principal $principal `
  -Description 'Synbad daemon — keyboard/mouse/clipboard sharing supervisor.' | Out-Null

# Start it now so the user doesn't have to log out + back in.
Start-ScheduledTask -TaskName $TaskName

Write-Host ""
Write-Host "[synbad] installed. Useful commands:"
Write-Host "  Get-ScheduledTask -TaskName $TaskName"
Write-Host "  Stop-ScheduledTask  -TaskName $TaskName"
Write-Host "  Start-ScheduledTask -TaskName $TaskName"
Write-Host "  Unregister-ScheduledTask -TaskName $TaskName -Confirm:`$false  # uninstall"
