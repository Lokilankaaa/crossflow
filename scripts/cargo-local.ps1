param([Parameter(ValueFromRemainingArguments=$true)][string[]]$CargoArgs)
$ErrorActionPreference = 'Stop'
$projectRoot = Split-Path -Parent $PSScriptRoot
$env:CARGO_HOME = Join-Path $projectRoot '.tools/cargo'
$env:RUSTUP_HOME = Join-Path $projectRoot '.tools/rustup'
$compilerBin = Join-Path $projectRoot '.tools/llvm-mingw-20261020-ucrt-x86_64/bin'
$env:PATH = "$compilerBin;$env:CARGO_HOME/bin;$env:PATH"
$env:CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER = Join-Path $compilerBin 'x86_64-w64-mingw32-gcc.exe'
$env:RUSTFLAGS = '-C link-self-contained=yes'
$env:LIBRARY_PATH = Join-Path $env:RUSTUP_HOME 'toolchains/stable-x86_64-pc-windows-gnu/lib/rustlib/x86_64-pc-windows-gnu/lib/self-contained'
& (Join-Path $env:CARGO_HOME 'bin/cargo.exe') @CargoArgs
exit $LASTEXITCODE
