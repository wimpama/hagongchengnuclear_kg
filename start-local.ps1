[CmdletBinding()]
param(
    [switch]$NoBuild,
    [switch]$CheckOnly,
    [string]$EnvFile
)

# Windows starts the same single backend without Docker. Resources use OSS.
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
$backendDir = $PSScriptRoot
if ([string]::IsNullOrWhiteSpace($EnvFile)) {
    $EnvFile = Join-Path $backendDir '.env'
}
$localDir = Join-Path $backendDir 'data/local'
$qdrantDir = Join-Path $localDir 'qdrant'
$logDir = Join-Path $localDir 'logs'
$qdrantExe = Join-Path $backendDir 'data/tools/qdrant/qdrant.exe'
$serverExe = Join-Path $backendDir 'target/debug/course-kb-server.exe'
$savedEnvironment = @{}
$qdrantProcess = $null
$serverProcess = $null
$runtimeFile = Join-Path $localDir 'runtime.json'

function Set-LocalEnvironment([string]$Name, [AllowEmptyString()][string]$Value) {
    if (-not $savedEnvironment.ContainsKey($Name)) {
        $savedEnvironment[$Name] = [Environment]::GetEnvironmentVariable($Name, 'Process')
    }
    [Environment]::SetEnvironmentVariable($Name, $Value, 'Process')
}

function Assert-FreePort([int]$Port) {
    $listener = [Net.Sockets.TcpListener]::new([Net.IPAddress]::Loopback, $Port)
    try {
        $listener.Server.ExclusiveAddressUse = $true
        $listener.Start()
    } catch {
        throw "Local port $Port is occupied. Stop the service that owns it first; this script will not stop unrelated processes."
    } finally {
        $listener.Stop()
    }
}

function Wait-LocalReady([string]$Url, [Diagnostics.Process]$Process, [string]$Name) {
    $deadline = [DateTime]::UtcNow.AddSeconds(30)
    while ([DateTime]::UtcNow -lt $deadline) {
        if ($Process.HasExited) {
            throw "$Name exited during startup. See logs in $logDir."
        }
        try {
            $response = Invoke-WebRequest -UseBasicParsing -Uri $Url -TimeoutSec 2
            if ($response.StatusCode -eq 200) { return }
        } catch {
            # Connection refusal is expected while the child is starting.
        }
        Start-Sleep -Milliseconds 250
    }
    throw "$Name was not ready within 30 seconds. See logs in $logDir."
}

