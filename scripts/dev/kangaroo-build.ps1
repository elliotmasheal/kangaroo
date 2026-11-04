# Kangaroo build script (reusable) - fixes the two environment incidents:
#   * pagefile exhaustion / rustc LLVM "no space on device"  -> E: was full;
#     the cargo target dir now lives on a drive with free space
#     (CARGO_TARGET_DIR, override with -TargetDir).
#   * link.exe 1318 / 0xc0000005 flakiness                    -> fixed
#     parallelism (-Jobs 2) and CARGO_INCREMENTAL=0; retry once on
#     transient linker failures.
# Usage:
#   .\scripts\dev\kangaroo-build.ps1              # build the kangaroo bin
#   .\scripts\dev\kangaroo-build.ps1 test         # + crate tests
#   .\scripts\dev\kangaroo-build.ps1 test -Features dev-self-amendment
# Requires: scoop rustup layout (RUSTUP_HOME/CARGO_HOME below) and VS
# Build Tools / SDK discovered from the registry (see Import-MsvcEnv).
param(
    [Parameter(ValueFromRemainingArguments = $true)]
    [string[]]$CargoArgs = @("build", "-p", "zeroclaw-super-agent", "--bin", "kangaroo"),
    [string]$TargetDir = "C:\Users\Elliot\.cache\kangaroo-target"
)
$ErrorActionPreference = "Stop"

function Import-MsvcEnv {
    $roots = @(
        "D:\Microsoft Visual Studio",
        "C:\Program Files\Microsoft Visual Studio",
        "C:\Program Files (x86)\Microsoft Visual Studio"
    ) | Where-Object { Test-Path $_ }
    foreach ($root in $roots) {
        $vcvars = Get-ChildItem "$root\*\*\VC\Auxiliary\Build\vcvars64.bat" -ErrorAction SilentlyContinue |
            Sort-Object FullName -Descending | Select-Object -First 1
        if ($vcvars) { return $vcvars.FullName }
    }
    throw "vcvars64.bat not found (searched: $($roots -join '; '))"
}

function Import-SdkPaths {
    $kitsRoot = (Get-ItemProperty "HKLM:\SOFTWARE\Microsoft\Windows Kits\Installed Roots" -ErrorAction Stop).KitsRoot10
    $sdkVer = Get-ChildItem (Join-Path $kitsRoot "Lib") -Directory | Sort-Object Name -Descending | Select-Object -First 1
    $msvcRoot = Get-ChildItem "D:\Microsoft Visual Studio\18\Insiders\VC\Tools\MSVC" -Directory -ErrorAction SilentlyContinue |
        Sort-Object Name -Descending | Select-Object -First 1
    if (-not $msvcRoot) { throw "MSVC toolchain not found" }
    $env:INCLUDE = @(
        (Join-Path $sdkVer.FullName "Include\um"),
        (Join-Path $sdkVer.FullName "Include\ucrt"),
        (Join-Path $sdkVer.FullName "Include\shared"),
        (Join-Path $sdkVer.FullName "Include\winrt"),
        (Join-Path $msvcRoot.FullName "include")
    ) -join ";"
    $env:LIB = @(
        (Join-Path $sdkVer.FullName "Lib\um\x64"),
        (Join-Path $sdkVer.FullName "Lib\ucrt\x64"),
        (Join-Path $msvcRoot.FullName "lib\x64")
    ) -join ";"
    $env:Path = (Join-Path $msvcRoot.FullName "bin\Hostx64\x64") + ";" + $env:Path
}

# --- Scoop persist rustup layout ---
$env:RUSTUP_HOME = "D:\Scoop\persist\rustup\.rustup"
$env:CARGO_HOME = "D:\Scoop\persist\rustup\.cargo"
$env:Path = "D:\Scoop\persist\rustup\.cargo\bin;$env:Path"

# --- Environment guards against the two recorded incidents ---
$env:CARGO_INCREMENTAL = "0"          # rustc incremental-codegen crashes seen twice
$env:CARGO_TARGET_DIR = $TargetDir    # E: ran out of space (0 bytes free)
New-Item -ItemType Directory -Force $TargetDir | Out-Null
$freeGB = [Math]::Round((Get-PSDrive ($TargetDir.Substring(0, 1))).Free / 1GB, 1)
if ($freeGB -lt 10) { throw "target drive has only ${freeGB}GB free; free space first" }

Import-MsvcEnv | Out-Null
Import-SdkPaths

# --- Build/test with fixed parallelism and one retry on transient linker failure ---
function Invoke-CargoRetry {
    # NOTE: never name a parameter `$Args` — it collides with PowerShell's
    # automatic `$args` variable and the splat silently passes nothing, so
    # cargo never runs and a stale `$LASTEXITCODE` reports a false success.
    param([string[]]$CargoArguments)
    & cargo @CargoArguments
    $code = $LASTEXITCODE
    if ($null -eq $code) { $code = 1 }
    if ($code -ne 0) {
        Write-Host "first attempt failed (exit $code); retrying once with -j 2..." -ForegroundColor Yellow
        & cargo @CargoArguments "--jobs" "2"
        $code = $LASTEXITCODE
        if ($null -eq $code) { $code = 1 }
    }
    return $code
}

$exitCode = Invoke-CargoRetry $CargoArgs
if ($exitCode -ne 0) {
    Write-Host "=== failure diagnostics ===" -ForegroundColor Red
    Write-Host "target drive free: $freeGB GB"
    Write-Host "pagefile: $((Get-CimInstance Win32_PageFileUsage -ErrorAction SilentlyContinue).AllocatedBaseSize) MB"
    Write-Host "last 40 lines of the failure are above; keep them when reporting"
}
exit $exitCode
