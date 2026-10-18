<#
.SYNOPSIS
    Dev helper: installs the freshly built WSL binary over the running
    agent-notify tray and restarts it. No typing per run.

.DESCRIPTION
    Resolves the source binary from the WSL checkout (wslpath), finds
    where the running tray actually lives (process path, with a
    fallback), kills it, copies the new exe in place, and starts it
    again. If the destination denies the write, the script relaunches
    itself elevated (UAC prompt) and waits.

.EXAMPLE
    powershell -NoProfile -ExecutionPolicy Bypass -File scripts/dev-update.ps1
    # or with a non-default distro:
    powershell -NoProfile -ExecutionPolicy Bypass -File scripts/dev-update.ps1 -WslDistro Debian
#>
param(
    # Explicit source exe; defaults to the checkout this script lives in.
    [string]$Source,
    # Fallback directory when no running process reveals its location.
    [string]$TargetDir = 'S:\standalone\autostart'
)
$ErrorActionPreference = 'Stop'

# The script runs straight from the WSL checkout, so the binary is one
# directory up: \\wsl.localhost\<distro>\<repo>\scripts\..\bin
if (-not $Source) {
    $Source = Join-Path (Split-Path $PSScriptRoot) 'bin\agent-notify.exe'
}
if (-not $Source -or -not (Test-Path $Source)) {
    throw "source binary not found: '$Source' (build it first: make build in WSL)"
}

$running = Get-Process agent-notify -ErrorAction SilentlyContinue | Select-Object -First 1
if ($running -and $running.Path) {
    $TargetDir = Split-Path $running.Path
}
$target = Join-Path $TargetDir 'agent-notify.exe'
Write-Host "dev-update: $Source -> $target"

if ($running) {
    taskkill /IM agent-notify.exe /F | Out-Null
    Start-Sleep -Milliseconds 300
}

try {
    Copy-Item $Source $target -Force -ErrorAction Stop
} catch {
    Write-Host "dev-update: write denied ($($_.Exception.Message)); relaunching elevated..."
    $argList = "-NoProfile -ExecutionPolicy Bypass -File `"$PSCommandPath`" -Source `"$Source`" -TargetDir `"$TargetDir`""
    Start-Process powershell -Verb RunAs -ArgumentList $argList -Wait
    exit 0
}

Start-Process $target
Start-Sleep -Milliseconds 500
$now = Get-Process agent-notify -ErrorAction SilentlyContinue
if ($now) {
    Write-Host "dev-update: running (pid $($now.Id)) from $($now.Path)"
} else {
    Write-Warning "dev-update: tray did not start; launch $target manually"
}
