# uninstall-weaver-task.ps1 — Remove the scheduled task created by
# install-weaver-task.ps1 and stop the daemon if it is running.
#
# Usage:
#   .\scripts\uninstall-weaver-task.ps1
#   .\scripts\uninstall-weaver-task.ps1 -TaskName Foo

[CmdletBinding()]
param(
    [string]$TaskName = 'WeaverDaemon'
)

$ErrorActionPreference = 'Stop'

$task = Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
if (-not $task) {
    Write-Host "No scheduled task named '$TaskName'." -ForegroundColor Yellow
} else {
    Stop-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
    Unregister-ScheduledTask -TaskName $TaskName -Confirm:$false
    Write-Host "Removed scheduled task '$TaskName'." -ForegroundColor Green
}

Stop-Process -Name weaver -Force -ErrorAction SilentlyContinue
Write-Host "Stopped any running weaver process."
