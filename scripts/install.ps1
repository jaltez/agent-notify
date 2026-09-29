<#
.SYNOPSIS
    One-line installer for agent-notify on Windows:
      irm https://raw.githubusercontent.com/jaltez/agent-notify/main/scripts/install.ps1 | iex
    With options:
      & ([scriptblock]::Create((irm https://raw.githubusercontent.com/jaltez/agent-notify/main/scripts/install.ps1))) -NoAutostart
.DESCRIPTION
    Downloads the latest GitHub release, verifies the sha256 checksum,
    and installs the binaries into %LOCALAPPDATA%\Programs\agent-notify.
    Everything else — user PATH, Startup shortcut (skipped with
    -NoAutostart), config, test popup, and with -WithWSL the headless
    daemon inside the WSL distro — is delegated to
    `agent-notify-console.exe setup --yes`, the same wizard a manually
    downloaded binary offers, so all installs end up identical.
#>
[CmdletBinding(SupportsShouldProcess)]
param(
    # Do not create the Startup shortcut.
    [switch]$NoAutostart,
    # Also install the daemon inside the WSL distro (needs systemd there).
    [switch]$WithWSL,
    # WSL distro for -WithWSL (default: wsl.exe's default distro).
    [string]$WslDistro,
    # Release tag to install (e.g. v0.4.0); default: latest release.
    [string]$Version,
    # Installation directory.
    [string]$Dir
)

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
try {
    [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
} catch {}

$Repo  = 'jaltez/agent-notify'
$Asset = 'agent-notify_windows_amd64.zip'

if (-not $env:LOCALAPPDATA) {
    throw "install: this installer is Windows-only; on Linux/WSL use scripts/install.sh"
}
if (-not $Dir) {
    $Dir = Join-Path $env:LOCALAPPDATA 'Programs\agent-notify'
}

function Fetch([string]$Url, [string]$Out) {
    # -UseBasicParsing: no IE first-run dependency on Windows PowerShell 5.1.
    Invoke-WebRequest -UseBasicParsing -Uri $Url -OutFile $Out
}

if ($Version) {
    $Tag = $Version
} else {
    Write-Host 'install: finding latest release'
    $Tag = (Invoke-RestMethod -Uri "https://api.github.com/repos/$Repo/releases/latest").tag_name
}
$Base = "https://github.com/$Repo/releases/download/$Tag"

Write-Host "install: installing $Tag"

$tmp = Join-Path ([IO.Path]::GetTempPath()) ('agent-notify-' + [Guid]::NewGuid().ToString('N'))
$null = [IO.Directory]::CreateDirectory($tmp)
try {
    Write-Host "install: downloading and verifying $Asset"
    Fetch "$Base/$Asset"       (Join-Path $tmp $Asset)
    Fetch "$Base/checksums.txt" (Join-Path $tmp 'checksums.txt')

    $line = Select-String -Path (Join-Path $tmp 'checksums.txt') `
        -Pattern ([regex]::Escape($Asset)) | Select-Object -First 1
    if (-not $line) { throw "install: no checksum entry for $Asset" }
    $expected = ($line.Line -split '\s+')[0].ToLower()
    # Get-FileHash has no ShouldProcess, but its internal Get-Item obeys
    # $WhatIfPreference and then yields nothing - disable the preference
    # around the call.
    $prevWhatIf = $WhatIfPreference
    $WhatIfPreference = $false
    $actual = (Get-FileHash -Algorithm SHA256 (Join-Path $tmp $Asset)).Hash.ToLower()
    $WhatIfPreference = $prevWhatIf
    if ($actual -ne $expected) {
        throw "install: checksum mismatch (expected $expected, got $actual)"
    }

    # A running tray locks its exe; remember whether one was up so it can be
    # restarted after the swap (same pattern the self-updater uses).
    $running = @(Get-Process 'agent-notify', 'agent-notify-console' -ErrorAction SilentlyContinue)
    if ($running.Count -gt 0 -and $PSCmdlet.ShouldProcess($running[0].Path, 'stop running agent-notify')) {
        $running | Stop-Process -Force
        Wait-Process -Id $running[0].Id -ErrorAction SilentlyContinue -Timeout 5
    }

    $extract = Join-Path $tmp 'x'
    # Guarded so -WhatIf never invokes the cmdlet: PS 5.1's Expand-Archive
    # crashes under -WhatIf when the destination does not exist yet.
    if ($PSCmdlet.ShouldProcess($Asset, 'extract archive')) {
        Expand-Archive -Path (Join-Path $tmp $Asset) -DestinationPath $extract
    }

    if ($PSCmdlet.ShouldProcess($Dir, 'install agent-notify binaries')) {
        $null = New-Item -ItemType Directory -Force -Path $Dir
        foreach ($exe in 'agent-notify.exe', 'agent-notify-console.exe') {
            Copy-Item (Join-Path $extract $exe) (Join-Path $Dir $exe) -Force
        }
        Write-Host "install: installed $Dir"
    }

    # The binary configures itself — PATH, Startup shortcut, config, test
    # popup — so script installs and manual `agent-notify setup` installs
    # are identical. Run via the console twin so output shows under irm|iex.
    $setupArgs = @('setup', '--yes')
    if ($NoAutostart) { $setupArgs += '--no-autostart' }
    if ($WithWSL) {
        $setupArgs += '--with-wsl'
        if ($WslDistro) { $setupArgs += @('--wsl-distro', $WslDistro) }
    }
    if ($PSCmdlet.ShouldProcess($Dir, 'run agent-notify setup')) {
        & (Join-Path $Dir 'agent-notify-console.exe') @setupArgs
        if ($LASTEXITCODE -ne 0) {
            Write-Warning "install: setup exited with $LASTEXITCODE (see its messages above)"
        }
    }

    if ($running.Count -gt 0 -and $PSCmdlet.ShouldProcess("$Dir\agent-notify.exe", 'restart previously running tray')) {
        Start-Process (Join-Path $Dir 'agent-notify.exe')
    }

    Write-Host "install: done - run 'agent-notify probe' to see what it detects"
} finally {
    Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
}