try {
    if (-not (Test-Path -LiteralPath $EnvFile -PathType Leaf)) {
        throw 'Missing back_end/.env. Copy env.example to .env and configure provider keys and ENCRYPTION_MASTER_KEY.'
    }
    # Parse literal KEY=VALUE lines. Never execute or expand .env contents.
    $lineNumber = 0
    foreach ($line in [IO.File]::ReadAllLines((Resolve-Path -LiteralPath $EnvFile).Path)) {
        $lineNumber++
        if ($line -match '^\s*(#|$)') { continue }
        if ($line -notmatch '^\s*([A-Za-z_][A-Za-z0-9_]*)\s*=(.*)$') {
            throw "Invalid .env assignment at line $lineNumber. Expected KEY=VALUE."
        }
        $name = $Matches[1]
        $value = $Matches[2].Trim()
        if ($value.Length -ge 2 -and (
            ($value.StartsWith('"') -and $value.EndsWith('"')) -or
            ($value.StartsWith("'") -and $value.EndsWith("'"))
        )) {
            $value = $value.Substring(1, $value.Length - 2)
        }
        if ($name -notmatch '^(AUTH_|ARK_|LLM_|DEEPSEEK_|ASR_|KB_|QDRANT_|OSS_|COMPOSE_PROJECT_NAME$|ENCRYPTION_MASTER_KEY$|RUST_LOG$|TOKIO_WORKER_THREADS$)') {
            throw "Unsupported .env variable at line $lineNumber. Only knowledge-service configuration is accepted."
        }
        Set-LocalEnvironment $name $value
    }
    $llmKeyName = if ($env:LLM_PROVIDER -eq 'openai') { 'LLM_API_KEY' } else { 'DEEPSEEK_API_KEY' }
    foreach ($name in @('ARK_API_KEY', $llmKeyName, 'ENCRYPTION_MASTER_KEY', 'OSS_ACCESS_KEY_ID', 'OSS_ACCESS_KEY_SECRET')) {
        if ([string]::IsNullOrWhiteSpace([Environment]::GetEnvironmentVariable($name, 'Process'))) {
            throw "Configure $name in back_end/.env before starting."
        }
    }
    try {
        if ([Convert]::FromBase64String($env:ENCRYPTION_MASTER_KEY).Length -ne 32) { throw 'length' }
    } catch {
        throw 'ENCRYPTION_MASTER_KEY must be the same persistent base64-encoded 32-byte key.'
    }
    Set-LocalEnvironment 'KB_STORAGE_BACKEND' 'oss'
    if (-not (Test-Path -LiteralPath $qdrantExe -PathType Leaf)) {
        throw 'Qdrant is missing. Put the official Windows Qdrant binary at back_end/data/tools/qdrant/qdrant.exe. See back_end/README.md.'
    }
    if (-not $NoBuild -and -not (Get-Command cargo -ErrorAction SilentlyContinue)) {
        throw 'Rust/Cargo is missing. Install Rust 1.94 or later, or use -NoBuild with an existing backend binary.'
    }
    # WindowsApps can expose a python.exe placeholder that exits successfully
    # from discovery but cannot run a parser. Resolve and probe the interpreter.
    $pythonCandidates = @()
    if (-not [string]::IsNullOrWhiteSpace($env:KB_PYTHON_BIN)) {
        $pythonCandidates += @{ executable = $env:KB_PYTHON_BIN; arguments = @() }
    }
    if (Get-Command py -ErrorAction SilentlyContinue) {
        $pythonCandidates += @{ executable = 'py'; arguments = @('-3') }
    }
    $pythonCandidates += @{ executable = 'python'; arguments = @() }
    $pythonCandidates += @{ executable = 'python3'; arguments = @() }
    $resolvedPython = $null
    foreach ($candidate in $pythonCandidates) {
        if (-not (Get-Command $candidate.executable -ErrorAction SilentlyContinue)) { continue }
        try {
            $pythonArgs = @($candidate.arguments) + @('-I', '-c', 'import sys,json,zipfile,xml.etree.ElementTree; print(sys.executable)')
            $pythonPath = & $candidate.executable @pythonArgs 2>$null
            if ($LASTEXITCODE -eq 0 -and $pythonPath -and (Test-Path -LiteralPath ([string]$pythonPath).Trim() -PathType Leaf)) {
                $resolvedPython = ([string]$pythonPath).Trim()
                break
            }
        } catch { continue }
    }
    if ($resolvedPython) {
        Set-LocalEnvironment 'KB_PYTHON_BIN' $resolvedPython
        & $resolvedPython -I -c "import importlib.util,sys; sys.exit(0 if importlib.util.find_spec('pypdf') else 1)"
        if ($LASTEXITCODE -ne 0) {
            Write-Warning 'PDF parsing needs pypdf. Install once with the selected Python: -m pip install -r processing/requirements.txt'
        }
    } else {
        Write-Warning 'A working Python interpreter was not found; PDF/DOCX/PPTX extraction needs Python. Set KB_PYTHON_BIN in back_end/.env to its executable path.'
    }
    foreach ($command in @('ffmpeg', 'ffprobe')) {
        if (-not (Get-Command $command -ErrorAction SilentlyContinue)) {
            Write-Warning "$command is unavailable; the corresponding media/document features need it."
        }
    }
    foreach ($command in @('pdfinfo', 'pdftoppm')) {
        $override = [Environment]::GetEnvironmentVariable('KB_' + $command.ToUpperInvariant() + '_BIN', 'Process')
        $configuredTool = if ($env:KB_POPPLER_BIN_DIR) { Join-Path $env:KB_POPPLER_BIN_DIR ($command + '.exe') } else { '' }
        $toolExists = -not [string]::IsNullOrWhiteSpace($configuredTool) -and (Test-Path -LiteralPath $configuredTool -PathType Leaf)
        if (-not $override -and -not $toolExists -and -not (Get-Command $command -ErrorAction SilentlyContinue)) {
            Write-Warning "$command is unavailable; PDF page rendering needs Poppler. Set KB_POPPLER_BIN_DIR in back_end/.env."
        }
    }
    foreach ($port in @(8080, 16333, 16334)) { Assert-FreePort $port }
    if ($CheckOnly) {
        Write-Host 'Local prerequisites, configuration, and ports are ready. No services were started.'
        return
    }
    if (-not $NoBuild) {
        Push-Location $backendDir
        try {
            Set-LocalEnvironment 'CARGO_INCREMENTAL' '0'
            & cargo build --locked --offline
            if ($LASTEXITCODE -ne 0) {
                throw 'Offline Rust build failed. If dependencies are missing, run cargo fetch --locked once with network access, then retry.'
            }
        } finally { Pop-Location }
    }
    if (-not (Test-Path -LiteralPath $serverExe -PathType Leaf)) {
        throw 'Backend binary is missing. Retry without --no-build / -NoBuild.'
    }
    foreach ($port in @(8080, 16333, 16334)) { Assert-FreePort $port }
    foreach ($directory in @($localDir, $qdrantDir, $logDir, (Join-Path $localDir 'cache/dev'), (Join-Path $localDir 'cache/prod'))) {
        [IO.Directory]::CreateDirectory($directory) | Out-Null
    }
    $runId = [DateTime]::Now.ToString('yyyyMMdd-HHmmss') + '-' + [Guid]::NewGuid().ToString('N').Substring(0, 8)
    # Windows Qdrant can fail to clear an old snapshot staging directory.
    # Give each run a fresh snapshot directory and retain every older directory.
    # The persistent vector storage path stays unchanged across all runs.
    $qdrantConfig = Join-Path $qdrantDir 'local.yaml'
    $config = @'
storage:
  storage_path: ./storage
  snapshots_path: ./snapshots/__SNAPSHOT_RUN__
service:
  host: 127.0.0.1
  http_port: 16333
  grpc_port: 16334
telemetry_disabled: true
'@
    [IO.File]::WriteAllText($qdrantConfig, $config.Replace('__SNAPSHOT_RUN__', $runId), [Text.UTF8Encoding]::new($false))
    # Always isolate preview data from Compose data and earlier test collections.
    $databasePath = (Join-Path $localDir 'knowledge.sqlite').Replace('\', '/')
    Set-LocalEnvironment 'KB_BIND_ADDR' '127.0.0.1:8080'
    Set-LocalEnvironment 'KB_DATABASE_URL' ("sqlite://" + $databasePath + '?mode=rwc')
    Set-LocalEnvironment 'KB_DEV_DATABASE_URL' ("sqlite://" + $databasePath + '?mode=rwc')
    Set-LocalEnvironment 'KB_PROD_DATABASE_URL' ("sqlite://" + (Join-Path $localDir 'prod/knowledge.sqlite').Replace('\', '/') + '?mode=rwc')
    Set-LocalEnvironment 'KB_UPLOADS_DIR' ''
    Set-LocalEnvironment 'KB_CACHE_DIR' (Join-Path $localDir 'cache')
    Set-LocalEnvironment 'QDRANT_HTTP_URL' 'http://127.0.0.1:16333'
    Set-LocalEnvironment 'QDRANT_GRPC_URL' 'http://127.0.0.1:16334'
    Set-LocalEnvironment 'QDRANT_API_KEY' ''
    Set-LocalEnvironment 'TOKIO_WORKER_THREADS' '2'
    $qdrantOut = Join-Path $logDir "$runId-qdrant.log"
    $qdrantErr = Join-Path $logDir "$runId-qdrant-error.log"
    $serverOut = Join-Path $logDir "$runId-server.log"
    $serverErr = Join-Path $logDir "$runId-server-error.log"
    $qdrantProcess = Start-Process -FilePath $qdrantExe -WorkingDirectory $qdrantDir `
        -ArgumentList @('--config-path', ('"' + $qdrantConfig + '"')) -WindowStyle Hidden -PassThru `
        -RedirectStandardOutput $qdrantOut -RedirectStandardError $qdrantErr
    Wait-LocalReady 'http://127.0.0.1:16333/readyz' $qdrantProcess 'Qdrant'
    $serverProcess = Start-Process -FilePath $serverExe -WorkingDirectory $backendDir `
        -WindowStyle Hidden -PassThru -RedirectStandardOutput $serverOut -RedirectStandardError $serverErr
    Wait-LocalReady 'http://127.0.0.1:8080/readyz' $serverProcess 'Knowledge backend'
    @{ qdrantPid = $qdrantProcess.Id; serverPid = $serverProcess.Id; startedAt = [DateTime]::UtcNow.ToString('o'); dataDir = $localDir } |
        ConvertTo-Json | Set-Content -LiteralPath $runtimeFile -Encoding UTF8
    Write-Host 'Knowledge backend is ready: http://127.0.0.1:8080'
    Write-Host "SQLite and Qdrant data: $localDir; resource originals: OSS"
    Write-Host "Logs: $logDir"
    Write-Host 'Keep this terminal open. Ctrl+C stops only the two services started by this script.'
    while (-not $serverProcess.HasExited -and -not $qdrantProcess.HasExited) {
        Start-Sleep -Milliseconds 500
    }
    throw "A local service exited. See logs in $logDir."
} catch {
    Write-Error $_ -ErrorAction Continue
    exit 1
} finally {
    $cleanupComplete = $true
    foreach ($child in @($serverProcess, $qdrantProcess)) {
        try {
            if ($null -ne $child -and -not $child.HasExited) {
                Stop-Process -InputObject $child -ErrorAction Stop
                if (-not $child.WaitForExit(5000)) {
                    $cleanupComplete = $false
                    Write-Warning 'A local child process did not exit within five seconds.'
                }
            }
        } catch {
            $cleanupComplete = $false
            Write-Warning 'Could not stop a local child process; check data/local/runtime.json.'
        }
    }
    if ($null -ne $serverProcess -or $null -ne $qdrantProcess) {
        if ($cleanupComplete) {
            Remove-Item -LiteralPath $runtimeFile -Force -ErrorAction SilentlyContinue
            Write-Host 'Local knowledge services stopped.'
        } else {
            Write-Warning 'Local cleanup was incomplete. Process metadata was retained.'
        }
    }
    foreach ($name in $savedEnvironment.Keys) {
        [Environment]::SetEnvironmentVariable($name, $savedEnvironment[$name], 'Process')
    }
}
