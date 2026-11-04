@echo off
rem Kangaroo double-click launcher: hands off to dev\kangaroo.ps1.
rem Keep every line BEFORE `chcp 65001` pure ASCII (cmd parses it with the
rem OEM codepage first). This file must be saved as UTF-8 without BOM.
chcp 65001 >nul
rem From here on the file is read back as UTF-8, so Chinese text is safe.
rem Process-level environment only; nothing below touches system variables.
set "RUSTUP_HOME=D:\Scoop\persist\rustup\.rustup"
set "CARGO_HOME=D:\Scoop\persist\rustup\.cargo"
set "PATH=D:\Scoop\persist\rustup\.cargo\bin;%PATH%"
rem Prefer PowerShell 7 (reads UTF-8 scripts without a BOM); fall back to
rem Windows PowerShell 5.1, which needs the BOM the .ps1 carries.
where pwsh >nul 2>nul
if "%ERRORLEVEL%"=="0" (
    set "KANGAROO_SHELL=pwsh"
) else (
    set "KANGAROO_SHELL=powershell"
)
"%KANGAROO_SHELL%" -NoProfile -ExecutionPolicy Bypass -File "%~dp0dev\kangaroo.ps1" %*
set "KANGAROO_EXIT_CODE=%ERRORLEVEL%"
if not "%KANGAROO_EXIT_CODE%"=="0" (
    echo.
    echo [袋鼠] 启动过程没有走完，上面的信息说明了发生了什么、可以怎么办。
)
rem Keep the window open: a failure must stay readable, and the closing
rem line after a normal conversation should not vanish instantly.
pause
exit /b %KANGAROO_EXIT_CODE%
