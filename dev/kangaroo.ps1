# 一键启动袋鼠（Kangaroo）。
# 用法：在 PowerShell 里执行  .\dev\kangaroo.ps1
# 首次运行会走 5 题身份初始化；之后每次启动直接进入对话；:quit 退出。
#
# 脚本职责（不需要用户手动设置任何环境）：
#   1. 设置 Scoop 版 Rust 的 RUSTUP_HOME / CARGO_HOME / PATH；
#   2. 从注册表定位 MSVC + Windows SDK 并导入链接环境；
#   3. 把 cargo 产物目录固定到有空间的盘（E 盘曾满过）；
#   4. cargo run 失败时给出中文诊断，而不是堆栈。
param(
    [string]$TargetDir = "C:\Users\Elliot\.cache\kangaroo-target"
)
$ErrorActionPreference = "Stop"
$repo = Split-Path -Parent $PSScriptRoot
Set-Location $repo

function Fail([string]$message) {
    Write-Host ""
    Write-Host "[袋鼠] 启动失败：$message" -ForegroundColor Red
    Write-Host "[袋鼠] 常见原因：" -ForegroundColor Yellow
    Write-Host "  - 磁盘空间不足（target 目录所在盘需要 >10GB 空闲；当前回收站可能占用了大量空间）"
    Write-Host "  - Rust 工具链不完整（重跑：scoop install rustup；rustup default stable）"
    Write-Host "  - 链接器偶发失败（1318/1455）：直接重新执行本脚本即可，已内置一次自动重试"
    exit 1
}

try {
    # 1. Scoop persist 版 rustup
    if (-not (Test-Path "D:\Scoop\persist\rustup\.cargo\bin\cargo.exe")) {
        Fail "没有找到 D:\Scoop\persist\rustup 下的 Rust 工具链。请先执行：scoop install rustup"
    }
    $env:RUSTUP_HOME = "D:\Scoop\persist\rustup\.rustup"
    $env:CARGO_HOME = "D:\Scoop\persist\rustup\.cargo"
    $env:Path = "D:\Scoop\persist\rustup\.cargo\bin;$env:Path"

    # 2. MSVC + Windows SDK（注册表定位，不依赖 VS 安装路径猜测）
    $kitsRoot = (Get-ItemProperty "HKLM:\SOFTWARE\Microsoft\Windows Kits\Installed Roots" -ErrorAction Stop).KitsRoot10
    $sdkVer = Get-ChildItem (Join-Path $kitsRoot "Lib") -Directory | Sort-Object Name -Descending | Select-Object -First 1
    $msvcRoot = Get-ChildItem "D:\Microsoft Visual Studio\18\Insiders\VC\Tools\MSVC" -Directory -ErrorAction SilentlyContinue |
        Sort-Object Name -Descending | Select-Object -First 1
    if (-not $sdkVer -or -not $msvcRoot) {
        Fail "没有找到 MSVC 工具链或 Windows SDK（D:\Microsoft Visual Studio 或 D:\Windows Kits）"
    }
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

    # 3. 构建产物目录与增量护栏（历史事故：E 盘满 / 增量缓存崩溃）
    $env:CARGO_INCREMENTAL = "0"
    $env:CARGO_TARGET_DIR = $TargetDir
    New-Item -ItemType Directory -Force $TargetDir | Out-Null
    $freeGB = [Math]::Round((Get-PSDrive ($TargetDir.Substring(0, 1))).Free / 1GB, 1)
    if ($freeGB -lt 5) {
        Fail "target 所在盘 ${TargetDir.Substring(0,1)}: 只剩 ${freeGB}GB（需要 >5GB）"
    }

    # 4. 启动（失败自动重试一次——链接器偶发 1318）
    # stdin 分流：真实终端直继承（可交互输入）；管道喂入（自动化/记录）时
    # 转发管道内容，否则子进程会读到空 stdin。
    Write-Host "[袋鼠] 正在编译/启动（首次约数分钟，之后秒级）……"
    if ([Console]::IsInputRedirected) {
        $input | & cargo run --jobs 2 -p zeroclaw-super-agent --bin kangaroo
        if ($LASTEXITCODE -ne 0) {
            Write-Host "[袋鼠] 第一次启动失败（exit $LASTEXITCODE），自动重试一次……" -ForegroundColor Yellow
            $input | & cargo run --jobs 2 -p zeroclaw-super-agent --bin kangaroo
            if ($LASTEXITCODE -ne 0) {
                Fail "cargo run 两次失败（exit $LASTEXITCODE）。请把上方最后 40 行输出发给开发。"
            }
        }
    } else {
        & cargo run --jobs 2 -p zeroclaw-super-agent --bin kangaroo
        if ($LASTEXITCODE -ne 0) {
            Write-Host "[袋鼠] 第一次启动失败（exit $LASTEXITCODE），自动重试一次……" -ForegroundColor Yellow
            & cargo run --jobs 2 -p zeroclaw-super-agent --bin kangaroo
            if ($LASTEXITCODE -ne 0) {
                Fail "cargo run 两次失败（exit $LASTEXITCODE）。请把上方最后 40 行输出发给开发。"
            }
        }
    }
} catch {
    Fail $_.Exception.Message
}
