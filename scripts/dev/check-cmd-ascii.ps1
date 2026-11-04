# Byte-level gate for cmd batch launchers. cmd.exe parses batch files with
# the OEM codepage before any in-file chcp takes effect and mishandles bare
# LF line endings, so a launcher containing non-ASCII bytes or bare LFs gets
# shredded into phantom commands (seen live on 2026-11-04). This check pins
# the contract: ASCII bytes only, CRLF endings only, file ends with CRLF.
# Usage: .\scripts\dev\check-cmd-ascii.ps1 [-Path <file>]  (default: the
# repository's 袋鼠.cmd)
param(
    [string]$Path = (Join-Path (Split-Path -Parent (Split-Path -Parent $PSScriptRoot)) "袋鼠.cmd")
)
$ErrorActionPreference = "Stop"
$bytes = [System.IO.File]::ReadAllBytes($Path)

$nonAscii = 0
$lfCount = 0
$crlfCount = 0
for ($i = 0; $i -lt $bytes.Count; $i++) {
    $b = $bytes[$i]
    if (($b -lt 0x20 -and $b -ne 0x0D -and $b -ne 0x0A -and $b -ne 0x09) -or $b -gt 0x7E) {
        $nonAscii++
    }
    if ($b -eq 0x0A) {
        $lfCount++
        if ($i -gt 0 -and $bytes[$i - 1] -eq 0x0D) { $crlfCount++ }
    }
}
$endsWithCrlf = $bytes.Count -ge 2 -and $bytes[-2] -eq 0x0D -and $bytes[-1] -eq 0x0A
$bareLf = $lfCount - $crlfCount

Write-Host "file: $Path"
Write-Host "bytes: $($bytes.Count)"
Write-Host "non-ascii bytes: $nonAscii"
Write-Host "LF lines: $lfCount (CRLF: $crlfCount, bare LF: $bareLf)"
Write-Host "ends with CRLF: $endsWithCrlf"

$failed = $false
if ($nonAscii -ne 0) { Write-Host "FAIL: non-ASCII bytes found (cmd shreds them)" -ForegroundColor Red; $failed = $true }
if ($bareLf -ne 0) { Write-Host "FAIL: bare LF line endings found" -ForegroundColor Red; $failed = $true }
if (-not $endsWithCrlf) { Write-Host "FAIL: file does not end with CRLF" -ForegroundColor Red; $failed = $true }
if ($failed) { exit 1 }
Write-Host "OK: pure ASCII, CRLF-only, CRLF-terminated" -ForegroundColor Green
