param([string]$LibraryDir = $env:CROSSFLOW_BORINGSSL_LIB_DIR, [switch]$Offline)
$ErrorActionPreference = 'Stop'
$projectRoot = Split-Path -Parent $PSScriptRoot
if ([string]::IsNullOrWhiteSpace($LibraryDir) -or -not [IO.Path]::IsPathFullyQualified($LibraryDir)) {
    throw 'Supply -LibraryDir with the absolute directory containing libcrypto.a built from experiments/pairing-spake2/source-lock.json.'
}
if (-not (Test-Path -LiteralPath (Join-Path $LibraryDir 'libcrypto.a') -PathType Leaf)) {
    throw 'The pinned BoringSSL static library is missing. This script does not download native dependencies.'
}
$toolchainBin = Join-Path $projectRoot '.tools/llvm-mingw-20261020-ucrt-x86_64/bin'
$runtimes = @('libc++.dll', 'libunwind.dll', 'libwinpthread-1.dll')
foreach ($name in $runtimes) {
    if (-not (Test-Path -LiteralPath (Join-Path $toolchainBin $name) -PathType Leaf)) {
        throw "Missing tested LLVM-MinGW runtime: $name"
    }
}
$env:CROSSFLOW_BORINGSSL_LIB_DIR = $LibraryDir
$manifest = 'experiments/pairing-spake2/rust-pairing/Cargo.toml'
$targetDir = Join-Path $projectRoot 'experiments/pairing-spake2/rust-pairing/target'
$targetTriple = 'x86_64-pc-windows-gnu'
$arguments = @('build', '--manifest-path', $manifest, '--target-dir', $targetDir, '--target', $targetTriple, '--bin', 'crossflow-enroll', '--release', '--locked')
if ($Offline) { $arguments += '--offline' }
Push-Location $projectRoot
try {
    & (Join-Path $PSScriptRoot 'cargo-local.ps1') -CargoArgs $arguments
    if ($LASTEXITCODE -ne 0) { throw 'Rust pairing worker build failed.' }
}
finally { Pop-Location }
$package = Join-Path $projectRoot '.tools/pairing-package'
New-Item -ItemType Directory -Force -Path $package | Out-Null
Copy-Item -LiteralPath (Join-Path $targetDir "$targetTriple/release/crossflow-enroll.exe") -Destination (Join-Path $package 'crossflow-enroll.exe') -Force
foreach ($name in $runtimes) {
    Copy-Item -LiteralPath (Join-Path $toolchainBin $name) -Destination (Join-Path $package $name) -Force
}
Write-Output 'Pairing worker and its three native runtime libraries are ready for the WinUI package.'
