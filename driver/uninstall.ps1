# OpenMouse Attack Shark X11 — remove the WinUSB driver package.
#
# Reverts sign-and-install.ps1: finds our driver package by provider name
# (locale-independent) and removes it, restoring the default HID driver on the
# next reconnect. No firmware is touched. Run from an ADMIN PowerShell:
#   powershell -ExecutionPolicy Bypass -File .\uninstall.ps1

#Requires -RunAsAdministrator
$ErrorActionPreference = 'Stop'

Write-Host 'Looking for the OpenMouse driver package...'

# Packages bound to a live device (provider name is locale-independent).
$infs = @(Get-CimInstance Win32_PnPSignedDriver -ErrorAction SilentlyContinue |
    Where-Object { $_.DriverProviderName -eq 'OpenMouse' } |
    ForEach-Object { $_.InfName })

# Also catch a package that was staged in the driver store but never bound: scan
# pnputil's output for our original INF name (a literal value, so this works in
# any Windows display language) and pull the oemNN.inf published name near it.
$enum = (& pnputil /enum-drivers) -join "`n"
foreach ($block in ($enum -split "`r?`n`r?`n")) {
    if ($block -match 'OpenMouse-AttackShark-X11\.inf') {
        $m = [regex]::Match($block, 'oem\d+\.inf')
        if ($m.Success) { $infs += $m.Value }
    }
}
$infs = @($infs | Sort-Object -Unique)

if (-not $infs) {
    Write-Host 'No OpenMouse driver package is installed. Nothing to do.'
    return
}

foreach ($inf in $infs) {
    Write-Host "Removing $inf ..."
    & pnputil /delete-driver $inf /uninstall
    if ($LASTEXITCODE -ne 0 -and $LASTEXITCODE -ne 3010) {
        Write-Warning "pnputil returned $LASTEXITCODE for $inf. You can also remove it from Device Manager."
    }
}

Write-Host 'Done. Unplug and replug the mouse to restore its normal driver.'
