@echo off
setlocal
chcp 65001 >nul
rem Kangaroo double-click launcher. Pure ASCII by policy: every byte must
rem be 0x20-0x7E plus CRLF, because the cmd parser reads this file with the
rem OEM codepage before any chcp takes effect and cannot handle bare LFs.
rem All user-facing Chinese text lives in dev\kangaroo.ps1 (UTF-8 BOM).
set "RUSTUP_HOME=D:\Scoop\persist\rustup\.rustup"
set "CARGO_HOME=D:\Scoop\persist\rustup\.cargo"
set "PATH=D:\Scoop\persist\rustup\.cargo\bin;%PATH%"
set "KANGAROO_SHELL=powershell"
where pwsh >nul 2>nul
if "%ERRORLEVEL%"=="0" set "KANGAROO_SHELL=pwsh"
"%KANGAROO_SHELL%" -NoProfile -ExecutionPolicy Bypass -File "%~dp0dev\kangaroo.ps1" %*
set "KANGAROO_EXIT_CODE=%ERRORLEVEL%"
rem Keep the window open: on a failure the Chinese diagnostics are already
rem on screen from the PowerShell layer, and the closing line after a
rem normal conversation should not vanish instantly.
pause
endlocal
exit /b %KANGAROO_EXIT_CODE%
