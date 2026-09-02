# run-weaver-daemon.ps1 — Launch the weaver daemon with logging.
#
# Invoked by the "WeaverDaemon" scheduled task (see install-weaver-task.ps1),
# but also runnable by hand for a foreground daemon that logs to a file.
#
# Configuration resolution, in order of preference:
#   1. .weaver.yaml at the repo root  -> weaver --config .weaver.yaml
#   2. the -*/env parameters below     -> explicit CLI flags
#
# CLI flags / WEAVER_* environment variables always override .weaver.yaml,
# so you can keep a checked-in .weaver.yaml and still tweak a single value.

[CmdletBinding()]
param(
    [string]$Bind             = $env:WEAVER_BIND             ?? '127.0.0.1:8444',
    [string]$Db               = $env:WEAVER_DB,
    [string]$EmbeddingProvider = $env:WEAVER_EMBEDDING_PROVIDER ?? 'lmstudio',
    [string]$EmbeddingUrl      = $env:WEAVER_EMBEDDING_URL      ?? 'http://localhost:1234',
    [string]$EmbeddingModel    = $env:WEAVER_EMBEDDING_MODEL    ?? 'text-embedding-harrier-oss-v1-270m',
    [string]$LlmProvider       = $env:WEAVER_LLM_PROVIDER       ?? 'lmstudio',
    [string]$LlmUrl            = $env:WEAVER_LLM_URL            ?? 'http://localhost:1234',
    [string]$LlmModel          = $env:WEAVER_LLM_MODEL          ?? 'ternary-bonsai-8b',
    [string]$LlmApiKey         = $env:WEAVER_LLM_API_KEY        ?? 'lm-studio',
    [string]$LogDir
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot

if (-not $Db)     { $Db = Join-Path $root 'arch.db' }
if (-not $LogDir) { $LogDir = Join-Path $root 'logs' }
New-Item -ItemType Directory -Force -Path $LogDir | Out-Null

# Prefer the release binary; fall back to debug.
$exe = Join-Path $root 'target\release\weaver.exe'
if (-not (Test-Path $exe)) { $exe = Join-Path $root 'target\debug\weaver.exe' }
if (-not (Test-Path $exe)) {
    throw "weaver.exe not found. Build it first: cargo build --release"
}

Set-Location $root
if (-not $env:RUST_LOG) { $env:RUST_LOG = 'info' }

# Rotate a single previous log so a crash loop cannot fill the disk.
$log = Join-Path $LogDir 'weaver-daemon.log'
if (Test-Path $log) { Move-Item -Force $log (Join-Path $LogDir 'weaver-daemon.prev.log') }

$configFile = Join-Path $root '.weaver.yaml'
if (Test-Path $configFile) {
    Write-Host "Starting weaver via .weaver.yaml -> $log"
    $args = @('--config', $configFile, '--daemon', '--bind', $Bind)
} else {
    Write-Host "Starting weaver with explicit flags -> $log"
    $args = @(
        '--db', $Db,
        '--daemon', '--bind', $Bind,
        '--embedding-provider', $EmbeddingProvider,
        '--embedding-url', $EmbeddingUrl,
        '--embedding-model', $EmbeddingModel,
        '--llm-provider', $LlmProvider,
        '--llm-url', $LlmUrl,
        '--llm-model', $LlmModel,
        '--llm-api-key', $LlmApiKey
    )
}

# weaver writes tracing output to stderr; capture both streams into the log.
& $exe @args *>&1 | Tee-Object -FilePath $log
exit $LASTEXITCODE
