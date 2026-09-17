# One-shot terminal image diagnostic.
#
# ASCII only, on purpose: Windows PowerShell 5.1 reads a .ps1 without a BOM as
# ANSI, so any non-ASCII character corrupts the quoting and the rest of the
# script leaks out as literal text instead of running.
#
# Run this INSIDE WezTerm. It tries each image protocol under a labelled
# banner, so one look tells you which ones render.

$ErrorActionPreference = 'Continue'
$wt = "C:\Program Files\WezTerm\wezterm.exe"
$png = "D:\Tools\yomu\tools\test.png"
$yomu = "D:\Tools\yomu\target\release\yomu.exe"

function Banner($n, $text) {
    Write-Host ""
    Write-Host ("=" * 60) -ForegroundColor DarkGray
    Write-Host " $n. $text" -ForegroundColor Cyan
    Write-Host ("=" * 60) -ForegroundColor DarkGray
}

Banner 0 "Environment"
Write-Host "wezterm:      $(& $wt --version)"
Write-Host "TERM_PROGRAM: $env:TERM_PROGRAM"
Write-Host "WEZTERM_PANE: $env:WEZTERM_PANE"
if (-not $env:WEZTERM_PANE) {
    Write-Host "not running inside WezTerm - results below mean nothing" -ForegroundColor Yellow
}
# Our config defines Ctrl+Shift+Y. show-keys renders that as "CTRL  SHIFT  Y".
$keys = & $wt show-keys 2>&1 | Out-String
if ($keys -match 'Y\s+->\s+SpawnCommandInNewTab') {
    Write-Host "config file is valid and readable" -ForegroundColor Green
} else {
    Write-Host "config file not being read" -ForegroundColor Red
}

Banner 1 "iTerm2 protocol (wezterm imgcat - WezTerm's own tool)"
& $wt imgcat $png
Write-Host ""

Banner 2 "Kitty protocol (raw escapes, no application involved)"
& wsl.exe -e python3 /mnt/d/Tools/yomu/tools/kitty-test.py

Banner 3 "Verdict"
Write-Host "Which banners showed a colour gradient?"
Write-Host "  1 only  -> iterm2  (expected on Windows: ConPTY strips kitty's APC escapes)"
Write-Host "  2 only  -> kitty"
Write-Host "  both    -> either; kitty is faster for long strips"
Write-Host "  neither -> images are stripped before reaching the screen"
Write-Host ""
Write-Host "yomu now auto-selects iterm2 inside WezTerm. To force a choice:"
Write-Host "  $yomu --protocol iterm2 --font-size 8x18"
Write-Host ""
