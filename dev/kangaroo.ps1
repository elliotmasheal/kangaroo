# 一键启动袋鼠（Kangaroo）。
# 用法：在 PowerShell 里执行  .\dev\kangaroo.ps1
#       .\dev\kangaroo.ps1 -Full     # 显示完整编译输出（含被折叠的上游警告）
# 首次运行会走 5 题身份初始化；之后每次启动直接进入对话；:quit 退出。
#
# 启动策略（2026-11-04 复测后定稿）：
#   - 二进制比全部源码新 -> 完全跳过 cargo，直接运行已构建二进制（毫秒级）；
#   - 源码有变更 -> cargo build（错误完整显示；上游既有警告折叠为一行摘要，
#     袋鼠自身 crate 的警告照常显示；绝不用 --quiet 吞掉编译错误）。
# 脚本职责（不需要用户手动设置任何环境）：
#   1. 设置 Scoop 版 Rust 的 RUSTUP_HOME / CARGO_HOME / PATH；
#   2. 从注册表定位 MSVC + Windows SDK 并导入链接环境；
#   3. 把 cargo 产物目录固定到有空间的盘（E 盘曾满过）；
#   4. 失败时给出中文诊断，而不是堆栈。
param(
    [switch]$Full,
    [string]$TargetDir = "C:\Users\Elliot\.cache\kangaroo-target"
)
$ErrorActionPreference = "Stop"
$repo = Split-Path -Parent $PSScriptRoot
Set-Location $repo
$exe = Join-Path $TargetDir "debug\kangaroo.exe"
# Materialize the script-level pipeline input ONCE here: inside a function,
# `$input` refers to the function's own (empty) pipeline, not the script's.
$script:PipedInput = @($input)

function Fail([string]$message) {
    Write-Host ""
    Write-Host "[袋鼠] 启动失败：$message" -ForegroundColor Red
    Write-Host "[袋鼠] 常见原因：" -ForegroundColor Yellow
    Write-Host "  - 磁盘空间不足（target 目录所在盘需要 >5GB 空闲；当前回收站可能占用了大量空间）"
    Write-Host "  - Rust 工具链不完整（重跑：scoop install rustup；rustup default stable）"
    Write-Host "  - 链接器偶发失败（1318/1455）：直接重新执行本脚本即可，已内置一次自动重试"
    exit 1
}

