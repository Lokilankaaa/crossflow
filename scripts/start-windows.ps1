<#
.SYNOPSIS
Start a bounded Crossflow Windows mouse-sharing experiment.
.DESCRIPTION
Relative identity and log directory paths are resolved from the repository root.
Build and pair first. This script never creates or changes an identity.
Normal mode starts paused; use the control window or Ctrl+Alt+Space to share.
Synthetic mode is only for a Mac DrySink (omit --inject on the Mac).
.EXAMPLE
.\scripts\start-windows.ps1 -MacHost 192.168.50.2
.EXAMPLE
.\scripts\start-windows.ps1 -MacHost 192.168.50.2 -Identity runs\lab\windows -Seconds 10 -DryRunSynthetic
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [ValidateNotNullOrEmpty()]
    [string]$MacHost,
    [ValidateRange(1, 65535)]
    [int]$Port = 24800,
    [ValidateNotNullOrEmpty()]
    [string]$Identity = '.pairing/windows',
    [ValidateRange(1, 3600)]
    [int]$Seconds = 180,
    [ValidateRange(60, 8000)]
    [int]$MaxHz = 1000,
    [ValidateNotNullOrEmpty()]
    [string]$LogDirectory = 'runs',
    [switch]$DryRunSynthetic,
    [switch]$ExperimentalInputs
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
if ($DryRunSynthetic -and $ExperimentalInputs) { throw 'ExperimentalInputs cannot be combined with DryRunSynthetic.' }
$projectRoot = Split-Path -Parent $PSScriptRoot
$binary = Join-Path $projectRoot 'target/release/crossflow.exe'

# Accept a hostname or IP, never a URL or a shell command. Port is separate.
$parsedAddress = $null
if ([System.Net.IPAddress]::TryParse($MacHost, [ref]$parsedAddress)) {
    $destination = if ($parsedAddress.AddressFamily -eq [System.Net.Sockets.AddressFamily]::InterNetworkV6) {
        '[{0}]:{1}' -f $parsedAddress, $Port
    } else {
        '{0}:{1}' -f $parsedAddress, $Port
    }
} elseif ($MacHost -match '^(?=.{1,253}$)[A-Za-z0-9](?:[A-Za-z0-9.-]*[A-Za-z0-9])?$') {
    $destination = '{0}:{1}' -f $MacHost, $Port
} else {
    throw 'MacHost must be an IP address or hostname without a port. Use -Port separately.'
}
if (-not (Test-Path -LiteralPath $binary -PathType Leaf)) {
    throw "Release binary is missing: $binary. Build first with scripts/cargo-local.ps1 -CargoArgs @('build','--release')."
}
function Resolve-ProjectPath([string]$Value) {
    if ([System.IO.Path]::IsPathRooted($Value)) {
        return [System.IO.Path]::GetFullPath($Value)
    }
    return [System.IO.Path]::GetFullPath((Join-Path $projectRoot $Value))
}
$identityPath = Resolve-ProjectPath $Identity
if (-not (Test-Path -LiteralPath (Join-Path $identityPath 'identity.json') -PathType Leaf)) {
    throw "Identity bundle is missing: $identityPath. Supply the existing Windows bundle with -Identity."
}
$logRoot = Resolve-ProjectPath $LogDirectory
[System.IO.Directory]::CreateDirectory($logRoot) | Out-Null
$mode = if ($DryRunSynthetic) { 'synthetic' } else { 'input' }
$runId = '{0}-{1}' -f [DateTime]::UtcNow.ToString('yyyyMMdd-HHmmss-fff'), [Guid]::NewGuid().ToString('N')
$logPath = Join-Path $logRoot "windows-$mode-$runId.jsonl"
# The Rust logger also opens with create_new, so an existing log is never replaced.
if (Test-Path -LiteralPath $logPath) { throw "Refusing to replace existing log: $logPath" }
$nativeArguments = @(
    'send', '--host', $destination, '--identity', $identityPath,
    '--log', $logPath, '--seconds', $Seconds.ToString([Globalization.CultureInfo]::InvariantCulture),
    '--max-hz', $MaxHz.ToString([Globalization.CultureInfo]::InvariantCulture)
)
Write-Host "Crossflow -> $destination | $Seconds seconds | maximum send rate $MaxHz Hz"
Write-Host "Log: $logPath"
if ($DryRunSynthetic) {
    $nativeArguments += '--synthetic'
    Write-Warning 'SYNTHETIC TEST: the Mac MUST use DrySink (no --inject). This sends generated motion automatically; it does not sample your mouse or measure visible smoothness.'
    Write-Host 'No control window or sharing hotkeys in this mode. Ctrl+C stops the test.'
} else {
    Write-Host 'Start the Mac receiver first. Sharing starts PAUSED, including after reconnect.'
    Write-Host 'Ctrl+Alt+Space: start/pause. Ctrl+Alt+Esc: stop. The control window offers the same actions.'
    if ($ExperimentalInputs) {
        $nativeArguments += '--experimental-inputs'
        Write-Warning 'EXPERIMENTAL KEYS/BUTTONS/WHEEL: Windows local input still acts locally. Start with a Mac DrySink; live actions require --inject --inject-actions on the Mac.'
    } else {
        Write-Host 'Movement only: clicks are not shared, and the Windows cursor still moves.'
    }
}
# Splat a real argument array; never build a shell command or Invoke-Expression.
& $binary @nativeArguments
if ($LASTEXITCODE -ne 0) { throw "Crossflow exited with code $LASTEXITCODE. Log: $logPath" }
Write-Host "Test finished. Log: $logPath"
