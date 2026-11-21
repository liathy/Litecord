# Fast local test build. Not for releases or CPU measurements.
# Usage: powershell -ExecutionPolicy Bypass -File scripts/quick-build.ps1
$ErrorActionPreference = 'Stop'
Set-Location (Split-Path -Parent $PSScriptRoot)

$sysroot = (& rustc --print sysroot).Trim()
$lld = Join-Path $sysroot 'lib\rustlib\x86_64-pc-windows-msvc\bin\rust-lld.exe'
if (Test-Path $lld) {
    $env:CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER = 'rust-lld'
}

$started = Get-Date
& cargo build --profile quick --features automation --bin litecord
$code = $LASTEXITCODE
Write-Output ("quick build exit={0} in {1}s" -f $code, [int]((Get-Date) - $started).TotalSeconds)
exit $code