function Invoke-Kangaroo {
    # Out-Host: assigning a function call captures its output stream, which
    # would swallow the binary's stdout into $runCode. Route it to the console.
    if ($script:PipedInput.Count -gt 0) {
        $script:PipedInput | & $exe | Out-Host
    } else {
        & $exe | Out-Host
    }
    $code = $LASTEXITCODE
    if ($null -eq $code) { $code = 1 }
    return $code
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

    # 3. 构建产物目录与空间护栏（历史事故：E 盘满 / 增量缓存崩溃）
    $env:CARGO_INCREMENTAL = "0"
    $env:CARGO_TARGET_DIR = $TargetDir
    New-Item -ItemType Directory -Force $TargetDir | Out-Null
    $freeGB = [Math]::Round((Get-PSDrive ($TargetDir.Substring(0, 1))).Free / 1GB, 1)
    if ($freeGB -lt 5) {
        Fail "target 所在盘只剩 ${freeGB}GB（需要 >5GB）"
    }

    # 4. 新鲜度判断：二进制比所有参与编译的源码新 -> 直接复用，跳过 cargo。
    #    参与编译 = crates 下的 .rs / Cargo.toml / .ftl + 根 Cargo.toml / Cargo.lock。
    function Test-BinaryFresh {
        if (-not (Test-Path $exe)) { return $false }
        $exeTime = (Get-Item $exe).LastWriteTime
        $files = @(Get-ChildItem "$repo\crates" -Recurse -File -ErrorAction SilentlyContinue |
            Where-Object { $_.Extension -in ".rs", ".ftl" -or $_.Name -eq "Cargo.toml" })
        foreach ($root in @("$repo\Cargo.toml", "$repo\Cargo.lock")) {
            if (Test-Path $root) { $files += Get-Item $root }
        }
        foreach ($f in $files) {
            if ($f.LastWriteTime -gt $exeTime) { return $false }
        }
        return $true
    }

    if (Test-BinaryFresh) {
        # 源码未变：完全跳过 cargo，直接复用已构建二进制。
        Write-Host "[袋鼠] 二进制已是最新，直接启动……" -ForegroundColor DarkGray
    } else {
        # 源码有变更：编译。输出策略——错误完整、上游警告折叠、自身警告保留。
        if (Test-Path $exe) {
            Write-Host "[袋鼠] 检测到源码有变更，正在增量编译……"
        } else {
            Write-Host "[袋鼠] 首次运行需要完整编译，大约 5-10 分钟，请稍候……"
        }
    $ErrorActionPreference = "Continue"
    function Invoke-FilteredBuild {
        $out = & cargo build --jobs 2 -p zeroclaw-super-agent --bin kangaroo 2>&1 |
            ForEach-Object { "$_" }
        $code = $LASTEXITCODE
        if ($null -eq $code) { $code = 1 }
        return @{
            Code = $code
            Out  = $out
        }
    }

    $build = Invoke-FilteredBuild
    $hasErrors = ($build.Out | Where-Object { $_ -match '^error' }).Count -gt 0
    if ($build.Code -ne 0 -or $hasErrors) {
        # 编译失败：完整输出，绝不折叠——错误必须可读。
        $build.Out | ForEach-Object { Write-Host $_ }
        if ($build.Code -ne 0) {
            Write-Host "[袋鼠] 第一次编译失败（exit $($build.Code)），自动重试一次……" -ForegroundColor Yellow
            $build = Invoke-FilteredBuild
            $hasErrors = ($build.Out | Where-Object { $_ -match '^error' }).Count -gt 0
            if ($build.Code -ne 0 -or $hasErrors) {
                $build.Out | ForEach-Object { Write-Host $_ }
                Fail "编译两次失败（exit $($build.Code)）。请把上方最后 40 行输出发给开发。"
            }
            Write-Host "[袋鼠] 重试后编译成功。" -ForegroundColor Green
        }
    }

    if (-not (Test-Path $exe)) {
        Fail "编译报告成功但没有找到 $exe"
    }

    # 警告分流：块首为 warning: 的连续行算一块；含 zeroclaw-super-agent 的块
    # 原样显示，其余（上游既有警告）折叠为计数；-Full 时全部显示。
    if ($Full) {
        $build.Out | ForEach-Object { Write-Host $_ }
    } else {
        $upstreamCount = 0
        $current = $null
        $blocks = New-Object System.Collections.Generic.List[System.Collections.Generic.List[string]]
        foreach ($line in $build.Out) {
            if ($line -match '^warning') {
                if ($current) { $blocks.Add($current) }
                $current = New-Object System.Collections.Generic.List[string]
                $current.Add($line)
            } elseif ($current) {
                $current.Add($line)
            }
        }
        if ($current) { $blocks.Add($current) }
        foreach ($block in $blocks) {
            $text = $block -join "`n"
            if ($text -match 'zeroclaw-super-agent') {
                $block | ForEach-Object { Write-Host $_ }
            } else {
                $upstreamCount++
            }
        }
        if ($upstreamCount -gt 0) {
            Write-Host "[袋鼠] 上游 $upstreamCount 条既有警告已折叠（-Full 查看）" -ForegroundColor DarkGray
        }
    }

    Write-Host "[袋鼠] 编译完成，启动……" -ForegroundColor Green
    }

    $runCode = Invoke-Kangaroo
    if ($runCode -eq 0) {
        Write-Host ""
        Write-Host "[袋鼠] 对话已结束。下次双击 袋鼠.cmd 就能继续。"
    }
    exit $runCode
} catch {
    Fail $_.Exception.Message
}
