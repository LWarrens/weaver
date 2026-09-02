# start.ps1 — Run from an external PowerShell window to keep both servers alive
# Usage: .\start.ps1
# To stop: Ctrl+C (stops both jobs and the dev server)

$root = $PSScriptRoot

# Kill any stale weaver process
Stop-Process -Name weaver -Force -ErrorAction SilentlyContinue
Start-Sleep -Milliseconds 500

Write-Host "Starting weaver daemon..." -ForegroundColor Cyan
$daemon = Start-Job -ScriptBlock {
    param($r)
    Set-Location $r
    $env:RUST_LOG = 'info'
    & "$r\target\debug\weaver.exe" `
        --db "$r\arch.db" `
        --embedding-provider lmstudio `
        --embedding-url http://localhost:1234 `
        --embedding-model text-embedding-harrier-oss-v1-270m `
        --llm-provider lmstudio `
        --llm-url http://localhost:1234 `
        --llm-model ternary-bonsai-8b `
        --llm-api-key lm-studio `
        --daemon `
        --bind 127.0.0.1:8444
} -ArgumentList $root

Write-Host "Starting manager-client dev server..." -ForegroundColor Cyan
$devServer = Start-Job -ScriptBlock {
    param($r)
    Set-Location "$r\manager-client"
    npm run dev
} -ArgumentList $root

Write-Host ""
Write-Host "Daemon:     http://127.0.0.1:8444/mcp" -ForegroundColor Green
Write-Host "Dev server: http://localhost:5173/" -ForegroundColor Green
Write-Host ""
Write-Host "Press Ctrl+C to stop both servers." -ForegroundColor Yellow

try {
    while ($true) {
        # Stream job output
        Receive-Job $daemon, $devServer | Write-Host
        Start-Sleep -Seconds 2
    }
} finally {
    Write-Host "Stopping servers..." -ForegroundColor Red
    Stop-Job $daemon, $devServer
    Remove-Job $daemon, $devServer
    Stop-Process -Name weaver -Force -ErrorAction SilentlyContinue
}
