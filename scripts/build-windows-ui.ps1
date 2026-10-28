param([ValidateSet('Debug','Release')][string]$Configuration = 'Release', [switch]$NoRestore, [switch]$BootstrapSdk, [switch]$SkipBridge)
$ErrorActionPreference = 'Stop'
$projectRoot = Split-Path -Parent $PSScriptRoot
$sdk = Join-Path $projectRoot '.tools/dotnet/dotnet.exe'
if (-not (Test-Path -LiteralPath $sdk)) {
    if (-not $BootstrapSdk) { throw 'Portable .NET SDK missing. Run this script with -BootstrapSdk to download the pinned official SDK into .tools only.' }
    $archive = Join-Path $projectRoot '.tools/dotnet-sdk-10.0.403-win-x64.zip'
    New-Item -ItemType Directory -Force (Split-Path -Parent $sdk) | Out-Null
    Invoke-WebRequest -Uri 'https://builds.dotnet.microsoft.com/dotnet/Sdk/10.0.403/dotnet-sdk-10.0.403-win-x64.zip' -OutFile $archive
    $expectedHash = '7a114c318fa7f1d9822991b534b2814d0e78386ceda816b371f5141acefe741d87ccb606ede06cadac8f9ac5070e32b708a67bbd1619ff5e5a7858a27874ef48'
    if ((Get-FileHash -LiteralPath $archive -Algorithm SHA512).Hash -ne $expectedHash) { throw 'Official SDK archive failed SHA512 verification.' }
    Expand-Archive -LiteralPath $archive -DestinationPath (Split-Path -Parent $sdk) -Force
}
$env:DOTNET_ROOT = Split-Path -Parent $sdk
$env:DOTNET_CLI_HOME = Join-Path $projectRoot '.tools/dotnet-home'
$env:DOTNET_SKIP_FIRST_TIME_EXPERIENCE = '1'
$env:DOTNET_CLI_TELEMETRY_OPTOUT = '1'
$env:DOTNET_NOLOGO = '1'
$env:NUGET_PACKAGES = Join-Path $projectRoot '.tools/nuget'
if (-not $SkipBridge) {
    Push-Location $projectRoot
    try {
        if (Test-Path -LiteralPath (Join-Path $projectRoot '.tools/cargo/bin/cargo.exe')) {
            & (Join-Path $PSScriptRoot 'cargo-local.ps1') -CargoArgs @('build', '--release', '--locked', '--package', 'crossflow-bridge')
        } else {
            & cargo build --release --locked --package crossflow-bridge
        }
        if ($LASTEXITCODE -ne 0) { throw 'Rust bridge build failed.' }
    }
    finally { Pop-Location }
}
if (-not (Test-Path -LiteralPath (Join-Path $projectRoot 'target/release/crossflow_bridge.dll'))) { throw 'Rust bridge DLL is missing. Build crossflow-bridge before the native client.' }
$project = Join-Path $projectRoot 'clients/windows/Crossflow.Windows/Crossflow.Windows.csproj'
$arguments = @('build', $project, '-c', $Configuration, '-p:Platform=x64')
if ($NoRestore) { $arguments += '--no-restore' }
& $sdk @arguments
exit $LASTEXITCODE
