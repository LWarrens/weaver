# install-weaver-task.ps1 — Register a per-user scheduled task that starts the
# weaver daemon at logon.
#
# This is deliberately NOT a Windows service: the daemon depends on LM Studio
# (or another provider) running in your interactive session, so it must start
# after logon and run as you, not as LocalSystem at boot.
#
# Usage:
#   .\scripts\install-weaver-task.ps1                 # start at every logon
#   .\scripts\install-weaver-task.ps1 -RunNow         # ...and start it immediately
#   .\scripts\install-weaver-task.ps1 -TaskName Foo   # custom task name
#
# Uninstall with .\scripts\uninstall-weaver-task.ps1

[CmdletBinding()]
param(
    [string]$TaskName = 'WeaverDaemon',
    [switch]$RunNow
)

$ErrorActionPreference = 'Stop'
$root   = Split-Path -Parent $PSScriptRoot
$runner = Join-Path $PSScriptRoot 'run-weaver-daemon.ps1'
if (-not (Test-Path $runner)) { throw "missing $runner" }

$pwsh = (Get-Command pwsh -ErrorAction SilentlyContinue)?.Source
if (-not $pwsh) { $pwsh = (Get-Command powershell).Source }

$action = New-ScheduledTaskAction `
    -Execute $pwsh `
    -Argument "-NoProfile -WindowStyle Hidden -ExecutionPolicy Bypass -File `"$runner`"" `
    -WorkingDirectory $root

$trigger = New-ScheduledTaskTrigger -AtLogOn -User $env:USERNAME

$settings = New-ScheduledTaskSettingsSet `
    -AllowStartIfOnBatteries `
    -DontStopIfGoingOnBatteries `
    -StartWhenAvailable `
    -RestartCount 3 `
    -RestartInterval (New-TimeSpan -Minutes 1) `
    -ExecutionTimeLimit ([TimeSpan]::Zero)

$principal = New-ScheduledTaskPrincipal -UserId "$env:USERDOMAIN\$env:USERNAME" -LogonType Interactive -RunLevel Limited

Register-ScheduledTask -TaskName $TaskName -Action $action -Trigger $trigger `
    -Settings $settings -Principal $principal -Force `
    -Description 'Starts the weaver architectural-memory MCP daemon at logon.' | Out-Null

Write-Host "Registered scheduled task '$TaskName'." -ForegroundColor Green
Write-Host "  Runner: $runner"
Write-Host "  Logs:   $(Join-Path $root 'logs\weaver-daemon.log')"

if ($RunNow) {
    Start-ScheduledTask -TaskName $TaskName
    Write-Host "Started '$TaskName' now." -ForegroundColor Green
} else {
    Write-Host "It will start at your next logon, or run: Start-ScheduledTask -TaskName $TaskName"
}
