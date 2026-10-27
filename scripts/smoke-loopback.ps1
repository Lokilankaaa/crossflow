param([string]$Binary = '', [int]$Port = 24801)
$ErrorActionPreference = 'Stop'
$projectRoot = Split-Path -Parent $PSScriptRoot
if (-not $Binary) { $Binary = Join-Path $projectRoot 'target/release/crossflow.exe' }
if (-not (Test-Path -LiteralPath $Binary)) { throw 'Build the release binary first.' }
$runId = 'loopback-' + (Get-Date -Format 'yyyyMMdd-HHmmss-fff')
$runDir = Join-Path $projectRoot "runs/$runId"
New-Item -ItemType Directory -Path $runDir | Out-Null
$pairDir = Join-Path $runDir 'pairing'
& $Binary pair --out $pairDir | Out-Null
if ($LASTEXITCODE -ne 0) { throw 'Pair generation failed.' }
$receiverLog = Join-Path $runDir 'receiver.jsonl'
$senderLog = Join-Path $runDir 'sender.jsonl'
$receiverArgs = @('receive','--bind',"127.0.0.1:$Port",'--identity',"`"$pairDir/mac`"",'--log',"`"$receiverLog`"",'--seconds','6')
$receiver = Start-Process -FilePath $Binary -ArgumentList $receiverArgs -WindowStyle Hidden -PassThru -RedirectStandardOutput (Join-Path $runDir 'receiver.stdout') -RedirectStandardError (Join-Path $runDir 'receiver.stderr')
try {
    Start-Sleep -Milliseconds 400
    & $Binary send --host "127.0.0.1:$Port" --identity "$pairDir/windows" --log $senderLog --seconds 3 --synthetic --max-hz 1000
    if ($LASTEXITCODE -ne 0) { throw 'Synthetic sender failed.' }
    if (-not $receiver.WaitForExit(10000)) { throw 'Receiver did not terminate.' }
    $receiver.Refresh()
    if ($receiver.ExitCode -ne 0) { throw "Receiver failed; inspect $runDir" }
    $source = & $Binary summarize $senderLog | ConvertFrom-Json
    $target = & $Binary summarize $receiverLog | ConvertFrom-Json
    if ($target.injection_call.samples -lt 100) { throw 'Insufficient delivered movement samples.' }
    if (-not $source.complete_log -or -not $target.complete_log) { throw 'Incomplete logs.' }
    if ($source.dropped_log_records -gt 0 -or $target.dropped_log_records -gt 0) { throw 'Dropped log records.' }
    [pscustomobject]@{ Run=$runId; Sent=$source.source_processing.samples; AppliedDryRun=$target.injection_call.samples; SourceP95Ms=$source.source_processing.p95_ms; ReceiveP95Ms=$target.receiver_processing.p95_ms; VisibleSmoothness='NOT_MEASURED'; Logs=$runDir } | Format-List
} finally {
    if (-not $receiver.HasExited) { $receiver.Kill(); $receiver.WaitForExit() }
    $receiver.Dispose()
}
